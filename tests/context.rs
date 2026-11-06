use privateer::{
    dispatch,
    store::{Caller, Store},
};
use serde_json::{Value, json};
use std::fs;

fn fleet() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    dispatch(
        &mut s,
        "charter.put",
        json!({"id":"fleet","objective":"Deliver","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    for (id, owner, title) in [
        ("repair", "infra", "Repair the staging deploy pipeline"),
        ("cleanup", "infra", "Clean up old build artifacts"),
        ("feature", "apps", "Ship the billing feature"),
    ] {
        dispatch(
            &mut s,
            "task.create",
            json!({"id":id,"charter_id":"fleet","owner":owner,"title":title}),
        )
        .unwrap();
    }
    for name in ["apps", "infra"] {
        dispatch(&mut s, "ship.put", json!({"name":name,"scope":name})).unwrap();
    }
    (temp, s)
}

fn put(s: &mut Store, focus: &str, body: &str) -> anyhow::Result<Value> {
    dispatch(
        s,
        "note.put",
        json!({"ship":"infra","focus":focus,"body":body}),
    )
}

fn child(s: &mut Store, id: &str, parent: &str) {
    dispatch(
        s,
        "task.create",
        json!({"id":id,"parent":parent,"owner":"infra","title":"Bounded repair"}),
    )
    .unwrap();
}

fn read_parent(s: &mut Store) -> Value {
    dispatch(
        s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair","versions":5}),
    )
    .unwrap()
}

#[test]
fn parent_reads_show_finished_child_facts_without_rewriting_commentary() {
    let (temp, mut s) = fleet();
    child(&mut s, "index", "repair");
    child(&mut s, "grandchild", "index");
    child(&mut s, "unrelated", "cleanup");
    put(&mut s, "task:repair", "The index child is still running.").unwrap();
    put(
        &mut s,
        "thread:question",
        "The index child is still running.",
    )
    .unwrap();
    let before = read_parent(&mut s);
    let started = before["child_status"]["children"][0]["status_changed_ms"]
        .as_i64()
        .unwrap();
    // A recorded merge is evidence, not a claim inferred from prose or pins.
    s.connection()
        .execute(
            "INSERT INTO merges(id,ship,task_id,repo,pr,head_sha,record,created_ms)
         VALUES('merge-index','infra','index','example/repairs',42,'abc123',?1,?2)",
            rusqlite::params![json!({"merge_sha":"def456"}).to_string(), Store::now_ms()],
        )
        .unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"index","actor":"infra","status":"done"}),
    )
    .unwrap();
    let after = read_parent(&mut s);
    let children = after["child_status"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "only direct children: {after}");
    let finished = &children[0];
    assert_eq!(finished["id"], "index");
    assert_eq!(finished["status"], "done");
    assert_eq!(finished["status_changed_approx"], false);
    assert!(finished["status_changed_ms"].as_i64().unwrap() >= started);
    assert_eq!(finished["closing_evidence"]["task_done"], true);
    assert_eq!(finished["closing_evidence"]["merge"]["pr"], 42);
    assert_eq!(finished["closing_evidence"]["merge"]["head_sha"], "abc123");
    assert_eq!(finished["closing_evidence"]["merge"]["merge_sha"], "def456");
    assert_eq!(after["notes"], before["notes"]);
    let digest = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(digest["child_status"], after["child_status"]);
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert_eq!(
        bearings["child_statuses"]["task:repair"],
        after["child_status"]
    );
    assert!(bearings["text"].as_str().unwrap().contains("PR#42 def456"));
    // Audit retention and a restart must not discard closing facts or times.
    s.connection().execute("DELETE FROM audit", []).unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(read_parent(&mut s), after);
    let thread = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"thread:question"}),
    )
    .unwrap();
    assert_eq!(
        thread["notes"][0]["body"],
        "The index child is still running."
    );
    assert!(thread.get("child_status").is_none());
}

#[test]
fn child_status_is_bounded_newest_first_and_does_not_leak_another_owners_parent() {
    let (_temp, mut s) = fleet();
    for n in 0..23 {
        let id = format!("child-{n:02}");
        child(&mut s, &id, "repair");
        s.connection()
            .execute(
                "UPDATE tasks SET status_changed_ms=?2 WHERE id=?1",
                rusqlite::params![id, n],
            )
            .unwrap();
    }
    let read = read_parent(&mut s);
    let children = read["child_status"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 20);
    assert_eq!(children[0]["id"], "child-22");
    assert_eq!(children[19]["id"], "child-03");
    assert_eq!(read["child_status"]["more"], true);
    let foreign = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"apps","focus":"task:repair"}),
    )
    .unwrap();
    assert!(foreign["child_status"].is_null());
    let empty = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:cleanup"}),
    )
    .unwrap();
    assert_eq!(empty["child_status"], json!({"children":[],"more":false}));
}

