fn desk_exposure_args() -> Value {
    json!({"door":"fake","domain":"beta-domain","mode":"fresh_replay","replay_policy":"open_and_new"})
}
fn desk_exposure_owner(fleet: &Fleet) {
    let staged = fleet.call(
        "alpha",
        "machine.desk.propose",
        json!({"domain":"beta-domain","owner":"here","sources":["here"]}),
    );
    let p = &staged["records"][0]["proposal"];
    fleet.call(
        "alpha",
        "machine.desk.accept",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
    );
}
fn desk_exposure_failure(fleet: &Fleet, command: &str, args: Value, code: &str) {
    let out = fleet.run_in(
        &fleet.root.join("ships/alpha"),
        &[command, "--json", &args.to_string()],
    );
    assert!(!out.status.success(), "{command} unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
// Fixture-only mutation models a future certificate/restore, without adding a
// production placement-update command or assuming generation 2 is supported.
fn desk_exposure_change_manifest(fleet: &Fleet, mutate: impl FnOnce(&mut Value)) {
    use sha2::{Digest, Sha256};
    let registry = fleet.root.join("state/fleet.sqlite");
    let conn = rusqlite::Connection::open(&registry).unwrap();
    let raw: String = conn
        .query_row("SELECT record FROM desk_placement_record", [], |r| r.get(0))
        .unwrap();
    let mut record: Value = serde_json::from_str(&raw).unwrap();
    mutate(&mut record["proposal"]["manifest"]);
    record["proposal"]["manifest_hash"] = json!(format!(
        "{:x}",
        Sha256::digest(record["proposal"]["manifest"].to_string().as_bytes())
    ));
    record["certificate"]["proposal"] = record["proposal"].clone();
    let guard = json!({"proposal":record["proposal"],"certificate":record["certificate"]});
    fs::write(
        registry.with_extension("desk-placement-owner.json"),
        guard.to_string(),
    )
    .unwrap();
    conn.execute(
        "UPDATE desk_placement_record SET record=?1",
        [record.to_string()],
    )
    .unwrap();
}

#[test]
fn desk_opt_in_is_operator_only_explicit_durable_and_unexposed() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    delegated_adapter(&fleet);
    let schema = || {
        let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
        conn.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = schema();
    let status = fleet.call("alpha", "door.desk.status", json!({"door":"fake"}));
    assert_eq!(status["readiness_code"], "no_placement");
    assert_eq!(status["state"], "no placement");
    assert_eq!(
        before,
        schema(),
        "never-enabled status must not initialize schema"
    );
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        desk_exposure_args(),
        "no_placement",
    );
    assert_eq!(
        before,
        schema(),
        "failed enable must not initialize exposure state"
    );
    for command in ["door.desk.enable", "door.desk.status", "door.desk.disable"] {
        let out = fleet
            .command(&fleet.root.join("ships/alpha"))
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "alpha")
            .args([command, "--json", &desk_exposure_args().to_string()])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("operator command"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    desk_exposure_owner(&fleet);
    for args in [
        json!({"door":"fake","domain":"beta-domain","mode":"fresh_replay"}),
        json!({"door":"fake","domain":"beta-domain","mode":"fresh_replay","replay_policy":"all_history"}),
    ] {
        desk_exposure_failure(&fleet, "door.desk.enable", args, "replay_policy");
    }
    let enabled = fleet.call("alpha", "door.desk.enable", desk_exposure_args());
    assert_eq!(enabled["state"], "enabled at generation 1");
    assert_eq!(enabled["readiness_code"], Value::Null);
    assert_eq!(enabled["binding"]["replay_policy"], "open_and_new");
    assert_eq!(
        enabled["binding"]["switch_on"][0]["state"],
        "switch_on_pending"
    );
    assert_eq!(enabled["slots_used"], 0);
    assert_eq!(enabled["slots_free"], 16);
    assert_eq!(enabled["client_exposed"], false);
    let again = fleet.call("alpha", "door.desk.enable", desk_exposure_args());
    assert_eq!(
        enabled["binding"], again["binding"],
        "identical enable must retain its boundary across core restart"
    );
    let disabled = fleet.call("alpha", "door.desk.disable", json!({"door":"fake"}));
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["binding"], enabled["binding"]);
    let resumed: Value = serde_json::from_str(&fleet.ok(&[
        "door",
        "desk",
        "enable",
        "fake",
        "--domain",
        "beta-domain",
        "--mode",
        "fresh_replay",
        "--replay-policy",
        "open_and_new",
    ]))
    .unwrap();
    assert_eq!(resumed["binding"], enabled["binding"]);
    assert_eq!(resumed["enabled"], true);
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        json!({"door":"fake","domain":"gamma-domain","mode":"fresh_replay","replay_policy":"open_and_new"}),
        "domain_mismatch",
    );
    desk_exposure_failure(
        &fleet,
        "door.desk.status",
        json!({"door":"fake","owner":"here"}),
        "unknown argument",
    );
    let supervisor = fleet.supervise();
    fleet.wait("unexposed scoped hello", |e| e.first().cloned());
    // Neither trusted nor scoped adapter requests acquire operator opt-in or
    // subscription entry points. No new grant scopes or capabilities exist.
    let (_, token) = delegated_issue(&fleet, "beta", "mail:read", None);
    for command in [
        "door.desk.enable",
        "door.desk.status",
        "door.desk.disable",
        "desk.subscription.open",
    ] {
        assert_eq!(
            delegated_exchange(&fleet, &token, command, json!({}))["ok"],
            false
        );
    }
    stop(supervisor);
}

