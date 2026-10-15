#![cfg(feature = "dashboard")]

mod support;

use privateer::{
    dashboard::{
        boat::Boat,
        herdr::Client,
        model::{Snapshot, clean},
        ui::App,
    },
    dispatch,
    store::Store,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::Path,
    thread,
    time::Duration,
};

fn fixture() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let mut store = Store::open(&root.path().join("home")).unwrap();
    dispatch(
        &mut store,
        "charter.put",
        json!({"id":"project","objective":"Inspect dashboard","repo":repo,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"main","charter_id":"project","owner":"navigator","title":"Test the bridge"}),
    )
    .unwrap();
    (root, store)
}

#[test]
fn observations_preserve_pending_sequences_audit_and_job_records() {
    let (_root, mut store) = fixture();
    for n in 0..50 {
        dispatch(&mut store,"event.emit",json!({"id":format!("fact-{n}"),"task_id":"main","issue_key":"route","kind":"blocked","payload":{"summary":format!("attempt {n}")}})).unwrap();
    }
    // If a job list were used, this intentionally orphaned record would be
    // reconciled or fail parsing. The dashboard only reads its stored fields.
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('job',?1)",
            [json!({"task_id":"main","status":"running"}).to_string()],
        )
        .unwrap();
    let before: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    let first = Snapshot::read(store.home()).unwrap();
    assert_eq!(first.pending.len(), 1);
    assert_eq!(first.pending[0].seq, 50);
    assert_eq!(first.jobs[0].status, "running");
    dispatch(&mut store,"event.emit",json!({"id":"newer","task_id":"main","issue_key":"route","kind":"blocked","payload":{"summary":"new fact"}})).unwrap();
    let second = Snapshot::read(store.home()).unwrap();
    assert_eq!(second.pending[0].seq, 51);
    let after: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        dispatch(&mut store, "event.pending", json!({})).unwrap()["events"][0]["seq"],
        51
    );
    assert_eq!(
        store
            .connection()
            .query_row("SELECT record FROM jobs WHERE id='job'", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        json!({"task_id":"main","status":"running"}).to_string()
    );
}

#[test]
fn missing_home_is_not_created_and_symlink_home_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    assert!(Snapshot::read(&missing).is_err());
    assert!(!missing.exists());
    let (_fixture, store) = fixture();
    let link = root.path().join("linked");
    std::os::unix::fs::symlink(store.home(), &link).unwrap();
    assert!(Snapshot::read(&link).is_err());
}

#[test]
fn large_state_is_bounded_without_pruning_the_runtime() {
    let (_root, store) = fixture();
    for n in 0..205 {
        store.connection().execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES(?1,'project','apps',?2,'open',0)",rusqlite::params![format!("task-{n:03}"),"x".repeat(3000)]).unwrap();
    }
    let view = Snapshot::read(store.home()).unwrap();
    assert_eq!(view.tasks.len(), 200);
    assert!(view.truncated);
    assert!(view.tasks.iter().all(|t| t.title.len() <= 512));
    assert_eq!(
        store
            .connection()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        206
    );
}

