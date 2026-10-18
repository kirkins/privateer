mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_privateer")
}

fn run(home: &Path, command: &str, args: Value, ssh: Option<&Path>) -> Output {
    let mut process = support::command(binary());
    process.args([
        "--home",
        home.to_str().unwrap(),
        command,
        "--json",
        &args.to_string(),
    ]);
    if let Some(ssh) = ssh {
        process.env("PRIVATEER_SSH_EXECUTABLE", ssh);
    }
    process.output().unwrap()
}

fn run_rpc(home: &Path, command: &str, args: Value, ssh: &Path) -> Output {
    let mut child = support::command(binary())
        .args(["--home", home.to_str().unwrap(), "rpc"])
        .env("PRIVATEER_SSH_EXECUTABLE", ssh)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // JSON belongs on the protocol stream. A 200 KiB argument exceeds Linux's
    // per-argument limit on hosts with 4 KiB pages before Privateer can run.
    let request = json!({"id":"deadline-test","command":command,"args":args});
    let mut stdin = child.stdin.take().unwrap();
    serde_json::to_writer(&mut stdin, &request).unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn ok(home: &Path, command: &str, args: Value, ssh: Option<&Path>) -> Value {
    let output = run(home, command, args, ssh);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(unix)]
fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn put(
    home: &Path,
    remote: &Path,
    ssh_host: Option<&str>,
    timeout_ms: u64,
    limit: usize,
    executable: &Path,
) {
    let mut config = json!({"name":"infrastructure","transport":if ssh_host.is_some(){"ssh"}else{"local"},"executable":executable,"home":remote,"timeout_ms":timeout_ms,"max_response_bytes":limit});
    if let Some(host) = ssh_host {
        config["host"] = json!(host);
    }
    ok(home, "peer.put", config, None);
}

#[test]
fn local_peer_round_trip_is_durable_and_correlated() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    let remote = fixture.path().join("department");
    put(&home, &remote, None, 10_000, 1_048_576, Path::new(binary()));
    let peers = ok(&home, "peer.list", json!({}), None);
    assert_eq!(peers["peers"][0]["name"], "infrastructure");
    let response = ok(
        &home,
        "peer.call",
        json!({"name":"infrastructure","id":"specific-request","command":"status","args":{}}),
        None,
    );
    assert_eq!(response["id"], "specific-request");
    assert_eq!(response["ok"], true);
    assert!(response.get("result").is_some());
}

#[test]
fn rejects_unsafe_endpoint_and_path_configuration() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    let base = json!({"name":"infra","host":"department","executable":binary(),"home":fixture.path().join("remote")});
    for (field, invalid) in [
        ("host", "-oProxyCommand=evil"),
        ("host", "user@host;touch-pwn"),
        ("host", "user@@host"),
        ("executable", "relative/runtime"),
        ("home", "/tmp/../secret"),
        ("home", "/tmp/home\ncommand"),
        ("name", "bad name"),
    ] {
        let mut config = base.clone();
        config[field] = json!(invalid);
        assert!(
            !run(&home, "peer.put", config, None).status.success(),
            "accepted {field}={invalid}"
        );
    }
}

#[cfg(unix)]
#[test]
fn ssh_remote_shell_paths_are_quoted_including_apostrophes_and_substitutions() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    let remote = fixture.path().join("peer's home; $(touch PWNED)");
    let executable = fixture.path().join("runtime's binary; $(touch PWNED)");
    fs::copy(binary(), &executable).unwrap();
    let ssh = script(
        fixture.path(),
        "fake-ssh",
        "for argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"",
    );
    put(
        &home,
        &remote,
        Some("captain@department"),
        10_000,
        1_048_576,
        &executable,
    );
    let response = ok(
        &home,
        "peer.call",
        json!({"name":"infrastructure","id":"quoted","command":"status","args":{}}),
        Some(&ssh),
    );
    assert_eq!(response["ok"], true);
    assert!(remote.join("state.sqlite3").exists() || remote.read_dir().unwrap().count() > 0);
    assert!(!fixture.path().join("PWNED").exists());
    assert!(!Path::new("PWNED").exists());
}

#[cfg(unix)]
#[test]
fn remote_error_envelope_is_preserved_without_interpreting_as_local_success() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    let remote = fixture.path().join("department");
    put(&home, &remote, None, 10_000, 1_048_576, Path::new(binary()));
    let response = ok(
        &home,
        "peer.call",
        json!({"name":"infrastructure","id":"failed","command":"task.show","args":{"id":"missing"}}),
        None,
    );
    assert_eq!(response["id"], "failed");
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().unwrap().contains("task"));
}

#[cfg(unix)]
#[test]
fn rejects_wrong_ids_and_multiple_responses() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    put(
        &home,
        &fixture.path().join("department"),
        Some("department"),
        2_000,
        1_048_576,
        Path::new(binary()),
    );
    for (name, body) in [
        (
            "mismatch",
            "printf '%s\\n' '{\"id\":\"wrong\",\"ok\":true,\"result\":{}}'",
        ),
        (
            "multiple",
            "printf '%s\\n' '{\"id\":\"request\",\"ok\":true,\"result\":{}}' '{\"id\":\"request\",\"ok\":true,\"result\":{}}'",
        ),
    ] {
        let ssh = script(fixture.path(), name, body);
        let result = run(
            &home,
            "peer.call",
            json!({"name":"infrastructure","id":"request","command":"status"}),
            Some(&ssh),
        );
        assert!(!result.status.success());
    }
}

