//! The captain's files on one machine, through the real binary: a file put to
//! a ship lands in that ship's home, its mail carries it, and nothing else may.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A privateer command in `ship`'s folder, as the captain (the operator) unless
/// `agent` names the ship whose identity it runs under.
fn run(root: &Path, ship: &str, agent: bool, command: &str, args: Value) -> Output {
    let mut process: Command = support::isolated(root);
    process.current_dir(root.join(ship));
    if agent {
        process
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", ship);
    }
    process
        .arg(command)
        .arg("--json")
        .arg(args.to_string())
        .output()
        .unwrap()
}

fn ok(output: Output) -> Value {
    assert!(output.status.success(), "{}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Two ships on this machine, each with its own folder and home.
fn fleet() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    for ship in ["apps", "infra"] {
        fs::create_dir_all(root.join(ship)).unwrap();
        let made = support::isolated(&root)
            .current_dir(root.join(ship))
            .args(["init", "--ship", ship, "--harness", "codex"])
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", text(&made));
    }
    (temp, root)
}

fn source(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join("sources").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn inbox(root: &Path, ship: &str) -> Vec<Value> {
    ok(run(
        root,
        ship,
        false,
        "mail.inbox",
        json!({"address": ship}),
    ))["messages"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn a_file_the_captain_puts_is_in_the_ships_mail_with_its_bytes() {
    let (_temp, root) = fleet();
    let plan = source(&root, "plan.md", b"# Plan\n\nShip the release.\n");
    let put = ok(run(
        &root,
        "apps",
        false,
        "file.put",
        json!({"to": "apps", "name": "../plan.md", "content_file": plan}),
    ));
    assert_eq!(put["name"], "plan.md", "the name is its base name");
    assert_eq!(put["type"], "text/markdown");
    assert_eq!(put["ship"], "apps");
    let stored = PathBuf::from(put["path"].as_str().unwrap());
    assert!(
        stored.starts_with(root.join("apps").join(".privateer")),
        "{stored:?}"
    );
    assert_eq!(fs::read(&stored).unwrap(), fs::read(&plan).unwrap());

    let sent = ok(run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "apps", "body": "The plan is attached.",
            "payload": {"files": [{"file_id": put["file_id"]}]}}),
    ));
    assert_eq!(sent["to"], "apps", "{sent}");

    let mail = inbox(&root, "apps");
    let message = mail
        .iter()
        .find(|m| m["body"] == "The plan is attached.")
        .unwrap_or_else(|| panic!("no captain mail in {mail:?}"));
    let file = &message["payload"]["files"][0];
    assert_eq!(file["file_id"], put["file_id"]);
    assert_eq!(file["name"], "plan.md");
    assert_eq!(file["bytes"], fs::metadata(&plan).unwrap().len());
    assert_eq!(file["path"], put["path"]);
    assert_eq!(
        fs::read(file["path"].as_str().unwrap()).unwrap(),
        fs::read(&plan).unwrap()
    );
}

#[test]
fn only_the_captain_puts_files_and_a_ship_gets_only_its_own() {
    let (_temp, root) = fleet();
    let plan = source(&root, "plan.md", b"plan");
    let put = ok(run(
        &root,
        "apps",
        false,
        "file.put",
        json!({"to": "apps", "name": "plan.md", "content_file": plan}),
    ));
    let file_id = put["file_id"].as_str().unwrap();

    let by_ship = run(
        &root,
        "apps",
        true,
        "file.put",
        json!({"to": "apps", "name": "x.md", "content_file": plan}),
    );
    assert!(!by_ship.status.success());
    assert!(
        text(&by_ship).contains("operator command"),
        "{}",
        text(&by_ship)
    );

    let foreign = run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "infra", "body": "Plan.",
            "payload": {"files": [{"file_id": file_id}]}}),
    );
    assert!(!foreign.status.success());
    assert!(text(&foreign).contains(file_id), "{}", text(&foreign));
    assert!(
        text(&foreign).contains("apps's, not infra's"),
        "{}",
        text(&foreign)
    );
    assert!(inbox(&root, "infra").is_empty());

    let unknown = run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "apps", "body": "Plan.",
            "payload": {"files": [{"file_id": "file-missing"}]}}),
    );
    assert!(
        text(&unknown).contains("file-missing"),
        "{}",
        text(&unknown)
    );

    let from_ship = run(
        &root,
        "apps",
        true,
        "mail.send",
        json!({"from": "apps", "to": "infra", "kind": "notice", "body": "Plan.",
            "payload": {"files": [{"file_id": file_id}]}}),
    );
    assert!(
        text(&from_ship).contains("captain's"),
        "{}",
        text(&from_ship)
    );
}

