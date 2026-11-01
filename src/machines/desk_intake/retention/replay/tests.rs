use super::*;
use crate::{mail::source, store::Store};
use std::time::{Duration, Instant};
mod review_tests;
mod round3_tests;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
const BETA: &str = "00000000-0000-4000-8000-00000000000b";

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60)
}

struct Fixture {
    dir: tempfile::TempDir,
    h: Store,
    r: Connection,
    m: Manifest,
}
impl Fixture {
    fn new(old: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Store::open(&dir.path().join("alpha")).unwrap();
        source::bind(h.connection(), ALPHA).unwrap();
        source::init_intake(h.connection()).unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        let m = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,"owner_core":BETA,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"push"},{"core":BETA,"name":"beta","ships":["relay"],"intake_direction":"local"}]})).unwrap();
        let r = Connection::open(dir.path().join("registry.sqlite")).unwrap();
        super::super::super::init(&r).unwrap();
        let mut f = Self { dir, h, r, m };
        if old {
            f.send("old");
        }
        let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
        source::enrollment::begin(f.h.connection(), &bound).unwrap();
        let baseline = source::enrollment::open(&f.dir.path().join("alpha"), true).unwrap();
        source::init(&baseline).unwrap();
        source::init_intake(&baseline).unwrap();
        let upper =
            source::enrollment::freeze(f.h.connection(), &baseline, &bound, deadline()).unwrap();
        source::enrollment::publish(f.h.connection(), &baseline, &bound).unwrap();
        assert!(upper >= 0);
        f.capture();
        f
    }
    fn send(&mut self, id: &str) {
        crate::mail::dispatch(
            &mut self.h,
            "mail.send",
            json!({"id":id,"from":"survey","to":"captain","kind":"request","body":"content"}),
        )
        .unwrap();
    }
    fn capture(&self) -> Value {
        super::super::super::home::capture_ready_until(
            &self.dir.path().join("alpha"),
            self.h.connection(),
            &self.r,
            &self.m,
            ALPHA,
            deadline(),
        )
        .unwrap()
    }
    fn witness(&self) -> Witness {
        let body: String = self
            .r
            .query_row(
                "SELECT body FROM desk_replay_witness ORDER BY intent LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&body).unwrap()
    }
    fn cut(&self, w: &Witness) -> Cut {
        Cut {
            checkpoint_digest: "proved-cut".into(),
            binding: w.binding.clone(),
            source: w.source.clone(),
            home: w.home.clone(),
            mailbox: w.mailbox.clone(),
            nonce: w.nonce.clone(),
            journal_upper: w.journal_order - 1,
            delta_upper: w.delta_order - 1,
            mail_upper: w.mail_order - 1,
        }
    }
}

