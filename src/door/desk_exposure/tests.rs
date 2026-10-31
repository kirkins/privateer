use super::*;

#[test]
fn placement_card_reference_is_an_unambiguous_ship_id_tuple() {
    let reference = super::super::placement_reference;
    assert_ne!(reference("alpha", "same"), reference("beta", "same"));
    assert_ne!(
        reference("alpha:beta", "gamma"),
        reference("alpha", "beta:gamma")
    );
    assert_eq!(reference("alpha", "same"), reference("alpha", "same"));
}

fn manifest() -> Manifest {
    serde_json::from_value(json!({"domain":"beta-domain","manifest_revision":1,
        "max_source_age_ms":900000,"owner_core":"00000000-0000-4000-8000-000000000001",
        "generation":1,"incarnation":"00000000-0000-4000-8000-000000000002",
        "coordinator":"00000000-0000-4000-8000-000000000001","sources":[]}))
    .unwrap()
}
fn fixture() -> (Connection, Manifest, Binding) {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE desk_placement_record(domain TEXT,record TEXT);
        CREATE TABLE door_clients(id TEXT,door TEXT,resource TEXT,scopes TEXT,ships TEXT,
        paused INTEGER,revoked_ms INTEGER,expires_ms INTEGER);",
    )
    .unwrap();
    super::super::subscription::install(&conn).unwrap();
    install(&conn).unwrap();
    let m = manifest();
    let b = Binding {
        domain: m.domain.clone(),
        owner_core: m.owner_core.clone(),
        generation: 1,
        incarnation: m.incarnation.clone(),
        manifest_revision: 1,
        manifest_hash: crate::core::hash(&serde_json::to_value(&m).unwrap()),
        mode: "fresh_replay".into(),
        replay_policy: "open_and_new".into(),
        switch_on_ms: 123,
        switch_on: vec![json!({"source":m.owner_core,"state":"switch_on_pending","through":null})],
    };
    conn.execute(
        "INSERT INTO door_desk_exposure VALUES('beta',1,?1)",
        [serde_json::to_string(&b).unwrap()],
    )
    .unwrap();
    (conn, m, b)
}
#[test]
fn capacity_counts_all_doors_and_keeps_paused_disabled_custody() {
    let (conn, m, _) = fixture();
    for (id, door, scopes, ships, paused, revoked, expires) in [
        (
            "reader",
            "beta",
            "[\"mail:read\"]",
            Some("[\"alpha\"]"),
            false,
            None,
            None,
        ),
        ("paused", "beta", "[\"mail:read\"]", None, true, None, None),
        (
            "revoked",
            "beta",
            "[\"mail:read\"]",
            None,
            false,
            Some(1),
            None,
        ),
        (
            "expired",
            "beta",
            "[\"mail:read\"]",
            None,
            false,
            None,
            Some(0),
        ),
        (
            "metadata",
            "beta",
            "[\"fleet:read\"]",
            None,
            false,
            None,
            None,
        ),
        ("other", "gamma", "[\"mail:read\"]", None, false, None, None),
    ] {
        conn.execute(
            "INSERT INTO door_clients VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                door,
                format!("door:{door}"),
                scopes,
                ships,
                paused,
                revoked,
                expires
            ],
        )
        .unwrap();
    }
    for i in 0..16 {
        let (door, grant) = if i == 0 {
            ("beta", "paused".to_string())
        } else {
            ("gamma", format!("other-{i}"))
        };
        conn.execute(
            "INSERT INTO desk_subscription VALUES(?1,?2,?3,'{}','inc','vis')",
            params![
                format!("sub-{i}"),
                m.domain,
                json!([m.owner_core, door, format!("door:{door}"), grant]).to_string()
            ],
        )
        .unwrap();
    }
    let before = conn.total_changes();
    let s = status(&conn, "beta", Ok(m.clone())).unwrap();
    assert_eq!(
        conn.total_changes(),
        before,
        "status must not retire or modify readers"
    );
    assert_eq!(s["slots_used"], 16);
    assert_eq!(s["slots_free"], 0);
    assert_eq!(s["slots_limit"], 16);
    let grants = s["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 5);
    assert_eq!(grants.iter().filter(|g| g["eligible"] == true).count(), 1);
    let reader = grants.iter().find(|g| g["id"] == "reader").unwrap();
    assert_eq!(reader["ships"], json!(["alpha"]));
    let paused = grants.iter().find(|g| g["id"] == "paused").unwrap();
    assert_eq!(paused["lifecycle"], "paused");
    assert_eq!(paused["slots_held"], 1);
    assert!(
        s["slot_release"]
            .as_str()
            .unwrap()
            .contains("Revoking the owning grant is the only supported operator action")
    );
    conn.execute("UPDATE door_desk_exposure SET enabled=0", [])
        .unwrap();
    let disabled = status(&conn, "beta", Ok(m)).unwrap();
    assert_eq!(disabled["state"], "disabled");
    assert_eq!(disabled["slots_used"], 16);
    assert_eq!(disabled["binding"], s["binding"]);
    assert!(!disabled.to_string().contains("token_hash"));
}
#[test]
fn binding_is_exact_and_manifest_change_never_resets_boundary() {
    let (conn, mut m, b) = fixture();
    assert!(b.matches(&m).unwrap());
    m.max_source_age_ms += 1;
    assert!(!b.matches(&m).unwrap());
    assert!(b.manifest_changed(&m).unwrap());
    let s = status(&conn, "beta", Ok(m.clone())).unwrap();
    assert_eq!(s["readiness_code"], "stale_generation");
    assert_eq!(s["detail"], "manifest_changed");
    assert_eq!(s["binding"]["switch_on_ms"], 123);
    m.generation = 2;
    assert!(!b.manifest_changed(&m).unwrap());
    assert_eq!(
        status(&conn, "beta", Ok(m)).unwrap()["readiness_code"],
        "stale_generation"
    );
}
#[test]
fn unconfigured_diagnoses_are_honest_and_never_create_schema() {
    let conn = Connection::open_in_memory().unwrap();
    for (refusal, state, code) in [
        (ExposureRefusal::NoPlacement, "no placement", "no_placement"),
        (
            ExposureRefusal::RemoteOwner {
                reverse_only: false,
            },
            "remote owner",
            "remote_owner",
        ),
        (
            ExposureRefusal::RemoteOwner { reverse_only: true },
            "reverse-only",
            "remote_owner",
        ),
        (
            ExposureRefusal::StaleGeneration,
            "unavailable",
            "stale_generation",
        ),
    ] {
        let s = status(&conn, "beta", Err(refusal)).unwrap();
        assert_eq!(s["state"], state);
        assert_eq!(s["readiness_code"], code);
        assert!(s["slots_used"].is_null());
        assert_eq!(s["client_exposed"], false);
        assert!(
            s["warning"]
                .as_str()
                .unwrap()
                .contains("unsupported read direction / incomplete feature")
        );
    }
    assert_eq!(conn.total_changes(), 0);
    assert!(!exists(&conn, TABLE).unwrap());
}

#[test]
fn boundary_records_only_proved_sources_and_preserves_dated_pending_evidence() {
    let mut m = manifest();
    m.sources = serde_json::from_value(json!([
        {"core":"alpha","name":"alpha","ships":[],"intake_direction":"push"},
        {"core":"beta","name":"beta","ships":[],"intake_direction":"local"},
        {"core":"gamma","name":"gamma","ships":[],"intake_direction":"collect"}
    ]))
    .unwrap();
    let boundary = switch_on(
        &m,
        &[
            json!({"id":"alpha","complete":true,"state":"covered","through":42,"received_at":100,"barrier_mode":"last_received_push"}),
            json!({"id":"beta","complete":false,"state":"cached","through":99,"received_at":50,"barrier_mode":"local"}),
        ],
    );
    assert_eq!(boundary[0]["state"], "established");
    assert_eq!(boundary[0]["through"], 42);
    assert_eq!(boundary[0]["received_at"], 100);
    assert_eq!(boundary[1]["state"], "switch_on_pending");
    assert_eq!(boundary[1]["through"], 99);
    assert_eq!(boundary[1]["evidence_state"], "cached");
    assert_eq!(boundary[2]["state"], "switch_on_pending");
    assert!(boundary[2]["through"].is_null());
}
