//! Optional, trusted executable adapters. This is a protocol boundary, not a sandbox.
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    id: String,
    /// `harness` (the default) or another plugin type, such as `front-door`.
    #[serde(default)]
    kind: Option<String>,
    executable: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_output")]
    max_output_bytes: usize,
}
fn default_timeout() -> u64 {
    2_000
}
fn default_output() -> usize {
    65_536
}

/// Launch planning waits at least this long for a plugin, whatever the
/// manifest declares: under load, interpreter start-up alone can exceed a
/// 2 s manifest timeout, and a busy machine should delay a launch, not fail it.
const LAUNCH_FLOOR_MS: u64 = 15_000;
/// Load can stretch the wait only this far, so a hung plugin still fails a
/// launch within a minute and the launch's own backoff takes over.
pub(crate) const LAUNCH_CEILING_MS: u64 = 60_000;

/// How long a plugin request may take.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Wait {
    /// The timeout the manifest (or built-in adapter) declares.
    Declared,
    /// Planning a launch: the validate before a hosted first mate starts, the
    /// command plan `agent.start` runs, and the tool bundle check either needs.
    Launch,
}

impl Wait {
    fn millis(self, declared: u64) -> u64 {
        match self {
            Wait::Declared => declared,
            Wait::Launch => launch_wait(declared, crate::admission::load_per_cpu().unwrap_or(1.0)),
        }
    }
}

/// The floor (or a longer declared timeout), scaled by the one-minute load
/// per CPU admission gates on once that passes 1, and capped by the ceiling.
fn launch_wait(declared: u64, load_per_cpu: f64) -> u64 {
    let base = declared.max(LAUNCH_FLOOR_MS) as f64;
    (base * load_per_cpu.max(1.0)).min(LAUNCH_CEILING_MS as f64) as u64
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    request(store, command, args, Wait::Declared)
}

/// The same request as `dispatch`, made to plan a launch.
pub(crate) fn plan_launch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    request(store, command, args, Wait::Launch)
}

fn request(store: &mut Store, command: &str, args: Value, wait: Wait) -> Result<Value> {
    match command {
        "agent.brief" => {
            let (context, _) = task_context(store, &args)?;
            Ok(json!({"task_id": context["task_id"], "brief": brief(&context), "context": context}))
        }
        "plugin.validate" | "plugin.command" => {
            let (context, charter) = task_context(store, &args)?;
            let (manifest_path, config_path) = selected_paths(&charter, &args)?;
            let raw = fs::read(&manifest_path).context("cannot read plugin manifest")?;
            ensure!(raw.len() <= 65_536, "plugin manifest too large");
            let manifest: Manifest =
                serde_json::from_slice(&raw).context("invalid plugin manifest")?;
            ensure!(
                manifest.schema_version == 1,
                "unsupported plugin manifest version"
            );
            ensure!(
                manifest.kind.as_deref().is_none_or(|k| k == "harness"),
                "{} is a {} plugin, not a harness adapter",
                manifest.id,
                manifest.kind.as_deref().unwrap_or("")
            );
            valid_identity(&manifest.id, &manifest.executable)?;
            ensure!(
                (1..=30_000).contains(&manifest.timeout_ms),
                "plugin timeout outside allowed range"
            );
            ensure!(
                (128..=1_048_576).contains(&manifest.max_output_bytes),
                "plugin output limit outside allowed range"
            );
            let config = config_path.map(|p| private_config(&p)).transpose()?;
            let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
            ensure!(options.is_object(), "plugin options must be an object");
            let id = uuid::Uuid::new_v4().to_string();
            let method = if command == "plugin.command" {
                "command"
            } else {
                "validate"
            };
            let mut request = json!({"id": id, "version": 1, "method": method, "context": context, "options": options});
            // Wakes and errands run as the ship's first mate; anything else is crew.
            let role = if crate::workspace::is_wake(store, &args)? {
                "first_mate"
            } else {
                "crew"
            };
            request["context"]["role"] = json!(role);
            if let Some(selected) = args.get("tool_bundles") {
                let mut harness = None;
                for name in ["pi", "claude"] {
                    let bundled = crate::bundled::resolve(&format!("bundled:{name}"))?;
                    if manifest_path == fs::canonicalize(bundled)? {
                        harness = Some(name);
                    }
                }
                let harness = harness.context(
                    "bounded tool bundles support only the bundled Pi and Claude adapters",
                )?;
                let plan = crate::tool_bundles::resolve(store, selected, role, harness, wait)?;
                if harness == "claude" && method == "command" {
                    let id = args.get("id").and_then(Value::as_str).unwrap_or_default();
                    crate::tool_bundles::record_plan(
                        store,
                        id,
                        role,
                        &plan,
                        options.get("resume"),
                    )?;
                }
                request["context"]["tool_bundles"] = plan;
            }
            request["context"]["tool_permissions"] = crate::permissions::for_store(store)?;
            if let Some(path) = config {
                request["config_path"] = json!(path);
            }
            let request = serde_json::to_vec(&request)?;
            ensure!(request.len() <= 65_536, "plugin request too large");
            let response = invoke(
                &manifest,
                manifest_path.parent().context("manifest needs parent")?,
                request,
                wait,
            )?;
            ensure!(
                response["version"] == 1 && response["id"] == id,
                "plugin response identity or version mismatch"
            );
            if response["error_code"] == "unsupported-tool-policy" {
                bail!(
                    "fleet YOLO is unavailable for this harness transport; select a supported transport or standard mode"
                );
            }
            ensure!(
                response["ok"] == true,
                "plugin refused request (diagnostics withheld to avoid exposing credentials)"
            );
            let mut result = response
                .get("result")
                .cloned()
                .context("plugin omitted result")?;
            if method == "command" {
                result = command_result(&context, &result)?;
            } else {
                ensure!(result["valid"] == true, "plugin validation did not pass");
                // Export only protocol-defined facts. Adapter output is not an evidence/credential channel.
                result = json!({"valid": true, "plugin_id": manifest.id});
            }
            store.audit(
                "plugin.request",
                &json!({"task_id": context["task_id"], "plugin_id": manifest.id, "method": method}),
            )?;
            Ok(result)
        }
        _ => bail!("unknown plugin command: {command}"),
    }
}

