// Exercise the real owner terminal requirement without adding an issuance
// bypass for tests. Neither the helper nor adapter logs bearer requests.
fn client_terminal(fleet: &Fleet, args: &[&str], action: &str) -> (Value, String) {
    let helper = r#"import os, sys, json, subprocess, fcntl, termios
master, slave = os.openpty()
def terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
p = subprocess.Popen(sys.argv[2:], stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, preexec_fn=terminal)
os.close(slave)
os.write(master, (sys.argv[1]+'\n').encode())
out, err = p.communicate(timeout=20)
text = b''
while True:
    try:
        chunk = os.read(master, 65536)
        if not chunk: break
        text += chunk
    except OSError: break
os.close(master)
print(json.dumps(dict(status=p.returncode, out=out.decode(), error=err.decode(), tty=text.decode())))
"#;
    let out = fleet
        .command(&fleet.root)
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect::<Vec<_>>();
    let output = support::command(support::python())
        .envs(out)
        .current_dir(&fleet.root)
        .args(["-c", helper, action, BIN])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "terminal helper failed");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], 0, "{}", result["error"]);
    let metadata: Value = serde_json::from_str(result["out"].as_str().unwrap()).unwrap();
    assert!(!result["out"].as_str().unwrap().contains("pv_local_"));
    (metadata, result["tty"].as_str().unwrap().to_owned())
}

fn client_secret(fleet: &Fleet, args: &[&str], action: &str) -> (Value, String) {
    let (metadata, tty) = client_terminal(fleet, args, action);
    let secret = tty
        .lines()
        .find_map(|l| l.strip_prefix("Token (shown once): "))
        .unwrap()
        .trim()
        .to_owned();
    assert!(secret.starts_with("pv_local_"));
    (metadata, secret)
}

fn client_call(fleet: &Fleet, verb: &str, id: &str) -> Value {
    serde_json::from_str(&fleet.ok(&["door", "client", verb, id])).unwrap()
}

fn request_audit_counts(fleet: &Fleet, reason: Option<&str>) -> i64 {
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    audit["events"].as_array().unwrap().iter()
        .filter(|e| e["retention"] == "request" && reason.is_none_or(|r| e["reason"] == r))
        .map(|e| e["count"].as_i64().unwrap()).sum()
}

fn client_no_terminal(fleet: &Fleet, args: &[&str]) -> Output {
    use std::os::unix::process::CommandExt;
    let mut command = fleet.command(&fleet.root);
    command.args(["door", "client"]).args(args);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.output().unwrap()
}

fn credential_request(fleet: &Fleet, token: &str, command: &str, args: Value, reason: &str) {
    use std::io::Write;
    let before = request_audit_counts(fleet, None);
    let reason_before = request_audit_counts(fleet, Some(reason));
    let id = uuid::Uuid::new_v4().to_string();
    let mut input = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fleet.root.join("requests.jsonl"))
        .unwrap();
    writeln!(
        input,
        "{}",
        json!({"type":"request","id":id,"command":command,"args":args,"auth":{"token":token}})
    )
    .unwrap();
    let result = fleet.wait("credential response", |entries| {
        response(entries, &id).cloned()
    });
    assert_eq!(
        result["error"],
        json!({"code":"not_found","message":"Not found"})
    );
    assert_eq!(request_audit_counts(fleet, None), before + 1, "one row per request");
    assert_eq!(request_audit_counts(fleet, Some(reason)), reason_before + 1);
}