#[test]
fn boat_is_bounded_and_freezes_when_reduced_or_hidden() {
    let mut boat = Boat::default();
    for width in 0..150 {
        for _ in 0..20 {
            boat.advance();
            assert!(
                boat.frame(width, true, false, false)
                    .iter()
                    .all(|s| s.chars().count() == width)
            );
        }
        assert!(
            boat.frame(width, true, false, true)
                .iter()
                .all(|s| s.is_ascii() && s.len() == width)
        );
    }
    let frame = boat.frame(50, true, true, false);
    assert_eq!(frame, boat.frame(50, false, true, false));
    assert_eq!(frame, boat.frame(50, false, true, false));
    // A draw without a clock advance does not move the boat.
    assert_eq!(
        boat.frame(50, true, false, false),
        boat.frame(50, true, false, false)
    );
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mouse_at(
    kind: crossterm::event::MouseEventKind,
    x: u16,
    y: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

fn click_text(app: &mut App, screen: &str, label: &str) {
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(label))
        .unwrap();
    let column = line[..line.find(label).unwrap()].chars().count();
    app.handle_mouse(mouse_at(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column as u16,
        row as u16,
    ));
}

#[test]
fn mouse_navigation_selects_visible_tabs_rows_and_details_without_opening_agents() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = App {
        snapshot: Snapshot::demo(),
        compact: true,
        ..App::default()
    };
    app.select_ship(Some("apps".into()));
    for (tab, label) in [
        (1, "2 Assignments"),
        (2, "3 Terminals"),
        (3, "4 Logbook"),
        (0, "1 Bridge"),
    ] {
        let screen = render(&mut app, 120, 34);
        click_text(&mut app, &screen, label);
        assert_eq!(app.tab, tab);
    }
    app.select_tab(1);
    let screen = render(&mut app, 120, 34);
    click_text(&mut app, &screen, "Investigate the failing build");
    let screen = render(&mut app, 120, 34);
    assert!(screen.contains("Task: app-build"));
    app.handle_mouse(mouse_at(MouseEventKind::ScrollDown, 80, 25));
    assert!(app.scroll > 0);
    let before = app.tab;
    app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 110, 3));
    assert_eq!(app.tab, before);
    render(&mut app, 20, 8);
    app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), 15, 3));
    assert_eq!(
        app.tab, before,
        "a resize notice must clear the old hit map"
    );
}

#[test]
fn mouse_selection_tracks_the_scrolled_list_instead_of_its_first_record() {
    let mut snapshot = Snapshot::demo();
    let template = snapshot.tasks[0].clone();
    snapshot.tasks = (0..30)
        .map(|n| {
            let mut task = template.clone();
            task.id = format!("task-{n}");
            task.title = format!("Assignment {n}");
            task
        })
        .collect();
    let mut app = App {
        snapshot,
        selected: 29,
        ..App::default()
    };
    app.ship = Some("navigator".into());
    app.tab = 1;
    app.focus_content = true;
    let screen = render(&mut app, 80, 20);
    click_text(&mut app, &screen, "Assignment 28");
    assert_eq!(app.selected_task().unwrap().id, "task-28");
}

#[test]
fn compact_view_keeps_failures_visible_and_untrusted_controls_are_not_rendered() {
    let mut app = App {
        snapshot: Snapshot::demo(),
        compact: true,
        home_age: Some(0),
        home_error: Some("disk unavailable\x1b[2J".into()),
        ..App::default()
    };
    let text = render(&mut app, 120, 34);
    assert!(text.contains("STATE UNAVAILABLE"));
    assert!(text.contains("Repeated connection facts"));
    assert!(!text.contains('\x1b'));
    assert_eq!(clean("hello\x1b[2J\u{202e}world", 100), "hello[2Jworld");
    for (w, h) in [(1, 1), (20, 8), (40, 14), (80, 24)] {
        render(&mut app, w, h);
    }
}

fn snapshot(terminal: &str) -> Value {
    json!({"snapshot":{"version":"0.8.2","protocol":20,"panes":[{"pane_id":"w1:p1","terminal_id":terminal,"workspace_id":"w1","tab_id":"w1:t1","agent":"pi","agent_status":"working","title":"Worker"}]}})
}

fn server(path: &Path, replies: Vec<Value>) -> thread::JoinHandle<Vec<Value>> {
    let listener = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut requests = Vec::new();
        for reply in replies {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            writeln!(
                reader.get_mut(),
                "{}",
                json!({"id":request["id"],"result":reply})
            )
            .unwrap();
            requests.push(request);
        }
        requests
    })
}

#[test]
fn herdr_snapshot_and_explicit_focus_use_correlated_structured_requests() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let server = server(
        &path,
        vec![
            snapshot("terminal-a"),
            snapshot("terminal-a"),
            json!({"type":"focused"}),
        ],
    );
    let mut client = Client::connect(&path).unwrap();
    let view = client.snapshot().unwrap();
    client.focus(&view, &view.panes[0]).unwrap();
    let requests = server.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|r| r["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session.snapshot", "session.snapshot", "agent.focus"]
    );
    assert_eq!(requests[2]["params"], json!({"target":"w1:p1"}));
}

