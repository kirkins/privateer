use super::*;
use crate::store::Store;
use serde_json::json;

fn send(store: &mut Store, id: &str, kind: &str) -> Value {
    crate::mail::dispatch(store, "mail.send", json!({"id":id,"from":"survey","to":"captain","kind":kind,"body":"retained","created_ms":1})).unwrap()
}
fn home() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    bind(store.connection(), "00000000-0000-4000-8000-00000000000a").unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    (dir, store)
}
fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn copy_identity(mailbox: &str) -> Provenance {
    Provenance::Copy(
        crate::door::coverage::MailIdentity::new(
            "00000000-0000-4000-8000-00000000000b",
            mailbox,
            "replayed-copy",
        )
        .unwrap(),
    )
}

fn copy_home() -> (tempfile::TempDir, Store) {
    let (dir, store) = home();
    // Runtime delivery is remote mail, not a local sender bypass. Match a
    // real collected machine peer while keeping the fixture isolated.
    store
        .connection()
        .execute(
            "INSERT INTO peer_endpoints(name,config_json) VALUES('machine-beta',?1)",
            [json!({"transport":"collected"}).to_string()],
        )
        .unwrap();
    store
        .connection()
        .execute(
            "UPDATE ships SET record=json_set(record,'$.location',json(?1)) WHERE name='survey'",
            [json!({"peer":"machine-beta"}).to_string()],
        )
        .unwrap();
    (dir, store)
}

#[test]
fn replay_upgrades_unresolved_or_missing_copy_evidence_without_loss() {
    for missing in [false, true] {
        for active in [false, true] {
            let (_dir, store) = copy_home();
            let message = json!({"id":"replayed-copy","from":"survey","to":"captain",
                "kind":"request","body":"Retained request","payload":{}});
            crate::mail::deliver_with_provenance(&store, &message, &Provenance::Unresolved)
                .unwrap();
            let c = store.connection();
            if missing {
                c.execute(
                    "DELETE FROM mail_source_origin WHERE id='replayed-copy'",
                    [],
                )
                .unwrap();
            }
            if active {
                snapshot(c, "binding").unwrap();
                // Exercise a genuinely missing pin as well as Unresolved
                // after enrollment (the frozen baseline stays immutable).
                if missing {
                    c.execute(
                        "DELETE FROM mail_source_origin WHERE id='replayed-copy'",
                        [],
                    )
                    .unwrap();
                }
            }
            let provenance = copy_identity("00000000-0000-4000-8000-000000000001");
            let before = super::super::by_id(&store, "replayed-copy")
                .unwrap()
                .unwrap();
            let result =
                crate::mail::deliver_with_provenance(&store, &message, &provenance).unwrap();
            assert_eq!(result["duplicate"], true);
            assert_eq!(copy_provenance(c, "replayed-copy"), provenance);
            assert!(!lost(c).unwrap(), "missing={missing}, active={active}");
            assert_eq!(
                super::super::by_id(&store, "replayed-copy")
                    .unwrap()
                    .unwrap(),
                before
            );
            assert_eq!(count(c, "mail"), 1);
            assert_eq!(count(c, "mail_source_outbox"), i64::from(active));
            if active {
                let event: String = c
                    .query_row("SELECT event FROM mail_source_outbox", [], |r| r.get(0))
                    .unwrap();
                let event: HomeFact = serde_json::from_str(&event).unwrap();
                assert_eq!(event.provenance, provenance);
                assert_eq!(event.state.as_deref(), Some("open"));
                assert!(event.pending && event.unread);
                let baseline: String = c
                    .query_row("SELECT event FROM mail_source_import", [], |r| r.get(0))
                    .unwrap();
                let baseline: HomeFact = serde_json::from_str(&baseline).unwrap();
                assert_eq!(baseline.provenance, Provenance::Unresolved);
                assert_eq!(event.content, baseline.content);
            }
            let result =
                crate::mail::deliver_with_provenance(&store, &message, &provenance).unwrap();
            assert_eq!(result["duplicate"], true);
            assert_eq!(copy_provenance(c, "replayed-copy"), provenance);
            assert_eq!(
                count(c, "mail_source_outbox"),
                i64::from(active),
                "same-copy replay must not append another observation"
            );
            assert!(!lost(c).unwrap());
        }
    }
}

