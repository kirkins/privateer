//! This process's ancestry, as the kernel reports it, without a subprocess
//! (docs/design/codex-sandboxed-callers.md).
//!
//! Linux reads `/proc`. macOS reads `sysctl kern.proc.pid.<pid>` for each
//! parent link and start time, and `sysctl kern.procargs2` for argv. Codex's
//! Seatbelt profile allows the first. No rule of it allows the second, yet
//! macOS CI read same-user argv under it, so a sandboxed caller may or may
//! not see its ancestors' command lines; `crew_scope` handles both.
// The kernel walk runs on macOS; elsewhere only its tests use it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use anyhow::Result;

/// Parent links followed from a caller to its supervisor.
pub(super) const DEPTH: usize = 64;

/// One ancestor of this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ancestor {
    pub pid: u32,
    /// Its start identity, in the form `jobs::process_identity` records,
    /// where the kernel read gives it (macOS). `None` defers the proof to
    /// `jobs::is_recorded_supervisor` (Linux).
    pub started: Option<String>,
    /// Its kernel command name (`p_comm`, at most 16 bytes), where known.
    pub comm: Option<String>,
    /// Its argv, or `None` when a sandbox hides it.
    pub argv: Option<Vec<String>>,
}

/// What one kernel read says about a live process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Kinfo {
    pub ppid: u32,
    pub started: String,
    pub comm: String,
    /// Whether it is exiting (`P_WEXIT`): its argv may already be torn down
    /// though it is not yet a zombie.
    pub exiting: bool,
}

/// The refusal for a kernel read the sandbox denies. It names a sandbox
/// limit: no Privateer permission or grant would lift it.
pub(super) fn hidden_ancestry(pid: u32, error: &std::io::Error) -> String {
    format!(
        "this caller's sandbox hides its process ancestry (sysctl kern.proc.pid for process {pid}: {error}), so Privateer cannot tell which job it runs in and refuses the command. This is a limit of the sandbox, not a missing Privateer permission: run the command outside the sandbox."
    )
}

/// Walk from `me` to its ancestors, nearest first, through `kinfo` (one
/// kernel read; `Ok(None)` once the process has gone) and `argv` (an error
/// is a hidden argv, or the end of the chain if the process has gone
/// meanwhile). A read the kernel refuses is a refusal, never an empty
/// ancestry.
pub(crate) fn walk(
    me: u32,
    kinfo: impl Fn(u32) -> std::io::Result<Option<Kinfo>>,
    argv: impl Fn(u32) -> std::io::Result<Vec<String>>,
) -> Result<Vec<Ancestor>> {
    let read = |pid: u32| -> Result<Option<Kinfo>> {
        match kinfo(pid) {
            Ok(info) => Ok(info),
            Err(error) => Err(super::SandboxLimit(hidden_ancestry(pid, &error)).into()),
        }
    };
    let mut found = vec![];
    let Some(mut info) = read(me)? else {
        return Ok(found);
    };
    // The process whose parent `info.ppid` is.
    let mut child = me;
    for _ in 0..DEPTH {
        let pid = info.ppid;
        if pid <= 1 {
            break;
        }
        let Some(next) = read(pid)? else {
            break;
        };
        let args = match argv(pid) {
            Ok(args) => Some(args),
            // Still there, not exiting and still its child's parent: the
            // argv is hidden, never taken as empty. Gone since its child
            // named it, exiting (the kernel tears an exiting process's argv
            // down before it is a zombie), its pid already reused, or no
            // longer the parent of the process below it: the chain ends
            // here, as it does on Linux, and nothing above it is read as
            // this caller's ancestry.
            Err(_) => match read(pid)? {
                Some(again)
                    if again.started == next.started
                        && !again.exiting
                        && read(child)?.is_some_and(|below| below.ppid == pid) =>
                {
                    None
                }
                _ => break,
            },
        };
        found.push(Ancestor {
            pid,
            started: Some(next.started.clone()),
            comm: Some(next.comm.clone()),
            argv: args,
        });
        child = pid;
        info = next;
    }
    Ok(found)
}

/// A start time in seconds since the epoch, formatted as `ps -o lstart=`
/// prints it with `TZ=UTC0 LC_ALL=C` and `jobs::identity` records it: the
/// C locale's `%c` (`%a %b %e %H:%M:%S %Y`), whitespace collapsed.
pub(crate) fn lstart(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let of_day = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{} {} {} {:02}:{:02}:{:02} {}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        day,
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        year
    )
}

