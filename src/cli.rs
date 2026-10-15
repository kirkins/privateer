//! Commands for people: `privateer init`, `ask`, `inbox` and friends.
//!
//! A ship is a dedicated folder. Its state lives in `<folder>/.privateer`,
//! found from the current directory upward the way git finds `.git`, and the
//! folder is its charter's repository root, with the projects it works on
//! cloned into `projects/`. `init` turns a folder into a ship with sensible
//! defaults and writes the instructions that make any harness launched there
//! its first mate. JSON commands for agents and RPC are unchanged.
use crate::store::{Store, identifier};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".privateer";
/// Commands that mean the human form when given without `--json`.
pub const VERBS: &[&str] = &[
    "init", "status", "ships", "ask", "tell", "inbox", "reply", "wakes", "project", "start",
    "hook", "service", "ship",
];
const GENERATED: &str = "<!-- privateer:generated -->";
/// Harness binaries in order of preference, with their bundled adapter ids.
const HARNESSES: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("pi", "pi"),
    ("omp", "omp"),
    ("opencode", "opencode"),
    ("gemini", "gemini"),
    ("cursor-agent", "cursor"),
    ("grok", "grok"),
];

/// The ship home for the current directory: the nearest `.privateer`.
pub fn discover_home() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(STATE_DIR);
        if candidate.join("privateer.sqlite").is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

/// The bundled adapters: `PRIVATEER_PLUGINS`, a `plugins/` directory beside
/// or above the executable (a source checkout's target/ build), or the
/// checkout the binary was built from.
fn plugins_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PRIVATEER_PLUGINS") {
        return Ok(PathBuf::from(dir));
    }
    let executable = std::env::current_exe()?;
    for dir in executable.ancestors().skip(1) {
        for candidate in [dir.join("plugins"), dir.join("share/privateer/plugins")] {
            if candidate.join("native.py").is_file() {
                return Ok(candidate);
            }
        }
    }
    // A binary installed with `cargo install --path` still finds the
    // checkout it was built from.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins");
    if source.join("native.py").is_file() {
        return Ok(source);
    }
    bail!("cannot find Privateer's bundled plugins; set PRIVATEER_PLUGINS to its plugins directory")
}

/// The harness a new ship's first mate will use: the one named, or the
/// first supported harness installed.
fn detect_harness(plugins: &Path, named: Option<&String>) -> Result<String> {
    match named {
        Some(id) => Ok(id.clone()),
        None => HARNESSES
            .iter()
            .find(|(binary, id)| {
                on_path(binary) && plugins.join(id).join("manifest.json").is_file()
            })
            .map(|(_, id)| (*id).to_owned())
            .context(
                "no supported AI harness found on PATH (claude, codex, pi, …); pass --harness ID",
            ),
    }
}

fn ship_name(folder: &Path) -> Result<String> {
    let raw = folder
        .file_name()
        .and_then(|n| n.to_str())
        .context("cannot name a ship after this folder; pass --ship NAME")?;
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    identifier(&name).with_context(|| format!("cannot name a ship {raw:?}; pass --ship NAME"))?;
    Ok(name)
}

struct Flags {
    positional: Vec<String>,
    values: std::collections::HashMap<String, String>,
    switches: Vec<String>,
}

fn flags(args: &[String], with_value: &[&str]) -> Result<Flags> {
    let mut out = Flags {
        positional: vec![],
        values: Default::default(),
        switches: vec![],
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            if with_value.contains(&name) {
                let value = iter
                    .next()
                    .with_context(|| format!("--{name} needs a value"))?;
                out.values.insert(name.into(), value.clone());
            } else {
                out.switches.push(name.into());
            }
        } else {
            out.positional.push(arg.clone());
        }
    }
    Ok(out)
}

