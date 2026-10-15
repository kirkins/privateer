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
    fn proxy(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let proxy = self.root.join("privateer-proxy");
        fs::write(&proxy,format!("#!/bin/sh\nexport HOME='{}' PRIVATEER_FLEET='{}/fleet.sqlite' PRIVATEER_SERVICE=none PRIVATEER_RESOURCE_HOME='{}/ledger'\nunset PRIVATEER_ROLE PRIVATEER_ACTOR PRIVATEER_HOME HERDR_ENV HERDR_SOCKET_PATH\nexec '{}' \"$@\"\n",self.root.display(),self.root.display(),self.root.display(),env!("CARGO_BIN_EXE_privateer"))).unwrap();
        fs::set_permissions(&proxy, fs::Permissions::from_mode(0o755)).unwrap();
        proxy
    }
    fn send_batch(&self, folder: &Path, messages: Vec<Value>) -> Vec<Value> {
        use std::io::Write;
        let input = self.root.join("commands.jsonl");
        let mut file = fs::File::create(&input).unwrap();
        for (id, args) in messages.iter().enumerate() {
            writeln!(
                file,
                "{}",
                json!({"id":id,"command":"mail.send","args":args})
            )
            .unwrap();
        }
        drop(file);
        let output = self
            .command(env!("CARGO_BIN_EXE_privateer"), folder)
            .arg("rpc")
            .stdin(fs::File::open(input).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let replies: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), messages.len());
        for reply in &replies {
            assert_eq!(reply["ok"], true, "{reply}");
        }
        replies
            .into_iter()
            .map(|reply| reply["result"].clone())
            .collect()
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
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
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

fn screen(app: &mut privateer::dashboard::ui::App) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn replies_offer_only_reply_semantics_and_answer_in_the_originating_home() {
    use crossterm::event::KeyCode;
    use privateer::dashboard::{captain::InputMode, command, fleet, mailbox::Mailbox, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
    for kind in ["request", "decision"] {
        m.json(
            &apps,
            "mail.send",
            json!({"id":kind,"from":"apps","to":"captain","kind":kind,"body":"Please answer"}),
        );
        m.json(
            &infra,
            "mail.send",
            json!({"id":kind,"from":"infra","to":"captain","kind":kind,"body":"Other home"}),
        );
        let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
        let mut mailbox = Mailbox::default();
        mailbox.refresh(&proxy, &snapshot, None);
        assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
        let selected = mailbox
            .messages
            .iter()
            .position(|mail| mail.id == kind && mail.from == "apps")
            .unwrap();
        let mut app = App {
            snapshot,
            mail: mailbox.messages,
            ..App::default()
        };
        app.open_inbox();
        app.selected = selected;
        app.reply();
        app.edit_composer(KeyCode::Enter);
        assert!(app.action.is_none());
        assert!(app.composer.is_some());
        for c in "Proceed".chars() {
            app.edit_composer(KeyCode::Char(c));
        }
        for _ in 0..3 {
            app.edit_composer(KeyCode::Tab);
            assert_eq!(app.composer.as_ref().unwrap().mode, InputMode::Message);
            let view = screen(&mut app);
            assert!(view.contains("Enter: reply"));
            assert!(!view.contains("Tab: mode"));
            assert!(!view.contains("Read recorded state"));
            assert!(!view.contains("A side question"));
        }
        app.edit_composer(KeyCode::Enter);
        let action = app.action.take().unwrap();
        assert_eq!(action.home, apps.join(".privateer"));
        assert_eq!(action.args["kind"], "reply");
        assert_eq!(action.args["correlation"], kind);
        assert_eq!(action.args["body"], "Proceed");
        command::json(&proxy, &action.home, &action.verb, &action.args).unwrap();
        assert_eq!(
            m.json(&apps, "mail.show", json!({"id":kind}))["state"],
            "answered"
        );
        assert_eq!(
            m.json(&infra, "mail.show", json!({"id":kind}))["state"],
            "open"
        );
    }
}

#[test]
fn empty_status_reads_state_while_ask_and_message_require_text() {
    use crossterm::event::KeyCode;
    use privateer::dashboard::{captain::InputMode, command, fleet, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let proxy = m.proxy();
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut app = App {
        snapshot,
        ..App::default()
    };
    app.select_ship(Some("apps".into()));
    app.compose();
    app.edit_composer(KeyCode::Tab);
    assert_eq!(app.composer.as_ref().unwrap().mode, InputMode::Status);
    assert!(screen(&mut app).contains("Enter: status"));
    app.edit_composer(KeyCode::Enter);
    assert!(app.composer.is_none());
    assert!(app.action.is_none());
    assert_eq!(app.notice, "0 open requests · 0 unread mail · not aboard");
    for (mode, kind) in [(InputMode::Ask, "request"), (InputMode::Message, "notice")] {
        app.compose();
        while app.composer.as_ref().unwrap().mode != mode {
            app.edit_composer(KeyCode::Tab);
        }
        app.edit_composer(KeyCode::Enter);
        assert!(app.action.is_none());
        assert!(app.composer.is_some());
        app.edit_composer(KeyCode::Char('Q'));
        app.edit_composer(KeyCode::Enter);
        let action = app.action.take().unwrap();
        assert_eq!(action.args["kind"], kind);
        assert_eq!(action.args["correlation"], Value::Null);
        command::json(&proxy, &action.home, &action.verb, &action.args).unwrap();
        app.composer = None;
    }
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"apps"}))["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        m.json(&apps, "wake.list", json!({"ship":"apps"}))["wakes"],
        json!([])
    );
    app.demo = true;
    app.compose();
    app.edit_composer(KeyCode::Tab);
    app.edit_composer(KeyCode::Enter);
    assert!(app.composer.is_none());
    assert!(app.action.is_none());
    assert_eq!(app.exchanges.len(), 1);
    assert_eq!(app.exchanges[0].mode, InputMode::Status);
}

