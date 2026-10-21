//! Captain-approved privileged actions ([design](../docs/design/privileged-action.md)).
//!
//! A first mate stages one exact command with `privileged.stage`; the runtime
//! binds it by hash and writes the only kind of captain request no one else
//! may write. The captain grants it through the ordinary correlated
//! `approval.grant`, from the bridge or a front door, then confirms it at the
//! machine with `privateer privileged run ID`: the hash prefix typed on the
//! terminal, then sudo's own password when sudo asks. The command runs once,
//! as `sudo -k -- PROGRAM ARGS...`, with no shell, and the ship is told the
//! result in the request's thread.
use crate::store::{Caller, Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const REQUEST_KIND: &str = "privileged_action";
/// How long a privileged request stays good, from when it was staged: unapproved,
/// or approved and not yet run. There is no shorter limit after approval.
pub const PRIVILEGED_REQUEST_TTL_MS: i64 = 24 * 60 * 60 * 1000;
pub const ARTIFACT_PREFIX: &str = "privileged:";
pub const ENVIRONMENT_PREFIX: &str = "host:";
const VERSION: u64 = 1;
const MAX_ARGS: usize = 64;
const MAX_ARG_BYTES: usize = 4096;
const MAX_ARGS_BYTES: usize = 16 * 1024;
const MAX_REASON_BYTES: usize = 1024;
const MAX_PATH_BYTES: usize = 4096;
/// The hex characters the captain types to confirm a run.
pub const CONFIRM_CHARS: usize = 8;
const OUTPUT_KEPT: usize = 64 * 1024;
/// What of the kept output rides the result notice, inside mail's 64 KiB.
const OUTPUT_MAILED: usize = 16 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_TIMEOUT_SECS: u64 = 3600;
const KILL_GRACE: Duration = Duration::from_secs(5);
const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const MAX_ID_BYTES: usize = 48;
/// Needs pages the door reads to find the item, as the bridge does.
const NEEDS_PAGES: usize = 4;
const USAGE: &str = "usage: privateer privileged run ID [--timeout-secs N] | list";

/// Programs that would run something other than the staged argv, by the
/// basename of the canonical path: (what they are, exact names, name prefixes).
const REFUSED: &[(&str, &[&str], &[&str])] = &[
    (
        "a shell",
        &[
            "sh", "bash", "zsh", "dash", "ksh", "fish", "csh", "tcsh", "busybox",
        ],
        &[],
    ),
    (
        "a launcher",
        &[
            "env",
            "nice",
            "nohup",
            "timeout",
            "stdbuf",
            "time",
            "xargs",
            "find",
            "watch",
            "script",
            "chroot",
            "nsenter",
            "unshare",
            "setpriv",
            "runuser",
            "systemd-run",
            "ssh",
            "tmux",
            "screen",
        ],
        &[],
    ),
    (
        "a privilege tool",
        &["sudo", "sudoedit", "su", "doas", "pkexec", "visudo"],
        &[],
    ),
    (
        "an interpreter",
        &[
            "node",
            "deno",
            "bun",
            "tclsh",
            "osascript",
            "awk",
            "gawk",
            "mawk",
            "sed",
        ],
        &["python", "perl", "ruby", "php", "lua"],
    ),
    (
        "an editor or pager",
        &["nvim", "emacs", "nano", "less", "more", "man"],
        &["vi"],
    ),
];

fn refused_class(name: &str) -> Option<&'static str> {
    REFUSED.iter().find_map(|(class, exact, prefixes)| {
        (exact.contains(&name) || prefixes.iter().any(|p| name.starts_with(p))).then_some(*class)
    })
}

/// Unicode's format characters (category Cf: bidi overrides and isolates,
/// zero-width characters, the byte order mark, tags) and the line and
/// paragraph separators: invisible, or able to move text on a screen.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
        | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
        | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// One line of visible text: no control character (category Cc: NUL, CR,
/// LF, escapes) and no format or separator character.
fn no_control(text: &str, what: &str) -> Result<()> {
    ensure!(
        !text.chars().any(|c| c.is_control() || invisible(c)),
        "{what} contains a control character, NUL or invisible formatting character"
    );
    Ok(())
}

fn check_reason(reason: &str) -> Result<()> {
    ensure!(
        !reason.trim().is_empty() && reason.len() <= MAX_REASON_BYTES,
        "reason must be 1 to {MAX_REASON_BYTES} bytes"
    );
    no_control(reason, "reason")
}

/// A staged command, checked: the canonical program and the existing folder.
struct Checked {
    program: PathBuf,
    program_sha256: String,
    args: Vec<String>,
    cwd: PathBuf,
}

/// The owner a program file and its folders must have: root. A test build
/// may also trust one named uid, since tests cannot create root's files.
fn trusted_owner(uid: u32) -> bool {
    #[cfg(feature = "test-sudo")]
    if std::env::var("PRIVATEER_TEST_TRUST_UID").ok() == Some(uid.to_string()) {
        return true;
    }
    uid == 0
}

fn owned_by_root(path: &Path) -> Result<fs::Metadata> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(
        trusted_owner(meta.uid()),
        "{} is not owned by root",
        path.display()
    );
    ensure!(
        meta.mode() & 0o022 == 0,
        "{} is writable by group or others",
        path.display()
    );
    Ok(meta)
}

