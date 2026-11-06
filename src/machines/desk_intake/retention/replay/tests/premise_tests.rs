use super::*;

#[test]
fn premise_is_pinned_but_resolution_does_not_change_native_replay_content() {
    for outcome in ["done", "cancelled"] {
        let mut f = Fixture::new(false);
        let premise = json!({"kind":"issue","repo":"example/charts","number":752});
        crate::mail::dispatch(
            &mut f.h,
            "mail.send",
            json!({"id":"ask","from":"survey","to":"captain","kind":"request",
                "body":"Reassess the chart","premise":premise}),
        )
        .unwrap();
        // Resolve before intake: capture must still produce the original native
        // witness rather than silently withholding it as a digest mismatch.
        f.h.connection()
            .execute(
                "UPDATE mail SET premise_outcome=?1 WHERE id='ask'",
                [outcome],
            )
            .unwrap();
        f.capture();
        let w = f.witness();
        assert_eq!(w.origin.id, "ask");
        let pin: String =
            f.h.connection()
                .query_row(
                    "SELECT content FROM mail_source_origin WHERE id='ask'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        let pin: Value = serde_json::from_str(&pin).unwrap();
        assert_eq!(pin["premise"], premise);
        assert!(pin.get("premise_outcome").is_none());
        assert_eq!(crate::core::hash(&pin), w.digest);

        // Restart does not require re-pinning or changing the committed witness.
        let home = f.dir.path().join("alpha");
        drop(f.h);
        f.h = Store::open(&home).unwrap();
        assert!(
            snapshot::native_now(&home, f.h.connection(), &f.m, ALPHA, &w.origin, &w.digest)
                .unwrap()
                .is_some()
        );
        f.h.connection()
            .execute("UPDATE mail SET premise_outcome=NULL WHERE id='ask'", [])
            .unwrap();
        assert!(
            snapshot::native_now(&home, f.h.connection(), &f.m, ALPHA, &w.origin, &w.digest)
                .unwrap()
                .is_some()
        );

        // Sharing the projection must not weaken the immutable-premise check.
        f.h.connection()
            .execute(
                "UPDATE mail SET premise=?1 WHERE id='ask'",
                [json!({"kind":"issue","repo":"example/charts","number":753}).to_string()],
            )
            .unwrap();
        assert!(
            snapshot::native_now(&home, f.h.connection(), &f.m, ALPHA, &w.origin, &w.digest)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn pre_premise_pin_retains_native_witness_and_delta_after_restart() {
    let mut f = Fixture::new(false);
    f.send("ordinary");
    let row = f
        .h
        .connection()
        .query_row(
            &format!(
                "SELECT {} FROM mail WHERE id='ordinary'",
                crate::mail::COLUMNS
            ),
            [],
            crate::mail::message,
        )
        .unwrap();
    // Build the old projection independently of immutable_content: these are
    // exactly the immutable keys emitted before premise columns existed.
    let mut pin = json!({});
    for key in [
        "id",
        "from",
        "to",
        "kind",
        "correlation",
        "task_id",
        "request_kind",
        "body",
        "payload",
        "urgency",
        "deadline_ms",
        "created_ms",
        "because_of",
    ] {
        pin[key] = row[key].clone();
    }
    let digest = crate::core::hash(&pin);
    let saved: String = f
        .h
        .connection()
        .query_row(
            "SELECT content FROM mail_source_origin WHERE id='ordinary'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // New pins must be byte-identical to the legacy layout too.
    assert_eq!(saved, pin.to_string());
    f.h.connection()
        .execute(
            "UPDATE mail_source_origin SET content=?1 WHERE id='ordinary'",
            [pin.to_string()],
        )
        .unwrap();

    let home = f.dir.path().join("alpha");
    drop(f.h);
    f.h = Store::open(&home).unwrap();
    let provenance: String = f
        .h
        .connection()
        .query_row(
            "SELECT provenance FROM mail_source_origin WHERE id='ordinary'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let source::Provenance::Origin(origin) = serde_json::from_str(&provenance).unwrap() else {
        panic!("expected native origin");
    };
    let (_, _, mail_order) =
        snapshot::native_now(&home, f.h.connection(), &f.m, ALPHA, &origin, &digest)
            .unwrap()
            .expect("legacy pin must still supply native replay evidence");
    assert_eq!(mail_order, row["seq"].as_i64().unwrap());

    f.capture();
    let w = f.witness();
    assert_eq!(w.origin, origin);
    assert_eq!(w.digest, digest);
    assert_eq!(w.mail_order, mail_order);
    assert!(w.delta_order > 0);
    let delta_order: i64 = f
        .r
        .query_row(
            "SELECT delta_order FROM desk_replay_import WHERE intent=?1 AND native=1 AND lane='delta'",
            [&w.intent],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(delta_order, w.delta_order);
}