#[test]
fn replaced_terminal_is_not_focused_even_with_the_same_pane_id() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let server = server(&path, vec![snapshot("terminal-a"), snapshot("terminal-b")]);
    let mut client = Client::connect(&path).unwrap();
    let view = client.snapshot().unwrap();
    assert!(
        client
            .focus(&view, &view.panes[0])
            .unwrap_err()
            .to_string()
            .contains("occupant changed")
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn a_replaced_socket_requires_a_new_observation_before_focus() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let first = server(&path, vec![snapshot("terminal-a")]);
    let view = Client::connect(&path).unwrap().snapshot().unwrap();
    first.join().unwrap();
    // Keep the old inode allocated by renaming its socket path.
    fs::rename(&path, root.path().join("old.sock")).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    let mut client = Client::connect(&path).unwrap();
    assert!(
        client
            .focus(&view, &view.panes[0])
            .unwrap_err()
            .to_string()
            .contains("socket changed")
    );
    drop(listener);
}

#[test]
fn malformed_old_and_uncorrelated_responses_fail_closed() {
    for reply in [
        json!({"snapshot":{"protocol":19,"panes":[]}}),
        json!({"snapshot":{"protocol":20,"panes":[{}]}}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let server = server(&path, vec![reply]);
        assert!(Client::connect(&path).unwrap().snapshot().is_err());
        server.join().unwrap();
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({"id":"another-request","result":snapshot("t")})
        )
        .unwrap();
    });
    assert!(
        Client::connect(&path)
            .unwrap()
            .snapshot()
            .unwrap_err()
            .to_string()
            .contains("ID mismatch")
    );
    server.join().unwrap();
}

#[test]
fn silent_socket_times_out_without_holding_the_operator_loop() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(1100));
    });
    let started = std::time::Instant::now();
    assert!(Client::connect(&path).unwrap().snapshot().is_err());
    assert!(started.elapsed() < Duration::from_millis(1050));
    server.join().unwrap();
}

#[test]
fn demo_cli_never_opens_an_inherited_herdr_socket() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = support::command(env!("CARGO_BIN_EXE_privateer-dashboard"))
        .args(["--demo", "--once"])
        .env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", &path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["demo"], true);
    assert!(result["herdr"].is_null());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn refresh_preserves_selected_identity_and_requires_reselection_after_replacement() {
    use privateer::dashboard::herdr::{Pane, View};
    let pane: Pane =
        serde_json::from_value(snapshot("terminal-a")["snapshot"]["panes"][0].clone()).unwrap();
    let mut other = pane.clone();
    other.pane_id = "w2:p1".into();
    other.terminal_id = "other".into();
    let mut app = App {
        tab: 2,
        ship: Some("worker".into()),
        focus_content: true,
        snapshot: Snapshot {
            ships: vec![privateer::dashboard::model::Ship {
                id: "worker".into(),
                name: "Worker".into(),
                terminals: vec![
                    privateer::dashboard::model::TerminalBinding {
                        pane_id: pane.pane_id.clone(),
                        terminal_id: pane.terminal_id.clone(),
                    },
                    privateer::dashboard::model::TerminalBinding {
                        pane_id: other.pane_id.clone(),
                        terminal_id: other.terminal_id.clone(),
                    },
                ],
                ..Default::default()
            }],
            ..Snapshot::default()
        },
        herdr: Some(View {
            panes: vec![pane.clone()],
            socket_identity: (1, 2),
            ..View::default()
        }),
        ..App::default()
    };
    app.update_herdr(View {
        panes: vec![other.clone(), pane.clone()],
        socket_identity: (1, 2),
        ..View::default()
    });
    assert_eq!(app.selected, 0);
    assert!(!app.selection_invalidated);
    let mut replaced = pane;
    replaced.terminal_id = "replacement".into();
    app.update_herdr(View {
        panes: vec![other, replaced],
        socket_identity: (1, 2),
        ..View::default()
    });
    assert!(app.selection_invalidated);
    app.move_selection(false);
    assert!(!app.selection_invalidated);
}

#[test]
fn oversized_and_slow_drip_socket_responses_are_bounded() {
    for oversized in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            if oversized {
                let _ = stream.write_all(&vec![b' '; 1_048_577]);
            } else {
                for _ in 0..15 {
                    if stream.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        let started = std::time::Instant::now();
        let result = Client::connect(&path).unwrap().snapshot();
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(1200));
        if oversized {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("exceeds 1 MiB"), "{error:#}");
        }
        server.join().unwrap();
    }
}

