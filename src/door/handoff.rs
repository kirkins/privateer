//! Descriptor-relative scoped upload admission. No source path is reopened.
use anyhow::{Context, Result, ensure};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

pub(super) fn open(state: &Path, source: &Path) -> Result<File> {
    let relative = source
        .strip_prefix(state)
        .context("source outside door state")?;
    let components = relative.components().collect::<Vec<_>>();
    ensure!(
        !components.is_empty() && components.iter().all(|c| matches!(c, Component::Normal(_))),
        "source must be below door state without parent components"
    );
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state)?;
    for (index, component) in components.iter().enumerate() {
        let name = std::ffi::CString::new(component.as_os_str().as_bytes())?;
        let last = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: directory and name remain alive for openat; on success this
        // function takes sole ownership of the newly returned descriptor.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "source must be a regular single-link file"
            );
            ensure!(
                metadata.len() <= crate::files::MAX_FILE_BYTES,
                "file exceeds quota"
            );
            return Ok(file);
        }
        directory = file;
    }
    unreachable!("nonempty components")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn opened_descriptor_survives_rename_and_refuses_escapes_and_special_files() {
        use std::io::Read;
        let root = std::env::temp_dir().join(format!("privateer-handoff-{}", uuid::Uuid::new_v4()));
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        let source = state.join("source");
        fs::write(&source, b"admitted").unwrap();
        let mut admitted = open(&state, &source).unwrap();
        fs::rename(&source, state.join("moved")).unwrap();
        fs::write(&source, b"replacement").unwrap();
        let mut bytes = String::new();
        admitted.read_to_string(&mut bytes).unwrap();
        assert_eq!(bytes, "admitted");
        symlink(&source, state.join("symlink")).unwrap();
        assert!(open(&state, &state.join("symlink")).is_err());
        fs::hard_link(&source, state.join("hardlink")).unwrap();
        assert!(open(&state, &source).is_err());
        assert!(open(&state, &root.join("outside")).is_err());
        assert!(open(&state, &state.join("../outside")).is_err());
        symlink(&state, state.join("directory-link")).unwrap();
        assert!(open(&state, &state.join("directory-link/moved")).is_err());
        let fifo = state.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(open(&state, &fifo).is_err()); // O_NONBLOCK: cannot hang on open
        assert!(open(&state, Path::new("/dev/null")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