#[test]
fn child_status_times_track_real_edges_including_scheduler_writes() {
    let (_temp, mut s) = fleet();
    child(&mut s, "index", "repair");
    s.connection()
        .execute("UPDATE tasks SET status_changed_ms=1 WHERE id='index'", [])
        .unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"index","actor":"infra","status":"open"}),
    )
    .unwrap();
    dispatch(&mut s, "task.rank", json!({"id":"index","rank":1})).unwrap();
    assert_eq!(
        read_parent(&mut s)["child_status"]["children"][0]["status_changed_ms"],
        1
    );
    // Scheduling changes status directly in the same transaction; every path
    // must capture the edge, not just task.update.
    s.connection()
        .execute("UPDATE tasks SET status='blocked' WHERE id='index'", [])
        .unwrap();
    let read = read_parent(&mut s);
    assert_eq!(read["child_status"]["children"][0]["status"], "blocked");
    assert!(
        read["child_status"]["children"][0]["status_changed_ms"]
            .as_i64()
            .unwrap()
            > 1
    );
    assert_eq!(
        read["child_status"]["children"][0]["closing_evidence"],
        json!({"task_done":false,"merge":null})
    );
}

/// One schedule row the store's readers accept, with the fields under test
/// layered on. The dispatcher's real record format, including how a fault
/// is stored, is covered in tests/scheduling.rs.
fn schedule_row(s: &mut Store, id: &str, fields: Value) {
    let mut record = json!({"task_id":id,"owner":"infra","revision":2,
        "brief":"Run the isolated fixture","state":"waiting","not_before_ms":0,
        "attempts":1,"created_ms":1,"last_checked_ms":2,"lease_until_ms":0});
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            record[key] = value;
        }
    }
    s.connection()
        .execute(
            "INSERT INTO task_schedule(task_id,record) VALUES(?1,?2)",
            rusqlite::params![id, record.to_string()],
        )
        .unwrap();
}

/// How the fault mark renders from a schedule row: a row in state 'fault'
/// marks an open or running child with the fault's reason as a JSON fact, a
/// row that left the fault or never faulted does not, and a closed child
/// never carries the mark, whatever its row still holds. A fault with no
/// reason still marks, and a long reason is capped.
#[test]
fn child_lines_render_a_live_schedule_fault_row() {
    let (temp, mut s) = fleet();
    for (id, status) in [
        ("runner", "running"),
        ("held", "blocked"),
        ("plain", "open"),
        ("closed", "done"),
        ("silent", "open"),
        ("long", "open"),
    ] {
        child(&mut s, id, "repair");
        dispatch(
            &mut s,
            "task.update",
            json!({"id":id,"actor":"infra","status":status}),
        )
        .unwrap();
    }
    schedule_row(
        &mut s,
        "runner",
        json!({"state":"fault","faulted_ms":2,
            "reason":"Coordinator ended without completion, live child work or a typed wait"}),
    );
    schedule_row(&mut s, "held", json!({}));
    schedule_row(
        &mut s,
        "closed",
        json!({"state":"fault","faulted_ms":3,
            "reason":"Execution claim expired without proven completion or a typed wait"}),
    );
    schedule_row(&mut s, "silent", json!({"state":"fault","faulted_ms":4}));
    schedule_row(
        &mut s,
        "long",
        json!({"state":"fault","faulted_ms":6,
            "reason":"Coordinator ended without completion. ".repeat(30)}),
    );
    let read = read_parent(&mut s);
    let lines = read["child_status"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_owned(), c.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        lines["runner"]["scheduling_fault"],
        json!({"reason":"Coordinator ended without completion, live child work or a typed wait",
            "faulted_ms":2})
    );
    // A fault with no reason still marks.
    assert_eq!(
        lines["silent"]["scheduling_fault"],
        json!({"reason":null,"faulted_ms":4})
    );
    let capped = lines["long"]["scheduling_fault"]["reason"]
        .as_str()
        .unwrap();
    assert_eq!(capped.chars().count(), 1000);
    assert!(capped.starts_with("Coordinator ended without completion."));
    assert_eq!(lines["held"]["scheduling_fault"], Value::Null);
    assert_eq!(lines["plain"]["scheduling_fault"], Value::Null);
    assert_eq!(lines["closed"]["scheduling_fault"], Value::Null);
    let digest = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(digest["child_status"], read["child_status"]);
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert_eq!(
        bearings["child_statuses"]["task:repair"],
        read["child_status"]
    );
    let text = bearings["text"].as_str().unwrap();
    let line = |id: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("- {id} ")))
            .unwrap_or_else(|| panic!("{id} line missing: {text}"))
            .to_owned()
    };
    assert!(line("runner").starts_with("- runner [running, sched fault] "));
    assert!(line("silent").starts_with("- silent [open, sched fault] "));
    assert!(line("long").starts_with("- long [open, sched fault] "));
    assert!(line("held").starts_with("- held [blocked] "));
    assert!(line("plain").starts_with("- plain [open] "));
    // A closed child never carries the mark, whatever its row still holds.
    assert!(line("closed").starts_with("- closed [done] "));
    // The mark is durable fact, not a derived hint: it survives a restart.
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(read_parent(&mut s), read);
}

/// A reason that is not a string must not fail the child-line read: the
/// mark still shows, without reason text. The runtime's own writers keep a
/// reason a string, so this row is malformed on purpose, and the schedule
/// snapshot would refuse it; the child-line projection reads it anyway.
#[test]
fn a_non_string_fault_reason_still_marks_the_child_line() {
    let (_temp, mut s) = fleet();
    child(&mut s, "odd", "repair");
    schedule_row(
        &mut s,
        "odd",
        json!({"state":"fault","faulted_ms":5,"reason":7}),
    );
    let read = read_parent(&mut s);
    assert_eq!(
        read["child_status"]["children"][0]["scheduling_fault"],
        json!({"reason":null,"faulted_ms":5}),
        "{read}"
    );
    let digest = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(digest["child_status"], read["child_status"]);
}

