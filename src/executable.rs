//! Executable identities have two different lifetimes: supervisors must use
//! this runtime's image, while agents and persisted commands need an installed
//! path that another process can execute after an upgrade.
use anyhow::{Context, Result, ensure};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// A durable command path. Never search PATH or forward a process-relative
/// `/proc/self/exe` alias to a harness or a persisted peer/service record.
/// Replacing the installation is trusted just as invoking that path anew is;
/// this does not verify compatibility between different Privateer releases.
pub(crate) fn installed() -> Result<PathBuf> {
    let image = installed_path()?;
    let path = match crate::releases::Release::from_executable(&image) {
        Some(release) => release.entrypoint("privateer")?,
        None => image,
    };
    let recovery = || {
        format!(
            "Privateer executable {} is unavailable or not executable; restore the installation and restart the dispatcher",
            path.display()
        )
    };
    let metadata = fs::metadata(&path).with_context(recovery)?;
    let c_path = CString::new(path.as_os_str().as_bytes()).with_context(recovery)?;
    ensure!(
        metadata.is_file()
            && unsafe {
                libc::faccessat(
                    libc::AT_FDCWD,
                    c_path.as_ptr(),
                    libc::X_OK,
                    libc::AT_EACCESS,
                )
            } == 0,
        "{}",
        recovery()
    );
    Ok(path)
}

#[cfg(target_os = "linux")]
fn installed_path() -> Result<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::fs::MetadataExt;

    for _ in 0..3 {
        let reported = std::env::current_exe().context("cannot locate Privateer executable")?;
        let image = fs::metadata("/proc/self/exe")
            .context("cannot identify the running Privateer image")?;
        let still_linked = fs::metadata(&reported)
            .is_ok_and(|file| file.dev() == image.dev() && file.ino() == image.ino());
        if std::env::current_exe()? != reported {
            // A rename/unlink raced the lookup; do not mistake a literal
            // filename ending in ` (deleted)` for the kernel's marker.
            continue;
        }
        return Ok(
            match reported.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
                Some(original) if !still_linked => PathBuf::from(OsStr::from_bytes(original)),
                _ => reported,
            },
        );
    }
    anyhow::bail!(
        "Privateer executable changed during lookup; finish the installation and restart the dispatcher"
    )
}

#[cfg(not(target_os = "linux"))]
fn installed_path() -> Result<PathBuf> {
    std::env::current_exe().context("cannot locate Privateer executable")
}

/// On Linux, reexec the *running image* to keep the internal supervisor
/// protocol consistent across atomic replacement or removal. A proc alias is
/// safe here because it is used only by this process's immediate exec, never
/// serialized into a child agent's context. Other Unix platforms require the
/// installed executable instead.
pub(crate) fn supervisor() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        installed()
    }
}
