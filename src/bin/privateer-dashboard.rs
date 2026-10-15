use anyhow::{Context, Result, bail, ensure};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
};
use privateer::dashboard::{
    captain::{Mail, sample_mail},
    fleet,
    herdr::{Client, Pane, View},
    mailbox::{Action, Mailbox},
    model::Snapshot,
    ui::{App, BoardTarget, PendingBoard},
};
use serde_json::json;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Options {
    home: Option<PathBuf>,
    socket: Option<PathBuf>,
    demo: bool,
    once: bool,
    ascii: bool,
    no_herdr: bool,
    board_liaison: bool,
}

fn options() -> Result<Option<Options>> {
    let mut out = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => out.home = Some(args.next().context("--home requires a path")?.into()),
            "--herdr-socket" => {
                out.socket = Some(
                    args.next()
                        .context("--herdr-socket requires a path")?
                        .into(),
                )
            }
            "--demo" => out.demo = true,
            "--once" => out.once = true,
            "--ascii" => out.ascii = true,
            "--no-herdr" => out.no_herdr = true,
            "--board-liaison" => out.board_liaison = true,
            "--help" | "-h" => {
                println!(
                    "Privateer — the bridge\n\nprivateer-dashboard --demo [--once]\nprivateer-dashboard [--home /absolute/home] [--herdr-socket /absolute/socket]\n\n--once             Print a bounded JSON snapshot; no terminal required\n--no-herdr         Observe Privateer only, without connecting to Herdr\n--board-liaison    Open/focus the liaison on entry (used by bare privateer)\n--ascii            Plain ASCII attention markers\n\nStart on Fleet overview; select a ship, then use 1–4 for its views. 0 returns home.\ni: captain's inbox; l: open/focus liaison conversation; m: Status/Ask/Message composer.\nInside Herdr, uses HERDR_SOCKET_PATH when HERDR_ENV=1.\nFleet observations are read-only. b: Board; n: New ship; x: Retire; d: Dispatcher.\nMail and management use Privateer commands. Board launches the harness; no input to existing agents."
                );
                return Ok(None);
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    ensure!(!(out.demo && out.home.is_some()), "choose --demo or --home");
    ensure!(
        !(out.no_herdr && out.socket.is_some()),
        "--no-herdr conflicts with --herdr-socket"
    );
    if !out.demo && out.home.is_none() {
        out.home = std::env::var_os("PRIVATEER_HOME").map(PathBuf::from);
    }

    if out.socket.is_none() && std::env::var("HERDR_ENV").as_deref() == Ok("1") {
        out.socket = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    }
    if out.demo || out.no_herdr {
        out.socket = None;
    }
    Ok(Some(out))
}

struct Sample<T> {
    value: Result<T>,
    at: Instant,
    notice: Option<String>,
}
enum HerdrAction {
    Focus(Focus),
    Board {
        home: PathBuf,
        name: String,
        observed: View,
    },
}
struct Focus {
    observed: View,
    pane: Pane,
}

struct MouseCapture;
impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}

fn state_worker(registry: PathBuf, home: Option<PathBuf>) -> Receiver<Sample<Snapshot>> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let value = fleet::read(&registry, home.as_deref());
            if send
                .send(Sample {
                    value,
                    at: Instant::now(),
                    notice: None,
                })
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
    receive
}