/// The program file and every folder above it are root's and writable by no
/// one else, so no ordinary process can change what the hash names.
fn check_program_file(program: &Path) -> Result<String> {
    let meta = owned_by_root(program)?;
    ensure!(
        meta.is_file(),
        "{} is not a regular file",
        program.display()
    );
    ensure!(
        meta.permissions().mode() & 0o111 != 0,
        "{} is not executable",
        program.display()
    );
    let mut parent = program.parent();
    while let Some(folder) = parent {
        owned_by_root(folder)?;
        parent = folder.parent();
    }
    let bytes = fs::read(program).with_context(|| format!("cannot read {}", program.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// The working folder: a real folder, named without a symlink or `..`, that
/// is root's all the way up and writable by no one else. The root command
/// may run files from it (make, cargo, npm and pip do), so nothing but root
/// may put them there. `/` is the usual folder.
fn check_folder(cwd: &str) -> Result<PathBuf> {
    no_control(cwd, "cwd")?;
    ensure!(
        cwd.len() <= MAX_PATH_BYTES && Path::new(cwd).is_absolute(),
        "cwd must be an absolute path"
    );
    let mut walked = PathBuf::new();
    for part in Path::new(cwd).components() {
        match part {
            Component::RootDir => walked.push("/"),
            Component::CurDir => {}
            Component::Normal(name) => {
                walked.push(name);
                let meta = fs::symlink_metadata(&walked)
                    .map_err(|_| anyhow::anyhow!("cwd must be an existing folder"))?;
                ensure!(
                    !meta.file_type().is_symlink(),
                    "cwd must name no symlink, and {} is one",
                    walked.display()
                );
            }
            _ => bail!("cwd must not contain '..'"),
        }
    }
    let canonical =
        fs::canonicalize(cwd).map_err(|_| anyhow::anyhow!("cwd must be an existing folder"))?;
    ensure!(canonical.is_dir(), "cwd must be an existing folder");
    ensure!(
        !canonical
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("sudoers"),
        "a working folder naming sudoers is refused"
    );
    let mut folder = Some(canonical.as_path());
    while let Some(path) = folder {
        owned_by_root(path).map_err(|e| {
            anyhow::anyhow!("cwd must be root's all the way up (\"/\" is the usual folder): {e}")
        })?;
        folder = path.parent();
    }
    Ok(canonical)
}

/// The checked working folder, opened again just before sudo starts: one
/// folder at a time from `/`, never following a symlink, each checked by
/// its open descriptor. The child enters the last one with `fchdir`, so no
/// path is looked up between this check and exec.
fn open_folder(path: &Path) -> Result<File> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let checked = |file: File, shown: &Path| -> Result<File> {
        let meta = file.metadata()?;
        ensure!(
            meta.is_dir(),
            "the working folder {} changed",
            shown.display()
        );
        ensure!(
            trusted_owner(meta.uid()) && meta.mode() & 0o022 == 0,
            "the working folder {} is no longer root's alone",
            shown.display()
        );
        Ok(file)
    };
    let root = c"/";
    let fd = unsafe { libc::open(root.as_ptr(), flags) };
    ensure!(fd >= 0, "cannot open /: {}", std::io::Error::last_os_error());
    let mut folder = checked(unsafe { File::from_raw_fd(fd) }, Path::new("/"))?;
    let mut walked = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                walked.push(name);
                let name = std::ffi::CString::new(name.as_bytes())?;
                let fd = unsafe { libc::openat(folder.as_raw_fd(), name.as_ptr(), flags) };
                ensure!(
                    fd >= 0,
                    "cannot open the working folder {} without following a symlink: {}",
                    walked.display(),
                    std::io::Error::last_os_error()
                );
                folder = checked(unsafe { File::from_raw_fd(fd) }, &walked)?;
            }
            _ => bail!("the working folder {} is not canonical", path.display()),
        }
    }
    Ok(folder)
}

/// Every shape and program refusal, at staging and again at run.
fn check(program: &str, args: &[String], cwd: &str) -> Result<Checked> {
    no_control(program, "program")?;
    ensure!(
        program.len() <= MAX_PATH_BYTES && Path::new(program).is_absolute(),
        "program must be an absolute path"
    );
    ensure!(
        args.len() <= MAX_ARGS,
        "at most {MAX_ARGS} arguments are staged"
    );
    let mut total = 0;
    for arg in args {
        no_control(arg, "an argument")?;
        ensure!(
            arg.len() <= MAX_ARG_BYTES,
            "each argument is at most {MAX_ARG_BYTES} bytes"
        );
        total += arg.len();
        ensure!(
            !arg.to_ascii_lowercase().contains("sudoers"),
            "an argument naming sudoers is refused"
        );
    }
    ensure!(
        total <= MAX_ARGS_BYTES,
        "the arguments are at most {MAX_ARGS_BYTES} bytes in total"
    );
    let cwd = check_folder(cwd)?;
    let given = Path::new(program);
    let canonical = fs::canonicalize(given).with_context(|| format!("no program at {program}"))?;
    let canonical_str = canonical.to_str().context("program path must be UTF-8")?;
    no_control(canonical_str, "program")?;
    for path in [given, canonical.as_path()] {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("program has no name")?;
        if let Some(class) = refused_class(name) {
            bail!("{name} is {class}; stage the exact program to run instead");
        }
    }
    let program_sha256 = check_program_file(&canonical)?;
    Ok(Checked {
        program: canonical,
        program_sha256,
        args: args.to_vec(),
        cwd,
    })
}

/// The canonical binding: keys sorted (serde_json's map is ordered), compact.
#[allow(clippy::too_many_arguments)]
fn binding(
    request_id: &str,
    ship: &str,
    task_id: &str,
    machine: &str,
    program: &str,
    program_sha256: &str,
    args: &[String],
    cwd: &str,
) -> Value {
    let mut argv = vec![program.to_owned()];
    argv.extend(args.iter().cloned());
    json!({"v": VERSION, "request_id": request_id, "ship": ship, "task_id": task_id,
        "machine": machine, "program": program, "program_sha256": program_sha256,
        "argv": argv, "cwd": cwd})
}

fn strings(value: &Value, what: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .with_context(|| format!("{what} must be an array of strings"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .with_context(|| format!("{what} must be an array of strings"))
        })
        .collect()
}