#[test]
fn replay_with_different_resolved_copy_identity_still_marks_loss() {
    let (_dir, store) = copy_home();
    let message = json!({"id":"replayed-copy","from":"survey","to":"captain",
        "kind":"notice","body":"Retained notice","payload":{}});
    let first = copy_identity("00000000-0000-4000-8000-000000000001");
    crate::mail::deliver_with_provenance(&store, &message, &first).unwrap();
    let conflicting = copy_identity("00000000-0000-4000-8000-000000000002");
    let result = crate::mail::deliver_with_provenance(&store, &message, &conflicting).unwrap();
    assert_eq!(result["duplicate"], true, "mail receipt must still succeed");
    assert!(lost(store.connection()).unwrap());
    assert_eq!(copy_provenance(store.connection(), "replayed-copy"), first);
    assert_eq!(count(store.connection(), "mail"), 1);
}

#[test]
fn copy_claim_cannot_upgrade_a_stored_local_instruction() {
    let (_dir, mut store) = home();
    send(&mut store, "replayed-copy", "request");
    let c = store.connection();
    // fact() persists Origin, never Local. Treat a corrupt/old stored Local
    // instruction as a conflict, not permission to overwrite the real origin.
    c.execute(
        "UPDATE mail_source_origin SET provenance='\"Local\"' WHERE id='replayed-copy'",
        [],
    )
    .unwrap();
    let before = super::super::by_id(&store, "replayed-copy").unwrap();
    verify_copy(
        c,
        "replayed-copy",
        &copy_identity("00000000-0000-4000-8000-000000000001"),
    );
    assert!(lost(c).unwrap());
    assert_eq!(copy_provenance(c, "replayed-copy"), Provenance::Local);
    assert_eq!(
        super::super::by_id(&store, "replayed-copy").unwrap(),
        before
    );
}