/// `struct kinfo_proc` as 64-bit macOS lays it out (`<sys/sysctl.h>`,
/// `<sys/proc.h>`): only the fields read here are named.
#[repr(C)]
#[allow(dead_code)]
pub(crate) struct KinfoProc {
    /// `kp_proc.p_un.__p_starttime`.
    start_sec: i64,
    start_usec: i32,
    _a: [u8; 20],
    /// `kp_proc.p_flag`, where the kernel reports an exiting process
    /// (`P_LEXIT`) as `P_WEXIT`.
    flag: i32,
    /// `kp_proc.p_stat`.
    stat: i8,
    _b: [u8; 3],
    /// `kp_proc.p_pid`.
    pid: i32,
    _c: [u8; 199],
    /// `kp_proc.p_comm`, `MAXCOMLEN + 1` bytes.
    comm: [u8; 17],
    _d: [u8; 300],
    /// `kp_eproc.e_ppid`.
    ppid: i32,
    _e: [u8; 84],
}
const _: () = {
    assert!(std::mem::size_of::<KinfoProc>() == 648);
    assert!(std::mem::offset_of!(KinfoProc, flag) == 32);
    assert!(std::mem::offset_of!(KinfoProc, stat) == 36);
    assert!(std::mem::offset_of!(KinfoProc, pid) == 40);
    assert!(std::mem::offset_of!(KinfoProc, comm) == 243);
    assert!(std::mem::offset_of!(KinfoProc, ppid) == 560);
};

/// `p_stat` of a zombie (`SZOMB`).
const SZOMB: i8 = 5;
/// `p_flag` of an exiting process (`P_WEXIT`, `<sys/proc.h>`).
const P_WEXIT: i32 = 0x0000_2000;

impl KinfoProc {
    /// The identity a raw record gives, or `None` for a process that is not
    /// there (an empty reply) or is a zombie.
    pub(crate) fn kinfo(&self) -> Option<Kinfo> {
        if self.pid <= 0 || self.stat == SZOMB {
            return None;
        }
        let end = self.comm.iter().position(|b| *b == 0).unwrap_or(16);
        Some(Kinfo {
            ppid: u32::try_from(self.ppid).ok()?,
            started: lstart(self.start_sec),
            comm: String::from_utf8_lossy(&self.comm[..end]).into_owned(),
            exiting: self.flag & P_WEXIT != 0,
        })
    }
}

/// argv from a `kern.procargs2` reply: `argc`, the executable path, padding
/// NULs, then `argc` NUL-terminated arguments.
pub(crate) fn procargs(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let rest = &buffer[4..];
    let path_end = rest.iter().position(|b| *b == 0)?;
    let mut at = path_end;
    // An empty argv[0] (and any empty arguments right after it) cannot be
    // told from this padding, so it is skipped too: the list then starts at
    // the first non-empty argument and takes as many environment strings
    // after the arguments to fill argc.
    while rest.get(at) == Some(&0) {
        at += 1;
    }
    let mut args = vec![];
    for arg in rest[at..]
        .split(|b| *b == 0)
        .take(usize::try_from(argc).ok()?)
    {
        args.push(String::from_utf8_lossy(arg).into_owned());
    }
    Some(args)
}

/// argv and the environment from a `kern.procargs2` reply: the environment
/// is the NUL-terminated `NAME=value` strings after the arguments, up to the
/// first empty one. A string without `=` there is not environment and ends
/// the list.
/// `(NAME, value)` pairs, as raw bytes.
pub(crate) type Environment = Vec<(Vec<u8>, Vec<u8>)>;