fn instructions(ship: &str, scope: &str, cli: &str) -> String {
    let scope = if scope.is_empty() {
        String::new()
    } else {
        format!(" Its scope: {scope}.")
    };
    format!(
        r#"{GENERATED}
# First mate of {ship}

You are the first mate of the Privateer ship **{ship}**.{scope} The captain, the person you are talking to, gives you direction. Ships are peers: you coordinate with other ships through mail, never by command, and you run your own crew as supervised jobs. This file is written by `privateer init`; remove the first line to keep your own edits.

Run Privateer as `{cli} COMMAND --json '{{...}}'`.

## At the start of every session

1. `ship.bearings {{"ship":"{ship}"}}`: open tasks, requests owed and owing, ship knowledge.
2. `mail.inbox {{"address":"{ship}"}}`: new mail. Answer requests, act on notices, then `mail.ack {{"address":"{ship}","through_seq":N}}`.
3. `note.list {{"ship":"{ship}"}}` and `note.get {{"ship":"{ship}","focus":"task:<id>"}}`: the handoff notes you left for each focus.

## Working

- Mail as yourself: `mail.send {{"from":"{ship}","to":"<ship or captain>","kind":"request|notice|reply","body":"...","task_id":"<optional>"}}`; a reply sets `correlation` to the request id. `ship.list {{}}` shows who handles what.
- Builds and test suites that load the machine: `job.start {{"task_id":"<task>","command":[...],"heavy":true,"wait_for_capacity":true}}`. You need not poll it: when it ends you get its outcome.
- Delegate with `task.create {{"id":"...","owner":"<ship>","title":"...","charter_id":"{ship}"}}`; hand off with `task.handoff`, which takes effect when the recipient runs `task.accept`.
- History for a focus: `focus.digest {{"ship":"{ship}","focus":"task:<id>"}}`.
- Before you stop, replace the handoff note for each focus you worked on: `note.put {{"ship":"{ship}","focus":"task:<id>|thread:<id>|captain|ship","body":"..."}}`. Keep it under 2 KB and to judgement: what you are doing and why, what you are waiting on, what comes next. Facts are already recorded. Put lasting preferences and conventions in the `ship` note.
- Repositories live in `projects/`; the captain adds them with `privateer project add <git url>`.
- New area of work? Create a ship for it: `ship.create {{"name":"...","scope":"...","objective":"..."}}` (beside this folder; its charter can only have actions yours has). Retire ships you created with `ship.retire {{"name":"..."}}`. `fleet.list {{}}` shows every ship on this machine.

## Crew

Crew are the jobs you start (`job.start`, or `agent.start` for another harness) on your tasks. When one finishes, fails, times out or is orphaned, the runtime mails you its outcome from `crew`, under the task's focus: status, exit code, reason, duration and the end of its logs, taken from the job's record. An exit status is not proof: check the result yourself (`job.status`, the logs, the work) before relying on it or reporting it, and record your judgement in the task's note. A crew member that needs a decision sends you a request from `crew`; answer it with `mail.send {{"from":"{ship}","to":"crew","kind":"reply","correlation":"<request id>","body":"..."}}`.

If you are crew yourself (`PRIVATEER_JOB` is set and you were not woken as first mate), report to your first mate with `crew.report {{"kind":"progress","body":"..."}}`, or ask with `crew.report {{"kind":"decision","body":"..."}}` and read the answer with `crew.answer {{"id":"<returned id>","wait_secs":300}}`.

## Rules

- Speak only as {ship}; never claim to be another ship or the captain.
- Production deployment needs the captain's approval or a standing grant.
- Do not claim a message, test, merge or deployment succeeded without evidence from Privateer or the tool.
"#
    )
}

/// Add Privateer's Bash permission to a Claude project settings file,
/// keeping whatever else it holds.
fn claude_settings(folder: &Path, binary: &str) -> Result<()> {
    let path = folder.join(".claude/settings.json");
    let mut settings: Value = match fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw).context("cannot read .claude/settings.json")?,
        Err(_) => json!({}),
    };
    let rule = format!("Bash({binary}:*)");
    let allow = settings
        .as_object_mut()
        .context(".claude/settings.json must hold an object")?
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("permissions must be an object")?
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("permissions.allow must be a list")?;
    if !allow.iter().any(|r| r == &json!(rule)) {
        allow.push(json!(rule));
    }
    // Attended mode: the session registers presence, sees new mail at each
    // prompt and turn end, and releases presence when it ends.
    let hooks = settings
        .as_object_mut()
        .context(".claude/settings.json must hold an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")?;
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        let command = format!("{binary} hook claude {event}");
        let entries = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} must be a list"))?;
        let present = entries.iter().any(|entry| {
            entry["hooks"]
                .as_array()
                .is_some_and(|hs| hs.iter().any(|h| h["command"] == json!(command)))
        });
        if !present {
            entries
                .push(json!({"hooks": [{"type": "command", "command": command, "timeout": 30}]}));
        }
    }
    fs::create_dir_all(folder.join(".claude"))?;
    fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(())
}

