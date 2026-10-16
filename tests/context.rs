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
