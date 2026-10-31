use super::*;

#[path = "original_tests.rs"]
mod original_tests;
#[path = "review_tests.rs"]
mod review_tests;

const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
const BETA: &str = "00000000-0000-4000-8000-00000000000b";
const BOX: &str = "00000000-0000-4000-8000-0000000000a1";
const COPY_BOX: &str = "00000000-0000-4000-8000-0000000000b1";

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn manifest() -> Manifest {
    serde_json::from_value(json!({"domain":"survey", "manifest_revision":1,
        "max_source_age_ms":coverage::DEFAULT_MAX_SOURCE_AGE_MS,
        "owner_core":BETA,"generation":1,"incarnation":id(),"coordinator":ALPHA,
        "sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"push"},
                   {"core":BETA,"name":"beta","ships":["relay"],"intake_direction":"local"}]}))
    .unwrap()
}
fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    conn
}
fn fact(n: usize) -> Fact {
    Fact {
        origin: MailIdentity::new(ALPHA, BOX, &format!("mail-{n}")).unwrap(),
        copy: None,
        content: json!({"from":"survey","to":"captain","body":format!("notice {n}"),"correlation":"ask-1","created_ms":1}),
        state: None,
        unread: true,
        pending: true,
    }
}
fn clock(wall_ms: i64) -> Clock<'static> {
    Clock {
        session: "process-1",
        wall_ms,
        tick_ms: wall_ms,
    }
}
fn capture(conn: &Connection, m: &Manifest, f: &Fact) {
    append(conn, m, ALPHA, &id(), f).unwrap();
}
fn one_page(conn: &Connection, m: &Manifest) -> Page {
    let cp = checkpoint(conn, m, ALPHA, &id()).unwrap();
    page(conn, m, &cp, 0, ITEMS, BYTES).unwrap()
}
fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn received(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT received_at FROM desk_intake_checkpoint", [], |r| {
        r.get(0)
    })
    .unwrap()
}
fn valid(conn: &Connection) -> bool {
    conn.query_row("SELECT clock_valid FROM desk_intake_checkpoint", [], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn snapshot_delta_closure_and_outgoing_are_projection_only() {
    let source = db();
    let desk = db();
    let m = manifest();
    let mut old = fact(0);
    old.state = Some("open".into());
    capture(&source, &m, &old);
    let mut outgoing = fact(1);
    outgoing.content = json!({"from":"captain","to":"survey","body":"local send","created_ms":1});
    capture(&source, &m, &outgoing);
    assert_eq!(count(&desk, "desk_intake_fact"), 0);
    let p = one_page(&source, &m);
    assert_eq!(
        p.events
            .iter()
            .map(|e| e.fact.origin.id.as_str())
            .collect::<Vec<_>>(),
        ["mail-0", "mail-1"]
    );
    assert_eq!(
        accept(&desk, &m, &p, &clock(100)).unwrap()["complete"],
        false
    );
    assert_eq!(count(&desk, "desk_intake_fact"), 2);
    old.state = Some("closed".into());
    old.pending = false;
    capture(&source, &m, &old);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let delta = page(&source, &m, &cp, p.through, 50, BYTES).unwrap();
    accept(&desk, &m, &delta, &clock(200)).unwrap();
    assert_eq!(count(&desk, "desk_intake_fact"), 2);
    let state: String = desk
        .query_row(
            "SELECT state FROM desk_intake_fact WHERE id='mail-0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "closed");
    // Only the projection tables exist. No delivery, request lifecycle or approval effects.
    let effects:i64=desk.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('mail','mail_cursor','door_cursor','desk_placement_audit')",[],|r|r.get(0)).unwrap();
    assert_eq!(effects, 0);
}

#[test]
fn imports_and_lost_receipt_replay_after_real_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let source = db();
    let m = manifest();
    let f = fact(0);
    let intent = id();
    assert_eq!(append(&source, &m, ALPHA, &intent, &f).unwrap(), 1);
    assert_eq!(append(&source, &m, ALPHA, &intent, &f).unwrap(), 1);
    assert_eq!(count(&source, "desk_source_event"), 1);
    let p = one_page(&source, &m);
    {
        let desk = Connection::open(&path).unwrap();
        init(&desk).unwrap();
        accept(&desk, &m, &p, &clock(100)).unwrap(); // Commit; receipt is lost.
    }
    let desk = Connection::open(&path).unwrap();
    let restarted = Clock {
        session: "process-2",
        wall_ms: 150,
        tick_ms: 0,
    };
    assert_eq!(accept(&desk, &m, &p, &restarted).unwrap()["replayed"], true);
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
    assert_eq!(received(&desk), Some(100));
    assert_eq!(
        source_coverage(&desk, &m, ALPHA, &restarted).unwrap().state,
        "cached"
    );
    assert!(!valid(&desk));
    assert!(!source_evidence(&desk, &m, ALPHA, &restarted).unwrap().1);
    // Old session cannot make persisted restart invalidation disappear.
    source_coverage(&desk, &m, ALPHA, &clock(160)).unwrap();
    assert!(!valid(&desk));
}

#[test]
fn snapshot_upper_is_immutable_and_limits_count_the_whole_wire_page() {
    let source = db();
    let m = manifest();
    for n in 0..3 {
        capture(&source, &m, &fact(n));
    }
    let contact = id();
    let cp = checkpoint(&source, &m, ALPHA, &contact).unwrap();
    capture(&source, &m, &fact(3));
    assert_eq!(checkpoint(&source, &m, ALPHA, &contact).unwrap(), cp);
    let first = page(&source, &m, &cp, 0, 1, BYTES).unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.through, 1);
    let exact = serde_json::to_vec(&first).unwrap().len();
    assert_eq!(page(&source, &m, &cp, 0, 50, exact).unwrap(), first);
    assert!(page(&source, &m, &cp, 0, 50, exact - 1).is_err());
    assert!(page(&source, &m, &cp, 0, 51, BYTES).is_err());
    assert!(page(&source, &m, &cp, 0, 1, BYTES + 1).is_err());
    let rest = page(&source, &m, &cp, 1, 50, BYTES).unwrap();
    assert_eq!(rest.events.len(), 2);
    assert_eq!(rest.through, 3);
    assert!(rest.events.iter().all(|e| e.fact.origin.id != "mail-3"));
}

#[test]
fn partial_contact_then_empty_contacts_refresh_but_old_replays_do_not() {
    let source = db();
    let desk = db();
    let m = manifest();
    for n in 0..2 {
        capture(&source, &m, &fact(n));
    }
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let first = page(&source, &m, &cp, 0, 1, BYTES).unwrap();
    accept(&desk, &m, &first, &clock(100)).unwrap();
    assert_eq!(received(&desk), None);
    assert!(!valid(&desk));
    assert!(!source_evidence(&desk, &m, ALPHA, &clock(100)).unwrap().1);
    let last = page(&source, &m, &cp, 1, 1, BYTES).unwrap();
    accept(&desk, &m, &last, &clock(200)).unwrap();
    assert_eq!(received(&desk), Some(200));
    assert!(valid(&desk));
    let empty_cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let empty = page(&source, &m, &empty_cp, 2, 50, BYTES).unwrap();
    assert!(empty.events.is_empty());
    accept(&desk, &m, &empty, &clock(300)).unwrap();
    assert_eq!(received(&desk), Some(300));
    assert!(
        accept(&desk, &m, &last, &clock(400))
            .unwrap_err()
            .to_string()
            .contains("superseded")
    );
    accept(&desk, &m, &empty, &clock(500)).unwrap();
    assert_eq!(received(&desk), Some(300));
    let coverage = source_coverage(&desk, &m, ALPHA, &clock(600)).unwrap();
    assert_eq!(coverage.state, "cached"); // No activated complete claim.
    let b = coverage.barrier.unwrap();
    assert_eq!(b.mode, "last_received_push");
    assert_eq!(b.received_at, 300);
    assert_eq!(b.age_ms, 300);
    assert_eq!(b.max_source_age_ms, 900_000);
    assert!(
        source_evidence(&desk, &m, ALPHA, &clock(900_300))
            .unwrap()
            .1
    );
    assert!(
        !source_evidence(&desk, &m, ALPHA, &clock(900_301))
            .unwrap()
            .1
    );
}

#[test]
fn empty_initial_contact_has_evidence_but_legacy_receipt_has_none() {
    let source = db();
    let desk = db();
    let m = manifest();
    assert!(
        source_coverage(&desk, &m, ALPHA, &clock(10))
            .unwrap()
            .barrier
            .is_none()
    );
    let p = one_page(&source, &m);
    assert_eq!(p.through, 0);
    accept(&desk, &m, &p, &clock(20)).unwrap();
    assert_eq!(count(&desk, "desk_intake_fact"), 0);
    assert_eq!(received(&desk), Some(20));
}

#[test]
fn default_operator_bound_expiry_and_clock_discontinuity() {
    assert!(contact_fresh(
        true,
        true,
        Some(900_000),
        coverage::DEFAULT_MAX_SOURCE_AGE_MS
    ));
    assert!(!contact_fresh(
        true,
        true,
        Some(900_001),
        coverage::DEFAULT_MAX_SOURCE_AGE_MS
    ));
    assert!(contact_fresh(true, true, Some(5_000), 5_000));
    assert!(!contact_fresh(true, true, Some(5_001), 5_000));
    assert!(!contact_fresh(false, true, Some(0), 900_000));
    assert!(!contact_fresh(true, false, Some(0), 900_000));
    assert!(!contact_fresh(true, true, Some(-1), 900_000));
    let source = db();
    let desk = db();
    let mut m = manifest();
    m.max_source_age_ms = 5_000;
    let p = one_page(&source, &m);
    accept(&desk, &m, &p, &clock(100)).unwrap();
    assert!(source_evidence(&desk, &m, ALPHA, &clock(5_100)).unwrap().1);
    assert!(!source_evidence(&desk, &m, ALPHA, &clock(5_101)).unwrap().1);
    let b = source_coverage(&desk, &m, ALPHA, &clock(5_101))
        .unwrap()
        .barrier
        .unwrap();
    assert_eq!(b.age_ms, 5_001);
    assert_eq!(b.max_source_age_ms, 5_000);
    assert!(valid(&desk));
    let backwards = Clock {
        session: "process-1",
        wall_ms: 50,
        tick_ms: 200,
    };
    source_coverage(&desk, &m, ALPHA, &backwards).unwrap();
    assert!(!valid(&desk));
    accept(&desk, &m, &p, &clock(300)).unwrap();
    assert!(!valid(&desk)); // replay cannot heal clock.
    let p2 = one_page(&source, &m);
    accept(&desk, &m, &p2, &clock(400)).unwrap();
    assert!(valid(&desk));
    let forwards = Clock {
        session: "process-1",
        wall_ms: 100_000,
        tick_ms: 401,
    };
    source_coverage(&desk, &m, ALPHA, &forwards).unwrap();
    assert!(!valid(&desk));
}

#[test]
fn conflict_rolls_back_entire_page_aliases_and_checkpoint_but_keeps_gap() {
    let source = db();
    let desk = db();
    let m = manifest();
    let mut f = fact(0);
    f.copy = Some(MailIdentity::new(BETA, COPY_BOX, "desk-copy").unwrap());
    capture(&source, &m, &f);
    let p = one_page(&source, &m);
    accept(&desk, &m, &p, &clock(100)).unwrap();
    let mut alias = fact(1);
    alias.copy = f.copy.clone();
    let mut provisional = fact(2);
    provisional.copy = Some(MailIdentity::new(BETA, COPY_BOX, "provisional-copy").unwrap());
    capture(&source, &m, &provisional);
    capture(&source, &m, &alias);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let p2 = page(&source, &m, &cp, 1, 50, BYTES).unwrap();
    assert!(
        accept(&desk, &m, &p2, &clock(200))
            .unwrap_err()
            .to_string()
            .contains("conflicting copy")
    );
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
    assert_eq!(count(&desk, "door_source_alias"), 1);
    assert_eq!(count(&desk, "desk_intake_contact"), 1);
    assert_eq!(received(&desk), Some(100));
    assert_eq!(
        source_coverage(&desk, &m, ALPHA, &clock(201))
            .unwrap()
            .state,
        "history_gap"
    );
    let raw: String = desk
        .query_row("SELECT state FROM desk_intake_gap", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw, "integrity_gap");
}

#[test]
fn conflicting_content_intent_and_reopening_are_not_silently_suppressed() {
    let source = db();
    let desk = db();
    let m = manifest();
    let intent = id();
    let f = fact(0);
    append(&source, &m, ALPHA, &intent, &f).unwrap();
    let p = one_page(&source, &m);
    accept(&desk, &m, &p, &clock(10)).unwrap();
    let mut bad = f.clone();
    bad.content["body"] = json!("different");
    assert!(append(&source, &m, ALPHA, &intent, &bad).is_err());
    capture(&source, &m, &bad);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    let p2 = page(&source, &m, &cp, 1, 50, BYTES).unwrap();
    assert!(accept(&desk, &m, &p2, &clock(20)).is_err());
    assert_eq!(received(&desk), Some(10));
    // A closure is monotonic, even without mail effects.
    let source = db();
    let desk = db();
    let mut closed = f.clone();
    closed.state = Some("closed".into());
    capture(&source, &m, &closed);
    let p = one_page(&source, &m);
    accept(&desk, &m, &p, &clock(10)).unwrap();
    closed.state = Some("open".into());
    capture(&source, &m, &closed);
    let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
    assert!(
        accept(
            &desk,
            &m,
            &page(&source, &m, &cp, 1, 50, BYTES).unwrap(),
            &clock(20)
        )
        .is_err()
    );
}

#[test]
fn copied_sequences_or_equal_ids_are_not_global_identity() {
    let source = db();
    let desk = db();
    let m = manifest();
    let f = fact(0);
    let mut other = f.clone();
    other.origin.mailbox = COPY_BOX.into();
    capture(&source, &m, &f);
    capture(&source, &m, &other);
    accept(&desk, &m, &one_page(&source, &m), &clock(10)).unwrap();
    assert_eq!(count(&desk, "desk_intake_fact"), 2);
}

#[test]
fn forged_pages_gaps_unknown_sources_and_generation_mismatch_fail_closed() {
    let source = db();
    let desk = db();
    let m = manifest();
    capture(&source, &m, &fact(0));
    let p = one_page(&source, &m);
    let mut forged = p.clone();
    forged.events[0].order = 9;
    assert!(accept(&desk, &m, &forged, &clock(10)).is_err());
    assert_eq!(count(&desk, "desk_intake_checkpoint"), 0);
    let mut forged = p.clone();
    forged.checkpoint.source = id();
    assert!(accept(&desk, &m, &forged, &clock(10)).is_err());
    let mut next = m.clone();
    next.generation = 2;
    next.incarnation = id();
    assert!(accept(&desk, &next, &p, &clock(10)).is_err());
    assert!(append(&source, &next, ALPHA, &id(), &fact(1)).is_err());
    assert!(checkpoint(&source, &next, ALPHA, &id()).is_err());
    let mut transitive = fact(2);
    transitive.origin.core = BETA.into();
    assert!(append(&source, &m, ALPHA, &id(), &transitive).is_err());
    assert!(
        page(
            &source,
            &m,
            &Checkpoint {
                upper: 2,
                ..p.checkpoint.clone()
            },
            0,
            50,
            BYTES
        )
        .is_err()
    );
    assert_eq!(count(&desk, "desk_intake_fact"), 0);
}

#[test]
fn source_backpressure_preserves_facts_while_completed_desk_receipts_are_reclaimed() {
    let source = db();
    let desk = db();
    let m = manifest();
    let mut f = fact(0);
    f.state = Some("open".into());
    capture(&source, &m, &f);
    let p = one_page(&source, &m);
    accept(&desk, &m, &p, &clock(10)).unwrap();
    for n in 1..BARRIERS {
        let cp = checkpoint(&source, &m, ALPHA, &id()).unwrap();
        let empty = page(&source, &m, &cp, 1, 50, BYTES).unwrap();
        accept(&desk, &m, &empty, &clock(10 + n)).unwrap();
    }
    assert!(checkpoint(&source, &m, ALPHA, &id()).is_err());
    // A fabricated contact with a reused ordinal cannot replace the latest.
    let mut more = p.clone();
    more.checkpoint.id = id();
    more.after = 1;
    more.events.clear();
    assert!(accept(&desk, &m, &more, &clock(100)).is_err());
    assert_eq!(count(&source, "desk_source_barrier"), BARRIERS);
    assert_eq!(count(&desk, "desk_intake_contact"), 1);
    assert_eq!(count(&desk, "desk_intake_fact"), 1);
    assert_eq!(
        source_coverage(&source, &m, ALPHA, &clock(100))
            .unwrap()
            .state,
        "backpressure"
    );
    let tuple: (bool, bool, String) = desk
        .query_row(
            "SELECT unread,pending,state FROM desk_intake_fact",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(tuple, (true, true, "open".into()));
}

#[test]
fn unsupported_unreadable_and_skipped_history_are_explicit_partial_states() {
    let desk = db();
    let m = manifest();
    for state in ["unsupported", "unreadable", "history_gap"] {
        let desk = db(); // Each independent failure remains visible; gaps have precedence.
        gap(&desk, &m, ALPHA, state).unwrap();
        let s = source_coverage(&desk, &m, ALPHA, &clock(10)).unwrap();
        assert_eq!(s.state, state);
        assert!(s.through.is_none());
        assert!(s.barrier.is_none());
    }
    assert!(source_coverage(&desk, &m, &id(), &clock(10)).is_err());
}

#[test]
fn read_only_open_does_not_migrate_or_invent_mail_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.sqlite3");
    drop(Connection::open(&path).unwrap());
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(source_coverage(&conn, &manifest(), ALPHA, &clock(10)).is_err());
    assert_eq!(count(&conn, "sqlite_master"), 0);
}

#[test]
fn concurrent_acceptance_has_one_commit_and_one_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("desk.sqlite3");
    let source = db();
    let m = manifest();
    capture(&source, &m, &fact(0));
    let p = one_page(&source, &m);
    let conn = Connection::open(&path).unwrap();
    init(&conn).unwrap();
    drop(conn);
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let start = start.clone();
            let m = m.clone();
            let p = p.clone();
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                start.wait();
                accept(&conn, &m, &p, &clock(100)).unwrap()["replayed"]
                    .as_bool()
                    .unwrap()
            })
        })
        .collect();
    let mut results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    results.sort();
    assert_eq!(results, [false, true]);
    let conn = Connection::open(path).unwrap();
    assert_eq!(count(&conn, "desk_intake_fact"), 1);
    assert_eq!(count(&conn, "desk_intake_contact"), 1);
}