/// Check a stored request's binding against its own fields, without the
/// disk. Returns its hash. Every path that grants, lists or runs calls this.
pub(crate) fn verify(request: &Value) -> Result<String> {
    ensure!(
        request["request_kind"] == REQUEST_KIND
            && request["kind"] == "request"
            && request["to"] == crate::mail::CAPTAIN,
        "not a privileged request to the captain"
    );
    let payload = &request["payload"];
    let staged = &payload["privileged"];
    ensure!(staged["v"] == VERSION, "unknown privileged request version");
    let field = |key: &str| -> Result<&str> {
        staged[key]
            .as_str()
            .with_context(|| format!("privileged request has no {key}"))
    };
    check_reason(field("reason")?)?;
    let args = strings(&staged["args"], "args")?;
    let expected = binding(
        request["id"].as_str().context("request id")?,
        field("ship")?,
        field("task_id")?,
        field("machine")?,
        field("program")?,
        field("program_sha256")?,
        &args,
        field("cwd")?,
    );
    let hash = crate::core::hash(&expected);
    ensure!(
        field("hash")? == hash
            && staged["argv"] == expected["argv"]
            && request["from"] == staged["ship"]
            && request["task_id"] == staged["task_id"]
            && payload["artifact"] == json!(artifact(&hash))
            && payload["environment"] == json!(environment(field("machine")?)),
        "privileged request binding changed"
    );
    Ok(hash)
}

pub(crate) fn artifact(hash: &str) -> String {
    format!("{ARTIFACT_PREFIX}sha256:{hash}")
}

pub(crate) fn environment(machine: &str) -> String {
    format!("{ENVIRONMENT_PREFIX}{machine}")
}

/// Whether an artifact or environment names a privileged approval, which
/// only a correlated grant writes and only `privileged.run` uses up.
pub(crate) fn reserved(artifact: &str, environment: &str) -> bool {
    artifact.starts_with(ARTIFACT_PREFIX) || environment.starts_with(ENVIRONMENT_PREFIX)
}

pub(crate) fn expires_ms(request: &Value) -> i64 {
    request["created_ms"]
        .as_i64()
        .unwrap_or(0)
        .saturating_add(PRIVILEGED_REQUEST_TTL_MS)
}

/// Written in this home by a ship hosted here: not a copy carried from
/// another machine, whose program, sudo and terminal are elsewhere.
pub(crate) fn local(store: &Store, request: &Value) -> Result<bool> {
    if !request["delivery"].is_null() {
        return Ok(false);
    }
    let Some(ship) = request["from"].as_str() else {
        return Ok(false);
    };
    Ok(crate::mail::ship(store, ship)?.is_some_and(|s| s["location"] == "here"))
}

/// The request a correlated grant or denial names, checked inside core's
/// authority transaction: its binding, that it lives here, and its expiry.
pub(crate) fn check_answerable(store: &Store, request: &Value) -> Result<()> {
    verify(request)?;
    ensure!(
        local(store, request)?,
        "this privileged request was staged on another machine; approve it there"
    );
    ensure!(
        request["payload"]["privileged"]["machine"] == crate::machines::this_machine(),
        "this privileged request names another machine"
    );
    ensure!(
        Store::now_ms() < expires_ms(request),
        "this privileged request expired"
    );
    Ok(())
}

/// One argument as the captain reads it: bare when plainly safe, otherwise
/// quoted, with anything but printable ASCII escaped.
pub fn display(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if plain {
        return arg.to_owned();
    }
    let mut out = String::from("'");
    for c in arg.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            // A backtick could end a code block on a surface that renders
            // markdown, and show the rest of the command as something else.
            ' '..='_' | 'a'..='~' => out.push(c),
            _ => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
        }
    }
    out.push('\'');
    out
}