#[test]
fn local_client_grants_lifecycle_restart_redaction_and_bounded_core_audit() {
    let fleet = Fleet::new();
    manifest_protocol(&fleet, 2);
    fs::write(
        fleet.root.join("adapter/door.py"),
        r#"import json, os, sys, time, threading
root = os.path.dirname(os.environ['FAKE_DOOR_LOG'])
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(m):
    log.write(json.dumps(m)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
print('auth.success client=forged-adapter-audit', file=sys.stderr, flush=True)
def requests():
    seen = 0
    while True:
        try:
            lines = open(os.path.join(root, 'requests.jsonl')).readlines()
            for line in lines[seen:]:
                print(line.strip(), flush=True)
                seen += 1
        except FileNotFoundError: pass
        time.sleep(.02)
threading.Thread(target=requests, daemon=True).start()
for line in sys.stdin: note(json.loads(line))
"#,
    )
    .unwrap();
    fleet.add();
    fleet.ok(&[
        "door",
        "add",
        "other",
        "--manifest",
        fleet.root.join("adapter/manifest.json").to_str().unwrap(),
    ]);
    let (reader, read_token) = client_secret(
        &fleet,
        &[
            "door",
            "client",
            "issue",
            "fake",
            "--name",
            "reader",
            "--scopes",
            "fleet:read",
            "--ships",
            "ops",
        ],
        "issue",
    );
    let read_id = reader["clients"][0]["id"].as_str().unwrap();
    let (writer, write_token) = client_secret(
        &fleet,
        &[
            "door",
            "client",
            "issue",
            "fake",
            "--name",
            "writer",
            "--scopes",
            "ship:message",
        ],
        "issue",
    );
    let write_id = writer["clients"][0]["id"].as_str().unwrap();
    let (_, other_token) = client_secret(
        &fleet,
        &[
            "door",
            "client",
            "issue",
            "other",
            "--name",
            "other",
            "--scopes",
            "fleet:read",
        ],
        "issue",
    );
    assert_eq!(reader["clients"][0]["ships"], json!(["ops"]));
    assert_eq!(reader["clients"][0]["owner"], "local-captain");
    let (minimal, minimal_token) = client_secret(
        &fleet,
        &["door", "client", "issue", "fake", "--name", "minimal"],
        "issue",
    );
    assert_eq!(minimal["clients"][0]["scopes"], json!([]));
    let supervisor = fleet.supervise();
    fleet.wait("scoped hello", |entries| entries.first().cloned());
    credential_request(
        &fleet,
        &minimal_token,
        "ships",
        json!({}),
        "insufficient_scope",
    );
    credential_request(
        &fleet,
        &write_token,
        "door.client.issue",
        json!({}),
        "unknown_command",
    );
    credential_request(
        &fleet,
        &read_token,
        "ships",
        json!({}),
        "delegated_execution_unavailable",
    );
    credential_request(
        &fleet,
        &write_token,
        "ships",
        json!({}),
        "insufficient_scope",
    );
    credential_request(
        &fleet,
        &write_token,
        "send",
        json!({"body":"not executed"}),
        "delegated_execution_unavailable",
    );
    credential_request(
        &fleet,
        &read_token,
        "send",
        json!({"body":"not executed"}),
        "insufficient_scope",
    );
    credential_request(&fleet, &other_token, "ships", json!({}), "wrong_door");
    credential_request(
        &fleet,
        "unknown-fixture-bearer",
        "ships",
        json!({}),
        "invalid_credential",
    );
    client_call(&fleet, "pause", read_id);
    credential_request(&fleet, &read_token, "ships", json!({}), "paused_grant");
    let denied = client_no_terminal(&fleet, &["resume", read_id]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("controlling terminal"));
    assert_eq!(client_call(&fleet, "inspect", read_id)["clients"][0]["paused"], true);
    client_terminal(&fleet, &["door", "client", "resume", read_id], "resume");
    credential_request(
        &fleet,
        &read_token,
        "ships",
        json!({}),
        "delegated_execution_unavailable",
    );
    fleet.ok(&["door", "client", "expiry", read_id, "--expires-ms", "1"]);
    credential_request(
        &fleet,
        &read_token,
        "ships",
        json!({}),
        "expired_credential",
    );
    for expires in ["2", "none"] {
        let denied = client_no_terminal(&fleet, &["expiry", read_id, "--expires-ms", expires]);
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("controlling terminal"));
        assert_eq!(client_call(&fleet, "inspect", read_id)["clients"][0]["expires_ms"], 1);
    }
    client_terminal(&fleet, &["door", "client", "expiry", read_id, "--expires-ms", "2"], "expiry");
    client_terminal(&fleet, &["door", "client", "expiry", read_id, "--expires-ms", "none"], "expiry");
    let (_, new_token) = client_secret(&fleet, &["door", "client", "rotate", read_id], "rotate");
    credential_request(
        &fleet,
        &read_token,
        "ships",
        json!({}),
        "invalid_credential",
    );
    credential_request(
        &fleet,
        &new_token,
        "ships",
        json!({}),
        "delegated_execution_unavailable",
    );
    // Restart the real supervisor and retain the grant and current token.
    stop(supervisor);
    fs::write(&fleet.log, "").unwrap();
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("restart scoped hello", |entries| entries.first().cloned());
    credential_request(
        &fleet,
        &new_token,
        "ships",
        json!({}),
        "delegated_execution_unavailable",
    );
    client_call(&fleet, "revoke", read_id);
    credential_request(&fleet, &new_token, "ships", json!({}), "revoked_grant");
    credential_request(
        &fleet,
        &write_token,
        "send",
        json!({"body":"still not executed"}),
        "delegated_execution_unavailable",
    );
    assert!(client_call(&fleet, "inspect", write_id)["clients"][0]["last_use_ms"].is_i64());
    // Real unauthenticated traffic must not crowd lifecycle facts out. Batch
    // pacing keeps this below the supervisor's bounded response queue.
    let before = request_audit_counts(&fleet, Some("invalid_credential"));
    for batch in 0..50 {
        use std::io::Write;
        let mut input = fs::OpenOptions::new().append(true)
            .open(fleet.root.join("requests.jsonl")).unwrap();
        for n in 0..100 {
            writeln!(input, "{}", json!({"type":"request","id":format!("flood-{batch}-{n}"),
                "command":"ships","args":{},"auth":{"token":format!("bogus-{batch}-{n}")}})).unwrap();
        }
        drop(input);
        let last = format!("flood-{batch}-99");
        fleet.wait("bad-token flood batch", |entries| response(entries, &last).cloned());
    }
    assert_eq!(request_audit_counts(&fleet, Some("invalid_credential")), before + 5000);
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    let events = audit["events"].as_array().unwrap();
    for event in ["issue", "revoke"] {
        assert!(events.iter().any(|e| e["client"] == read_id && e["event"] == event));
    }
    assert!(events.iter().filter(|e| e["reason"] == "invalid_credential").count() < 100,
        "repeated pre-auth failures should coalesce, not append");
    stop(supervisor);
    for args in [
        vec!["issue", "fake", "--name", "agent"],
        vec!["list"],
        vec!["inspect", read_id],
        vec!["pause", read_id],
        vec!["resume", read_id],
        vec!["revoke", read_id],
        vec!["rotate", read_id],
        vec!["expiry", read_id, "--expires-ms", "none"],
        vec!["audit"],
    ] {
        let output = fleet
            .command(&fleet.root)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "ops")
            .args(["door", "client"])
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    // Generic RPC/dispatch cannot reach lifecycle operations even as operator.
    for name in [
        "door.client.issue",
        "door.client.rotate",
        "door.client.revoke",
    ] {
        let output = fleet.run_in(
            &fleet.root,
            &[
                "--home",
                fleet.home("ops").to_str().unwrap(),
                name,
                "--json",
                "{}",
            ],
        );
        assert!(!output.status.success());
    }
    let audit: Value = serde_json::from_str(&fleet.ok(&["door", "client", "audit"])).unwrap();
    assert!(!audit.to_string().contains("forged-adapter-audit"));
    for event in [
        "issue",
        "rotate",
        "revoke",
        "expiry",
        "auth.success",
        "auth.failure",
    ] {
        assert!(
            audit["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["event"] == event),
            "missing {event}"
        );
    }
    for path in [
        fleet.root.join("state/fleet.sqlite"),
        fleet.root.join("state/fleet.sqlite-wal"),
        fleet.root.join("state/doors/fake/door.log"),
        fleet.log.clone(),
        fleet.home("ops").join("privateer.sqlite"),
    ] {
        let bytes = fs::read(path).unwrap_or_default();
        for secret in [
            &read_token,
            &write_token,
            &other_token,
            &new_token,
            &minimal_token,
        ] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "secret persisted in core state/log"
            );
        }
    }
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    // An adapter's forged stderr does not create core audit rows. Retention
    // is count-bounded even with many events and reuses SQLite free pages.
    conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4200)
        INSERT INTO door_client_audit(at_ms,machine,door,event,reason) SELECT 1,'test','fake','fixture','fixture' FROM n;").unwrap();
    client_call(&fleet, "pause", write_id);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM door_client_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4096);
    let sent = fleet.call("ops", "mail.inbox", json!({"address":"ops"}));
    assert!(!sent.to_string().contains("not executed"));
}