#[test]
fn native_witness_binds_exact_import_not_lifecycle_or_import_sequence() {
    let mut f = Fixture::new(true);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    f.send("new");
    let result = f.capture();
    assert!(result.get("desk_intake_replay").is_none());
    assert!(result.get("desk_intake_replay_proof").is_none());
    let w = f.witness();
    assert_eq!(w.origin.id, "new");
    let (journal, d, m): (i64, i64, i64) =
        f.r.query_row(
            "SELECT journal_order,delta_order,mail_order FROM desk_replay_import WHERE intent=?1",
            [&w.intent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (w.journal_order, w.delta_order, w.mail_order),
        (journal, d, m)
    );
    let cut = f.cut(&w);
    assert!(w.beyond(&cut));
    for field in [
        "journal", "delta", "mail", "nonce", "source", "home", "mailbox", "binding",
    ] {
        let mut bad = cut.clone();
        match field {
            "journal" => bad.journal_upper = w.journal_order,
            "delta" => bad.delta_upper = w.delta_order,
            "mail" => bad.mail_upper = w.mail_order,
            "nonce" => bad.nonce = "other".into(),
            "source" => bad.source = BETA.into(),
            "home" => bad.home = "other".into(),
            "mailbox" => bad.mailbox = "other".into(),
            _ => bad.binding = "other".into(),
        }
        assert!(!w.beyond(&bad), "{field}");
    }
    // An old native request's later closure has a new D but not a new M.
    crate::mail::dispatch(
        &mut f.h,
        "mail.withdraw",
        json!({"id":"old","actor":"survey","reason":"done"}),
    )
    .unwrap();
    f.capture();
    // Withdrawal also creates a distinct native notification. Its witness
    // is legitimate; only the original request must remain history evidence.
    let old_witnesses: i64 =
        f.r.query_row(
            "SELECT COUNT(*) FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_witnesses, 0);
    let (first_order, lane): (i64, String) = f.r.query_row(
        "SELECT first_order,lane FROM desk_replay_anchor WHERE json_extract(origin,'$.id')='old'",
        [], |r| Ok((r.get(0)?,r.get(1)?)),
    ).unwrap();
    assert_eq!((first_order, lane), (1, "baseline".into()));
    let later: i64 =
        f.r.query_row(
            "SELECT COUNT(*) FROM desk_replay_import WHERE json_extract(origin,'$.id')='old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(later, 0, "lifecycle updates need no rebuild details");
}

#[test]
fn pressure_keeps_custody_and_same_cut_rebuild_requires_present_unchanged_home() {
    let mut f = Fixture::new(false);
    f.send("retained");
    f.capture();
    let retained = f.witness();
    f.r.execute(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_witness SELECT 'filler','filler',CAST(x AS TEXT),'{}',0 FROM n",
        [WITNESSES - 1],
    )
    .unwrap();
    f.send("waiting");
    f.capture();
    let intent: String =
        f.r.query_row(
            "SELECT intent FROM desk_replay_import WHERE json_extract(origin,'$.id')='waiting'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let w = candidate(&f.r, &f.m, ALPHA, &intent).unwrap().unwrap();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), WITNESSES);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 1);
    assert_eq!(cleanup(&f.r, &f.m, ALPHA).unwrap(), 0);
    let saved: String =
        f.r.query_row(
            "SELECT body FROM desk_replay_witness WHERE domain=?1 AND source=?2 AND intent=?3",
            params![f.m.domain, ALPHA, retained.intent],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<Witness>(&saved).unwrap(), retained);
    // Only explicit adoption releases existing custody. A page receipt cannot.
    adopt(&f.r, &f.m, &retained).unwrap();
    assert_eq!(cleanup(&f.r, &f.m, ALPHA).unwrap(), 1);
    assert_eq!(count(&f.r, "desk_replay_anchor").unwrap(), 1);
    let cut = f.cut(&w);
    let read = f.h.connection().unchecked_transaction().unwrap();
    assert!(
        rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            &read,
            &f.m,
            ALPHA,
            &intent,
            &cut
        )
        .unwrap()
    );
    drop(read);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), WITNESSES);
    // Persisted omission ranges remain disclosed even after successful rebuild.
    let range: (i64, i64) =
        f.r.query_row(
            "SELECT first_order,last_order FROM desk_replay_omission",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(range, (w.journal_order, w.journal_order));
    f.r.execute(
        "DELETE FROM desk_replay_witness WHERE domain='filler' AND intent='1'",
        [],
    )
    .unwrap();
    f.h.connection()
        .execute("DELETE FROM mail WHERE id='waiting'", [])
        .unwrap();
    let read = f.h.connection().unchecked_transaction().unwrap();
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            &read,
            &f.m,
            ALPHA,
            &intent,
            &cut
        )
        .unwrap()
    );
    drop(read);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 2);
    let reason: String =
        f.r.query_row(
            "SELECT reason FROM desk_replay_omission WHERE reason='rebuild_unavailable'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "rebuild_unavailable");
}

#[test]
fn rebuild_rejects_substituted_cut_regression_and_changed_pinned_content() {
    let mut f = Fixture::new(false);
    f.send("new");
    f.capture();
    let w = f.witness();
    let mut cut = f.cut(&w);
    cut.mail_upper = w.mail_order;
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &cut
        )
        .unwrap()
    );
    let replaced = f.cut(&w);
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &replaced
        )
        .unwrap(),
        "cannot substitute an older cut after pinning"
    );
    // Keep the original pinned cut for the remaining current-home checks.
    cut.mail_upper = w.mail_order - 1;
    f.r.execute("UPDATE desk_replay_import SET rebuild_cut=NULL", [])
        .unwrap();
    f.h.connection()
        .execute(
            "UPDATE mail_source_origin SET provenance='\"Unresolved\"' WHERE id='new'",
            [],
        )
        .unwrap();
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &cut
        )
        .unwrap()
    );
    f.h.connection()
        .execute(
            "UPDATE mail_source_origin SET provenance=?1 WHERE id='new'",
            [serde_json::to_string(&Provenance::Origin(w.origin.clone())).unwrap()],
        )
        .unwrap();
    f.h.connection()
        .execute("UPDATE mail SET body='changed' WHERE id='new'", [])
        .unwrap();
    assert!(
        !rebuild(
            &f.dir.path().join("alpha"),
            &f.r,
            f.h.connection(),
            &f.m,
            ALPHA,
            &w.intent,
            &cut
        )
        .unwrap()
    );
}

