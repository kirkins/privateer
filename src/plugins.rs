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

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
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
                !manifest.id.is_empty() && manifest.id.len() <= 128,
                "invalid plugin id"
            );
            ensure!(
                !manifest.executable.is_empty() && manifest.executable.len() <= 64,
                "invalid plugin executable"
            );
            ensure!(
                manifest
                    .executable
                    .iter()
                    .all(|s| !s.is_empty() && !s.contains('\0') && s.len() <= 4096),
                "invalid plugin executable argument"
            );
            ensure!(
                (1..=30_000).contains(&manifest.timeout_ms),
                "plugin timeout outside allowed range"
            );
            ensure!(
                (128..=1_048_576).contains(&manifest.max_output_bytes),
                "plugin output limit outside allowed range"
            );
            let config = config_path
                .map(|path| -> Result<PathBuf> {
                    ensure!(
                        !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                        "plugin configuration cannot be a symlink"
                    );
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    let metadata = fs::metadata(&path)?;
                    ensure!(
                        metadata.permissions().mode() & 0o077 == 0
                            && metadata.uid() == unsafe { libc::geteuid() },
                        "plugin configuration must be user-owned and private (mode 0600)"
                    );
                    Ok(path)
                })
                .transpose()?;
            let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
            ensure!(options.is_object(), "plugin options must be an object");
            let id = uuid::Uuid::new_v4().to_string();
            let method = if command == "plugin.command" {
                "command"
            } else {
                "validate"
            };
            let mut request = json!({"id": id, "version": 1, "method": method, "context": context, "options": options});
            if let Some(path) = config {
                request["config_path"] = json!(path);
            }
            let request = serde_json::to_vec(&request)?;
            ensure!(request.len() <= 65_536, "plugin request too large");
            let response = invoke(
                &manifest,
                manifest_path.parent().context("manifest needs parent")?,
                request,
            )?;
            ensure!(
                response["version"] == 1 && response["id"] == id,
                "plugin response identity or version mismatch"
            );
            ensure!(
                response["ok"] == true,
                "plugin refused request (diagnostics withheld to avoid exposing credentials)"
            );
            let mut result = response
                .get("result")
                .cloned()
                .context("plugin omitted result")?;
            if method == "command" {
                result = command_result(&charter, &result)?;
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
        let manifest = absolute_file(field(args, "manifest")?)?;
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
    let executable = std::env::current_exe().context("cannot locate Privateer executable")?;
    let mut context = json!({"task_id": id, "title": title, "repo": charter["repo"], "owner": task["owner"], "actions": charter["actions"], "control":{"version":1,"cli_argv":[executable,"--home",store.home()]}});
    context["brief"] = json!(brief(&context));
    Ok((context, charter))
}
fn brief(context: &Value) -> String {
    let task = context["task_id"].as_str().unwrap_or("");
    let owner = context["owner"].as_str().unwrap_or("");
    format!(
        "Task {task}: {}\nRepository: {}\nAuthorized actions: {}\nWork within the charter; coordinate with peers by task-linked messages; report tested results. Ask before production deployment. Never widen your authority or change a provider route. You are agent {owner}: operator commands are refused and actor/from must be {owner}.\nPrivateer control protocol v1: CLI base argv {}. Append COMMAND, --json, JSON as separate argv elements (no shell interpolation). Commands:\n- task.update {}\n- mail.inbox {{\"address\":\"{owner}\"}}; after handling, mail.ack {{\"address\":\"{owner}\",\"through_seq\":N}}\n- mail.send {{\"from\":\"{owner}\",\"to\":\"ship\",\"kind\":\"request|notice|reply\",\"body\":\"...\",\"task_id\":\"{task}\"}}; a reply sets correlation to the request id. ship.list {{}} shows who handles what\n- Builds/test suites that load the machine: job.start {{\"task_id\":\"{task}\",\"command\":[...],\"heavy\":true,\"wait_for_capacity\":true}}, then job.status. resource.configure tunes host limits if your person asks\n- Mail reaches ships in other homes too; peer.call {{\"name\":\"peer\",\"command\":...,\"args\":{{...}}}} runs other commands there\n- task.create {{\"id\":\"unique-child\",\"parent\":\"{task}\",\"owner\":\"peer-owner\",\"title\":\"bounded outcome\"}}\n- task.handoff {{\"id\":\"{task}\",\"actor\":\"{owner}\",\"target\":\"peer-owner\"}}; custody changes only when target calls task.accept.\nRead structured results; do not claim a message, handoff, test, merge or deployment succeeded without evidence.",
        context["title"].as_str().unwrap_or(""),
        context["repo"].as_str().unwrap_or(""),
        context["actions"],
        context["control"]["cli_argv"],
        json!({"id":task,"actor":owner,"status":"running"})
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
fn invoke(manifest: &Manifest, directory: &Path, mut request: Vec<u8>) -> Result<Value> {
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
    let deadline = Instant::now() + Duration::from_millis(manifest.timeout_ms);
    let mut stdout = Vec::new();
    let mut bytes = 0;
    let mut ended = 0;
    let result = (|| -> Result<Value> {
        loop {
            if Instant::now() >= deadline {
                bail!("plugin request timed out");
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
