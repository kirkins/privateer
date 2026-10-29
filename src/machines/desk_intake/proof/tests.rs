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
fn only_provably_recoverable_obstacles_can_be_bypassed_and_legacy_gaps_are_loss() {
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
    // Upgrade an actual file-backed pre-proof schema with ambiguous old gaps.
    let old = Connection::open(dir.path().join("legacy.sqlite")).unwrap();
    crate::machines::init(&old).unwrap();
    old.execute_batch("DROP TABLE desk_intake_loss; INSERT INTO desk_intake_gap VALUES('alpha','beta','history_gap')").unwrap();
    super::super::init(&old).unwrap();
    assert_eq!(
        old.query_row("SELECT state FROM desk_intake_loss", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "history_gap"
    );
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