#[cfg(unix)]
#[test]
fn response_and_stderr_are_bounded() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    put(
        &home,
        &fixture.path().join("department"),
        Some("department"),
        2_000,
        128,
        Path::new(binary()),
    );
    let ssh = script(
        fixture.path(),
        "overflow",
        "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
    );
    let result = run(
        &home,
        "peer.call",
        json!({"name":"infrastructure","command":"status"}),
        Some(&ssh),
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("byte limit"));
    let ssh = script(
        fixture.path(),
        "stderr-overflow",
        "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' >&2; done",
    );
    let result = run(
        &home,
        "peer.call",
        json!({"name":"infrastructure","command":"status"}),
        Some(&ssh),
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("byte limit"));
}

#[cfg(unix)]
#[test]
fn deadline_covers_blocked_request_writes_and_inherited_output_pipes() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    put(
        &home,
        &fixture.path().join("department"),
        Some("department"),
        100,
        1_048_576,
        Path::new(binary()),
    );
    for (name, body) in [("blocked", "sleep 5"), ("inherited", "sleep 5 &\nexit 0")] {
        let ssh = script(fixture.path(), name, body);
        let started = Instant::now();
        let result = run_rpc(
            &home,
            "peer.call",
            json!({"name":"infrastructure","command":"status","args":{"padding":"x".repeat(200_000)}}),
            &ssh,
        );
        assert!(result.status.success());
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response["id"], "deadline-test");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(unix)]
#[test]
fn agent_role_travels_to_the_remote_home() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("navigator");
    let remote = fixture.path().join("department");
    // Provision as the operator: agent RPC may only open an existing home.
    ok(&remote, "status", json!({}), None);
    let ssh = script(
        fixture.path(),
        "fake-ssh",
        "for argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"",
    );
    for host in [None, Some("department")] {
        put(&home, &remote, host, 10_000, 1_048_576, Path::new(binary()));
        let call = |command: &str, args: Value| -> Value {
            let output = support::command(binary())
                .args(["--home", home.to_str().unwrap(), "peer.call", "--json"])
                .arg(json!({"name":"infrastructure","command":command,"args":args}).to_string())
                .env("PRIVATEER_SSH_EXECUTABLE", &ssh)
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "apps")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        };
        assert_eq!(call("status", json!({}))["ok"], true, "{host:?}");
        let refused = call("charter.put", json!({"id":"remote","active":true}));
        assert_eq!(refused["ok"], false, "{host:?}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("operator command"),
            "{refused}"
        );
        let spoofed = call("message.inbox", json!({"actor":"infra"}));
        assert_eq!(spoofed["ok"], false, "{host:?}");
        assert_eq!(call("message.inbox", json!({"actor":"apps"}))["ok"], true);
    }
}

#[cfg(unix)]
#[test]
fn a_correction_reaches_the_thread_its_sender_home_never_stored() {
    let fixture = TempDir::new().unwrap();
    let home = fixture.path().join("privateer");
    let remote = fixture.path().join("fitforge");
    // Each home reaches the other, as two machines on one machine do.
    put(&home, &remote, None, 10_000, 1_048_576, Path::new(binary()));
    put(&remote, &home, None, 10_000, 1_048_576, Path::new(binary()));
    for (this, other) in [(&home, "forge"), (&remote, "rig")] {
        ok(
            this,
            "ship.put",
            json!({"name":"rig","scope":"deploys"}),
            None,
        );
        ok(
            this,
            "ship.put",
            json!({"name":"forge","scope":"forges"}),
            None,
        );
        ok(
            this,
            "ship.put",
            json!({"name":other,"scope":"other","location":{"peer":"infrastructure"}}),
            None,
        );
    }
    // rig asks the captain, so the parent request lives only in this home.
    ok(
        &home,
        "mail.send",
        json!({"id":"baf32096","from":"rig","to":"captain","kind":"request",
        "body":"Which machine can I use?"}),
        None,
    );
    // rig raises a follow-up on that answer, naming the request it grows from.
    let follow_up = ok(
        &home,
        "mail.send",
        json!({"id":"e376ceec","from":"rig","to":"forge","kind":"request",
        "because_of":"baf32096","body":"Release the laptop"}),
        None,
    );
    assert_eq!(follow_up["because_of"], "baf32096");
    // The link carries the follow-up and its link, which is how the other
    // home reads it.
    let held = ok(&remote, "mail.show", json!({"id":"e376ceec"}), None);
    assert_eq!(held["because_of"], "baf32096");
    assert_eq!(held["correlation"], json!(null));
    // fitforge corrects the thread it never stored: the parent request is in
    // another home, and a low notice that names it is still delivered.
    let correction = ok(
        &remote,
        "mail.send",
        json!({"id":"correction","from":"forge","to":"rig","kind":"notice","urgency":"low",
        "correlation":"baf32096","body":"Correction: the desktop is free"}),
        None,
    );
    assert_eq!(correction["delivery"], "delivered");
    // rig holds the correction in its own home, and it is actionable: the
    // follow-up rig raised there is still open.
    let pending = ok(&home, "mail.pending", json!({"address":"rig"}), None);
    assert_eq!(pending["actionable"], 1);
    let arrived = ok(&home, "mail.show", json!({"id":"correction"}), None);
    assert_eq!(arrived["correlation"], "baf32096");
    assert_eq!(arrived["urgency"], "low");
    // A reply naming that same unknown thread is refused where it is written:
    // the other home cannot judge an answer it holds no request for.
    assert!(
        !run(
            &remote,
            "mail.send",
            json!({"id":"answer","from":"forge","to":"rig","kind":"reply",
            "correlation":"baf32096","body":"Done"}),
            None,
        )
        .status
        .success()
    );
}
