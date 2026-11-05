//! Finding a launch request's requester from outside its sandbox, and
//! proving it is the process that wrote the request
//! (docs/design/codex-sandboxed-launch.md, step 5).
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The largest environment a launcher adopts: its variables, and their
/// `NAME=value` bytes. Codex's shells carry a few hundred variables; macOS
/// caps argv and the environment together at 1 MiB (`kern.argmax`).
pub(super) const ENV_ENTRIES: usize = 4096;
pub(super) const ENV_BYTES: usize = 1 << 20;

/// The requester, seen from the launcher: its pid here, its environment and
/// its working directory, all read from the kernel.
pub(super) struct Found {
    pub pid: u32,
    pub env: Vec<(OsString, OsString)>,
    /// Its working directory, where this process reaches the same directory
    /// by that path; `None` sends the command to the home.
    pub cwd: Option<PathBuf>,
}

/// This process's identity as a requester records it: its pid, start
/// identity and PID namespace as it sees them itself. On macOS the start
/// comes from `sysctl kern.proc.pid`, which a Seatbelt sandbox allows.
pub(super) fn own() -> Result<Value> {
    Ok(serde_json::from_str(&crate::jobs::own_identity()?)?)
}

/// Whether two recorded identities name the same process.
pub(super) fn same_process(a: &Value, b: &Value) -> bool {
    a["pid"] == b["pid"] && a["started"] == b["started"] && a["pid_namespace"] == b["pid_namespace"]
}

/// The argv a bound requester runs: `_launch-wait` for this home and row,
/// with the digest of the row's content it recorded (`launch::digest`).
pub(super) fn bound(argv: &[String], home: &Path, id: &str, digest: &str) -> bool {
    let Some(home) = home.to_str() else {
        return false;
    };
    argv.len() == 6 && argv[1..] == ["--home", home, "_launch-wait", id, digest]
}

/// Refuse an environment larger than a launcher adopts.
pub(super) fn ensure_bounded(env: &[(OsString, OsString)]) -> Result<()> {
    let bytes: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
    ensure!(
        env.len() <= ENV_ENTRIES && bytes <= ENV_BYTES,
        "its environment ({} variables, {bytes} bytes) is larger than a launcher adopts (at most {ENV_ENTRIES} variables and {ENV_BYTES} bytes)",
        env.len()
    );
    Ok(())
}

/// `path`, if this process reaches by it the directory whose device and
/// inode the kernel reported for the requester's working directory: a
/// sandbox may show a path that is another directory, or none, outside.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn same_directory(path: PathBuf, dev: u64, ino: u64) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let ours = std::fs::metadata(&path).ok()?;
    (ours.is_dir() && ours.dev() == dev && ours.ino() == ino).then_some(path)
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