enum MailAction {
    Send(Action),
    Read(Mail),
}
struct MailUpdate {
    messages: Vec<Mail>,
    errors: Vec<String>,
    notice: Option<String>,
    sent: bool,
    completed_send: bool,
    board: Option<BoardTarget>,
}
fn mail_worker(
    registry: PathBuf,
    fallback: Option<PathBuf>,
    binary: PathBuf,
) -> (Receiver<MailUpdate>, SyncSender<MailAction>) {
    let (send, receive) = mpsc::sync_channel(1);
    let (actions, commands) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut mailbox = Mailbox::default();
        loop {
            let action = match commands.recv_timeout(Duration::from_secs(2)) {
                Ok(a) => Some(a),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            };
            let mut sent = false;
            let mut board = None;
            let completed_send = matches!(&action, Some(MailAction::Send(_)));
            let notice = action.map(|a| match a {
                MailAction::Read(mail) => match mailbox.mark_read(&binary, &mail) {
                    Ok(()) => "Marked read; earlier unseen mail remains unread.".into(),
                    Err(e) => format!("Acknowledgment failed: {e}"),
                },
                MailAction::Send(a) => {
                    let result = if a.verb == "init" {
                        privateer::dashboard::command::initialize(
                            &binary,
                            std::path::Path::new(a.args["folder"].as_str().unwrap_or("")),
                            a.args["name"].as_str().unwrap_or(""),
                            a.args["harness"].as_str().unwrap_or(""),
                            a.args["liaison"] == true,
                        )
                    } else if a.verb == "service" {
                        privateer::dashboard::command::service(
                            &binary,
                            &a.home,
                            a.args["action"].as_str().unwrap_or(""),
                        )
                        .map(|v| json!({"report":String::from_utf8_lossy(&v)}))
                    } else {
                        privateer::dashboard::command::json(&binary, &a.home, &a.verb, &a.args)
                    };
                    match result {
                        Ok(value) => {
                            sent = true;
                            if a.verb == "init" {
                                board = value["ship"].as_str().zip(value["home"].as_str()).map(
                                    |(ship, home)| BoardTarget {
                                        ship: ship.to_owned(),
                                        home: PathBuf::from(home),
                                    },
                                );
                            }
                            privateer::dashboard::command::summary(&a.verb, &value)
                        }
                        Err(e) => format!("{} failed: {e}; form/draft retained", a.verb),
                    }
                }
            });
            let errors = match fleet::read(&registry, fallback.as_deref()) {
                Ok(snapshot) => {
                    mailbox.refresh(&binary, &snapshot, fallback.as_deref());
                    mailbox.errors.clone()
                }
                Err(e) => vec![e.to_string()],
            };
            if send
                .send(MailUpdate {
                    messages: mailbox.messages.clone(),
                    errors,
                    notice,
                    sent,
                    completed_send,
                    board,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (receive, actions)
}

fn herdr_worker(path: PathBuf) -> (Receiver<Sample<View>>, SyncSender<HerdrAction>) {
    let (send, receive) = mpsc::sync_channel(1);
    let (focus, commands) = mpsc::sync_channel::<HerdrAction>(1);
    thread::spawn(move || {
        let mut delay = Duration::ZERO;
        let mut failures = 0_u32;
        let mut launching = std::collections::BTreeMap::<PathBuf, (String, Instant)>::new();
        loop {
            let action = match commands.recv_timeout(delay) {
                Ok(action) => Some(action),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut notice = None;
            let value = (|| {
                let mut client = Client::connect(&path)?;
                let binary = privateer::dashboard::command::binary()?;
                launching.retain(|home, (name, deadline)| {
                    Instant::now() < *deadline
                        && !privateer::dashboard::command::json(
                            &binary,
                            home,
                            "ship.presence",
                            &json!({"ship":name}),
                        )
                        .is_ok_and(|presence| presence["attended"] == true)
                });
                if let Some(action) = action {
                    let result = match action {
                        HerdrAction::Focus(action) => client.focus(&action.observed, &action.pane),
                        HerdrAction::Board {
                            home,
                            name,
                            observed,
                        } => (|| -> Result<()> {
                            privateer::dashboard::command::existing_home(&home)?;
                            let presence = privateer::dashboard::command::json(
                                &binary,
                                &home,
                                "ship.presence",
                                &json!({"ship":name}),
                            )?;
                            if presence["attended"] == true {
                                launching.remove(&home);
                            }
                            ensure!(
                                presence["attended"] != true,
                                "a session is already aboard; refresh its binding or reach it directly"
                            );
                            ensure!(
                                !launching.contains_key(&home),
                                "launch already submitted; wait for startup before retrying"
                            );
                            let folder = home.parent().context("ship folder")?;
                            launching.insert(
                                home.clone(),
                                (name.clone(), Instant::now() + Duration::from_secs(15)),
                            );
                            client.board(&observed, folder, &name, &binary, &home)?;
                            Ok(())
                        })(),
                    };
                    notice = Some(match result {
                        Ok(()) => "Boarding in Herdr; use Herdr navigation to return.".into(),
                        Err(e) => format!("Board action failed: {e}"),
                    });
                }
                client.snapshot()
            })();
            failures = if value.is_ok() {
                0
            } else {
                failures.saturating_add(1)
            };
            delay = Duration::from_secs(if failures == 0 {
                2
            } else {
                2_u64.pow(failures.min(4))
            });
            if send
                .send(Sample {
                    value,
                    at: Instant::now(),
                    notice,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (receive, focus)
}

fn request_board(app: &mut App, sender: Option<&SyncSender<HerdrAction>>) -> bool {
    if app.demo {
        app.notice = "Demo only; no ship launched.".into();
        return false;
    }
    if app.home_error.is_some() || app.home_age.is_none_or(|age| age >= 10) {
        app.notice = "Refresh Privateer state before boarding.".into();
        return false;
    }
    let Some(sender) = sender else {
        app.notice = app
            .snapshot
            .ships
            .iter()
            .find(|ship| Some(&ship.id) == app.ship.as_ref())
            .and_then(BoardTarget::from_ship)
            .map_or_else(
                || "Board needs an explicitly configured Herdr connection.".into(),
                |target| format!(
                    "No Herdr connection configured; to talk directly, run: privateer --home '{}' start",
                    target.home.to_string_lossy().replace('\'', "'\"'\"'")
                ),
            );
        return false;
    };
    let Some(view) = &app.herdr else {
        app.notice = "Waiting for the configured Herdr connection before boarding…".into();
        return false;
    };
    if app.herdr_error.is_some()
        || app.herdr_age.is_none_or(|age| age >= 10)
        || app.selection_invalidated
    {
        app.notice = "Refresh Herdr and select the ship again.".into();
        return false;
    }
    if app.binding_ambiguous() {
        app.notice =
            "Pane is bound to more than one ship; resolve the exact bindings before boarding."
                .into();
        return false;
    }
    let selected = app
        .snapshot
        .ships
        .iter()
        .find(|s| Some(&s.id) == app.ship.as_ref());
    let target = selected.and_then(BoardTarget::from_ship);
    if app.pending_board.as_ref().is_some_and(|pending| {
        let pending = pending.target();
        Some(&pending.ship) == app.ship.as_ref() && Some(pending) != target.as_ref()
    }) {
        app.notice = "Ship home changed or is unavailable; automatic Board withheld.".into();
        return false;
    }
    let action = if let Some(pane) = app.scoped_panes().first() {
        Some(HerdrAction::Focus(Focus {
            observed: view.clone(),
            pane: (*pane).clone(),
        }))
    } else {
        selected
            .filter(|s| s.live.as_ref().is_some_and(|l| l.error.is_none()))
            .and(target)
            .map(|target| HerdrAction::Board {
                home: target.home,
                name: target.ship,
                observed: view.clone(),
            })
    };
    let submitted = action.is_some_and(|a| sender.try_send(a).is_ok());
    app.notice = if submitted {
        "Checking ship and terminal before boarding…".into()
    } else {
        "Board action unavailable or already pending.".into()
    };
    submitted
}

fn run(opts: Options) -> Result<()> {
    privateer::store::Caller::from_env(None, None)?.require_operator("dashboard")?;
    let snapshot = if opts.demo {
        Snapshot::demo()
    } else {
        fleet::read(&privateer::fleet::registry_path()?, opts.home.as_deref())
            .context("read existing fleet homes")?
    };
    let binary = (!opts.demo)
        .then(privateer::dashboard::command::binary)
        .transpose()?;
    if opts.once {
        let (herdr, error) = match opts.socket.as_ref() {
            Some(path) => match Client::connect(path).and_then(|mut c| c.snapshot()) {
                Ok(view) => (Some(view), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, None),
        };
        let mut mailbox = Mailbox::default();
        if let Some(binary) = &binary {
            mailbox.refresh(binary, &snapshot, opts.home.as_deref());
        }
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"demo":opts.demo,"read_only":true,"snapshot":snapshot,"herdr":herdr,"herdr_error":error,"mail":if opts.demo {sample_mail()} else {mailbox.messages},"mail_errors":mailbox.errors})
            )?
        );
        return Ok(());
    }
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive dashboard requires a terminal; use --once for JSON"
    );
    let mailbox = binary.map(|binary| {
        mail_worker(
            privateer::fleet::registry_path().unwrap(),
            opts.home.clone(),
            binary,
        )
    });
    let home_updates = if opts.demo {
        None
    } else {
        Some(state_worker(privateer::fleet::registry_path()?, opts.home))
    };
    let herdr = opts.socket.map(herdr_worker);
    let mut app = App {
        snapshot,
        herdr: opts.demo.then(View::demo),
        compact: true,
        ascii: opts.ascii,
        demo: opts.demo,
        mail: if opts.demo { sample_mail() } else { Vec::new() },
        herdr_configured: herdr.is_some(),
        home_age: Some(0),
        ..App::default()
    };
    if !opts.demo && app.snapshot.ships.is_empty() {
        app.onboard();
    } else if opts.board_liaison && !opts.demo {
        app.open_liaison();
        if let Some(ship) = app
            .snapshot
            .ships
            .iter()
            .find(|ship| ship.liaison && Some(&ship.id) == app.ship.as_ref())
        {
            if herdr.is_some() {
                app.pending_board = BoardTarget::from_ship(ship).map(PendingBoard::Liaison);
            } else {
                request_board(&mut app, None);
            }
        }
    }
    let mut home_updated = Instant::now();
    let mut herdr_updated: Option<Instant> = None;
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        let _mouse_capture = MouseCapture;
        execute!(io::stdout(), EnableMouseCapture)?;
        let mut redraw = true;
        let mut last_draw = Instant::now();
        loop {
            if let Some(updates) = &home_updates {
                while let Ok(update) = updates.try_recv() {
                    redraw = true;
                    match update.value {
                        Ok(snapshot) => {
                            app.update_snapshot(snapshot);
                            app.home_error = None;
                            home_updated = update.at;
                        }
                        Err(error) => app.home_error = Some(error.to_string()),
                    }
                }
            } else {
                home_updated = Instant::now();
            }
            if let Some((updates, _)) = &mailbox {
                while let Ok(update) = updates.try_recv() {
                    app.update_mail(update.messages, update.errors);
                    if let Some(notice) = update.notice {
                        app.notice = notice;
                    }
                    if update.completed_send {
                        app.mail_pending = false;
                    }
                    if let Some(ship) = update.board {
                        app.pending_board = Some(PendingBoard::Onboarding(ship));
                    }
                    if update.sent {
                        app.composer = None;
                        app.form = None;
                    }
                    redraw = true;
                }
            }
            if let Some((updates, _)) = &herdr {
                while let Ok(update) = updates.try_recv() {
                    redraw = true;
                    if let Some(notice) = update.notice {
                        app.notice = notice;
                    }
                    match update.value {
                        Ok(view) => {
                            app.update_herdr(view);
                            app.herdr_error = None;
                            herdr_updated = Some(update.at);
                        }
                        Err(error) => app.herdr_error = Some(error.to_string()),
                    }
                }
            }
            app.home_age = Some(home_updated.elapsed().as_secs());
            app.herdr_age = herdr_updated.map(|t| t.elapsed().as_secs());
            if opts.demo {
                app.herdr_age = Some(0);
            }
            if let Some(pending) = app
                .pending_board
                .as_ref()
                .filter(|_| app.home_error.is_none() && app.home_age.is_some_and(|age| age < 10))
            {
                let target = pending.target().clone();
                let observed = app
                    .snapshot
                    .ships
                    .iter()
                    .find(|s| s.id == target.ship)
                    .and_then(BoardTarget::from_ship);
                let liaison_changed = matches!(pending, PendingBoard::Liaison(_))
                    && (app.snapshot.presentation_error.is_some()
                        || app.snapshot.ships.iter().filter(|s| s.liaison).count() != 1
                        || !app
                            .snapshot
                            .ships
                            .iter()
                            .any(|s| s.liaison && s.id == target.ship)
                        || observed.as_ref() != Some(&target));
                if liaison_changed {
                    app.pending_board = None;
                    app.notice =
                        "Liaison changed or unavailable; automatic Board cancelled.".into();
                } else if matches!(pending, PendingBoard::Liaison(_))
                    && app.ship.as_ref() != Some(&target.ship)
                {
                    app.pending_board = None;
                    app.notice = "Navigation changed; automatic Board cancelled.".into();
                } else if let Some(observed) = observed {
                    if observed != target {
                        app.pending_board = None;
                        app.notice = "Ship home changed; automatic Board cancelled.".into();
                    } else {
                        if matches!(pending, PendingBoard::Onboarding(_)) {
                            app.select_ship(Some(target.ship.clone()));
                        }
                        if herdr.is_some() {
                            app.board_requested = true;
                        } else {
                            app.notice = format!("Created {}.", target.ship);
                            app.pending_board = None;
                        }
                    }
                }
            }
            if app.board_requested {
                app.board_requested = false;
                if request_board(&mut app, herdr.as_ref().map(|(_, sender)| sender))
                    && app.pending_board.as_ref().is_some_and(|pending| {
                        app.snapshot
                            .ships
                            .iter()
                            .find(|s| Some(&s.id) == app.ship.as_ref())
                            .and_then(BoardTarget::from_ship)
                            .as_ref()
                            == Some(pending.target())
                    })
                {
                    app.pending_board = None;
                }
            }
            if let Some(action) = app.action.take() {
                if let Some((_, sender)) = &mailbox {
                    if sender.try_send(MailAction::Send(action)).is_ok() {
                        app.mail_pending = true;
                        app.notice = "Sending through Privateer…".into();
                    } else {
                        app.notice = "Mailbox action already pending; draft retained.".into();
                    }
                }
            }
            app.selected = app.selected.min(app.selection_len().saturating_sub(1));
            if app.tick_notice(Instant::now()) {
                redraw = true;
            }
            if redraw || last_draw.elapsed() >= Duration::from_secs(1) {
                terminal.draw(|frame| app.draw(frame))?;
                last_draw = Instant::now();
                redraw = false;
            }
            if !event::poll(Duration::from_secs(1))? {
                continue;
            }
            let input = event::read()?;
            if let Event::Mouse(mouse) = input {
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                ) {
                    app.handle_mouse(mouse);
                    redraw = true;
                }
            } else if let Event::Key(key) = input {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                redraw = true;
                if app.form.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if key.code == KeyCode::Char('u')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        app.edit_form(KeyCode::Delete);
                    } else if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        app.edit_form(key.code);
                    }

                    continue;
                }
                if app.composer.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        app.edit_composer(key.code);
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Esc if app.ship.is_some() || app.inbox_open => app.select_ship(None),
                    KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('0') | KeyCode::Home => app.select_ship(None),
                    KeyCode::Char('i') => app.open_inbox(),
                    KeyCode::Char('l') => app.talk_to_liaison(),
                    KeyCode::Char('m') => app.compose(),
                    KeyCode::Char('b') if app.ship.is_some() => app.board_requested = true,
                    KeyCode::Char('n') => app.manage(privateer::dashboard::forms::Kind::New),
                    KeyCode::Char('x') if app.ship.is_some() => {
                        app.manage(privateer::dashboard::forms::Kind::Retire)
                    }
                    KeyCode::Char('d') if app.ship.is_some() => {
                        app.manage(privateer::dashboard::forms::Kind::Dispatcher)
                    }
                    KeyCode::Char('r') if app.inbox_open => app.reply(),
                    KeyCode::Enter if app.inbox_open => {
                        if let (Some(mail), Some((_, sender))) =
                            (app.mail.get(app.selected), &mailbox)
                        {
                            app.notice = if sender.try_send(MailAction::Read(mail.clone())).is_ok()
                            {
                                "Recording read…".into()
                            } else {
                                "Mailbox action already pending.".into()
                            };
                        }
                    }
                    KeyCode::Left => app.focus_content = false,
                    KeyCode::Right => app.focus_content = app.ship.is_some() || app.inbox_open,
                    KeyCode::Char('1'..='4') => {
                        if let KeyCode::Char(n) = key.code {
                            app.select_tab(n as usize - '1' as usize);
                        }
                    }
                    KeyCode::Tab => {
                        app.select_tab((app.tab + 1) % 4);
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(true),
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(false),
                    KeyCode::PageDown => app.scroll = app.scroll.saturating_add(8),
                    KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(8),
                    KeyCode::Char('c') => app.compact = !app.compact,
                    KeyCode::Enter if !app.focus_content => {
                        app.focus_content = app.ship.is_some() || app.inbox_open
                    }
                    KeyCode::Enter | KeyCode::Char('f')
                        if app.ship.is_some() && matches!(app.tab, 0 | 2) =>
                    {
                        if app.selected_pane().is_none() {
                            app.notice = "No terminal is connected to this ship.".into();
                            continue;
                        }
                        if app.demo {
                            app.notice = "Demo terminal only; no live agent is running.".into();
                            continue;
                        }
                        if let (Some(view), Some((_, sender))) = (&app.herdr, &herdr) {
                            if !app.selection_invalidated
                                && app.herdr_error.is_none()
                                && app.herdr_age.is_some_and(|age| age < 10)
                            {
                                if let Some(pane) = app.selected_pane() {
                                    app.notice = if sender
                                        .try_send(HerdrAction::Focus(Focus {
                                            observed: view.clone(),
                                            pane: pane.clone(),
                                        }))
                                        .is_ok()
                                    {
                                        "Checking terminal identity before opening…".into()
                                    } else {
                                        "Terminal request already pending.".into()
                                    };
                                }
                            } else {
                                app.notice = "Herdr observations or selection changed; reconnect and select an agent again.".into();
                            }
                        } else {
                            app.notice = "No live Herdr connection configured.".into();
                        }
                    }
                    _ => {}
                }
            } else if matches!(input, Event::Resize(_, _)) {
                redraw = true;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn main() {
    if let Err(error) = options().and_then(|o| o.map_or(Ok(()), run)) {
        eprintln!("privateer-dashboard: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_app() -> App {
        let mut app = App {
            snapshot: Snapshot::demo(),
            herdr: Some(View::demo()),
            home_age: Some(0),
            herdr_age: Some(0),
            ..App::default()
        };
        app.select_ship(Some("apps".into()));
        app
    }

    #[test]
    fn board_reports_acceptance_only_after_the_bounded_queue_accepts_it() {
        let mut app = ready_app();
        let (sender, receive) = mpsc::sync_channel(1);
        assert!(request_board(&mut app, Some(&sender)));
        assert!(!request_board(&mut app, Some(&sender)));
        let HerdrAction::Focus(focus) = receive.try_recv().unwrap() else {
            panic!("expected focus of the explicitly bound pane");
        };
        assert_eq!(focus.pane.pane_id, "demo:apps");
        assert!(request_board(&mut app, Some(&sender)));
        assert!(receive.try_recv().is_ok());
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(!request_board(&mut app, None));
    }

    #[test]
    fn board_refuses_missing_failed_stale_or_invalid_observations() {
        for condition in [
            "home-error",
            "home-stale",
            "home-unknown",
            "herdr-missing",
            "herdr-error",
            "herdr-stale",
            "herdr-unknown",
            "invalid-selection",
            "ambiguous",
            "demo",
        ] {
            let mut app = ready_app();
            match condition {
                "home-error" => app.home_error = Some("unavailable".into()),
                "home-stale" => app.home_age = Some(10),
                "home-unknown" => app.home_age = None,
                "herdr-missing" => app.herdr = None,
                "herdr-error" => app.herdr_error = Some("unavailable".into()),
                "herdr-stale" => app.herdr_age = Some(10),
                "herdr-unknown" => app.herdr_age = None,
                "invalid-selection" => app.selection_invalidated = true,
                "ambiguous" => app.snapshot.presentation_error = Some("ambiguous".into()),
                "demo" => app.demo = true,
                _ => unreachable!(),
            }
            let (sender, receive) = mpsc::sync_channel(1);
            assert!(!request_board(&mut app, Some(&sender)), "{condition}");
            assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        }
    }

    #[test]
    fn pending_board_requires_the_created_home_for_focus_and_launch() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        let expected = BoardTarget {
            ship: "apps".into(),
            home: std::fs::canonicalize(&original).unwrap(),
        };
        let mut app = ready_app();
        app.pending_board = Some(PendingBoard::Onboarding(expected.clone()));
        let ship = app
            .snapshot
            .ships
            .iter_mut()
            .find(|s| s.id == "apps")
            .unwrap();
        ship.live = Some(fleet::ShipState {
            home: replacement,
            ..fleet::ShipState::default()
        });
        let (sender, receive) = mpsc::sync_channel(1);
        assert!(!request_board(&mut app, Some(&sender)));
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let ship = app
            .snapshot
            .ships
            .iter_mut()
            .find(|s| s.id == "apps")
            .unwrap();
        ship.live.as_mut().unwrap().home = original.join(".");
        assert!(request_board(&mut app, Some(&sender)));
        assert!(matches!(receive.try_recv(), Ok(HerdrAction::Focus(_))));
        app.snapshot
            .ships
            .iter_mut()
            .find(|s| s.id == "apps")
            .unwrap()
            .terminals
            .clear();
        assert!(request_board(&mut app, Some(&sender)));
        let HerdrAction::Board { home, name, .. } = receive.try_recv().unwrap() else {
            panic!("expected a Board launch");
        };
        assert_eq!(home, expected.home);
        assert_eq!(name, expected.ship);
    }
}