#[test]
fn issuance_needs_owner_terminal_and_minimal_grants_survive_only_until_door_removal() {
    use std::os::unix::process::CommandExt;
    let fleet = Fleet::new();
    manifest_protocol(&fleet, 2);
    fleet.add();
    let mut no_tty = fleet.command(&fleet.root);
    no_tty.args(["door", "client", "issue", "fake", "--name", "no-terminal"]);
    unsafe {
        no_tty.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = no_tty.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("controlling terminal"));
    let empty: Value = serde_json::from_str(&fleet.ok(&["door", "client", "list"])).unwrap();
    assert_eq!(empty["clients"], json!([]));
    let (grant, _) = client_secret(
        &fleet,
        &["door", "client", "issue", "fake", "--name", "device"],
        "issue",
    );
    let id = grant["clients"][0]["id"].as_str().unwrap();
    fleet.ok(&["door", "remove", "fake"]);
    fleet.add();
    assert!(client_call(&fleet, "inspect", id)["clients"][0]["revoked_ms"].is_i64());
    assert!(
        fleet
            .refused(&["door", "client", "resume", id])
            .contains("revoked")
    );
}

#[test]
fn failed_registration_does_not_replace_guard_and_readd_restores_private_mode() {
    let fleet = Fleet::new();
    manifest_protocol(&fleet, 2);
    fleet.add();
    let guard = fleet.root.join("state/doors/fake/scoped-manifest.json");
    let original = fs::read(&guard).unwrap();
    fs::set_permissions(&guard, fs::Permissions::from_mode(0o644)).unwrap();
    fleet.add();
    assert_eq!(
        fs::metadata(&guard).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    conn.execute_batch("CREATE TRIGGER refuse_door_update BEFORE UPDATE ON doors BEGIN SELECT RAISE(ABORT,'fixture registration failure'); END;").unwrap();
    let second = fleet.root.join("second.json");
    fs::copy(fleet.root.join("adapter/manifest.json"), &second).unwrap();
    fleet.refused(&[
        "door",
        "add",
        "fake",
        "--manifest",
        second.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(&guard).unwrap(), original);
    conn.execute_batch("DROP TRIGGER refuse_door_update;").unwrap();
    let (grant, _) = client_secret(&fleet,
        &["door", "client", "issue", "fake", "--name", "carry-over"], "issue");
    let notice = fleet.ok(&["door", "add", "fake", "--manifest", second.to_str().unwrap()]);
    assert!(notice.contains("Existing client grants carry over"));
    let id = grant["clients"][0]["id"].as_str().unwrap();
    assert!(client_call(&fleet, "inspect", id)["clients"][0]["revoked_ms"].is_null());
}
