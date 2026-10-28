use super::*;
use crate::store::Store;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
const BETA: &str = "00000000-0000-4000-8000-00000000000b";
fn manifest() -> Manifest {
    serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,"owner_core":BETA,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"push"},{"core":BETA,"name":"beta","ships":["relay"],"intake_direction":"local"}]})).unwrap()
}
fn registry(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    super::super::init(&c).unwrap();
    c
}
fn home(path: &Path) -> Store {
    let mut store = Store::open(path).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    store
}
fn send(store: &mut Store, id: &str) {
    crate::mail::dispatch(
        store,
        "mail.send",
        json!({"id":id,"from":"survey","to":"captain","kind":"request","body":"old open"}),
    )
    .unwrap();
}
fn count(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn crash_between_each_home_registry_commit_retries_stable_intents() {
    let dir = tempfile::tempdir().unwrap();
    let hp = dir.path().join("home");
    let rp = dir.path().join("registry.sqlite");
    let mut h = home(&hp);
    let r = registry(&rp);
    let m = manifest();
    send(&mut h, "old-open");
    source::snapshot(h.connection(), &format!("{}:{ALPHA}", binding(&m).unwrap())).unwrap();
    let (intent, event): (String, String) = h
        .connection()
        .query_row("SELECT intent,event FROM mail_source_import", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Crash after home prepare, before registry append.
    drop(h);
    drop(r);
    let h = home(&hp);
    let r = registry(&rp);
    let f: HomeFact = serde_json::from_str(&event).unwrap();
    append(
        &r,
        &m,
        ALPHA,
        &intent,
        &resolved(&f, ALPHA).unwrap().unwrap(),
    )
    .unwrap();
    // Crash after append but before registry receipt.
    drop(h);
    drop(r);
    let h = home(&hp);
    let r = registry(&rp);
    let receipt = import_one(&r, &m, ALPHA, &intent, &event).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 1);
    // Crash after registry receipt, before the home stores it.
    drop(h);
    drop(r);
    let h = home(&hp);
    let r = registry(&rp);
    assert_eq!(import_one(&r, &m, ALPHA, &intent, &event).unwrap(), receipt);
    let result = capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(result["complete"], false);
    assert_eq!(count(&r, "desk_source_event"), 1);
    assert_eq!(count(&r, "desk_home_receipt"), 1);
    let stored: String = h
        .connection()
        .query_row("SELECT receipt FROM mail_source_import", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, receipt);
    assert_eq!(
        capture(h.connection(), &r, &m, ALPHA).unwrap()["imported"],
        0
    );
    let state: String = h
        .connection()
        .query_row("SELECT state FROM mail WHERE id='old-open'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "open");
    assert_eq!(count(h.connection(), "captain_approvals"), 0);
    let mut newer = m.clone();
    newer.generation += 1;
    assert_eq!(
        import_one(&r, &newer, ALPHA, &intent, &event)
            .unwrap_err()
            .to_string(),
        "stale_generation"
    );
}

#[test]
fn snapshot_then_real_writer_closure_replays_ordered_deltas_without_polling() {
    let dir = tempfile::tempdir().unwrap();
    let hp = dir.path().join("home");
    let mut h = home(&hp);
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    send(&mut h, "old-open");
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    drop(h);
    let hp2 = hp.clone();
    std::thread::spawn(move || {
        let mut writer = Store::open(&hp2).unwrap();
        crate::mail::dispatch(
            &mut writer,
            "mail.withdraw",
            json!({"id":"old-open","actor":"survey","reason":"finished"}),
        )
        .unwrap();
    })
    .join()
    .unwrap();
    let h = home(&hp);
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    let mut stmt = r
        .prepare("SELECT seq,event FROM desk_source_event ORDER BY seq")
        .unwrap();
    let events = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| {
            let (seq, text) = r.unwrap();
            (seq, serde_json::from_str::<Fact>(&text).unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].1.state.as_deref(), Some("open"));
    assert_eq!(events[2].1.state.as_deref(), Some("closed"));
    assert_eq!(events.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(events[0].1.content, events[2].1.content);
    // Returning the live row to open WITHOUT an observation does not invent
    // an event: the capture reads outbox deltas, never guesses old closures.
    h.connection()
        .execute("UPDATE mail SET state='open' WHERE id='old-open'", [])
        .unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 3);
}

#[test]
fn explicit_remote_copy_is_alias_evidence_not_wrong_source_fact_or_closure() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    let origin =
        MailIdentity::new(BETA, "00000000-0000-4000-8000-0000000000b1", "remote-ask").unwrap();
    crate::mail::deliver_with_provenance(&h,&json!({"id":"copy-ask","from":"survey","to":"captain","kind":"request","body":"remote","payload":{}}),&Provenance::Copy(origin.clone())).unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 0);
    assert_eq!(count(&r, "desk_home_copy"), 1);
    let mailbox: String = h
        .connection()
        .query_row("SELECT mailbox FROM mail_source_health", [], |r| r.get(0))
        .unwrap();
    let copy = MailIdentity::new(ALPHA, &mailbox, "copy-ask").unwrap();
    assert_eq!(coverage::origin_of(&r, &copy).unwrap(), origin);
    assert_eq!(count(&r, "desk_intake_fact"), 0);
    assert_eq!(count(&r, "desk_intake_gap"), 1);
    // The existing foundation still rejects a remote origin appended under
    // the local source. Copy support has not weakened source binding.
    let f = Fact {
        origin,
        copy: Some(copy),
        content: json!({}),
        state: Some("closed".into()),
        unread: false,
        pending: false,
    };
    assert!(
        append(&r, &m, ALPHA, &uuid::Uuid::new_v4().to_string(), &f)
            .unwrap_err()
            .to_string()
            .starts_with("integrity_gap")
    );
}

