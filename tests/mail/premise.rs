use super::*;

#[test]
fn premise_and_resolution_cross_the_existing_outbox_to_a_peer() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    let repo = _temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    dispatch(
        &mut a,
        "charter.put",
        json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut a,
        "task.create",
        json!({"id":"work","charter_id":"project","owner":"apps","title":"Deliver"}),
    )
    .unwrap();
    let premise = json!({"kind":"task","ref":"work"});
    send(&mut a,json!({"id":"peer-ask","from":"apps","to":"infra","kind":"request","body":"Review?","premise":premise})).unwrap();
    assert_eq!(
        dispatch(&mut b, "mail.show", json!({"id":"peer-ask"})).unwrap()["premise"],
        premise
    );
    dispatch(
        &mut a,
        "task.update",
        json!({"id":"work","actor":"apps","status":"cancelled"}),
    )
    .unwrap();
    dispatch(&mut a, "mail.flush", json!({})).unwrap();
    let ask = dispatch(&mut b, "mail.show", json!({"id":"peer-ask"})).unwrap();
    assert_eq!(ask["state"], "open");
    assert_eq!(ask["premise_outcome"], "cancelled");
    assert_eq!(notices(&a), 1);
    assert_eq!(notices(&b), 0);
    dispatch(&mut a, "mail.flush", json!({})).unwrap();
    let count: i64 = b
        .connection()
        .query_row(
            "SELECT count(*) FROM mail WHERE json_extract(payload,'$.premise_changed') IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let notice: (String, String) = b.connection().query_row(
        "SELECT body,payload FROM mail WHERE json_extract(payload,'$.premise_changed') IS NOT NULL", [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert!(
        notice
            .0
            .contains("the requester may reassess, withdraw or update it")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&notice.1).unwrap()["attached_to"],
        "peer-ask"
    );
}

#[test]
fn unrecognized_premise_changes_remain_ordinary_mail_and_do_not_block_peer_queue() {
    for case in [
        "old-peer",
        "mismatch",
        "future-outcome",
        "wrong-thread",
        "wrong-sender",
    ] {
        let Pair {
            _temp,
            mut a,
            mut b,
            ..
        } = pair();
        let premise = json!({"kind":"task","ref":"work"});
        let ask = json!({"id":"ask","from":"apps","to":"infra","kind":"request","body":"Review?","payload":{}});
        seed_pending(&a, &ask, 1);
        dispatch(&mut a, "mail.flush", json!({})).unwrap();
        if case != "old-peer" {
            b.connection()
                .execute(
                    "UPDATE mail SET premise=?1 WHERE id='ask'",
                    [premise.to_string()],
                )
                .unwrap();
        }
        if case == "wrong-sender" {
            b.connection()
                .execute("UPDATE mail SET sender='infra' WHERE id='ask'", [])
                .unwrap();
        }
        let change = json!({"ask":if case == "wrong-thread" {"other"} else {"ask"},
            "premise":if case == "mismatch" {json!({"kind":"task","ref":"other"})} else {premise},
            "outcome":if case == "future-outcome" {"superseded"} else {"done"}});
        seed_pending(
            &a,
            &json!({"id":"change","from":"apps","to":"infra","kind":"notice","correlation":"ask","body":"Premise changed","payload":{"premise_changed":change,"attached_to":"ask","attached_ship":"apps"}}),
            2,
        );
        seed_pending(
            &a,
            &json!({"id":"later","from":"apps","to":"infra","kind":"notice","body":"Queue still flows","payload":{}}),
            3,
        );
        dispatch(&mut a, "mail.flush", json!({})).unwrap();
        let received = dispatch(&mut b, "mail.show", json!({"id":"change"})).unwrap();
        assert_eq!(received["payload"]["premise_changed"], change, "{case}");
        assert!(received["payload"]["attached_to"].is_null(), "{case}");
        assert!(received["payload"]["attached_ship"].is_null(), "{case}");
        assert!(
            dispatch(&mut b, "mail.show", json!({"id":"ask"})).unwrap()["premise_outcome"]
                .is_null(),
            "{case}"
        );
        dispatch(&mut b, "mail.show", json!({"id":"later"})).unwrap();
        assert_eq!(
            dispatch(&mut a, "mail.show", json!({"id":"change"})).unwrap()["delivery"],
            "delivered",
            "{case}"
        );
    }
}

