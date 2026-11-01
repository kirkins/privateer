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
    // These are exactly the six states accepted by b2b-i's gap() writer.
    // In particular, retention/provenance failures used history_gap or
    // integrity_gap, not separate retention_loss/provenance_conflict labels.
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
    ];
    for (kind, _) in kinds {
        old.execute("INSERT INTO desk_intake_gap VALUES('alpha',?1,?1)", [kind])
            .unwrap();
    }
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
    let before = old.total_changes();
    super::super::init(&old).unwrap();
    assert_eq!(old.total_changes(), before, "migration is idempotent");
}

#[test]
fn legacy_history_gap_stays_recoverable_in_real_reclamation_states() {
    // Drive the actual b2b-i reclamation algorithm, rather than inventing a
    // missing event. A live row can pin the prefix while later superseded or
    // disposable rows disappear, including across the 50-row work boundary.
    for (name, rows, batches, expected, prefix) in [
        ("unreclaimed", vec![("alpha", true)], 0, vec![1], 0),
        (
            "prefix",
            vec![("alpha", false), ("beta", true)],
            1,
            vec![2],
            1,
        ),
        (
            "interleaved",
            vec![
                ("alpha", true),
                ("beta", true),
                ("gamma", true),
                ("beta", true),
                ("gamma", true),
                ("alpha", true),
            ],
            1,
            vec![4, 5, 6],
            3,
        ),
        (
            "sparse",
            vec![
                ("alpha", true),
                ("beta", true),
                ("gamma", true),
                ("beta", true),
            ],
            1,
            vec![1, 3, 4],
            0,
        ),
        (
            "empty",
            vec![("alpha", false), ("beta", false)],
            1,
            vec![],
            2,
        ),
        (
            "batch-boundary",
            std::iter::once(("alpha", true))
                .chain(std::iter::repeat_n(("beta", true), 51))
                .collect(),
            1,
            vec![1, 51, 52],
            0,
        ),
        (
            "batch-resumed",
            std::iter::once(("alpha", true))
                .chain(std::iter::repeat_n(("beta", true), 51))
                .collect(),
            2,
            vec![1, 52],
            0,
        ),
    ] {
        let (_dir, r, _h, m) = setup();
        for (id, live) in rows {
            let fact = Fact {
                origin: MailIdentity::new(ALPHA, BETA, id).unwrap(),
                copy: None,
                content: json!({"body":id}),
                state: None,
                unread: live,
                pending: false,
            };
            append(&r, &m, ALPHA, &uuid::Uuid::new_v4().to_string(), &fact).unwrap();
        }
        let cp = checkpoint(&r, &m, ALPHA, &uuid::Uuid::new_v4().to_string()).unwrap();
        accept_all(&r, &m, &cp);
        let final_page = page(&r, &m, &cp, cp.upper, 50, BYTES).unwrap();
        for _ in 0..batches {
            retention::acknowledged(&r, &m, &final_page, deadline()).unwrap();
        }
        let retained = r
            .prepare("SELECT seq FROM desk_source_event ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(retained, expected, "{name}");
        let reclaimed: i64 = r
            .query_row("SELECT reclaimed_through FROM desk_source_head", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(reclaimed, prefix, "{name}");
        // Recreate the pre-proof schema and its unconditional first-capture
        // marker. There was no separate loss table in b2b-i.
        r.execute_batch("DROP TABLE desk_intake_loss; DROP TABLE desk_source_proof_work")
            .unwrap();
        obstacle(&r, &m, ALPHA, "history_gap").unwrap();
        super::super::init(&r).unwrap();
        let losses: i64 = r
            .query_row("SELECT COUNT(*) FROM desk_intake_loss", [], |r| r.get(0))
            .unwrap();
        assert_eq!(losses, 0, "{name}: normal reclamation is not history loss");
        let state: String = r
            .query_row("SELECT state FROM desk_intake_gap", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "history_gap", "{name}: retain the dated obstacle");
        let before = r.total_changes();
        super::super::init(&r).unwrap();
        assert_eq!(r.total_changes(), before, "{name}: quiet migration");
    }
}

#[test]
fn migrated_reclaimed_unresolved_receipt_blocks_proof_until_home_pin_resolves_or_mail_is_deleted() {
    fn notice(h: &Store) -> Value {
        h.connection()
            .query_row(
                &format!(
                    "SELECT {} FROM mail WHERE id='legacy-notice'",
                    crate::mail::COLUMNS
                ),
                [],
                crate::mail::message,
            )
            .unwrap()
    }
    for delete_mail in [false, true] {
        let (_dir, r, mut h, m) = setup();
        h.connection()
            .execute(
                "INSERT INTO peer_endpoints(name,config_json) VALUES('machine-beta',?1)",
                [json!({"transport":"collected"}).to_string()],
            )
            .unwrap();
        h.connection()
            .execute(
                "UPDATE ships SET record=json_set(record,'$.location',json(?1)) WHERE name='alpha'",
                [json!({"peer":"machine-beta"}).to_string()],
            )
            .unwrap();
        let message = json!({"id":"legacy-notice","from":"alpha","to":"captain",
            "kind":"notice","body":"Retained legacy mail","payload":{},"created_ms":1});
        crate::mail::deliver_with_provenance(&h, &message, &source::Provenance::Unresolved)
            .unwrap();
        let seq = notice(&h)["seq"].as_i64().unwrap();
        crate::mail::dispatch(
            &mut h,
            "mail.ack",
            json!({"address":"captain","through_seq":seq}),
        )
        .unwrap();
        capture(&h, &r, &m);
        let (intent, event, receipt): (String, String, String) = r
            .query_row(
                "SELECT intent,event,receipt FROM desk_home_receipt",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let fact: source::HomeFact = serde_json::from_str(&event).unwrap();
        assert_eq!(fact.provenance, source::Provenance::Unresolved);
        assert!(!fact.unread && !fact.pending && fact.state.as_deref() != Some("open"));
        // Model the old cleanup's exact cross-database discharge order for a
        // read/non-open/non-pending observation: registry receipt first, then
        // the home's receipted import/outbox. No origin pin is reclaimed.
        r.execute(
            "DELETE FROM desk_home_receipt WHERE intent=?1 AND receipt=?2",
            params![intent, receipt],
        )
        .unwrap();
        source::reclaim_imports(h.connection(), &[(intent, receipt)], 0, 0).unwrap();
        assert_eq!(
            h.connection()
                .query_row("SELECT COUNT(*) FROM mail_source_import", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            source::copy_provenance(h.connection(), "legacy-notice"),
            source::Provenance::Unresolved
        );
        assert!(!source::lost(h.connection()).unwrap());
        // b2b-i had only the recoverable history_gap diagnostic, not today's
        // sticky loss table. Upgrade with no retained unresolved receipt.
        r.execute_batch("DROP TABLE desk_intake_loss; DROP TABLE desk_source_proof_work")
            .unwrap();
        super::super::init(&r).unwrap();
        assert_eq!(
            r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "history_gap"
        );
        assert_eq!(
            r.query_row("SELECT COUNT(*) FROM desk_intake_loss", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
        let p = reconcile::load(&r, &m, ALPHA).unwrap().unwrap();
        let vector = serde_json::to_value(&p).unwrap();
        assert_eq!(vector["membership_proved"], true);
        assert!(!p.candidate);
        assert_eq!(vector["history_proved"], false);
        assert_eq!(vector["homes"][0]["state"], "unresolved");
        assert!(
            issued(&r, &m).1.is_none(),
            "reclaimed receipt cannot hide unresolved home provenance"
        );
        if delete_mail {
            assert_eq!(crate::mail::prune(&h, i64::MAX, false).unwrap(), 1);
            assert_eq!(
                h.connection()
                    .query_row("SELECT COUNT(*) FROM mail_source_origin", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1,
                "frozen payload pin survives history.prune, but orphan pin must not block proof"
            );
        } else {
            let identity = MailIdentity::new(
                ALPHA,
                "00000000-0000-4000-8000-000000000001",
                "legacy-notice",
            )
            .unwrap();
            let before = notice(&h);
            let replay = crate::mail::deliver_with_provenance(
                &h,
                &message,
                &source::Provenance::Copy(identity.clone()),
            )
            .unwrap();
            assert_eq!(replay["duplicate"], true);
            assert_eq!(
                source::copy_provenance(h.connection(), "legacy-notice"),
                source::Provenance::Copy(identity)
            );
            assert_eq!(
                notice(&h),
                before,
                "resolution does not change mail lifecycle"
            );
        }
        capture(&h, &r, &m);
        let (cp, evidence) = issued(&r, &m);
        let evidence =
            evidence.expect("resolved/deleted home pin permits proof through the legacy obstacle");
        assert!(evidence.valid && evidence.vector.candidate);
        accept_all(&r, &m, &cp);
        receive(&r, &m, ALPHA, Some(evidence.clone())).unwrap();
        assert!(proved(&r, &m, ALPHA).unwrap());
        assert!(!source::lost(h.connection()).unwrap());
        let changes = (h.connection().total_changes(), r.total_changes());
        for _ in 0..3 {
            reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
            assert!(reconcile::revalidate(&r, &m, ALPHA, &evidence.vector, deadline()).unwrap());
        }
        assert_eq!(
            changes,
            (h.connection().total_changes(), r.total_changes()),
            "quiet inspection/revalidation writes nothing"
        );
        if !delete_mail {
            // A legacy/skewed writer changing only a pin must also invalidate
            // an already saved vector during the bounded contact recheck.
            h.connection()
                .execute(
                    "UPDATE mail_source_origin SET provenance=?1 WHERE id='legacy-notice'",
                    [serde_json::to_string(&source::Provenance::Unresolved).unwrap()],
                )
                .unwrap();
            assert!(!reconcile::revalidate(&r, &m, ALPHA, &evidence.vector, deadline()).unwrap());
        }
    }
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
        content: json!({"body":"Authoritative immutable content","created_ms":2000}),
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
    let mut origin_content = event.content;
    origin_content["created_ms"] = json!(1000);
    let fact = Fact {
        origin,
        copy: None,
        content: origin_content,
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
            "SELECT json_extract(content,'$.created_ms') FROM desk_intake_fact WHERE id='copy'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1000,
        "copy's local timestamp never replaces the origin"
    );
    let mut legacy = e.clone();
    legacy.version = 1;
    receive(&r, &m, ALPHA, Some(legacy)).unwrap();
    assert!(
        !proved(&r, &m, ALPHA).unwrap(),
        "legacy digest semantics cannot claim completion"
    );
    receive(&r, &m, ALPHA, Some(e.clone())).unwrap();
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
    assert_eq!(
        with_sidecar(&frame, &json!({"invalid":true}), &Value::Null),
        frame
    );
    let replay = retention::replay::wire::marker();
    let siblings = json!({retention::replay::wire::KEY: replay});
    let extended = with_sidecar(&frame, &json!(e), &siblings);
    assert!(extended["proof"].is_object());
    assert_eq!(extended["replay"], replay);
    assert!(
        with_sidecar(&frame, &Value::Null, &replay)
            .get("replay")
            .is_none()
    );
    let legacy = json!({"desk_intake":frame,"messages":[]});
    assert_eq!(with_sidecar(&frame, &Value::Null, &legacy), frame);
    assert!(serde_json::from_value::<Evidence>(json!({"version":2})).is_err());
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &e.vector, Instant::now()).unwrap_or(false));
    // Keep Path used here to assert opaque handles, not filesystem identities.
    assert!(
        !serde_json::to_string(&e)
            .unwrap()
            .contains(Path::new("registry.sqlite").to_str().unwrap())
    );
}

#[test]
fn transitive_peer_cannot_prove_membership_with_an_authorized_ship_name() {
    let (_dir, r, h, m) = setup();
    capture(&h, &r, &m);
    assert!(reconcile::load(&r, &m, ALPHA).unwrap().unwrap().candidate);
    r.execute(
        "INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','beta','fleet')",
        [],
    )
    .unwrap();
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    assert!(!reconcile::load(&r, &m, ALPHA).unwrap().unwrap().candidate);
    assert!(issued(&r, &m).1.is_none());
    r.execute("DELETE FROM machine_ships WHERE machine='gamma'", [])
        .unwrap();
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    assert!(reconcile::load(&r, &m, ALPHA).unwrap().unwrap().candidate);
}

#[test]
fn source_completion_is_derived_and_read_only_and_never_clears_real_loss() {
    let (dir, r, mut h, m) = setup();
    send(&mut h, "alpha-retained-open");
    capture(&h, &r, &m);
    let (cp, e) = issued(&r, &m);
    accept_all(&r, &m, &cp);
    receive(&r, &m, ALPHA, e).unwrap();
    let clock = Clock {
        session: "alpha",
        wall_ms: 1100,
        tick_ms: 1100,
    };
    let status = |conn: &Connection, clock: &Clock<'_>| {
        super::super::completion::snapshot(conn, &m, ALPHA, clock, deadline()).unwrap()
    };
    assert_eq!(status(&r, &clock)["complete"], true);
    {
        let now = Instant::now();
        let _clock = source::SlowClock::start(now + Duration::from_secs(2), Duration::ZERO);
        let exhausted = super::super::completion::snapshot(&r, &m, ALPHA, &clock, now).unwrap();
        assert_eq!(exhausted["complete"], false);
        assert_eq!(exhausted["reason"], "local_revalidation_budget_exhausted");
    }
    let readonly = Connection::open_with_flags(
        dir.path().join("registry.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let before = r.total_changes();
    r.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(status(&readonly, &clock)["complete"], true);
    let restarted = Clock {
        session: "beta",
        wall_ms: 1100,
        tick_ms: 0,
    };
    assert_eq!(status(&readonly, &restarted)["complete"], false);
    assert_eq!(readonly.total_changes(), 0);
    r.execute_batch("ROLLBACK").unwrap();
    assert_eq!(r.total_changes(), before);
    send(&mut h, "alpha-post-barrier");
    assert_eq!(status(&r, &clock)["reason"], "local_vector_changed");
    capture(&h, &r, &m);
    let after = cp.upper;
    let (cp, e) = issued(&r, &m);
    let next = page(&r, &m, &cp, after, 50, BYTES).unwrap();
    accept(&r, &m, &next, &clock).unwrap();
    receive(&r, &m, ALPHA, e).unwrap();
    assert_eq!(status(&r, &clock)["complete"], true);
    gap(&r, &m, ALPHA, "integrity_gap").unwrap();
    r.execute("DELETE FROM desk_intake_gap", []).unwrap();
    assert_eq!(status(&r, &clock)["complete"], false);
    assert_eq!(
        r.query_row(
            "SELECT COUNT(*) FROM desk_intake_fact WHERE id LIKE 'alpha-%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}
