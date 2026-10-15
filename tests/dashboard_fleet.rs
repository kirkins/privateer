#![cfg(feature = "dashboard")]
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Machine {
    _temp: tempfile::TempDir,
    root: PathBuf,
}
impl Machine {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        Self { _temp: temp, root }
    }
    fn command(&self, binary: &str, folder: &Path) -> Command {
        let mut c = Command::new(binary);
        c.current_dir(folder)
            .env("HOME", &self.root)
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        c
    }
    fn cli(&self, folder: &Path, args: &[&str]) -> String {
        let out = self
            .command(env!("CARGO_BIN_EXE_privateer"), folder)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn json(&self, folder: &Path, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.cli(folder, &[command, "--json", &args.to_string()])).unwrap()
    }
    fn ship(&self, name: &str) -> PathBuf {
        let folder = self.root.join(name);
        fs::create_dir(&folder).unwrap();
        self.cli(
            &folder,
            &[
                "init",
                "--ship",
                name,
                "--scope",
                &format!("Scope of {name}"),
                "--harness",
                "codex",
            ],
        );
        folder
    }
    fn dashboard(&self) -> Value {
        let out = self
            .command(env!("CARGO_BIN_EXE_privateer-dashboard"), &self.root)
            .args(["--once", "--no-herdr"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn empty_fleet_startup_creates_no_state() {
    let m = Machine::new();
    let view = m.dashboard();
    assert_eq!(view["snapshot"]["ships"], json!([]));
    assert_eq!(fs::read_dir(&m.root).unwrap().count(), 0);
}

#[test]
fn fleet_uses_each_home_and_never_resurrects_a_missing_ship() {
    use std::os::fd::AsRawFd;
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let gone = m.ship("gone");
    m.json(
        &infra,
        "ship.attend",
        json!({"ship":"infra","holder":"test-live","harness":"codex"}),
    );
    m.json(
        &apps,
        "mail.send",
        json!({"from":"captain","to":"apps","kind":"request","body":"An actual request"}),
    );
    let lock = fs::File::create(infra.join(".privateer/serve.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    fs::remove_dir_all(&gone).unwrap();
    let view = m.dashboard();
    let ships = view["snapshot"]["ships"].as_array().unwrap();
    assert_eq!(ships.len(), 3);
    let app = ships.iter().find(|s| s["id"] == "apps").unwrap();
    assert_eq!(app["description"], "Scope of apps");
    assert_eq!(app["live"]["harness"], "codex");
    assert_eq!(app["live"]["unread"], 1);
    assert_eq!(app["live"]["requests"], 1);
    assert_eq!(app["live"]["attended"], false);
    let inf = ships.iter().find(|s| s["id"] == "infra").unwrap();
    assert_eq!(inf["live"]["attended"], true);
    assert_eq!(inf["live"]["dispatcher"], true);
    assert!(
        ships.iter().find(|s| s["id"] == "gone").unwrap()["live"]["error"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    assert!(!gone.exists());
    drop(lock);
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"apps"}))["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    m.json(&apps, "ship.retire", json!({"name":"infra"}));
    assert_eq!(
        m.dashboard()["snapshot"]["ships"].as_array().unwrap().len(),
        2
    );
    assert!(infra.exists());
}

#[test]
fn captain_mail_keeps_home_identity_replies_and_acknowledges_only_read_prefix() {
    use privateer::dashboard::{command, fleet, mailbox::Mailbox};
    use std::os::unix::fs::PermissionsExt;
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.root.join("privateer-proxy");
    // Exercise the real binary with per-child isolation rather than changing
    // this multithreaded test process's environment.
    fs::write(&proxy,format!("#!/bin/sh\nexport HOME='{}' PRIVATEER_FLEET='{}/fleet.sqlite' PRIVATEER_SERVICE=none PRIVATEER_RESOURCE_HOME='{}/ledger'\nunset PRIVATEER_ROLE PRIVATEER_ACTOR PRIVATEER_HOME\nexec '{}' \"$@\"\n",m.root.display(),m.root.display(),m.root.display(),env!("CARGO_BIN_EXE_privateer"))).unwrap();
    fs::set_permissions(&proxy, fs::Permissions::from_mode(0o755)).unwrap();
    let first=m.json(&apps,"mail.send",json!({"id":"same-id","from":"apps","to":"captain","kind":"notice","body":"Earlier notice"}));
    let decision=m.json(&apps,"mail.send",json!({"id":"decision","from":"apps","to":"captain","kind":"decision","body":"Please choose"}));
    m.json(&infra,"mail.send",json!({"id":"same-id","from":"infra","to":"captain","kind":"request","body":"Independent request"}));
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    assert_eq!(mailbox.messages.len(), 3);
    assert_eq!(mailbox.messages[0].kind, "decision");
    assert_eq!(mailbox.messages[1].kind, "request");
    mailbox
        .mark_read(&proxy, &mailbox.messages[0].clone())
        .unwrap();
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        0
    );
    mailbox
        .mark_read(&proxy, &mailbox.messages[2].clone())
        .unwrap();
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        decision["seq"]
    );
    // A newer arrival survives a stale read, and acknowledged open decisions
    // remain in the dashboard until answered.
    m.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"notice","body":"Later"}),
    );
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(
        mailbox
            .messages
            .iter()
            .any(|m| m.id == "decision" && m.acknowledged)
    );
    let reply=command::json(&proxy,&apps.join(".privateer"),"mail.send",&json!({"id":"captain-reply","from":"captain","to":"apps","kind":"reply","correlation":"decision","body":"Proceed"})).unwrap();
    assert_eq!(reply["kind"], "reply");
    assert_eq!(
        m.json(&apps, "mail.show", json!({"id":"decision"}))["state"],
        "answered"
    );
    assert_eq!(
        m.json(&infra, "mail.show", json!({"id":"same-id"}))["state"],
        "open"
    );
    assert_eq!(
        m.json(&apps, "mail.show", json!({"id":first["id"]}))["body"],
        "Earlier notice"
    );
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"captain"}))["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(!mailbox.messages.iter().any(|m| m.id == "decision"));
}

#[test]
fn retirement_rejects_a_name_that_now_belongs_to_a_different_home() {
    let m = Machine::new();
    let apps = m.ship("apps");
    let out = m
        .command(env!("CARGO_BIN_EXE_privateer"), &apps)
        .args([
            "ship.retire",
            "--json",
            &json!({"name":"apps","expected_home":m.root.join("other/.privateer")}).to_string(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        m.json(&apps, "fleet.list", json!({}))["ships"][0]["retired_ms"],
        Value::Null
    );
}

#[test]
fn management_forms_require_explicit_retirement_and_never_mutate_demo() {
    use crossterm::event::KeyCode;
    use privateer::dashboard::{fleet, forms::Kind, ui::App};
    let m = Machine::new();
    m.ship("apps");
    let mut app = App {
        snapshot: fleet::read(&m.root.join("fleet.sqlite"), None).unwrap(),
        ..App::default()
    };
    app.select_ship(Some("apps".into()));
    app.manage(Kind::Retire);
    app.edit_form(KeyCode::Enter);
    assert!(app.action.is_none());
    for c in "apps".chars() {
        app.edit_form(KeyCode::Char(c));
    }
    app.edit_form(KeyCode::Enter);
    let action = app.action.take().unwrap();
    assert_eq!(action.verb, "ship.retire");
    assert_eq!(action.args["name"], "apps");
    app.form = None;
    app.demo = true;
    app.manage(Kind::New);
    assert!(app.form.is_none());
    assert!(app.action.is_none());
}