#[test]
fn desk_opt_in_manifest_change_stops_readiness_and_refuses_rebinding_without_losing_slots() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    delegated_adapter(&fleet);
    desk_exposure_owner(&fleet);
    let original = fleet.call("alpha", "door.desk.enable", desk_exposure_args());
    // Synthetic engine custody backed by a real grant: operator commands must
    // not retire it or reset progress. Engine tests own admission and retirement.
    let (grant, _) = delegated_issue(&fleet, "beta", "mail:read", None);
    let grant_id = grant["clients"][0]["id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    conn.execute("INSERT INTO desk_subscription VALUES('held','beta-domain',?1,'{}','inc','visibility')",
        [json!(["core","fake","door:fake",grant_id]).to_string()]).unwrap();
    desk_exposure_change_manifest(&fleet, |m| m["max_source_age_ms"] = json!(800000));
    for revision in [1, 2] {
        if revision == 2 {
            desk_exposure_change_manifest(&fleet, |m| m["manifest_revision"] = json!(2));
        }
        let changed = fleet.call("alpha", "door.desk.status", json!({"door":"fake"}));
        // The first iteration starts enabled; the second follows the explicit
        // disable below. Neither manifest changes nor failed enables may flip it.
        assert_eq!(
            changed["enabled"],
            revision == 1,
            "retain the operator's last enable/disable, not authority to serve"
        );
        assert_eq!(changed["readiness_code"], "stale_generation");
        assert_eq!(changed["detail"], "manifest_changed");
        assert_eq!(changed["slots_used"], 1);
        assert_eq!(changed["binding"], original["binding"]);
        desk_exposure_failure(
            &fleet,
            "door.desk.enable",
            desk_exposure_args(),
            "stale_generation",
        );
        let disabled = fleet.call("alpha", "door.desk.disable", json!({"door":"fake"}));
        assert_eq!(disabled["enabled"], false);
        assert_eq!(disabled["binding"], original["binding"]);
        desk_exposure_failure(
            &fleet,
            "door.desk.enable",
            desk_exposure_args(),
            "stale_generation",
        );
        assert_eq!(
            fleet.call("alpha", "door.desk.status", json!({"door":"fake"}))["enabled"],
            false
        );
    }
    client_call(&fleet, "revoke", grant_id);
    let revoked = fleet.call("alpha", "door.desk.status", json!({"door":"fake"}));
    assert_eq!(revoked["slots_used"], 0);
    assert_eq!(revoked["slots_free"], 16);
    assert_eq!(revoked["reclaimable_at_next_admission"], 1);
    let grants = revoked["grants"].as_array().unwrap();
    let revoked_grant = grants.iter().find(|g| g["id"] == grant_id).unwrap();
    assert_eq!(revoked_grant["slots_held"], 0);
    assert_eq!(revoked_grant["lifecycle"], "revoked");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM desk_subscription WHERE id='held'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn desk_opt_in_refuses_stale_pending_and_restored_placements() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    delegated_adapter(&fleet);
    let staged = fleet.call(
        "alpha",
        "machine.desk.propose",
        json!({"domain":"beta-domain","owner":"here","sources":["here"]}),
    );
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        desk_exposure_args(),
        "placement_pending",
    );
    let p = &staged["records"][0]["proposal"];
    fleet.call(
        "alpha",
        "machine.desk.accept",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
    );
    desk_exposure_change_manifest(&fleet, |m| m["generation"] = json!(2));
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        desk_exposure_args(),
        "stale_generation",
    );
    let registry = fleet.root.join("state/fleet.sqlite");
    fs::remove_file(registry.with_extension("desk-placement-owner.json")).unwrap();
    desk_exposure_failure(
        &fleet,
        "door.desk.enable",
        desk_exposure_args(),
        "restore_guard",
    );
}