fn ensure_lines(path: &Path, lines: &[&str]) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut text = existing.clone();
    for line in lines {
        if !existing.lines().any(|l| l.trim() == *line) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(line);
            text.push('\n');
        }
    }
    if text != existing {
        fs::write(path, text)?;
    }
    Ok(())
}

/// Add this optional adapter's hooks beside existing project hooks. No
/// global Cursor config, credentials, provider route or trust store is changed.
fn cursor_settings(folder: &Path, binary: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = folder.join(".cursor");
    let path = dir.join("hooks.json");
    for item in [&dir, &path] {
        ensure!(
            !item
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink()),
            "Cursor hook configuration cannot be a symlink"
        );
    }
    let mut settings: Value = match fs::read(&path) {
        Ok(raw) => {
            ensure!(raw.len() <= 65_536, "Cursor hooks configuration too large");
            serde_json::from_slice(&raw)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"version":1,"hooks":{}})
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(settings["version"] == 1, "unsupported Cursor hooks version");
    let hooks = settings
        .as_object_mut()
        .context("Cursor hooks must hold an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Cursor hooks must be an object")?;
    for event in ["sessionStart", "beforeSubmitPrompt", "stop", "sessionEnd"] {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Cursor hook entries must be a list")?;
        let suffix = format!(" hook cursor {event}");
        entries.retain(|e| !e["command"].as_str().is_some_and(|s| s.ends_with(&suffix)));
        let command = format!(
            "'{}'{suffix}",
            binary.to_string_lossy().replace('\'', "'\\''")
        );
        let mut entry = json!({"type":"command","command":command,"timeout":30});
        if event == "stop" {
            entry["timeout"] = json!(28_800);
            entry["loop_limit"] = Value::Null;
        }
        entries.push(entry);
    }
    fs::create_dir_all(&dir)?;
    let temp = dir.join(format!(".privateer-hooks-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(&settings)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Turn the current folder into a ship. Safe to run again: it updates the
/// ship's settings and rewrites only files it generated.
/// What to make a ship with; unset fields fall back to sensible defaults.
#[derive(Default)]
pub(crate) struct Setup {
    pub ship: Option<String>,
    pub scope: Option<String>,
    pub harness: Option<String>,
    /// Charter actions for a new charter; None means the default set.
    pub actions: Option<Value>,
    pub objective: Option<String>,
    /// Who made it, recorded in the fleet list.
    pub created_by: String,
}

/// Make (or update) the ship whose state lives in `home`, returning its name,
/// harness and whether AGENTS.md is Privateer's. Safe to run again: it keeps
/// the captain's charter edits, scope and accepted kinds, and rewrites only
/// files it generated.
pub(crate) fn setup(home: &Path, setup: &Setup) -> Result<(String, String, bool)> {
    setup_inner(home, setup, true)
}

/// Bootstrap a ship without publishing it. Fleet creation holds the creating
/// home's authority lock until setup completes, so linking must wait: it writes
/// that home through a separate connection.
pub(crate) fn setup_unlinked(home: &Path, setup: &Setup) -> Result<(String, String, bool)> {
    setup_inner(home, setup, false)
}

fn setup_inner(home: &Path, setup: &Setup, join_fleet: bool) -> Result<(String, String, bool)> {
    let folder = home.parent().context("ship folder")?.to_path_buf();
    let ship = match &setup.ship {
        Some(name) => {
            identifier(name)?;
            name.clone()
        }
        None => ship_name(&folder)?,
    };
    crate::mail::unreserved(&ship)?;
    let plugins = plugins_dir()?;
    let harness = detect_harness(&plugins, setup.harness.as_ref())?;
    let manifest = plugins.join(&harness).join("manifest.json");
    ensure!(
        manifest.is_file(),
        "no bundled adapter for harness {harness} in {}",
        plugins.display()
    );
    fs::create_dir_all(folder.join("projects"))?;
    let mut store = Store::open(home)?;
    let binary = std::env::current_exe()?;
    let cli = format!("{} --home {}", binary.display(), store.home().display());

    crate::dispatch(&mut store, "init", json!({"ship": ship}))?;
    // The shared ledger must exist before a sandboxed first mate is planned.
    crate::jobs::ensure_ledger(&store)?;
    // Created once; later edits to the charter are the captain's to keep.
    if store.charter(&ship).is_err() {
        let mut charter = json!({"id": ship,
            "objective": setup.objective.clone().unwrap_or_else(|| format!("Work as the ship {ship}")),
            "repo": folder, "min_free_bytes": 0, "job_timeout_secs": 3600});
        if let Some(actions) = &setup.actions {
            charter["actions"] = actions.clone();
        }
        crate::dispatch(&mut store, "charter.put", charter)?;
    }
    let previous = crate::dispatch(&mut store, "ship.show", json!({"name": ship})).ok();
    let scope = setup
        .scope
        .clone()
        .or_else(|| {
            previous
                .as_ref()
                .and_then(|s| s["scope"].as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    let accepts = previous
        .as_ref()
        .map(|s| s["accepts"].clone())
        .unwrap_or(json!([]));
    let duty = format!("{ship}-duty");
    if store.task(&duty).is_err() {
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id": duty, "charter_id": ship, "owner": ship,
                "title": format!("{ship} first mate: mail and coordination")}),
        )?;
    }
    // Headless first mates must be able to act: Codex writes inside its
    // sandbox, Claude may edit files.
    let options = match harness.as_str() {
        "codex" => json!({"sandbox": "workspace-write"}),
        "claude" => json!({"permission_mode": "acceptEdits"}),
        _ => json!({}),
    };
    crate::dispatch(
        &mut store,
        "ship.put",
        json!({"name": ship, "scope": scope, "accepts": accepts, "first_mate":
            {"manifest": manifest, "duty_task": duty, "options": options}}),
    )?;

    let agents = folder.join("AGENTS.md");
    let ours = fs::read_to_string(&agents).map_or(true, |t| t.starts_with(GENERATED));
    if ours {
        fs::write(&agents, instructions(&ship, &scope, &cli))?;
    }
    let claude = folder.join("CLAUDE.md");
    if !claude.exists() {
        fs::write(&claude, "@AGENTS.md\n")?;
    }
    claude_settings(&folder, &binary.display().to_string())?;
    ensure_lines(&folder.join(".gitignore"), &[".privateer/", "projects/"])?;
    if join_fleet {
        // Join the fleet: ships on this machine find and mail each other.
        crate::fleet::register(&ship, store.home(), &setup.created_by)?;
        crate::fleet::link(&store, &ship)?;
    }
    Ok((ship, harness, ours))
}

fn init(home: &Path, args: &[String], quiet: bool) -> Result<()> {
    let flags = flags(args, &["ship", "harness", "scope"])?;
    let folder = home.parent().context("ship folder")?.to_path_buf();
    let (ship, harness, ours) = setup(
        home,
        &Setup {
            ship: flags.values.get("ship").cloned(),
            scope: flags.values.get("scope").cloned(),
            harness: flags.values.get("harness").cloned(),
            created_by: "captain".into(),
            ..Setup::default()
        },
    )?;
    if quiet {
        println!(
            "Aye. {} is now the ship {ship}, crewed by {harness}.",
            folder.display()
        );
        return Ok(());
    }
    println!("Aye. {} is now the ship {ship}.", folder.display());
    println!("  first mate harness: {harness} (change with: privateer init --harness ID)");
    if !ours {
        println!("  AGENTS.md exists and was not written by Privateer, so it was left alone.");
    }
    println!("\nNext:");
    println!("  privateer project add <git url>    clone a repository for {ship} to work on");
    println!(
        "  privateer serve                     answer mail while you are away (keep it running)"
    );
    println!("  privateer start                     talk to {ship}'s first mate ({harness}) here");
    println!(
        "  privateer ask {ship} \"...\"           or send it a request from anywhere in this folder"
    );
    Ok(())
}

fn print_mail(message: &Value) {
    let state = message["state"]
        .as_str()
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default();
    println!(
        "#{} {} from {}{}  ({})",
        message["seq"],
        message["kind"].as_str().unwrap_or(""),
        message["from"].as_str().unwrap_or(""),
        state,
        message["id"].as_str().unwrap_or("")
    );
    for line in message["body"].as_str().unwrap_or("").lines() {
        println!("    {line}");
    }
}

fn text(positional: &[String], from: usize, usage: &str) -> Result<String> {
    let body = positional[from.min(positional.len())..].join(" ");
    ensure!(!body.trim().is_empty(), "usage: {usage}");
    Ok(body)
}

/// `privateer start [HARNESS] [-- ARGS]`: talk to the ship's first mate.
/// Launches the harness in the ship folder as the ship (agent role) and holds
/// the ship's presence lease while it runs, so `serve` leaves the mail to
/// this session. Claude uses hooks; Pi-family sessions receive the optional
/// native mail extension explicitly, without changing native configuration.
fn start(home: &Path, args: &[String]) -> Result<()> {
    let (ours, passthrough) = match args.iter().position(|a| a == "--") {
        Some(at) => (&args[..at], &args[at + 1..]),
        None => (args, &args[args.len()..]),
    };
    let mut store = Store::open(home)?;
    let canonical_home = store.home().to_path_buf();
    let home = canonical_home.as_path();
    let ship = crate::presence::folder_ship(&store)
        .context("this folder's ship is unknown; run `privateer init` here")?;
    let profile = crate::dispatch(&mut store, "ship.show", json!({"name": ship}))?;
    let harness = match ours.first() {
        Some(name) => name.clone(),
        None => profile["first_mate"]["manifest"]
            .as_str()
            .and_then(|m| {
                Path::new(m)
                    .parent()?
                    .file_name()?
                    .to_str()
                    .map(str::to_owned)
            })
            .context("no first-mate harness configured; pass one: privateer start claude")?,
    };
    let binary = if harness == "opencode-v2" {
        "opencode".to_owned()
    } else {
        HARNESSES
            .iter()
            .find(|(_, id)| *id == harness)
            .map_or(harness.as_str(), |(binary, _)| binary)
            .to_owned()
    };
    let folder = home.parent().context("ship folder")?;
    let holder = format!("start:{}:{}", std::process::id(), uuid::Uuid::new_v4());
    let control = std::env::current_exe()?;
    let mut command = std::process::Command::new(&binary);
    if matches!(harness.as_str(), "pi" | "pi-signed" | "omp") {
        let plugins = fs::canonicalize(plugins_dir()?)?;
        let extension = plugins.join("attended/mail.ts");
        ensure!(
            extension.is_file(),
            "attended mail adapter missing from {}",
            plugins.display()
        );
        command = std::process::Command::new("python3");
        command
            .arg(plugins.join("native_launch.py"))
            .arg(&harness)
            .arg(&binary)
            .arg("-e")
            .arg(extension);
    } else if harness == "claude" {
        let plugins = fs::canonicalize(plugins_dir()?)?;
        let plugin = plugins.join("attended-claude");
        ensure!(
            plugin.join("hooks/hooks.json").is_file(),
            "Claude attended mail plugin missing"
        );
        command = std::process::Command::new("python3");
        command
            .arg(plugins.join("native_launch.py"))
            .arg(&harness)
            .arg(&binary)
            .arg("--plugin-dir")
            .arg(plugin);
    } else if harness == "opencode-v2" {
        let plugins = fs::canonicalize(plugins_dir()?)?;
        crate::opencode_mail::install(folder, &plugins)?;
        command = std::process::Command::new("python3");
        command
            .arg(plugins.join("native_launch.py"))
            .arg(&harness)
            .arg(&binary)
            .arg("--standalone");
    } else if harness == "cursor" {
        cursor_settings(folder, &control)?;
        let plugins = fs::canonicalize(plugins_dir()?)?;
        command = std::process::Command::new("python3");
        command
            .arg(plugins.join("native_launch.py"))
            .arg(&harness)
            .arg(&binary);
    }
    command
        .args(passthrough)
        .current_dir(folder)
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", &ship)
        .env("PRIVATEER_HOME", home)
        .env("PRIVATEER_CONTROL_CLI", control)
        .env("PRIVATEER_ATTENDED_HARNESS", &harness)
        .env("PRIVATEER_PRESENCE_HOLDER", &holder);
    // The child can invoke a startup hook immediately after spawn.
    crate::presence::claim(&store, &ship, &holder, 90, Some(&harness))?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = crate::presence::leave(&store, &ship, &holder);
            return Err(error).with_context(|| format!("cannot start {binary}; is it installed?"));
        }
    };
    // Ctrl-C belongs to the harness; this process must live to release
    // presence afterwards.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heartbeat = {
        let (stop, home, ship, holder) = (
            stop.clone(),
            home.to_path_buf(),
            ship.clone(),
            holder.clone(),
        );
        std::thread::spawn(move || {
            let Ok(store) = Store::open(&home) else {
                return;
            };
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for _ in 0..30 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                if !crate::presence::refresh(&store, &ship, &holder, 90).unwrap_or(false) {
                    return;
                }
            }
        })
    };
    let status = child.wait()?;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = heartbeat.join();
    crate::presence::leave(&store, &ship, &holder)?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Bare `privateer`: get aboard. Inside a ship, make sure its dispatcher is