#[test]
fn legacy_child_times_are_recovered_once_without_inventing_missing_history() {
    let (temp, mut s) = fleet();
    child(&mut s, "known", "repair");
    child(&mut s, "unknown", "repair");
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"known","actor":"infra","status":"blocked"}),
    )
    .unwrap();
    // Include repeated same-status updates and unrelated audit traffic.
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"known","actor":"infra","status":"blocked"}),
    )
    .unwrap();
    s.connection().execute_batch(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000)
         INSERT INTO audit(time_ms,kind,payload) SELECT x,'unrelated','{}' FROM n;
         DELETE FROM audit WHERE json_extract(payload,'$.id')='unknown';
         UPDATE audit SET time_ms=1234 WHERE kind='task.update' AND json_extract(payload,'$.id')='known';
         DROP TRIGGER task_status_created; DROP TRIGGER task_status_changed;
         DROP INDEX tasks_parent_status_time; ALTER TABLE tasks DROP COLUMN status_changed_ms;
         ALTER TABLE tasks DROP COLUMN status_changed_approx;"
    ).unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    let read = read_parent(&mut s);
    assert_eq!(read["child_status"]["children"][0]["id"], "known");
    assert_eq!(
        read["child_status"]["children"][0]["status_changed_ms"],
        1234
    );
    assert!(read["child_status"]["children"][1]["status_changed_ms"].is_null());
    assert_eq!(
        read["child_status"]["children"][0]["status_changed_approx"],
        true
    );
    // A same-status audit update need not be a real edge; the recovered time
    // must remain visibly approximate until a new, observed transition.
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert!(bearings["text"].as_str().unwrap().contains("~1970-"));
    s.connection().execute("DELETE FROM audit", []).unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    assert_eq!(read_parent(&mut s), read);
    s.connection()
        .execute("UPDATE tasks SET status='blocked' WHERE id='known'", [])
        .unwrap();
    assert_eq!(
        read_parent(&mut s)["child_status"]["children"][0]["status_changed_approx"],
        true
    );
    s.connection()
        .execute("UPDATE tasks SET status='open' WHERE id='known'", [])
        .unwrap();
    assert_eq!(
        read_parent(&mut s)["child_status"]["children"][0]["status_changed_approx"],
        false
    );
}

#[test]
fn bearings_marks_children_omitted_by_the_shared_budget() {
    let (_temp, mut s) = fleet();
    // The first parent fills the budget exactly; another parent is omitted.
    for n in 0..20 {
        child(&mut s, &format!("child-{n}"), "cleanup");
    }
    child(&mut s, "last-child", "repair");
    s.connection()
        .execute(
            "UPDATE tasks SET status='done' WHERE parent IS NOT NULL",
            [],
        )
        .unwrap();
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    let statuses = bearings["child_statuses"].as_object().unwrap();
    assert_eq!(
        statuses
            .values()
            .map(|v| v["children"].as_array().unwrap().len())
            .sum::<usize>(),
        20
    );
    assert_eq!(bearings["child_statuses_more"], true);
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("More child status omitted")
    );
}

#[test]
fn notes_replace_each_other_and_keep_only_recent_versions() {
    let (_temp, mut s) = fleet();
    for n in 1..=7 {
        put(&mut s, "task:repair", &format!("Doing: step {n}")).unwrap();
    }
    let current = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(current["notes"][0]["version"], 7);
    assert_eq!(current["notes"][0]["body"], "Doing: step 7");
    assert_eq!(current["notes"][0]["author"], "operator");
    let history = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair","versions":10}),
    )
    .unwrap();
    let versions: Vec<_> = history["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["version"].as_i64().unwrap())
        .collect();
    assert_eq!(versions, [7, 6, 5, 4, 3]);
    put(&mut s, "thread:why-failed", "Waiting on: infra logs").unwrap();
    let list = dispatch(&mut s, "note.list", json!({"ship":"infra"})).unwrap();
    assert_eq!(list["notes"].as_array().unwrap().len(), 2);
}

#[test]
fn agent_ship_note_shrink_refusal_preserves_history_and_survives_reopen() {
    let (temp, mut s) = fleet();
    let standing = "rule ".repeat(800);
    for _ in 0..7 {
        put(&mut s, "ship", &standing).unwrap();
    }
    dispatch(
        &mut s,
        "note.pin",
        json!({"ship":"infra","focus":"ship","key":"policy","value":"staging first"}),
    )
    .unwrap();
    let read = json!({"ship":"infra","focus":"ship","versions":5});
    let before = dispatch(&mut s, "note.get", read.clone()).unwrap();
    drop(s);
    let mut s = Store::open(&temp.path().join("home")).unwrap();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    let short = "next ".repeat(180);
    for replace in [None, Some(json!(false))] {
        let mut args = json!({"ship":"infra","focus":"ship","body":short});
        if let Some(replace) = replace {
            args["replace"] = replace;
        }
        let refused = dispatch(&mut s, "note.put", args).unwrap_err().to_string();
        for expected in [
            "4000 to 900 bytes",
            "3100 bytes dropped",
            "\"replace\":true",
            "note.get",
        ] {
            assert!(refused.contains(expected), "{refused}");
        }
        assert_eq!(dispatch(&mut s, "note.get", read.clone()).unwrap(), before);
    }
    let result = dispatch(
        &mut s,
        "note.put",
        json!({"ship":"infra","focus":"ship","body":short,"replace":true}),
    )
    .unwrap();
    assert_eq!(result["version"], 8);
    let after = dispatch(&mut s, "note.get", read).unwrap();
    assert_eq!(after["notes"][0]["body"], short);
    assert_eq!(after["notes"][0]["author"], "infra");
    assert_eq!(after["pins"], before["pins"]);
    let history = after["notes"].as_array().unwrap();
    assert_eq!(history.len(), 5);
    assert_eq!(&history[1..], &before["notes"].as_array().unwrap()[..4]);
}

