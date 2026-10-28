use super::*;
const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";
fn contact_for_test(owner: bool) -> Contact {
    let manifest = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,
        "max_source_age_ms":900000,"owner_core":"00000000-0000-4000-8000-00000000000b",
        "generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":SOURCE,
        "sources":[{"core":SOURCE,"name":"alpha","ships":[],"intake_direction":"push"}]}))
    .unwrap();
    Contact {
        manifest,
        source: SOURCE.into(),
        owner,
        direction: "push".into(),
        _fence: tempfile::tempfile().unwrap(),
    }
}
fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    init(&c).unwrap();
    c.execute_batch("CREATE TABLE ships(home TEXT);").unwrap();
    c
}
#[test]
fn empty_pages_keep_custody_until_exact_receipt_and_prior_receipts_are_harmless() {
    let source = db();
    let desk = db();
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = source_frame(&source, &c, true, deadline).unwrap();
    let first_id = first.page.as_ref().unwrap().checkpoint.id.clone();
    let retry = source_frame(&source, &c, true, deadline).unwrap();
    assert_eq!(retry.page.as_ref().unwrap().checkpoint.id, first_id);
    receive(&desk, &owner, first).unwrap();
    let first_ack = owner_frame(&desk, &owner).unwrap();
    let old_wire = serde_json::to_value(&first_ack).unwrap();
    receive(&source, &c, first_ack).unwrap();
    let second = source_frame(&source, &c, true, deadline).unwrap();
    let second_id = second.page.as_ref().unwrap().checkpoint.id.clone();
    assert_ne!(first_id, second_id);
    // The desk may repeat its previous receipt after our new page failed.
    receive(&source, &c, serde_json::from_value(old_wire).unwrap()).unwrap();
    let retried = source_frame(&source, &c, true, deadline).unwrap();
    assert_eq!(retried.page.as_ref().unwrap().checkpoint.id, second_id);
    let mut forged = owner_frame(&desk, &owner).unwrap();
    forged.ack.as_mut().unwrap().receipt = "forged".into();
    assert!(
        receive(&source, &c, forged)
            .unwrap_err()
            .to_string()
            .contains("invalid intake receipt")
    );
    assert!(
        !source
            .query_row("SELECT receipted FROM desk_intake_transport", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    receive(&desk, &owner, second).unwrap();
    receive(&source, &c, owner_frame(&desk, &owner).unwrap()).unwrap();
    assert!(
        source
            .query_row("SELECT receipted FROM desk_intake_transport", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
}
#[test]
fn optional_budget_interrupts_vm_and_restores_mail_busy_policy() {
    let c = db();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    {
        let _budget = Budget::new(&c, Instant::now()).unwrap();
        let result = c.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT SUM(x) FROM n",[],|r|r.get::<_,i64>(0));
        assert!(result.is_err());
        assert!(check(Instant::now()).is_err());
    }
    assert_eq!(
        c.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        10000
    );
    assert_eq!(
        c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).unwrap(),
        1
    );
}
#[test]
fn malformed_legacy_wrong_binding_role_direction_and_oversized_frames_fail_closed() {
    let c = contact_for_test(false);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    let good = serde_json::to_value(frame(&owner).unwrap()).unwrap();
    assert!(validate(&c, &good).is_ok());
    assert!(validate(&c, &Value::Null).is_err());
    for (key, value) in [
        ("protocol", json!("old")),
        ("binding", json!("wrong")),
        ("role", json!("source")),
        ("direction", json!("collect")),
        ("complete", json!(true)),
        ("extra", json!(true)),
    ] {
        let mut bad = good.clone();
        bad[key] = value;
        assert!(validate(&c, &bad).is_err(), "{key}");
    }
    let cdb = db();
    let mut mail = json!({"messages":[{"id":"ordinary","body":"x".repeat(701*1024)}]});
    let unchanged = mail.clone();
    attach(&cdb, "beta", &mut mail, good);
    assert_eq!(mail, unchanged);
}
