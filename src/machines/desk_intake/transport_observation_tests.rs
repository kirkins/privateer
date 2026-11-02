//! The new contact evidence remains unserved. Exercise the real reconstruction,
//! strict validation and receive boundary rather than calling its writer alone.
use super::*;
use retention::replay::wire::{self as replay_wire, observation::Sidecar};

fn row(c: &Connection) -> Value {
    c.query_row("SELECT binding,support,sidecar,observed_at,session,tick_ms FROM desk_replay_observation",[],|r|
        Ok(json!({"binding":r.get::<_,String>(0)?,"support":r.get::<_,String>(1)?,"sidecar":r.get::<_,String>(2)?,
            "observed_at":r.get::<_,i64>(3)?,"session":r.get::<_,String>(4)?,"tick_ms":r.get::<_,i64>(5)?}))).unwrap()
}
fn reconstructed(c: &Contact, sidecar: Option<Value>, evidence: Option<Value>) -> Value {
    let mut f = frame(c).unwrap();
    f.role = "source".into();
    let mut siblings = json!({});
    if let Some(sidecar) = sidecar {
        siblings[replay_wire::KEY] = sidecar;
    }
    proof::with_sidecar(&json!(f), &evidence.unwrap_or(Value::Null), &siblings)
}
fn deliver(c: &Connection, contact: &Contact, wire: Value) -> Result<()> {
    let frame = validate(contact, &wire)?;
    receive(c, contact, frame)
}

#[test]
fn no_sidecar_marker_after_real_proof_restart_and_unsupported_stay_distinct() {
    let mut f = replay_wire::tests::Fixture::remote();
    crate::door::subscription::init(&f.desk).unwrap();
    let owner = Contact {
        source: SOURCE.into(),
        owner: true,
        direction: "collect".into(),
        manifest: f.m.clone(),
        _fence: tempfile::tempfile().unwrap(),
    };
    // An intake-capable old peer with no replay sibling is unknown support,
    // not falsely unsupported, and still performs ordinary contact bookkeeping.
    deliver(&f.desk, &owner, reconstructed(&owner, None, None)).unwrap();
    assert_eq!(row(&f.desk)["support"], "unknown");
    assert_eq!(row(&f.desk)["sidecar"], "absent");
    f.send("native");
    let sidecar = f.stage_latest(false);
    let evidence = sidecar["proof"]["evidence"].clone();
    deliver(
        &f.desk,
        &owner,
        reconstructed(&owner, Some(sidecar), Some(evidence)),
    )
    .unwrap();
    assert!(
        count(&f.desk, "desk_replay_adopted") > 0,
        "real witness was adopted"
    );
    assert_eq!(row(&f.desk)["support"], "supported");
    assert_eq!(row(&f.desk)["sidecar"], "proof");
    let vector: String = f
        .desk
        .query_row("SELECT body FROM desk_replay_vector", [], |r| r.get(0))
        .unwrap();
    deliver(
        &f.desk,
        &owner,
        reconstructed(&owner, Some(replay_wire::marker()), None),
    )
    .unwrap();
    let marker = row(&f.desk);
    assert_eq!(marker["support"], "supported");
    assert_eq!(marker["sidecar"], "marker");
    assert_eq!(
        f.desk
            .query_row("SELECT body FROM desk_replay_vector", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        vector,
        "observation never replaces actual proof"
    );
    assert_eq!(count(&f.desk, "desk_replay_observation"), 1);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.sqlite");
    f.desk
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let reopened = Connection::open(path).unwrap();
    crate::door::subscription::init(&reopened).unwrap();
    assert_eq!(
        row(&reopened),
        marker,
        "restart preserves exact desk metadata"
    );
    deliver(&reopened, &owner, reconstructed(&owner, None, None)).unwrap();
    assert_eq!(row(&reopened)["support"], "supported");
    assert_eq!(row(&reopened)["sidecar"], "absent");
    for (wire, state) in [
        (json!({"version":99}), Sidecar::Unsupported),
        (json!({"version":1,"proof":"invalid"}), Sidecar::Invalid),
        (
            json!({"version":1,"padding":"x".repeat(replay_wire::WIRE)}),
            Sidecar::Oversized,
        ),
    ] {
        let wire = reconstructed(&owner, Some(wire), None);
        assert!(
            wire["replay"].is_null(),
            "discard raw malformed/oversized proof"
        );
        assert_eq!(wire["replay_observation"], json!(state));
        // Private reconstruction metadata must never leak back onto the wire.
        let frame = validate(&owner, &wire).unwrap();
        assert!(json!(frame).get("replay_observation").is_none());
        receive(&reopened, &owner, frame).unwrap();
        assert_eq!(row(&reopened)["sidecar"], json!(state));
    }
    assert_eq!(count(&reopened, "desk_replay_observation"), 1);
}

#[test]
fn rejected_intake_frames_and_source_receipts_cannot_record_desk_observations() {
    let db = db();
    crate::door::subscription::init(&db).unwrap();
    let c = contact_for_test(true);
    let wire = reconstructed(&c, Some(replay_wire::marker()), None);
    for field in ["binding", "source", "role", "direction", "protocol"] {
        let mut illegal = wire.clone();
        illegal[field] = json!("illegal");
        assert!(deliver(&db, &c, illegal).is_err());
        assert_eq!(
            count(&db, "desk_replay_observation"),
            0,
            "{field} rejected before recording"
        );
    }
    // Strip any peer-supplied reconstruction metadata. Only raw siblings may
    // establish this closed diagnostic, never an embedded assertion.
    let mut forged = wire;
    forged["replay_observation"] = json!("unsupported");
    let clean = proof::with_sidecar(
        &forged,
        &Value::Null,
        &json!({replay_wire::KEY:replay_wire::marker()}),
    );
    assert!(clean.get("replay_observation").is_none());
    deliver(&db, &c, clean).unwrap();
    let saved = row(&db);
    let source = Contact { owner: false, ..c };
    receive_replay(&db, &source, Some(&replay_wire::marker()), None);
    assert_eq!(
        row(&db),
        saved,
        "source-side receipt observation is not desk evidence"
    );
    // Even a full optional table never fails ordinary accepted contacts.
    db.execute_batch("CREATE TRIGGER block_observation BEFORE UPDATE ON desk_replay_observation BEGIN SELECT RAISE(ABORT,'fixture busy'); END;").unwrap();
    let wire = reconstructed(&source, Some(replay_wire::marker()), None);
    let owner = Contact {
        owner: true,
        ..source
    };
    deliver(&db, &owner, wire).unwrap();
    assert_eq!(row(&db), saved);
    assert!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_intake_transport WHERE supported=1)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}