#[test]
fn agent_ship_note_shrink_guard_has_strict_byte_boundaries() {
    let (_temp, mut s) = fleet();
    for (prior_bytes, new_bytes, allowed) in [
        (1_023, 1, true),
        (1_024, 1, true),
        (1_025, 1, false),
        (1_025, 512, false),
        (1_025, 513, true),
        (2_048, 1_023, false),
        (2_048, 1_024, true),
        (2_048, 2_049, true),
    ] {
        s.set_caller(Caller::Operator);
        let prior = "a ".repeat(prior_bytes / 2) + &"a".repeat(prior_bytes % 2);
        put(&mut s, "ship", &prior).unwrap();
        s.set_caller(Caller::Agent {
            actor: "infra".into(),
        });
        let body = "b ".repeat(new_bytes / 2) + &"b".repeat(new_bytes % 2);
        let result = put(&mut s, "ship", &body);
        assert_eq!(
            result.is_ok(),
            allowed,
            "{prior_bytes} -> {new_bytes}: {result:?}"
        );
    }
    // Measure UTF-8 bytes, not SQLite's character length or word count.
    s.set_caller(Caller::Operator);
    put(&mut s, "ship", &"é ".repeat(400)).unwrap();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    let refused = put(&mut s, "ship", &"word ".repeat(119))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("1200 to 595 bytes (605 bytes dropped)"),
        "{refused}"
    );
}

#[test]
fn shrink_guard_leaves_operator_writes_and_other_handoff_foci_unchanged() {
    let (_temp, mut s) = fleet();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    // No prior note, short prior note, and a normal update need no override.
    put(&mut s, "ship", "First rules.").unwrap();
    put(&mut s, "ship", "Done.").unwrap();
    put(&mut s, "ship", &"rule ".repeat(800)).unwrap();
    put(&mut s, "ship", &"rule ".repeat(600)).unwrap();
    for focus in [
        "task:repair",
        "captain",
        "thread:request",
        "assignment:repair",
    ] {
        put(&mut s, focus, &"work ".repeat(400)).unwrap();
        put(&mut s, focus, "Done.").unwrap();
    }
    s.set_caller(Caller::Operator);
    put(&mut s, "ship", "Done.").unwrap();
}

#[test]
fn replace_must_be_boolean_and_does_not_bypass_other_note_checks() {
    let (_temp, mut s) = fleet();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    put(&mut s, "ship", &"rule ".repeat(800)).unwrap();
    for replace in [json!("true"), json!(1), Value::Null, json!({})] {
        let refused = dispatch(
            &mut s,
            "note.put",
            json!({"ship":"infra","focus":"ship","body":"Done.","replace":replace}),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("replace must be a boolean"), "{refused}");
    }
    for (ship, focus, body, expected) in [
        (
            "infra",
            "ship",
            "word ".repeat(820),
            "limited to 4096 bytes",
        ),
        ("infra", "ship", "x".repeat(200), "spaces removed"),
        ("infra", "ship", String::new(), "body is required"),
        ("apps", "ship", String::new(), "cannot act as"),
        ("infra", "task:feature", String::new(), "not owned by infra"),
    ] {
        let refused = dispatch(
            &mut s,
            "note.put",
            json!({"ship":ship,"focus":focus,"body":body,"replace":true}),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains(expected), "{refused}");
    }
    let kept = dispatch(&mut s, "note.get", json!({"ship":"infra","focus":"ship"})).unwrap();
    assert_eq!(kept["notes"][0]["version"], 1);
}

#[test]
fn notes_are_bounded_and_scoped_to_valid_foci() {
    let (_temp, mut s) = fleet();
    assert!(put(&mut s, "task:repair", &"word ".repeat(410)).is_err());
    put(&mut s, "ship", &"word ".repeat(800)).unwrap();
    assert!(put(&mut s, "ship", &"word ".repeat(820)).is_err());
    for focus in [
        "task:feature",
        "task:missing",
        "repair",
        "thread:bad id",
        "anything:else",
    ] {
        assert!(put(&mut s, focus, "x").is_err(), "{focus}");
    }
    put(
        &mut s,
        "captain",
        "Captain prefers summaries in the morning",
    )
    .unwrap();
    assert!(
        dispatch(
            &mut s,
            "note.put",
            json!({"ship":"nobody","focus":"ship","body":"x"})
        )
        .is_err()
    );
}

#[test]
fn agents_keep_notes_and_read_bearings_only_for_their_own_ship() {
    let (_temp, mut s) = fleet();
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    put(&mut s, "task:repair", "Doing: rerunning the pipeline").unwrap();
    let note = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(note["notes"][0]["author"], "infra");
    assert!(
        dispatch(
            &mut s,
            "note.put",
            json!({"ship":"apps","focus":"task:feature","body":"x"})
        )
        .is_err()
    );
    assert!(dispatch(&mut s, "ship.bearings", json!({"ship":"apps"})).is_err());
    dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
}

