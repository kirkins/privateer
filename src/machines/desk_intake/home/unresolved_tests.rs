use super::*;

#[test]
fn unresolved_and_local_receipts_follow_live_mail_even_in_frozen_baselines() {
    for provenance in [Provenance::Unresolved, Provenance::Local] {
        for frozen in [false, true] {
            for action in ["retain", "delete", "resolve"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("alpha");
                let mut h = home(&path);
                let r = registry(&dir.path().join("beta.sqlite"));
                let m = manifest();
                send(&mut h, "retained");
                h.connection()
                    .execute(
                        "UPDATE mail_source_origin SET provenance=?1 WHERE id='retained'",
                        [serde_json::to_string(&provenance).unwrap()],
                    )
                    .unwrap();
                let f = HomeFact {
                    mailbox: "00000000-0000-4000-8000-00000000000c".into(),
                    id: "retained".into(),
                    provenance: provenance.clone(),
                    content: json!({}),
                    state: Some("open".into()),
                    unread: true,
                    pending: true,
                };
                let event = serde_json::to_string(&f).unwrap();
                let intent = uuid::Uuid::new_v4().to_string();
                let receipt = import_one(&r, &m, ALPHA, &intent, &event).unwrap();
                assert_eq!(count(&r, "desk_intake_loss"), 0);
                let sidecar = source::enrollment::open(&path, true).unwrap();
                source::enrollment::init_sidecar(&sidecar).unwrap();
                let imports = if frozen { &sidecar } else { h.connection() };
                imports
                    .execute(
                        "INSERT INTO mail_source_import(intent,event,receipt) VALUES(?1,?2,?3)",
                        params![intent, event, receipt],
                    )
                    .unwrap();
                match action {
                    "delete" => {
                        h.connection()
                            .execute("DELETE FROM mail WHERE id='retained'", [])
                            .unwrap();
                    }
                    "resolve" => {
                        let origin = MailIdentity::new(ALPHA, &f.mailbox, &f.id).unwrap();
                        h.connection()
                            .execute(
                                "UPDATE mail_source_origin SET provenance=?1 WHERE id='retained'",
                                [serde_json::to_string(&Provenance::Origin(origin)).unwrap()],
                            )
                            .unwrap();
                    }
                    _ => (),
                }
                // A second sweep also proves an extant unresolved observation
                // is not reclaimed merely because its cleanup cursor advanced.
                for _ in 0..2 {
                    reclaim_imports_from(imports, h.connection(), &r, &m, ALPHA, 1, None).unwrap();
                    let expected = i64::from(action == "retain");
                    assert_eq!(count(imports, "mail_source_import"), expected);
                    assert_eq!(count(&r, "desk_home_receipt"), expected);
                    let blocker: bool = r.query_row(
                        "SELECT EXISTS(SELECT 1 FROM desk_home_receipt WHERE json_extract(event,'$.provenance') IN ('Unresolved','Local'))",
                        [], |r| r.get(0),
                    ).unwrap();
                    assert_eq!(blocker, action == "retain");
                    assert_eq!(count(h.connection(), "mail"), i64::from(action != "delete"));
                    assert_eq!(count(h.connection(), "mail_source_origin"), 1);
                }
            }
        }
    }
}
