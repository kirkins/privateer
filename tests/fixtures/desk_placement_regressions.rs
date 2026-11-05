#[test]
fn conflicting_proposal_does_not_block_collect_or_drop_ship_mail() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    // Model two independently staged proposals, conflicting on both directions.
    let mut rival = b.row();
    rival["proposal"]["proposal_id"] = json!(uuid::Uuid::new_v4().to_string());
    b.save_row(&rival);
    let guard = b.registry().with_extension("desk-placement-owner.json");
    let mut g: Value = serde_json::from_slice(&fs::read(&guard).unwrap()).unwrap();
    g["proposal"] = rival["proposal"].clone();
    fs::write(guard, g.to_string()).unwrap();
    for id in ["during-conflict", "conflict-retry"] {
        b.queued_mail(id, "survey");
        a.collect();
        a.assert_mail(id);
    }
    for m in [&a, &b] {
        assert!(m.audit_count("desk.placement.exchange_failed") > 0);
        assert!(m.row()["certificate"].is_null());
    }
}

#[test]
fn active_replay_and_ship_mail_survive_ship_create_after_commit() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    a.accept();
    a.status();
    let cert = a.row()["certificate"].clone();
    for (m, name) in [(&a, "soundings"), (&b, "beacon")] {
        m.ok("ship.create", json!({"name":name,"harness":"codex","folder":m.root.join(name)}));
    }
    b.queued_mail("after-create", "survey");
    a.collect();
    a.assert_mail("after-create");
    assert_eq!(a.row()["certificate"], cert);
    assert_eq!(b.row()["certificate"], cert);
    assert_eq!(a.audit_count("desk.placement.exchange_failed"), 0);
    assert_eq!(b.audit_count("desk.placement.exchange_failed"), 0);
}

#[test]
fn malformed_records_are_isolated_and_frame_limits_do_not_stop_mail() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    let mut frame = a.gateway("desk.placement.status", json!({"machine":"beta"}))["result"]["desk_placement"].clone();
    // A bad first record must not prevent the later valid record converging.
    let mut accepted = frame["records"][0].clone();
    accepted["approval"] = json!(true);
    frame["records"] = json!([{"proposal":"malformed"}, accepted]);
    let response = b.gateway("fleet.collect", json!({"machine":"alpha","captain":false,"desk_placement":frame}));
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(b.row()["approval"], true);
    b.queued_mail("over-limit", "survey");
    frame["records"] = json!(vec![json!({}); 17]);
    let response = b.gateway("fleet.collect", json!({"machine":"alpha","captain":false,"desk_placement":frame}));
    assert_eq!(response["ok"], true, "{response}");
    assert!(response["result"]["messages"].as_array().unwrap().iter().any(|m| m["id"] == "over-limit"));
    a.collect();
    a.assert_mail("over-limit");
    assert!(b.audit_count("desk.placement.exchange_failed") > 0);
}

#[test]
fn ship_payload_is_reserved_and_copied_cards_cannot_approve() {
    let t = tempfile::tempdir().unwrap();
    let a = Machine::new(t.path().join("alpha"), "survey");
    proposal(&a, "here", &["here"]);
    let p = a.row()["proposal"].clone();
    let binding = json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]});
    a.fails("mail.send", json!({"id":"forged","from":"survey","to":"captain","kind":"decision","body":"Misleading decision","payload":{"desk_placement":binding,"decision":{"question":"Approve unrelated work?","options":[{"id":"accept","label":"Yes"}]}}}), "payload.desk_placement is set by the runtime");
    // Defense in depth for runtime-imported copies with the right binding but
    // a different card ID. Import is not the ship-authoring path above.
    let store = Connection::open(a.root.join("survey/.privateer/privateer.sqlite")).unwrap();
    store.execute_batch(&format!(
        "CREATE TEMP TABLE copy AS SELECT * FROM mail WHERE id='{}';
         UPDATE copy SET seq=NULL, id='copied-card', body='Unrelated work', state='open';
         INSERT INTO mail SELECT * FROM copy;",
        p["decision"].as_str().unwrap()
    )).unwrap();
    drop(store);
    assert_eq!(a.ok("mail.show", json!({"id":"copied-card"}))["payload"]["desk_placement"], binding);
    a.ok("captain.reply", json!({"id":"copied-card","body":"Yes to unrelated work","payload":{"answer":{"option_id":"accept"}}}));
    assert!(a.row()["approval"].is_null());
    assert!(a.row()["certificate"].is_null());
    a.ok("captain.reply", json!({"id":p["decision"],"body":"Place this desk","payload":{"answer":{"option_id":"accept"}}}));
    assert_eq!(a.row()["state"], "active");
}