#[test]
fn bearings_summarize_a_ship_from_records() {
    let (_temp, mut s) = fleet();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"cleanup","actor":"infra","status":"done"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.send",
        json!({"id":"q","from":"apps","to":"infra","kind":"request","body":"Is staging up?"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.send",
        json!({"id":"ask","from":"infra","to":"apps","kind":"request","body":"Need the API spec"}),
    )
    .unwrap();
    put(&mut s, "ship", "Deploys go through staging first.").unwrap();
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert_eq!(bearings["open_tasks"].as_array().unwrap().len(), 1);
    assert!(
        bearings["open_tasks"][0]
            .as_str()
            .unwrap()
            .starts_with("task repair [open]")
    );
    assert!(
        bearings["requests_owed"][0]
            .as_str()
            .unwrap()
            .starts_with("q from apps")
    );
    assert!(
        bearings["requests_owing"][0]
            .as_str()
            .unwrap()
            .starts_with("ask to apps")
    );
    assert_eq!(bearings["unacknowledged_mail"], 1);
    let text = bearings["text"].as_str().unwrap();
    assert!(text.contains("Deploys go through staging first."));
    assert!(!text.contains("cleanup"));
}

#[test]
fn bearings_flag_only_unacked_received_mail_covered_by_successful_wakes() {
    let (_temp, mut s) = fleet();
    let mut seqs = vec![];
    for id in ["handled", "handoff", "correction", "pending"] {
        let mail = dispatch(
            &mut s,
            "mail.send",
            json!({"id":id,"from":"apps","to":"infra","kind":"notice","body":id}),
        )
        .unwrap();
        seqs.push(mail["seq"].as_i64().unwrap());
    }
    s.connection()
        .execute("UPDATE mail SET delivery='pending' WHERE id='pending'", [])
        .unwrap();
    let bearings = |s: &mut Store| dispatch(s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    let initial = bearings(&mut s);
    assert_eq!(initial["stale_unacknowledged_mail"]["count"], 0);
    assert!(initial["stale_unacknowledged_mail"]["oldest_seq"].is_null());
    assert!(
        !initial["text"]
            .as_str()
            .unwrap()
            .contains("Stale unacknowledged mail:")
    );

    // A completed wake covers the handoff, not the correction that arrived
    // afterwards. Failed/running wakes, another ship and outgoing copies do
    // not enlarge that coverage. No timestamps are used as evidence of seeing.
    for (id, ship, status, through) in [
        ("previous", "infra", "succeeded", seqs[1]),
        ("failed", "infra", "failed", seqs[3]),
        ("running", "infra", "running", seqs[3]),
        ("other-ship", "apps", "succeeded", seqs[3]),
    ] {
        s.connection()
            .execute(
                "INSERT INTO wakes(id,ship,focus,through_seq,mode,status,created_ms,finished_ms)
                 VALUES(?1,?2,'ship',?3,'fresh',?4,1,2)",
                rusqlite::params![id, ship, through, status],
            )
            .unwrap();
    }
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"infra","through_seq":seqs[0]}),
    )
    .unwrap();
    // The warning survives reopening the store and reading it does not ack.
    let home = s.home().to_path_buf();
    drop(s);
    let mut s = Store::open(&home).unwrap();
    let stale = bearings(&mut s);
    assert_eq!(stale["unacknowledged_mail"], 2);
    assert_eq!(
        stale["stale_unacknowledged_mail"],
        json!({
            "count":1, "oldest_seq":seqs[1], "woken_through_seq":seqs[1]
        })
    );
    let text = stale["text"].as_str().unwrap();
    assert!(text.contains(&format!("oldest seq {}", seqs[1])), "{text}");
    assert!(text.contains("still needing handling or an ack"), "{text}");
    assert!(
        text.contains("not to other mail in this or later wakes"),
        "{text}"
    );
    assert_eq!(
        bearings(&mut s)["stale_unacknowledged_mail"],
        stale["stale_unacknowledged_mail"]
    );

    // Successful coverage includes notices counted but not rendered. Pending
    // delivery is still excluded even if its seq is below the new watermark.
    s.connection()
        .execute(
            "UPDATE wakes SET through_seq=?1 WHERE id='previous'",
            [seqs[3]],
        )
        .unwrap();
    let covered = bearings(&mut s);
    assert_eq!(covered["stale_unacknowledged_mail"]["count"], 2);
    dispatch(
        &mut s,
        "mail.ack",
        json!({"address":"infra","through_seq":seqs[2]}),
    )
    .unwrap();
    let cleared = bearings(&mut s);
    assert_eq!(cleared["stale_unacknowledged_mail"]["count"], 0);
    assert!(cleared["stale_unacknowledged_mail"]["oldest_seq"].is_null());
    assert!(
        !cleared["text"]
            .as_str()
            .unwrap()
            .contains("Stale unacknowledged mail:")
    );
}

