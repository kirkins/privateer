use super::*;
use crate::store::Store;
use std::time::Duration;
const SOURCE: &str = "00000000-0000-4000-8000-00000000000a";
fn manifest() -> Manifest {
    serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,
        "owner_core":SOURCE,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":SOURCE,
        "sources":[{"core":SOURCE,"name":"alpha","ships":["survey"],"intake_direction":"local"}]})).unwrap()
}
fn registry(path: &Path) -> Connection {
    let r = Connection::open(path).unwrap();
    super::super::super::init(&r).unwrap();
    r.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)",
    )
    .unwrap();
    r
}
fn inspect_now(r: &Connection, m: &Manifest) -> Proof {
    inspect(r, m, SOURCE, &[], Instant::now() + Duration::from_secs(10)).unwrap()
}
fn capture(path: &Path, h: &Store, r: &Connection, m: &Manifest) {
    for _ in 0..4 {
        super::home::capture_ready_until(
            path,
            h.connection(),
            r,
            m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
    }
}
#[test]
fn census_proves_baselines_and_deltas_including_retired_homes_and_falls_back() {
    let t = tempfile::tempdir().unwrap();
    let r = registry(&t.path().join("registry.sqlite"));
    let m = manifest();
    let mut stores = Vec::new();
    for (name, retired) in [("survey", false), ("old", true)] {
        let path = t.path().join(name);
        let mut h = Store::open(&path).unwrap();
        source::bind(h.connection(), SOURCE).unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        crate::mail::dispatch(&mut h,"mail.send",json!({"id":name,"from":"survey","to":"captain","kind":"request","body":"Old retained request"})).unwrap();
        r.execute(
            "INSERT INTO ships VALUES(?1,?2,?3)",
            params![name, path.to_string_lossy(), retired.then_some(1)],
        )
        .unwrap();
        let p = inspect_now(&r, &m);
        assert!(!p.history_proved);
        assert!(p.homes.iter().any(|h| h.state == "unpublished"));
        super::baseline::enroll_home(
            &path,
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
            100,
            || Ok(()),
        )
        .unwrap();
        assert!(
            inspect_now(&r, &m)
                .homes
                .iter()
                .any(|h| h.state == "baseline_pending")
        );
        capture(&path, &h, &r, &m);
        stores.push((path, h));
    }
    let p = inspect_now(&r, &m);
    assert!(p.membership_proved && p.history_proved, "{p:?}");
    assert_eq!(p.homes.iter().filter(|h| h.retired).count(), 1);
    assert!(
        p.homes
            .iter()
            .all(|h| h.unread == Some(1) && h.open == Some(1))
    );
    let (path, h) = &mut stores[0];
    crate::mail::dispatch(h,"mail.send",json!({"id":"later","from":"survey","to":"captain","kind":"notice","body":"Post-baseline observation"})).unwrap();
    assert!(!inspect_now(&r, &m).history_proved);
    assert!(
        inspect_now(&r, &m)
            .homes
            .iter()
            .any(|h| h.state == "delta_pending")
    );
    capture(path, h, &r, &m);
    assert!(inspect_now(&r, &m).history_proved);
    let skipped = inspect(
        &r,
        &m,
        SOURCE,
        std::slice::from_ref(path),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    assert!(!skipped.history_proved);
    assert!(skipped.homes.iter().any(|h| h.state == "skipped"));
    r.execute(
        "INSERT INTO desk_home_inventory_gap VALUES(1,'history_gap')",
        [],
    )
    .unwrap();
    let gap = inspect_now(&r, &m);
    assert!(gap.membership_history_gap && !gap.membership_proved && !gap.history_proved);
}
#[test]
fn registry_gaps_block_history_even_when_all_home_pointers_are_imported() {
    let t = tempfile::tempdir().unwrap();
    let r = registry(&t.path().join("registry.sqlite"));
    let m = manifest();
    let path = t.path().join("survey");
    let h = Store::open(&path).unwrap();
    source::bind(h.connection(), SOURCE).unwrap();
    r.execute(
        "INSERT INTO ships VALUES('survey',?1,NULL)",
        [path.to_string_lossy().as_ref()],
    )
    .unwrap();
    super::baseline::enroll_home(
        &path,
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        100,
        || Ok(()),
    )
    .unwrap();
    capture(&path, &h, &r, &m);
    assert!(inspect_now(&r, &m).history_proved);
    // Gaps belong to one domain/source, not the whole registry.
    r.execute(
        "INSERT INTO desk_intake_gap VALUES('other-domain',?1,'integrity_gap')",
        [SOURCE],
    )
    .unwrap();
    r.execute(
        "INSERT INTO desk_intake_gap VALUES(?1,'other-source','integrity_gap')",
        [&m.domain],
    )
    .unwrap();
    assert!(inspect_now(&r, &m).history_proved);
    for state in [
        "integrity_gap",
        "history_gap",
        "unreadable",
        "unsupported",
        "baseline_too_large",
        "backpressure",
    ] {
        let error: Result<()> = Err(anyhow::anyhow!("{state}: home reclamation receipt"));
        record_failure(&r, &m, SOURCE, &error).unwrap();
        let before = r.total_changes();
        let proof = inspect_now(&r, &m);
        assert!(proof.membership_proved);
        assert!(proof.homes.iter().all(|h| h.state == "imported"));
        assert!(
            !proof.history_proved,
            "registry {state} must block history proof"
        );
        assert_eq!(
            r.total_changes(),
            before,
            "inspection must not clear the gap"
        );
        if state == "integrity_gap" {
            gap(&r, &m, SOURCE, "history_gap").unwrap();
            assert!(
                !inspect_now(&r, &m).history_proved,
                "sticky integrity gap survived a later census"
            );
        }
        // Explicit test-only recovery; production inspection never clears it.
        r.execute(
            "DELETE FROM desk_intake_gap WHERE domain=?1 AND source=?2",
            params![m.domain, SOURCE],
        )
        .unwrap();
        r.execute(
            "DELETE FROM desk_intake_loss WHERE domain=?1 AND source=?2",
            params![m.domain, SOURCE],
        )
        .unwrap();
        assert!(inspect_now(&r, &m).history_proved);
    }
}

#[test]
fn negative_upper_unknown_transitive_missing_and_read_only_census() {
    let t = tempfile::tempdir().unwrap();
    let r = registry(&t.path().join("registry.sqlite"));
    let m = manifest();
    let path = t.path().join("survey");
    let h = Store::open(&path).unwrap();
    source::bind(h.connection(), SOURCE).unwrap();
    r.execute(
        "INSERT INTO ships VALUES('survey',?1,NULL)",
        [path.to_string_lossy().as_ref()],
    )
    .unwrap();
    enrollment::begin(
        h.connection(),
        &format!("{}:{SOURCE}", binding(&m).unwrap()),
    )
    .unwrap();
    let before = h.connection().total_changes();
    let p = inspect_now(&r, &m);
    assert_eq!(p.homes[0].state, "unpublished");
    assert_eq!(p.homes[0].baseline_upper, None);
    assert!(!p.history_proved);
    assert_eq!(h.connection().total_changes(), before);
    enrollment::deactivate(h.connection()).unwrap();
    super::baseline::enroll_home(
        &path,
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        100,
        || Ok(()),
    )
    .unwrap();
    capture(&path, &h, &r, &m);
    assert!(inspect_now(&r, &m).history_proved);
    save(&r, &m, SOURCE, Instant::now() + Duration::from_secs(10)).unwrap();
    let readonly = Connection::open_with_flags(
        t.path().join("registry.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert!(inspect_now(&readonly, &m).history_proved);
    let writer = Connection::open(t.path().join("registry.sqlite")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    // A quiet save and a fully read-only query require no registry writer lock.
    save(&r, &m, SOURCE, Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(inspect_now(&r, &m).history_proved);
    writer.execute_batch("ROLLBACK").unwrap();
    r.execute(
        "INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','unknown','fleet')",
        [],
    )
    .unwrap();
    assert!(!inspect_now(&r, &m).membership_proved);
    r.execute("DELETE FROM machine_ships", []).unwrap();
    r.execute(
        "INSERT INTO desk_home_inventory VALUES(?1)",
        [t.path().join("missing").to_string_lossy().as_ref()],
    )
    .unwrap();
    let missing = inspect_now(&r, &m);
    assert!(!missing.history_proved);
    assert!(missing.homes.iter().any(|h| h.state == "missing"));
}
#[test]
fn unavailable_retained_home_keeps_cached_unread_and_open_facts() {
    let t = tempfile::tempdir().unwrap();
    let r = registry(&t.path().join("registry.sqlite"));
    let m = manifest();
    let path = t.path().join("survey");
    let mut h = Store::open(&path).unwrap();
    source::bind(h.connection(), SOURCE).unwrap();
    crate::mail::dispatch(
        &mut h,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    crate::mail::dispatch(&mut h, "mail.send", json!({"id":"retained-open","from":"survey","to":"captain","kind":"request","body":"Retain this request"})).unwrap();
    r.execute(
        "INSERT INTO ships VALUES('survey',?1,NULL)",
        [path.to_string_lossy().as_ref()],
    )
    .unwrap();
    super::baseline::enroll_home(
        &path,
        &r,
        &m,
        SOURCE,
        Instant::now() + Duration::from_secs(10),
        100,
        || Ok(()),
    )
    .unwrap();
    capture(&path, &h, &r, &m);
    save(&r, &m, SOURCE, Instant::now() + Duration::from_secs(10)).unwrap();
    drop(h);
    std::fs::rename(&path, t.path().join("retained-backup")).unwrap();
    save(&r, &m, SOURCE, Instant::now() + Duration::from_secs(10)).unwrap();
    let raw: String = r
        .query_row("SELECT proof FROM desk_source_reconciliation", [], |r| {
            r.get(0)
        })
        .unwrap();
    let p: Proof = serde_json::from_str(&raw).unwrap();
    assert!(!p.history_proved);
    assert_eq!(p.homes[0].state, "missing");
    assert!(p.homes[0].facts_cached);
    assert_eq!((p.homes[0].unread, p.homes[0].open), (Some(1), Some(1)));
}

#[test]
fn failed_attempt_discards_only_overflow_not_a_new_provenance_conflict() {
    for fallback in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let mut h = Store::open(t.path()).unwrap();
        source::bind(h.connection(), SOURCE).unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        crate::mail::dispatch(
            &mut h,
            "mail.send",
            json!({"id":"conflict","from":"survey","to":"captain","kind":"notice","body":"Report"}),
        )
        .unwrap();
        enrollment::begin(h.connection(), "binding").unwrap();
        if fallback {
            h.connection().execute_batch("CREATE TRIGGER refuse_loss BEFORE UPDATE OF lost ON mail_source_health BEGIN SELECT RAISE(ABORT,'health unavailable'); END;").unwrap();
        }
        source::observe(
            h.connection(),
            "conflict",
            Some(&source::Provenance::Unresolved),
        );
        assert!(source::lost(h.connection()).unwrap());
        enrollment::deactivate(h.connection()).unwrap();
        assert!(
            source::lost(h.connection()).unwrap(),
            "conflict during attempt was erased"
        );
        assert_eq!(
            h.connection()
                .query_row("SELECT COUNT(*) FROM mail", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