#[test]
fn bridge_preview_identifies_demo_and_does_not_pretend_chat_is_connected() {
    let mut app = App {
        snapshot: Snapshot::demo(),
        mail: privateer::dashboard::captain::sample_mail(),
        demo: true,
        herdr: Some(privateer::dashboard::herdr::View::demo()),
        compact: true,
        home_age: Some(0),
        herdr_age: Some(0),
        ..App::default()
    };
    let home = render(&mut app, 120, 34);
    assert!(home.contains("Fleet overview"));
    assert!(home.contains("Research"));
    app.select_ship(Some("navigator".into()));
    let view = render(&mut app, 120, 34);
    assert!(view.contains("DEMO / sample data"));
    assert!(view.contains("Conversation and agent launch are not connected"));
    if let Some(path) = std::env::var_os("PRIVATEER_DASHBOARD_PREVIEW") {
        fs::write(&path, &home).unwrap();
        app.select_ship(Some("apps".into()));
        app.select_tab(1);
        let assignments = render(&mut app, 120, 34);
        fs::write(
            std::path::PathBuf::from(&path).with_file_name("dashboard-preview-assignments.txt"),
            assignments,
        )
        .unwrap();
        app.open_inbox();
        fs::write(
            std::path::PathBuf::from(&path).with_file_name("dashboard-preview-inbox.txt"),
            render(&mut app, 120, 34),
        )
        .unwrap();
        app.open_liaison();
        app.compose();
        fs::write(
            std::path::PathBuf::from(&path).with_file_name("dashboard-preview-compose.txt"),
            render(&mut app, 120, 34),
        )
        .unwrap();
    }
}

#[test]
fn captain_inbox_navigation_is_separate_from_ships_and_survives_terminal_refresh() {
    use privateer::dashboard::{captain::sample_mail, herdr::View};
    let mut app = App {
        snapshot: Snapshot::demo(),
        mail: sample_mail(),
        demo: true,
        ..App::default()
    };
    let before = serde_json::to_value(&app.snapshot).unwrap();
    let screen = render(&mut app, 120, 34);
    click_text(&mut app, &screen, "[i Inbox]");
    assert!(app.inbox_open && app.ship.is_none());
    let screen = render(&mut app, 120, 34);
    click_text(&mut app, &screen, "Reply · Build investigation");
    assert_eq!(app.selected, 1);
    app.update_herdr(View::demo());
    assert_eq!(
        app.selected, 1,
        "a terminal refresh cannot reset a mail selection"
    );
    assert!(render(&mut app, 120, 34).contains("still owned by Apps"));
    assert!(app.selected_pane().is_none());
    assert!(app.assignment_indices().is_empty());
    let screen = render(&mut app, 120, 34);
    click_text(&mut app, &screen, "[l Talk to liaison]");
    assert_eq!(app.ship.as_deref(), Some("navigator"));
    assert!(!app.inbox_open);
    assert!(render(&mut app, 120, 34).contains("Liaison · your default contact"));
    assert_eq!(serde_json::to_value(&app.snapshot).unwrap(), before);
    app.select_ship(None);
    app.move_selection(true);
    assert!(app.inbox_open);
    app.move_selection(true);
    assert_eq!(app.ship.as_deref(), Some("navigator"));
}

#[test]
fn demo_composer_bounds_input_and_history_without_changing_runtime_state() {
    use crossterm::event::KeyCode;
    use privateer::dashboard::captain::InputMode;
    let mut app = App {
        snapshot: Snapshot::demo(),
        demo: true,
        ..App::default()
    };
    let before = serde_json::to_value(&app.snapshot).unwrap();
    app.open_liaison();
    app.compose();
    for c in "quit?".chars() {
        app.edit_composer(KeyCode::Char(c));
    }
    app.edit_composer(KeyCode::Esc);
    assert!(app.exchanges.is_empty());
    app.compose();
    for _ in 0..1100 {
        app.edit_composer(KeyCode::Char('界'));
    }
    app.edit_composer(KeyCode::Char('\u{202e}'));
    assert_eq!(app.composer.as_ref().unwrap().body.chars().count(), 1024);
    let small = render(&mut app, 50, 14);
    assert!(
        small.contains("Esc: cancel"),
        "long input must not hide modal controls"
    );
    app.edit_composer(KeyCode::Tab);
    assert_eq!(app.composer.as_ref().unwrap().mode, InputMode::Status);
    app.edit_composer(KeyCode::Enter);
    app.edit_composer(KeyCode::Enter);
    assert_eq!(app.exchanges.len(), 1, "submission consumes the draft once");
    assert!(app.exchanges[0].response.contains("No model call or wake"));
    for _ in 0..25 {
        app.compose();
        app.edit_composer(KeyCode::Char('a'));
        app.edit_composer(KeyCode::Enter);
    }
    assert_eq!(app.exchanges.len(), 20);
    assert_eq!(serde_json::to_value(&app.snapshot).unwrap(), before);
}