pub(crate) fn procargs_env(buffer: &[u8]) -> Option<(Vec<String>, Environment)> {
    let argv = procargs(buffer)?;
    let argc = usize::try_from(i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &buffer[4..];
    let mut at = rest.iter().position(|b| *b == 0)?;
    while rest.get(at) == Some(&0) {
        at += 1;
    }
    let mut strings = rest[at..].split(|b| *b == 0).skip(argc);
    let mut env = vec![];
    for entry in strings.by_ref() {
        let Some(eq) = entry.iter().position(|b| *b == b'=') else {
            break;
        };
        env.push((entry[..eq].to_vec(), entry[eq + 1..].to_vec()));
    }
    Some((argv, env))
}

#[cfg(target_os = "macos")]
mod kernel {
    use super::{Kinfo, KinfoProc, procargs, procargs_env};

    /// One `sysctl kern.proc.pid.<pid>` read.
    pub(crate) fn kinfo(pid: u32) -> std::io::Result<Option<Kinfo>> {
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        // SAFETY: an all-zero kinfo_proc is valid plain data; sysctl writes
        // at most `size` bytes into it and reports how many.
        let mut record: KinfoProc = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<KinfoProc>();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                (&mut record as *mut KinfoProc).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if size < std::mem::size_of::<KinfoProc>() {
            return Ok(None);
        }
        Ok(record.kinfo())
    }

    /// One `sysctl kern.procargs2.<pid>` read.
    pub(crate) fn argv(pid: u32) -> std::io::Result<Vec<String>> {
        let buffer = procargs2(pid)?;
        procargs(&buffer).ok_or_else(|| std::io::Error::other("malformed kern.procargs2 reply"))
    }

    /// A process's environment as raw `(name, value)` pairs.
    pub(crate) type EnvPairs = Vec<(Vec<u8>, Vec<u8>)>;

    /// argv and the environment of `pid`, from one `kern.procargs2` read.
    pub(crate) fn argv_env(pid: u32) -> std::io::Result<(Vec<String>, EnvPairs)> {
        let buffer = procargs2(pid)?;
        procargs_env(&buffer).ok_or_else(|| std::io::Error::other("malformed kern.procargs2 reply"))
    }

    fn procargs2(pid: u32) -> std::io::Result<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size = 0usize;
        // SAFETY: a null buffer asks only for the size.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; size];
        // SAFETY: sysctl writes at most `size` bytes into the buffer.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(size);
        Ok(buffer)
    }
}
#[cfg(target_os = "macos")]
pub(crate) use kernel::{argv, argv_env, kinfo};

/// This process's ancestors, nearest first.
pub(crate) fn ancestors() -> Result<Vec<Ancestor>> {
    ancestors_of(std::process::id())
}

/// The ancestors of process `pid`, nearest first.
#[cfg(target_os = "macos")]
pub(crate) fn ancestors_of(pid: u32) -> Result<Vec<Ancestor>> {
    walk(pid, kinfo, argv)
}

/// This process's kernel command name.
#[cfg(target_os = "macos")]
pub(crate) fn own_comm() -> Option<String> {
    kinfo(std::process::id())
        .ok()
        .flatten()
        .map(|info| info.comm)
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn own_comm() -> Option<String> {
    None
}

/// The ancestors of process `pid`, nearest first, with their argv.
#[cfg(target_os = "linux")]
pub(crate) fn ancestors_of(start: u32) -> Result<Vec<Ancestor>> {
    use anyhow::Context;
    let parent = |pid: u32| -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the command's final ')' start at the state; the
        // parent PID follows it.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let mut found = vec![];
    let mut pid = start;
    for _ in 0..DEPTH {
        match parent(pid) {
            Some(next) if next > 1 => pid = next,
            _ => break,
        }
        // A process that exited since its child named it has no argv to
        // read; any other failure is not "no crew launch" and is reported.
        let argv = match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(argv) => argv,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(libc::ESRCH) =>
            {
                Vec::new()
            }
            Err(e) => return Err(e).with_context(|| format!("read argv of ancestor {pid}")),
        };
        found.push(Ancestor {
            pid,
            started: None,
            comm: None,
            argv: Some(
                argv.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect(),
            ),
        });
    }
    Ok(found)
}

