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
