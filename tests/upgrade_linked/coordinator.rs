//! Operator coordination must not move the mail desk or weaken admission.
use super::*;

fn move_desk(machine: &Machine) {
    let out = machine
        .command(&["machine", "captain", "peer"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
}

fn routing(machine: &Machine) -> (String, Vec<(String, String, String)>) {
    let conn = rusqlite::Connection::open(machine.root.join("fleet.sqlite")).unwrap();
    let desk = conn
        .query_row("SELECT machine FROM captain_desk", [], |r| r.get(0))
        .unwrap();
    let mut query = conn
        .prepare("SELECT name,role,peer FROM machines ORDER BY name")
        .unwrap();
    let links = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    (desk, links)
}

fn from_here(machine: &Machine, dry_run: bool) -> Output {
    let mut command = machine.command(&["upgrade", B, "--from-here"]);
    if dry_run {
        command.arg("--dry-run");
    }
    command.output().unwrap()
}

#[test]
fn explicit_operator_coordinates_three_machines_without_moving_the_mail_desk() {
    let (_dir, coordinator, remote) = fixture(true);
    let third = Machine::new(coordinator.root.parent().unwrap().join("other"), true);
    coordinator.link(&third, "other");
    let desk_plan = coordinator.json("upgrade.plan", json!({"commit":B}), false);
    move_desk(&coordinator);
    let before = routing(&coordinator);
    let denied = coordinator
        .command(&["upgrade", B, "--dry-run"])
        .output()
        .unwrap();
    assert!(!denied.status.success(), "{}", text(&denied));
    assert!(text(&denied).contains("--from-here"));
    let agent_plan = coordinator.rpc("upgrade.plan", json!({"commit":B}), true);
    assert!(!agent_plan.status.success(), "{}", text(&agent_plan));
    assert!(text(&agent_plan).contains("captain's desk"));
    let dry = from_here(&coordinator, true);
    assert!(dry.status.success(), "{}", text(&dry));
    let dry: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry["machines"], json!(["other", "peer", "local"]));
    assert_eq!(dry["scope"], "linked-from-here");
    assert_ne!(dry["environment"], desk_plan["environment"]);
    assert!(
        dry["environment"]
            .as_str()
            .unwrap()
            .starts_with("upgrade-from-here:")
    );
    coordinator.assert_nothing_prepared();
    for machine in [&remote, &third] {
        assert!(
            machine
                .calls()
                .iter()
                .all(|c| !c["command"].as_str().unwrap().starts_with("upgrade."))
        );
    }
    assert_eq!(routing(&coordinator), before);

    // Use the prepared target as controller while current still selects A.
    // An older installed CLI need not understand --from-here to bootstrap.
    let out = coordinator
        .command_at(
            &coordinator
                .data()
                .join("releases")
                .join(B)
                .join("bin/privateer"),
            &["upgrade", B, "--from-here"],
        )
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let round: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["scope"], "linked-from-here");
    assert_eq!(round["environment"], dry["environment"]);
    assert_eq!(routing(&coordinator), before);
    for machine in [&coordinator, &remote, &third] {
        assert_eq!(machine.selected(), B);
    }
    for machine in [&remote, &third] {
        let calls = machine.calls();
        let apply = calls
            .iter()
            .find(|c| c["command"] == "upgrade.apply")
            .unwrap();
        assert_eq!(apply["desk_ready"], true, "{calls:?}");
        assert_eq!(apply["remote_ready"], true);
        assert_eq!(apply["desk_current"], format!("releases/{A}"));
    }
}