#[test]
fn digests_return_a_focus_history_on_demand() {
    let (_temp, mut s) = fleet();
    dispatch(
        &mut s,
        "event.emit",
        json!({"id":"e1","task_id":"repair","issue_key":"ci","kind":"alert","payload":{"failed":"deploy"}}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.send",
        json!({"id":"status","from":"apps","to":"infra","kind":"request","task_id":"repair","body":"How is the repair?"}),
    )
    .unwrap();
    dispatch(
        &mut s,
        "mail.send",
        json!({"from":"infra","to":"apps","kind":"reply","correlation":"status","body":"Halfway"}),
    )
    .unwrap();
    put(&mut s, "task:repair", "Doing: patching the runner").unwrap();
    let task = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(task["task"]["title"], "Repair the staging deploy pipeline");
    assert_eq!(task["events"][0]["kind"], "alert");
    assert_eq!(task["mail"][0]["id"], "status");
    assert_eq!(task["note"]["body"], "Doing: patching the runner");
    let thread = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"thread:status"}),
    )
    .unwrap();
    let bodies: Vec<_> = thread["mail"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(bodies, ["How is the repair?", "Halfway"]);
    dispatch(
        &mut s,
        "mail.send",
        json!({"from":"captain","to":"infra","kind":"notice","body":"Please prioritise the repair"}),
    )
    .unwrap();
    let captain = dispatch(
        &mut s,
        "focus.digest",
        json!({"ship":"infra","focus":"captain"}),
    )
    .unwrap();
    assert_eq!(captain["mail"][0]["body"], "Please prioritise the repair");
}

#[test]
fn prune_drops_notes_for_finished_tasks_and_old_threads_only() {
    let (_temp, mut s) = fleet();
    put(&mut s, "task:repair", "open task").unwrap();
    put(&mut s, "task:cleanup", "finished task").unwrap();
    put(&mut s, "thread:old", "old thread").unwrap();
    put(&mut s, "captain", "captain preferences").unwrap();
    put(&mut s, "ship", "ship knowledge").unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"cleanup","actor":"infra","status":"done"}),
    )
    .unwrap();
    let pruned = dispatch(
        &mut s,
        "history.prune",
        json!({"before_ms": Store::now_ms() + 1000}),
    )
    .unwrap();
    assert_eq!(pruned["notes"], 2);
    let left: Vec<_> = dispatch(&mut s, "note.list", json!({"ship":"infra"})).unwrap()["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["focus"].as_str().unwrap().to_owned())
        .collect();
    let mut left = left;
    left.sort();
    assert_eq!(left, ["captain", "ship", "task:repair"]);
}

#[test]
fn a_note_squeezed_by_dropping_spaces_is_refused_while_prose_with_ids_is_kept() {
    let (_temp, mut s) = fleet();
    let prose = "Waiting on the hosting gate before starting the bridge crew. The fixture \
        emitted a session id that is not a UUID, so planning failed before the job existed; \
        I fixed the fixture at ad536c874dc193b7d7bc2aa0367b747dcf68ec03 in \
        /home/captain/.local/state/privateer-host/worktrees/7316ffd9 and queued one focused \
        run. Next: read its logs, then the full suite, then open the PR.";
    put(&mut s, "task:repair", prose).unwrap();
    let squeezed = "Diagnosticworker cb32424c terminalSUCCEEDED0(handoff308/309), focused92157bb0 \
        terminalFAILED101 independently FULLlogs/status/command read. Literalclean103c pre/post. \
        Source4lease7316ffd retained. FIRSTbackofffallback actuallySUCCEEDED/native0/payloadexit, \
        SECONDdue missing-socketattempt reportshosterror asintended, butmode=resume/planningFAILED \
        beforejobcreated; diagnosticthen lookedupnonexistentjobandpanicked.";
    let refused = put(&mut s, "task:repair", squeezed)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("spaces removed"), "{refused}");
    let kept = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(kept["notes"][0]["body"], prose);
}

#[test]
fn an_assignment_reaches_its_crew_and_pins_hold_a_few_exact_facts() {
    let (_temp, mut s) = fleet();
    // An assignment is longer than a handoff note and comes with the task's note.
    let assignment =
        "Rebuild the cache index, keep the old one until the new one passes. ".repeat(120);
    assert!(assignment.len() > 2_048 && assignment.len() < 16_384);
    dispatch(
        &mut s,
        "note.put",
        json!({"ship":"infra","focus":"assignment:repair","body":assignment}),
    )
    .unwrap();
    put(&mut s, "task:repair", "Waiting on the crew's first report.").unwrap();
    let read = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(read["assignment"], assignment.as_str());
    assert_eq!(
        read["notes"][0]["body"],
        "Waiting on the crew's first report."
    );
    assert!(
        dispatch(
            &mut s,
            "note.put",
            json!({"ship":"infra","focus":"assignment:feature","body":"Not infra's task."}),
        )
        .is_err()
    );

    let pin = |s: &mut Store, key: &str, value: &str| {
        dispatch(
            s,
            "note.pin",
            json!({"ship":"infra","focus":"task:repair","key":key,"value":value}),
        )
    };
    for n in 0..8 {
        pin(&mut s, &format!("fact{n}"), &format!("value {n}")).unwrap();
    }
    let full = pin(&mut s, "ninth", "one too many")
        .unwrap_err()
        .to_string();
    assert!(full.contains("at most 8 pins"), "{full}");
    // Replacing a pin and unpinning one never count against the limit.
    pin(&mut s, "fact0", "ad536c874dc193b7d7bc2aa0367b747dcf68ec03").unwrap();
    pin(&mut s, "fact1", "").unwrap();
    pin(&mut s, "head", "86dc091").unwrap();
    assert!(pin(&mut s, "log", "line one\nline two").is_err());
    let read = dispatch(
        &mut s,
        "note.get",
        json!({"ship":"infra","focus":"task:repair"}),
    )
    .unwrap();
    assert_eq!(
        read["pins"]["fact0"],
        "ad536c874dc193b7d7bc2aa0367b747dcf68ec03"
    );
    assert_eq!(read["pins"]["head"], "86dc091");
    assert!(read["pins"].get("fact1").is_none());
}