/// running and talk to its first mate. Elsewhere, offer to make this folder a
/// ship first; without a terminal to ask in (or `--yes`), print help.
pub fn board(home: Option<PathBuf>, yes: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};
    let home = match home {
        Some(home) => home,
        None => {
            let folder = std::env::current_dir()?;
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if !yes && !interactive {
                println!(
                    "Privateer: autonomous ships under a charter.\n\nIn a folder for a ship:  privateer          make it a ship (asks first) and talk to its first mate\n                         privateer --yes    the same, without asking\nMore commands:           privateer --help"
                );
                return Ok(());
            }
            let user_home = std::env::var_os("HOME").map(PathBuf::from);
            ensure!(
                folder.parent().is_some() && Some(&folder) != user_home.as_ref(),
                "make a dedicated folder for a ship first, for example: mkdir -p ~/ships/infra && cd ~/ships/infra"
            );
            let ship = ship_name(&folder)?;
            let harness = detect_harness(&plugins_dir()?, None)?;
            if !yes {
                print!(
                    "{} is not a ship yet. Make it the ship \"{ship}\" with {harness} as its first mate? [Y/n] ",
                    folder.display()
                );
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes") {
                    println!("Left as it is.");
                    return Ok(());
                }
            }
            let home = folder.join(STATE_DIR);
            init(&home, &[], true)?;
            home
        }
    };
    let store = Store::open(&home)?;
    let ship = crate::presence::folder_ship(&store)
        .context("this folder's ship is unknown; run `privateer init` here")?;
    drop(store);
    println!("Dispatcher: {}", crate::service::ensure(&home, &ship)?);
    start(&home, &[])
}