#[test]
fn explicit_reject_recovers_a_lost_card_and_aborted_accept_is_not_audited() {
    let t = tempfile::tempdir().unwrap();
    let a = Machine::new(t.path().join("alpha"), "survey");
    proposal(&a, "here", &["here"]);
    let p = a.row()["proposal"].clone();
    Connection::open(a.root.join("survey/.privateer/privateer.sqlite")).unwrap().execute("DELETE FROM mail WHERE id=?1", [p["decision"].as_str().unwrap()]).unwrap();
    a.fails("machine.desk.reject", json!({"proposal_id":p["proposal_id"],"manifest_hash":"wrong"}), "manifest_hash mismatch");
    let binding = json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]});
    a.ok("machine.desk.reject", binding.clone());
    assert_eq!(a.row()["state"], "aborted");
    assert!(!a.registry().with_extension("desk-placement-owner.json").exists());
    let before = a.audit_count("desk.placement.approved");
    a.fails("machine.desk.accept", binding.clone(), "aborted proposal");
    a.ok("machine.desk.reject", binding);
    assert_eq!(a.audit_count("desk.placement.approved"), before);
    proposal(&a, "here", &["here"]);
    a.accept();
    let p = a.row()["proposal"].clone();
    a.fails("machine.desk.reject", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}), "already certified");
}

#[test]
fn aborted_replay_cleans_interrupted_guard_even_after_membership_changes() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    let pending_guard = fs::read(b.registry().with_extension("desk-placement-owner.json")).unwrap();
    let p = b.row()["proposal"].clone();
    b.ok("machine.desk.reject", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
    // Crash boundary: aborted row durable, guard removal not yet durable.
    fs::write(b.registry().with_extension("desk-placement-owner.json"), pending_guard).unwrap();
    b.ok("ship.create", json!({"name":"beacon","harness":"codex","folder":b.root.join("beacon")}));
    let response = b.gateway("desk.placement.accept", json!({"machine":"alpha","desk_placement":a.gateway("desk.placement.status", json!({"machine":"beta"}))["result"]["desk_placement"]}));
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(b.row()["state"], "aborted");
    assert!(!b.registry().with_extension("desk-placement-owner.json").exists());
    assert_eq!(b.status()["restore_guard"], false);
    assert!(b.run(&["afk", "out"]).status.success());
    assert!(b.run(&["back"]).status.success());
}