#[test]
fn desk_opt_in_remote_and_reverse_only_owners_never_fall_back() {
    for reverse in [false, true] {
        let source = Fleet::with_ships(&[("alpha", true)]);
        let owner = Fleet::with_ships(&[("beta", true)]);
        delegated_adapter(&source);
        let (caller, ship, peer, other, known_as) = if reverse {
            (&owner, "beta", "alpha", &source, "beta")
        } else {
            (&source, "alpha", "beta", &owner, "alpha")
        };
        caller.call(ship,"machine.add",json!({"name":peer,"target":"local-test","executable":BIN,"as":known_as,"fleet":other.root.join("state/fleet.sqlite"),"transport":"local"}));
        let staged = caller.call(ship,"machine.desk.propose",json!({"domain":"beta-domain","owner":if reverse {"here"} else {"beta"},"sources":["alpha","beta"]}));
        let p = &staged["records"][0]["proposal"];
        for (f, s) in [(&source, "alpha"), (&owner, "beta")] {
            f.call(
                s,
                "machine.desk.accept",
                json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
            );
        }
        caller.call(ship, "machine.desk.status", json!({}));
        let status = source.call("alpha", "door.desk.status", json!({"door":"fake"}));
        assert_eq!(status["readiness_code"], "remote_owner", "{status}");
        assert_eq!(
            status["state"],
            if reverse {
                "reverse-only"
            } else {
                "remote owner"
            }
        );
        assert!(
            status["warning"]
                .as_str()
                .unwrap()
                .contains("unsupported read direction / incomplete feature")
        );
        desk_exposure_failure(
            &source,
            "door.desk.enable",
            desk_exposure_args(),
            "remote_owner",
        );
    }
}

#[test]
fn placement_card_dedupe_uses_ship_and_id_and_lasts_only_this_session() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    fleet.add();
    let staged = fleet.call(
        "alpha",
        "machine.desk.propose",
        json!({"domain":"beta-domain","owner":"here","sources":["here"]}),
    );
    let p = &staged["records"][0]["proposal"];
    fleet.call(
        "alpha",
        "machine.desk.accept",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
    );
    let id = p["decision"].as_str().unwrap();
    fleet.call("beta","mail.send",json!({"id":id,"from":"beta","to":"captain","kind":"notice","body":"different ship, same mail id"}));
    let supervisor = fleet.supervise();
    let card = fleet.wait("alpha placement card", |e| {
        e.iter().find(|v| v["control"] == "desk_placement").cloned()
    });
    assert_eq!(card["ship"], "alpha");
    let collision = fleet.wait("beta same-id ordinary mail", |e| {
        e.iter()
            .find(|v| v["ship"] == "beta" && v["mail"]["id"] == id)
            .cloned()
    });
    assert!(collision["control"].is_null());
    fleet.wait_delivered(&[collision]);
    stop(supervisor);
    let supervisor = fleet.supervise();
    fleet.wait("control is emitted again after session restart", |e| {
        events(e, 1, "mail")
            .iter()
            .find(|v| v["ship"] == "alpha" && v["control"] == "desk_placement")
            .cloned()
    });
    stop(supervisor);
}