#[test]
fn bearings_name_running_tasks_that_nothing_is_moving() {
    let (_temp, mut s) = fleet();
    let task = |s: &mut Store, id: &str, parent: Option<&str>, status: &str| {
        let mut args =
            json!({"id":id,"charter_id":"fleet","owner":"infra","title":format!("Work on {id}")});
        if let Some(parent) = parent {
            args["parent"] = json!(parent);
        }
        dispatch(s, "task.create", args).unwrap();
        dispatch(
            s,
            "task.update",
            json!({"id":id,"actor":"infra","status":status}),
        )
        .unwrap();
    };
    for id in ["announced", "delegated", "asking", "parked", "infra-duty"] {
        task(&mut s, id, None, "running");
    }
    task(&mut s, "stuck", None, "blocked");
    // A crew job on a child task moves its parent.
    task(&mut s, "delegated-step", Some("delegated"), "running");
    s.connection()
        .execute(
            "INSERT INTO jobs(id, record) VALUES('crew-1', ?1)",
            [json!({"id":"crew-1","task_id":"delegated-step","status":"running"}).to_string()],
        )
        .unwrap();
    // An open request waits on someone else.
    dispatch(
        &mut s,
        "mail.send",
        json!({"from":"infra","to":"apps","kind":"request","task_id":"asking","body":"Which region?"}),
    )
    .unwrap();
    // A pinned wait is a recorded reason.
    dispatch(
        &mut s,
        "note.pin",
        json!({"ship":"infra","focus":"task:parked","key":"waiting","value":"PR #117 CI"}),
    )
    .unwrap();
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert_eq!(
        bearings["idle_tasks"],
        json!([{"id":"announced","title":"Work on announced"}])
    );
    let text = bearings["text"].as_str().unwrap();
    assert!(text.contains("nothing moving them"), "{text}");
    assert!(
        text.contains("- task announced: Work on announced"),
        "{text}"
    );
    // Once the job ends, the parent has nothing moving it either.
    s.connection()
        .execute(
            "UPDATE jobs SET record=json_set(record,'$.status','succeeded') WHERE id='crew-1'",
            [],
        )
        .unwrap();
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    let idle: Vec<&str> = bearings["idle_tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(idle, ["announced", "delegated", "delegated-step"]);
}

#[test]
fn bearings_turn_attention_reports_into_pressure() {
    let (_temp, mut s) = fleet();
    let report = |s: &mut Store, kind: &str, tokens: i64| {
        dispatch(
            s,
            "attention.report",
            json!({"ship":"infra","kind":kind,"tokens":tokens,"window":272_000,"summary_chars":1000}),
        )
        .unwrap()
    };
    // Nothing reported yet: the bearings stay as they were.
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert!(bearings["attention"].is_null());
    assert!(!bearings["text"].as_str().unwrap().contains("Attention"));

    for tokens in [60_000, 90_000, 120_000] {
        report(&mut s, "turn", tokens);
    }
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    let attention = &bearings["attention"];
    assert_eq!(attention["pressure"], "normal", "{attention}");
    assert_eq!(attention["added_hour"], 60_000);
    assert_eq!(attention["percent"], 44.0);
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("Attention: normal (context 44% of 272k, 60k added"),
        "{text}"
    );

    // A compaction resets the baseline and makes the pressure high.
    report(&mut s, "compaction", 258_000);
    report(&mut s, "turn", 40_000);
    report(&mut s, "turn", 50_000);
    let attention = dispatch(&mut s, "attention.show", json!({"ship":"infra"})).unwrap();
    assert_eq!(attention["pressure"], "high");
    assert_eq!(attention["added_hour"], 70_000);
    assert_eq!(attention["compactions_hour"], 1);
    let text = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("Start no new task"), "{text}");

    // Unread crew reports also count, as does a nearly full context.
    let (_temp, mut s) = fleet();
    report(&mut s, "turn", 210_000);
    let attention = dispatch(&mut s, "attention.show", json!({"ship":"infra"})).unwrap();
    assert_eq!(attention["reasons"], json!(["context 77% full"]));
    // Another ship cannot report for this one.
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    assert!(
        dispatch(
            &mut s,
            "attention.report",
            json!({"ship":"infra","kind":"turn","tokens":1,"window":2})
        )
        .is_err()
    );
}

