use super::*;

#[test]
fn many_small_numbers_do_not_regress_ordinary_frame_validation() {
    let db = db();
    let c = contact_for_test(false);
    let fact = Fact {
        origin: MailIdentity::new(SOURCE, "00000000-0000-4000-8000-00000000000c", "numbers")
            .unwrap(),
        copy: None,
        content: json!({"numbers":vec![1; 20_000]}),
        state: None,
        unread: false,
        pending: false,
    };
    append(
        &db,
        &c.manifest,
        SOURCE,
        &uuid::Uuid::new_v4().to_string(),
        &fact,
    )
    .unwrap();
    let cp = checkpoint(&db, &c.manifest, SOURCE, &uuid::Uuid::new_v4().to_string()).unwrap();
    let mut f = frame(&c).unwrap();
    f.page = Some(page(&db, &c.manifest, &cp, 0, ITEMS, WIRE_BYTES).unwrap());
    let wire = json!(f);
    assert!(
        json_size_bound(&wire) > WIRE_BYTES + 4096 + proof::WIRE + retention::replay::wire::WIRE
    );
    assert!(json_size(&wire) < WIRE_BYTES);
    let owner = Contact {
        manifest: c.manifest.clone(),
        ..contact_for_test(true)
    };
    assert!(validate(&owner, &wire).is_ok());
}

#[test]
fn filled_small_response_with_fifty_numeric_events_keeps_page_and_drops_replay() {
    let db = db();
    let c = contact_for_test(false);
    for n in 0..50 {
        let fact = Fact {
            origin: MailIdentity::new(
                SOURCE,
                "00000000-0000-4000-8000-00000000000c",
                &format!("numbers-{n}"),
            )
            .unwrap(),
            copy: None,
            content: json!({"numbers":vec![1; 80]}),
            state: None,
            unread: false,
            pending: false,
        };
        append(
            &db,
            &c.manifest,
            SOURCE,
            &uuid::Uuid::new_v4().to_string(),
            &fact,
        )
        .unwrap();
    }
    let limit = 64 * 1024;
    let cp = checkpoint(&db, &c.manifest, SOURCE, &uuid::Uuid::new_v4().to_string()).unwrap();
    let p = page(&db, &c.manifest, &cp, 0, ITEMS, limit - 4096).unwrap();
    assert_eq!(p.events.len(), 50);
    let mut f = frame(&c).unwrap();
    f.page = Some(p);
    let wire = json!(f);
    let mut mail = json!({"messages":[],"padding":""});
    let marker = retention::replay::wire::marker();
    let reserved = json_size(&mail) + json_size(&wire) + json_size(&marker) + 128 + 1024;
    mail["padding"] = json!("x".repeat(limit - reserved));
    let mut replay_wire = wire.clone();
    replay_wire["replay"] = json!({"version":1,"proof":null,"receipts":[]});
    attach(&db, "beta", &mut mail, replay_wire, limit);
    assert_eq!(mail["desk_intake"], wire);
    assert_eq!(mail[retention::replay::wire::KEY], marker);
    let response = json!({"id":"\\\"".repeat(128),"result":mail});
    assert!(json_size(&response) < limit);
    assert!(limit - json_size(&response) < 2048);
}