#[test]
fn live_captain_view_does_not_infer_mail_or_allow_demo_sends() {
    let (_root, store) = fixture();
    let mut app = App {
        snapshot: Snapshot::read(store.home()).unwrap(),
        ..App::default()
    };
    app.open_inbox();
    assert_eq!(app.selection_len(), 0);
    assert!(render(&mut app, 120, 34).contains("No messages for the captain"));
    app.select_ship(Some("navigator".into()));
    app.compose();
    assert!(app.composer.is_none());
    assert!(app.notice.contains("No message sent"));
    assert!(app.exchanges.is_empty());
    app.open_liaison();
    assert!(app.notice.contains("No liaison configured"));
}

#[test]
fn registry_allows_one_optional_liaison_without_changing_task_ownership() {
    let (_root, store) = fixture();
    let path = store.home().join("dashboard.json");
    fs::write(
        &path,
        json!({"ships":[{"id":"contact","name":"Contact","liaison":true}]}).to_string(),
    )
    .unwrap();
    let snapshot = Snapshot::read(store.home()).unwrap();
    assert!(
        snapshot
            .ships
            .iter()
            .any(|s| s.id == "contact" && s.liaison)
    );
    assert!(snapshot.tasks.iter().all(|t| t.owner == "navigator"));
    fs::write(&path, json!({"ships":[{"id":"one","name":"One","liaison":true},{"id":"two","name":"Two","liaison":true}]}).to_string()).unwrap();
    assert!(
        Snapshot::read(store.home())
            .unwrap_err()
            .to_string()
            .contains("only one ship may be the liaison")
    );
}

#[test]
fn ship_workspaces_scope_assignments_alerts_history_and_bound_terminals() {
    use privateer::dashboard::herdr::View;
    let mut app = App {
        snapshot: Snapshot::demo(),
        herdr: Some(View::demo()),
        ..App::default()
    };
    assert!(app.ship.is_none());
    app.select_ship(Some("apps".into()));
    let bridge = render(&mut app, 140, 40);
    assert!(!bridge.contains("Repeated connection facts"));
    assert!(
        !bridge.contains("Investigate the failing build"),
        "assignments belong in their own view"
    );
    app.select_tab(1);
    assert!(render(&mut app, 140, 40).contains("Investigate the failing build"));
    assert_eq!(app.assignment_indices().len(), 1);
    app.select_tab(2);
    assert_eq!(app.scoped_panes().len(), 1);
    assert_eq!(app.selected_pane().unwrap().agent.as_deref(), Some("codex"));
    app.select_ship(Some("research".into()));
    assert!(app.assignment_indices().is_empty());
    assert!(app.scoped_panes().is_empty());
    app.select_tab(3);
    assert!(!render(&mut app, 140, 40).contains("Sample logbook outcome"));
    app.select_ship(None);
    let home = render(&mut app, 140, 40);
    assert!(home.contains("4 ships"));
    assert!(home.contains("Research"));
}