#[test]
fn a_filled_in_file_entry_is_refused_whatever_its_path() {
    let (_temp, root) = fleet();
    let plan = source(&root, "plan.md", b"plan");
    let put = ok(run(
        &root,
        "apps",
        false,
        "file.put",
        json!({"to": "apps", "name": "plan.md", "content_file": plan}),
    ));
    let file_id = put["file_id"].as_str().unwrap();

    // A forged entry that names another ship's file with any path: the caller
    // never fills an entry in, and the file is looked up by its id alone.
    let forged = run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "apps", "body": "Plan.",
            "payload": {"files": [{"file_id": file_id, "name": "plan.md", "bytes": 4,
                "type": "text/markdown",
                "path": "/home/kirkins/.ssh/id_ed25519"}]}}),
    );
    assert!(!forged.status.success(), "{}", text(&forged));
    assert!(
        text(&forged).contains("carries more than file_id"),
        "{}",
        text(&forged)
    );
    assert!(inbox(&root, "apps").is_empty());

    // Any extra key is refused the same way, even one of the runtime's own.
    let extra = run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "apps", "body": "Plan.",
            "payload": {"files": [{"file_id": file_id, "name": "plan.md"}]}}),
    );
    assert!(!extra.status.success(), "{}", text(&extra));
    assert!(
        text(&extra).contains("carries more than file_id"),
        "{}",
        text(&extra)
    );
    assert!(inbox(&root, "apps").is_empty());
}

#[test]
fn a_ships_own_file_read_from_a_home_that_claims_it_names_both_ships() {
    let (_temp, root) = fleet();
    let plan = source(&root, "plan.md", b"plan");
    let put = ok(run(
        &root,
        "apps",
        false,
        "file.put",
        json!({"to": "apps", "name": "plan.md", "content_file": plan}),
    ));
    let file_id = put["file_id"].as_str().unwrap();

    // infra's home wrongly records apps as hosted there, as a stale directory
    // entry can. The captain's mail to apps written in that home cannot carry
    // apps's file, and the refusal names both ships, not apps twice.
    let mut wrong_home = privateer::store::Store::open(&root.join("infra/.privateer")).unwrap();
    privateer::dispatch(
        &mut wrong_home,
        "ship.put",
        json!({"name": "apps", "location": "here"}),
    )
    .unwrap();
    let misplaced = run(
        &root,
        "infra",
        false,
        "mail.send",
        json!({"from": "captain", "to": "apps", "kind": "notice", "body": "Plan.",
            "payload": {"files": [{"file_id": file_id}]}}),
    );
    let said = text(&misplaced);
    assert!(!misplaced.status.success(), "{said}");
    assert!(said.contains("apps's home"), "{said}");
    assert!(said.contains("not in infra's"), "{said}");
    assert!(!said.contains("apps's, not apps's"), "{said}");
    assert!(inbox(&root, "apps").is_empty());
}

#[test]
fn a_file_past_the_cap_is_refused_before_it_is_stored() {
    let (_temp, root) = fleet();
    let big = root.join("sources").join("big.bin");
    fs::create_dir_all(big.parent().unwrap()).unwrap();
    // Sparse: the length is what counts, and no bytes are written.
    fs::File::create(&big)
        .unwrap()
        .set_len(50 * 1024 * 1024 + 1)
        .unwrap();
    let refused = run(
        &root,
        "apps",
        false,
        "file.put",
        json!({"to": "apps", "name": "big.bin", "content_file": big}),
    );
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("at most 50 MiB"),
        "{}",
        text(&refused)
    );
    assert!(!root.join("apps/.privateer/files/uploads").exists());
}

#[test]
fn the_newest_200_files_a_ship_has_are_kept_and_the_rest_are_removed() {
    let (_temp, root) = fleet();
    let mut stored = vec![];
    for n in 0..201 {
        let plan = source(
            &root,
            &format!("note-{n}.txt"),
            format!("note {n}").as_bytes(),
        );
        let put = ok(run(
            &root,
            "apps",
            false,
            "file.put",
            json!({"to": "apps", "name": format!("note-{n}.txt"), "content_file": plan}),
        ));
        stored.push(PathBuf::from(put["path"].as_str().unwrap()));
    }
    assert!(
        !stored[0].exists(),
        "the oldest file's folder should be gone"
    );
    assert!(stored[1].exists(), "the second file is still kept");
    assert_eq!(fs::read(&stored[200]).unwrap(), b"note 200");

    let refused = run(
        &root,
        "apps",
        false,
        "captain.send",
        json!({"to": "apps", "body": "Old note.",
            "payload": {"files": [{"file_id": stored[0].parent().unwrap().file_name().unwrap().to_str().unwrap()}]}}),
    );
    assert!(!refused.status.success(), "{}", text(&refused));
}
