#![cfg(feature = "dashboard")]
mod support;
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
        let mut c = support::command(binary);
        c.current_dir(folder)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
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
        fs::write(&proxy,format!("#!/bin/sh\nexport HOME='{}' XDG_CONFIG_HOME='{}/config' PRIVATEER_FLEET='{}/fleet.sqlite' PRIVATEER_SERVICE=none PRIVATEER_RESOURCE_HOME='{}/ledger'\nunset PRIVATEER_ROLE PRIVATEER_ACTOR PRIVATEER_HOME PRIVATEER_JOB PRIVATEER_PRESENCE_HOLDER PRIVATEER_WAKE PRIVATEER_CONTROL_CLI PRIVATEER_ATTENDED_HARNESS HERDR_ENV HERDR_SOCKET_PATH\nexec '{}' \"$@\"\n",self.root.display(),self.root.display(),self.root.display(),self.root.display(),env!("CARGO_BIN_EXE_privateer"))).unwrap();
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
    fn job(&self, folder: &Path, task: &str, id: &str) {
        use std::time::{Duration, Instant};
        self.json(folder, "job.start", json!({"task_id":task,"id":id,"command":["/bin/sh","-c",":"],"heavy":false,"timeout_secs":5}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let job = self.json(folder, "job.status", json!({"id":id}));
            if job["status"] == "succeeded" {
                return;
            }
            assert!(Instant::now() < deadline, "job did not succeed: {job}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn dashboard(&self) -> Value {
        self.dashboard_args(&["--once", "--no-herdr"])
    }
    fn dashboard_args(&self, args: &[&str]) -> Value {
        let out = self
            .command(env!("CARGO_BIN_EXE_privateer-dashboard"), &self.root)
            .args(args)
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
fn preset_contacts_are_inferred_only_when_unique_and_explicit_claims_still_validate() {
    use privateer::dashboard::{model::Snapshot, ui::App};

    fn contact(snapshot: Snapshot, expected: Option<&str>) {
        assert!(snapshot.presentation_error.is_none());
        let contacts: Vec<_> = snapshot
            .ships
            .iter()
            .filter(|ship| ship.liaison)
            .map(|ship| ship.id.as_str())
            .collect();
        assert_eq!(contacts, expected.into_iter().collect::<Vec<_>>());
        let mut app = App {
            snapshot,
            ..App::default()
        };
        app.open_liaison();
        assert_eq!(app.ship.as_deref(), expected);
        if expected.is_none() {
            assert!(app.notice.contains("No liaison configured"));
        }
        for id in ["alpha", "beta", "gamma"] {
            if app.snapshot.ships.iter().any(|ship| ship.id == id) {
                app.select_ship(Some(id.into()));
                assert!(!app.binding_ambiguous());
            }
        }
    }

    let m = Machine::new();
    let snapshot =
        || serde_json::from_value::<Snapshot>(m.dashboard()["snapshot"].clone()).unwrap();
    let mut configs = Vec::new();
    for name in ["alpha", "beta"] {
        let folder = m.root.join(name);
        fs::create_dir(&folder).unwrap();
        m.cli(
            &folder,
            &["init", "--ship", name, "--harness", "codex", "--liaison"],
        );
        let config = folder.join(".privateer/config.json");
        configs.push((config.clone(), fs::read(config).unwrap()));
        let observed = snapshot();
        assert_eq!(
            observed
                .ships
                .iter()
                .filter(|ship| ship.live.as_ref().is_some_and(|live| live.liaison))
                .count(),
            configs.len()
        );
        contact(observed, if name == "alpha" { Some("alpha") } else { None });
    }
    m.ship("gamma");
    contact(snapshot(), None);
    let alpha = m.root.join("alpha/.privateer/dashboard.json");
    let beta = m.root.join("beta/.privateer/dashboard.json");
    for preferred in ["gamma", "alpha", "beta"] {
        fs::write(
            &alpha,
            json!({"ships":[{"id":preferred,"name":preferred,"liaison":true}]}).to_string(),
        )
        .unwrap();
        contact(snapshot(), Some(preferred));
    }
    fs::write(
        &beta,
        json!({"ships":[{"id":"alpha","name":"Alpha","liaison":true}]}).to_string(),
    )
    .unwrap();
    let conflicted = snapshot();
    assert!(
        conflicted
            .presentation_error
            .as_ref()
            .unwrap()
            .contains("only one ship may be the liaison")
    );
    let mut app = App {
        snapshot: conflicted,
        ..App::default()
    };
    for id in ["alpha", "beta", "gamma"] {
        app.select_ship(Some(id.into()));
        assert!(app.binding_ambiguous());
    }
    let terminal = json!({"pane_id":"shared-pane","terminal_id":"shared-terminal"});
    fs::write(
        &alpha,
        json!({"ships":[{"id":"alpha","name":"Alpha","terminals":[terminal]}]}).to_string(),
    )
    .unwrap();
    fs::write(
        &beta,
        json!({"ships":[{"id":"beta","name":"Beta","terminals":[terminal]}]}).to_string(),
    )
    .unwrap();
    let conflicted = snapshot();
    assert!(
        conflicted
            .presentation_error
            .as_ref()
            .unwrap()
            .contains("a pane can belong to only one ship")
    );
    app.update_snapshot(conflicted);
    assert!(app.binding_ambiguous());
    fs::remove_file(alpha).unwrap();
    fs::remove_file(beta).unwrap();
    contact(snapshot(), None);
    for (path, bytes) in configs {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
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
fn fleet_harness_labels_support_bundled_legacy_and_custom_references_without_writes() {
    let m = Machine::new();
    let apps = m.ship("apps");
    let conn = rusqlite::Connection::open(apps.join(".privateer/privateer.sqlite")).unwrap();
    let raw: String = conn
        .query_row("SELECT record FROM ships WHERE name='apps'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: Value = serde_json::from_str(&raw).unwrap();
    for (reference, expected) in [
        (json!("bundled:codex"), "codex"),
        (json!("bundled:pi-signed"), "pi-signed"),
        (
            json!(m.root.join("deleted-checkout/plugins/claude/manifest.json")),
            "claude",
        ),
        (
            json!(m.root.join("custom/my-harness/manifest.json")),
            "my-harness",
        ),
        (Value::Null, ""),
    ] {
        record["first_mate"]["manifest"] = reference;
        let stored = record.to_string();
        conn.execute("UPDATE ships SET record=?1 WHERE name='apps'", [&stored])
            .unwrap();
        let view = m.dashboard();
        let ship = &view["snapshot"]["ships"][0];
        assert_eq!(ship["live"]["error"], Value::Null, "{view}");
        assert_eq!(ship["live"]["harness"], expected, "{view}");
        let after: String = conn
            .query_row("SELECT record FROM ships WHERE name='apps'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after, stored,
            "dashboard must not migrate manifest references"
        );
    }
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
    screen_width(app, 120)
}

fn screen_width(app: &mut privateer::dashboard::ui::App, width: u16) -> String {
    use ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(width, 34)).unwrap();
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
        let view = screen(&mut app);
        assert!(view.contains(&format!("Reply to the {kind} from apps:")));
        assert!(view.contains("“Please answer”"));
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
    assert!(
        mailbox
            .messages
            .iter()
            .all(|mail| mail.body.len() == 65_000)
    );
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
    use privateer::dashboard::{command, fleet, mailbox::Mailbox, ui::App};
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
    let mut app = App {
        snapshot: snapshot.clone(),
        mail: mailbox.messages.clone(),
        ..App::default()
    };
    app.open_inbox();
    app.selected = app
        .mail
        .iter()
        .position(|mail| mail.id == read.id && mail.home == read.home)
        .unwrap();
    app.scroll = 40;
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
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.scroll, 0);
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
fn bounded_mailbox_keeps_kind_priority_across_homes_and_failed_refreshes() {
    use privateer::dashboard::{command, fleet, mailbox::Mailbox};
    let m = Machine::new();
    let homes: Vec<_> = (0..6).map(|i| m.ship(&format!("s{i}"))).collect();
    let proxy = m.proxy();
    for (i, home) in homes.iter().take(5).enumerate() {
        m.send_batch(home, (0..command::MAIL_PAGE_LIMIT).map(|n| {
            json!({"id":format!("notice-{n}"),"from":format!("s{i}"),"to":"captain","kind":"notice","body":format!("Notice {n}")})
        }).collect());
    }
    m.json(
        &homes[5],
        "mail.send",
        json!({"id":"last-notice","from":"s5","to":"captain","kind":"notice","body":"Notice"}),
    );
    m.json(
        &homes[5],
        "mail.send",
        json!({"id":"question","from":"captain","to":"s5","kind":"request","body":"Status?"}),
    );
    m.json(&homes[5], "mail.send", json!({"id":"reply","from":"s5","to":"captain","kind":"reply","correlation":"question","body":"Ready"}));
    for kind in ["request", "decision"] {
        m.json(
            &homes[5],
            "mail.send",
            json!({"id":kind,"from":"s5","to":"captain","kind":kind,"body":"Needs attention"}),
        );
    }
    let mut snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    for failed in [false, true] {
        if failed {
            snapshot.ships[5].live.as_mut().unwrap().error = Some("offline".into());
            mailbox.refresh(&proxy, &snapshot, None);
            assert_eq!(mailbox.errors.len(), 1);
        }
        assert_eq!(mailbox.messages.len(), 1000);
        assert_eq!(
            mailbox
                .messages
                .iter()
                .take(3)
                .map(|m| m.kind.as_str())
                .collect::<Vec<_>>(),
            ["decision", "request", "reply"]
        );
        for mail in mailbox.messages.iter().take(3) {
            assert_eq!(mail.home.as_ref(), Some(&homes[5].join(".privateer")));
            assert_eq!(mail.from, "s5");
            assert!(!mail.acknowledged);
        }
        assert!(!mailbox.messages.iter().any(|m| m.id == "last-notice"));
        for home in homes.iter().take(5) {
            assert!(
                mailbox.messages.iter().any(
                    |m| m.id == "notice-0" && m.home.as_ref() == Some(&home.join(".privateer"))
                )
            );
        }
    }
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
            .presentation_error
            .as_ref()
            .unwrap()
            .contains("invalid dashboard.json")
    );
    assert!(failed.ships[0].live.as_ref().unwrap().error.is_none());
    assert!(failed.ships[1].live.as_ref().unwrap().error.is_some());
    mailbox.refresh(&proxy, &failed, Some(&infra.join(".privateer")));
    assert_eq!(mailbox.messages.len(), 2);
    assert_eq!(mailbox.errors.len(), 1);
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

#[test]
fn delegated_work_keeps_owners_origins_and_custody_visible_across_homes() {
    use privateer::dashboard::{fleet, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    m.ship("infra");
    let home = apps.join(".privateer");
    for (id, owner) in [
        ("delegated", "infra"),
        ("transferred", "crew"),
        ("unregistered", "crew"),
    ] {
        m.json(
            &apps,
            "task.create",
            json!({"id":id,"owner":owner,"title":format!("Assignment {id}"),"charter_id":"apps"}),
        );
        m.json(&apps, "event.emit", json!({"id":format!("event-{id}"),"task_id":id,"issue_key":"review","kind":"blocked","payload":{"summary":format!("Fact {id}")}}));
        m.job(&apps, id, &format!("job-{id}"));
    }
    m.json(
        &apps,
        "task.handoff",
        json!({"id":"transferred","actor":"crew","target":"infra"}),
    );
    m.json(
        &apps,
        "task.accept",
        json!({"id":"transferred","actor":"infra"}),
    );
    let view = m.dashboard();
    let snapshot = &view["snapshot"];
    assert!(view["mail_errors"].as_array().unwrap().is_empty());
    for (id, owner) in [
        ("delegated", "infra"),
        ("transferred", "infra"),
        ("unregistered", "crew"),
    ] {
        let rows: Vec<_> = snapshot["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["id"] == id)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["owner"], owner);
        assert_eq!(rows[0]["home"], json!(home));
        for section in ["pending", "activity", "jobs"] {
            let rows: Vec<_> = snapshot[section]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["task_id"] == id)
                .collect();
            assert!(!rows.is_empty(), "missing {section} for {id}");
            for row in rows {
                assert_eq!(row["owner"], owner);
                assert_eq!(row["home"], json!(home));
            }
        }
    }
    let custody: Vec<_> = snapshot["custody"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["task_id"] == "transferred")
        .collect();
    assert_eq!(custody.len(), 2);
    assert!(
        custody
            .iter()
            .any(|r| r["actor"] == "crew" && r["kind"] == "task.handoff")
    );
    assert!(
        custody
            .iter()
            .any(|r| r["actor"] == "infra" && r["kind"] == "task.accept")
    );
    for row in custody {
        assert_eq!(row["owner"], "infra");
        assert_eq!(row["home"], json!(home));
    }
    let snapshot = serde_json::from_value(snapshot.clone()).unwrap();
    let mut app = App {
        snapshot,
        compact: false,
        ..App::default()
    };
    app.select_ship(Some("infra".into()));
    app.select_tab(1);
    assert_eq!(
        app.assignment_indices()
            .iter()
            .filter(|&&i| app.snapshot.tasks[i].home.as_ref() == Some(&home))
            .count(),
        2
    );
    app.select_tab(0);
    // Keep the complete origin visible even when the test root is a long
    // isolated worktree path. Normal-width rendering is covered separately.
    let bridge = screen_width(&mut app, 240);
    assert!(bridge.contains("Fact delegated"));
    assert!(bridge.contains("job-delegated"));
    assert!(bridge.contains(&home.display().to_string()));
    app.select_tab(3);
    let logbook = screen(&mut app);
    assert!(logbook.contains("Fact transferred"));
    assert!(logbook.contains("accepted responsibility"));
    app.select_ship(Some("crew".into()));
    app.select_tab(1);
    assert_eq!(app.assignment_indices().len(), 1);
    assert_eq!(app.selected_task().unwrap().id, "unregistered");
    assert!(
        app.snapshot
            .ships
            .iter()
            .find(|s| s.id == "crew")
            .unwrap()
            .live
            .is_none()
    );
    app.compose();
    assert!(app.composer.is_none());
    app.select_tab(3);
    assert!(screen(&mut app).contains("offered a handoff"));
    let local = fleet::read(&m.root.join("absent-registry.sqlite"), Some(&home)).unwrap();
    assert!(local.tasks.iter().all(|r| r.home.as_ref() == Some(&home)));
    assert!(local.pending.iter().all(|r| r.home.as_ref() == Some(&home)));
    assert!(
        local
            .activity
            .iter()
            .all(|r| r.home.as_ref() == Some(&home))
    );
    assert!(local.jobs.iter().all(|r| r.home.as_ref() == Some(&home)));
    assert!(local.custody.iter().all(|r| r.home.as_ref() == Some(&home)));
}

#[test]
fn duplicate_task_ids_join_jobs_and_preserve_selection_by_origin_during_handoffs() {
    use privateer::dashboard::{fleet, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let apps_home = apps.join(".privateer");
    let infra_home = infra.join(".privateer");
    for (folder, owner) in [(&apps, "apps"), (&infra, "infra")] {
        m.json(
            folder,
            "task.create",
            json!({"id":"build","owner":owner,"title":format!("{owner} build"),"charter_id":owner}),
        );
        m.json(folder, "event.emit", json!({"id":"same-event","task_id":"build","issue_key":"build","kind":"blocked","payload":{"summary":format!("{owner} build fact")}}));
        m.job(folder, "build", &format!("{owner}-job"));
    }
    let registry = m.root.join("fleet.sqlite");
    let mut app = App {
        snapshot: fleet::read(&registry, None).unwrap(),
        ..App::default()
    };
    app.select_ship(Some("apps".into()));
    app.select_tab(1);
    app.selected = app
        .assignment_indices()
        .iter()
        .position(|&i| app.snapshot.tasks[i].id == "build")
        .unwrap();
    let assignment = screen_width(&mut app, 240);
    assert!(assignment.contains("Job apps-job"));
    assert!(!assignment.contains("Job infra-job"));
    assert!(assignment.contains(&apps_home.display().to_string()));
    m.json(
        &apps,
        "task.handoff",
        json!({"id":"build","actor":"apps","target":"infra"}),
    );
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    app.select_ship(Some("infra".into()));
    app.select_tab(1);
    assert_eq!(
        app.assignment_indices()
            .iter()
            .filter(|&&i| app.snapshot.tasks[i].id == "build")
            .count(),
        2
    );
    app.selected = app
        .assignment_indices()
        .iter()
        .position(|&i| {
            app.snapshot.tasks[i].id == "build"
                && app.snapshot.tasks[i].home.as_ref() == Some(&infra_home)
        })
        .unwrap();
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    assert_eq!(
        app.selected_task().unwrap().home.as_ref(),
        Some(&infra_home)
    );
    assert_eq!(app.selected_task().unwrap().title, "infra build");
    let assignment = screen(&mut app);
    assert!(assignment.contains("Job infra-job"));
    assert!(!assignment.contains("Job apps-job"));
    app.selected = app
        .assignment_indices()
        .iter()
        .position(|&i| {
            app.snapshot.tasks[i].id == "build"
                && app.snapshot.tasks[i].home.as_ref() == Some(&apps_home)
        })
        .unwrap();
    let incoming = screen(&mut app);
    assert!(incoming.contains("Offered to infra"));
    assert!(incoming.contains("Job apps-job"));
    assert!(!incoming.contains("Job infra-job"));
    m.json(&apps, "task.accept", json!({"id":"build","actor":"infra"}));
    let mut refreshed = fleet::read(&registry, None).unwrap();
    refreshed.tasks.reverse();
    app.update_snapshot(refreshed);
    assert_eq!(app.selected_task().unwrap().home.as_ref(), Some(&apps_home));
    assert_eq!(app.selected_task().unwrap().owner, "infra");
    assert!(app.selected_task().unwrap().handoff_to.is_none());
    let accepted = screen(&mut app);
    assert!(accepted.contains("Job apps-job"));
    assert!(!accepted.contains("Job infra-job"));
    assert_eq!(
        m.json(&infra, "task.show", json!({"id":"build"}))["owner"],
        "infra"
    );
    let view = m.dashboard();
    let snapshot = &view["snapshot"];
    assert_eq!(
        snapshot["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["id"] == "build")
            .count(),
        2
    );
    for section in ["pending", "activity", "jobs"] {
        let rows: Vec<_> = snapshot[section]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["task_id"] == "build")
            .collect();
        assert_eq!(rows.len(), 2, "{section}");
        assert!(rows.iter().any(|r| r["home"] == json!(apps_home)));
        assert!(rows.iter().any(|r| r["home"] == json!(infra_home)));
        assert!(rows.iter().all(|r| r["owner"] == "infra"));
    }
}

#[test]
fn demo_work_origins_stay_sample_only_without_creating_state() {
    let m = Machine::new();
    let view = m.dashboard_args(&["--demo", "--once"]);
    assert_eq!(view["demo"], true);
    for section in ["tasks", "pending", "activity", "jobs", "custody"] {
        let rows = view["snapshot"][section].as_array().unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row["home"].is_null()));
    }
    assert_eq!(fs::read_dir(&m.root).unwrap().count(), 0);
}

#[test]
fn read_requests_beyond_the_inbox_page_survive_refresh_and_ack_when_reachable() {
    use privateer::dashboard::{command, fleet, mailbox::Mailbox};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
    m.send_batch(&apps, (0..command::MAIL_PAGE_LIMIT).map(|i| {
        json!({"id":format!("notice-{i}"),"from":"apps","to":"captain","kind":"notice","body":"Earlier unseen notice"})
    }).collect());
    let request = m.json(&apps, "mail.send", json!({"id":"later-request","from":"apps","to":"captain","kind":"request","body":"Read beyond the inbox page"}));
    let gap = m.json(&apps, "mail.send", json!({"id":"unseen-gap","from":"apps","to":"captain","kind":"notice","body":"Read this before the later decision can be acknowledged"}));
    let last_read = m.json(&apps, "mail.send", json!({"id":"later-decision","from":"apps","to":"captain","kind":"decision","body":"Read beyond an unseen gap"}));
    m.json(&infra, "mail.send", json!({"id":"later-request","from":"infra","to":"captain","kind":"request","body":"Independent unread request"}));
    let unseen = m.json(&apps, "mail.send", json!({"id":"last-unseen","from":"apps","to":"captain","kind":"notice","body":"Leave this unread"}));
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let home = apps.join(".privateer");
    let page = m.json(
        &apps,
        "mail.inbox",
        json!({"address":"captain","limit":command::MAIL_PAGE_LIMIT}),
    );
    assert_eq!(
        page["messages"].as_array().unwrap().len(),
        command::MAIL_PAGE_LIMIT
    );
    assert!(
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|mail| mail["kind"] == "notice")
    );
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    for kind in ["request", "decision"] {
        let mail = mailbox
            .messages
            .iter()
            .find(|mail| mail.id == format!("later-{kind}") && mail.home.as_ref() == Some(&home))
            .unwrap()
            .clone();
        mailbox.mark_read(&proxy, &mail).unwrap();
        mailbox.refresh(&proxy, &snapshot, None);
        assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
        assert_eq!(
            m.json(&apps, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
            0
        );
    }
    mailbox.refresh(&m.root.join("missing-binary"), &snapshot, None);
    assert_eq!(mailbox.errors.len(), 2);
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty());
    let arrival = m.json(&apps, "mail.send", json!({"id":"new-arrival","from":"apps","to":"captain","kind":"notice","body":"Arrived after the explicit reads"}));
    for i in 0..command::MAIL_PAGE_LIMIT {
        let notice = mailbox
            .messages
            .iter()
            .find(|mail| mail.id == format!("notice-{i}"))
            .unwrap()
            .clone();
        mailbox.mark_read(&proxy, &notice).unwrap();
        mailbox.refresh(&proxy, &snapshot, None);
        assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
    }
    let inbox = m.json(&apps, "mail.inbox", json!({"address":"captain"}));
    assert_eq!(inbox["acknowledged_through"], request["seq"]);
    assert_eq!(inbox["messages"][0]["id"], gap["id"]);
    let gap = mailbox
        .messages
        .iter()
        .find(|mail| mail.id == "unseen-gap")
        .unwrap()
        .clone();
    mailbox.mark_read(&proxy, &gap).unwrap();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty());
    let inbox = m.json(&apps, "mail.inbox", json!({"address":"captain"}));
    assert_eq!(inbox["acknowledged_through"], last_read["seq"]);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 2);
    assert_eq!(inbox["messages"][0]["id"], unseen["id"]);
    assert_eq!(inbox["messages"][1]["id"], arrival["id"]);
    assert_eq!(
        m.json(&infra, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        0
    );
    mailbox.refresh(&proxy, &snapshot, None);
    for kind in ["request", "decision"] {
        assert!(
            mailbox
                .messages
                .iter()
                .any(|mail| mail.id == format!("later-{kind}")
                    && mail.home.as_ref() == Some(&home)
                    && mail.acknowledged)
        );
        assert_eq!(
            m.json(&apps, "mail.show", json!({"id":format!("later-{kind}")}))["state"],
            "open"
        );
    }
}

#[test]
fn live_labels_show_mail_counts_and_failures_in_fleet_inbox_and_ship_views() {
    use privateer::dashboard::{fleet, mailbox::Mailbox, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
    let registry = m.root.join("fleet.sqlite");
    let mut mailbox = Mailbox::default();
    let mut app = App {
        snapshot: fleet::read(&registry, None).unwrap(),
        ..App::default()
    };
    mailbox.refresh(&proxy, &app.snapshot, None);
    assert!(mailbox.errors.is_empty());
    let empty = screen(&mut app);
    assert!(empty.contains("LIVE / fleet · captain mail"));
    assert!(empty.contains("No messages"));
    assert!(!empty.contains("READ-ONLY / local home"));
    assert!(!empty.contains("Not connected"));
    for (folder, from) in [(&apps, "apps"), (&infra, "infra")] {
        m.json(
            folder,
            "mail.send",
            json!({"from":from,"to":"captain","kind":"notice","body":format!("Mail from {from}")}),
        );
    }
    mailbox.refresh(&proxy, &app.snapshot, None);
    app.mail = mailbox.messages.clone();
    let populated = screen(&mut app);
    assert!(populated.contains("2 unread"));
    assert!(!populated.contains("Not connected"));
    fs::rename(infra.join(".privateer"), infra.join("saved-home")).unwrap();
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    mailbox.refresh(&proxy, &app.snapshot, None);
    app.mail = mailbox.messages.clone();
    app.mail_errors = mailbox.errors.clone();
    assert_eq!(app.mail.len(), 2);
    assert_eq!(app.mail_errors.len(), 1);
    for view in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
        match view {
            None => app.select_ship(None),
            Some(4) => app.open_inbox(),
            Some(tab) => {
                app.select_ship(Some("apps".into()));
                app.select_tab(tab);
            }
        }
        let failed = screen(&mut app);
        assert!(failed.contains("LIVE / fleet · captain mail"));
        assert!(failed.contains("2 unread · 1 error"));
        assert!(failed.contains("MAILBOX UNAVAILABLE"));
        assert!(!failed.contains("READ-ONLY / local home"));
        assert!(!failed.contains("Not connected"));
        assert!(!failed.contains("Conversation and agent launch are not connected"));
    }
    fs::rename(apps.join(".privateer"), apps.join("saved-home")).unwrap();
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    mailbox.refresh(&proxy, &app.snapshot, None);
    app.mail_errors = mailbox.errors.clone();
    assert!(screen(&mut app).contains("2 unread · 2 errors"));
    for folder in [&apps, &infra] {
        fs::rename(folder.join("saved-home"), folder.join(".privateer")).unwrap();
    }
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    mailbox.refresh(&proxy, &app.snapshot, None);
    app.mail = mailbox.messages;
    app.mail_errors = mailbox.errors;
    let recovered = screen(&mut app);
    assert!(recovered.contains("2 unread"));
    assert!(!recovered.contains("MAILBOX UNAVAILABLE"));
    assert!(app.mail_errors.is_empty());
}

fn scroll_to(app: &mut privateer::dashboard::ui::App, text: &str) -> bool {
    for offset in (0..2000).step_by(8) {
        app.scroll = offset;
        let rendered = screen(app);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\0'));
        if rendered.contains(text) {
            return true;
        }
    }
    false
}

#[test]
fn full_mail_bodies_and_payloads_are_readable_before_ack_and_after_leaving_the_inbox() {
    use privateer::dashboard::{fleet, mailbox::Mailbox, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let proxy = m.proxy();
    let body = format!(
        "Start of full body\n{} BODY-MIDDLE {}\nFULL-BODY-END\x1b[2J\u{202e}\0",
        "x".repeat(32_000),
        "z".repeat(32_800)
    );
    let payload = json!({"choice":"approve or decline","note":format!("{} PAYLOAD-END \u{202e}", "y".repeat(10_000))});
    m.json(&apps, "mail.send", json!({"id":"large-body","from":"apps","to":"captain","kind":"request","body":body,"payload":{"body_metadata":"BODY-PAYLOAD"}}));
    let decision = m.json(&apps, "mail.send", json!({"id":"payload-only","from":"apps","to":"captain","kind":"decision","payload":payload}));
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    for acknowledged in [false, true] {
        if acknowledged {
            m.json(
                &apps,
                "mail.ack",
                json!({"address":"captain","through_seq":decision["seq"]}),
            );
            assert!(
                m.json(&apps, "mail.inbox", json!({"address":"captain"}))["messages"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        mailbox.refresh(&proxy, &snapshot, None);
        assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
        assert_eq!(mailbox.messages.len(), 2);
        assert!(
            mailbox.messages.iter().all(
                |mail| mail.subject.chars().count() <= 100 && mail.acknowledged == acknowledged
            )
        );
        let full = mailbox
            .messages
            .iter()
            .find(|mail| mail.id == "large-body")
            .unwrap();
        assert_eq!(full.body, body);
        assert_eq!(full.payload, json!({"body_metadata":"BODY-PAYLOAD"}));
        let payload_only = mailbox
            .messages
            .iter()
            .find(|mail| mail.id == "payload-only")
            .unwrap();
        assert!(payload_only.body.is_empty());
        assert!(!payload_only.subject.is_empty());
        assert_eq!(payload_only.payload, payload);
        let mut app = App {
            snapshot: snapshot.clone(),
            mail: mailbox.messages.clone(),
            ..App::default()
        };
        app.open_inbox();
        app.selected = app
            .mail
            .iter()
            .position(|mail| mail.id == "large-body")
            .unwrap();
        assert!(scroll_to(&mut app, "BODY-MIDDLE"));
        assert!(scroll_to(&mut app, "FULL-BODY-END"));
        assert!(scroll_to(&mut app, "BODY-PAYLOAD"));
        app.selected = app
            .mail
            .iter()
            .position(|mail| mail.id == "payload-only")
            .unwrap();
        app.scroll = 0;
        assert!(screen(&mut app).contains("Payload (JSON):"));
        assert!(scroll_to(&mut app, "PAYLOAD-END"));
        assert_eq!(
            m.json(&apps, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
            if acknowledged {
                decision["seq"].clone()
            } else {
                json!(0)
            }
        );
    }
    mailbox.refresh(&m.root.join("missing-binary"), &snapshot, None);
    assert_eq!(mailbox.errors.len(), 1);
    assert_eq!(
        mailbox
            .messages
            .iter()
            .find(|mail| mail.id == "large-body")
            .unwrap()
            .body,
        body
    );
    assert_eq!(
        mailbox
            .messages
            .iter()
            .find(|mail| mail.id == "payload-only")
            .unwrap()
            .payload,
        payload
    );
}

#[test]
fn fleet_presentation_conflicts_disable_navigation_without_losing_work_and_recover() {
    use privateer::dashboard::{fleet, herdr::View, model::Snapshot, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    for (folder, owner) in [(&apps, "apps"), (&infra, "infra")] {
        m.json(
            folder,
            "task.create",
            json!({"id":"build","charter_id":owner,"owner":owner,"title":format!("{owner} build")}),
        );
        m.json(folder, "event.emit", json!({"id":"fact","task_id":"build","issue_key":"build","kind":"blocked","payload":{"summary":format!("{owner} fact")}}));
        m.job(folder, "build", &format!("{owner}-job"));
    }
    m.json(
        &apps,
        "task.handoff",
        json!({"id":"build","actor":"apps","target":"infra"}),
    );
    m.json(&apps, "task.accept", json!({"id":"build","actor":"infra"}));
    let config = |id: &str, pane: &str, terminal: &str, liaison: bool| json!({"ships":[{"id":id,"name":"Shared display label","liaison":liaison,"terminals":[{"pane_id":pane,"terminal_id":terminal}]}]});
    let apps_config = apps.join(".privateer/dashboard.json");
    let infra_config = infra.join(".privateer/dashboard.json");
    let herdr = View::demo();
    m.json(
        &apps,
        "ship.attend",
        json!({"ship":"apps","holder":"automatic","ttl_secs":3600}),
    );
    let binding = json!({"socket":"/isolated/socket","socket_identity":herdr.socket_identity,"pane":herdr.panes[1]});
    rusqlite::Connection::open(apps.join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE presence SET binding=?1 WHERE ship='apps'",
            [binding.to_string()],
        )
        .unwrap();
    fs::write(
        &apps_config,
        config("apps", "demo:apps", "demo-terminal-apps", true).to_string(),
    )
    .unwrap();
    fs::write(
        &infra_config,
        config(
            "infra",
            "demo:infrastructure",
            "demo-terminal-infrastructure",
            false,
        )
        .to_string(),
    )
    .unwrap();
    let registry = m.root.join("fleet.sqlite");
    let scope = "é".repeat(300);
    m.cli(
        &apps,
        &[
            "init",
            "--ship",
            "apps",
            "--harness",
            "codex",
            "--scope",
            &scope,
        ],
    );
    assert_eq!(
        m.json(&apps, "ship.show", json!({"name":"apps"}))["scope"],
        scope
    );
    let baseline = m.dashboard()["snapshot"].clone();
    assert!(baseline["presentation_error"].is_null());
    assert_eq!(
        baseline["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ship| ship["id"] == "apps")
            .unwrap()["description"],
        scope
    );
    let mut app = App {
        snapshot: fleet::read(&registry, None).unwrap(),
        herdr: Some(herdr),
        ..App::default()
    };
    app.open_liaison();
    assert_eq!(app.ship.as_deref(), Some("apps"));
    assert_eq!(app.selected_pane().unwrap().pane_id, "demo:apps");
    assert!(screen(&mut app).contains(&"é".repeat(30)));
    app.select_ship(Some("infra".into()));
    assert_eq!(app.selected_pane().unwrap().pane_id, "demo:infrastructure");
    for (pane, terminal, liaison, reason) in [
        (
            "demo:apps",
            "demo-terminal-apps",
            false,
            "a pane can belong to only one ship",
        ),
        (
            "demo:apps",
            "different-terminal",
            false,
            "a pane can belong to only one ship",
        ),
        (
            "demo:infrastructure",
            "demo-terminal-infrastructure",
            true,
            "only one ship may be the liaison",
        ),
    ] {
        fs::write(
            &infra_config,
            config("infra", pane, terminal, liaison).to_string(),
        )
        .unwrap();
        Snapshot::read(&apps.join(".privateer")).unwrap();
        Snapshot::read(&infra.join(".privateer")).unwrap();
        let view = m.dashboard();
        let failed = &view["snapshot"];
        assert!(
            failed["presentation_error"]
                .as_str()
                .unwrap()
                .contains(reason)
        );
        for section in ["tasks", "pending", "activity", "jobs", "custody"] {
            assert_eq!(failed[section], baseline[section], "{section}");
        }
        assert!(view["mail_errors"].as_array().unwrap().is_empty());
        app.update_snapshot(fleet::read(&registry, None).unwrap());
        assert!(
            app.snapshot
                .ships
                .iter()
                .all(|ship| !ship.liaison && ship.terminals.len() == 1)
        );
        assert_eq!(
            serde_json::to_value(
                app.snapshot.ships[0]
                    .live
                    .as_ref()
                    .unwrap()
                    .binding
                    .as_ref()
                    .unwrap()
            )
            .unwrap(),
            binding
        );
        for id in ["apps", "infra"] {
            app.select_ship(Some(id.into()));
            assert!(app.binding_ambiguous());
            for tab in [0, 2] {
                app.select_tab(tab);
                assert!(app.scoped_panes().is_empty());
                assert!(app.selected_pane().is_none());
                assert!(screen(&mut app).contains("PRESENTATION UNAVAILABLE"));
            }
        }
        let mut truncated = App {
            snapshot: app.snapshot.clone(),
            herdr: app.herdr.clone(),
            ..App::default()
        };
        truncated.snapshot.ships.truncate(1);
        truncated.snapshot.truncated = true;
        truncated.select_ship(Some("apps".into()));
        assert!(truncated.binding_ambiguous());
        assert!(truncated.scoped_panes().is_empty());
        assert!(truncated.selected_pane().is_none());
        app.select_ship(None);
        app.open_liaison();
        assert!(app.ship.is_none());
        assert!(app.notice.contains(reason));
        app.compose();
        assert!(app.composer.is_none());
        assert_eq!(
            app.snapshot
                .tasks
                .iter()
                .filter(|task| task.id == "build" && task.owner == "infra")
                .count(),
            2
        );
    }
    fs::write(
        &apps_config,
        config("apps", "demo:apps", "demo-terminal-apps", false).to_string(),
    )
    .unwrap();
    fs::write(
        &infra_config,
        config(
            "infra",
            "demo:infrastructure",
            "demo-terminal-infrastructure",
            true,
        )
        .to_string(),
    )
    .unwrap();
    app.update_snapshot(fleet::read(&registry, None).unwrap());
    assert!(app.snapshot.presentation_error.is_none());
    app.open_liaison();
    assert_eq!(app.ship.as_deref(), Some("infra"));
    assert_eq!(app.selected_pane().unwrap().pane_id, "demo:infrastructure");
    assert!(!screen(&mut app).contains("PRESENTATION UNAVAILABLE"));
}

#[test]
fn foreign_and_invalid_local_claims_block_navigation_without_hiding_durable_work() {
    use privateer::dashboard::{herdr::View, model::Snapshot, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    for (folder, owner) in [(&apps, "apps"), (&infra, "infra")] {
        m.json(
            folder,
            "task.create",
            json!({"id":"work","charter_id":owner,"owner":owner,"title":"Recorded work"}),
        );
        m.json(folder, "event.emit", json!({"id":"fact","task_id":"work","issue_key":"review","kind":"blocked","payload":{"summary":"Durable fact"}}));
        m.job(folder, "work", "job");
        m.json(
            folder,
            "mail.send",
            json!({"from":owner,"to":"captain","kind":"notice","body":"Still visible"}),
        );
    }
    m.json(
        &apps,
        "task.handoff",
        json!({"id":"work","actor":"apps","target":"infra"}),
    );
    m.json(&apps, "task.accept", json!({"id":"work","actor":"infra"}));
    let herdr = View::demo();
    let pane = &herdr.panes[1];
    let binding =
        json!({"socket":"/isolated/socket","socket_identity":herdr.socket_identity,"pane":pane});
    m.json(
        &apps,
        "ship.attend",
        json!({"ship":"apps","holder":"automatic","ttl_secs":3600}),
    );
    rusqlite::Connection::open(apps.join(".privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE presence SET binding=?1 WHERE ship='apps'",
            [binding.to_string()],
        )
        .unwrap();
    let apps_config = apps.join(".privateer/dashboard.json");
    let infra_config = infra.join(".privateer/dashboard.json");
    let terminal = json!({"pane_id":pane.pane_id,"terminal_id":pane.terminal_id});
    let own = json!({"id":"apps","name":"Apps","liaison":true});
    let foreign = json!({"id":"infra","name":"Infra","terminals":[terminal]});
    let conflicting = json!({"id":"apps","name":"Apps","liaison":true,"terminals":[terminal]});
    let baseline = m.dashboard()["snapshot"].clone();
    let mut overflow = vec![own.clone()];
    overflow.extend((0..198).map(|i| json!({"id":format!("peer-{i}"),"name":"Peer"})));
    overflow.push(json!({"id":"crew","name":"Crew","terminals":[terminal]}));
    for (source, config, peer) in [
        (&apps_config, json!({"ships":[own, foreign]}).to_string(), "infra"),
        (&apps_config, json!({"ships":[own, {"id":"infra","name":"Infra","terminals":[{"pane_id":pane.pane_id,"terminal_id":"older-terminal"}]}]}).to_string(), "infra"),
        (&apps_config, json!({"ships":[own, {"id":"crew","name":"Crew","terminals":[terminal]}]}).to_string(), "crew"),
        (&apps_config, json!({"ships":[conflicting, foreign]}).to_string(), "infra"),
        (&infra_config, json!({"ships":[conflicting, foreign]}).to_string(), "infra"),
        (&apps_config, json!({"ships":[own, {"id":"infra","name":"","terminals":[terminal]}]}).to_string(), "infra"),
        (&apps_config, "invalid JSON".into(), ""),
        (&apps_config, json!({"ships":overflow}).to_string(), ""),
    ] {
        fs::write(&apps_config, json!({"ships":[own]}).to_string()).unwrap();
        fs::write(&infra_config, "{\"ships\":[]}").unwrap();
        fs::write(source, config).unwrap();
        let result = m.dashboard();
        let snapshot: Snapshot = serde_json::from_value(result["snapshot"].clone()).unwrap();
        assert!(snapshot.presentation_error.is_some());
        for section in ["tasks", "pending", "activity", "jobs", "custody"] {
            assert_eq!(result["snapshot"][section], baseline[section], "{section}");
        }
        assert!(result["mail_errors"].as_array().unwrap().is_empty());
        assert_eq!(result["mail"].as_array().unwrap().len(), 2);
        if !peer.is_empty() {
            assert!(snapshot.ships.iter().any(|ship| ship.id == peer && ship.terminals.iter().any(|b| b.pane_id == pane.pane_id)));
        }
        let mut app = App { snapshot, herdr: Some(herdr.clone()), ..App::default() };
        for id in ["apps", peer].into_iter().filter(|id| !id.is_empty()) {
            app.select_ship(Some(id.into()));
            assert!(app.binding_ambiguous(), "{id}");
            for tab in [0, 2] {
                app.select_tab(tab);
                assert!(app.scoped_panes().is_empty(), "{id}");
                assert!(app.selected_pane().is_none(), "{id}");
            }
        }
        app.select_ship(None);
        app.open_liaison();
        assert!(app.selected_pane().is_none());
    }
    for path in [&apps_config, &infra_config] {
        fs::write(
            path,
            json!({"ships":[{"id":"apps","name":"Apps","liaison":true,"terminals":[terminal]}]})
                .to_string(),
        )
        .unwrap();
    }
    let recovered = m.dashboard();
    assert!(recovered["snapshot"]["presentation_error"].is_null());
    let mut app = App {
        snapshot: serde_json::from_value(recovered["snapshot"].clone()).unwrap(),
        herdr: Some(herdr.clone()),
        ..App::default()
    };
    app.open_liaison();
    assert_eq!(app.selected_pane(), Some(pane));
    assert_eq!(
        app.snapshot
            .ships
            .iter()
            .find(|s| s.id == "apps")
            .unwrap()
            .terminals
            .len(),
        1
    );
}

#[test]
fn local_presentation_metadata_and_terminal_fields_still_require_valid_configuration() {
    use privateer::dashboard::model::Snapshot;
    let m = Machine::new();
    let apps = m.ship("apps");
    let home = apps.join(".privateer");
    let config = home.join("dashboard.json");
    for (ship, reason) in [
        (
            json!({"id":"apps","name":"Apps","description":"é".repeat(300)}),
            "invalid ship name/description length",
        ),
        (
            json!({"id":"apps","name":"Apps","terminals":[{"pane_id":"pane","terminal_id":""}]}),
            "invalid terminal binding",
        ),
    ] {
        fs::write(&config, json!({"ships":[ship]}).to_string()).unwrap();
        let error = Snapshot::read(&home).unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
        let fleet = m.dashboard();
        assert!(
            fleet["snapshot"]["presentation_error"]
                .as_str()
                .unwrap()
                .contains(reason)
        );
    }
    fs::write(&config, json!({"ships":[{"id":"apps","name":"Apps","description":"Valid local description","terminals":[{"pane_id":"pane","terminal_id":"terminal"}]}]}).to_string()).unwrap();
    assert_eq!(
        Snapshot::read(&home)
            .unwrap()
            .ships
            .iter()
            .find(|ship| ship.id == "apps")
            .unwrap()
            .terminals
            .len(),
        1
    );
}

#[test]
fn mailbox_refresh_preserves_scroll_by_home_and_id_and_resets_after_ack_and_reply() {
    use privateer::dashboard::{fleet, mailbox::Mailbox, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let proxy = m.proxy();
    let apps_home = apps.join(".privateer");
    let infra_home = infra.join(".privateer");
    let notice_body = format!("{}LONG-NOTICE-END", "Full notice line\n".repeat(100));
    m.json(
        &apps,
        "mail.send",
        json!({"id":"same-id","from":"apps","to":"captain","kind":"notice","body":notice_body}),
    );
    m.json(&infra, "mail.send", json!({"id":"same-id","from":"infra","to":"captain","kind":"notice","body":"Replacement from infra"}));
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let mut mailbox = Mailbox::default();
    mailbox.refresh(&proxy, &snapshot, None);
    assert!(mailbox.errors.is_empty());
    let mut app = App {
        snapshot: snapshot.clone(),
        ..App::default()
    };
    app.open_inbox();
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail[app.selected].home.as_ref(), Some(&apps_home));
    assert!(scroll_to(&mut app, "LONG-NOTICE-END"));
    let offset = app.scroll;
    assert!(offset > 0);
    let mut reordered = mailbox.messages.clone();
    reordered.reverse();
    app.update_mail(reordered, Vec::new());
    assert_eq!(app.selected, 1);
    assert_eq!(app.scroll, offset);
    assert_eq!(app.mail[app.selected].home.as_ref(), Some(&apps_home));
    mailbox.refresh(&m.root.join("missing-binary"), &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail_errors.len(), 2);
    assert_eq!(app.scroll, offset);
    assert_eq!(app.mail[app.selected].home.as_ref(), Some(&apps_home));
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.scroll, offset);
    assert!(screen(&mut app).contains("LONG-NOTICE-END"));
    mailbox.mark_read(&proxy, &app.mail[app.selected]).unwrap();
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail[app.selected].id, "same-id");
    assert_eq!(app.mail[app.selected].home.as_ref(), Some(&infra_home));
    assert_eq!(app.scroll, 0);
    assert!(screen(&mut app).contains("Replacement from infra"));
    assert_eq!(
        m.json(&infra, "mail.inbox", json!({"address":"captain"}))["acknowledged_through"],
        0
    );
    app.scroll = 8;
    mailbox.mark_read(&proxy, &app.mail[app.selected]).unwrap();
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert!(app.mail.is_empty());
    assert_eq!(app.scroll, 0);
    assert!(screen(&mut app).contains("No messages for the captain"));
    let request_body = format!("{}REQUEST-END", "Full request line\n".repeat(100));
    m.json(
        &infra,
        "mail.send",
        json!({"id":"request","from":"infra","to":"captain","kind":"request","body":request_body}),
    );
    m.json(&apps, "mail.send", json!({"id":"after-reply","from":"apps","to":"captain","kind":"notice","body":"After reply details"}));
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail[app.selected].id, "request");
    assert!(scroll_to(&mut app, "REQUEST-END"));
    let offset = app.scroll;
    mailbox.mark_read(&proxy, &app.mail[app.selected]).unwrap();
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail[app.selected].id, "request");
    assert!(app.mail[app.selected].acknowledged);
    assert_eq!(app.scroll, offset);
    m.json(&infra, "mail.send", json!({"from":"captain","to":"infra","kind":"reply","correlation":"request","body":"Proceed"}));
    mailbox.refresh(&proxy, &snapshot, None);
    app.update_mail(mailbox.messages.clone(), mailbox.errors.clone());
    assert_eq!(app.mail[app.selected].id, "after-reply");
    assert_eq!(app.scroll, 0);
    assert!(screen(&mut app).contains("After reply details"));
    app.select_ship(Some("apps".into()));
    app.select_tab(1);
    app.scroll = 12;
    let selected = app.selected;
    app.update_mail(Vec::new(), Vec::new());
    assert_eq!(app.selected, selected);
    assert_eq!(app.scroll, 12);
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

#[test]
fn retirement_cannot_mark_a_replacement_home_retired_after_dispatcher_stop() {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };
    let m = Machine::new();
    let old = m.ship("apps");
    let systemctl = m.root.join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let started = m
        .command(env!("CARGO_BIN_EXE_privateer"), &old)
        .args(["service", "start"])
        .env("PRIVATEER_SERVICE", "systemd")
        .env("PRIVATEER_SYSTEMCTL", &systemctl)
        .output()
        .unwrap();
    assert!(started.status.success());
    let barrier = m.root.join("stop-entered");
    let release = m.root.join("release-stop");
    let blocked = m.root.join("blocked-systemctl");
    fs::write(
        &blocked,
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\n",
            barrier.display(),
            release.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
    let mut first = m
        .command(env!("CARGO_BIN_EXE_privateer"), &old)
        .args([
            "ship.retire",
            "--json",
            &json!({"name":"apps","expected_home":old.join(".privateer")}).to_string(),
        ])
        .env("PRIVATEER_SYSTEMCTL", &blocked)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !barrier.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !barrier.exists() {
        fs::write(&release, "").unwrap();
        let _ = first.wait();
        panic!("retirement never reached isolated dispatcher stop");
    }
    // A second operator completes retirement and reuses the name while the
    // first command is in its external dispatcher boundary.
    let second = m
        .command(env!("CARGO_BIN_EXE_privateer"), &old)
        .args(["ship.retire", "--json", "{\"name\":\"apps\"}"])
        .env("PRIVATEER_SYSTEMCTL", &systemctl)
        .output()
        .unwrap();
    let replacement = m.root.join("replacement");
    fs::create_dir(&replacement).unwrap();
    let initialized = if second.status.success() {
        m.command(env!("CARGO_BIN_EXE_privateer"), &replacement)
            .args(["init", "--ship", "apps", "--harness", "codex"])
            .output()
            .unwrap()
    } else {
        second
    };
    fs::write(release, "").unwrap();
    let finished = first.wait_with_output().unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(
        !finished.status.success(),
        "stale retirement must refuse: {}",
        String::from_utf8_lossy(&finished.stdout)
    );
    assert!(String::from_utf8_lossy(&finished.stderr).contains("entry changed"));
    let entry = m.json(&replacement, "fleet.list", json!({}))["ships"][0].clone();
    assert_eq!(entry["retired_ms"], Value::Null);
    assert_eq!(
        entry["home"],
        replacement.join(".privateer").to_string_lossy().as_ref()
    );
    assert_eq!(
        m.json(&replacement, "charter.show", json!({"id":"apps"}))["active"],
        true
    );
}

#[test]
fn primary_ship_actions_remain_visible_at_a_standard_terminal_width() {
    use privateer::dashboard::{fleet, ui::App};
    use ratatui::{Terminal, backend::TestBackend};
    let m = Machine::new();
    m.ship("apps");
    let mut app = App {
        snapshot: fleet::read(&m.root.join("fleet.sqlite"), None).unwrap(),
        ..App::default()
    };
    app.select_ship(Some("apps".into()));
    let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = (0..34)
        .map(|y| {
            (0..120)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for action in ["[b Board]", "[n New ship]", "[x Retire]", "[d Dispatcher]"] {
        assert!(text.contains(action), "{action} was clipped");
    }
}

#[test]
fn fleet_home_tabulates_live_ships_liaison_first_and_rows_open_their_workspace() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use privateer::dashboard::{fleet, ui::App};
    let m = Machine::new();
    let apps = m.ship("apps");
    let infra = m.ship("infra");
    let zeta = m.root.join("zeta");
    fs::create_dir(&zeta).unwrap();
    m.cli(
        &zeta,
        &["init", "--ship", "zeta", "--harness", "codex", "--liaison"],
    );
    m.json(
        &apps,
        "mail.send",
        json!({"from":"apps","to":"captain","kind":"request","body":"Ship it?"}),
    );
    fs::rename(infra.join(".privateer"), infra.join("saved-home")).unwrap();
    let snapshot = fleet::read(&m.root.join("fleet.sqlite"), None).unwrap();
    let order: Vec<_> = snapshot.ships.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(order, ["zeta", "apps", "infra"]);
    let mut app = App {
        snapshot,
        ..App::default()
    };
    let rows = |app: &mut App, width: u16| -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(width, 34)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..34)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    };
    let lines = rows(&mut app, 120);
    let view = lines.join("\n");
    let header = lines
        .iter()
        .position(|l| l.contains("Ship") && l.contains("Aboard") && l.contains("Last wake"))
        .expect("fleet table header");
    assert!(lines[header + 1].contains("zeta ★"), "{view}");
    assert!(lines[header + 2].contains("○ —"), "{view}");
    assert!(lines[header + 3].contains("✕ unavailable"), "{view}");
    assert!(view.contains("infra unavailable: ship home is missing"));
    assert!(view.contains("liaison · 1 task"), "{view}");
    assert!(view.contains("1 req · 1 task"), "{view}");
    assert!(view.contains("home unavailable"));
    assert!(
        !view.contains("Scope of apps"),
        "per-ship prose left the home"
    );

    // Clicking a table row opens that ship's workspace.
    let column = lines[header][..lines[header].find("Ship").unwrap()]
        .chars()
        .count() as u16;
    app.handle_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row: header as u16 + 2,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.ship.as_deref(), Some("apps"));
    assert_eq!(app.tab, 0);

    // Narrow terminals drop the lowest-priority columns, never the ship.
    app.select_ship(None);
    let narrow = rows(&mut app, 80).join("\n");
    assert!(narrow.contains("Aboard") && narrow.contains("Requests"));
    assert!(!narrow.contains("Last wake") && !narrow.contains("Dispatcher"));
}