/// The needs item's `privileged` block.
pub(crate) fn summary(request: &Value) -> Value {
    let staged = &request["payload"]["privileged"];
    let lines: Vec<String> = staged["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| display(a.as_str().unwrap_or("")))
        .collect();
    let unrooted: Vec<String> = staged["unrooted"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| a.as_str().unwrap_or("").to_owned())
        .collect();
    json!({"argv": lines, "cwd": display(staged["cwd"].as_str().unwrap_or("")),
        "reason": display(staged["reason"].as_str().unwrap_or("")),
        "warnings": warnings(&unrooted), "hash": staged["hash"],
        "machine": staged["machine"], "expires_ms": expires_ms(request)})
}

/// Whether a folder is root's all the way up: its deepest existing part,
/// canonical, and every folder above that.
fn rooted(folder: &Path) -> bool {
    let Some(existing) = folder.ancestors().find(|f| f.exists()) else {
        return false;
    };
    let Ok(canonical) = fs::canonicalize(existing) else {
        return false;
    };
    canonical.ancestors().all(|f| owned_by_root(f).is_ok())
}

/// The arguments that name a path the root command reads or writes and that
/// someone other than root could change after the captain approves: a
/// relative path, or an absolute one under a folder that is not root's all
/// the way up. Warned about, not refused; the folder is now. An option's
/// value (`--file=PATH`) counts too.
fn unrooted(args: &[String], cwd: &Path) -> Vec<String> {
    args.iter()
        .filter(|arg| {
            let mut paths = vec![arg.as_str()];
            if let Some((_, value)) = arg.split_once('=').filter(|_| arg.starts_with('-')) {
                paths.push(value);
            }
            paths.into_iter().any(|path| {
                if let Some(rest) = path.strip_prefix('/') {
                    !rest.is_empty() && !Path::new(path).parent().is_some_and(rooted)
                } else {
                    !path.is_empty()
                        && !path.starts_with('-')
                        && !path.contains("://")
                        && (path.contains('/') || fs::symlink_metadata(cwd.join(path)).is_ok())
                }
            })
        })
        .cloned()
        .collect()
}

fn warnings(unrooted: &[String]) -> Vec<String> {
    unrooted
        .iter()
        .map(|arg| {
            format!(
                "warning: {} is not under a root-owned folder; it can change after you approve",
                display(arg)
            )
        })
        .collect()
}

fn stage(store: &Store, args: &Value) -> Result<Value> {
    let fields = args.as_object().context("arguments must be an object")?;
    for key in ["env", "environment"] {
        ensure!(
            !fields.contains_key(key),
            "staged environment variables are not supported: sudo resets them"
        );
    }
    if let Some(key) = fields.keys().find(|k| {
        !matches!(
            k.as_str(),
            "id" | "from" | "task_id" | "program" | "args" | "cwd" | "reason"
        )
    }) {
        bail!("privileged.stage takes from, task_id, program, args, cwd and reason, not {key}");
    }
    let from = required(args, "from")?;
    identifier(from)?;
    let task_id = required(args, "task_id")?;
    let task = store.task(task_id)?;
    ensure!(
        task["owner"] == from,
        "{from} can stage a privileged command only for its own task"
    );
    let reason = required(args, "reason")?;
    check_reason(reason)?;
    let staged_args = match args.get("args").filter(|v| !v.is_null()) {
        Some(value) => strings(value, "args")?,
        None => vec![],
    };
    let checked = check(
        required(args, "program")?,
        &staged_args,
        required(args, "cwd")?,
    )?;
    let id = match args.get("id").filter(|v| !v.is_null()) {
        Some(id) => {
            let id = id.as_str().context("id must be a string")?;
            identifier(id)?;
            id.to_owned()
        }
        None => crate::mail::outcome_id(store, "privileged")?,
    };
    // Herdr's launch words are the only way the bridge opens a run, and a
    // phone's button carries the id in at most 64 bytes.
    ensure!(
        id.len() <= MAX_ID_BYTES
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "a privileged request id is at most {MAX_ID_BYTES} letters, digits, '-', '_' and '.'"
    );
    let machine = crate::machines::this_machine();
    let program = checked.program.to_str().context("program path")?.to_owned();
    let cwd = checked
        .cwd
        .to_str()
        .context("cwd must be UTF-8")?
        .to_owned();
    let bound = binding(
        &id,
        from,
        task_id,
        &machine,
        &program,
        &checked.program_sha256,
        &checked.args,
        &cwd,
    );
    let hash = crate::core::hash(&bound);
    let unrooted = unrooted(&checked.args, &checked.cwd);
    let payload = json!({
        "privileged": {"v": VERSION, "ship": from, "task_id": task_id, "machine": machine,
            "program": program, "program_sha256": checked.program_sha256,
            "args": checked.args, "argv": bound["argv"], "cwd": cwd, "reason": reason,
            "unrooted": unrooted, "hash": hash},
        "artifact": artifact(&hash),
        "environment": environment(&machine),
    });
    let shown: Vec<String> = bound["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| display(a.as_str().unwrap_or("")))
        .collect();
    let mut warned = warnings(&unrooted).join("\n");
    if !warned.is_empty() {
        warned.push('\n');
    }
    let body = format!(
        "Reason: {}\n{warned}Run as root on {machine}:\n{}\nin {}\nHash {hash}",
        display(reason),
        shown.join("\n"),
        display(&cwd)
    );
    let sent = crate::mail::send_privileged(
        store,
        &json!({"id": id, "from": from, "to": crate::mail::CAPTAIN, "kind": "request",
            "request_kind": REQUEST_KIND, "task_id": task_id, "body": body, "payload": payload}),
    )?;
    store.audit(
        "privileged.stage",
        &json!({"id": id, "ship": from, "task_id": task_id, "hash": hash}),
    )?;
    Ok(
        json!({"id": id, "hash": hash, "machine": machine, "argv": bound["argv"],
        "expires_ms": expires_ms(&sent), "mail": sent,
        "note": format!("The captain approves it, then confirms and runs it at {machine}. \
            The result arrives in this request's thread; do not run it yourself.")}),
    )
}

fn table(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS privileged_runs(request_id TEXT PRIMARY KEY,
           hash TEXT NOT NULL, started_ms INTEGER NOT NULL, finished_ms INTEGER,
           exit_code INTEGER, timed_out INTEGER, output TEXT);",
    )?;
    Ok(())
}

fn run_record(store: &Store, id: &str) -> Result<Option<Value>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT hash,started_ms,finished_ms,exit_code,timed_out,output FROM privileged_runs WHERE request_id=?1",
            [id],
            |r| {
                Ok(json!({"hash": r.get::<_, String>(0)?, "started_ms": r.get::<_, i64>(1)?,
                    "finished_ms": r.get::<_, Option<i64>>(2)?, "exit_code": r.get::<_, Option<i64>>(3)?,
                    "timed_out": r.get::<_, Option<bool>>(4)?, "output": r.get::<_, Option<String>>(5)?}))
            },
        )
        .optional()?)
}

