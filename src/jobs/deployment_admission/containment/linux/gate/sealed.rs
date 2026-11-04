//! Immutable executable bytes, NOT a deployment or configuration authorization.
//! The future consuming spawn must derive the executable digest from its pinned
//! adapter binding. No configured pathname is reopened by this primitive.
use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::FileExt;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

pub(super) struct SealedExecutable(File);

impl SealedExecutable {
    pub(super) fn copy(source: &File, expected_sha256: &str) -> Result<Self> {
        ensure!(
            expected_sha256.len() == 64
                && expected_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid executable SHA-256"
        );
        let metadata = source.metadata()?;
        ensure!(
            metadata.is_file() && (64..=MAX_BYTES).contains(&metadata.len()),
            "sealed executable source size/type refused"
        );
        let fd = unsafe {
            libc::memfd_create(
                c"privateer-adapter".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        ensure!(fd >= 0, "executable memfd unavailable");
        let mut copy = unsafe { File::from_raw_fd(fd) };
        let mut bytes = [0; 65_536];
        let mut offset = 0;
        loop {
            // Positional reads neither consume nor depend on a shared source
            // offset. A concurrent writer can only cause digest refusal, not
            // change the verified copy after sealing.
            let n = source.read_at(&mut bytes, offset)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            ensure!(offset <= MAX_BYTES, "sealed executable exceeds bound");
            copy.write_all(&bytes[..n])?;
        }
        ensure!(
            unsafe { libc::fchmod(fd, 0o500) } == 0,
            "executable memfd permission refused"
        );
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, SEALS) } == 0
                && unsafe { libc::fcntl(fd, libc::F_GET_SEALS) } == SEALS,
            "executable memfd sealing refused"
        );
        // Both checks MUST follow sealing, and inspect exactly the fd that is
        // duplicated into the child. Never hash the mutable source instead.
        ensure!(
            digest(&copy)? == expected_sha256,
            "sealed executable digest mismatch"
        );
        elf::validate(&copy)?;
        Ok(Self(copy))
    }

    pub(super) fn file(&self) -> &File {
        &self.0
    }
}

fn digest(file: &File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0; 65_536];
    let mut offset = 0;
    loop {
        let n = file.read_at(&mut bytes, offset)?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        ensure!(offset <= MAX_BYTES, "sealed executable hash exceeds bound");
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_fd_refuses_writes_growth_shrink_and_shared_writable_mapping() {
        let fixture = super::super::tests::Fixture::build();
        let file = fixture.executable.file();
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) },
            SEALS
        );
        let denied = |error: std::io::Error| assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        denied(file.write_at(b"gamma", 0).unwrap_err());
        denied(
            file.set_len(file.metadata().unwrap().len() + 1)
                .unwrap_err(),
        );
        denied(file.set_len(0).unwrap_err());
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapping != libc::MAP_FAILED {
            unsafe {
                libc::munmap(mapping, 4096);
            }
            panic!("sealed executable permitted shared writable mapping");
        }
        denied(std::io::Error::last_os_error());
        // Mode 0500 can refuse the open itself for an unprivileged owner.
        // Prove seals, not just DAC, prevent a newly opened writer: even changing
        // the mode to allow that open cannot change the sealed bytes.
        assert_eq!(unsafe { libc::fchmod(file.as_raw_fd(), 0o700) }, 0);
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .unwrap();
        denied(writer.write_at(b"gamma", 0).unwrap_err());
        assert_eq!(unsafe { libc::fchmod(file.as_raw_fd(), 0o500) }, 0);
        elf::validate(file).unwrap();
    }

    #[test]
    fn sealed_copy_ignores_source_offset_and_survives_source_rewrite() {
        use std::io::{Seek, SeekFrom};
        let fixture = super::super::tests::Fixture::build();
        let mut source = tempfile::tempfile().unwrap();
        let file = fixture.executable.file();
        let expected = digest(file).unwrap();
        let mut bytes = vec![0; file.metadata().unwrap().len() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        source.write_all(&bytes).unwrap();
        // copy() must read from byte zero, not the EOF offset left above.
        let sealed = SealedExecutable::copy(&source, &expected).unwrap();
        assert_eq!(source.stream_position().unwrap(), bytes.len() as u64);
        source.seek(SeekFrom::Start(0)).unwrap();
        source.write_all(b"#!/alpha\n").unwrap();
        source.set_len(9).unwrap();
        assert_eq!(digest(sealed.file()).unwrap(), expected);
        elf::validate(sealed.file()).unwrap();
        assert!(SealedExecutable::copy(&source, &expected).is_err());
    }

    #[test]
    fn mismatched_digest_invalid_elf_and_oversize_refuse_before_gate() {
        let fixture = super::super::tests::Fixture::build();
        assert!(
            SealedExecutable::copy(fixture.executable.file(), &"0".repeat(64))
                .err()
                .unwrap()
                .to_string()
                .contains("digest mismatch")
        );
        let mut source = tempfile::tempfile().unwrap();
        source.write_all(&[0; 120]).unwrap();
        let expected = digest(&source).unwrap();
        assert!(
            SealedExecutable::copy(&source, &expected)
                .err()
                .unwrap()
                .to_string()
                .contains("requires native")
        );
        assert!(SealedExecutable::copy(&source, "alpha").is_err());
        // A matching digest is insufficient: the sealed bytes must also pass
        // ELF policy. Introduce PT_INTERP into an otherwise native fixture.
        let file = fixture.executable.file();
        let mut bytes = vec![0; file.metadata().unwrap().len() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        let table = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
        bytes[table..table + 4].copy_from_slice(&3u32.to_le_bytes());
        let mut dynamic = tempfile::tempfile().unwrap();
        dynamic.write_all(&bytes).unwrap();
        assert!(
            SealedExecutable::copy(&dynamic, &digest(&dynamic).unwrap())
                .err()
                .unwrap()
                .to_string()
                .contains("PT_INTERP")
        );
        source.set_len(MAX_BYTES + 1).unwrap();
        assert!(
            SealedExecutable::copy(&source, &expected)
                .err()
                .unwrap()
                .to_string()
                .contains("size/type")
        );
    }
}
