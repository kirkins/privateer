use super::*;
use crate::{mail::source, store::Store};
use std::{path::Path, time::Duration};
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
const BETA: &str = "00000000-0000-4000-8000-00000000000b";
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn setup() -> (tempfile::TempDir, Connection, Store, Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let r = Connection::open(dir.path().join("registry.sqlite")).unwrap();
    crate::machines::init(&r).unwrap();
    r.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)",
    )
    .unwrap();
    let home = dir.path().join("alpha");
    let mut h = Store::open(&home).unwrap();
    source::bind(h.connection(), ALPHA).unwrap();
    crate::mail::dispatch(
        &mut h,
        "ship.put",
        json!({"name":"alpha","location":"here"}),
    )
    .unwrap();
    r.execute(
        "INSERT INTO ships VALUES('alpha',?1,NULL)",
        [home.to_string_lossy().as_ref()],
    )
    .unwrap();
    let m: Manifest = serde_json::from_value(json!({"domain":"alpha","manifest_revision":1,"max_source_age_ms":900000,"owner_core":ALPHA,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["alpha"],"intake_direction":"local"},{"core":BETA,"name":"beta","ships":["beta"],"intake_direction":"collect"}]})).unwrap();
    (dir, r, h, m)
}
fn capture(h: &Store, r: &Connection, m: &Manifest) {
    for _ in 0..4 {
        home::capture(h.connection(), r, m, ALPHA).unwrap();
    }
    reconcile::save(r, m, ALPHA, deadline()).unwrap();
}
fn issued(r: &Connection, m: &Manifest) -> (Checkpoint, Option<Evidence>) {
    let cp = checkpoint(r, m, ALPHA, &uuid::Uuid::new_v4().to_string()).unwrap();
    issue(r, m, &cp, deadline()).unwrap();
    let e = outgoing(r, m, &cp, deadline()).unwrap();
    (cp, e)
}
fn accept_all(r: &Connection, m: &Manifest, cp: &Checkpoint) {
    let mut after = 0;
    loop {
        let p = page(r, m, cp, after, 50, BYTES).unwrap();
        accept(
            r,
            m,
            &p,
            &Clock {
                session: "alpha",
                wall_ms: 1000,
                tick_ms: 1000,
            },
        )
        .unwrap();
        after = p.through;
        if after == cp.upper {
            break;
        }
    }
}
fn send(h: &mut Store, id: &str) {
    crate::mail::dispatch(
        h,
        "mail.send",
        json!({"id":id,"from":"alpha","to":"captain","kind":"request","body":"Retained fact"}),
    )
    .unwrap();
}