fn list(store: &Store, args: &Value) -> Result<Value> {
    table(store)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .clamp(1, 200);
    let ship = match store.caller() {
        Caller::Agent { actor } => Some(actor.clone()),
        Caller::Operator => args.get("ship").and_then(Value::as_str).map(str::to_owned),
    };
    let mut statement = store.connection().prepare(
        "SELECT id FROM mail WHERE request_kind=?1 AND recipient=?2 AND (?3 IS NULL OR sender=?3)
         ORDER BY seq DESC LIMIT ?4",
    )?;
    let ids = statement
        .query_map(
            params![REQUEST_KIND, crate::mail::CAPTAIN, ship, limit as i64],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut requests = vec![];
    for id in ids {
        let request = crate::mail::find(store, &id)?.context("request vanished")?;
        let mut run = run_record(store, &id)?;
        if let Some(run) = run.as_mut() {
            run.as_object_mut().map(|r| r.remove("output"));
        }
        requests.push(
            json!({"id": id, "ship": request["from"], "task_id": request["task_id"],
            "state": request["state"], "end_reason": request["end_reason"],
            "created_ms": request["created_ms"], "expires_ms": expires_ms(&request),
            "valid": verify(&request).is_ok(), "privileged": summary(&request), "run": run}),
        );
    }
    Ok(json!({"requests": requests}))
}

/// What a run may use, re-read and rechecked: under the write lock before it
/// uses the approval up, and once before the captain is asked to confirm.
struct Plan {
    request: Value,
    approval: String,
    hash: String,
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
}

fn plan(store: &Store, id: &str) -> Result<Plan> {
    table(store)?;
    let request = crate::mail::find(store, id)?
        .with_context(|| format!("no privileged request {id} in this home"))?;
    let hash = verify(&request)?;
    ensure!(
        local(store, &request)?,
        "{id} was staged on another machine; run it there"
    );
    let staged = &request["payload"]["privileged"];
    ensure!(
        staged["machine"] == crate::machines::this_machine(),
        "{id} names machine {}, not this one",
        staged["machine"]
    );
    ensure!(
        run_record(store, id)?.is_none(),
        "{id} already ran; a privileged request runs once"
    );
    ensure!(
        Store::now_ms() < expires_ms(&request),
        "{id} expired: a privileged request is good for 24 hours from when it was staged"
    );
    ensure!(
        request["state"] == "approved",
        "{id} is {}, not approved by the captain",
        request["state"].as_str().unwrap_or("unknown")
    );
    let task = request["task_id"].as_str().context("request task")?;
    let charter = store.active_charter(task)?;
    ensure!(
        request["payload"]["charter_hash"] == crate::core::hash(&charter),
        "{id}'s charter changed after it was staged"
    );
    let answer = request["answered_by"]
        .as_str()
        .context("approved request has no recorded answer")?;
    let answer = crate::mail::find(store, answer)?.context("approval answer missing")?;
    let approval = answer["payload"]["approval_id"]
        .as_str()
        .context("approval answer names no approval")?
        .to_owned();
    let unused: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM approvals WHERE id=?1 AND task_id=?2 AND artifact=?3
           AND environment=?4 AND charter_hash=?5 AND approver='captain' AND consumed=0)",
        params![
            approval,
            task,
            artifact(&hash),
            request["payload"]["environment"].as_str(),
            request["payload"]["charter_hash"].as_str()
        ],
        |r| r.get(0),
    )?;
    ensure!(unused, "{id} has no unused captain approval");
    let args = strings(&staged["args"], "args")?;
    // The program file, owner and folders as they are now, against the hash.
    let checked = check(
        staged["program"].as_str().context("program")?,
        &args,
        staged["cwd"].as_str().context("cwd")?,
    )?;
    ensure!(
        checked.program_sha256 == staged["program_sha256"].as_str().unwrap_or("")
            && checked.program.to_str() == staged["program"].as_str()
            && checked.cwd.to_str() == staged["cwd"].as_str(),
        "the program file or folder changed after the captain approved it"
    );
    Ok(Plan {
        request,
        approval,
        hash,
        program: checked.program,
        args: checked.args,
        cwd: checked.cwd,
    })
}

/// The terminal the captain confirms at. A test build may name a file of
/// typed input instead; it then prompts on stderr.
struct Tty {
    input: File,
    output: Box<dyn Write>,
    foreground: Option<std::os::fd::RawFd>,
}

fn open_tty() -> Result<Tty> {
    #[cfg(feature = "test-sudo")]
    if let Some(path) = std::env::var_os("PRIVATEER_TEST_TTY") {
        let input = File::open(path).context(
            "privileged run needs the captain at a terminal, and /dev/tty cannot be opened",
        )?;
        return Ok(Tty {
            input,
            output: Box::new(std::io::stderr()),
            foreground: None,
        });
    }
    let tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("privileged run needs the captain at a terminal, and /dev/tty cannot be opened")?;
    let fd = tty.as_raw_fd();
    let output = Box::new(tty.try_clone()?);
    Ok(Tty {
        input: tty,
        output,
        foreground: Some(fd),
    })
}

impl Tty {
    fn line(&mut self) -> Result<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 256 {
            match self.input.read(&mut byte)? {
                0 => break,
                _ if byte[0] == b'\n' => break,
                _ => line.push(byte[0]),
            }
        }
        Ok(String::from_utf8_lossy(&line).trim().to_ascii_lowercase())
    }
}

fn sudo() -> Result<PathBuf> {
    #[cfg(feature = "test-sudo")]
    if let Some(path) = std::env::var_os("PRIVATEER_SUDO") {
        let path = PathBuf::from(path);
        ensure!(path.is_absolute(), "PRIVATEER_SUDO must be absolute");
        return Ok(path);
    }
    for candidate in ["/usr/bin/sudo", "/bin/sudo"] {
        let path = Path::new(candidate);
        if path.is_file() {
            let path = fs::canonicalize(path)?;
            owned_by_root(&path)?;
            return Ok(path);
        }
    }
    bail!("sudo was not found in /usr/bin or /bin")
}

/// Hand the terminal's foreground to `group`, so sudo may read its password
/// prompt from it; SIGTTOU is ignored around the call, as shells do.
fn foreground(fd: std::os::fd::RawFd, group: libc::pid_t) {
    unsafe {
        let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(fd, group);
        libc::signal(libc::SIGTTOU, previous);
    }
}

struct Outcome {
    exit_code: i64,
    timed_out: bool,
    output: String,
}

fn keep(kept: &Mutex<VecDeque<u8>>, bytes: &[u8]) {
    let mut kept = kept.lock().unwrap_or_else(|e| e.into_inner());
    kept.extend(bytes);
    let over = kept.len().saturating_sub(OUTPUT_KEPT);
    kept.drain(..over);
}

