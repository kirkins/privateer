mod support;

use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;

fn request() -> Value {
    json!({"id":"alpha","expected_revision":0,"grantee":"alpha","environment":"beta",
        "production":true,"notify":true,"expires_ms":privateer::store::Store::now_ms() + 86_400_000,"max_uses":3,"wait_window_ms":1_800_000,
        "artifact_rule":{"kind":"green_branch_commit","repository_id":"gamma",
            "branch_ref":"refs/heads/main","required_checks":["test"],"app_id":"gamma",
            "workflow_path":".forgejo/workflows/test.yml","event":"push","provenance_mode":"runtime_build"},
        "verifier":{"id":"gamma","sha256":"a".repeat(64)},
        "adapter":{"id":"beta","sha256":"b".repeat(64)}})
}

fn command(root: &std::path::Path) -> std::process::Command {
    let mut cmd = support::isolated(root);
    cmd.args(["--home", root.join("home").to_str().unwrap()])
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("TMPDIR", root);
    cmd
}

#[test]
fn rpc_and_agents_cannot_create_deployment_grants() {
    let dir = support::tempdir();
    let mut child = command(dir.path())
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"id":"alpha","command":"deploy_grant.put","args":request()})
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result.to_string().contains("terminal"), "{result}");
    let output = command(dir.path())
        .args([
            "--role",
            "agent",
            "--actor",
            "alpha",
            "deploy_grant.put",
            "--json",
            &request().to_string(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("operator"));
}

#[test]
fn cli_requires_fresh_terminal_consent_then_lifecycle_remains_dormant() {
    let dir = support::tempdir();
    let output = command(dir.path())
        .args(["ship.put", "--json", r#"{"name":"alpha"}"#])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let input = dir.path().join("grant.json");
    std::fs::write(&input, request().to_string()).unwrap();
    // A real controlling terminal, not a JSON consent flag. The timeout bounds
    // a broken prompt; elapsed time is not part of the authority assertion.
    let script = r#"
import errno, os, pty, select, signal, sys, time
pid, fd = pty.fork()
if pid == 0:
    os.execv(sys.argv[1], [sys.argv[1], '--home', sys.argv[2], 'deploy_grant.put', '--file', sys.argv[3]])
buf = b''
confirmed = False
deadline = time.monotonic() + 30
try:
    while True:
        left = deadline - time.monotonic()
        if left <= 0: raise AssertionError('terminal consent timed out: ' + repr(buf))
        if not select.select([fd], [], [], left)[0]: continue
        try: chunk = os.read(fd, 65536)
        except OSError as e:
            if e.errno == errno.EIO: break
            raise
        if not chunk: break
        buf += chunk
        if not confirmed and b'Type deploy-grant to confirm:' in buf:
            os.write(fd, b'deploy-grant\n')
            confirmed = True
    _, status = os.waitpid(pid, 0)
    pid = None
    assert confirmed and os.waitstatus_to_exitcode(status) == 0, repr(buf)
finally:
    os.close(fd)
    if pid is not None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
"#;
    let output = support::command(support::python())
        .args(["-c", script, support::BIN])
        .arg(dir.path().join("home"))
        .arg(&input)
        .env("HOME", dir.path().join("user"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("PRIVATEER_FLEET", dir.path().join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", dir.path().join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (verb, expected_revision, revision) in [("pause", 1, 2), ("resume", 2, 3), ("revoke", 3, 4)]
    {
        let output = command(dir.path())
            .args([
                &format!("deploy_grant.{verb}"),
                "--json",
                &json!({"id":"alpha","expected_revision":expected_revision}).to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let grant: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(grant["revision"], revision);
        assert_eq!(grant["consumption_enabled"], false);
        assert_eq!(grant["wait_window_ms"], 1_800_000);
    }
    let output = command(dir.path())
        .args(["deployment.start", "--json", "{}"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let store = privateer::store::Store::open(&dir.path().join("home")).unwrap();
    let exists: bool = store
        .connection()
        .query_row("SELECT EXISTS(SELECT 1 FROM approvals)", [], |r| r.get(0))
        .unwrap();
    assert!(!exists);
}