#[test]
fn vector_is_barrier_bound_replayable_and_read_only_without_claiming_completion() {
    let (dir, r, mut h, m) = setup();
    send(&mut h, "old-open");
    capture(&h, &r, &m);
    let (cp, e) = issued(&r, &m);
    let e = e.unwrap();
    assert!(e.valid && e.vector.candidate);
    assert!(
        !eligible(&r, &m, ALPHA, &e).unwrap(),
        "proof alone is not intake"
    );
    accept_all(&r, &m, &cp);
    receive(&r, &m, ALPHA, Some(e.clone())).unwrap();
    assert!(proved(&r, &m, ALPHA).unwrap());
    let before = r.total_changes();
    receive(&r, &m, ALPHA, Some(e.clone())).unwrap();
    assert_eq!(
        before,
        r.total_changes(),
        "quiet proof replay writes nothing"
    );
    let p = page(&r, &m, &cp, 0, 50, BYTES).unwrap();
    assert_eq!(
        accept(
            &r,
            &m,
            &p,
            &Clock {
                session: "later",
                wall_ms: 9000,
                tick_ms: 5
            }
        )
        .unwrap()["replayed"],
        true
    );
    assert_eq!(
        r.total_changes(),
        before,
        "receipt replay does not refresh clock or proof"
    );
    let readonly = Connection::open_with_flags(
        dir.path().join("registry.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let writer = Connection::open(dir.path().join("registry.sqlite")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(proved(&readonly, &m, ALPHA).unwrap());
    let cov = source_coverage(
        &readonly,
        &m,
        ALPHA,
        &Clock {
            session: "alpha",
            wall_ms: 1100,
            tick_ms: 1100,
        },
    )
    .unwrap();
    assert_eq!(cov.state, "cached", "activation belongs to the successor");
    assert!(reconcile::revalidate(&readonly, &m, ALPHA, &e.vector, deadline()).unwrap());
    assert_eq!(readonly.total_changes(), 0);
    writer.execute_batch("ROLLBACK").unwrap();
    let mut wrong = e.clone();
    wrong.checkpoint.upper += 1;
    assert!(!eligible(&r, &m, ALPHA, &wrong).unwrap());
    let mut wrong = m.clone();
    wrong.generation += 1;
    assert!(!eligible(&r, &wrong, ALPHA, &e).unwrap());
    receive(&r, &m, ALPHA, None).unwrap();
    assert!(!proved(&r, &m, ALPHA).unwrap());
    let retained: String = r
        .query_row("SELECT body FROM desk_intake_proof", [], |r| r.get(0))
        .unwrap();
    assert!(
        !retained.contains("old-open"),
        "wire has opaque home handles, not local paths or message lists"
    );
    assert!(
        serde_json::from_str::<Evidence>(&retained)
            .unwrap()
            .vector
            .candidate
    );
}

#[test]
fn post_census_mail_inventory_health_and_import_changes_invalidate_without_losing_custody() {
    let (_dir, r, mut h, m) = setup();
    send(&mut h, "before");
    capture(&h, &r, &m);
    let (_cp, e) = issued(&r, &m);
    let e = e.unwrap();
    send(&mut h, "after");
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &e.vector, deadline()).unwrap());
    assert!(
        !outgoing(&r, &m, &e.checkpoint, deadline())
            .unwrap()
            .unwrap()
            .valid
    );
    assert!(
        issued(&r, &m).1.is_none(),
        "stale saved vector cannot attach to a newer barrier"
    );
    capture(&h, &r, &m);
    let (_, fresh) = issued(&r, &m);
    let fresh = fresh.unwrap();
    assert!(fresh.valid);
    h.connection()
        .execute("UPDATE mail_source_health SET lost=1", [])
        .unwrap();
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &fresh.vector, deadline()).unwrap());
    h.connection()
        .execute("UPDATE mail_source_health SET lost=0", [])
        .unwrap();
    r.execute(
        "INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','gamma','fleet')",
        [],
    )
    .unwrap();
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &fresh.vector, deadline()).unwrap());
    r.execute("DELETE FROM machine_ships", []).unwrap();
    r.execute(
        "INSERT INTO desk_home_inventory VALUES('unavailable-alpha')",
        [],
    )
    .unwrap();
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &fresh.vector, deadline()).unwrap());
    assert_eq!(
        h.connection()
            .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn recoverable_obstacles_and_legacy_migration_distinguish_unproved_history_from_loss() {
    let (dir, r, h, m) = setup();
    capture(&h, &r, &m);
    obstacle(&r, &m, ALPHA, "history_gap").unwrap();
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    assert!(
        issued(&r, &m).1.is_some(),
        "unproved enrollment is not lost history"
    );
    gap(&r, &m, ALPHA, "integrity_gap").unwrap();
    obstacle(&r, &m, ALPHA, "backpressure").unwrap();
    r.execute("DELETE FROM desk_intake_gap", []).unwrap();
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    assert!(
        issued(&r, &m).1.is_none(),
        "deleting a diagnostic cannot erase real loss"
    );
    // Upgrade a file-backed pre-proof schema with every legacy gap kind.
    let old = Connection::open(dir.path().join("legacy.sqlite")).unwrap();
    crate::machines::init(&old).unwrap();
    old.execute_batch("DROP TABLE desk_intake_loss; DROP TABLE desk_source_proof_work")
        .unwrap();
    let kinds = [
        ("history_gap", false),
        ("backpressure", false),
        ("unsupported", false),
        ("unreadable", false),
        ("baseline_too_large", false),
        ("integrity_gap", true),
        ("retention_loss", true),
        ("provenance_conflict", true),
        ("unknown_legacy_diagnostic", true),
    ];
    for (kind, _) in kinds {
        old.execute("INSERT INTO desk_intake_gap VALUES('alpha',?1,?1)", [kind])
            .unwrap();
    }
    old.execute_batch("INSERT INTO desk_intake_gap VALUES('alpha','beta','history_gap');
        INSERT INTO desk_source_head(domain,source,binding,upper_seq) VALUES('alpha','beta','alpha',1)").unwrap();
    super::super::init(&old).unwrap();
    for (kind, loss) in kinds {
        let migrated: bool = old
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_intake_loss WHERE source=?1)",
                [kind],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(migrated, loss, "{kind}");
        assert_eq!(
            old.query_row(
                "SELECT state FROM desk_intake_gap WHERE source=?1",
                [kind],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            kind,
            "retain the dated diagnostic"
        );
    }
    assert_eq!(
        old.query_row(
            "SELECT state FROM desk_intake_loss WHERE source='beta'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "history_gap",
        "a retained journal hole is positive loss evidence"
    );
    let before = old.total_changes();
    super::super::init(&old).unwrap();
    assert_eq!(old.total_changes(), before, "migration is idempotent");
}

#[test]
fn explicit_copy_requires_authoritative_content_and_never_changes_origin_lifecycle() {
    let (_dir, r, h, m) = setup();
    // Import a genuine explicit remote-origin copy, as normal capture does.
    let origin = MailIdentity::new(BETA, "00000000-0000-4000-8000-000000000001", "copy").unwrap();
    let event = crate::mail::source::HomeFact {
        mailbox: "00000000-0000-4000-8000-000000000002".into(),
        id: "copy".into(),
        provenance: source::Provenance::Copy(origin.clone()),
        content: json!({"body":"Authoritative immutable content"}),
        state: Some("closed".into()),
        unread: false,
        pending: false,
    };
    // The trusted local import has already recorded this alias. The sidecar
    // cannot itself change origin lifecycle or create an origin fact.
    let alias = MailIdentity::new(ALPHA, &event.mailbox, "copy").unwrap();
    coverage::record_alias_in(&r, &alias, &origin).unwrap();
    r.execute(
        "INSERT INTO desk_home_copy VALUES(?1,?2,?3,?4)",
        params![
            m.domain,
            ALPHA,
            uuid::Uuid::new_v4().to_string(),
            serde_json::to_string(&event).unwrap()
        ],
    )
    .unwrap();
    obstacle(&r, &m, ALPHA, "history_gap").unwrap();
    capture(&h, &r, &m);
    let (cp, e) = issued(&r, &m);
    let e = e.unwrap();
    accept_all(&r, &m, &cp);
    receive(&r, &m, ALPHA, Some(e.clone())).unwrap();
    assert!(
        !proved(&r, &m, ALPHA).unwrap(),
        "no authoritative origin yet"
    );
    let fact = Fact {
        origin,
        copy: None,
        content: event.content,
        state: Some("open".into()),
        unread: true,
        pending: true,
    };
    append(&r, &m, BETA, &uuid::Uuid::new_v4().to_string(), &fact).unwrap();
    let remote = checkpoint(&r, &m, BETA, &uuid::Uuid::new_v4().to_string()).unwrap();
    let page = page(&r, &m, &remote, 0, 50, BYTES).unwrap();
    accept(
        &r,
        &m,
        &page,
        &Clock {
            session: "alpha",
            wall_ms: 1000,
            tick_ms: 1000,
        },
    )
    .unwrap();
    assert!(proved(&r, &m, ALPHA).unwrap());
    assert_eq!(
        r.query_row(
            "SELECT state FROM desk_intake_fact WHERE id='copy'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "open"
    );
    let mut conflict = e.clone();
    conflict.vector.copies[0].content = "different".into();
    assert!(!eligible(&r, &m, ALPHA, &conflict).unwrap());
    receive(&r, &m, ALPHA, Some(conflict)).unwrap();
    assert!(
        !proved(&r, &m, ALPHA).unwrap(),
        "same barrier cannot change its immutable vector"
    );
    assert_eq!(
        r.query_row(
            "SELECT state FROM desk_intake_loss WHERE source=?1",
            [ALPHA],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "integrity_gap"
    );
}

#[test]
fn revalidation_work_does_not_scan_retained_mail_and_expiration_is_fail_closed() {
    for count in [10, 3000] {
        let (_dir, r, mut h, m) = setup();
        for i in 0..count {
            send(&mut h, &format!("alpha-{i}"));
        }
        // Legacy atomic capture is a fixture convenience, never the production
        // contact path. Import bounded batches before saving the idle vector.
        source::snapshot(h.connection(), &format!("{}:{ALPHA}", binding(&m).unwrap())).unwrap();
        for _ in 0..(count / 50 + 2) {
            home::capture(h.connection(), &r, &m, ALPHA).unwrap();
        }
        reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
        let p = reconcile::load(&r, &m, ALPHA).unwrap().unwrap();
        reconcile::REVALIDATE_VM.with(|n| n.set(0));
        assert!(reconcile::revalidate(&r, &m, ALPHA, &p, deadline()).unwrap());
        let work = reconcile::REVALIDATE_VM.with(|n| n.get());
        assert!(
            work < 1500,
            "{count} retained rows used {work} VM instructions"
        );
        assert!(!reconcile::revalidate(&r, &m, ALPHA, &p, Instant::now()).unwrap_or(false));
        assert_eq!(
            h.connection()
                .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            count
        );
    }
}

#[test]
fn envelope_overflow_does_not_turn_quiet_contacts_into_repeated_issuance() {
    let (_dir, r, h, m) = setup();
    capture(&h, &r, &m);
    let (cp, e) = issued(&r, &m);
    let mut vector = e.unwrap().vector;
    vector.copies.push(CopyEvidence {
        origin: MailIdentity::new(BETA, &uuid::Uuid::new_v4().to_string(), "alpha").unwrap(),
        alias: MailIdentity::new(ALPHA, &uuid::Uuid::new_v4().to_string(), "alpha").unwrap(),
        content: String::new(),
    });
    let base = serde_json::to_vec(&vector).unwrap().len();
    vector.copies[0].content = "x".repeat(WIRE - 1 - base);
    let raw = serde_json::to_string(&vector).unwrap();
    assert_eq!(raw.len(), WIRE - 1);
    r.execute("UPDATE desk_source_reconciliation SET proof=?1", [raw])
        .unwrap();
    assert!(reconcile::load(&r, &m, ALPHA).unwrap().is_some());
    assert!(
        reconcile::revalidate(&r, &m, ALPHA, &vector, deadline()).unwrap(),
        "size must be the rejecting predicate, not stale metadata"
    );
    assert!(
        candidate(&r, &m, &cp).unwrap().is_none(),
        "vector fits, evidence envelope does not"
    );
    let before = r.total_changes();
    for _ in 0..5 {
        assert!(!changed(&r, &m, &cp, deadline()).unwrap());
    }
    assert_eq!(r.total_changes(), before);
    let later = checkpoint(&r, &m, ALPHA, &uuid::Uuid::new_v4().to_string()).unwrap();
    let before = r.total_changes();
    issue(&r, &m, &later, deadline()).unwrap();
    assert_eq!(
        r.total_changes(),
        before,
        "over-limit evidence does not even save a work cursor"
    );
    assert!(outgoing(&r, &m, &later, deadline()).unwrap().is_none());
}

#[test]
fn proof_sub_budget_reserves_half_and_skips_when_insufficient() {
    let now = Instant::now() + Duration::from_secs(3600);
    let _clock = source::SlowClock::start(now, Duration::ZERO);
    assert_eq!(
        sub_deadline(now + Duration::from_millis(20)),
        Some(now + Duration::from_millis(10))
    );
    assert!(sub_deadline(now + Duration::from_millis(1)).is_none());
    assert!(sub_deadline(now).is_none());
}

#[test]
fn oversized_vectors_and_copy_sets_withhold_only_optional_proof() {
    let (_dir, r, h, m) = setup();
    capture(&h, &r, &m);
    let (_, e) = issued(&r, &m);
    let e = e.unwrap();
    let oversized = format!(
        "{}{}",
        serde_json::to_string(&e.vector).unwrap(),
        " ".repeat(WIRE)
    );
    r.execute(
        "UPDATE desk_source_reconciliation SET proof=?1",
        [oversized],
    )
    .unwrap();
    assert!(reconcile::load(&r, &m, ALPHA).unwrap().is_none());
    let (cp, missing) = issued(&r, &m);
    assert!(missing.is_none());
    accept_all(&r, &m, &cp);
    let mut wire = json!({"protocol":"fleet.door-mail.v1","page":null,"complete":false});
    let legacy = wire.clone();
    wire["proof"] = json!({"oversized":"x".repeat(WIRE)});
    let mut body = json!({"messages":[{"id":"mail-still-flows"}]});
    let before = r.total_changes();
    transport::attach(&r, "beta", &mut body, wire, 4096);
    assert_eq!(
        body["desk_intake"], legacy,
        "omit proof before a viable legacy frame"
    );
    assert!(body.get("desk_intake_proof").is_none());
    assert_eq!(body["messages"][0]["id"], "mail-still-flows");
    assert_eq!(
        r.total_changes(),
        before,
        "proof omission alone needs no audit/write"
    );
    let f = source::HomeFact {
        mailbox: uuid::Uuid::new_v4().to_string(),
        id: "copy".into(),
        provenance: source::Provenance::Copy(
            MailIdentity::new(BETA, &uuid::Uuid::new_v4().to_string(), "copy").unwrap(),
        ),
        content: json!({"body":"copy"}),
        state: None,
        unread: true,
        pending: false,
    };
    for _ in 0..51 {
        r.execute(
            "INSERT INTO desk_home_copy VALUES(?1,?2,?3,?4)",
            params![
                m.domain,
                ALPHA,
                uuid::Uuid::new_v4().to_string(),
                serde_json::to_string(&f).unwrap()
            ],
        )
        .unwrap();
    }
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    let p = reconcile::load(&r, &m, ALPHA).unwrap().unwrap();
    assert!(
        !p.candidate,
        "never truncate required copy history into a successful proof"
    );
    assert_eq!(p.copies.len(), 50);
}

#[test]
fn optional_sidecar_is_strict_bounded_and_legacy_frame_stays_unchanged() {
    let (_dir, r, h, m) = setup();
    capture(&h, &r, &m);
    let (_, e) = issued(&r, &m);
    let e = e.unwrap();
    let frame = json!({"protocol":"fleet.door-mail.v1","complete":false});
    assert_eq!(with_sidecar(&frame, &json!({"invalid":true})), frame);
    let extended = with_sidecar(&frame, &json!(e));
    assert!(extended["proof"].is_object());
    assert!(serde_json::from_value::<Evidence>(json!({"version":2})).is_err());
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &e.vector, Instant::now()).unwrap_or(false));
    // Keep Path used here to assert opaque handles, not filesystem identities.
    assert!(
        !serde_json::to_string(&e)
            .unwrap()
            .contains(Path::new("registry.sqlite").to_str().unwrap())
    );
}
