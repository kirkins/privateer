use super::*;

#[test]
fn interrupted_home_item_rolls_back_its_pin_but_keeps_the_completed_prefix() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    for id in ["first", "second", "third"] {
        f.send(id);
    }
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let mut attempts = 0;
    let visited = step_with(&f.source, &f.m, ALPHA, deadline(), |item| {
        attempts += 1;
        if attempts == 2 {
            let home = item.home;
            // A real SQLite VM interrupt inside native_now, after rebuild's
            // tentative registry cut pin, not a pre-loop clock check.
            home.progress_handler(1, Some(|| true));
            let error = item.run().unwrap_err();
            home.progress_handler(0, None::<fn() -> bool>);
            assert!(cooperative_retry(&error), "{error:#}");
            Err(error)
        } else {
            item.run()
        }
    })
    .unwrap();
    assert_eq!(visited, 2);
    assert_eq!(attempts, 2);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
    let (cursor,first,failures):(i64,i64,i64)=f.source.query_row("SELECT cursor,MIN(journal_order),failures FROM desk_replay_cut_return,desk_replay_import",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(cursor, first);
    assert_eq!(failures, 0, "an interrupt is a yield, not a failed home");
    let pins: Vec<Option<String>> = f
        .source
        .prepare("SELECT rebuild_cut FROM desk_replay_import ORDER BY journal_order")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(pins[0].is_some());
    assert!(pins[1].is_none());
    assert!(pins[2].is_none());
    assert_eq!(count(&f.source, "desk_replay_omission"), 0);
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 2);
    assert_eq!(count(&retry, "desk_replay_witness"), 3);
}

#[test]
fn contended_home_yields_without_counting_a_failure_or_losing_custody() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.send("pending");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let cursor: i64 = f
        .source
        .query_row("SELECT cursor FROM desk_replay_cut_return", [], |r| {
            r.get(0)
        })
        .unwrap();
    // Rollback-journal locking makes a read conflict deterministic; WAL
    // writers normally coexist with readers and would not prove contention.
    f.home
        .connection()
        .execute_batch("PRAGMA journal_mode=DELETE")
        .unwrap();
    let lock = Connection::open(f.dir.path().join("alpha/privateer.sqlite")).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(step(&f.source, &f.m, ALPHA, deadline()).is_ok());
    let (after, failures): (i64, i64) = f
        .source
        .query_row(
            "SELECT cursor,failures FROM desk_replay_cut_return",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(after, cursor);
    assert_eq!(failures, 0);
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    assert_eq!(count(&f.source, "desk_replay_import"), 1);
    assert_eq!(count(&f.source, "desk_replay_omission"), 0);
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 1);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
}

#[test]
fn no_progress_yields_preserve_prior_hard_failures() {
    for mode in ["interrupt", "deadline", "witness_pressure", "quiet"] {
        let mut f = fixture(false);
        deliver(&f, &returned(&f)).unwrap();
        if mode != "quiet" {
            f.send("pending");
            f.source
                .execute("DELETE FROM desk_replay_witness", [])
                .unwrap();
        }
        if mode == "witness_pressure" {
            f.source
                .execute(
                    "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
                     INSERT INTO desk_replay_witness SELECT 'filler','filler',CAST(x AS TEXT),'{}',0 FROM n",
                    [WITNESSES],
                )
                .unwrap();
        }
        f.source
            .execute("UPDATE desk_replay_cut_return SET failures=3", [])
            .unwrap();
        let cursor: i64 = f
            .source
            .query_row("SELECT cursor FROM desk_replay_cut_return", [], |r| {
                r.get(0)
            })
            .unwrap();
        let until = if mode == "deadline" {
            crate::mail::source::work_now()
        } else {
            deadline()
        };
        step_with(&f.source, &f.m, ALPHA, until, |item| {
            assert_eq!(mode, "interrupt", "other yields never run an item");
            let home = item.home;
            home.progress_handler(1, Some(|| true));
            let error = item.run().unwrap_err();
            home.progress_handler(0, None::<fn() -> bool>);
            assert!(cooperative_retry(&error), "{error:#}");
            Err(error)
        })
        .unwrap();
        let (after, failures): (i64, i64) = f
            .source
            .query_row(
                "SELECT cursor,failures FROM desk_replay_cut_return",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, cursor, "{mode}");
        assert_eq!(failures, 3, "{mode}");
        assert_eq!(
            count(&f.source, "desk_replay_witness"),
            if mode == "witness_pressure" {
                WITNESSES
            } else {
                0
            },
            "{mode}"
        );
    }
}

