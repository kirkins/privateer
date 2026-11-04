//! Deliberately small exec policy, not a general ELF loader. Only native 64-bit
//! little-endian ELF on the listed Linux targets is supported. The kernel still
//! validates loadability; this check prevents an ambient PT_INTERP escape from
//! the held-inode digest policy before any child is created.
use super::*;
use std::os::unix::fs::FileExt;

fn native_machine() -> Result<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(62),
        "aarch64" => Ok(183),
        "riscv64" => Ok(243),
        _ => anyhow::bail!("unsupported static ELF architecture"),
    }
}

pub(super) fn validate(file: &File) -> Result<()> {
    let size = file.metadata()?.len();
    ensure!(
        file.metadata()?.is_file() && (64..=67_108_864).contains(&size),
        "static ELF file size/type refused"
    );
    let mut header = [0; 64];
    file.read_exact_at(&mut header, 0)?;
    ensure!(
        &header[..7] == b"\x7fELF\x02\x01\x01" && cfg!(target_endian = "little"),
        "requires native little-endian ELF64, not a script"
    );
    let word = |offset| u16::from_le_bytes(header[offset..offset + 2].try_into().unwrap());
    let long = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    let wide = |offset| u64::from_le_bytes(header[offset..offset + 8].try_into().unwrap());
    ensure!(
        matches!(word(16), 2 | 3) && word(18) == native_machine()? && long(20) == 1,
        "unsupported ELF type/machine/version"
    );
    let count = word(56) as u64;
    let offset = wide(32);
    ensure!(
        word(52) == 64
            && word(54) == 56
            && (1..=128).contains(&count)
            && offset >= 64
            && offset
                .checked_add(count * 56)
                .is_some_and(|end| end <= size),
        "malformed or oversized ELF program table"
    );
    let entry = wide(24);
    let mut executable_entry = false;
    for index in 0..count {
        let mut program = [0; 56];
        file.read_exact_at(&mut program, offset + index * 56)?;
        let kind = u32::from_le_bytes(program[..4].try_into()?);
        ensure!(kind != 3, "PT_INTERP refused: adapter must be static ELF");
        let flags = u32::from_le_bytes(program[4..8].try_into()?);
        let at = u64::from_le_bytes(program[8..16].try_into()?);
        let address = u64::from_le_bytes(program[16..24].try_into()?);
        let bytes = u64::from_le_bytes(program[32..40].try_into()?);
        let memory = u64::from_le_bytes(program[40..48].try_into()?);
        ensure!(
            at.checked_add(bytes).is_some_and(|end| end <= size),
            "ELF segment outside held file"
        );
        if kind == 1 {
            ensure!(
                bytes <= memory && address.checked_add(memory).is_some(),
                "malformed ELF load segment"
            );
            executable_entry |= flags & 1 != 0
                && entry >= address
                && address.checked_add(bytes).is_some_and(|end| entry < end);
        }
    }
    ensure!(
        executable_entry,
        "ELF entry not in an executable file segment"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Vec<u8> {
        let mut bytes = vec![0; 120];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&native_machine().unwrap().to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&64u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
        bytes[68..72].copy_from_slice(&1u32.to_le_bytes());
        bytes[96..104].copy_from_slice(&120u64.to_le_bytes());
        bytes[104..112].copy_from_slice(&120u64.to_le_bytes());
        bytes
    }

    fn check(bytes: &[u8]) -> Result<()> {
        let mut file = tempfile::tempfile()?;
        file.write_all(bytes)?;
        validate(&file)
    }

    #[test]
    fn refuses_interpreter_scripts_foreign_and_malformed_images() {
        let valid = image();
        check(&valid).unwrap();
        let mut dynamic = valid.clone();
        dynamic[64..68].copy_from_slice(&3u32.to_le_bytes());
        assert!(
            check(&dynamic)
                .unwrap_err()
                .to_string()
                .contains("PT_INTERP")
        );
        for (offset, replacement) in [
            (4, vec![1]),
            (5, vec![2]),
            (16, vec![1, 0]),
            (18, vec![0, 0]),
            (20, vec![0]),
            (52, vec![0]),
            (54, vec![0]),
            (56, vec![0xff, 0xff]),
            (32, u64::MAX.to_le_bytes().to_vec()),
            (72, u64::MAX.to_le_bytes().to_vec()),
            (80, u64::MAX.to_le_bytes().to_vec()),
            (96, 121u64.to_le_bytes().to_vec()),
            (104, 119u64.to_le_bytes().to_vec()),
            (24, 120u64.to_le_bytes().to_vec()),
        ] {
            let mut malformed = valid.clone();
            malformed[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(check(&malformed).is_err(), "offset {offset}");
        }
        assert!(check(&valid[..100]).is_err());
        assert!(check(b"#!/bin/false\n").is_err());
    }
}