/// Qualify registry/resources using the same bounded subprocess and redacted
/// error boundary as native adapters. No native harness is executed here.
pub(crate) fn bundle_request(
    directory: &Path,
    registry: &Value,
    selected: Option<(&Value, &str, &str)>,
    wait: Wait,
) -> Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut request = json!({"id":id,"version":1,"method":"validate","registry":registry});
    if let Some((selection, role, harness)) = selected {
        request["selection"] = selection.clone();
        request["role"] = json!(role);
        request["harness"] = json!(harness);
    }
    let manifest = Manifest {
        schema_version: 1,
        id: "bundles".into(),
        kind: None,
        executable: vec![
            "python3".into(),
            directory.join("bundles.py").to_string_lossy().into(),
        ],
        timeout_ms: 5_000,
        max_output_bytes: 32_768,
    };
    let response = invoke(&manifest, directory, serde_json::to_vec(&request)?, wait)?;
    ensure!(
        response["id"] == id && response["version"] == 1,
        "bounded resource validation failed; inspect approved registry locally"
    );
    // A refusal carries the adapter's public message: the failing bundle and
    // field or rule, never a value from the registry.
    if response["ok"] != true {
        let reason = response["error"]
            .as_str()
            .filter(|reason| reason.len() <= 200 && !reason.contains('\n'))
            .unwrap_or("bounded resource validation failed; inspect approved registry locally");
        bail!("{reason}");
    }
    ensure!(
        response["result"]["valid"] == true,
        "bounded resource validation failed; inspect approved registry locally"
    );
    Ok(response["result"].clone())
}

/// A plugin's id and executable argv, as every manifest type declares them.
pub(crate) fn valid_identity(id: &str, executable: &[String]) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 128, "invalid plugin id");
    ensure!(
        !executable.is_empty() && executable.len() <= 64,
        "invalid plugin executable"
    );
    ensure!(
        executable
            .iter()
            .all(|s| !s.is_empty() && !s.contains('\0') && s.len() <= 4096),
        "invalid plugin executable argument"
    );
    Ok(())
}