#[test]
fn mail_high_water_survives_pruning_and_rejects_unsafe_schemas() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE mail(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT);")
        .unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), Some(0));
    c.execute_batch("INSERT INTO mail(id) VALUES('old'); DELETE FROM mail;")
        .unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), Some(1));
    c.execute_batch(
        "INSERT INTO mail(id) VALUES('new'); DELETE FROM sqlite_sequence WHERE name='mail';",
    )
    .unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), None);
    c.execute_batch("DROP TABLE mail; CREATE TABLE mail(seq INTEGER PRIMARY KEY,id TEXT); INSERT INTO mail VALUES(1,'legacy');").unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), None);
    c.execute_batch("DELETE FROM mail;").unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), None);
    c.execute_batch(
        "DROP TABLE mail; CREATE TABLE mail(seq INTEGER PRIMARY KEY, \"autoincrement\" TEXT);",
    )
    .unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), None);
    c.execute_batch("INSERT INTO mail VALUES(1,'not a sequence');")
        .unwrap();
    assert_eq!(snapshot::mail_high_water(&c).unwrap(), None);
}

#[test]
fn no_witness_with_missing_mail_sequence_or_recreated_id() {
    let mut f = Fixture::new(false);
    f.send("first");
    f.h.connection()
        .execute("DELETE FROM sqlite_sequence WHERE name='mail'", [])
        .unwrap();
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(
        count(&f.r, "desk_source_event").unwrap(),
        1,
        "ordinary intake continues"
    );
    // Re-created IDs cannot replace retained original insertion/content anchors.
    f.h.connection()
        .execute("DELETE FROM mail WHERE id='first'", [])
        .unwrap();
    f.send("first");
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_anchor").unwrap(), 1);
}

#[test]
fn baseline_and_delta_orders_stay_exact_and_conflicting_intent_is_atomic() {
    let mut f = Fixture::new(true);
    f.send("new");
    f.capture();
    let w = f.witness();
    let rows: Vec<(i64, String)> =
        f.r.prepare("SELECT journal_order,lane FROM desk_replay_import ORDER BY journal_order")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    assert_eq!(rows, vec![(2, "delta".into())]);
    let (intent, event): (String, String) =
        f.h.connection()
            .query_row(
                "SELECT intent,event FROM mail_source_import WHERE intent=?1",
                [&w.intent],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
    let mut bad: HomeFact = serde_json::from_str(&event).unwrap();
    bad.content["body"] = json!("conflict");
    let tx = f.r.unchecked_transaction().unwrap();
    assert!(
        record(
            &tx,
            &f.m,
            ALPHA,
            &intent,
            &serde_json::to_string(&bad).unwrap(),
            w.journal_order,
            None
        )
        .is_err()
    );
    drop(tx);
    assert_eq!(f.witness(), w);
    // An append/ledger failure before receipt must roll back all registry work.
    f.send("retry");
    f.r.execute_batch("CREATE TRIGGER fail_replay BEFORE INSERT ON desk_replay_import BEGIN SELECT RAISE(ABORT,'crash'); END;").unwrap();
    let result = super::super::super::home::capture_ready_until(
        &f.dir.path().join("alpha"),
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        deadline(),
    );
    assert!(result.is_err());
    assert_eq!(count(&f.r, "desk_source_event").unwrap(), 2);
    f.r.execute_batch("DROP TRIGGER fail_replay").unwrap();
    f.capture();
    assert_eq!(count(&f.r, "desk_source_event").unwrap(), 3);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 2);
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 3);
}

#[test]
fn registry_commit_before_home_receipt_retries_without_new_or_retroactive_witness() {
    let mut f = Fixture::new(false);
    f.send("retry");
    f.h.connection().execute_batch("CREATE TRIGGER stop_receipt BEFORE UPDATE OF receipt ON mail_source_import BEGIN SELECT RAISE(ABORT,'home crash'); END;").unwrap();
    let outcome = super::super::super::home::capture_ready_until(
        &f.dir.path().join("alpha"),
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        deadline(),
    );
    assert!(outcome.is_err());
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 1);
    let w = f.witness();
    drop(f.r);
    f.r = Connection::open(f.dir.path().join("registry.sqlite")).unwrap();
    super::super::super::init(&f.r).unwrap();
    f.h.connection()
        .execute_batch("DROP TRIGGER stop_receipt")
        .unwrap();
    f.capture();
    assert_eq!(f.witness(), w);
    assert_eq!(count(&f.r, "desk_source_event").unwrap(), 1);
    // Simulate a legacy committed receipt: retry cannot retroactively prove it.
    f.r.execute("DELETE FROM desk_replay_witness", []).unwrap();
    f.r.execute("DELETE FROM desk_replay_import", []).unwrap();
    f.h.connection()
        .execute("UPDATE mail_source_import SET receipt=NULL", [])
        .unwrap();
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
}