#[test]
fn full_pages_of_large_valid_mail_can_be_loaded_read_and_answered() {
    use privateer::dashboard::{command, fleet, mailbox::Mailbox};
    let m = Machine::new();
    let apps = m.ship("apps");
    let proxy = m.proxy();
    let sent = m.send_batch(&apps, (0..command::MAIL_PAGE_LIMIT).map(|i| {
        json!({"id":format!("large-{i}"),"from":"apps","to":"captain","kind":"request","body":"x".repeat(65_000)})
    }).collect());
    let home = apps.join(".privateer");
    for (verb, field) in [("mail.inbox", "messages"), ("mail.requests", "requests")] {
        let page = command::json(
            &proxy,
            &home,
            verb,
            &json!({"address":"captain","state":"open","limit":command::MAIL_PAGE_LIMIT}),
        )
        .unwrap();
        assert_eq!(
            page[field].as_array().unwrap().len(),
            command::MAIL_PAGE_LIMIT
        );
        assert_eq!(page[field][0]["body"].as_str().unwrap().len(), 65_000);
    }
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    assert_eq!(mailbox.messages.len(), command::MAIL_PAGE_LIMIT);
    mailbox
        .mark_read(&proxy, &mailbox.messages[0].clone())
        .unwrap();
    assert_eq!(
        m.json(&apps, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        sent[0]["seq"]
    );
    command::json(&proxy, &home, "mail.send", &json!({"from":"captain","to":"apps","kind":"reply","correlation":sent[0]["id"],"body":"Received"})).unwrap();
    assert_eq!(
        m.json(&apps, "mail.show", json!({"id":sent[0]["id"]}))["state"],
        "answered"
    );
}

#[test]
fn bounded_mailbox_admits_unread_mail_and_retains_evicted_read_markers() {
    use privateer::dashboard::{command, fleet, mailbox::Mailbox};
    let m = Machine::new();
    let homes: Vec<_> = (0..6).map(|i| m.ship(&format!("s{i}"))).collect();
    let proxy = m.proxy();
    for (i, home) in homes.iter().take(5).enumerate() {
        let sent = m.send_batch(home, (0..command::MAIL_PAGE_LIMIT).map(|n| {
            json!({"from":format!("s{i}"),"to":"captain","kind":"decision","body":format!("Decision {n}")})
        }).collect());
        m.json(
            home,
            "mail.ack",
            json!({"address":"captain","through_seq":sent.last().unwrap()["seq"]}),
        );
    }
    let notice = m.json(&homes[5], "mail.send", json!({"id":"earlier","from":"s5","to":"captain","kind":"notice","body":"Read before acknowledging"}));
    let decision = m.json(&homes[5], "mail.send", json!({"id":"later","from":"s5","to":"captain","kind":"decision","body":"Read later first"}));
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    assert_eq!(mailbox.messages.len(), 1000);
    assert_eq!(mailbox.messages[0].kind, "decision");
    assert!(mailbox.messages.iter().any(|m| m.id == notice["id"]));
    let read = mailbox
        .messages
        .iter()
        .find(|m| m.id == decision["id"])
        .unwrap()
        .clone();
    mailbox.mark_read(&proxy, &read).unwrap();
    assert_eq!(
        m.json(&homes[5], "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        0
    );
    let mut first_home_through = Value::Null;
    for (i, home) in homes.iter().take(5).enumerate() {
        let sent = m.send_batch(home, (0..command::MAIL_PAGE_LIMIT).map(|n| {
            json!({"from":format!("s{i}"),"to":"captain","kind":"notice","body":format!("New notice {n}")})
        }).collect());
        if i == 0 {
            first_home_through = sent.last().unwrap()["seq"].clone();
        }
    }
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    assert_eq!(mailbox.messages.len(), 1000);
    assert!(!mailbox.messages.iter().any(|m| m.id == decision["id"]));
    m.json(
        &homes[0],
        "mail.ack",
        json!({"address":"captain","through_seq":first_home_through}),
    );
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    let earlier = mailbox
        .messages
        .iter()
        .find(|m| m.id == notice["id"])
        .unwrap()
        .clone();
    mailbox.mark_read(&proxy, &earlier).unwrap();
    assert_eq!(
        m.json(&homes[5], "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        decision["seq"]
    );
}

#[test]
fn failed_fleet_observations_retain_mail_show_errors_and_never_open_missing_homes() {
    use privateer::dashboard::{fleet, mailbox::Mailbox, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
    for (folder, from) in [(&apps, "apps"), (&infra, "infra")] {
        m.json(
            folder,
            "mail.send",
            json!({"from":from,"to":"captain","kind":"notice","body":format!("Cached {from}")}),
        );
    }
    let mut mailbox = Mailbox::default();
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty());
    assert_eq!(mailbox.messages.len(), 2);
    fs::write(apps.join(".privateer/dashboard.json"), "invalid JSON").unwrap();
    fs::rename(infra.join(".privateer"), infra.join("saved-home")).unwrap();
    let failed = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    assert!(
        failed
            .ships
            .iter()
            .all(|s| s.live.as_ref().unwrap().error.is_some())
    );
    let must_not_run = m.root.join("must-not-run");
    mailbox.refresh(&must_not_run, &failed, Some(&infra.join(".privateer")));
    assert_eq!(mailbox.messages.len(), 2);
    assert_eq!(mailbox.errors.len(), 2);
    assert!(mailbox.errors.iter().all(|e| !e.contains("launch command")));
    assert!(!infra.join(".privateer").exists());
    let mut app = App {
        snapshot: failed,
        mail: mailbox.messages.clone(),
        mail_errors: mailbox.errors.clone(),
        ..App::default()
    };
    app.open_inbox();
    let view = screen(&mut app);
    assert!(view.contains("Mailbox unavailable"));
    assert!(view.contains("Cached"));
    app.mail.clear();
    let view = screen(&mut app);
    assert!(view.contains("Mailbox unavailable"));
    assert!(view.contains("No messages for the captain"));
    fs::remove_file(apps.join(".privateer/dashboard.json")).unwrap();
    fs::rename(infra.join("saved-home"), infra.join(".privateer")).unwrap();
    let recovered = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    mailbox.refresh(&proxy, &recovered, None);
    assert!(mailbox.errors.is_empty());
    assert_eq!(mailbox.messages.len(), 2);
    fs::remove_file(&proxy).unwrap();
    mailbox.refresh(&proxy, &recovered, None);
    assert_eq!(mailbox.errors.len(), 2);
    assert_eq!(mailbox.messages.len(), 2);
}