/// Private plugin configuration may name credentials: it must be a regular,
/// user-owned file no one else can read, and not a symlink.
pub(crate) fn private_config(path: &Path) -> Result<PathBuf> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "plugin configuration cannot be a symlink"
    );
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "plugin configuration must be user-owned and private (mode 0600)"
    );
    Ok(path.to_path_buf())
}

fn field<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string field {key}"))
}
fn absolute_file(s: &str) -> Result<PathBuf> {
    let path = Path::new(s);
    ensure!(path.is_absolute(), "plugin paths must be absolute");
    let path = fs::canonicalize(path).context("plugin file missing")?;
    ensure!(path.is_file(), "plugin path must be a regular file");
    Ok(path)
}
fn config_file(s: &str) -> Result<PathBuf> {
    ensure!(
        !fs::symlink_metadata(s)?.file_type().is_symlink(),
        "plugin configuration cannot be a symlink"
    );
    absolute_file(s)
}
/// The harness a launch runs, and the memory its manifest declares for crew
/// (`memory_bytes`, optional). Read the manifest the way plugin dispatch does,
/// so a pinned provider and a listed route are both covered.
pub(crate) fn launch_manifest(charter: &Value, args: &Value) -> Result<(String, Option<u64>)> {
    let (path, _) = selected_paths(charter, args)?;
    let raw = fs::read(&path).context("cannot read plugin manifest")?;
    ensure!(raw.len() <= 65_536, "plugin manifest too large");
    let manifest: Value = serde_json::from_slice(&raw).context("invalid plugin manifest")?;
    let id = manifest["id"]
        .as_str()
        .context("plugin manifest has no id")?
        .to_owned();
    let memory = match manifest.get("memory_bytes") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|bytes| *bytes > 0)
                .context("memory_bytes must be a positive number of bytes")?,
        ),
    };
    Ok((id, memory))
}
fn selected_paths(charter: &Value, args: &Value) -> Result<(PathBuf, Option<PathBuf>)> {
    let pin = charter.get("provider_plugin").filter(|v| !v.is_null());
    if let Some(pin) = pin {
        ensure!(
            pin.is_object(),
            "charter provider_plugin must be a manifest/config object"
        );
        let manifest = absolute_file(field(pin, "manifest")?)?;
        let config = pin
            .get("config")
            .filter(|v| !v.is_null())
            .map(|v| config_file(v.as_str().context("pinned config must be a path")?))
            .transpose()?;
        if let Some(supplied) = args.get("manifest") {
            ensure!(
                absolute_file(supplied.as_str().context("manifest must be a path")?)? == manifest,
                "plugin differs from charter's selected provider adapter"
            );
        }
        if let Some(supplied) = args.get("config") {
            let supplied = config_file(supplied.as_str().context("config must be a path")?)?;
            ensure!(
                config.as_ref() == Some(&supplied),
                "plugin configuration differs from charter's selected provider adapter"
            );
        }
        Ok((manifest, config))
    } else {
        let manifest = crate::bundled::resolve(field(args, "manifest")?)?;
        let manifest = absolute_file(manifest.to_str().context("manifest path must be UTF-8")?)?;
        let config = args
            .get("config")
            .filter(|v| !v.is_null())
            .map(|v| config_file(v.as_str().context("config must be a path")?))
            .transpose()?;
        Ok((manifest, config))
    }
}
fn task_context(store: &Store, args: &Value) -> Result<(Value, Value)> {
    let id = field(args, "task_id")?;
    let charter = store.authorize(id, "spawn")?;
    let task = store.task(id)?;
    let title: String = task["title"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(512)
        .collect();
    let executable = crate::executable::installed()?;
    // A sandboxed harness must be able to write the home and the shared
    // resource ledger to keep records and start crew jobs.
    let writable = json!([store.home(), crate::jobs::ledger_dir(store)?]);
    let source_repo = fs::canonicalize(charter["repo"].as_str().context("charter repo missing")?)?;
    let repo = if let Some(lease) = args.get("_workspace_lease_id") {
        crate::workspace::planning_root(
            store,
            id,
            &source_repo,
            lease.as_str().context("workspace lease must be a string")?,
        )?
    } else if let Some(cwd) = args.get("_workspace_cwd") {
        let cwd =
            Path::new(cwd.as_str().context("workspace cwd must be a path")?).canonicalize()?;
        ensure!(
            cwd.is_dir() && cwd.starts_with(&source_repo),
            "plugin cwd escapes charter repository"
        );
        cwd
    } else {
        source_repo.clone()
    };
    let crew = !crate::workspace::is_wake(store, args)?;
    let mut context = json!({"task_id": id, "title": title, "repo": repo,"source_repo":source_repo,"owner": task["owner"], "actions": charter["actions"], "crew": crew, "control":{"version":1,"cli_argv":[executable,"--home",store.home()],"writable":writable}});
    // Wake-specific text goes after the stable brief, so the brief's bytes
    // stay identical across wakes and its prefix keeps hitting the cache.
    let mut text = brief(&context);
    if let Some(wake) = args.get("wake").filter(|v| !v.is_null()) {
        let wake = wake.as_str().context("wake must be text")?;
        ensure!(wake.len() <= 32_768, "wake text exceeds 32 KiB");
        // A resumed native session already holds the brief.
        if args.get("wake_only") == Some(&json!(true)) {
            text.clear();
        } else {
            text.push_str("\n\n");
        }
        text.push_str(wake);
    }
    context["brief"] = json!(text);
    Ok((context, charter))
}
fn brief(context: &Value) -> String {
    let task = context["task_id"].as_str().unwrap_or("");
    let owner = context["owner"].as_str().unwrap_or("");
    if context["crew"] == json!(true) {
        return crew_brief(context, task, owner);
    }
    // The first mate's bytes stay fixed so its cached prefix keeps hitting.
    format!(
        "Task {task}: {}\nRepository: {}\nAuthorized actions: {}\nWork within the charter; coordinate with peers by task-linked messages; report tested results. Ask before production deployment. Never widen your authority or change a provider route. You are agent {owner}: operator commands are refused and actor/from must be {owner}.\nPrivateer control protocol v1: CLI base argv {}. Append COMMAND, --json, JSON as separate argv elements (no shell interpolation). Commands:\n- task.update {}\n- mail.inbox {{\"address\":\"{owner}\"}}; after handling, mail.ack {{\"address\":\"{owner}\",\"through_seq\":N}}\n- mail.send {{\"from\":\"{owner}\",\"to\":\"ship\",\"kind\":\"request|notice|reply\",\"body\":\"...\",\"task_id\":\"{task}\"}}; a reply sets correlation to the request id, and so does passing on another ship's message. ship.list {{}} shows who handles what\n- Builds/test suites that load the machine: job.start {{\"task_id\":\"{task}\",\"command\":[...],\"heavy\":true,\"wait_for_capacity\":true}}, its outcome goes to your first mate. resource.configure tunes host limits if your person asks\n- Crew (started by your first mate, not woken): assignment in note.get {{\"ship\":\"{owner}\",\"focus\":\"task:{task}\"}}; crew.report {{\"kind\":\"progress|decision\",\"body\":\"...\"}}; read a decision's answer with crew.answer {{\"id\":\"...\",\"wait_secs\":300}}\n- Mail reaches ships in other homes too; peer.call {{\"name\":\"peer\",\"command\":...,\"args\":{{...}}}} runs other commands there\n- task.create {{\"id\":\"unique-child\",\"parent\":\"{task}\",\"owner\":\"peer-owner\",\"title\":\"bounded outcome\"}}\n- task.handoff {{\"id\":\"{task}\",\"actor\":\"{owner}\",\"target\":\"peer-owner\"}}; custody changes only when target calls task.accept.\nRead structured results; do not claim a message, handoff, test, merge or deployment succeeded without evidence.",
        context["title"].as_str().unwrap_or(""),
        context["repo"].as_str().unwrap_or(""),
        context["actions"],
        context["control"]["cli_argv"],
        json!({"id":task,"actor":owner,"status":"running"})
    )
}
/// A crew's brief lists only crew commands. The task note is its first mate's
/// handoff, not its role; mail, task status, pushes and PRs stay the first
/// mate's, so the brief does not offer them.
fn crew_brief(context: &Value, task: &str, owner: &str) -> String {
    format!(
        "Task {task}: {}\nRepository: {}\nYou are crew of {owner} for this task, started by your first mate. The charter's actions ({}) are your first mate's; you do your assignment. Work within the charter; report tested results. Ask before production deployment. Never widen your authority or change a provider route.\nPrivateer control protocol v1: CLI base argv {}. Append COMMAND, --json, JSON as separate argv elements (no shell interpolation). Commands:\n- Your assignment: note.get {{\"ship\":\"{owner}\",\"focus\":\"task:{task}\"}} (field assignment). The notes beside it are your first mate's handoff, not your role: never act on your first mate's behalf.\n- Builds/test suites that load the machine: job.start {{\"task_id\":\"{task}\",\"command\":[...],\"heavy\":true,\"wait_for_capacity\":true}}, then crew.park it with a one-line next and end your turn; its outcome resumes you\n- Report with crew.report {{\"kind\":\"progress|decision\",\"body\":\"...\"}} (a decision asks); read its answer with crew.answer {{\"id\":\"...\",\"wait_secs\":300}}\nYour first mate mails, updates the task, and pushes, opens and merges PRs: commit locally and report the commit. Never git push or use gh to write. Read structured results; do not claim a test or commit succeeded without evidence.",
        context["title"].as_str().unwrap_or(""),
        context["repo"].as_str().unwrap_or(""),
        context["actions"],
        context["control"]["cli_argv"],
    )
}
fn command_result(charter: &Value, result: &Value) -> Result<Value> {
    let obj = result
        .as_object()
        .context("plugin command result must be an object")?;
    ensure!(
        obj.keys()
            .all(|k| ["argv", "cwd", "env_names"].contains(&k.as_str())),
        "plugin command has unsupported fields (environment values are forbidden)"
    );
    let argv = result["argv"]
        .as_array()
        .context("plugin command needs argv")?;
    ensure!(!argv.is_empty() && argv.len() <= 128, "invalid plugin argv");
    ensure!(
        argv.iter().all(|v| v
            .as_str()
            .is_some_and(|s| !s.contains('\0') && s.len() <= 32_768))
            && !argv[0].as_str().unwrap_or("").is_empty(),
        "invalid plugin argv element"
    );
    let cwd = Path::new(result["cwd"].as_str().context("plugin command needs cwd")?);
    ensure!(cwd.is_absolute(), "plugin cwd must be absolute");
    let cwd = cwd.canonicalize().context("plugin cwd missing")?;
    let repo =
        Path::new(charter["repo"].as_str().context("charter repo missing")?).canonicalize()?;
    ensure!(
        cwd.is_dir() && cwd.starts_with(&repo),
        "plugin cwd escapes charter repository"
    );
    let env = result["env_names"]
        .as_array()
        .context("plugin command needs env_names")?;
    ensure!(
        env.len() <= 128 && env.iter().all(|v| v.as_str().is_some_and(env_name)),
        "invalid plugin environment name"
    );
    Ok(json!({"argv": argv, "cwd": cwd, "env_names": env}))
}
fn env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

enum ReadEvent {
    Chunk(bool, Vec<u8>),
    End,
    Failure,
}
fn reader<R: Read + Send + 'static>(mut pipe: R, stderr: bool, tx: mpsc::SyncSender<ReadEvent>) {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(ReadEvent::End);
                    break;
                }
                Ok(n) => {
                    if tx
                        .send(ReadEvent::Chunk(stderr, buffer[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(ReadEvent::Failure);
                    break;
                }
            }
        }
    });
}
fn invoke(
    manifest: &Manifest,
    directory: &Path,
    mut request: Vec<u8>,
    wait: Wait,
) -> Result<Value> {
    request.push(b'\n');
    let mut command = Command::new(&manifest.executable[0]);
    command
        .args(&manifest.executable[1..])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("cannot start plugin executable")?;
    let pid = child.id();
    let mut input = child.stdin.take().context("plugin stdin missing")?;
    let (tx, rx) = mpsc::sync_channel(32);
    reader(
        child.stdout.take().context("plugin stdout missing")?,
        false,
        tx.clone(),
    );
    reader(
        child.stderr.take().context("plugin stderr missing")?,
        true,
        tx.clone(),
    );
    thread::spawn(move || {
        if input.write_all(&request).is_err() {
            let _ = tx.send(ReadEvent::Failure);
        }
    });
    let limit = wait.millis(manifest.timeout_ms);
    let deadline = Instant::now() + Duration::from_millis(limit);
    let mut stdout = Vec::new();
    let mut bytes = 0;
    let mut ended = 0;
    let result = (|| -> Result<Value> {
        loop {
            if Instant::now() >= deadline {
                bail!("plugin request timed out after {limit} ms");
            }
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(ReadEvent::Chunk(stderr, chunk)) => {
                    bytes += chunk.len();
                    ensure!(
                        bytes <= manifest.max_output_bytes,
                        "plugin output exceeded limit"
                    );
                    if !stderr {
                        stdout.extend(chunk);
                    }
                }
                Ok(ReadEvent::End) => ended += 1,
                Ok(ReadEvent::Failure) => bail!("plugin stdio failed"),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    ensure!(ended == 2, "plugin stdio disconnected");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if ended == 2 {
                // Observe exit without reaping, so a failure below can still
                // signal the plugin's group while its PID is reserved.
                if let Some(success) = crate::jobs::exited_unreaped(pid)? {
                    ensure!(success, "plugin executable failed (diagnostics withheld)");
                    let text =
                        std::str::from_utf8(&stdout).context("plugin returned invalid UTF-8")?;
                    let lines: Vec<&str> = text.lines().filter(|s| !s.trim().is_empty()).collect();
                    ensure!(lines.len() == 1, "plugin must return exactly one JSON line");
                    return serde_json::from_str(lines[0]).context("plugin returned invalid JSON");
                }
            }
        }
    })();
    if result.is_err() {
        // Child owns a separate group. Trusted adapters cannot leave ordinary
        // descendants after a failure or deadline. The plugin is not reaped
        // yet, so its PID still names that group and cannot have been reused.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_wait_has_a_floor_grows_with_load_and_stays_under_the_ceiling() {
        // A 2 s manifest still gets the floor on an idle machine.
        assert_eq!(launch_wait(2_000, 0.2), LAUNCH_FLOOR_MS);
        assert_eq!(launch_wait(2_000, f64::NAN), LAUNCH_FLOOR_MS);
        // A longer declared timeout is the base the load scales.
        assert_eq!(launch_wait(20_000, 1.0), 20_000);
        assert_eq!(launch_wait(2_000, 2.0), 2 * LAUNCH_FLOOR_MS);
        let mut last = 0;
        for load in [1.0, 1.5, 2.5, 3.5, 8.0, 100.0, f64::INFINITY] {
            let wait = launch_wait(2_000, load);
            assert!(wait >= last && wait <= LAUNCH_CEILING_MS, "{load}: {wait}");
            last = wait;
        }
        assert_eq!(last, LAUNCH_CEILING_MS);
        assert_eq!(launch_wait(30_000, 100.0), LAUNCH_CEILING_MS);
        assert_eq!(Wait::Declared.millis(2_000), 2_000);
        assert!(Wait::Launch.millis(2_000) >= LAUNCH_FLOOR_MS);
    }

    /// A real subprocess that answers after its declared timeout but well
    /// within the launch floor.
    #[test]
    fn a_slow_plugin_fails_an_ordinary_request_but_not_a_launch_plan() {
        let directory = tempfile::TempDir::new().unwrap();
        let script = directory.path().join("slow.py");
        fs::write(
            &script,
            "import json,sys,time\nr=json.loads(sys.stdin.readline())\ntime.sleep(1.5)\nprint(json.dumps({'id':r['id'],'version':1,'ok':True}))\n",
        )
        .unwrap();
        let manifest = Manifest {
            schema_version: 1,
            id: "slow".into(),
            kind: None,
            executable: vec!["python3".into(), script.to_string_lossy().into()],
            timeout_ms: 300,
            max_output_bytes: 4_096,
        };
        let request = || serde_json::to_vec(&json!({"id": "r1", "version": 1})).unwrap();
        let error = invoke(&manifest, directory.path(), request(), Wait::Declared).unwrap_err();
        assert!(
            error.to_string().contains("timed out after 300 ms"),
            "{error}"
        );
        let response = invoke(&manifest, directory.path(), request(), Wait::Launch).unwrap();
        assert_eq!(response["id"], "r1");
        assert_eq!(response["ok"], true);
    }
}