/// Other Unix systems: one process table from ps, as before. ps joins argv
/// with spaces, so a home path with a space is not recognised.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn ancestors_of(start: u32) -> Result<Vec<Ancestor>> {
    use anyhow::{Context, ensure};
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,ppid=,args="])
        .env("LC_ALL", "C")
        .output()
        .context("inspect process ancestry")?;
    ensure!(
        output.status.success(),
        "process ancestry inspection failed"
    );
    let mut table = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(Ok(pid)), Some(Ok(parent))) = (
            fields.next().map(str::parse::<u32>),
            fields.next().map(str::parse::<u32>),
        ) else {
            continue;
        };
        table.insert(pid, (parent, fields.map(str::to_owned).collect::<Vec<_>>()));
    }
    let mut found = vec![];
    let mut pid = start;
    for _ in 0..DEPTH {
        match table.get(&pid) {
            Some((next, _)) if *next > 1 => pid = *next,
            _ => break,
        }
        let argv = table.get(&pid).map(|(_, a)| a.clone()).unwrap_or_default();
        found.push(Ancestor {
            pid,
            started: None,
            comm: None,
            argv: Some(argv),
        });
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn info(ppid: u32, started: &str, comm: &str) -> Kinfo {
        Kinfo {
            ppid,
            started: started.into(),
            comm: comm.into(),
            exiting: false,
        }
    }

    #[test]
    fn lstart_matches_ps_in_the_c_locale() {
        // References from `date -u -d @N '+%a %b %e %H:%M:%S %Y'`, collapsed.
        assert_eq!(lstart(0), "Thu Jan 1 00:00:00 1970");
        assert_eq!(lstart(951_782_400), "Tue Feb 29 00:00:00 2000");
        assert_eq!(lstart(1_793_848_699), "Thu Nov 5 03:18:19 2026");
        assert_eq!(lstart(-1), "Wed Dec 31 23:59:59 1969");
    }

    #[test]
    fn the_walk_follows_parents_to_init_with_their_argv() {
        let table = HashMap::from([
            (40, info(30, "s40", "privateer")),
            (30, info(20, "s30", "codex")),
            (20, info(1, "s20", "privateer")),
        ]);
        let found = walk(
            40,
            |pid| Ok(table.get(&pid).cloned()),
            |pid| Ok(vec![format!("argv-{pid}")]),
        )
        .unwrap();
        assert_eq!(
            found.iter().map(|a| a.pid).collect::<Vec<_>>(),
            vec![30, 20]
        );
        assert_eq!(found[0].argv, Some(vec!["argv-30".to_owned()]));
        assert_eq!(found[1].started.as_deref(), Some("s20"));
        assert_eq!(found[1].comm.as_deref(), Some("privateer"));
    }

    #[test]
    fn a_hidden_argv_is_never_read_as_empty() {
        let table = HashMap::from([
            (40, info(30, "s40", "sh")),
            (30, info(1, "s30", "privateer")),
        ]);
        let found = walk(
            40,
            |pid| Ok(table.get(&pid).cloned()),
            |_| Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].argv, None);
    }

    #[test]
    fn an_ancestor_gone_before_its_argv_is_read_ends_the_chain() {
        // 30 exits between its kinfo and argv reads: neither it nor the
        // process above it is reported, as Linux stops at a gone ancestor.
        let table = std::cell::RefCell::new(HashMap::from([
            (50, info(40, "s50", "sh")),
            (40, info(30, "s40", "sh")),
            (30, info(20, "s30", "sh")),
            (20, info(1, "s20", "privateer")),
        ]));
        let found = walk(
            50,
            |pid| Ok(table.borrow().get(&pid).cloned()),
            |pid| {
                if pid == 30 {
                    table.borrow_mut().remove(&pid);
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(vec![format!("argv-{pid}")])
            },
        )
        .unwrap();
        assert_eq!(found.iter().map(|a| a.pid).collect::<Vec<_>>(), vec![40]);
        assert_eq!(found[0].argv, Some(vec!["argv-40".to_owned()]));
        // A pid reused by another process meanwhile ends it too.
        let restarted = std::cell::RefCell::new(HashMap::from([
            (40, info(30, "s40", "sh")),
            (30, info(20, "s30", "sh")),
            (20, info(1, "s20", "privateer")),
        ]));
        let found = walk(
            40,
            |pid| Ok(restarted.borrow().get(&pid).cloned()),
            |pid| {
                if pid == 30 {
                    restarted.borrow_mut().insert(30, info(20, "later", "sh"));
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(vec![])
            },
        )
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_exiting_or_departed_parent_whose_argv_is_gone_ends_the_chain() {
        // A detached runner's Privateer parent replies and exits while the
        // runner walks its ancestry (macOS CI, #732): the kernel has torn
        // its argv down but it is not yet a zombie. It is not a hidden
        // ancestor, so the runner is not refused as sandboxed.
        let exiting = HashMap::from([
            (40, info(30, "s40", "privateer")),
            (
                30,
                Kinfo {
                    exiting: true,
                    ..info(20, "s30", "privateer")
                },
            ),
            (20, info(1, "s20", "sh")),
        ]);
        let found = walk(
            40,
            |pid| Ok(exiting.get(&pid).cloned()),
            |pid| match pid {
                30 => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
                _ => Ok(vec![format!("argv-{pid}")]),
            },
        )
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
        // The parent's argv read failed and the walker has meanwhile been
        // reparented: the failed read is not this caller's ancestry either.
        let table = std::cell::RefCell::new(HashMap::from([
            (40, info(30, "s40", "privateer")),
            (30, info(20, "s30", "privateer")),
            (20, info(1, "s20", "sh")),
        ]));
        let found = walk(
            40,
            |pid| Ok(table.borrow().get(&pid).cloned()),
            |pid| {
                if pid == 30 {
                    table.borrow_mut().insert(40, info(1, "s40", "privateer"));
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(vec![format!("argv-{pid}")])
            },
        )
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
        // A live parent that is still the walker's stays hidden, however far
        // up the chain it is.
        let hidden = HashMap::from([
            (50, info(40, "s50", "sh")),
            (40, info(30, "s40", "sh")),
            (30, info(1, "s30", "privateer")),
        ]);
        let found = walk(
            50,
            |pid| Ok(hidden.get(&pid).cloned()),
            |pid| match pid {
                30 => Err(std::io::Error::from_raw_os_error(libc::EPERM)),
                _ => Ok(vec![format!("argv-{pid}")]),
            },
        )
        .unwrap();
        assert_eq!(
            found.iter().map(|a| a.pid).collect::<Vec<_>>(),
            vec![40, 30]
        );
        assert_eq!(found[1].argv, None);
    }

    #[test]
    fn a_denied_kernel_read_is_a_sandbox_refusal_not_a_crash() {
        let table = HashMap::from([(40, info(30, "s40", "sh"))]);
        let error = walk(
            40,
            |pid| match table.get(&pid) {
                Some(info) => Ok(Some(info.clone())),
                None => Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            },
            |_| Ok(vec![]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("sandbox hides its process ancestry"),
            "{error}"
        );
        assert!(error.contains("process 30"), "{error}");
        assert!(
            error.contains("not a missing Privateer permission"),
            "{error}"
        );
        assert!(!error.contains("permissions.blocked"), "{error}");
        // Even this process's own record.
        let error = walk(
            40,
            |_| Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            |_| Ok(vec![]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("process 40"), "{error}");
    }

    #[test]
    fn the_walk_is_bounded() {
        // A parent loop never ends on its own.
        let found = walk(40, |pid| Ok(Some(info(pid + 1, "s", "x"))), |_| Ok(vec![])).unwrap();
        assert_eq!(found.len(), DEPTH);
    }

    #[test]
    fn procargs_skips_the_executable_path_and_its_padding() {
        let mut reply = 2i32.to_ne_bytes().to_vec();
        reply.extend_from_slice(b"/opt/bin/privateer\0\0\0\0privateer\0--home /a b\0FOO=env\0");
        assert_eq!(
            procargs(&reply),
            Some(vec!["privateer".to_owned(), "--home /a b".to_owned()])
        );
        assert_eq!(procargs(b"\x01"), None);
    }

    #[test]
    fn procargs_env_reads_the_environment_after_the_arguments() {
        let mut reply = 2i32.to_ne_bytes().to_vec();
        reply.extend_from_slice(
            b"/opt/bin/privateer\0\0\0\0privateer\0_launch-wait\0HOME=/Users/harbor\0A=x=y\0\0\0ptr_munge=\0",
        );
        let (argv, env) = procargs_env(&reply).unwrap();
        assert_eq!(argv, ["privateer", "_launch-wait"]);
        assert_eq!(
            env,
            [
                (b"HOME".to_vec(), b"/Users/harbor".to_vec()),
                (b"A".to_vec(), b"x=y".to_vec())
            ]
        );
        assert_eq!(procargs_env(b"\x01"), None);
    }

    #[test]
    fn a_kinfo_record_reads_its_fields_and_skips_zombies() {
        // SAFETY: all-zero is valid plain data.
        let mut record: KinfoProc = unsafe { std::mem::zeroed() };
        assert_eq!(record.kinfo(), None, "an empty reply is no process");
        record.pid = 42;
        record.ppid = 7;
        record.start_sec = 951_782_400;
        record.comm[..9].copy_from_slice(b"privateer");
        assert_eq!(
            record.kinfo(),
            Some(info(7, "Tue Feb 29 00:00:00 2000", "privateer"))
        );
        record.flag = P_WEXIT;
        assert!(record.kinfo().unwrap().exiting);
        record.stat = SZOMB;
        assert_eq!(record.kinfo(), None);
    }

    /// Only macOS CI runs these: the kernel reads against `ps` and libc.
    #[cfg(target_os = "macos")]
    mod macos {
        use super::super::*;

        #[test]
        fn the_kernel_read_agrees_with_ps_and_getppid() {
            let me = std::process::id();
            let own = kinfo(me).unwrap().expect("this process");
            assert_eq!(own.ppid, unsafe { libc::getppid() } as u32);
            // The proof compares this string with what supervisors record
            // through `ps -o lstart=`; they must be byte for byte the same.
            assert_eq!(
                Some(own.started.clone()),
                crate::jobs::process_identity(me).unwrap()
            );
            let parent = kinfo(own.ppid).unwrap().expect("the parent");
            assert_eq!(
                Some(parent.started),
                crate::jobs::process_identity(own.ppid).unwrap()
            );
            assert!(!own.comm.is_empty());
            assert_eq!(own_comm(), Some(own.comm));
            assert_eq!(argv(me).unwrap(), std::env::args().collect::<Vec<_>>());
            let found = ancestors().unwrap();
            assert_eq!(found.first().map(|a| a.pid), Some(own.ppid));
            // Unsandboxed, the parent's argv (cargo's) is visible; an
            // ancestor of another user farther up may not be.
            assert!(
                found[0].argv.as_ref().is_some_and(|a| !a.is_empty()),
                "{found:?}"
            );
        }

        #[test]
        fn a_missing_pid_reads_as_gone() {
            // Above the default pid_max, so no process holds it.
            assert_eq!(kinfo(99_999_999).unwrap(), None);
        }

        /// The rules of Codex's `workspace-write` base profile that decide
        /// ancestry (codex-rs/sandboxing/src/seatbelt_base_policy.sbpl at
        /// openai/codex 696b4502, read 2026-11-04; its `sysctl-read` list is
        /// copied verbatim), with its full-disk read access.
        const CODEX_LIKE_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow user-preference-read)
(allow file-read*)
(allow file-write-data (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))
(allow sysctl-read
  (sysctl-name "hw.activecpu") (sysctl-name "hw.busfrequency_compat") (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cacheconfig") (sysctl-name "hw.cachelinesize_compat") (sysctl-name "hw.cpufamily")
  (sysctl-name "hw.cpufrequency_compat") (sysctl-name "hw.cputype") (sysctl-name "hw.l1dcachesize_compat")
  (sysctl-name "hw.l1icachesize_compat") (sysctl-name "hw.l2cachesize_compat") (sysctl-name "hw.l3cachesize_compat")
  (sysctl-name "hw.logicalcpu_max") (sysctl-name "hw.machine") (sysctl-name "hw.model") (sysctl-name "hw.memsize")
  (sysctl-name "hw.ncpu") (sysctl-name "hw.nperflevels") (sysctl-name "hw.packages") (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.pagesize") (sysctl-name "hw.physicalcpu") (sysctl-name "hw.physicalcpu_max")
  (sysctl-name "hw.logicalcpu") (sysctl-name "hw.cpufrequency") (sysctl-name "hw.tbfrequency_compat")
  (sysctl-name "hw.vectorunit") (sysctl-name "machdep.cpu.brand_string") (sysctl-name "kern.argmax")
  (sysctl-name "kern.hostname") (sysctl-name "kern.maxfilesperproc") (sysctl-name "kern.maxproc")
  (sysctl-name "kern.osproductversion") (sysctl-name "kern.osrelease") (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status") (sysctl-name "kern.osversion") (sysctl-name "kern.secure_kernel")
  (sysctl-name "kern.sysv.semmns") (sysctl-name "kern.usrstack64") (sysctl-name "kern.version")
  (sysctl-name "sysctl.proc_cputype") (sysctl-name "vm.loadavg")
  (sysctl-name-prefix "hw.optional.arm.") (sysctl-name-prefix "hw.optional.armv8_")
  (sysctl-name-prefix "hw.perflevel") (sysctl-name-prefix "kern.proc.pgrp.")
  (sysctl-name-prefix "kern.proc.pid.") (sysctl-name-prefix "net.routetable."))
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
(allow ipc-posix-sem)
"#;

        /// Run inside the sandbox by the test below; writes one PROBE line to
        /// stderr (libtest's own `test ... ` has no newline on stdout).
        #[test]
        #[ignore = "run under sandbox-exec by a_codex_like_sandbox_denies_ps_but_not_the_kernel_walk"]
        fn sandbox_probe() {
            if std::env::var_os("PRIVATEER_732_PROBE").is_none() {
                return;
            }
            let ps = std::process::Command::new("/bin/ps")
                .args(["-axww", "-o", "pid=,ppid=,args="])
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false);
            let walked = match ancestors() {
                Ok(found) => serde_json::json!(
                    found
                        .iter()
                        .map(|a| serde_json::json!({
                            "pid": a.pid, "started": a.started, "comm": a.comm,
                            "argv": a.argv,
                        }))
                        .collect::<Vec<_>>()
                ),
                Err(error) => serde_json::json!(format!("{error:#}")),
            };
            // The resolver's own ancestry step, as a sandboxed CLI runs it.
            let resolved = match crate::crew_scope::supervising_job(None) {
                Ok(found) => serde_json::json!({"job": found.map(|(_, job, _)| job)}),
                Err(error) => serde_json::json!({"error": format!("{error:#}")}),
            };
            eprintln!(
                "\nPROBE {}",
                serde_json::json!({"ps_ok": ps, "ancestors": walked, "resolved": resolved})
            );
        }

        /// Codex's profile denies `ps` (#732) but allows the kernel walk.
        /// Its argv reads are not asserted either way: no rule allows
        /// `kern.procargs2`, yet macOS CI (run 41963181277) read every
        /// same-user ancestor's argv under it. Resolution must hold in both
        /// cases (docs/design/codex-sandboxed-callers.md).
        #[test]
        fn a_codex_like_sandbox_denies_ps_but_not_the_kernel_walk() {
            let sandbox = std::path::Path::new("/usr/bin/sandbox-exec");
            assert!(sandbox.exists(), "macOS ships {}", sandbox.display());
            let output = std::process::Command::new(sandbox)
                .arg("-p")
                .arg(CODEX_LIKE_PROFILE)
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "crew_scope::ancestry::tests::macos::sandbox_probe",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("PRIVATEER_732_PROBE", "1")
                .env_remove("PRIVATEER_JOB")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let line = stderr
                .lines()
                .chain(stdout.lines())
                .find_map(|l| l.find("PROBE {").map(|at| &l[at + "PROBE ".len()..]))
                .unwrap_or_else(|| {
                    panic!(
                        "the probe did not report: {:?}\nstdout: {stdout}\nstderr: {stderr}",
                        output.status
                    )
                });
            let probe: serde_json::Value = serde_json::from_str(line).unwrap();
            // The profile reproduces #732: ps cannot read the process table.
            assert_eq!(probe["ps_ok"], false, "{probe}");
            // sandbox-exec execs the probe, so this test process is its parent,
            // outside the sandbox: read by pid, start and command name.
            let me = std::process::id();
            let own = kinfo(me).unwrap().unwrap();
            let parent = &probe["ancestors"][0];
            assert_eq!(parent["pid"], me, "{probe}");
            assert_eq!(parent["started"], own.started.as_str(), "{probe}");
            assert_eq!(parent["comm"], own.comm.as_str(), "{probe}");
            let resolved = &probe["resolved"];
            if parent["argv"].is_null() {
                // Hidden: the parent carries this CLI's command name and no
                // record names it, so the caller is refused as a sandbox
                // limit, never resolved as having no supervisor.
                let error = resolved["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{probe}"));
                assert!(
                    error.contains("not a missing Privateer permission"),
                    "{probe}"
                );
                assert!(!error.contains("permissions.blocked"), "{probe}");
            } else {
                // Visible: the exact argv, so today's rule applies. This test
                // process is no supervisor, and resolution does not error.
                let argv: Vec<String> = std::env::args().collect();
                assert_eq!(parent["argv"], serde_json::json!(argv), "{probe}");
                assert!(resolved.get("error").is_none(), "{probe}");
            }
        }
    }
}
