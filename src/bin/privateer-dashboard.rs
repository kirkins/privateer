use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use privateer::dashboard::{
    herdr::{Client, Pane, View},
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
    reduced: bool,
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
            "--reduced-motion" => out.reduced = true,
            "--ascii" => out.ascii = true,
            "--no-herdr" => out.no_herdr = true,
            "--help" | "-h" => {
                println!(
                    "Privateer — the bridge\n\nprivateer-dashboard --demo [--once]\nprivateer-dashboard --home /absolute/home [--herdr-socket /absolute/socket]\n\n--once             Print a bounded JSON snapshot; no terminal required\n--no-herdr         Observe Privateer only, without connecting to Herdr\n--reduced-motion   Static boat (m toggles in the dashboard)\n--ascii            Plain ASCII boat\n\nInside Herdr, uses HERDR_SOCKET_PATH when HERDR_ENV=1.\nRead-only home view. Enter in Terminals focuses an existing agent; no launches,\ninput injection, acknowledgments, inference, or fleet updates."
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
    ensure!(
        out.demo || out.home.is_some(),
        "provide --home or PRIVATEER_HOME, or use --demo"
    );
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

fn state_worker(home: PathBuf) -> Receiver<Sample<Snapshot>> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let value = Snapshot::read(&home);
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
        Snapshot::read(opts.home.as_ref().unwrap())
            .context("open existing Privateer home read-only")?
    };
    if opts.once {
        let (herdr, error) = match opts.socket.as_ref() {
            Some(path) => match Client::connect(path).and_then(|mut c| c.snapshot()) {
                Ok(view) => (Some(view), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, None),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"demo":opts.demo,"read_only":true,"snapshot":snapshot,"herdr":herdr,"herdr_error":error})
            )?
        );
        return Ok(());
    }
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive dashboard requires a terminal; use --once for JSON"
    );
    let home_updates = opts.home.map(state_worker);
    let herdr = opts.socket.map(herdr_worker);
    let mut app = App {
        snapshot,
        calm: true,
        reduced_motion: opts.reduced,
        ascii: opts.ascii,
        demo: opts.demo,
        home_age: Some(0),
        ..App::default()
    };
    let mut home_updated = Instant::now();
    let mut herdr_updated: Option<Instant> = None;
    let mut terminal = ratatui::try_init()?;
    let mut wave_tick = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            if let Some(updates) = &home_updates {
                while let Ok(update) = updates.try_recv() {
                    match update.value {
                        Ok(snapshot) => {
                            app.snapshot = snapshot;
                            app.home_error = None;
                            home_updated = update.at;
                        }
                        Err(error) => app.home_error = Some(error.to_string()),
                    }
                }
            } else {
                home_updated = Instant::now();
            }
            if let Some((updates, _)) = &herdr {
                while let Ok(update) = updates.try_recv() {
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
            app.selected = app.selected.min(app.selection_len().saturating_sub(1));
            if wave_tick.elapsed() >= Duration::from_millis(220) {
                if app.animation_active() {
                    app.boat.advance();
                }
                wave_tick = Instant::now();
            }
            terminal.draw(|frame| app.draw(frame))?;
            if !event::poll(Duration::from_millis(if app.animation_active() {
                220
            } else {
                1000
            }))? {
                continue;
            }
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('1'..='4') => {
                        if let KeyCode::Char(n) = key.code {
                            app.tab = n as usize - '1' as usize;
                            app.selected = 0;
                            app.scroll = 0;
                            app.selection_invalidated = false;
                        }
                    }
                    KeyCode::Tab => {
                        app.tab = (app.tab + 1) % 4;
                        app.selected = 0;
                        app.scroll = 0;
                        app.selection_invalidated = false;
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.move_selection(true),
                    KeyCode::Up | KeyCode::Char('k') => app.move_selection(false),
                    KeyCode::PageDown => app.scroll = app.scroll.saturating_add(8),
                    KeyCode::PageUp => app.scroll = app.scroll.saturating_sub(8),
                    KeyCode::Char('c') => app.calm = !app.calm,
                    KeyCode::Char('m') => app.reduced_motion = !app.reduced_motion,
                    KeyCode::Enter | KeyCode::Char('f') if app.tab == 3 => {
                        if let (Some(view), Some((_, sender))) = (&app.herdr, &herdr) {
                            if !app.selection_invalidated
                                && app.herdr_error.is_none()
                                && app.herdr_age.is_some_and(|age| age < 10)
                            {
                                if let Some(pane) = view.panes.get(app.selected) {
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
