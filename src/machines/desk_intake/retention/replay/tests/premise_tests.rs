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
fn null_premise_and_outcome_still_produce_native_witness_and_delta() {
    let mut f = Fixture::new(false);
    f.send("ordinary");
    f.capture();
    let w = f.witness();
    assert_eq!(w.origin.id, "ordinary");
    assert!(w.delta_order > 0);
    let pin: String =
        f.h.connection()
            .query_row(
                "SELECT content FROM mail_source_origin WHERE id='ordinary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    let pin: Value = serde_json::from_str(&pin).unwrap();
    assert_eq!(pin.get("premise"), Some(&Value::Null));
    assert!(pin.get("premise_outcome").is_none());
    assert_eq!(crate::core::hash(&pin), w.digest);
}