#[test]
fn legacy_provenance_is_retained_as_gap_not_inferred_from_matching_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(&dir.path().join("home"));
    let r = registry(&dir.path().join("registry.sqlite"));
    let m = manifest();
    send(&mut h, "legacy");
    h.connection()
        .execute("DELETE FROM mail_source_origin", [])
        .unwrap();
    capture(h.connection(), &r, &m, ALPHA).unwrap();
    assert_eq!(count(&r, "desk_source_event"), 0);
    assert_eq!(count(&r, "desk_home_receipt"), 1);
    assert_eq!(count(&r, "desk_intake_gap"), 1);
}

#[test]
fn census_keeps_retired_pre_move_missing_skipped_and_unreadable_homes() {
    let dir = tempfile::tempdir().unwrap();
    let r = registry(&dir.path().join("registry.sqlite"));
    r.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
        .unwrap();
    let old = dir.path().join("old");
    let active = dir.path().join("active");
    let unreadable = dir.path().join("unreadable");
    let missing = dir.path().join("missing");
    let skip = dir.path().join("skip");
    let _old = home(&old);
    let _active = home(&active);
    std::fs::create_dir_all(&unreadable).unwrap();
    std::fs::write(unreadable.join("privateer.sqlite"), b"not sqlite").unwrap();
    for (name, path, retired) in [
        ("survey", &active, false),
        ("relay", &old, true),
        ("alpha", &unreadable, false),
        ("beta", &missing, true),
        ("gamma", &skip, false),
    ] {
        r.execute(
            "INSERT INTO ships VALUES(?1,?2,?3)",
            params![name, path.to_string_lossy(), retired.then_some(1)],
        )
        .unwrap();
    }
    let pre_move = dir.path().join("pre-move");
    retain_home(&r, &pre_move).unwrap();
    let c = enumerate(&r, &manifest(), ALPHA, std::slice::from_ref(&skip)).unwrap();
    assert!(!c.complete);
    assert!(c.membership_gap);
    assert_eq!(c.homes.len(), 6);
    let state = |path: &Path| c.homes.iter().find(|h| h.path == path).unwrap();
    assert!(state(&old).retired);
    assert_eq!(state(&old).state, "retained");
    assert_eq!(state(&pre_move).state, "missing");
    assert_eq!(state(&skip).state, "skipped");
    assert_eq!(state(&missing).state, "missing");
    assert_eq!(state(&unreadable).state, "unreadable");
    assert!(!missing.exists());
    assert!(!skip.exists());
    assert_eq!(count(&r, "desk_intake_gap"), 1);
}

#[test]
fn registry_schema_fast_path_is_read_only_and_missing_tables_migrate_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.sqlite");
    let r = registry(&path);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    super::super::init(&r).unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    r.execute_batch("DROP TABLE desk_home_copy").unwrap();
    let ro =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(super::super::init(&ro).is_err());
    assert!(source::missing(&ro, &["desk_home_copy"]).unwrap());
    super::super::init(&r).unwrap();
    assert!(!source::missing(&r, &["desk_home_copy"]).unwrap());
}