/// Run a human command. `home` is the discovered or given ship home.
pub fn run(home: Option<PathBuf>, verb: &str, args: &[String]) -> Result<()> {
    if verb == "init" {
        let home = match home {
            Some(home) => home,
            None => std::env::current_dir()?.join(STATE_DIR),
        };
        return init(&home, args, false);
    }
    let home = home.context(
        "not inside a ship folder (no .privateer found here or above); run `privateer init` to make this folder a ship",
    )?;
    if verb == "start" {
        return start(&home, args);
    }
    if verb == "ship" {
        let flags = flags(args, &["scope", "folder", "harness", "objective"])?;
        let mut store = Store::open(&home)?;
        let usage = "usage: privateer ship new NAME [--scope TEXT] [--objective TEXT] [--folder PATH] [--harness ID] | privateer ship retire NAME";
        let name = flags.positional.get(1).context(usage)?;
        match flags.positional.first().map(String::as_str) {
            Some("new") => {
                let mut request = json!({"name": name});
                for key in ["scope", "folder", "harness", "objective"] {
                    if let Some(value) = flags.values.get(key) {
                        request[key] = json!(value);
                    }
                }
                let made = crate::dispatch(&mut store, "ship.create", request)?;
                println!(
                    "Aye. The ship {} is ready at {}, crewed by {}.\n  dispatcher: {}\n  talk to it:  cd {} && privateer",
                    made["ship"].as_str().unwrap_or(""),
                    made["folder"].as_str().unwrap_or(""),
                    made["harness"].as_str().unwrap_or(""),
                    made["dispatcher"].as_str().unwrap_or(""),
                    made["folder"].as_str().unwrap_or("")
                );
            }
            Some("retire") => {
                let done = crate::dispatch(&mut store, "ship.retire", json!({"name": name}))?;
                println!(
                    "Retired {name}: charter revoked, {}. Its folder was kept.",
                    done["dispatcher"].as_str().unwrap_or("")
                );
            }
            _ => bail!(usage),
        }
        return Ok(());
    }
    if verb == "service" {
        let store = Store::open(&home)?;
        let ship = crate::presence::folder_ship(&store)
            .context("this folder's ship is unknown; run `privateer init` here")?;
        drop(store);
        let report = match args.first().map(String::as_str) {
            Some("status") | None => crate::service::status(&home, &ship)?,
            Some("stop") => crate::service::stop(&home, &ship)?,
            Some("start") => format!("Dispatcher: {}", crate::service::ensure(&home, &ship)?),
            Some(other) => bail!("usage: privateer service [status|start|stop], not {other}"),
        };
        println!("{report}");
        return Ok(());
    }
    let mut store = Store::open(&home)?;
    let flags = flags(args, &[])?;
    let p = &flags.positional;
    match verb {
        "status" => {
            let status = crate::dispatch(&mut store, "status", json!({}))?;
            let ships = crate::dispatch(&mut store, "ship.list", json!({}))?;
            println!("Home: {}", home.display());
            for ship in ships["ships"].as_array().into_iter().flatten() {
                let name = ship["name"].as_str().unwrap_or("");
                if ship["location"] == "here" {
                    let bearings = crate::context::bearings(&store, name)?;
                    print!("{}", bearings["text"].as_str().unwrap_or(""));
                    if crate::presence::attended(&store, name)? {
                        println!("The captain's session is aboard {name}.");
                    }
                } else {
                    println!("Ship {name}: behind peer {}", ship["location"]["peer"]);
                }
            }
            let captain = crate::dispatch(&mut store, "mail.inbox", json!({"address": "captain"}))?;
            println!(
                "Captain's unread mail: {}   open requests: {}   undelivered mail: {}",
                captain["messages"].as_array().map_or(0, Vec::len),
                status["open_requests"],
                status["undelivered_mail"]
            );
            let wakes = crate::dispatch(&mut store, "wake.list", json!({"limit": 3}))?;
            for wake in wakes["wakes"].as_array().into_iter().flatten() {
                println!(
                    "Wake {} for {}: {} ({})",
                    wake["id"].as_str().unwrap_or(""),
                    wake["ship"].as_str().unwrap_or(""),
                    wake["status"].as_str().unwrap_or(""),
                    wake["mode"].as_str().unwrap_or("")
                );
            }
        }
        "ships" => {
            let fleet = crate::dispatch(&mut store, "fleet.list", json!({}))?;
            let active: Vec<_> = fleet["ships"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["retired_ms"].is_null())
                .collect();
            println!("Fleet on this machine ({} ships):", active.len());
            for ship in active {
                println!(
                    "  {} at {} (created by {})",
                    ship["name"].as_str().unwrap_or(""),
                    Path::new(ship["home"].as_str().unwrap_or(""))
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    ship["created_by"].as_str().unwrap_or("")
                );
            }
            println!("Known to this ship:");
            let ships = crate::dispatch(&mut store, "ship.list", json!({}))?;
            for ship in ships["ships"].as_array().into_iter().flatten() {
                let place = if ship["location"] == "here" {
                    "here".to_owned()
                } else {
                    format!("peer {}", ship["location"]["peer"].as_str().unwrap_or("?"))
                };
                let mate = ship["first_mate"]["manifest"]
                    .as_str()
                    .and_then(|m| Path::new(m).parent()?.file_name()?.to_str())
                    .map(|h| format!(", first mate: {h}"))
                    .unwrap_or_default();
                println!(
                    "{} ({place}{mate}) {}",
                    ship["name"].as_str().unwrap_or(""),
                    ship["scope"].as_str().unwrap_or("")
                );
            }
        }
        "ask" | "tell" => {
            let to = p
                .first()
                .with_context(|| format!("usage: privateer {verb} SHIP TEXT"))?;
            let body = text(p, 1, &format!("privateer {verb} SHIP TEXT"))?;
            let kind = if verb == "ask" { "request" } else { "notice" };
            let sent = crate::dispatch(
                &mut store,
                "mail.send",
                json!({"from": "captain", "to": to, "kind": kind, "body": body}),
            )?;
            let delivery = sent["delivery"]
                .as_str()
                .map(|d| format!(", delivery {d}"))
                .unwrap_or_default();
            println!(
                "Sent {kind} {} to {to}{delivery}.",
                sent["id"].as_str().unwrap_or("")
            );
            if verb == "ask" {
                println!("Its answer will appear in `privateer inbox`.");
            }
        }
        "inbox" => {
            let all = flags.switches.iter().any(|s| s == "all");
            let inbox = crate::dispatch(
                &mut store,
                "mail.inbox",
                json!({"address": "captain", "all": all}),
            )?;
            let messages = inbox["messages"].as_array().cloned().unwrap_or_default();
            if messages.is_empty() {
                println!("No new mail, captain.");
            }
            for message in &messages {
                print_mail(message);
            }
            // Shown is seen.
            if let Some(last) = messages.last().and_then(|m| m["seq"].as_i64()) {
                crate::dispatch(
                    &mut store,
                    "mail.ack",
                    json!({"address": "captain", "through_seq": last}),
                )?;
                println!("\nAnswer with: privateer reply <id> \"...\"");
            }
        }
        "reply" => {
            let id = p.first().context("usage: privateer reply ID TEXT")?;
            let body = text(p, 1, "privateer reply ID TEXT")?;
            let message = crate::dispatch(&mut store, "mail.show", json!({"id": id}))?;
            let kind = if message["state"] == "open" {
                "reply"
            } else {
                "notice"
            };
            crate::dispatch(
                &mut store,
                "mail.send",
                json!({"from": "captain", "to": message["from"], "kind": kind,
                    "correlation": id, "body": body}),
            )?;
            println!("Sent {kind} to {}.", message["from"].as_str().unwrap_or(""));
        }
        "wakes" => {
            let wakes = crate::dispatch(&mut store, "wake.list", json!({"limit": 10}))?;
            let list = wakes["wakes"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No wakes yet. Is `privateer serve` running?");
            }
            for wake in list {
                let usage = wake["usage"]
                    .as_object()
                    .map(|u| {
                        u.iter()
                            .map(|(k, v)| format!("{k} {v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "{} {} {} {} {}{}",
                    wake["id"].as_str().unwrap_or(""),
                    wake["ship"].as_str().unwrap_or(""),
                    wake["mode"].as_str().unwrap_or(""),
                    wake["status"].as_str().unwrap_or(""),
                    wake["focus"].as_str().unwrap_or(""),
                    if usage.is_empty() {
                        String::new()
                    } else {
                        format!("  [{usage}]")
                    }
                );
                if let Some(error) = wake["error"].as_str() {
                    println!("    {error}");
                }
            }
        }
        "project" => {
            ensure!(
                p.first().map(String::as_str) == Some("add"),
                "usage: privateer project add GIT_URL [NAME]"
            );
            let url = p
                .get(1)
                .context("usage: privateer project add GIT_URL [NAME]")?;
            let name = match p.get(2) {
                Some(name) => name.clone(),
                None => url
                    .trim_end_matches('/')
                    .rsplit(['/', ':'])
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(".git")
                    .to_owned(),
            };
            identifier(&name).context("pass a project NAME")?;
            let folder = home.parent().context("ship folder")?;
            let target = folder.join("projects").join(&name);
            ensure!(!target.exists(), "{} already exists", target.display());
            let status = std::process::Command::new("git")
                .args(["clone", "--", url])
                .arg(&target)
                .status()
                .context("run git clone")?;
            ensure!(status.success(), "git clone failed");
            println!("Cloned {url} into projects/{name}.");
        }
        _ => bail!("unknown command {verb}"),
    }
    Ok(())
}
