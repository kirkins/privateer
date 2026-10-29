// Two disposable peers expose the same ship, but distinct recorded jobs.
// A transport wrapper can hold A's completed answer, replace its link with B,
// and only then release the answer. No clock advancement or sleep is involved.
#[test]
fn replaced_links_never_publish_previous_peer_runtime_or_native_facts() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    for (machine, job) in [("beta", "alpha-job"), ("gamma", "gamma-job")] {
        let root = fleet.root.join(machine);
        let home = root.join("survey");
        fs::create_dir_all(&home).unwrap();
        let output = fleet
            .command(&home)
            .env("HOME", &root)
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
            .args(["init", "--ship", "survey", "--harness", "codex"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let conn = rusqlite::Connection::open(home.join(".privateer/privateer.sqlite")).unwrap();
        conn.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey-task','survey','survey','survey','open',0)", []).unwrap();
        conn.execute(
            "INSERT INTO jobs(id,record) VALUES(?1,?2)",
            rusqlite::params![
                job,
                json!({"id":job,"task_id":"survey-task","status":"queued","created_ms":0})
                    .to_string()
            ],
        )
        .unwrap();
    }
    let replace = |peer: &str| {
        fleet.ok(&[
            "machine",
            "add",
            "beta",
            "fixture",
            "--executable",
            BIN,
            "--remote-fleet",
            fleet.root.join(peer).join("fleet.sqlite").to_str().unwrap(),
            "--as",
            "alpha",
        ]);
    };
    replace("beta");
    let registry_path = fleet.root.join("state/fleet.sqlite");
    let registry = rusqlite::Connection::open(&registry_path).unwrap();
    let mode = fleet.root.join("link-mode");
    let calls = fleet.root.join("link-calls");
    fs::write(&mode, "live").unwrap();
    fs::write(&ssh, format!(r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
r = json.loads(sys.stdin.readline())
observing = r['command'] in ('fleet.runtime.v1', 'fleet.snapshot')
mode = pathlib.Path({mode:?}).read_text() if observing else 'live'
if observing:
 with open({calls:?}, 'a') as f: f.write(r['command']+'\n')
if mode == 'down': sys.exit(73)
p = subprocess.run(['/bin/sh','-c',sys.argv[-1]], input=json.dumps(r)+'\n', text=True, capture_output=True)
if p.returncode: sys.exit(p.returncode)
if mode == 'replace':
 # The OLD peer has answered; keep its bytes held while a real machine.add
 # replaces the named link, including its directory, in another subprocess.
 pathlib.Path({mode:?}).write_text('down')
 q = subprocess.run([{binary:?}, '--home', {home:?}, 'machine', 'add', 'beta', 'fixture', '--executable', {binary:?}, '--remote-fleet', {gamma:?}, '--as', 'alpha'], capture_output=True, text=True)
 if q.returncode: sys.stderr.write(q.stderr); sys.exit(q.returncode)
print(p.stdout, end='')
"#, mode=mode.to_str().unwrap(), calls=calls.to_str().unwrap(), binary=BIN,
        home=fleet.home("alpha").to_str().unwrap(), gamma=fleet.root.join("gamma/fleet.sqlite").to_str().unwrap())).unwrap();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "survey", "fleet:read", Some("survey"));
    let supervisor = fleet.supervise();
    fleet.wait("link incarnation hello", |e| e.first().cloned());
    let args = json!({"schema":"runtime.v1", "ship":"survey"});
    let read = |command: &str| delegated_ok(&fleet, &token, command, args.clone());
    let native = || fleet.call("alpha", "fleet.snapshot", json!({}));
    let assert_new = |v: &Value| {
        assert!(!v.to_string().contains("alpha-job"), "old peer facts: {v}");
        assert!(
            v.to_string().contains("gamma-job"),
            "new peer facts missing: {v}"
        );
    };
    let first = read("status");
    assert!(first.to_string().contains("alpha-job"), "{first}");
    let first_native = native();
    assert!(
        first_native.to_string().contains("alpha-job"),
        "{first_native}"
    );
    replace("gamma");
    // The same grant, selector, machine name and ship name must not reuse A.
    assert_new(&read("ships"));
    let new_native = native();
    assert_new(&new_native);
    let old_row = first_native["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "beta")
        .unwrap();
    let new_row = new_native["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "beta")
        .unwrap();
    assert_ne!(new_row["incarnation"], old_row["incarnation"]);
    let conditional = fleet.call("alpha", "fleet.snapshot", json!({"since":{"beta":{"incarnation":old_row["incarnation"],"changed_ms":new_row["changed_ms"]}}}));
    assert_new(&conditional);
    // A recent legacy row cannot become either native reuse or runtime fallback.
    registry
        .execute(
            "UPDATE fleet_snapshots SET incarnation=NULL WHERE machine='beta'",
            [],
        )
        .unwrap();
    fs::write(&mode, "down").unwrap();
    let down = native();
    assert!(!down.to_string().contains("alpha-job"));
    assert!(!down.to_string().contains("gamma-job"));

    // A-B-A-B: the original cache must not revive even on the original route.
    replace("beta");
    let failed = read("status");
    assert!(!failed.to_string().contains("alpha-job"), "{failed}");
    assert!(!failed.to_string().contains("gamma-job"), "{failed}");
    fs::write(&mode, "replace").unwrap();
    let held_runtime = read("ships");
    assert!(
        !held_runtime.to_string().contains("alpha-job"),
        "{held_runtime}"
    );
    // The held reply may not seed B's cache, including its failed-contact path.
    let after_held = read("status");
    assert!(
        !after_held.to_string().contains("alpha-job"),
        "{after_held}"
    );
    fs::write(&mode, "live").unwrap();
    assert_new(&read("status"));

    replace("beta");
    // No stored section for this new incarnation, so this necessarily contacts
    // A; its wrapper replaces the link with B before returning A's answer.
    fs::write(&mode, "replace").unwrap();
    let held_native = native();
    assert!(
        !held_native.to_string().contains("alpha-job"),
        "{held_native}"
    );
    let after_held = native();
    assert!(
        !after_held.to_string().contains("alpha-job"),
        "{after_held}"
    );
    let kept: String = registry
        .query_row(
            "SELECT body FROM fleet_snapshots WHERE machine='beta'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !kept.contains("alpha-job"),
        "late native response was stored"
    );
    fs::write(&mode, "live").unwrap();
    assert_new(&native());
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .contains("fleet.runtime.v1")
    );
    stop(supervisor);
}