fn execute(plan: &Plan, timeout: Duration, tty: &Tty) -> Result<Outcome> {
    let mut command = Command::new(sudo()?);
    command
        .arg("-k")
        .arg("--")
        .arg(&plan.program)
        .args(&plan.args)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for key in ["TERM", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if let Some(fd) = tty.foreground {
        // In the child, before exec: its own group takes the terminal, so a
        // password prompt reads it without being stopped.
        unsafe {
            command.pre_exec(move || {
                foreground(fd, libc::getpid());
                Ok(())
            });
        }
    }
    // Checked once more by descriptor, last thing before the spawn.
    let folder = open_folder(&plan.cwd)?;
    let folder_fd = folder.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(folder_fd) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().context("could not start sudo")?;
    drop(folder);
    let group = child.id() as libc::pid_t;
    let kept = Arc::new(Mutex::new(VecDeque::new()));
    let mut copiers = vec![];
    let stdout = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    let stderr = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    for (stream, to_stderr) in [(stdout, false), (stderr, true)] {
        let Some(mut stream) = stream else { continue };
        let kept = kept.clone();
        copiers.push(thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                keep(&kept, &buffer[..n]);
                let _ = if to_stderr {
                    std::io::stderr().write_all(&buffer[..n])
                } else {
                    let mut out = std::io::stdout();
                    out.write_all(&buffer[..n]).and_then(|_| out.flush())
                };
            }
        }));
    }
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            unsafe { libc::kill(-group, libc::SIGTERM) };
            let grace = Instant::now() + KILL_GRACE;
            let mut ended = None;
            while Instant::now() < grace {
                if let Some(status) = child.try_wait()? {
                    ended = Some(status);
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            unsafe { libc::kill(-group, libc::SIGKILL) };
            break match ended {
                Some(status) => status,
                None => child.wait()?,
            };
        }
        thread::sleep(Duration::from_millis(50));
    };
    if let Some(fd) = tty.foreground {
        foreground(fd, unsafe { libc::getpgrp() });
    }
    // A grandchild that kept the pipes open must not hold the record back.
    let copied = Instant::now() + Duration::from_secs(2);
    for copier in copiers {
        while !copier.is_finished() && Instant::now() < copied {
            thread::sleep(Duration::from_millis(20));
        }
        if copier.is_finished() {
            let _ = copier.join();
        }
    }
    let exit_code = status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|s| 128 + i64::from(s)))
        .unwrap_or(-1);
    let bytes: Vec<u8> = kept
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    Ok(Outcome {
        exit_code,
        timed_out,
        output: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Tell the ship, in the request's thread, as the runtime: a notice from the
/// captain's desk carrying `privileged_result`, which no one else may write.
fn tell_ship(store: &Store, request: &Value, body: &str, result: &Value) -> Result<String> {
    let id = crate::mail::outcome_id(store, "privileged-result")?;
    store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms)
         VALUES(?1,'captain',?2,'notice',?3,?4,?5,?6,'normal',?7)",
        params![id, request["from"].as_str(), request["id"].as_str(), request["task_id"].as_str(),
            body, json!({"privileged_result": result}).to_string(), Store::now_ms()],
    )?;
    Ok(id)
}

fn tail(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// `privateer privileged run ID`: confirm at the terminal, use the approval
/// up, run once, record and report.
fn run(store: &Store, id: &str, timeout: Duration) -> Result<Value> {
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "privileged run is the captain's at a terminal; refused while {marker} is set"
        );
    }
    store.caller().require_operator("privileged.run")?;
    let mut tty = open_tty()?;
    identifier(id)?;
    let first = plan(store, id)?;
    let staged = &first.request["payload"]["privileged"];
    let mut shown = String::new();
    for arg in staged["argv"].as_array().into_iter().flatten() {
        shown.push_str(&format!("    {}\n", display(arg.as_str().unwrap_or(""))));
    }
    // The ship's words first, then the warnings; the command, its hash and
    // the prompt are the last thing on the screen.
    let mut warned = String::new();
    for line in warnings(&unrooted(&first.args, &first.cwd)) {
        warned.push_str(&line);
        warned.push('\n');
    }
    write!(
        tty.output,
        "Privileged command {id} from {} (task {}), approved by the captain.\n\
         Reason: {}\n{warned}\
         Runs as root on {} with sudo -k, no shell:\n{shown}in {}\n\
         Hash {}\nType the first {CONFIRM_CHARS} characters of the hash to run it: ",
        first.request["from"].as_str().unwrap_or("?"),
        first.request["task_id"].as_str().unwrap_or("?"),
        display(staged["reason"].as_str().unwrap_or("")),
        staged["machine"].as_str().unwrap_or("?"),
        display(staged["cwd"].as_str().unwrap_or("")),
        first.hash
    )?;
    tty.output.flush()?;
    let typed = tty.line()?;
    if typed != first.hash[..CONFIRM_CHARS] {
        store.audit("privileged.unconfirmed", &json!({"id": id}))?;
        bail!("the typed characters do not match the hash; nothing ran and the approval stands");
    }
    // Recheck and use the approval up in one transaction, before sudo starts:
    // a crash after this leaves a used approval and no second run.
    let tx = store.write_transaction()?;
    let plan = plan(store, id)?;
    ensure!(
        plan.hash == first.hash,
        "the request changed while you confirmed it"
    );
    let used = tx.execute(
        "UPDATE approvals SET consumed=1 WHERE id=?1 AND consumed=0",
        [&plan.approval],
    )?;
    ensure!(used == 1, "{id}'s approval was used meanwhile");
    let started = Store::now_ms();
    tx.execute(
        "INSERT INTO privileged_runs(request_id,hash,started_ms) VALUES(?1,?2,?3)",
        params![id, plan.hash, started],
    )?;
    store.audit(
        "privileged.run",
        &json!({"id": id, "hash": plan.hash, "approval_id": plan.approval}),
    )?;
    tx.commit()?;
    writeln!(tty.output)?;
    let outcome = execute(&plan, timeout, &tty);
    if outcome.as_ref().is_ok_and(|o| o.timed_out) {
        // The captain's own account cannot signal a root process, and under
        // use_pty sudo runs the command in a session of its own.
        let _ = writeln!(
            tty.output,
            "\nsudo was signalled after {} s; the command may still be running.",
            timeout.as_secs()
        );
    }
    let finished = Store::now_ms();
    let (exit_code, timed_out, output, error) = match &outcome {
        Ok(o) => (Some(o.exit_code), o.timed_out, o.output.clone(), None),
        Err(e) => (None, false, String::new(), Some(format!("{e:#}"))),
    };
    let tx = store.write_transaction()?;
    tx.execute(
        "UPDATE privileged_runs SET finished_ms=?2,exit_code=?3,timed_out=?4,output=?5 WHERE request_id=?1",
        params![id, finished, exit_code, timed_out, output],
    )?;
    let what = match (&error, timed_out, exit_code) {
        (Some(error), _, _) => format!("could not start: {error}"),
        (None, true, _) => format!(
            "timed out: sudo was signalled after {} s; the command may still be running",
            timeout.as_secs()
        ),
        (None, false, Some(0)) => "exited 0".to_owned(),
        (None, false, code) => format!("exited {}", code.unwrap_or(-1)),
    };
    let result = json!({"v": VERSION, "request_id": id, "hash": plan.hash, "state": "ran",
        "exit_code": exit_code, "timed_out": timed_out, "error": error,
        "started_ms": started, "finished_ms": finished,
        "output_tail": tail(&output, OUTPUT_MAILED), "output_bytes": output.len()});
    let notice = tell_ship(
        store,
        &plan.request,
        &format!("The captain ran privileged command {id} at the machine: it {what}."),
        &result,
    )?;
    store.audit(
        "privileged.finished",
        &json!({"id": id, "exit_code": exit_code, "timed_out": timed_out, "notice": notice}),
    )?;
    tx.commit()?;
    outcome?;
    Ok(
        json!({"id": id, "hash": plan.hash, "exit_code": exit_code, "timed_out": timed_out,
        "output_bytes": output.len(), "notice": notice}),
    )
}