#[test]
fn a_backlog_starts_ranked_tasks_first_and_holds_only_unstarted_work() {
    let (_temp, mut s) = fleet();
    for id in ["docs", "audit"] {
        dispatch(
            &mut s,
            "task.create",
            json!({"id":id,"charter_id":"fleet","owner":"infra","title":format!("Do {id}")}),
        )
        .unwrap();
    }
    let order = |s: &mut Store| -> Vec<String> {
        dispatch(s, "task.backlog", json!({"ship":"infra"})).unwrap()["backlog"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_owned())
            .collect()
    };
    // Unranked tasks wait oldest first.
    assert_eq!(order(&mut s), ["repair", "cleanup", "docs", "audit"]);
    s.set_caller(Caller::Agent {
        actor: "infra".into(),
    });
    dispatch(&mut s, "task.rank", json!({"id":"audit","rank":1})).unwrap();
    dispatch(&mut s, "task.rank", json!({"id":"docs","rank":2})).unwrap();
    assert_eq!(order(&mut s), ["audit", "docs", "repair", "cleanup"]);
    // A cleared rank falls back among the unranked; equal ranks go oldest first.
    dispatch(&mut s, "task.rank", json!({"id":"docs","rank":null})).unwrap();
    dispatch(&mut s, "task.rank", json!({"id":"cleanup","rank":1})).unwrap();
    assert_eq!(order(&mut s), ["cleanup", "audit", "repair", "docs"]);

    // Starting a task takes it out of the backlog and to the top of the work.
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"repair","actor":"infra","status":"running"}),
    )
    .unwrap();
    assert_eq!(order(&mut s), ["cleanup", "audit", "docs"]);
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert_eq!(
        bearings["open_tasks"],
        json!([
            "task repair [running]: Repair the staging deploy pipeline",
            "task cleanup [open #1]: Clean up old build artifacts",
            "task audit [open #1]: Do audit",
            "task docs [open]: Do docs"
        ])
    );
    assert_eq!(bearings["backlog"][0]["id"], "cleanup");
    // Only open tasks are ranked, and only by their own ship.
    assert!(dispatch(&mut s, "task.rank", json!({"id":"repair","rank":1})).is_err());
    s.set_caller(Caller::Agent {
        actor: "apps".into(),
    });
    let refused = dispatch(&mut s, "task.rank", json!({"id":"docs","rank":1})).unwrap_err();
    assert!(
        refused.to_string().contains("only infra orders"),
        "{refused}"
    );
    assert!(dispatch(&mut s, "task.backlog", json!({"ship":"infra"})).is_err());
}

#[test]
fn bearings_name_open_tasks_with_no_update_for_the_ships_days_oldest_first() {
    let (_temp, mut s) = fleet();
    for id in ["stale", "finished", "deferred", "parent", "ancient"] {
        dispatch(
            &mut s,
            "task.create",
            json!({"id":id,"charter_id":"fleet","owner":"infra","title":format!("Work on {id}")}),
        )
        .unwrap();
    }
    // An open parent is not stale while its open child is being worked.
    dispatch(
        &mut s,
        "task.create",
        json!({"id":"child","parent":"parent","owner":"infra","title":"Work on the child"}),
    )
    .unwrap();
    put(&mut s, "task:stale", "Doing: waiting for a review").unwrap();
    put(&mut s, "task:ancient", "Doing: nothing lately").unwrap();
    dispatch(
        &mut s,
        "task.update",
        json!({"id":"finished","actor":"infra","status":"done"}),
    )
    .unwrap();
    // A typed wait is its own update: a deferred task is not stale. The
    // fixture ship has no first mate to enqueue to, so its schedule is stored
    // as task.defer leaves it.
    let later = Store::now_ms() + 30 * 86_400_000;
    let schedule = json!({"task_id":"deferred","owner":"infra","revision":1,
        "brief":"Continue after the window.","state":"waiting",
        "wait":{"kind":"time","until_ms":later,"reason":"after the window"},
        "not_before_ms":later,"offer_id":null,"holder":null,"job_id":null,"profile":null,
        "charter_hash":null,"attempts":0,"created_ms":Store::now_ms(),
        "last_checked_ms":Store::now_ms(),"lease_until_ms":0,"reason":"after the window"});
    s.connection()
        .execute(
            "INSERT INTO task_schedule(task_id,record) VALUES('deferred',?1)",
            [schedule.to_string()],
        )
        .unwrap();
    // Age everything recorded about them; repair and cleanup stay fresh.
    let now = Store::now_ms();
    for (id, days) in [
        ("stale", 4),
        ("finished", 10),
        ("deferred", 10),
        ("parent", 10),
        // Past the horizon the audit is not read and the age is not exact.
        ("ancient", 40),
    ] {
        let at = now - days * 86_400_000 - 60_000;
        s.connection()
            .execute(
                "UPDATE audit SET time_ms=?2 WHERE json_extract(payload,'$.id')=?1",
                rusqlite::params![id, at],
            )
            .unwrap();
        s.connection()
            .execute(
                "UPDATE notes SET created_ms=?2 WHERE focus='task:' || ?1",
                rusqlite::params![id, at],
            )
            .unwrap();
    }
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains(
            "2 open tasks have had no update in 3 days or more: ancient (30+ d), stale (4 d)."
        ),
        "{text}"
    );
    let stale_warning = text
        .lines()
        .find(|line| line.contains("open tasks have had no update"))
        .unwrap();
    for other in [
        "finished (",
        "deferred (",
        "parent (",
        "child (",
        "repair (",
        "cleanup (",
    ] {
        assert!(!stale_warning.contains(other), "{other}: {text}");
    }
    assert_eq!(bearings["stale_tasks"], 2);
    // The threshold is the ship's `first_mate.stale_task_days`.
    s.connection()
        .execute(
            "UPDATE ships SET record=json_set(record,'$.first_mate.stale_task_days',5) WHERE name='infra'",
            [],
        )
        .unwrap();
    let bearings = dispatch(&mut s, "ship.bearings", json!({"ship":"infra"})).unwrap();
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("1 open task has had no update in 5 days or more: ancient (30+ d)."),
        "{}",
        bearings["text"]
    );
    assert_eq!(bearings["stale_tasks"], 1);
}