#[test]
fn future_premise_is_ignored_on_receive_but_refused_on_send() {
    let Pair {
        _temp,
        mut a,
        mut b,
        ..
    } = pair();
    let ask = json!({"id":"future","from":"apps","to":"infra","kind":"request","body":"Review?","premise":{"kind":"future","ref":"work"},"payload":{}});
    assert!(err(send(&mut a, ask.clone())).contains("premise must be"));
    seed_pending(&a, &ask, 1);
    a.connection()
        .execute(
            "UPDATE mail SET premise=?1 WHERE id='future'",
            [ask["premise"].to_string()],
        )
        .unwrap();
    seed_pending(
        &a,
        &json!({"id":"later","from":"apps","to":"infra","kind":"notice","body":"Queue still flows","payload":{}}),
        2,
    );
    dispatch(&mut a, "mail.flush", json!({})).unwrap();
    let received = dispatch(&mut b, "mail.show", json!({"id":"future"})).unwrap();
    assert!(received["premise"].is_null());
    assert_eq!(received["body"], "Review?");
    dispatch(&mut b, "mail.show", json!({"id":"later"})).unwrap();
}

#[test]
fn failed_notice_rolls_back_task_outcome_and_can_retry_without_loss() {
    let (_temp, mut store) = fleet();
    send(&mut store,json!({"id":"atomic","from":"apps","to":"captain","kind":"request","body":"Review?","premise":{"kind":"task","ref":"main"}})).unwrap();
    store.connection().execute_batch("CREATE TRIGGER fail_premise BEFORE INSERT ON mail WHEN json_extract(NEW.payload,'$.premise_changed') IS NOT NULL BEGIN SELECT RAISE(ABORT,'notice failed'); END;").unwrap();
    assert!(
        dispatch(
            &mut store,
            "task.update",
            json!({"id":"main","actor":"apps","status":"done"})
        )
        .is_err()
    );
    assert_eq!(notices(&store), 0);
    assert_eq!(
        dispatch(&mut store, "task.show", json!({"id":"main"})).unwrap()["status"],
        "open"
    );
    assert!(
        dispatch(&mut store, "mail.show", json!({"id":"atomic"})).unwrap()["premise_outcome"]
            .is_null()
    );
    store
        .connection()
        .execute_batch("DROP TRIGGER fail_premise;")
        .unwrap();
    dispatch(
        &mut store,
        "task.update",
        json!({"id":"main","actor":"apps","status":"done"}),
    )
    .unwrap();
    assert_eq!(notices(&store), 1);
}

fn notices(store: &Store) -> i64 {
    store.connection().query_row("SELECT count(*) FROM mail WHERE sender='crew' AND json_extract(payload,'$.premise_changed') IS NOT NULL", [], |r| r.get(0)).unwrap()
}

#[test]
fn accepts_typed_premises_and_refuses_malformed_missing_or_foreign_tasks() {
    let (_temp, mut store) = fleet();
    for (id, premise) in [
        ("task-ask", json!({"kind":"task","ref":"main"})),
        (
            "pr-ask",
            json!({"kind":"pr","repo":"acme/project","number":7}),
        ),
        (
            "issue-ask",
            json!({"kind":"issue","repo":"acme/project","number":8}),
        ),
    ] {
        let args = json!({"id":id,"from":"apps","to":"captain","kind":"request","body":"Review this","premise":premise});
        assert_eq!(send(&mut store, args.clone()).unwrap()["premise"], premise);
        assert_eq!(send(&mut store, args).unwrap()["duplicate"], true);
        assert!(send(&mut store,json!({"id":id,"from":"apps","to":"captain","kind":"request","body":"Review this","premise":{"kind":"task","ref":"main"},"payload":{"different":true}})).is_err());
    }
    for (premise, expected) in [
        (
            json!({"kind":"task","ref":"missing"}),
            "premise task must exist",
        ),
        (json!({"kind":"pr","repo":"bad","number":1}), "premise repo"),
        (
            json!({"kind":"issue","repo":"acme/project","number":0}),
            "premise number",
        ),
        (json!({"kind":"text","ref":"main"}), "premise must be"),
    ] {
        assert!(err(send(&mut store,json!({"from":"apps","to":"captain","kind":"decision","body":"Review this","premise":premise}))).contains(expected));
    }
    assert!(err(send(&mut store,json!({"from":"infra","to":"captain","kind":"request","body":"Review this","premise":{"kind":"task","ref":"main"}}))).contains("belong to the sender"));
    assert!(err(send(&mut store,json!({"from":"apps","to":"captain","kind":"notice","body":"Review this","premise":{"kind":"task","ref":"main"}}))).contains("premise applies only"));
    assert!(
        send(
            &mut store,
            json!({"from":"apps","to":"captain","kind":"notice","payload":{"premise_changed":{}}})
        )
        .is_err()
    );
    let bearings = dispatch(&mut store, "ship.bearings", json!({"ship":"apps"}))
        .unwrap()
        .to_string();
    assert!(
        bearings.contains("pr acme/project#7 (not observed)"),
        "{bearings}"
    );
    assert!(bearings.contains("issue acme/project#8 (not observed)"));
}