/// End lapsed privileged requests as `expired` and tell each ship. Runs on
/// each dispatcher pass; an approval lapsed unrun is used up so it is told once.
pub fn sweep(store: &Store) -> Result<Vec<String>> {
    let now = Store::now_ms();
    let lapsed_before = now - PRIVILEGED_REQUEST_TTL_MS;
    let mut statement = store.connection().prepare(&format!(
        "SELECT {} FROM mail WHERE request_kind=?1 AND recipient=?2 AND delivery IS NULL
           AND state IN ('open','approved') AND created_ms<=?3",
        crate::mail::COLUMNS
    ))?;
    let lapsed = statement
        .query_map(
            params![REQUEST_KIND, crate::mail::CAPTAIN, lapsed_before],
            crate::mail::message,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    if lapsed.is_empty() {
        return Ok(vec![]);
    }
    table(store)?;
    let mut ended = vec![];
    for request in lapsed {
        let id = request["id"].as_str().context("request id")?.to_owned();
        let tx = store.write_transaction()?;
        let current = crate::mail::find(store, &id)?.context("request vanished")?;
        let ran = run_record(store, &id)?.is_some();
        let told = if current["state"] == "open" {
            crate::mail::end_request(store, &current, "withdrawn", "expired")?;
            true
        } else if current["state"] == "approved" && !ran {
            // Use the lapsed approval up: told once, and never runnable.
            let used = tx.execute(
                "UPDATE approvals SET consumed=1 WHERE consumed=0 AND id=(SELECT json_extract(payload,'$.approval_id') FROM mail WHERE id=?1)",
                [current["answered_by"].as_str()],
            )?;
            used == 1
        } else {
            false
        };
        if told {
            tell_ship(
                store,
                &current,
                &format!("Privileged command {id} expired unrun, 24 hours after it was staged."),
                &json!({"v": VERSION, "request_id": id, "state": "expired",
                    "hash": current["payload"]["privileged"]["hash"]}),
            )?;
            store.audit("privileged.expired", &json!({"id": id}))?;
            ended.push(id);
        }
        tx.commit()?;
    }
    Ok(ended)
}

/// The door's approval: the captain's button on their phone. It answers
/// only a privileged request held in a home on this machine, through the
/// needs item that home lists for it now, so production approvals stay at
/// the bridge.
pub(crate) fn door_answer(args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    identifier(id)?;
    let verb = match required(args, "action")? {
        "grant" => "approval.grant",
        "deny" => "approval.deny",
        other => bail!("approval action must be grant or deny, not {other:?}"),
    };
    for (_, home) in crate::captain::fleet()? {
        let mut store = Store::open(&home)?;
        let Some(request) = crate::mail::find(&store, id)? else {
            continue;
        };
        ensure!(
            request["to"] == crate::mail::CAPTAIN && request["kind"] == "request",
            "{id} is not a request to the captain"
        );
        ensure!(
            request["request_kind"] == REQUEST_KIND,
            "the door approves only privileged commands; answer {id} at the bridge"
        );
        if !local(&store, &request)? {
            continue;
        }
        let mut after: Option<String> = None;
        for _ in 0..NEEDS_PAGES {
            let page = crate::needs::query(&store, &json!({"limit": 100, "after": after}))?;
            ensure!(
                page["available"] == true,
                "the captain's needs are unavailable in that home; try again"
            );
            let items = page["items"].as_array().context("needs items")?;
            if let Some(item) = items.iter().find(|i| i["source_id"] == id) {
                ensure!(
                    item["kind"] == "approval"
                        && item["privileged"].is_object()
                        && item["actions"]
                            .as_array()
                            .is_some_and(|a| a.contains(&json!(verb))),
                    "{id} cannot be answered now (expired, changed or not approvable here)"
                );
                // The hash the captain's phone showed: an answer is to that
                // command and no other.
                let shown = args
                    .get("hash")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .with_context(|| {
                        format!("an answer to {id} must carry the hash the phone showed; nothing was answered")
                    })?;
                let hash = item["privileged"]["hash"].as_str().unwrap_or("");
                ensure!(
                    shown.len() >= CONFIRM_CHARS && hash.starts_with(shown),
                    "{id} changed since your phone showed it; nothing was answered"
                );
                let mut answer = json!({"request_id": id, "expected_version": item["version"],
                    "task_id": item["task_id"], "artifact": item["artifact"],
                    "environment": item["environment"], "approver": crate::mail::CAPTAIN});
                if verb == "approval.grant" {
                    answer["id"] = json!(format!("privileged-grant-{id}"));
                }
                let result = crate::dispatch(&mut store, verb, answer)?;
                store.audit(
                    "door.approval",
                    &json!({"id": id, "action": verb, "home": home}),
                )?;
                return Ok(json!({"id": id, "action": verb, "ship": request["from"],
                    "machine": item["privileged"]["machine"], "result": result,
                    "run": format!("privateer --home {} privileged run {id}", home.display())}));
            }
            after = page["next_after"].as_str().map(str::to_owned);
            if after.is_none() {
                break;
            }
        }
        bail!("{id} is not open in the captain's needs");
    }
    bail!("no privileged request {id} in a home on this machine")
}

fn timeout_secs(value: Option<u64>) -> Result<Duration> {
    let secs = value.unwrap_or(DEFAULT_TIMEOUT_SECS);
    ensure!(
        (1..=MAX_TIMEOUT_SECS).contains(&secs),
        "--timeout-secs is 1 to {MAX_TIMEOUT_SECS}"
    );
    Ok(Duration::from_secs(secs))
}

/// `privateer privileged run ID [--timeout-secs N] | list`, and
/// `privileged.run --json {id, timeout_secs?}`: only ever from a terminal.
pub fn cli(
    home: Option<PathBuf>,
    positional: &[String],
    json_args: Option<&Value>,
    caller: Caller,
) -> Result<()> {
    let home =
        home.context("not inside a ship folder (no .privateer here or above); pass --home")?;
    let (verb, id, timeout) = match json_args {
        Some(args) => (
            "run".to_owned(),
            required(args, "id")?.to_owned(),
            args.get("timeout_secs").and_then(Value::as_u64),
        ),
        None => {
            let flags = crate::cli::flags(positional, &["timeout-secs"])?;
            ensure!(flags.switches.is_empty(), "{USAGE}");
            let timeout = flags
                .values
                .get("timeout-secs")
                .map(|v| v.parse::<u64>().context("--timeout-secs is a number"))
                .transpose()?;
            match flags.positional.as_slice() {
                [verb, id] if verb == "run" => ("run".to_owned(), id.clone(), timeout),
                [verb] if verb == "list" => ("list".to_owned(), String::new(), None),
                _ => bail!(USAGE),
            }
        }
    };
    let store = Store::open_as(&home, caller)?;
    if verb == "list" {
        println!(
            "{}",
            serde_json::to_string_pretty(&list(&store, &json!({}))?)?
        );
        return Ok(());
    }
    let result = run(&store, &id, timeout_secs(timeout)?)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "privileged.stage" => stage(store, &args),
        "privileged.list" => list(store, &args),
        "privileged.run" => bail!(
            "privileged.run runs only from the captain's terminal: privateer privileged run ID"
        ),
        _ => bail!("unknown command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_anything_odd_and_escapes_backticks_and_non_ascii() {
        assert_eq!(display("/usr/bin/id"), "/usr/bin/id");
        assert_eq!(display(""), "''");
        assert_eq!(display("a b"), "'a b'");
        assert_eq!(display("it's"), "'it\\'s'");
        assert_eq!(display("```"), "'\\u{60}\\u{60}\\u{60}'");
        assert_eq!(display("caf\u{e9}"), "'caf\\u{e9}'");
        assert_eq!(display("\u{202e}rtl"), "'\\u{202e}rtl'");
    }

    #[test]
    fn refused_programs_match_exact_names_and_prefixes() {
        for name in [
            "sh",
            "env",
            "sudo",
            "python3.12",
            "perl5.38",
            "vim",
            "php8",
            "lua5.4",
        ] {
            assert!(refused_class(name).is_some(), "{name}");
        }
        for name in ["id", "systemctl", "apt-get", "shred", "envsubst", "suspend"] {
            assert!(refused_class(name).is_none(), "{name}");
        }
    }

    #[test]
    fn the_binding_is_canonical_and_covers_every_field() {
        let args = vec!["-u".to_owned()];
        let base = binding("r", "apps", "t", "m", "/usr/bin/id", "aa", &args, "/");
        assert_eq!(
            base.to_string(),
            r#"{"argv":["/usr/bin/id","-u"],"cwd":"/","machine":"m","program":"/usr/bin/id","program_sha256":"aa","request_id":"r","ship":"apps","task_id":"t","v":1}"#
        );
        let hash = crate::core::hash(&base);
        for other in [
            binding("r2", "apps", "t", "m", "/usr/bin/id", "aa", &args, "/"),
            binding("r", "infra", "t", "m", "/usr/bin/id", "aa", &args, "/"),
            binding("r", "apps", "t2", "m", "/usr/bin/id", "aa", &args, "/"),
            binding("r", "apps", "t", "m2", "/usr/bin/id", "aa", &args, "/"),
            binding("r", "apps", "t", "m", "/usr/bin/ls", "aa", &args, "/"),
            binding("r", "apps", "t", "m", "/usr/bin/id", "bb", &args, "/"),
            binding("r", "apps", "t", "m", "/usr/bin/id", "aa", &[], "/"),
            binding("r", "apps", "t", "m", "/usr/bin/id", "aa", &args, "/tmp"),
        ] {
            assert_ne!(crate::core::hash(&other), hash, "{other}");
        }
    }
}