#[test]
fn expired_exchange_budget_rolls_back_baseline_without_pruning_mail() {
    let (_dir, mut store) = home();
    send(&mut store, "unread-old", "notice");
    send(&mut store, "open-old", "request");
    let error = snapshot_until(
        store.connection(),
        "binding",
        Some(std::time::Instant::now()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("snapshot time budget"));
    assert_eq!(count(store.connection(), "mail_source_snapshot"), 0);
    assert_eq!(count(store.connection(), "mail_source_import"), 0);
    assert_eq!(count(store.connection(), "mail"), 2);
    snapshot(store.connection(), "binding").unwrap();
    assert_eq!(count(store.connection(), "mail_source_import"), 2);
}

#[test]
fn prune_reclaims_only_orphan_dormant_origin_pins() {
    let (_dir, mut store) = home();
    let seen = send(&mut store, "seen", "notice");
    send(&mut store, "unread", "notice");
    send(&mut store, "open", "request");
    crate::mail::dispatch(
        &mut store,
        "mail.ack",
        json!({"address":"captain","through_seq":seen["seq"]}),
    )
    .unwrap();
    let c = store.connection();
    c.execute(
        "INSERT INTO mail_source_origin VALUES('retained','{}','{\"custody\":true}')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO mail_source_origin VALUES('orphan','{}','null')",
        [],
    )
    .unwrap();
    let cutoff = Store::now_ms() + 1;
    assert_eq!(crate::mail::prune(&store, cutoff, true).unwrap(), 1);
    assert_eq!(count(c, "mail_source_origin"), 5);
    // The production caller already holds a write transaction. Failed pin
    // cleanup must roll back the mail deletion, even if that caller catches
    // the error and keeps its outer transaction alive.
    let tx = c.unchecked_transaction().unwrap();
    c.execute_batch("CREATE TRIGGER prune_fail BEFORE DELETE ON mail_source_origin BEGIN SELECT RAISE(ABORT,'pin cleanup unavailable'); END;").unwrap();
    assert!(crate::mail::prune(&store, cutoff, false).is_err());
    assert_eq!(count(c, "mail"), 3);
    assert_eq!(count(c, "mail_source_origin"), 5);
    c.execute_batch("DROP TRIGGER prune_fail").unwrap();
    assert_eq!(crate::mail::prune(&store, cutoff, false).unwrap(), 1);
    assert_eq!(count(c, "mail_source_origin"), 3);
    tx.rollback().unwrap();
    assert_eq!(count(c, "mail_source_origin"), 5);
    assert_eq!(crate::mail::prune(&store, cutoff, false).unwrap(), 1);
    assert_eq!(count(c, "mail_source_origin"), 3);
    let ids: Vec<String> = c
        .prepare("SELECT id FROM mail_source_origin ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ids, ["open", "retained", "unread"]);
}

#[test]
fn ordinary_send_ack_close_survive_full_or_failing_outbox() {
    for fail in [false, true] {
        let (_dir, mut store) = home();
        snapshot(store.connection(), "binding").unwrap();
        if fail {
            store.connection().execute_batch("DROP TABLE mail_source_outbox; CREATE TABLE mail_source_outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT,intent TEXT NOT NULL UNIQUE,event TEXT NOT NULL CHECK(0));").unwrap();
        } else {
            store.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO mail_source_outbox(intent,event) SELECT 'retained-'||x,'{}' FROM n;").unwrap();
            store
                .connection()
                .execute("UPDATE mail_source_health SET entries=?1", [LIMIT])
                .unwrap();
        }
        let notice = send(&mut store, "notice-1", "notice");
        send(&mut store, "ask-1", "request");
        crate::mail::dispatch(
            &mut store,
            "mail.ack",
            json!({"address":"captain","through_seq":notice["seq"]}),
        )
        .unwrap();
        crate::mail::dispatch(
            &mut store,
            "mail.withdraw",
            json!({"id":"ask-1","actor":"survey","reason":"finished"}),
        )
        .unwrap();
        let state: String = store
            .connection()
            .query_row("SELECT state FROM mail WHERE id='ask-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "withdrawn");
        assert_eq!(
            count(store.connection(), "mail_source_outbox"),
            if fail { 0 } else { LIMIT }
        );
        let lost: bool = store
            .connection()
            .query_row("SELECT lost FROM mail_source_health", [], |r| r.get(0))
            .unwrap();
        assert!(lost);
        let timeout: u64 = store
            .connection()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 10_000);
    }
}

#[test]
fn locked_optional_table_has_no_wait_and_does_not_poison_mail_transaction() {
    // Shared-cache table locks isolate the optional table from ordinary mail.
    // A database-wide writer lock would lock mail too, and is not this case.
    for table in ["mail_source_outbox", "mail_source_health"] {
        let uri = format!(
            "file:source-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        let writer = Connection::open(&uri).unwrap();
        writer.execute_batch("CREATE TABLE mail(seq INTEGER PRIMARY KEY,id TEXT,sender TEXT,recipient TEXT,kind TEXT,correlation TEXT,task_id TEXT,request_kind TEXT,body TEXT,payload TEXT,urgency TEXT,deadline_ms INTEGER,created_ms INTEGER,state TEXT,answered_by TEXT,answered_ms INTEGER,ended_by TEXT,ended_ms INTEGER,end_reason TEXT,delivery TEXT,delivery_attempts INTEGER,delivery_error TEXT,delivered_ms INTEGER,because_of TEXT); CREATE TABLE mail_cursor(address TEXT PRIMARY KEY,through_seq INTEGER); CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT);").unwrap();
        init(&writer).unwrap();
        bind(&writer, "00000000-0000-4000-8000-00000000000a").unwrap();
        snapshot(&writer, "binding").unwrap();
        writer.busy_timeout(Duration::from_secs(10)).unwrap();
        let reader = Connection::open(&uri).unwrap();
        reader
            .execute_batch(&format!("BEGIN; SELECT * FROM {table};"))
            .unwrap();
        let started = std::time::Instant::now();
        writer.execute_batch("BEGIN; INSERT INTO mail(seq,id,sender,recipient,kind,body,payload,urgency,created_ms,delivery_attempts) VALUES(1,'mail-1','survey','captain','notice','body','{}','normal',1,0);").unwrap();
        observe(&writer, "mail-1", Some(&Provenance::Local));
        writer.execute_batch("INSERT INTO mail_cursor VALUES('captain',1); UPDATE mail SET state='acknowledged' WHERE id='mail-1'; COMMIT;").unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(count(&writer, "mail"), 1);
        let lost: bool = writer
        .query_row("SELECT lost OR EXISTS(SELECT 1 FROM metadata WHERE key='mail_source_loss') FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
        assert!(lost);
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&writer, "mail_source_outbox"), 0);
    }
}

#[test]
fn snapshot_is_atomic_bounded_and_has_no_age_cutoff() {
    let (_dir, mut store) = home();
    send(&mut store, "old-unread", "notice");
    send(&mut store, "old-open", "request");
    store
        .connection()
        .execute("UPDATE mail SET created_ms=1", [])
        .unwrap();
    crate::mail::dispatch(&mut store,"mail.send",json!({"id":"local-outgoing","from":"captain","to":"survey","kind":"notice","body":"local only"})).unwrap();
    let upper = snapshot(store.connection(), "binding").unwrap();
    assert_eq!(upper, 0);
    assert_eq!(count(store.connection(), "mail_source_import"), 3);
    let events: Vec<HomeFact> = store
        .connection()
        .prepare("SELECT event FROM mail_source_import ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect();
    assert_eq!(
        events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["old-unread", "old-open", "local-outgoing"]
    );
    assert!(events[0].unread);
    assert!(events[1].pending);
    assert_eq!(events[2].content["from"], "captain");
    assert!(events.iter().all(|e| matches!(&e.provenance, Provenance::Origin(origin) if origin.core == "00000000-0000-4000-8000-00000000000a")));
    crate::mail::dispatch(
        &mut store,
        "mail.withdraw",
        json!({"id":"old-open","actor":"survey"}),
    )
    .unwrap();
    assert_eq!(snapshot(store.connection(), "binding").unwrap(), upper);
    stage_deltas(store.connection()).unwrap();
    assert_eq!(count(store.connection(), "mail_source_import"), 5);
    let closure: String = store
        .connection()
        .query_row(
            "SELECT event FROM mail_source_import ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let closure: HomeFact = serde_json::from_str(&closure).unwrap();
    assert_eq!(closure.id, "old-open");
    assert_eq!(closure.state.as_deref(), Some("closed"));
    assert_eq!(closure.content, events[1].content);
    assert!(!closure.pending);
    assert_eq!(
        snapshot(store.connection(), "different-binding")
            .unwrap_err()
            .to_string(),
        "stale_generation"
    );
}

#[test]
fn oversized_snapshot_rolls_back_every_intent_and_barrier() {
    let (_dir, mut store) = home();
    send(&mut store, "notice-1", "notice");
    store
        .connection()
        .execute("UPDATE mail SET body=?1", ["x".repeat(ITEM_BYTES + 1)])
        .unwrap();
    // Existing immutable origin has bounded original content. A legacy row
    // has no such prior evidence, so force this fixture to be legacy.
    store
        .connection()
        .execute("DELETE FROM mail_source_origin", [])
        .unwrap();
    assert!(
        snapshot(store.connection(), "binding")
            .unwrap_err()
            .to_string()
            .starts_with("backpressure")
    );
    assert_eq!(count(store.connection(), "mail_source_snapshot"), 0);
    assert_eq!(count(store.connection(), "mail_source_import"), 0);
}

#[test]
fn snapshot_item_overflow_never_prunes_old_unread_or_open_facts() {
    let (_dir, store) = home();
    store.connection().execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state) SELECT 'old-'||x,'survey','captain','request','old open','{}','normal',1,'open' FROM n;").unwrap();
    assert!(
        snapshot(store.connection(), "binding")
            .unwrap_err()
            .to_string()
            .starts_with("backpressure")
    );
    assert_eq!(count(store.connection(), "mail"), LIMIT + 1);
    assert_eq!(count(store.connection(), "mail_source_import"), 0);
    assert_eq!(count(store.connection(), "mail_source_snapshot"), 0);
}

#[test]
fn mail_rollback_also_rolls_back_observation() {
    let (_dir, store) = home();
    snapshot(store.connection(), "binding").unwrap();
    let tx = store.write_transaction().unwrap();
    crate::mail::record_runtime(&store,&json!({"id":"rolled-back","from":"survey","to":"captain","kind":"request","body":"not committed","payload":{},"urgency":"normal"})).unwrap();
    assert_eq!(count(&tx, "mail_source_outbox"), 1);
    tx.rollback().unwrap();
    assert_eq!(count(store.connection(), "mail"), 0);
    assert_eq!(count(store.connection(), "mail_source_outbox"), 0);
}

#[test]
fn moving_home_pins_old_origins_and_starts_a_new_local_mailbox() {
    let (_dir, mut store) = home();
    let alpha = "00000000-0000-4000-8000-00000000000a";
    let beta = "00000000-0000-4000-8000-00000000000b";
    send(&mut store, "before-move", "notice");
    let old = copy_provenance(store.connection(), "before-move");
    bind(store.connection(), beta).unwrap();
    send(&mut store, "after-move", "notice");
    let new = copy_provenance(store.connection(), "after-move");
    let (Provenance::Copy(old), Provenance::Copy(new)) = (old, new) else {
        panic!("explicit origin required");
    };
    assert_eq!(old.core, alpha);
    assert_eq!(new.core, beta);
    assert_ne!(old.mailbox, new.mailbox);
    snapshot(store.connection(), "binding").unwrap();
    let event: String = store
        .connection()
        .query_row(
            "SELECT event FROM mail_source_import ORDER BY seq LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let f: HomeFact = serde_json::from_str(&event).unwrap();
    assert_eq!(f.provenance, Provenance::Origin(old));
    assert_eq!(f.mailbox, new.mailbox);
}

#[test]
fn unregistered_local_write_has_explicit_unresolved_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    send(&mut store, "unregistered", "notice");
    let raw: String = store
        .connection()
        .query_row("SELECT provenance FROM mail_source_origin", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Provenance>(&raw).unwrap(),
        Provenance::Unresolved
    );
}

#[test]
fn concurrent_snapshot_and_closure_cover_both_sides_of_the_barrier() {
    for _ in 0..4 {
        let (dir, mut store) = home();
        send(&mut store, "ask-1", "request");
        let path = dir.path().to_path_buf();
        let (ready_rx, go_tx) = {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let mut writer = Store::open(&path).unwrap();
                ready_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                crate::mail::dispatch(
                    &mut writer,
                    "mail.withdraw",
                    json!({"id":"ask-1","actor":"survey"}),
                )
                .unwrap();
            });
            (ready_rx, (go_tx, thread))
        };
        ready_rx.recv().unwrap();
        go_tx.0.send(()).unwrap();
        snapshot(store.connection(), "binding").unwrap();
        go_tx.1.join().unwrap();
        stage_deltas(store.connection()).unwrap();
        let rows = store
            .connection()
            .prepare("SELECT event FROM mail_source_import ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let latest = rows
            .iter()
            .rev()
            .map(|s| serde_json::from_str::<HomeFact>(s).unwrap())
            .find(|f| f.id == "ask-1")
            .unwrap();
        assert_eq!(latest.state.as_deref(), Some("closed"));
        let lost: bool = store
            .connection()
            .query_row("SELECT lost FROM mail_source_health", [], |r| r.get(0))
            .unwrap();
        assert!(!lost);
    }
}

#[test]
fn dormant_writes_beyond_outbox_limit_only_pin_origins() {
    let (_dir, store) = home();
    // An unusable outbox must not even be touched before a baseline exists.
    store.connection().execute_batch("DROP TABLE mail_source_outbox; CREATE TABLE mail_source_outbox(seq INTEGER PRIMARY KEY AUTOINCREMENT,intent TEXT NOT NULL UNIQUE,event TEXT NOT NULL CHECK(0));").unwrap();
    let tx = store.write_transaction().unwrap();
    for n in 0..=LIMIT {
        crate::mail::record_runtime(&store, &json!({"id":format!("dormant-{n}"),"from":"survey","to":"captain","kind":"notice","body":"retained","payload":{},"urgency":"normal"})).unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(count(store.connection(), "mail_source_origin"), LIMIT + 1);
    assert_eq!(count(store.connection(), "mail_source_outbox"), 0);
    assert_eq!(count(store.connection(), "mail_source_import"), 0);
    let health: (i64, i64, bool) = store
        .connection()
        .query_row(
            "SELECT entries,bytes,lost FROM mail_source_health",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(health, (0, 0, false));
    let payloads: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM mail_source_origin WHERE content<>'null'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        payloads, 0,
        "dormant pins must not duplicate message bodies"
    );
    let loss: bool = store
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='mail_source_loss')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!loss);
}

#[test]
fn dormant_large_ack_and_closure_are_in_baseline_not_lost_events() {
    let (_dir, mut store) = home();
    for n in 0..51 {
        send(&mut store, &format!("notice-{n}"), "notice");
    }
    send(&mut store, "ask-1", "request");
    crate::mail::dispatch(
        &mut store,
        "mail.ack",
        json!({"address":"captain","through_seq":51}),
    )
    .unwrap();
    crate::mail::dispatch(
        &mut store,
        "mail.withdraw",
        json!({"id":"ask-1","actor":"survey"}),
    )
    .unwrap();
    assert_eq!(count(store.connection(), "mail_source_outbox"), 0);
    assert_eq!(snapshot(store.connection(), "binding").unwrap(), 0);
    let lost: bool = store
        .connection()
        .query_row("SELECT lost FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    assert!(!lost);
    assert_eq!(count(store.connection(), "mail_source_import"), 53);
    let raw: String = store
        .connection()
        .query_row(
            "SELECT event FROM mail_source_import WHERE json_extract(event,'$.id')='ask-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<HomeFact>(&raw)
            .unwrap()
            .state
            .as_deref(),
        Some("closed")
    );
    let unread: i64 = store.connection().query_row("SELECT COUNT(*) FROM mail_source_import WHERE json_extract(event,'$.id') LIKE 'notice-%' AND json_extract(event,'$.unread')", [], |r| r.get(0)).unwrap();
    assert_eq!(unread, 0);
    // Once enrolled, the same large ack is explicitly a bounded coverage gap.
    let tx = store.write_transaction().unwrap();
    acknowledged(&tx, 0, 51);
    tx.commit().unwrap();
    let lost: bool = store
        .connection()
        .query_row("SELECT lost FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    assert!(lost);
}

#[test]
fn missing_health_row_uses_durable_metadata_loss_fallback() {
    let (dir, mut store) = home();
    store
        .connection()
        .execute("DELETE FROM mail_source_health", [])
        .unwrap();
    send(&mut store, "notice-1", "notice");
    drop(store);
    let conn = Connection::open(dir.path().join("privateer.sqlite")).unwrap();
    let loss: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key='mail_source_loss'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(loss, "1");
    assert_eq!(count(&conn, "mail"), 1);
}

#[test]
fn corrupt_byte_counter_cannot_overflow_or_fail_mail() {
    let (_dir, mut store) = home();
    snapshot(store.connection(), "binding").unwrap();
    store
        .connection()
        .execute("UPDATE mail_source_health SET bytes=?1", [i64::MAX])
        .unwrap();
    send(&mut store, "notice-1", "notice");
    assert_eq!(count(store.connection(), "mail"), 1);
    assert_eq!(count(store.connection(), "mail_source_outbox"), 0);
    let lost: bool = store
        .connection()
        .query_row("SELECT lost FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    assert!(lost);
}

#[test]
fn baseline_and_writer_lock_put_each_new_mail_on_exactly_one_side() {
    use rusqlite::hooks::{AuthAction, Authorization};
    use std::sync::mpsc::channel;
    for snapshot_first in [false, true] {
        let (dir, store) = home();
        let database = dir.path().join("privateer.sqlite");
        if snapshot_first {
            let (locked_tx, locked_rx) = channel();
            let (release_tx, release_rx) = channel();
            let snap = std::thread::spawn(move || {
                let conn = Connection::open(database).unwrap();
                conn.busy_timeout(Duration::from_secs(10)).unwrap();
                let mut paused = false;
                conn.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
                    if !paused
                        && matches!(
                            ctx.action,
                            AuthAction::Read {
                                table_name: "mail",
                                ..
                            }
                        )
                    {
                        paused = true;
                        // snapshot already holds IMMEDIATE and read its upper.
                        locked_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }
                    Authorization::Allow
                }));
                snapshot(&conn, "binding").unwrap()
            });
            locked_rx.recv().unwrap();
            let (started_tx, started_rx) = channel();
            let writer = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                crate::mail::record_runtime(&store, &json!({"id":"boundary","from":"survey","to":"captain","kind":"notice","body":"one","payload":{},"urgency":"normal"})).unwrap();
                store
            });
            started_rx.recv().unwrap();
            release_tx.send(()).unwrap();
            assert_eq!(snap.join().unwrap(), 0);
            let store = writer.join().unwrap();
            assert_eq!(count(store.connection(), "mail_source_import"), 0);
            assert_eq!(count(store.connection(), "mail_source_outbox"), 1);
            stage_deltas(store.connection()).unwrap();
            stage_deltas(store.connection()).unwrap();
            assert_eq!(count(store.connection(), "mail_source_import"), 1);
        } else {
            let tx = store.write_transaction().unwrap();
            crate::mail::record_runtime(&store, &json!({"id":"boundary","from":"survey","to":"captain","kind":"notice","body":"one","payload":{},"urgency":"normal"})).unwrap();
            assert_eq!(count(&tx, "mail_source_outbox"), 0);
            let (started_tx, started_rx) = channel();
            let snap = std::thread::spawn(move || {
                let conn = Connection::open(database).unwrap();
                conn.busy_timeout(Duration::from_secs(10)).unwrap();
                started_tx.send(()).unwrap();
                snapshot(&conn, "binding").unwrap()
            });
            started_rx.recv().unwrap();
            tx.commit().unwrap();
            assert_eq!(snap.join().unwrap(), 0);
            stage_deltas(store.connection()).unwrap();
            assert_eq!(count(store.connection(), "mail_source_outbox"), 0);
            assert_eq!(count(store.connection(), "mail_source_import"), 1);
        }
    }
}

#[test]
fn read_only_open_never_migrates_and_current_schema_init_needs_no_write_lock() {
    let (dir, store) = home();
    let database = dir.path().join("privateer.sqlite");
    let writer = Connection::open(&database).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    init(store.connection()).unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    for table in TABLES {
        store
            .connection()
            .execute_batch(&format!("DROP TABLE {table}"))
            .unwrap();
    }
    drop(store);
    let reader = Store::open_readonly(dir.path(), crate::store::Caller::Operator).unwrap();
    assert!(missing(reader.connection(), TABLES).unwrap());
    assert!(init(reader.connection()).is_err());
    assert!(missing(reader.connection(), TABLES).unwrap());
    drop(reader);
    let normal = Store::open(dir.path()).unwrap();
    assert!(!missing(normal.connection(), TABLES).unwrap());
}
