use anyhow::{Context, Result, bail, ensure};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
};
use privateer::{
    dashboard::{
        bridge::{self, Answer, Attention},
        captain::Mail,
        fleet,
        herdr::{Client, Pane, View},
        mailbox::Mailbox,
        model::Snapshot,
        ui::App,
    },
    store::Caller,
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
    once: bool,
    ascii: bool,
    reduced_motion: bool,
    no_herdr: bool,
    hosted: bool,
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
            "--once" => out.once = true,
            "--ascii" => out.ascii = true,
            "--reduced-motion" => out.reduced_motion = true,
            "--no-herdr" => out.no_herdr = true,
            "--help" | "-h" => {
                println!(
                    "Privateer · bridge\n\n\
privateer-dashboard [--home /absolute/home] [--herdr-socket /absolute/socket]\n\
--once: bounded read-only JSON observation\n\
--no-herdr: do not connect to Herdr\n\
--ascii: plain fleet markers (* aboard, o not aboard, x unavailable,\n\
         * liaison)\n\
--reduced-motion: hold the voyage sea still\n\n\
One column: Fleet table, Needs you, Recently done. Running crew counts jobs\n\
recorded as launching or running (not live health). ↑↓ selects; Tab switches\n\
Fleet/Needs; Enter focuses an EXISTING validated mate space or answers a typed\n\
runtime need. Choices are offered when provided; otherwise type a reply.\n\
Approvals bind exact artifact/environment/version; no deployment.\n\
i: separate mail history; v: voyage, an animated sea of the fleet (v or Esc\n\
returns); q: quit.\n\
No launch, Board, Watch, tabs, pane creation/reorder, or automatic focus."
                );
                return Ok(None);
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    ensure!(
        !(out.no_herdr && out.socket.is_some()),
        "--no-herdr conflicts with --herdr-socket"
    );
    // Only the host's no-argument direct launch adopts. Explicit --once,
    // --home, etc. remain the existing observation-only entry points,
    // even if a terminal happens to inherit a launch token.
    out.hosted = std::env::args_os().len() == 1 && privateer::host::bridge::requested();
    if out.home.is_none() && !out.hosted {
        out.home = std::env::var_os("PRIVATEER_HOME").map(PathBuf::from);
    }
    if out.socket.is_none() && std::env::var("HERDR_ENV").as_deref() == Ok("1") {
        out.socket = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    }
    if out.no_herdr {
        out.socket = None;
    }
    Ok(Some(out))
}

struct Update {
    snapshot: Result<Snapshot>,
    attention: Attention,
    mail: Vec<Mail>,
    errors: Vec<String>,
    outcome: Option<Result<String>>,
}
enum Action {
    Answer(Answer),
    Read(Mail),
    History,
}
/// One bounded page/home per pass. A single action queue; no observation on the input loop.
fn state_worker(
    registry: PathBuf,
    fallback: Option<PathBuf>,
) -> (Receiver<Update>, SyncSender<Action>) {
    let (send, updates) = mpsc::sync_channel(1);
    let (actions, commands) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut attention = Attention::default();
        let mut mailbox = Mailbox::default();
        let mut history = false;
        let mut snapshot = Snapshot::default();
        let mut observed = Instant::now() - Duration::from_secs(2);
        let mut failure = None::<String>;
        loop {
            let action = match commands.recv_timeout(Duration::from_millis(250)) {
                Ok(a) => Some(a),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            };
            if action.is_some() || observed.elapsed() >= Duration::from_secs(2) {
                match fleet::read(&registry, fallback.as_deref()) {
                    Ok(s) => {
                        snapshot = s;
                        failure = None;
                    }
                    Err(e) => failure = Some(e.to_string()),
                }
                observed = Instant::now();
            }
            let outcome = match action {
                Some(Action::Answer(answer)) => {
                    let result = if failure.is_some() {
                        Err(anyhow::anyhow!("Fleet unavailable; draft retained"))
                    } else {
                        bridge::execute(&snapshot, fallback.as_deref(), &attention, &answer).map(
                            |_| {
                                "Recorded answer; only authoritative refresh clears the item."
                                    .into()
                            },
                        )
                    };
                    attention.refresh_home(&answer.need.home);
                    Some(result)
                }
                Some(Action::Read(mail)) => Some(if failure.is_some() {
                    Err(anyhow::anyhow!("Fleet unavailable; read refused"))
                } else {
                    mailbox
                        .mark_read_existing(&snapshot, fallback.as_deref(), &mail)
                        .map(|()| {
                            "Marked contiguous read prefix; acknowledgement is not resolution."
                                .into()
                        })
                }),
                Some(Action::History) => {
                    history = true;
                    None
                }
                None => None,
            };
            if failure.is_none() {
                attention.step(&snapshot, fallback.as_deref());
            } else {
                attention.error = Some(
                    "Fleet observation unavailable; retained attention is NOT an all-clear".into(),
                );
            }
            if history && (outcome.is_some() || observed.elapsed() < Duration::from_millis(250)) {
                mailbox.refresh_readonly(&snapshot, fallback.as_deref());
            }
            if send
                .send(Update {
                    snapshot: failure
                        .as_ref()
                        .map_or_else(|| Ok(snapshot.clone()), |e| Err(anyhow::anyhow!(e.clone()))),
                    attention: attention.clone(),
                    mail: mailbox.messages.clone(),
                    errors: mailbox.errors.clone(),
                    outcome,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (updates, actions)
}
#[derive(Clone)]
struct Focus {
    observed: View,
    pane: Pane,
    app_snapshot: Snapshot,
    ship: String,
}
struct HerdrUpdate {
    value: Result<View>,
    notice: Option<String>,
}
fn herdr_worker(
    path: PathBuf,
    registry: PathBuf,
    fallback: Option<PathBuf>,
) -> (Receiver<HerdrUpdate>, SyncSender<Focus>) {
    let (send, updates) = mpsc::sync_channel(1);
    let (actions, commands) = mpsc::sync_channel::<Focus>(1);
    thread::spawn(move || {
        let mut delay = Duration::ZERO;
        let mut failures = 0_u32;
        loop {
            let action = match commands.recv_timeout(delay) {
                Ok(a) => Some(a),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            };
            let mut notice = None;
            let value = (|| -> Result<View> {
                let mut client = Client::connect(&path)?;
                if let Some(action) = action {
                    let result = (|| -> Result<()> {
                        // Re-read current home/custody/presentation; never launch on a missing binding.
                        let current = fleet::read(&registry, fallback.as_deref())?;
                        let before = action
                            .app_snapshot
                            .ships
                            .iter()
                            .find(|s| s.id == action.ship)
                            .context("ship selection missing")?;
                        let after = current
                            .ships
                            .iter()
                            .find(|s| s.id == action.ship)
                            .context("ship retired")?;
                        ensure!(
                            before.live.as_ref().map(|l| (&l.home, &l.identity))
                                == after.live.as_ref().map(|l| (&l.home, &l.identity)),
                            "ship home/identity changed"
                        );
                        let mut app = App {
                            snapshot: current,
                            herdr: Some(action.observed.clone()),
                            home_age: Some(0),
                            herdr_age: Some(0),
                            ..App::default()
                        };
                        app.select_ship(Some(action.ship));
                        let (_, pane) = app.focus_target()?;
                        ensure!(
                            pane == action.pane,
                            "mate binding changed; refresh and reselect"
                        );
                        client.focus(&action.observed, &action.pane)
                    })();
                    notice = Some(match result {
                        Ok(()) => "Focused existing mate space.".into(),
                        Err(e) => format!("Focus refused: {e}"),
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
            if send.send(HerdrUpdate { value, notice }).is_err() {
                break;
            }
        }
    });
    (updates, actions)
}
fn request_focus(app: &mut App, sender: Option<&SyncSender<Focus>>) {
    app.notice = match app.focus_target() {
        Err(e) => format!("Focus refused: {e}"),
        Ok((observed, pane)) => match sender {
            Some(sender)
                if sender
                    .try_send(Focus {
                        observed,
                        pane,
                        app_snapshot: app.snapshot.clone(),
                        ship: app.ship.clone().unwrap_or_default(),
                    })
                    .is_ok() =>
            {
                "Revalidating existing mate identity…".into()
            }
            _ => "Focus unavailable/already pending.".into(),
        },
    };
}
struct MouseCapture;
impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}
fn run(opts: Options) -> Result<()> {
    Caller::from_env(None, None)?.require_operator("dashboard")?;
    // A direct hosted process holds custody independently of its dispatcher.
    // Ordinary explicit invocations have no token and acquire nothing. This
    // startup handshake never creates/reorders/focuses a pane; observation below
    // remains read-only and observes the same fleet from every launching home.
    let _hosted_bridge = if opts.hosted {
        privateer::host::bridge::adopt()?
    } else {
        None
    };
    let registry = privateer::fleet::registry_path()?;
    let snapshot = fleet::read(&registry, opts.home.as_deref())?;
    if opts.once {
        let (herdr, error) = match opts.socket.as_ref() {
            Some(path) => match Client::connect(path).and_then(|mut c| c.snapshot()) {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, None),
        };
        let mut mailbox = Mailbox::default();
        mailbox.refresh_readonly(&snapshot, opts.home.as_deref());
        let mut attention = Attention::default();
        // --once is deliberately ONE page/home step, not an unbounded fleet scan.
        attention.step(&snapshot, opts.home.as_deref());
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"read_only":true,"snapshot":snapshot,"herdr":herdr,"herdr_error":error,"attention":attention,"mail":mailbox.messages,"mail_errors":mailbox.errors})
            )?
        );
        return Ok(());
    }
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "interactive dashboard requires a terminal; use --once for JSON"
    );
    let (updates, actions) = state_worker(registry.clone(), opts.home.clone());
    let herdr = opts
        .socket
        .map(|path| herdr_worker(path, registry, opts.home));
    let mut app = App {
        snapshot,
        ascii: opts.ascii,
        reduced: opts.reduced_motion,
        home_age: Some(0),
        herdr_configured: herdr.is_some(),
        ..App::default()
    };
    let mut home_updated = Instant::now();
    let mut herdr_updated = None::<Instant>;
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<()> {
        let _capture = MouseCapture;
        execute!(io::stdout(), EnableMouseCapture)?;
        loop {
            while let Ok(update) = updates.try_recv() {
                match update.snapshot {
                    Ok(s) => {
                        app.update_snapshot(s);
                        app.home_error = None;
                        home_updated = Instant::now();
                    }
                    Err(e) => app.home_error = Some(e.to_string()),
                }
                app.update_attention(update.attention);
                app.update_mail(update.mail, update.errors);
                if let Some(outcome) = update.outcome {
                    app.mail_pending = false;
                    app.notice = match outcome {
                        Ok(notice) => {
                            app.answer = None;
                            notice
                        }
                        Err(e) => format!(
                            "Action failed: {e}; draft retained. Refresh/reselect if stale."
                        ),
                    };
                }
            }
            if let Some((updates, _)) = &herdr {
                while let Ok(update) = updates.try_recv() {
                    if let Some(notice) = update.notice {
                        app.notice = notice;
                    }
                    match update.value {
                        Ok(v) => {
                            app.update_herdr(v);
                            app.herdr_error = None;
                            herdr_updated = Some(Instant::now());
                        }
                        Err(e) => app.herdr_error = Some(e.to_string()),
                    }
                }
            }
            app.home_age = Some(home_updated.elapsed().as_secs());
            app.herdr_age = herdr_updated.map(|t| t.elapsed().as_secs());
            app.tick_notice(Instant::now());
            terminal.draw(|frame| app.draw(frame))?;
            if !event::poll(Duration::from_millis(200))? {
                continue;
            }
            match event::read()? {
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        continue;
                    }
                    if app.answer.is_none() && key.code == KeyCode::Char('q') {
                        break;
                    }
                    if app.bridge_key(key.code) {
                        request_focus(&mut app, herdr.as_ref().map(|(_, s)| s));
                    }
                    if let Some(mail) = app.history_read.take() {
                        if actions.try_send(Action::Read(mail)).is_ok() {
                            app.mail_pending = true;
                        } else {
                            app.notice = "History read queue busy; reselect and retry.".into();
                        }
                    }
                    if app.inbox_open && key.code == KeyCode::Char('i') {
                        let _ = actions.try_send(Action::History);
                    }
                    if let Some(answer) = app.answer_action.take() {
                        if actions.try_send(Action::Answer(answer)).is_ok() {
                            app.mail_pending = true;
                            app.notice =
                                "Revalidating exact runtime item and recording answer…".into();
                        } else {
                            app.notice = "Action queue busy; draft retained.".into();
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}
fn main() {
    if let Err(e) = options().and_then(|o| o.map_or(Ok(()), run)) {
        eprintln!("privateer-dashboard: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use privateer::dashboard::model::{Discovery, Ship, TerminalBinding};

    /// One ship whose only terminal is the pane Herdr reports; no sample data.
    fn fixture() -> (Snapshot, View) {
        let snapshot = Snapshot {
            ships: vec![Ship {
                id: "apps".into(),
                name: "Apps".into(),
                terminals: vec![TerminalBinding {
                    pane_id: "w1:p1".into(),
                    terminal_id: "terminal-1".into(),
                }],
                ..Ship::default()
            }],
            discovery: Discovery::Complete,
            ..Snapshot::default()
        };
        let view = View {
            version: "test".into(),
            protocol: 20,
            panes: vec![Pane {
                pane_id: "w1:p1".into(),
                terminal_id: "terminal-1".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                agent: Some("codex".into()),
                agent_status: "idle".into(),
                title: None,
                agent_session: None,
            }],
            socket_identity: (1, 1),
        };
        (snapshot, view)
    }

    #[test]
    fn actual_enter_path_queues_only_existing_focus_and_refuses_stale_or_invalid() {
        let (snapshot, view) = fixture();
        let mut app = App {
            snapshot,
            herdr: Some(view),
            home_age: Some(0),
            herdr_age: Some(0),
            ..App::default()
        };
        app.select_ship(Some("apps".into()));
        let (send, receive) = mpsc::sync_channel(1);
        assert!(app.bridge_key(KeyCode::Enter));
        request_focus(&mut app, Some(&send));
        assert_eq!(receive.try_recv().unwrap().pane.pane_id, "w1:p1");
        // Display clipping alone does not remove validated existing focus.
        app.snapshot.truncated = true;
        request_focus(&mut app, Some(&send));
        assert_eq!(receive.try_recv().unwrap().pane.pane_id, "w1:p1");
        for case in ["stale", "ambiguous", "incomplete", "unknown", "invalid"] {
            app.herdr_age = Some(0);
            app.snapshot.presentation_error = None;
            app.snapshot.discovery = Discovery::Complete;
            app.selection_invalidated = false;
            match case {
                "stale" => app.herdr_age = Some(10),
                "ambiguous" => app.snapshot.presentation_error = Some("conflict".into()),
                "incomplete" => app.snapshot.discovery = Discovery::Incomplete,
                "unknown" => app.snapshot.discovery = Discovery::Unknown,
                _ => app.selection_invalidated = true,
            }
            request_focus(&mut app, Some(&send));
            assert!(receive.try_recv().is_err(), "{case} focus was queued");
        }
    }
}