#[test]
fn non_autoincrement_mail_fixture_and_copy_provenance_never_produce_witness() {
    for mode in ["legacy_schema", "copy"] {
        let mut f = Fixture::new(false);
        f.send("not_proved");
        if mode == "legacy_schema" {
            f.h.connection()
                .execute_batch(
                    "ALTER TABLE mail RENAME TO legacy_mail;
                CREATE TABLE mail AS SELECT * FROM legacy_mail;",
                )
                .unwrap();
        } else {
            let (intent, event): (String, String) =
                f.h.connection()
                    .query_row(
                        "SELECT intent,event FROM mail_source_outbox ORDER BY seq DESC LIMIT 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
            let mut fact: HomeFact = serde_json::from_str(&event).unwrap();
            let Provenance::Origin(origin) = fact.provenance else {
                panic!("native fixture");
            };
            // Copy to the SAME source identity can flatten to Fact.copy=None.
            // Provenance, not that flattened field, must control native proof.
            fact.provenance = Provenance::Copy(origin);
            f.h.connection()
                .execute(
                    "UPDATE mail_source_outbox SET event=?1 WHERE intent=?2",
                    params![serde_json::to_string(&fact).unwrap(), intent],
                )
                .unwrap();
            f.h.connection()
                .execute(
                    "UPDATE mail_source_origin SET provenance=?1 WHERE id='not_proved'",
                    [serde_json::to_string(&fact.provenance).unwrap()],
                )
                .unwrap();
        }
        f.capture();
        assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0, "{mode}");
        assert_eq!(count(&f.r, "desk_source_event").unwrap(), 1, "{mode}");
    }
}

#[test]
fn insert_before_cut_observed_later_is_not_beyond_mail_high_water() {
    let mut f = Fixture::new(false);
    f.send("queued");
    let upper = snapshot::mail_high_water(f.h.connection())
        .unwrap()
        .unwrap();
    f.capture();
    let w = f.witness();
    let mut cut = f.cut(&w);
    cut.mail_upper = upper;
    assert!(!w.beyond(&cut));
}

#[test]
fn interleaved_baseline_and_delta_ledger_never_claims_a_min_max_baseline_span() {
    let mut f = Fixture::new(true);
    f.send("delta-one");
    f.capture();
    let baseline = source::enrollment::open(&f.dir.path().join("alpha"), false).unwrap();
    let event: String = baseline
        .query_row("SELECT event FROM mail_source_import LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let h: HomeFact = serde_json::from_str(&event).unwrap();
    let Provenance::Origin(origin) = h.provenance else {
        panic!("native baseline");
    };
    let fact = Fact {
        origin,
        copy: None,
        content: h.content,
        state: h.state,
        unread: h.unread,
        pending: h.pending,
    };
    let intent = uuid::Uuid::new_v4().to_string();
    let o = observe(
        &f.dir.path().join("alpha"),
        &baseline,
        &f.m,
        ALPHA,
        &baseline
            .query_row::<String, _, _>("SELECT intent FROM mail_source_import LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap(),
        &event,
    );
    // A receipted baseline is not re-observed to synthesize new proof.
    assert!(o.is_err());
    let o = Observation {
        home: crate::core::hash(&json!(f.dir.path().join("alpha"))),
        mailbox: fact.origin.mailbox.clone(),
        nonce: source::enrollment::published_nonce(
            f.h.connection(),
            &format!("{}:{ALPHA}", binding(&f.m).unwrap()),
        )
        .unwrap(),
        lane: "baseline".into(),
        delta_order: None,
        mail_order: None,
        native: false,
    };
    let tx = f.r.unchecked_transaction().unwrap();
    let order = append_in(&tx, &f.m, ALPHA, &intent, &fact).unwrap();
    record(&tx, &f.m, ALPHA, &intent, &event, order, Some(&o)).unwrap();
    tx.commit().unwrap();
    f.send("delta-two");
    f.capture();
    let rows: Vec<(i64, String)> =
        f.r.prepare("SELECT journal_order,lane FROM desk_replay_import ORDER BY journal_order")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
    assert_eq!(rows, vec![(2, "delta".into()), (4, "delta".into())]);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 2);
}

#[test]
fn detail_budget_does_not_evict_witnesses_or_block_imports() {
    let mut f = Fixture::new(false);
    f.r.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_import(domain,source,binding,intent,home,mailbox,lane,journal_order,origin,digest,event_digest,native)
        SELECT 'filler','filler','filler',CAST(x AS TEXT),'home','mailbox','unknown',x,'{}','digest','digest',0 FROM n",[DETAILS]).unwrap();
    f.send("overflow");
    f.capture();
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), DETAILS);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 1);
    let (a, b): (i64, i64) =
        f.r.query_row(
            "SELECT first_order,last_order FROM desk_replay_omission",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((a, b), (1, 1));
}
