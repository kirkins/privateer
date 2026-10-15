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
    ui::App,
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
            "--help" | "-h" => {
                println!(
                    "Privateer — the bridge\n\nprivateer-dashboard --demo [--once]\nprivateer-dashboard [--home /absolute/home] [--herdr-socket /absolute/socket]\n\n--once             Print a bounded JSON snapshot; no terminal required\n--no-herdr         Observe Privateer only, without connecting to Herdr\n--ascii            Plain ASCII attention markers\n\nStart on Fleet overview; select a ship, then use 1–4 for its views. 0 returns home.\ni: captain's inbox; l: liaison; m: Status/Ask/Message composer.\nInside Herdr, uses HERDR_SOCKET_PATH when HERDR_ENV=1.\nRead-only fleet view. Enter in Terminals focuses an existing agent; no launches,\ninput injection, inference, or fleet updates. Live mail uses Privateer commands."
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
            let completed_send = matches!(&action, Some(MailAction::Send(_)));
            let notice=action.map(|a| match a {
                MailAction::Read(mail)=>match mailbox.mark_read(&binary,&mail) {Ok(())=>"Marked read; earlier unseen mail remains unread.".into(),Err(e)=>format!("Acknowledgment failed: {e}")},
                MailAction::Send(a)=>match privateer::dashboard::command::json(&binary,&a.home,&a.verb,&a.args) {Ok(_)=>{sent=true;"Message delivered to Privateer; the runtime records delivery and replies.".into()},Err(e)=>format!("Message command failed: {e}; draft retained with the same id for retry")}
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
                })
                .is_err()
            {
                break;
            }
        }
    });
    (receive, actions)
}

fn herdr_worker(path: PathBuf) -> (Receiver<Sample<View>>, SyncSender<Focus>) {
    let (send, receive) = mpsc::sync_channel(1);
    let (focus, commands) = mpsc::sync_channel::<Focus>(1);
    thread::spawn(move || {
        let mut delay = Duration::ZERO;
        let mut failures = 0_u32;
        loop {
            let action = match commands.recv_timeout(delay) {
                Ok(action) => Some(action),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let mut notice = None;
            let value = (|| {
                let mut client = Client::connect(&path)?;
                if let Some(action) = action {
                    notice = Some(match client.focus(&action.observed, &action.pane) {
                        Ok(()) => {
                            "Opened agent terminal in Herdr; use Herdr navigation to return.".into()
                        }
                        Err(error) => format!("Terminal was not opened: {error}"),
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

fn run(opts: Options) -> Result<()> {
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
        home_age: Some(0),
        ..App::default()
    };
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
                    let selected = app
                        .mail
                        .get(app.selected)
                        .map(|m| (m.home.clone(), m.id.clone()));
                    app.mail = update.messages;
                    app.mail_errors = update.errors;
                    if app.inbox_open {
                        app.selected = selected
                            .and_then(|(home, id)| {
                                app.mail.iter().position(|m| m.home == home && m.id == id)
                            })
                            .unwrap_or(0);
                    }
                    if let Some(notice) = update.notice {
                        app.notice = notice;
                    }
                    if update.completed_send {
                        app.mail_pending = false;
                    }
                    if update.sent {
                        app.composer = None;
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
                    KeyCode::Char('l') => app.open_liaison(),
                    KeyCode::Char('m') => app.compose(),
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
                                        .try_send(Focus {
                                            observed: view.clone(),
                                            pane: pane.clone(),
                                        })
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