#[test]
fn terminal_premise_notifies_each_open_ask_once_and_keeps_its_decision_unchanged_across_restart() {
    for status in ["done", "cancelled"] {
        let (_temp, mut store) = fleet();
        {
            let _pinned = Pinned::pin(&store);
            privateer::fleet::register("apps", store.home(), "captain").unwrap();
        }
        for (id, kind) in [
            ("open-request", "request"),
            ("open-decision", "decision"),
            ("answered", "request"),
            ("withdrawn", "request"),
            ("ended", "request"),
        ] {
            send(&mut store,json!({"id":id,"from":"apps","to":"captain","kind":kind,"body":"Still needed?","premise":{"kind":"task","ref":"main"}})).unwrap();
        }
        send(&mut store,json!({"from":"captain","to":"apps","kind":"reply","correlation":"answered","body":"Yes"})).unwrap();
        dispatch(
            &mut store,
            "mail.withdraw",
            json!({"id":"withdrawn","actor":"apps"}),
        )
        .unwrap();
        dispatch(&mut store, "captain.dismiss", json!({"id":"ended"})).unwrap();
        assert_eq!(notices(&store), 0);
        dispatch(
            &mut store,
            "task.update",
            json!({"id":"main","actor":"apps","status":status}),
        )
        .unwrap();
        assert_eq!(notices(&store), 2);
        for id in ["open-request", "open-decision"] {
            let ask = dispatch(&mut store, "mail.show", json!({"id":id})).unwrap();
            assert_eq!(ask["state"], "open");
            assert_eq!(ask["premise_outcome"], status);
            assert_eq!(ask["premise_stale"], true);
            let text: String = store
                .connection()
                .query_row(
                    "SELECT body FROM mail WHERE sender='crew' AND correlation=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                text.contains(&format!("premise task main is {status}")),
                "{text}"
            );
        }
        let bearings = dispatch(&mut store, "ship.bearings", json!({"ship":"apps"}))
            .unwrap()
            .to_string();
        assert!(
            bearings.contains(&format!("{status}; ask may be stale")),
            "{bearings}"
        );
        let waiting = dispatch(&mut store, "captain.waiting", json!({}))
            .unwrap()
            .to_string();
        assert!(waiting.contains("premise_outcome"));
        let inbox = dispatch(&mut store, "mail.inbox", json!({"address":"apps"})).unwrap();
        let through = inbox["messages"].as_array().unwrap().last().unwrap()["seq"].clone();
        dispatch(
            &mut store,
            "mail.ack",
            json!({"address":"apps","through_seq":through}),
        )
        .unwrap();
        let home = store.home().to_path_buf();
        drop(store);
        let mut store = Store::open(&home).unwrap();
        // Terminal tasks cannot be updated again; neither startup nor the
        // refused replay should create another premise notice.
        assert!(
            dispatch(
                &mut store,
                "task.update",
                json!({"id":"main","actor":"apps","status":status}),
            )
            .is_err()
        );
        assert_eq!(notices(&store), 2);
        // Display-only staleness must not replace the captain's actual answer.
        send(&mut store,json!({"from":"captain","to":"apps","kind":"reply","correlation":"open-decision","body":"Yes"})).unwrap();
        let ask = dispatch(&mut store, "mail.show", json!({"id":"open-decision"})).unwrap();
        assert_eq!(ask["state"], "answered");
        assert_eq!(ask["premise_outcome"], status);
    }
}

#[test]
fn already_terminal_premise_is_recorded_and_notified_in_the_send_transaction() {
    let (_temp, mut store) = fleet();
    dispatch(
        &mut store,
        "task.update",
        json!({"id":"main","actor":"apps","status":"cancelled"}),
    )
    .unwrap();
    let args = json!({"id":"late","from":"apps","to":"captain","kind":"request","body":"Review","premise":{"kind":"task","ref":"main"}});
    assert_eq!(
        send(&mut store, args.clone()).unwrap()["premise_outcome"],
        "cancelled"
    );
    send(&mut store, args).unwrap();
    assert_eq!(notices(&store), 1);
}

#[test]
fn task_link_closure_takes_precedence_over_premise_wake() {
    let (_temp, mut store) = fleet();
    send(&mut store,json!({"id":"linked","from":"apps","to":"captain","kind":"request","body":"Review","task_id":"main","premise":{"kind":"task","ref":"main"}})).unwrap();
    dispatch(
        &mut store,
        "task.update",
        json!({"id":"main","actor":"apps","status":"done"}),
    )
    .unwrap();
    assert_eq!(notices(&store), 0);
    assert_eq!(
        dispatch(&mut store, "mail.show", json!({"id":"linked"})).unwrap()["state"],
        "task_done"
    );
}