#[test]
fn accepted_handoff_moves_the_same_assignment_and_preserves_custody_history() {
    let (_root, mut store) = fixture();
    dispatch(
        &mut store,
        "task.handoff",
        json!({"id":"main","actor":"navigator","target":"infrastructure"}),
    )
    .unwrap();
    let before = Snapshot::read(store.home()).unwrap();
    let mut incoming = App {
        snapshot: before.clone(),
        ..App::default()
    };
    incoming.select_ship(Some("infrastructure".into()));
    incoming.select_tab(1);
    assert_eq!(incoming.selected_task().unwrap().owner, "navigator");
    assert!(render(&mut incoming, 140, 40).contains("Incoming"));
    dispatch(
        &mut store,
        "task.accept",
        json!({"id":"main","actor":"infrastructure"}),
    )
    .unwrap();
    let after = Snapshot::read(store.home()).unwrap();
    assert_eq!(after.tasks.len(), before.tasks.len());
    assert_eq!(after.tasks[0].id, "main");
    assert_eq!(after.tasks[0].owner, "infrastructure");
    incoming.update_snapshot(after.clone());
    assert!(incoming.selected_task().unwrap().handoff_to.is_none());
    let mut outgoing = App {
        snapshot: after,
        ..App::default()
    };
    outgoing.select_ship(Some("navigator".into()));
    outgoing.select_tab(1);
    assert!(outgoing.assignment_indices().is_empty());
    outgoing.select_tab(3);
    assert!(render(&mut outgoing, 140, 40).contains("offered a handoff"));
}

#[test]
fn ship_registry_keeps_idle_peers_and_rejects_ambiguous_terminal_ownership() {
    let (_root, store) = fixture();
    let path = store.home().join("dashboard.json");
    fs::write(
        &path,
        json!({"ships":[{"id":"idle","name":"Idle ship"}]}).to_string(),
    )
    .unwrap();
    let view = Snapshot::read(store.home()).unwrap();
    assert!(view.ships.iter().any(|s| s.id == "idle"));
    assert!(view.ships.iter().any(|s| s.id == "navigator"));
    let binding = json!({"pane_id":"w1:p1","terminal_id":"terminal-1"});
    fs::write(&path,json!({"ships":[{"id":"one","name":"One","terminals":[binding]},{"id":"two","name":"Two","terminals":[binding]}]}).to_string()).unwrap();
    assert!(
        Snapshot::read(store.home())
            .unwrap_err()
            .to_string()
            .contains("only one ship")
    );
    fs::write(&path, "x".repeat(65537)).unwrap();
    assert!(Snapshot::read(store.home()).is_err());
}

#[test]
fn boarding_never_sends_launch_input_after_the_new_pane_changes() {
    let (root, store) = fixture();
    let socket = root.path().join("launch.sock");
    let shell = json!({"pane_id":"w1:p9","terminal_id":"new-terminal","workspace_id":"w1","tab_id":"w1:t9","agent":null,"agent_status":"unknown"});
    let empty = json!({"snapshot":{"version":"test","protocol":20,"panes":[]}});
    let mut changed = shell.clone();
    changed["terminal_id"] = json!("replaced-terminal");
    let worker = server(
        &socket,
        vec![
            empty.clone(),
            empty,
            json!({"root_pane":shell}),
            json!({"snapshot":{"version":"test","protocol":20,"panes":[changed]}}),
        ],
    );
    let mut client = Client::connect(&socket).unwrap();
    let observed = client.snapshot().unwrap();
    assert!(
        client
            .board(
                &observed,
                store.home().parent().unwrap(),
                "apps",
                Path::new(env!("CARGO_BIN_EXE_privateer")),
                store.home()
            )
            .is_err()
    );
    let requests = worker.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|r| r["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "session.snapshot",
            "session.snapshot",
            "tab.create",
            "session.snapshot"
        ]
    );
}

#[test]
fn automatic_bindings_require_socket_terminal_location_and_pinned_session() {
    use privateer::herdr::{Binding, View};
    let view = View::demo();
    let pane = view.panes[0].clone();
    let mut binding = Binding {
        socket: "/isolated/socket".into(),
        socket_identity: view.socket_identity,
        pane: pane.clone(),
    };
    assert!(binding.matches(&view, &pane));
    let mut changed = pane.clone();
    changed.terminal_id = "replacement".into();
    assert!(!binding.matches(&view, &changed));
    changed = pane.clone();
    changed.tab_id = "different-tab".into();
    assert!(!binding.matches(&view, &changed));
    let mut other = view.clone();
    other.socket_identity = (9, 9);
    assert!(!binding.matches(&other, &pane));
    binding.pane.agent_session = Some(json!({"value":"original"}));
    changed = pane;
    changed.agent_session = Some(json!({"value":"replacement"}));
    assert!(!binding.matches(&view, &changed));
}
