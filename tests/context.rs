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
    assert!(put(&mut s, "task:repair", &"x".repeat(2_049)).is_err());
    put(&mut s, "ship", &"x".repeat(4_000)).unwrap();
    assert!(put(&mut s, "ship", &"x".repeat(4_097)).is_err());
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
