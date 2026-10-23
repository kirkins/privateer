//! What a live process runs, as the kernel reports it: its executable and
//! working directory, resolved, never the path it was invoked by (which on
//! macOS may go through today's `current`). Linux reads `/proc`; macOS asks
//! libproc.
use std::path::PathBuf;

/// The executable image of a live process; None once it has exited or when
/// it is not this user's to inspect.
pub(crate) fn image(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let image = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let text = image.to_string_lossy();
        Some(PathBuf::from(
            text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned(),
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if len <= 0 {
            return None;
        }
        buffer.truncate(len as usize);
        Some(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
    }
}

/// The working directory of a live process, where it is readable.
pub(crate) fn cwd(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if got != size {
            return None;
        }
        let path = &info.pvi_cdir.vip_path;
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), std::mem::size_of_val(path))
        };
        let end = bytes.iter().position(|b| *b == 0)?;
        (end > 0).then(|| PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec())))
    }
}

/// Every live process id.
pub(crate) fn all() -> std::io::Result<Vec<i32>> {
    #[cfg(target_os = "linux")]
    {
        Ok(std::fs::read_dir("/proc")?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .collect())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Room for processes started between the two calls.
        let mut pids: Vec<libc::pid_t> = vec![0; count as usize + 256];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        pids.truncate(count as usize);
        Ok(pids.into_iter().filter(|pid| *pid > 0).collect())
    }
}