#[test]
fn explicit_coordinator_keeps_reverse_links_and_rejects_agents_and_local_mix() {
    let (_dir, coordinator, remote) = fixture(true);
    let reverse = from_here(&remote, true);
    assert!(!reverse.status.success(), "{}", text(&reverse));
    assert!(text(&reverse).contains("reverse-only link"));
    for extra in ["--local", "--unknown"] {
        let out = coordinator
            .command(&["upgrade", B, "--from-here", extra, "--dry-run"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", text(&out));
    }
    let agent = coordinator
        .command(&["upgrade", B, "--from-here", "--dry-run"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "requester")
        .output()
        .unwrap();
    assert!(!agent.status.success(), "{}", text(&agent));
    for verb in ["upgrade.plan", "upgrade.start"] {
        let out = coordinator.rpc(verb, json!({"commit":B,"scope":"linked-from-here"}), true);
        assert!(!out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("scope must be linked or local"));
    }
    coordinator.assert_nothing_prepared();
    remote.assert_nothing_prepared();
    let conn = rusqlite::Connection::open(remote.root.join("fleet.sqlite")).unwrap();
    let role: String = conn
        .query_row("SELECT role FROM machines WHERE name='desk'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(role, "collected");
}

#[test]
fn explicit_coordinator_refuses_pending_and_invalid_placement_fences() {
    let (_dir, coordinator, _remote) = fixture(true);
    // Placement decisions require a fleet-registered home; the upgrade
    // fixture normally initializes only the store used by --home.
    coordinator.request_setup();
    let registry = rusqlite::Connection::open(coordinator.root.join("fleet.sqlite")).unwrap();
    registry
        .execute(
            "INSERT INTO ships(name,home,created_by,created_ms) VALUES('requester',?1,'fixture',0)",
            [coordinator.root.join("home").to_str().unwrap()],
        )
        .unwrap();
    coordinator.json(
        "machine.desk.propose",
        json!({"domain":"survey-domain","owner":"peer","sources":["desk","peer"]}),
        false,
    );
    let pending = from_here(&coordinator, true);
    assert!(!pending.status.success(), "{}", text(&pending));
    assert!(text(&pending).contains("pending placement"));
    fs::write(
        coordinator
            .root
            .join("fleet.sqlite")
            .with_extension("desk-placement-owner.json"),
        b"{",
    )
    .unwrap();
    let invalid = from_here(&coordinator, true);
    assert!(!invalid.status.success(), "{}", text(&invalid));
    coordinator.assert_nothing_prepared();
}

#[test]
fn explicit_coordinator_preparation_failure_switches_nothing() {
    let (_dir, coordinator, remote) = fixture(true);
    move_desk(&coordinator);
    fs::write(
        remote
            .data()
            .join("releases")
            .join(B)
            .join("bin/privateer-dashboard"),
        b"corrupt",
    )
    .unwrap();
    let out = from_here(&coordinator, false);
    assert!(!out.status.success(), "{}", text(&out));
    assert_eq!(coordinator.selected(), A);
    assert_eq!(remote.selected(), A);
    assert!(
        remote
            .calls()
            .iter()
            .all(|c| c["command"] != "upgrade.apply")
    );
}

#[test]
fn explicit_coordinator_rejects_a_retargeted_link_during_prepare() {
    let (_dir, coordinator, remote) = fixture(true);
    move_desk(&coordinator);
    let gateway = remote.root.join("gateway");
    let script = fs::read_to_string(&gateway).unwrap().replace("    sys.stdout.write(result.stdout)", r#"    if command=='upgrade.prepare':
        import sqlite3
        with sqlite3.connect(desk/'fleet.sqlite') as db:
            db.execute("UPDATE machines SET peer=json_set(peer,'$.executable',?) WHERE name='peer'", (str(root/'retargeted-gateway'),))
    sys.stdout.write(result.stdout)"#);
    fs::write(&gateway, script).unwrap();
    let marker = remote.root.join("retarget-used");
    executable(
        &remote.root.join("retargeted-gateway"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    let out = from_here(&coordinator, false);
    assert!(!out.status.success(), "{}", text(&out));
    let round: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        round["error"]
            .as_str()
            .unwrap()
            .contains("approval-bound upgrade route changed"),
        "{round}"
    );
    assert!(!marker.exists());
    assert_eq!(coordinator.selected(), A);
    assert_eq!(remote.selected(), A);
    remote.wait("prepare", round["machines"][0]["prepare"].clone());
    assert!(
        remote
            .calls()
            .iter()
            .all(|c| c["command"] != "upgrade.apply")
    );
}

#[test]
fn queued_agent_request_cannot_select_operator_coordination() {
    let (_dir, coordinator, remote) = fixture(true);
    coordinator.request_setup();
    let plan = coordinator.json("upgrade.plan", json!({"commit":B}), true);
    coordinator.approve("desk-only", B, &plan["environment"]);
    let out = coordinator.request(Some("desk-only"));
    assert!(out.status.success(), "{}", text(&out));
    let mut request: Value = serde_json::from_slice(&out.stdout).unwrap();
    request["scope"] = json!("linked-from-here");
    coordinator
        .database()
        .execute(
            "UPDATE metadata SET value=?2 WHERE key=?1",
            rusqlite::params![
                format!("upgrade-request:{}", request["id"].as_str().unwrap()),
                request.to_string()
            ],
        )
        .unwrap();
    // A rejection before spawn has no round file. Its durable outcome reply
    // is authoritative even after dispatcher recovery removes the request.
    coordinator.json("serve", json!({"once":true}), false);
    let round = coordinator.outcome_reply(&request);
    assert_eq!(round["outcome"], "failed", "{round}");
    assert!(
        round["error"]
            .as_str()
            .unwrap()
            .contains("scope must be linked or local"),
        "{round}"
    );
    coordinator.assert_nothing_prepared();
    remote.assert_nothing_prepared();
}
