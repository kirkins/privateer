use super::*;

#[test]
fn yield_write_failure_preserves_the_original_rebuild_error() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.send("pending");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    f.home
        .connection()
        .execute_batch("ALTER TABLE mail_source_origin RENAME TO saved_origin")
        .unwrap();
    f.source.execute_batch("CREATE TRIGGER fail_yield BEFORE UPDATE ON desk_replay_cut_return BEGIN SELECT RAISE(ABORT,'yield failed'); END;").unwrap();
    let error = step(&f.source, &f.m, ALPHA, deadline()).unwrap_err();
    assert!(
        error.to_string().contains("mail_source_origin"),
        "{error:#}"
    );
    assert!(!error.to_string().contains("yield failed"));
    assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='replay.rebuild_yield' AND body LIKE '%yield failed%')", [], |r| r.get::<_,bool>(0)).unwrap());
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    assert_eq!(count(&f.source, "desk_replay_omission"), 0);
}

#[test]
fn cooperative_deadline_commits_the_completed_prefix_and_retries_the_suffix() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    for id in ["first", "second", "third"] {
        f.send(id);
    }
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let now = Instant::now();
    let quantum = std::time::Duration::from_secs(3600);
    {
        let _clock = crate::mail::source::SlowClock::start(now, quantum);
        assert_eq!(step(&f.source, &f.m, ALPHA, now + quantum).unwrap(), 1);
    }
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
    let (cursor, first): (i64, i64) = f
        .source
        .query_row(
            "SELECT cursor,MIN(journal_order) FROM desk_replay_cut_return,desk_replay_import",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cursor, first);
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 2);
    assert_eq!(count(&retry, "desk_replay_witness"), 3);
    assert_eq!(count(&retry, "desk_replay_omission"), 0);
}

#[test]
fn republished_enrollment_records_a_lasting_unavailable_range_for_the_old_cut() {
    use crate::mail::source;
    let mut f = fixture(false);
    let v = returned(&f);
    deliver(&f, &v).unwrap();
    f.send("old-enrollment");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
    f.home
        .connection()
        .execute("DELETE FROM mail_source_snapshot", [])
        .unwrap();
    source::enrollment::begin(f.home.connection(), &bound).unwrap();
    let path = f.dir.path().join("alpha");
    let baseline = source::enrollment::open(&path, false).unwrap();
    source::enrollment::freeze(f.home.connection(), &baseline, &bound, deadline()).unwrap();
    source::enrollment::publish(f.home.connection(), &baseline, &bound).unwrap();
    let nonce = source::enrollment::published_nonce(f.home.connection(), &bound)
        .unwrap()
        .unwrap();
    assert_ne!(nonce, v["receipts"][0]["home"]["nonce"].as_str().unwrap());
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 1);
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 0);
    assert!(retry.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_omission o JOIN desk_replay_import i ON o.domain=i.domain AND o.source=i.source AND o.binding=i.binding AND i.journal_order BETWEEN o.first_order AND o.last_order WHERE o.reason='rebuild_unavailable')", [], |r| r.get::<_,bool>(0)).unwrap());
    assert_eq!(count(&retry, "desk_replay_witness"), 0);
    assert_eq!(
        count(&retry, "desk_replay_import"),
        1,
        "custody is not evicted"
    );
}

#[test]
fn repeated_failure_count_is_durable_bounded_disclosed_and_resets_on_success() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.send("blocked");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    f.home
        .connection()
        .execute_batch("ALTER TABLE mail_source_origin RENAME TO saved_origin")
        .unwrap();
    let before: i64 = f
        .source
        .query_row("SELECT cursor FROM desk_replay_cut_return", [], |r| {
            r.get(0)
        })
        .unwrap();
    let omissions = count(&f.source, "desk_replay_omission");
    for expected in 1..=3 {
        assert!(step(&f.source, &f.m, ALPHA, deadline()).is_err());
        let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
        let diag = diagnostics(&retry, &f.m, ALPHA).unwrap();
        assert_eq!(diag["state"], "retry_pending");
        assert_eq!(diag["cuts"][0]["consecutive_failures"], expected);
        assert_eq!(diag["cuts"][0]["through"], before);
        // The operator status fallback must disclose it even when source
        // barrier evaluation fails, without inventing source completeness.
        let status = crate::machines::desk_intake::completion::status(&retry, &f.m);
        let alpha = status.iter().find(|s| s["id"] == ALPHA).unwrap();
        assert_eq!(alpha["replay_rebuild"], diag);
    }
    f.source
        .execute(
            "UPDATE desk_replay_cut_return SET failures=?1",
            [FAILURE_CAP - 1],
        )
        .unwrap();
    for _ in 0..2 {
        assert!(step(&f.source, &f.m, ALPHA, deadline()).is_err());
        assert_eq!(
            diagnostics(&f.source, &f.m, ALPHA).unwrap()["cuts"][0]["consecutive_failures"],
            FAILURE_CAP
        );
    }
    assert_eq!(count(&f.source, "desk_replay_omission"), omissions);
    assert_eq!(count(&f.source, "desk_replay_import"), 1);
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    f.home
        .connection()
        .execute_batch("ALTER TABLE saved_origin RENAME TO mail_source_origin")
        .unwrap();
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 1);
    let diag = diagnostics(&f.source, &f.m, ALPHA).unwrap();
    assert_eq!(diag["state"], "no_recorded_failures");
    assert_eq!(diag["cuts"], json!([]));
    assert_eq!(count(&f.source, "desk_replay_omission"), omissions);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
}

#[test]
fn failure_diagnostics_window_is_bounded_and_upgrade_preserves_cut_custody() {
    let f = fixture(false);
    let v = returned(&f);
    deliver(&f, &v).unwrap();
    let body: String = f
        .source
        .query_row("SELECT body FROM desk_replay_cut_return", [], |r| r.get(0))
        .unwrap();
    // Populated B2 shape: the only new column can be removed to exercise a
    // real additive migration, readiness and idempotent restart.
    f.source.execute_batch("DROP INDEX desk_replay_cut_failures; ALTER TABLE desk_replay_cut_return DROP COLUMN failures;").unwrap();
    assert!(!super::super::super::ready(&f.source).unwrap());
    crate::machines::init(&f.source).unwrap();
    assert!(super::super::super::ready(&f.source).unwrap());
    let kept: String = f
        .source
        .query_row("SELECT body FROM desk_replay_cut_return", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, kept);
    let changes = f.source.total_changes();
    crate::machines::init(&f.source).unwrap();
    assert_eq!(changes, f.source.total_changes());
    let r: Receipt = serde_json::from_value(v["receipts"][0].clone()).unwrap();
    f.source.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<60)
        INSERT INTO desk_replay_cut_return(domain,source,binding,exposure,home,mailbox,nonce,body,cursor,failures)
        SELECT ?1,?2,?3,CAST(x AS TEXT),'filler','filler','filler','{}',0,1 FROM n", params![f.m.domain,ALPHA,r.binding]).unwrap();
    let diag = diagnostics(&f.source, &f.m, ALPHA).unwrap();
    assert_eq!(diag["cuts"].as_array().unwrap().len(), DIAGNOSTIC_CUTS);
    assert_eq!(diag["truncated"], true);
    assert_eq!(count(&f.source, "desk_replay_cut_return"), 61);
}
