//! Finding a launch request's requester from outside its sandbox, and
//! proving it is the process that wrote the request
//! (docs/design/codex-sandboxed-launch.md, step 5).
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::ffi::OsString;
use std::path::Path;

/// The requester, seen from the launcher: its pid here, and its environment.
pub(super) struct Found {
    pub pid: u32,
    pub env: Vec<(OsString, OsString)>,
}

/// This process's identity as a requester records it: its pid, start
/// identity and PID namespace as it sees them itself.
#[cfg(target_os = "linux")]
pub(super) fn own() -> Result<Value> {
    Ok(serde_json::from_str(&crate::jobs::own_identity()?)?)
}

/// On macOS the start comes from the kernel read the ancestry walk uses,
/// which a Seatbelt sandbox allows (`ps`, which `jobs::identity` runs, it
/// denies), in the same `lstart` form.
#[cfg(target_os = "macos")]
pub(super) fn own() -> Result<Value> {
    let pid = std::process::id();
    let info = crate::crew_scope::ancestry::kinfo(pid)
        .with_context(|| format!("read this process's identity (sysctl kern.proc.pid.{pid})"))?
        .context("this process has no kernel identity")?;
    Ok(
        serde_json::json!({"pid": pid, "started": info.started, "pid_namespace": null, "zombie": false}),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn own() -> Result<Value> {
    Ok(serde_json::from_str(&crate::jobs::own_identity()?)?)
}

/// Whether two recorded identities name the same process.
pub(super) fn same_process(a: &Value, b: &Value) -> bool {
    a["pid"] == b["pid"] && a["started"] == b["started"] && a["pid_namespace"] == b["pid_namespace"]
}

/// The argv a bound requester runs: `_launch-wait` for this home and row.
pub(super) fn bound(argv: &[String], home: &Path, id: &str) -> bool {
    let Some(home) = home.to_str() else {
        return false;
    };
    argv.len() == 5 && argv[1..] == ["--home", home, "_launch-wait", id]
}

/// The last pid on a `/proc/P/status` `NSpid:` line: the pid in the
/// process's own, innermost PID namespace.
#[cfg(any(target_os = "linux", test))]
pub(super) fn nspid_innermost(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))?
        .split_whitespace()
        .last()?
        .parse()
        .ok()
}

/// The real uid on a `/proc/P/status` `Uid:` line, as this namespace sees it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn real_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// `NAME=value` strings split at their first `=`; others are skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn environ(raw: &[u8]) -> Vec<(OsString, OsString)> {
    use std::os::unix::ffi::OsStringExt;
    raw.split(|b| *b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|b| *b == b'=')?;
            (eq > 0).then(|| {
                (
                    OsString::from_vec(entry[..eq].to_vec()),
                    OsString::from_vec(entry[eq + 1..].to_vec()),
                )
            })
        })
        .collect()
}

/// Find `recorded` from here and prove it is the waiter of request `id` on
/// `home`: the same start identity, this user, and the bound argv.
#[cfg(target_os = "linux")]
pub(super) fn requester(recorded: &Value, home: &Path, id: &str) -> Result<Found> {
    use std::fs;
    let pid = recorded["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .context("the request names no process")?;
    let started = recorded["started"]
        .as_str()
        .context("the request names no start identity")?;
    let namespace = recorded["pid_namespace"].as_str();
    let here = match namespace {
        None => pid,
        Some(ns) if Some(ns) == crate::jobs::pid_namespace() => pid,
        Some(ns) => outer_pid(pid, ns)?,
    };
    let identity = || crate::jobs::process_identity(here);
    ensure!(
        identity()?.as_deref() == Some(started),
        "process {pid} has exited or its pid was reused"
    );
    let status = fs::read_to_string(format!("/proc/{here}/status"))
        .with_context(|| format!("read process {here}"))?;
    let uid = real_uid(&status).context("process status has no uid")?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        uid == unsafe { libc::geteuid() },
        "process {pid} belongs to another user"
    );
    let argv: Vec<String> = fs::read(format!("/proc/{here}/cmdline"))
        .with_context(|| format!("read the command line of process {here}"))?
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    ensure!(
        bound(&argv, home, id),
        "process {pid} is not waiting on this request"
    );
    let env = environ(
        &fs::read(format!("/proc/{here}/environ"))
            .with_context(|| format!("read the environment of process {here}"))?,
    );
    // Still the same process after the reads.
    ensure!(
        identity()?.as_deref() == Some(started),
        "process {pid} exited while it was being identified"
    );
    Ok(Found { pid: here, env })
}

/// The pid here of the process whose innermost pid is `pid` in namespace
/// `namespace`: exactly one must match.
#[cfg(target_os = "linux")]
fn outer_pid(pid: u32, namespace: &str) -> Result<u32> {
    let mut found = vec![];
    for entry in std::fs::read_dir("/proc").context("list processes")? {
        let Some(candidate) = entry
            .ok()
            .and_then(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        else {
            continue;
        };
        let in_namespace = std::fs::read_link(format!("/proc/{candidate}/ns/pid"))
            .is_ok_and(|link| link.to_str() == Some(namespace));
        if !in_namespace {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{candidate}/status")) else {
            continue;
        };
        if nspid_innermost(&status) == Some(pid) {
            found.push(candidate);
        }
    }
    match found.as_slice() {
        [one] => Ok(*one),
        [] => anyhow::bail!(
            "no process {pid} in PID namespace {namespace} is visible from the dispatcher"
        ),
        _ => anyhow::bail!("several processes claim pid {pid} in PID namespace {namespace}"),
    }
}

#[cfg(target_os = "macos")]
pub(super) fn requester(recorded: &Value, home: &Path, id: &str) -> Result<Found> {
    use std::os::unix::ffi::OsStringExt;
    let pid = recorded["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .context("the request names no process")?;
    ensure!(
        recorded["pid_namespace"].is_null(),
        "the request names a PID namespace, which macOS does not have"
    );
    let started = recorded["started"]
        .as_str()
        .context("the request names no start identity")?;
    let live = || -> Result<bool> {
        Ok(crate::crew_scope::ancestry::kinfo(pid)?
            .is_some_and(|info| !info.exiting && info.started == started))
    };
    ensure!(live()?, "process {pid} has exited or its pid was reused");
    // SAFETY: an all-zero proc_bsdinfo is valid plain data, and
    // proc_pidinfo writes at most its size into it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    ensure!(read == size, "cannot read the owner of process {pid}");
    // SAFETY: geteuid has no preconditions.
    ensure!(
        info.pbi_ruid == unsafe { libc::geteuid() },
        "process {pid} belongs to another user"
    );
    let (argv, env) = crate::crew_scope::ancestry::argv_env(pid)
        .with_context(|| format!("read the command line of process {pid}"))?;
    ensure!(
        bound(&argv, home, id),
        "process {pid} is not waiting on this request"
    );
    ensure!(
        live()?,
        "process {pid} exited while it was being identified"
    );
    Ok(Found {
        pid,
        env: env
            .into_iter()
            .map(|(k, v)| (OsString::from_vec(k), OsString::from_vec(v)))
            .collect(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn requester(_recorded: &Value, _home: &Path, _id: &str) -> Result<Found> {
    anyhow::bail!("launches for sandboxed callers are supported on Linux and macOS only")
}