#[test]
fn source_event_item_and_byte_bounds_keep_the_import_pending() {
    let source = db();
    let m = manifest();
    let mut huge = fact(0);
    huge.content["body"] = json!("x".repeat(BYTES));
    assert!(append(&source, &m, ALPHA, &id(), &huge).is_err());
    assert_eq!(count(&source, "desk_source_event"), 0);
    source
        .execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000)
        INSERT INTO desk_source_event(domain,source,generation,incarnation,seq,intent,event)
        SELECT ?1,?2,?3,?4,x,CAST(x AS TEXT),'{}' FROM n",
            params![m.domain, ALPHA, m.generation, m.incarnation],
        )
        .unwrap();
    assert!(append(&source, &m, ALPHA, &id(), &fact(1)).is_err());
    assert_eq!(count(&source, "desk_source_event"), EVENTS);
}

#[test]
fn legal_direction_labels_do_not_create_a_reverse_reader() {
    let mut m = manifest();
    assert_eq!(mode(&m, ALPHA).unwrap(), "last_received_push");
    m.sources[0].intake_direction = "collect".into();
    assert_eq!(mode(&m, ALPHA).unwrap(), "on_demand");
    assert_eq!(mode(&m, BETA).unwrap(), "on_demand");
    m.sources[0].intake_direction = "relay".into();
    assert!(mode(&m, ALPHA).is_err());
}