#[test]
fn hard_item_failure_commits_prefix_and_failure_together() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    for id in ["first", "second", "third"] {
        f.send(id);
    }
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    f.source
        .execute("UPDATE desk_replay_cut_return SET failures=2", [])
        .unwrap();
    let first: i64 = f
        .source
        .query_row(
            "SELECT MIN(journal_order) FROM desk_replay_import",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut attempts = 0;
    let error = step_with(&f.source, &f.m, ALPHA, deadline(), |item| {
        attempts += 1;
        let result = item.run()?;
        if attempts == 2 {
            anyhow::bail!("persistent second-item failure");
        }
        Ok(result)
    })
    .unwrap_err();
    assert!(error.to_string().contains("persistent second-item failure"));
    assert_eq!(attempts, 2);
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    for failures in [3, 4] {
        let state: (i64, i64) = retry
            .query_row(
                "SELECT cursor,failures FROM desk_replay_cut_return",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, (first, failures));
        assert_eq!(count(&retry, "desk_replay_witness"), 1);
        assert_eq!(count(&retry, "desk_replay_omission"), 0);
        let pins: Vec<Option<String>> = retry
            .prepare("SELECT rebuild_cut FROM desk_replay_import ORDER BY journal_order")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(pins[0].is_some());
        assert!(pins[1..].iter().all(Option::is_none));
        if failures == 3 {
            step_with(&retry, &f.m, ALPHA, deadline(), |item| {
                item.run()?;
                anyhow::bail!("persistent second-item failure")
            })
            .unwrap_err();
        }
    }
    assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 2);
    assert_eq!(count(&retry, "desk_replay_witness"), 3);
    let failures: i64 = retry
        .query_row("SELECT failures FROM desk_replay_cut_return", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(failures, 0, "only actual forward progress clears failures");
}

#[test]
fn prefix_commit_failure_rolls_back_the_prefix_and_preserves_the_item_error() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    for id in ["first", "second"] {
        f.send(id);
    }
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let before: (i64, i64) = f
        .source
        .query_row(
            "SELECT cursor,failures FROM desk_replay_cut_return",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    f.source.execute_batch("CREATE TRIGGER fail_prefix BEFORE UPDATE ON desk_replay_cut_return BEGIN SELECT RAISE(ABORT,'prefix commit failed'); END;").unwrap();
    let mut attempts = 0;
    let error = step_with(&f.source, &f.m, ALPHA, deadline(), |item| {
        attempts += 1;
        let rebuilt = item.run()?;
        if attempts == 2 {
            anyhow::bail!("primary item failure");
        }
        Ok(rebuilt)
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "primary item failure");
    assert_eq!(attempts, 2);
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    let after: (i64, i64) = retry
        .query_row(
            "SELECT cursor,failures FROM desk_replay_cut_return",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(after, before, "prefix and failure write roll back together");
    assert_eq!(count(&retry, "desk_replay_witness"), 0);
    assert!(
        !retry
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM desk_replay_import WHERE rebuild_cut IS NOT NULL)",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    assert!(retry.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='replay.rebuild_yield' AND body LIKE '%prefix commit failed%')", [], |r| r.get::<_,bool>(0)).unwrap());
    retry.execute_batch("DROP TRIGGER fail_prefix").unwrap();
    assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 2);
    assert_eq!(count(&retry, "desk_replay_witness"), 2);
}

#[test]
fn conflicting_cuts_are_not_reported_as_retry_pending() {
    let f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.source
        .execute(
            "UPDATE desk_replay_cut_return SET failures=3,conflict=1",
            [],
        )
        .unwrap();
    let diag = diagnostics(&f.source, &f.m, ALPHA).unwrap();
    assert_eq!(diag["state"], "no_recorded_failures");
    assert_eq!(diag["cuts"], json!([]));
    assert_eq!(
        count(&f.source, "desk_replay_cut_return"),
        1,
        "conflict custody is not evicted"
    );
}

#[test]
fn optional_rebuild_diagnostic_failure_does_not_fail_the_source_snapshot() {
    let f = fixture(false);
    let clock = transport::process_clock();
    let before = crate::machines::desk_intake::completion::snapshot(
        &f.source,
        &f.m,
        ALPHA,
        &clock,
        deadline(),
    )
    .unwrap();
    f.source
        .execute_batch(
            "ALTER TABLE desk_replay_cut_return RENAME COLUMN cursor TO unavailable_cursor",
        )
        .unwrap();
    assert!(diagnostics(&f.source, &f.m, ALPHA).is_err());
    let mut after = crate::machines::desk_intake::completion::snapshot(
        &f.source,
        &f.m,
        ALPHA,
        &clock,
        deadline(),
    )
    .unwrap();
    assert_eq!(after["replay_rebuild"], json!({"state":"unavailable"}));
    after["replay_rebuild"] = before["replay_rebuild"].clone();
    assert_eq!(
        after, before,
        "dated coverage and retained home facts remain intact"
    );
}