#[test]
fn duplicate_topology_names_are_rejected_before_staging_and_card_names_owner() {
    let (t, a, b) = pair();
    let g = Machine::new(t.path().join("gamma"), "lookout");
    g.link("gamma", &b, "beta");
    a.status();
    let conn = Connection::open(a.registry()).unwrap();
    let description: String = conn.query_row("SELECT description FROM desk_placement_topology WHERE json_extract(description,'$.name')='gamma'", [], |r| r.get(0)).unwrap();
    let mut description: Value = serde_json::from_str(&description).unwrap();
    description["name"] = json!("beta");
    conn.execute("UPDATE desk_placement_topology SET description=?1 WHERE core=?2", rusqlite::params![description.to_string(),description["core"].as_str().unwrap()]).unwrap();
    // Prevent refresh overwriting the deliberately ambiguous cache.
    conn.execute("UPDATE machines SET role='collected'", []).unwrap();
    a.fails("machine.desk.propose", json!({"domain":"survey-domain","owner":"beta","sources":["alpha","beta"]}), "duplicate machine name");
    assert!(a.status()["records"].as_array().unwrap().is_empty());
    drop(conn);
    let (_t, a, _b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    let p = a.row()["proposal"].clone();
    let card = a.ok("mail.show", json!({"id":p["decision"]}));
    assert_eq!(card["payload"]["decision"]["question"], "Place desk domain survey-domain at beta?");
}

#[test]
fn reject_releases_a_pending_proposal_after_ship_create() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    // Ship creation while staged breaks the bootstrap inventory, so the
    // proposal can never certify; rejection must still release both fences.
    for (m, name) in [(&a, "soundings"), (&b, "beacon")] {
        m.ok("ship.create", json!({"name":name,"harness":"codex","folder":m.root.join(name)}));
    }
    let p = a.row()["proposal"].clone();
    a.ok("machine.desk.reject", json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
    b.queued_mail("after-reject", "survey");
    a.collect();
    a.assert_mail("after-reject");
    for m in [&a, &b] {
        assert_eq!(m.row()["state"], "aborted");
        assert_eq!(m.status()["restore_guard"], false);
        assert!(m.run(&["afk", "out"]).status.success());
        assert!(m.run(&["back"]).status.success());
    }
}

/// Give a machine `count` extra ships in its registry. Placement descriptions
/// and manifests carry ship names only, so a test can model a large fleet
/// without creating that many ship homes.
fn add_ships(m: &Machine, prefix: &str, count: usize) {
    let conn = Connection::open(m.registry()).unwrap();
    for i in 0..count {
        let name = format!("{prefix}-{i:02}");
        conn.execute(
            "INSERT INTO ships(name,home,created_by,created_ms) VALUES(?1,?2,'captain',0)",
            rusqlite::params![name, m.root.join(&name).to_string_lossy()],
        )
        .unwrap();
    }
}

/// A machine name within the link's 64-character peer-name bound, long enough
/// that six sources force the bounded card text to truncate.
fn long_machine_name(prefix: &str) -> String {
    format!("{prefix}{}", "x".repeat(60 - prefix.len()))
}

#[test]
fn a_large_manifest_still_writes_the_decision_card() {
    let t = tempfile::tempdir().unwrap();
    let names: Vec<String> = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
        .iter()
        .map(|prefix| long_machine_name(prefix))
        .collect();
    let ships = ["survey", "relay", "lookout", "beacon", "harbor", "watch"];
    let mut machines = vec![];
    for ship in ships {
        machines.push(Machine::new(t.path().join(ship), ship));
    }
    for i in 1..machines.len() {
        machines[0].link(&names[0], &machines[i], &names[i]);
    }
    // Six sources of about twenty ships each plus long names make the old
    // unbounded generation-one `why` (the whole source JSON) exceed 500
    // characters by far, and the bounded summary still has to truncate.
    for (i, m) in machines.iter().enumerate() {
        add_ships(m, &format!("m{i}"), 20);
    }
    let a = &machines[0];
    let sources: Vec<&str> = names.iter().map(String::as_str).collect();
    proposal(a, &names[0], &sources);
    let p = a.row()["proposal"].clone();
    let manifest_sources = p["manifest"]["sources"].as_array().unwrap();
    assert_eq!(manifest_sources.len(), 6);
    for source in manifest_sources {
        assert_eq!(source["ships"].as_array().unwrap().len(), 21, "{source}");
    }
    let card = a.ok("mail.show", json!({"id":p["decision"]}));
    assert_eq!(card["kind"], "decision");
    assert_eq!(card["state"], "open");
    let why = card["payload"]["decision"]["why"].as_str().unwrap();
    assert_eq!(why.chars().count(), 500, "why: {why}");
    assert!(
        why.ends_with('…'),
        "truncated why must end on a whole character: {why}"
    );
    assert!(why.contains(p["manifest_hash"].as_str().unwrap()), "{why}");
    assert!(why.contains("6 sources"), "{why}");
    // The full source facts stay readable on the card body and the record.
    let body = card["body"].as_str().unwrap();
    assert!(body.contains(p["manifest_hash"].as_str().unwrap()), "{body}");
    for name in &names {
        assert!(body.contains(name.as_str()), "{body}");
    }
    assert_eq!(a.status()["records"].as_array().unwrap().len(), 1);
}

#[test]
fn a_refused_decision_card_leaves_no_pending_record_or_guard() {
    let (_t, a, _b) = pair();
    let store = Connection::open(a.root.join("survey/.privateer/privateer.sqlite")).unwrap();
    store
        .execute_batch(
            "CREATE TRIGGER refuse_decision BEFORE INSERT ON mail
             BEGIN SELECT RAISE(ABORT, 'forced decision refusal'); END;",
        )
        .unwrap();
    drop(store);
    a.fails(
        "machine.desk.propose",
        json!({"domain":"survey-domain","owner":"beta","sources":["alpha","beta"]}),
        "forced decision refusal",
    );
    let status = a.status();
    assert!(status["records"].as_array().unwrap().is_empty(), "{status}");
    assert_eq!(status["restore_guard"], false, "{status}");
    assert!(!a.registry().with_extension("desk-placement-owner.json").exists());
    assert_eq!(a.audit_count("desk.placement.proposed"), 0);
    // Stable home paths only: drop the injected failure and stage normally.
    let store = Connection::open(a.root.join("survey/.privateer/privateer.sqlite")).unwrap();
    store.execute_batch("DROP TRIGGER refuse_decision;").unwrap();
    drop(store);
    proposal(&a, "beta", &["alpha", "beta"]);
    let p = a.row()["proposal"].clone();
    let card = a.ok("mail.show", json!({"id":p["decision"]}));
    assert_eq!(card["kind"], "decision");
    assert_eq!(a.status()["restore_guard"], false);
}