fn recorded_pid(recorded: &Value) -> Result<u32> {
    recorded["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .context("the request names no process")
}

fn recorded_start(recorded: &Value) -> Result<&str> {
    recorded["started"]
        .as_str()
        .context("the request names no start identity")
}

/// Whether the process at `here` is still the live, not exiting, process
/// whose start identity is `started`.
#[cfg(target_os = "linux")]
fn live(here: u32, started: &str) -> Result<bool> {
    Ok(crate::jobs::process_identity(here)?.as_deref() == Some(started))
}

#[cfg(target_os = "macos")]
fn live(here: u32, started: &str) -> Result<bool> {
    Ok(crate::crew_scope::ancestry::kinfo(here)?
        .is_some_and(|info| !info.exiting && info.started == started))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn live(_here: u32, _started: &str) -> Result<bool> {
    Ok(false)
}

/// After its caller is resolved: the requester found as `found` is still the
/// process that `recorded` names. A requester that exited, or whose pid was
/// reused, while its ancestry was walked has no proven caller, whatever the
/// walk returned.
pub(super) fn ensure_still(recorded: &Value, found: &Found) -> Result<()> {
    let pid = recorded_pid(recorded)?;
    ensure!(
        live(found.pid, recorded_start(recorded)?)?,
        "process {pid} exited while its caller was being resolved"
    );
    Ok(())
}

/// Whether a sandboxed requester's ancestry leaves its sandbox: some
/// ancestor in `chain` (pids as seen here, nearest first) below init is one
/// `outside` says is outside the requester's sandbox. `None` (gone, or not
/// knowable) never counts as outside. A requester whose chain does not
/// leave was not started by anything outside its sandbox (macOS: a process
/// that double-forked out of its job's tree, which launchd adopted), so the
/// walk cannot tell which job it runs in (docs/design/codex-sandboxed-launch.md,
/// (c)).
pub(super) fn leaves(chain: &[u32], outside: impl Fn(u32) -> Option<bool>) -> bool {
    chain
        .iter()
        .any(|&pid| pid > 1 && outside(pid) == Some(true))
}

/// The refusal reason for a sandboxed requester whose chain never leaves
/// its sandbox.
pub(super) fn never_leaves(pid: u32, sandbox: &str) -> String {
    format!(
        "process {pid} runs in {sandbox}, and no ancestor of it below init is outside that sandbox, so nothing outside the sandbox started it and its job cannot be told"
    )
}

/// Linux: a requester in another PID namespace than this launcher's must
/// have an ancestor (below init, as seen here) outside that namespace.
#[cfg(target_os = "linux")]
pub(super) fn ensure_leaves_sandbox(found: &Found, _claimed_sandbox: &str) -> Result<()> {
    let namespace = |pid: u32| {
        std::fs::read_link(format!("/proc/{pid}/ns/pid"))
            .ok()
            .map(|link| link.to_string_lossy().into_owned())
    };
    let theirs =
        namespace(found.pid).with_context(|| format!("process {} has exited", found.pid))?;
    if Some(theirs.as_str()) == crate::jobs::pid_namespace() {
        return Ok(());
    }
    let chain: Vec<u32> = crate::crew_scope::ancestry::ancestors_of(found.pid)?
        .iter()
        .map(|a| a.pid)
        .collect();
    ensure!(
        leaves(&chain, |pid| namespace(pid).map(|ns| ns != theirs)),
        "{}",
        never_leaves(found.pid, "its own PID namespace")
    );
    Ok(())
}

/// Whether process `pid` runs in a Seatbelt sandbox, by libsystem_sandbox's
/// `sandbox_check` with no operation: `None` where it cannot say (the
/// symbol is absent, or the call fails).
#[cfg(target_os = "macos")]
pub(super) fn seatbelt_state(pid: u32) -> Option<bool> {
    type Check =
        unsafe extern "C" fn(libc::pid_t, *const libc::c_char, libc::c_int, ...) -> libc::c_int;
    // SAFETY: a NUL-terminated name; a null result means the symbol is absent.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"sandbox_check".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: libsystem_sandbox declares `int sandbox_check(pid_t, const char
    // *operation, enum sandbox_filter_type, ...)`; a null operation with
    // SANDBOX_FILTER_NONE (0) asks only whether the process is sandboxed.
    let check: Check = unsafe { std::mem::transmute::<*mut libc::c_void, Check>(symbol) };
    match unsafe { check(pid as libc::pid_t, std::ptr::null(), 0) } {
        rc if rc < 0 => None,
        rc => Some(rc > 0),
    }
}

/// macOS: a Seatbelt-sandboxed requester must have an unsandboxed ancestor
/// below launchd, checked with `sandbox_check` on each ancestor from
/// outside. A requester the kernel cannot judge counts as sandboxed when its
/// request says so: the claim may add this check, never skip it.
#[cfg(target_os = "macos")]
pub(super) fn ensure_leaves_sandbox(found: &Found, claimed_sandbox: &str) -> Result<()> {
    match seatbelt_state(found.pid) {
        Some(true) => {}
        None if claimed_sandbox == "seatbelt" => {}
        _ => return Ok(()),
    }
    let chain: Vec<u32> = crate::crew_scope::ancestry::ancestors_of(found.pid)?
        .iter()
        .map(|a| a.pid)
        .collect();
    ensure!(
        leaves(&chain, |pid| seatbelt_state(pid)
            .map(|sandboxed| !sandboxed)),
        "{}",
        never_leaves(found.pid, "a macOS sandbox")
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn ensure_leaves_sandbox(_found: &Found, _claimed_sandbox: &str) -> Result<()> {
    Ok(())
}

/// The kernel's working directory of process `here`.
#[cfg(target_os = "linux")]
pub(super) fn kernel_cwd(here: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let link = format!("/proc/{here}/cwd");
    let theirs = std::fs::metadata(&link).ok()?;
    same_directory(std::fs::read_link(&link).ok()?, theirs.dev(), theirs.ino())
}

/// The kernel's working directory of process `pid`, by `proc_pidinfo`
/// `PROC_PIDVNODEPATHINFO`, which a launcher outside the sandbox may read
/// for a process of its own user.
#[cfg(target_os = "macos")]
pub(super) fn kernel_cwd(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: an all-zero proc_vnodepathinfo is valid plain data, and
    // proc_pidinfo writes at most its size into it.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .take_while(|b| *b != 0)
        .collect();
    let stat = &info.pvi_cdir.vip_vi.vi_stat;
    // `MetadataExt::dev` widens macOS's signed `dev_t`; so must this.
    let dev = i32::from_ne_bytes(stat.vst_dev.to_ne_bytes()) as u64;
    same_directory(
        PathBuf::from(std::ffi::OsStr::from_bytes(&path)),
        dev,
        stat.vst_ino,
    )
}

/// Find `recorded` from here and prove it is the waiter of request `id` on
/// `home` bound to `digest`: the same start identity, this user, and the
/// bound argv.
#[cfg(target_os = "linux")]
pub(super) fn requester(recorded: &Value, home: &Path, id: &str, digest: &str) -> Result<Found> {
    use std::fs;
    use std::io::Read;
    let pid = recorded_pid(recorded)?;
    let started = recorded_start(recorded)?;
    let namespace = recorded["pid_namespace"].as_str();
    let here = match namespace {
        None => pid,
        Some(ns) if Some(ns) == crate::jobs::pid_namespace() => pid,
        Some(ns) => outer_pid(pid, ns)?,
    };
    ensure!(
        live(here, started)?,
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
        bound(&argv, home, id, digest),
        "process {pid} is not waiting on this request"
    );
    let mut raw = vec![];
    fs::File::open(format!("/proc/{here}/environ"))
        .and_then(|file| file.take(ENV_BYTES as u64 + 1).read_to_end(&mut raw))
        .with_context(|| format!("read the environment of process {here}"))?;
    ensure!(
        raw.len() <= ENV_BYTES,
        "its environment is larger than a launcher adopts (at most {ENV_BYTES} bytes)"
    );
    let env = environ(&raw);
    ensure_bounded(&env)?;
    let cwd = kernel_cwd(here);
    // Still the same process after the reads.
    ensure!(
        live(here, started)?,
        "process {pid} exited while it was being identified"
    );
    Ok(Found {
        pid: here,
        env,
        cwd,
    })
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
pub(super) fn requester(recorded: &Value, home: &Path, id: &str, digest: &str) -> Result<Found> {
    use std::os::unix::ffi::OsStringExt;
    let pid = recorded_pid(recorded)?;
    ensure!(
        recorded["pid_namespace"].is_null(),
        "the request names a PID namespace, which macOS does not have"
    );
    let started = recorded_start(recorded)?;
    ensure!(
        live(pid, started)?,
        "process {pid} has exited or its pid was reused"
    );
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
        bound(&argv, home, id, digest),
        "process {pid} is not waiting on this request"
    );
    let env: Vec<(OsString, OsString)> = env
        .into_iter()
        .map(|(k, v)| (OsString::from_vec(k), OsString::from_vec(v)))
        .collect();
    ensure_bounded(&env)?;
    let cwd = kernel_cwd(pid);
    ensure!(
        live(pid, started)?,
        "process {pid} exited while it was being identified"
    );
    Ok(Found { pid, env, cwd })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn requester(
    _recorded: &Value,
    _home: &Path,
    _id: &str,
    _digest: &str,
) -> Result<Found> {
    anyhow::bail!("launches for sandboxed callers are supported on Linux and macOS only")
}
