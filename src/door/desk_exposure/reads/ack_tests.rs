use super::*;

fn grant(conn: &Connection) -> (String, String, DelegatedContext) {
    conn.execute("UPDATE door_desk_exposure SET enabled=1", [])
        .unwrap();
    let grant = uuid::Uuid::new_v4().to_string();
    let token = format!("alpha-{grant}");
    conn.execute("INSERT INTO door_clients(id,name,door,resource,owner,registration,scopes,created_ms,updated_ms,token_hash) VALUES(?1,'alpha','beta','door:beta','captain','alpha','[\"mail:read\"]',0,0,?2)",params![grant,Sha256::digest(token.as_bytes()).to_vec()]).unwrap();
    let context = crate::door::clients::resolve(conn, "beta", &token, &["mail:read"], None)
        .unwrap()
        .unwrap();
    (grant, token, context)
}
fn call(token: &str, c: &DelegatedContext, command: Command, args: Value) -> Result<Value, Denial> {
    request("beta", token, None, c, command, &args, &json!("alpha"))
}
fn count(conn: &Connection, table: &str, sub: &Value) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE subscription=?1"),
        [sub.as_str().unwrap()],
        |r| r.get(0),
    )
    .unwrap()
}

pub(super) fn ack_boundaries(conn: &Connection) {
    conn.execute("UPDATE door_clients SET revoked_ms=1", [])
        .unwrap();
    for mode in [
        "replay",
        "disable",
        "revoke",
        "restrict",
        "rotate",
        "pause",
        "binding",
        "commit_lost",
        "commit_failed",
    ] {
        let (grant, token, c) = grant(conn);
        let opened = call(&token, &c, Command::DeskOpen, json!({})).unwrap();
        let sub = &opened["subscription"];
        let page = call(&token, &c, Command::DeskPull, json!({"subscription":sub})).unwrap();
        let reference = &page["items"][0]["ref"];
        let args =
            json!({"subscription":sub,"ack_key":page["ack_key"],"refs":[reference,reference]});
        // Mixed batches roll back even the valid member, leaving its carry/pin.
        let mut bad = args.clone();
        bad["refs"] = json!([reference, "forged"]);
        assert_eq!(
            call(&token, &c, Command::DeskAck, bad).unwrap()["error"]["code"],
            "invalid_ack"
        );
        assert_eq!(count(conn, "desk_subscription_exception", sub), 1);
        let binding: String = conn
            .query_row("SELECT binding FROM door_desk_exposure", [], |r| r.get(0))
            .unwrap();
        let id = grant.clone();
        if matches!(mode, "commit_lost" | "commit_failed") {
            BEFORE_COMMIT.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |conn| {
                    // Force the actual transaction to have committed (lost success)
                    // or rolled back before the bridge's COMMIT reports an error.
                    conn.execute_batch(if mode == "commit_lost" {
                        "COMMIT"
                    } else {
                        "ROLLBACK"
                    })
                    .unwrap();
                }))
            });
        } else if mode != "replay" {
            AFTER_COMMIT.with(|h| *h.borrow_mut() = Some(Box::new(move |conn| {
                match mode {
                    "disable" => { conn.execute("UPDATE door_desk_exposure SET enabled=0",[]).unwrap(); }
                    "binding" => { conn.execute("UPDATE door_desk_exposure SET binding=json_set(binding,'$.switch_on_ms',2)",[]).unwrap(); }
                    "revoke" => { conn.execute("UPDATE door_clients SET revoked_ms=1 WHERE id=?1",[&id]).unwrap(); }
                    "restrict" => { conn.execute("UPDATE door_clients SET ships='[\"gamma\"]' WHERE id=?1",[&id]).unwrap(); }
                    "rotate" => { conn.execute("UPDATE door_clients SET token_hash=?2 WHERE id=?1",params![id,Sha256::digest(b"gamma").to_vec()]).unwrap(); }
                    "pause" => { conn.execute("UPDATE door_clients SET paused=1 WHERE id=?1",[&id]).unwrap(); }
                    _ => unreachable!(),
                }
            })));
        }
        let result = call(&token, &c, Command::DeskAck, args.clone());
        if mode == "replay" {
            assert_eq!(result.unwrap()["acknowledged"], 1);
        } else {
            assert_eq!(result, Err(Denial::OutcomeUnknown), "{mode}");
        }
        assert_eq!(
            count(conn, "desk_subscription_exception", sub),
            i64::from(mode == "commit_failed")
        );
        let outcome: Option<String> = conn
            .query_row(
                "SELECT batch FROM desk_subscription_receipt WHERE key=?1",
                [page["ack_key"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome.is_some(), mode != "commit_failed");
        if matches!(
            mode,
            "disable" | "revoke" | "restrict" | "rotate" | "pause" | "binding"
        ) {
            // The same mutation committed BEFORE admission is a definite
            // refusal, not a second possibly-applied ack.
            let denied = call(&token, &c, Command::DeskAck, args.clone());
            assert_ne!(denied, Err(Denial::OutcomeUnknown));
            assert!(denied.is_err() || denied.unwrap()["ok"] == false);
        }
        // Restore authorization without changing the retained reader/batch.
        conn.execute(
            "UPDATE door_desk_exposure SET enabled=1,binding=?1",
            [binding],
        )
        .unwrap();
        if mode == "rotate" {
            conn.execute(
                "UPDATE door_clients SET token_hash=?2 WHERE id=?1",
                params![grant, Sha256::digest(token.as_bytes()).to_vec()],
            )
            .unwrap();
        }
        if matches!(mode, "revoke" | "restrict" | "pause") {
            // Fixture-only restoration cannot erase the visibility revision.
            // Even restored scopes must not recover the old incarnation/key.
            conn.execute(
                "UPDATE door_clients SET revoked_ms=NULL,ships=NULL,paused=0 WHERE id=?1",
                [&grant],
            )
            .unwrap();
            let fresh = crate::door::clients::resolve(conn, "beta", &token, &["mail:read"], None)
                .unwrap()
                .unwrap();
            assert_eq!(
                call(&token, &fresh, Command::DeskAck, args).unwrap()["error"]["code"],
                "stale_or_unknown"
            );
            let resumed = call(
                &token,
                &fresh,
                Command::DeskResume,
                json!({"subscription":sub}),
            )
            .unwrap();
            assert_ne!(resumed["incarnation"], opened["incarnation"]);
            continue;
        }
        let replay = call(&token, &c, Command::DeskAck, args.clone()).unwrap();
        assert_eq!(replay["acknowledged"], 1);
        assert_eq!(replay["operation_complete"], true);
        assert_eq!(count(conn, "desk_subscription_exception", sub), 0);
        let expiry: i64 = conn
            .query_row(
                "SELECT expires FROM desk_subscription_receipt WHERE key=?1",
                [page["ack_key"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            call(&token, &c, Command::DeskAck, args.clone()).unwrap()["acknowledged"],
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT expires FROM desk_subscription_receipt WHERE key=?1",
                [page["ack_key"].as_str().unwrap()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            expiry
        );
        let mut changed = args.clone();
        changed["refs"] = json!(["forged"]);
        assert_eq!(
            call(&token, &c, Command::DeskAck, changed).unwrap()["error"]["code"],
            "invalid_ack"
        );
        conn.execute(
            "UPDATE desk_subscription_receipt SET expires=0 WHERE key=?1",
            [page["ack_key"].as_str().unwrap()],
        )
        .unwrap();
        assert_eq!(
            call(&token, &c, Command::DeskAck, args).unwrap()["error"]["code"],
            "stale_or_unknown"
        );
        // Before-admission disable is definite and cannot consume a new batch.
        conn.execute("UPDATE door_desk_exposure SET enabled=0", [])
            .unwrap();
        let receipts = count(conn, "desk_subscription_receipt", sub);
        assert_eq!(
            call(
                &token,
                &c,
                Command::DeskAck,
                json!({"subscription":sub,"ack_key":opened["ack_key"],"refs":[reference]})
            )
            .unwrap()["error"]["code"],
            "unavailable"
        );
        assert_eq!(count(conn, "desk_subscription_receipt", sub), receipts);
    }
}

pub(super) fn refusal_release_binding_race(conn: &Connection) {
    let (_, token, c) = grant(conn);
    let opened = call(&token, &c, Command::DeskOpen, json!({})).unwrap();
    BEFORE_REFUSAL.with(|h| {
        *h.borrow_mut() = Some(Box::new(|conn| {
            conn.execute(
                "UPDATE door_desk_exposure SET binding=json_set(binding,'$.switch_on_ms',2)",
                [],
            )
            .unwrap();
        }))
    });
    // This is an engine refusal with protected admitted coverage, not a page.
    assert_eq!(
        call(
            &token,
            &c,
            Command::DeskPull,
            json!({"subscription":opened["subscription"],"continuation":"forged"})
        ),
        Err(Denial::Scope)
    );
    assert_eq!(
        count(conn, "desk_subscription_snapshot", &opened["subscription"]),
        0
    );
}
