//! Commands for people: `privateer init`, `ask`, `inbox` and friends.
//!
//! A ship is a dedicated folder. Its state lives in `<folder>/.privateer`,
//! found from the current directory upward the way git finds `.git`, and the
//! folder is its charter's repository root, with the projects it works on
//! cloned into `projects/`. `init` turns a folder into a ship with sensible
//! defaults and writes the instructions that make any harness launched there
//! its first mate.
use crate::store::{Caller, Store, identifier};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".privateer";
/// Commands that mean the human form when given without `--json`.
pub const VERBS: &[&str] = &[
    "init",
    "status",
    "ships",
    "ask",
    "tell",
    "inbox",
    "reply",
    "dismiss",
    "wakes",
    "project",
    "start",
    "hook",
    "service",
    "ship",
    "door",
    "machine",
    "grant",
    "permissions",
    "routes",
    "watch",
    "upgrade",
    "host",
    "stow",
    "backlog",
    // Fleet-wide, handled before a home is needed.
    "afk",
    "back",
    "away",
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

/// The command a harness runs as.
pub(crate) fn harness_binary(harness: &str) -> Option<&'static str> {
    HARNESSES
        .iter()
        .find(|(_, id)| *id == harness)
        .map(|(binary, _)| *binary)
}

fn on_path(binary: &str) -> bool {
    std::env::var("PATH").is_ok_and(|paths| on_path_in(binary, &paths))
}

/// Whether `binary` is an executable in one of `paths`' directories.
pub(crate) fn on_path_in(binary: &str, paths: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(paths).any(|dir| {
        dir.join(binary).is_file()
            && dir
                .join(binary)
                .metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    })
}

/// Supported harnesses installed on this machine, in the runtime's normal order.
pub fn installed_harnesses() -> Result<Vec<String>> {
    let plugins = plugins_dir()?;
    Ok(HARNESSES
        .iter()
        .filter(|(binary, id)| on_path(binary) && plugins.join(id).join("manifest.json").is_file())
        .map(|(_, id)| (*id).to_owned())
        .collect())
}

/// Release images use only their own immutable bundle. Legacy/source builds
/// retain the explicit override and checkout discovery for development.
pub(crate) fn plugins_dir() -> Result<PathBuf> {
    let executable = crate::releases::running_image()?;
    if let Some(release) = crate::releases::Release::from_executable(&executable) {
        let plugins = release.plugins();
        ensure!(
            plugins.join("native.py").is_file(),
            "release {} is missing its bundled plugins",
            release.commit
        );
        return Ok(plugins);
    }
    if let Some(dir) = std::env::var_os("PRIVATEER_PLUGINS") {
        return Ok(PathBuf::from(dir));
    }
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

pub(crate) struct Flags {
    pub positional: Vec<String>,
    pub values: std::collections::HashMap<String, String>,
    pub switches: Vec<String>,
}

pub(crate) fn flags(args: &[String], with_value: &[&str]) -> Result<Flags> {
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

/// Generated instructions and native hook schemas carry shell command text.
fn shell_token(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.:".contains(&b))
    {
        text.into()
    } else {
        format!("'{}'", text.replace('\'', "'\"'\"'"))
    }
}

/// Write the first mate's AGENTS.md beside `home`, unless the captain has
/// taken the file over; returns whether Privateer owns it.
pub(crate) fn write_instructions(
    home: &Path,
    ship: &str,
    scope: &str,
    liaison: bool,
) -> Result<bool> {
    let agents = home.parent().context("ship folder")?.join("AGENTS.md");
    let ours = fs::read_to_string(&agents).map_or(true, |t| t.starts_with(GENERATED));
    if ours {
        fs::write(&agents, generated_instructions(home, ship, scope, liaison)?)?;
    }
    Ok(ours)
}

fn generated_instructions(home: &Path, ship: &str, scope: &str, liaison: bool) -> Result<String> {
    let cli = format!(
        "{} --home {}",
        shell_token(&crate::executable::installed()?.to_string_lossy()),
        shell_token(&home.to_string_lossy())
    );
    let mut text = instructions(ship, scope, &cli);
    if liaison {
        text.push_str(&liaison_instructions(ship));
    }
    Ok(text)
}

/// Bring a ship's generated AGENTS.md up to this build's instructions, so an
/// upgrade reaches the first mate without `privateer init`. A file the
/// captain took over is left alone. Returns whether the file changed.
pub(crate) fn refresh_instructions(store: &mut Store) -> Result<bool> {
    let Some(ship) = crate::presence::folder_ship(store) else {
        return Ok(false);
    };
    let agents = store
        .home()
        .parent()
        .context("ship folder")?
        .join("AGENTS.md");
    let Ok(current) = fs::read_to_string(&agents) else {
        return Ok(false);
    };
    if !current.starts_with(GENERATED) {
        return Ok(false);
    }
    let record = crate::dispatch(store, "ship.show", json!({"name": ship}))?;
    let scope = record["scope"].as_str().unwrap_or_default();
    let liaison = crate::door::is_liaison(store.home());
    let text = generated_instructions(store.home(), &ship, scope, liaison)?;
    if text == current {
        return Ok(false);
    }
    fs::write(&agents, text)?;
    Ok(true)
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

Use your agent identity for mail and delegation through these JSON commands or
`privateer ship new|retire` and `privateer project add`. Bare `privateer`,
`init`, `start`, `service`, the dashboard and charter edits are captain commands.

## At the start of every session

1. `ship.bearings {{"ship":"{ship}"}}`: open tasks, requests owed and owing, ship knowledge.
2. `mail.inbox {{"address":"{ship}"}}`: new mail. Answer requests, act on notices, then `mail.ack {{"address":"{ship}","through_seq":N}}`.
3. `note.list {{"ship":"{ship}"}}` and `note.get {{"ship":"{ship}","focus":"task:<id>"}}`: the handoff notes you left for each focus.

## Working

- Mail as yourself: `mail.send {{"from":"{ship}","to":"<ship or captain>","kind":"request|notice|reply","body":"...","task_id":"<optional>"}}`; a reply sets `correlation` to the request id. Task ids are this ship's own: name only your tasks in `task_id`, and link another ship's work through its request thread (`correlation`). `ship.list {{}}` shows who handles what; another ship's home is the peer `fleet-<ship>`.
- Anything about a request goes in that request's thread: its answer, a correction, or "no longer needed" all set `correlation` to the request id, never a new request to the same ship about the same thing. A low notice in the thread of a request you have open is not lost: it wakes you and names the follow-up it concerns.
- When you raise a request because of another one, set `because_of` to the id of the request it grows from (`mail.send {{"from":"{ship}","to":"<ship or captain>","kind":"request","because_of":"<parent request id>","body":"..."}}`). One level only: name the oldest request, not another follow-up. Any later mail in the parent thread then wakes you while that follow-up is open, so you can withdraw or update it.
- Write to another ship when it must act or decide. A fact it may want but need not act on, such as a heads-up that your work touches its area, is a notice with `"urgency":"low"`: it waits for that ship's next wake instead of waking it.
- A request you sent that no longer needs an answer, because the work happened another way or the question went stale, is withdrawn with `mail.withdraw {{"id":"<request id>","reason":"..."}}` (optional `actor` must be yourself), so it stops waiting on the captain or another ship. Acknowledging mail is not resolution; linked requests close when their task ends.
- Writing to `captain`: they read it on a phone, between other things. Use `"urgency":"high"` for the captain only when production or data is at risk, there is a security problem, money is being spent unexpectedly, or a deadline is hours away: high urgency reaches them even while they are away, so everything else is normal. While they are away, requests reach them at once and other notices go in a digest; a notice the captain already has is not shown again, and the send reply says so: do not resend it. Start with what happened and what you need from them, in plain words, in one to four short sentences. Name PRs and issues by number; leave out job ids, commit hashes, memory figures and internal task ids unless they ask. Write only when something they care about changes: a result, a decision you need, or a blocker. If the captain must do something with their own hands (enter a password, remove a root-owned file, plug in a device, approve on a phone), send it as a request with `"payload":{{"needs_hands":{{"step":"<the exact thing the captain must do, such as a command to run>"}}}}` and name that step in the body too. Never bury such an ask under a report: a request that reports a result and ends with an ask gets read as settled. Progress details go to the ship that asked, not the captain.
- When the captain must choose between options, send it as `kind` `decision` with `payload.decision`, not as a question in the body: `mail.send {{"from":"{ship}","to":"captain","kind":"decision","body":"<one line of context>","payload":{{"decision":{{"question":"<the choice>","options":[{{"id":"a","label":"<button text>","what_happens":"<the result>"}},{{"id":"b","label":"...","what_happens":"..."}}]}}}}}}`. Give two to eight options, with at most one `"recommended":true`. The captain's answer arrives as a reply with `payload.answer`, naming an `option_id`, `text`, or both.
- A long document for the captain (a plan, an audit, a design, research) goes as a report they read and annotate, not as a wall of text or only a link: put it in `payload.report` on a notice, or on a request when you need an answer, as `{{"title":"<title>","format":"markdown","content":"<the document>"}}` while the whole message fits 64 KiB; past that, store it first with `report.put {{"from":"{ship}","title":"<title>","format":"markdown","content_file":"<path>"}}` and send `"payload":{{"report":{{"report_id":"<the returned report_id>"}}}}`. Keep the body to one or two lines saying what it is and what you need. The captain's comments come back as a reply in that thread with `payload.annotations`: each is a comment anchored to a quote or a block of the report.
- A tool your harness denies becomes one captain decision, which the runtime sends, and its task waits on it: do not mail the captain about the block or ask again (your bearings say "Waiting on captain decision"). If your own session was denied a tool and the runtime could not see it, report it with `permissions.blocked {{"task_id":"<task>","tool":"<Tool>","input":{{...}}}}` and stop. When you pass another ship's message on to the captain, set `correlation` to it, or `about` to the decision it concerns, so it joins that ask instead of arriving twice.
- Builds and test suites that load the machine: `job.start {{"task_id":"<task>","command":[...],"heavy":true,"wait_for_capacity":true}}`. You need not poll it: when it ends you get its outcome. While a change is in progress, run the tests that cover it, not the whole suite at every review round; the pull request's CI runs the full suite on every platform, so run it locally once, if at all, on the commit you open the PR from. A queued job keeps its place in line: cancel it only when the commit it tests is wrong, never to re-queue the same check.
- Pull requests: if your charter allows `publish_pr` and `merge_green`, you own your pull requests end to end: push, fix CI, mark them ready, and merge when they are green. Do not hand publishing or review of your work to another ship unless the captain asks, and do not wait on another ship to publish a fix you already have. With `publish_pr` but not `merge_green`, you still push and fix CI, and leave the merge to the captain. Merging is not deployment: production still needs the captain's approval.
- Delegate with `task.create {{"id":"...","owner":"<ship>","title":"...","charter_id":"{ship}"}}`; hand off with `task.handoff`, which takes effect when the recipient runs `task.accept`.
- History for a focus: `focus.digest {{"ship":"{ship}","focus":"task:<id>"}}`.
- Your attention is the ship's real limit, not a number of tasks. The bearings' Attention line measures your live context: how full it is, how fast it filled in the last hour, compactions and unread crew reports. Check it before you start a new task; while it says high, start nothing new, send pending reviews to errands, and finish work in hand or record what it waits on, until it eases.
- Work you are not starting now waits in your backlog as an open task: `task.create` it, order it with `task.rank {{"id":"<task>","rank":1}}` (1 starts first), and read it with `task.backlog {{"ship":"{ship}"}}`. Open/ranked tasks are drafts until prepared. To entrust automatic pickup to the runtime, write the assignment and call `task.enqueue` with its id (or supply brief). The runtime snapshots that assignment and schedules it without needing new mail. For scheduled work use `task.defer` with a typed wait and its event reference or deadline; prose notes do not schedule resumption. Use wait kind manual for an explicit stop. Direct agent.start remains available for work you start now.
- Never end a turn on a promise. If you say what you will do next, start it in this turn (its crew, its check or the step itself), or record what wakes you for it. Scheduled work requires `task.defer` with a typed condition; setting status blocked or pinning a prose wait does not arrange resumption. For unscheduled work, record its blocker in the task and handoff note. A turn ends the moment you reply without a tool call, and nothing wakes you for work you only announced.
- Before you stop, replace the handoff note for each focus you worked on: `note.put {{"ship":"{ship}","focus":"task:<id>|thread:<id>|captain|ship","body":"..."}}`. Keep it under 2 KB, in plain sentences, and to judgement: what you are doing and why, what you are waiting on, what comes next. Facts are already recorded; leave logs to them, and keep temporary paths and copied state that will go stale out of the note. Pin the few exact facts you must carry, such as a frozen head, a gate job or a PR, with `note.pin {{"ship":"{ship}","focus":"...","key":"head","value":"..."}}` (at most eight per focus; an empty value unpins); each wake shows them. Put lasting preferences and conventions in the `ship` note. Keep notes and assignments in Privateer, never in `/tmp` or other scratch files, which nobody else can read and a reboot loses.
- Repositories live in `projects/`. Add one with `project.add {{"url":"<git url>","name":"<optional>"}}`, or to a ship you created with `"target":"<ship>"`.
- Reuse repository URLs from the task, existing projects or Git remotes before asking the captain to repeat them.
- Tool permissions are launch policy, separate from your charter. Routine `git` and `gh` calls are pre-approved for Claude. If a tool is denied, report the blocker; do not send the captain hand-edit instructions for `.claude` settings. The captain can add a fleet rule with `privateer permissions allow 'Tool(rule)'`, or use `privateer permissions yolo` / Telegram `/yolo on` for every first mate. Agents cannot change this policy themselves. Settings apply on the next launch, including resumed wakes.
- New area of work? Create a ship for it: `ship.create {{"name":"...","scope":"...","objective":"..."}}` (beside this folder; it gets your charter's actions, or the subset you pass as `"actions"`). Change a ship you created with `ship.actions {{"name":"...","actions":[...]}}`, within your own actions, and retire it with `ship.retire {{"name":"..."}}`. `fleet.list {{}}` shows every ship on this machine.
- Scopes route work: check `ship.list {{}}` before creating a ship, and give it a scope that does not overlap a neighbour's. When an area of work moves between ships, change their scopes so mail follows it: `ship.scope {{"name":"...","scope":"..."}}` restates your own scope or that of a ship you created; ask any other ship to restate its own.

## Crew

Crew are the jobs you start (`job.start`, or `agent.start` for another harness) on your tasks. When one finishes, fails, times out or is orphaned, the runtime mails you its outcome from `crew`, under the task's focus: status, exit code, reason, duration and the end of its logs, taken from the job's record. An exit status is not proof: check the result yourself (`job.status`, the logs, the work) before relying on it or reporting it, and record your judgement in the task's note. A crew member that needs a decision sends you a request from `crew`; answer it with `mail.send {{"from":"{ship}","to":"crew","kind":"reply","correlation":"<request id>","body":"..."}}`.

You orchestrate; crew implement. For implementation work, create a child task (`task.create {{"id":"<child>","parent":"<task>","owner":"{ship}","title":"<bounded outcome>"}}`), write its assignment (`note.put {{"ship":"{ship}","focus":"assignment:<child>","body":"..."}}`, up to 16 KB: goal, constraints, how to verify, what to report; the crew read it with their task's note, and your wakes do not load it), and start a crew agent on it: `agent.start {{"task_id":"<child>","cwd":"<this folder>/projects/<repo>"}}`. Crew take the first available of the captain's crew routes shown in your bearings, or the one you name with `"route":"<name>"`; with no routes listed they use your harness and model unless you pass `manifest` and `options`. Then end your wake: the crew's outcome wakes you. Keep planning, decisions and reporting for yourself, and do only small, mechanical fixes directly. Your conversation is the ship's portfolio, and whatever you read stays in it: do reviews, investigations of failed checks and catching up on long threads as errands, `errand.start {{"focus":"task:<id>","brief":"what to look at and what to decide"}}`. An errand is a short headless session on that focus that reads the logs, diffs and code and mails you only its verdict; a follow-up errand on the same focus continues where it left off. Read raw logs or large diffs yourself only for a quick look. Create one child task per independent worker.

Each git-backed crew task gets its own Treehouse workspace, and its jobs run there: the `cwd` you give under `projects/` is mapped into it. Use paths relative to it and never `cd` into `projects/` inside a command, which is the shared clone. Jobs for the same task share its checkout and keep it across process exits. `workspace.status {{"task_id":"<task>"}}` shows the path, branch and lease. After work lands and jobs stop, return it with `workspace.release {{"task_id":"<task>","lease_id":"<recorded lease>"}}`; `archive:true` preserves committed unfinished work first. Uncommitted work blocks release. Use `workspace.list {{}}` to find retained workspaces.

If you are crew yourself (`PRIVATEER_JOB` is set and you were not woken as first mate), report to your first mate with `crew.report {{"kind":"progress","body":"..."}}`, or ask with `crew.report {{"kind":"decision","body":"..."}}` and read the answer with `crew.answer {{"id":"<returned id>","wait_secs":300}}`. To wait on a heavy job you started, park with `crew.park {{"job_id":"<job>","next":"<one line>"}}` and end your turn; do not poll, and do not sleep.

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
        let command = format!("{} hook claude {event}", shell_token(binary));
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
    pub liaison: bool,
    /// First-mate harness options; None means the harness's defaults.
    pub options: Option<Value>,
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
    let existing = fs::read(home.join("config.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    let ship = match setup.ship.clone().or_else(|| {
        existing
            .as_ref()
            .and_then(|c| c["ship"].as_str().map(str::to_owned))
    }) {
        Some(name) => name,
        None => ship_name(&folder)?,
    };
    identifier(&ship)?;
    crate::mail::unreserved(&ship)?;
    let plugins = plugins_dir()?;
    // Running init again keeps the ship's harness unless one is named; the
    // first harness found on PATH is only a default for a new ship.
    let kept_manifest = match (&existing, &setup.harness) {
        (Some(_), None) => {
            crate::dispatch(&mut Store::open(home)?, "ship.show", json!({"name": ship}))
                .ok()
                .and_then(|s| s["first_mate"]["manifest"].as_str().map(str::to_owned))
        }
        _ => None,
    };
    let (harness, manifest) = match kept_manifest {
        Some(manifest) => (
            crate::bundled::harness(&manifest).context("cannot name the ship's harness")?,
            manifest,
        ),
        None => {
            let harness = detect_harness(&plugins, setup.harness.as_ref())?;
            let manifest = plugins.join(&harness).join("manifest.json");
            ensure!(
                manifest.is_file(),
                "no bundled adapter for harness {harness} in {}",
                plugins.display()
            );
            (harness, manifest.display().to_string())
        }
    };
    fs::create_dir_all(folder.join("projects"))?;
    let mut store = Store::open(home)?;
    let binary = crate::executable::installed()?;

    let liaison = setup.liaison || existing.as_ref().is_some_and(|c| c["preset"] == "liaison");
    let mut config = json!({"ship":ship});
    if liaison {
        config["preset"] = json!("liaison");
    }
    crate::dispatch(&mut store, "init", config)?;
    // The shared ledger must exist before a sandboxed first mate is planned.
    crate::jobs::ensure_ledger(&store)?;
    // Created once; later edits to the charter are the captain's to keep.
    // A liaison still holding the original contact-only preset, never edited,
    // gets the liaison's current authority.
    if liaison
        && store.charter(&ship).is_ok_and(|c| {
            c["revision"] == 1 && c["actions"] == json!(["inspect", "spawn", "message"])
        })
    {
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id": ship, "actions": crate::core::DEFAULT_ACTIONS}),
        )?;
    }
    if store.charter(&ship).is_err() {
        let mut charter = json!({"id": ship,
            "objective": setup.objective.clone().unwrap_or_else(|| if liaison {"Act for the captain: onboard them, set up and staff ships, relay mail and delegate work to peers".into()} else {format!("Work as the ship {ship}")}),
            "repo": folder, "job_timeout_secs": 3600});
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
    // Running init again keeps the captain's model and provider for the same
    // harness; a different harness starts from its own defaults.
    let kept = previous
        .as_ref()
        .map(|ship| &ship["first_mate"])
        .filter(|mate| {
            mate["manifest"]
                .as_str()
                .and_then(crate::bundled::harness)
                .as_deref()
                == Some(harness.as_str())
        })
        .map(|mate| mate["options"].clone())
        .filter(Value::is_object);
    let options = setup
        .options
        .clone()
        .or(kept)
        .unwrap_or_else(|| match harness.as_str() {
            "codex" => json!({"sandbox": "workspace-write"}),
            "claude" => json!({"permission_mode": "acceptEdits"}),
            _ => json!({}),
        });
    let mut first_mate = json!({"manifest": manifest, "duty_task": duty, "options": options});
    // Re-running init must not silently turn a bounded ship into a legacy
    // ambient-resource launch. A different harness needs an explicit profile
    // update by the captain, not an implicit downgrade.
    if let Some(mate) = previous
        .as_ref()
        .map(|s| &s["first_mate"])
        .filter(|mate| mate.get("tool_bundles").is_some())
    {
        ensure!(
            mate["manifest"]
                .as_str()
                .and_then(crate::bundled::harness)
                .as_deref()
                == Some(harness.as_str()),
            "explicitly reconfigure tool_bundles before changing harness"
        );
        first_mate["tool_bundles"] = mate["tool_bundles"].clone();
    }
    // A hosted first mate stays hosted.
    if let Some(host) = previous
        .as_ref()
        .map(|s| &s["first_mate"]["host"])
        .filter(|h| !h.is_null())
    {
        first_mate["host"] = host.clone();
    }
    crate::dispatch(
        &mut store,
        "ship.put",
        json!({"name": ship, "scope": scope, "accepts": accepts, "first_mate": first_mate}),
    )?;

    let ours = write_instructions(store.home(), &ship, &scope, liaison)?;
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
        // Linked machines learn the ship now, not at their next refresh.
        crate::machines::announce(Some(&store));
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
            liaison: flags.switches.iter().any(|s| s == "liaison"),
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

fn liaison_instructions(ship: &str) -> String {
    format!(
        r#"
## Liaison: the captain's contact

You act for the captain, who reaches you from the dashboard, a terminal or a
front door such as their phone. Do on their behalf what they ask: create and
retire ships, give them working authority, add repositories and route work.
Do not send the captain to the command line for something you can do.
You are the captain's one voice: when other ships report, tell the captain the
outcome in your own plain words rather than forwarding their reports.

Your charter holds a ship's ordinary actions; ships you create inherit them, or
the subset you choose. Production deployment stays with the captain, bound to
an exact artifact and environment. Keep turns short and stay responsive: route
lasting work to the ship that owns it, so each outcome keeps one owner. A
contact role grants no command over ships you did not create.
You delegate and relay: you do not publish, review or merge other ships' code,
and you do not push to their pull requests. The ship that owns a task owns its
pull request through to merge, unless the captain asks otherwise.
When the captain moves an area of work from one ship to another, change the
scopes so routing follows the decision: `ship.scope` for ships you created, and
a request to any other ship to restate its own. A note keeps your reasoning;
scopes are what every ship reads to find an owner.

## What waits on the captain

`captain.waiting {{}}` lists every open request and decision addressed to the
captain across the fleet, the raw mail list `/waiting` shows (distinct from the
dashboard's bounded typed Needs). Each entry carries `asked_ms` when it was sent
and `age_ms` how long ago that was, against the list's `now_ms`.
Report an outstanding item as "X asked at T, N min ago": the age is what tells
the captain how fresh it is. An open request is a frozen statement of what was
true when it was written, never a current fact: before you restate any claim of
runtime state in it (a machine stowed, a job running, something installed, a
ship awake), read that state back now — `stow.status`, `job.list`,
`ship.show`, the owning ship's latest mail — and tell the captain what it says
today, or that the claim is stale. If what the runtime says disagrees with the
request, say so instead of repeating the request body.
Relay the `hands_on` asks first and on their own, each with its exact `step` beside
its age, before any other item: they are the ones only the captain can clear. Take
an ask as hands-on only when it carries `step`, never because its text sounds like one.
When the captain asks what is outstanding, read it, ask each owning ship to
withdraw what is stale (`mail.withdraw`), then go through the real decisions
one at a time in plain words. When the captain decides one in conversation
with you, record it on the request: `captain.relay {{"request":"<request id>",
"source":"<id of the captain's message to you>","body":"<the answer>"}}`. The
reply keeps the captain's own words and says you relayed it. Relay only what
the captain actually said; production deployment approvals are never relayed.
The captain can close an item without answering it with `/dismiss ID [reason]`
(`captain.dismiss`, theirs alone); you cannot dismiss for them.

## Onboarding the captain

1. Ask what the captain works on, their projects, priorities and preferences.
2. Inspect `fleet.list` and propose useful ships with scopes and actions. Ask
   which proposals they want, then create those ships with `ship.create`.
3. Add each ship's repositories with `project.add` and its `target`. A ship you
   created earlier with too few actions gets more with `ship.actions`; never
   claim a ship holds actions its charter lacks.
4. Explain the dashboard's inbox, direct messages and Board action, and the
   direct `privateer start` path. The captain can talk to any ship directly.
5. Keep the captain's lasting preferences in {ship}'s `ship` note, and the
   onboarding progress in its `captain` note. Resume from those records.

The captain may use a different ship first or skip this checklist. Do not claim
onboarding happened until the conversation and runtime records support it.
"#
    )
}

fn print_stow(action: &str, out: &Value) -> Result<()> {
    let ships = out["ships"].as_array().cloned().unwrap_or_default();
    let name = |s: &Value| s["ship"].as_str().unwrap_or("").to_owned();
    // A linked machine's stow says which machine it describes; a plain one
    // says this machine, because the hold is this machine's alone.
    let machine = out["machine"].as_str();
    let on = machine
        .map(|m| format!("the fleet on {m}"))
        .unwrap_or_else(|| {
            format!(
                "this machine ({})",
                out["this_machine"].as_str().unwrap_or("?")
            )
        });
    let flag = machine
        .map(|m| format!(" --machine {m}"))
        .unwrap_or_default();
    // What each linked machine this one reaches says about its own hold. With
    // no machine named these are this machine's own answers; a named machine's
    // answer carries none, so one surface cannot show the other's fleet.
    let machines: Vec<Value> = out["machines"].as_array().cloned().unwrap_or_default();
    let print_machines = |hint: &str| {
        for one in &machines {
            let name = one["machine"].as_str().unwrap_or("");
            match one["held"] {
                Value::Bool(true) => println!("  {name}: still stowed"),
                Value::Bool(false) => println!("  {name}: running again"),
                _ => match one["error"].as_str() {
                    Some(error) => println!("  {name}: could not be reached ({error})"),
                    None => println!("  {name}: reaches this machine; its hold is unknown here"),
                },
            }
        }
        if machines.iter().any(|m| m["held"] == Value::Bool(true)) {
            println!("{hint}");
        }
    };
    // A machine asked to lift its hold and not heard from again is left
    // unknown: `resume --all` did not finish what it was asked to do. A
    // machine that only reaches this one is never asked, so it cannot count.
    let unknown_after_all: Vec<&str> = if out["all"] == true {
        machines
            .iter()
            .filter(|m| m["asked"] == true && m["held"] != Value::Bool(false))
            .filter_map(|m| m["machine"].as_str())
            .collect()
    } else {
        vec![]
    };
    let named_or_this = |s: &str| {
        let mut chars = s.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    };
    match action {
        "start" => {
            println!(
                "Stowing {on} ({}): new jobs are held until `privateer stow resume{flag}`.",
                out["stow"].as_str().unwrap_or("")
            );
            for ship in &ships {
                match ship["error"].as_str() {
                    None => println!("  asked {} to stow", name(ship)),
                    Some(error) => println!("  could not ask {}: {error}", name(ship)),
                }
            }
            println!("Follow it with `privateer stow status{flag}`.");
        }
        "resume" if out["stow"].is_null() => {
            println!("{} was not stowed.", named_or_this(&on));
            print_machines(
                "`privateer stow resume --all` lifts a linked machine's hold first, so no notice has to leave one still stowed.",
            );
        }
        "resume" => {
            println!(
                "Resumed {on} ({}): new jobs start again.",
                out["stow"].as_str().unwrap_or("")
            );
            if let Some(ended) = out["requests_ended"].as_u64().filter(|ended| *ended > 0) {
                println!("  ended {ended} request(s) that waited for this stow");
            }
            for home in out["homes_skipped"].as_array().into_iter().flatten() {
                println!(
                    "  could not end requests in {} (its home could not be opened)",
                    home.as_str().unwrap_or("?")
                );
            }
            print_machines(
                "`privateer stow resume --all` lifts a linked machine's hold first, so no notice has to leave one still stowed.",
            );
        }
        _ if out["stow"].is_null() => {
            println!("{} has never been stowed.", named_or_this(&on));
            print_machines("Resume a linked machine with `privateer stow resume --machine NAME`.");
        }
        _ => {
            let state = if out["held"] == true {
                "held"
            } else {
                "resumed"
            };
            println!(
                "Stow {on} ({state}) ({}):",
                out["stow"].as_str().unwrap_or("")
            );
            for ship in &ships {
                if let Some(error) = ship["error"].as_str() {
                    println!("  {}: {error}", name(ship));
                    continue;
                }
                let replied = if ship["replied"] == true {
                    "stowed"
                } else {
                    "not yet"
                };
                let jobs: Vec<String> = ship["jobs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|j| {
                        format!(
                            "{} {}",
                            j["task_id"].as_str().unwrap_or(""),
                            j["status"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                let jobs = if jobs.is_empty() {
                    "no jobs".to_owned()
                } else {
                    jobs.join(", ")
                };
                println!("  {}: {replied}; {jobs}", name(ship));
                if let Some(reply) = ship["reply"].as_str() {
                    for line in reply.lines().take(6) {
                        println!("      {line}");
                    }
                }
            }
            if out["held"] == true {
                println!(
                    "{}",
                    if out["ready"] == true {
                        "Ready: every ship has stowed and no job is running."
                    } else {
                        "Not ready yet: wait for every ship to reply and running jobs to finish."
                    }
                );
            }
            if machine.is_none() {
                print_machines(
                    "A linked machine still stowed keeps its own ships waiting: `privateer stow resume --all` lifts every machine this one reaches, then this one.",
                );
            }
        }
    };
    // An unknown hold after `--all` is a machine the operator asked to lift and
    // never heard from again, so the run says it did not finish rather than
    // reporting a fleet resumed.
    if !unknown_after_all.is_empty() {
        bail!(
            "`privateer stow resume --all` could not lift every machine it reaches; \
             hold unknown here: {}",
            unknown_after_all.join(", ")
        );
    }
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
    // `--hosted`: the dispatcher launched this session in the terminal host
    // and holds its lease; the session adopts it instead of claiming one.
    let hosted = ours.iter().any(|a| a == "--hosted");
    let ours: Vec<&String> = ours.iter().filter(|a| *a != "--hosted").collect();
    let mut store = Store::open(home)?;
    let canonical_home = store.home().to_path_buf();
    let home = canonical_home.as_path();
    let ship = crate::presence::folder_ship(&store)
        .context("this folder's ship is unknown; run `privateer init` here")?;
    let mut profile = crate::dispatch(&mut store, "ship.show", json!({"name": ship}))?;
    let folder = home.parent().context("ship folder")?;
    let holder = if hosted {
        std::env::var("PRIVATEER_HOSTED_HOLDER")
            .ok()
            .filter(|h| h.starts_with(crate::presence::HOSTED))
            .context("start --hosted is launched by the dispatcher")?
    } else {
        format!("start:{}:{}", std::process::id(), uuid::Uuid::new_v4())
    };
    if hosted
        && let Err(error) = crate::host::validate_start(&mut store, &ship, &profile["first_mate"])
    {
        let _ = crate::host::failed(&store, &ship, &holder, &format!("{error:#}"));
        crate::presence::leave(&store, &ship, &holder)?;
        return Err(error);
    }
    let mut hosted_lease = hosted.then(|| crate::host::Lease {
        store: &store,
        ship: &ship,
        holder: holder.clone(),
    });
    // This pane's release, so bearings and upgrade status can report drift.
    if hosted {
        let _ = crate::host::upgrade::record(&store, &ship);
    }
    let mut launch = launch_plan(&store, &profile, ours.first().copied(), passthrough, hosted)?;
    let control = crate::executable::installed()?;
    let plugins = fs::canonicalize(plugins_dir()?)?;
    prepare_harness(&launch.harness, &plugins, folder, &control)?;
    // Bounded live starts are fresh: old history can retain facts from
    // resources no longer selected, and --continue cannot name the exact
    // session a plan digest was recorded for. Omitted selection keeps resumes.
    // Rechecked at every launch: a session that slept has a conversation to
    // continue even if its first launch had none.
    let continuation_for = |launch: &Launch| -> Result<Option<crate::host::Conversation>> {
        let applies = hosted
            && launch.harness == "claude"
            && launch.bundle_plan.is_none()
            && !["--continue", "-c", "--resume", "-r", "--session-id"]
                .iter()
                .any(|flag| passes(passthrough, flag));
        if !applies {
            return Ok(None);
        }
        Ok(Some(crate::host::claude_conversation(
            &folder.canonicalize()?,
        )))
    };
    let build = |launch: &Launch,
                 profile: &Value,
                 holder: &str,
                 continuation: Option<crate::host::Conversation>|
     -> Result<std::process::Command> {
        // Read at every launch, so fleet YOLO set while a pane sleeps applies
        // at its wake.
        let policy = serde_json::to_string(&crate::permissions::load()?)?;
        let mut command = std::process::Command::new("python3");
        command.arg(plugins.join("native_launch.py"));
        if let Some(plan) = &launch.bundle_plan {
            command.arg("--tool-bundles").arg(plan.to_string());
        }
        command
            .arg("--attended-policy")
            .arg(&policy)
            .arg(&launch.harness)
            .arg(&launch.binary);
        if matches!(launch.harness.as_str(), "pi" | "pi-signed" | "omp") {
            command.arg("-e").arg(plugins.join("attended/mail.ts"));
            command.arg("-e").arg(plugins.join("attended/captain.ts"));
            if matches!(launch.harness.as_str(), "pi" | "pi-signed") {
                command.arg("-e").arg(plugins.join("attended/calm.ts"));
            }
        } else if launch.harness == "claude" {
            command
                .arg("--plugin-dir")
                .arg(plugins.join("attended-claude"));
        } else if launch.harness == "opencode-v2" {
            command.arg("--standalone");
        }
        command.args(&launch.route);
        match continuation {
            Some(conversation) if conversation.continues() => {
                command.arg("--continue");
            }
            // Transcripts exist yet none can be resumed: say so on the pane
            // rather than let a fresh session pass for the old one.
            Some(
                conversation @ (crate::host::Conversation::HeadlessOnly
                | crate::host::Conversation::BoundReached
                | crate::host::Conversation::NoEntrypoint),
            ) => eprintln!(
                "warning: this ship's Claude folder has transcripts ({}), none to continue; starting a fresh session",
                conversation.label()
            ),
            _ => {}
        }
        command
            .args(passthrough)
            .current_dir(folder)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", &ship)
            .env("PRIVATEER_HOME", home)
            .env("PRIVATEER_CONTROL_CLI", &control)
            .env("PRIVATEER_ATTENDED_HARNESS", &launch.harness)
            .env("PRIVATEER_PRESENCE_HOLDER", holder);
        for (name, key) in [
            ("PRIVATEER_QUIET_COMPACT_SECS", "quiet_compact_secs"),
            ("PRIVATEER_QUIET_COMPACT_PERCENT", "quiet_compact_percent"),
        ] {
            if let Some(value) = profile["first_mate"][key].as_u64() {
                command.env(name, value.to_string());
            }
        }
        if hosted {
            command.env("PRIVATEER_HOSTED", "1");
        }
        Ok(command)
    };
    // Decide before claiming presence, so an error leaves nothing to release.
    let mut continuation = continuation_for(&launch)?;
    // The child can invoke a startup hook immediately after spawn.
    if hosted {
        crate::presence::adopt(&store, &ship, &holder, 90)?;
    } else {
        crate::presence::claim(&store, &ship, &holder, 90, Some(&launch.harness))?;
    }
    // Ctrl-C belongs to the harness; this process must live to release
    // presence afterwards.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    // Ships recorded before the setting existed sleep by default too.
    let sleep_after_for = |profile: &Value| {
        if hosted {
            profile["first_mate"]["sleep_after_secs"]
                .as_u64()
                .unwrap_or(crate::host::sleep::DEFAULT_AFTER_SECS)
        } else {
            0
        }
    };
    let mut sleep_after = sleep_after_for(&profile);
    let terminal = crate::host::sleep::Terminal::save();
    let mut holder = holder;
    // A pane an upgrade relaunched while asleep, or catching up while idle,
    // starts asleep: the upgrade does not wake the ship.
    let mut start_asleep =
        hosted && std::env::var("PRIVATEER_HOSTED_START_ASLEEP").as_deref() == Ok("1");
    let status = loop {
        let ended = if std::mem::take(&mut start_asleep) {
            crate::host::started(&store, &ship, &holder)?;
            crate::host::sleep::Ended::Asleep(crate::host::sleep::asleep_at_start(
                &store, &ship, &holder,
            )?)
        } else {
            let mut child =
                match build(&launch, &profile, &holder, continuation).and_then(|mut command| {
                    command.spawn().with_context(|| {
                        format!("cannot start {}; is it installed?", launch.binary)
                    })
                }) {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = crate::presence::leave(&store, &ship, &holder);
                        return Err(error);
                    }
                };
            if hosted {
                // A failure to record readiness leaves the conservative reserved retry
                // in place; never abandon an already spawned native process over it.
                let _ = crate::host::started(&store, &ship, &holder);
                let _ = crate::host::upgrade::harness(&store, &ship, child.id());
            }
            crate::host::sleep::supervise(&store, &ship, &holder, &mut child, sleep_after)?
        };
        match ended {
            crate::host::sleep::Ended::Exited(status) => break status,
            crate::host::sleep::Ended::Relaunch(reason, fenced, through) => {
                // The harness has exited; leave its release behind for a fresh pane.
                // Only a catch-up leaves that pane asleep, and only a ship that sleeps
                // at all; a requested relaunch wakes it.
                terminal.restore();
                let start = crate::host::upgrade::start_point(reason, sleep_after, through);
                crate::host::upgrade::retire(&store, &ship, &fenced, reason, start);
                std::process::exit(0);
            }
            crate::host::sleep::Ended::Asleep(asleep) => {
                holder.clone_from(&asleep.holder);
                if let Some(lease) = hosted_lease.as_mut() {
                    lease.holder.clone_from(&holder);
                }
                let Some(reason) = crate::host::sleep::rest(&store, &ship, &asleep, &terminal)?
                else {
                    std::process::exit(0);
                };
                if matches!(reason, "relaunch" | "upgrade") {
                    // Asleep before, asleep after: a sleeping pane stays asleep.
                    crate::host::upgrade::retire(
                        &store,
                        &ship,
                        &holder,
                        reason,
                        Some(asleep.through_seq),
                    );
                    std::process::exit(0);
                }
                // What admitted the first launch may have been revoked since.
                let mut check = Store::open(home)?;
                profile = crate::dispatch(&mut check, "ship.show", json!({"name": ship}))?;
                if let Err(error) =
                    crate::host::validate_start(&mut check, &ship, &profile["first_mate"])
                {
                    let _ = crate::host::failed(&store, &ship, &holder, &format!("{error:#}"));
                    return Err(error);
                }
                // The wake is a fresh process: its harness arguments come from the
                // profile read now, so a changed permission mode, model, effort,
                // route or harness applies here rather than at the last launch.
                launch =
                    match launch_plan(&check, &profile, ours.first().copied(), passthrough, hosted)
                        .and_then(|next| -> Result<Launch> {
                            prepare_harness(&next.harness, &plugins, folder, &control)?;
                            Ok(next)
                        }) {
                        Ok(next) => next,
                        Err(error) => {
                            let _ =
                                crate::host::failed(&store, &ship, &holder, &format!("{error:#}"));
                            return Err(error);
                        }
                    };
                continuation = continuation_for(&launch)?;
                sleep_after = sleep_after_for(&profile);
                // Only a relaunch that is admitted again is a wake.
                crate::host::sleep::woke(&store, &ship, reason, continuation);
            }
        }
    };
    if hosted && !status.success() {
        crate::host::failed(
            &store,
            &ship,
            &holder,
            "hosted native process exited unsuccessfully",
        )?;
    }
    crate::presence::leave(&store, &ship, &holder)?;
    std::process::exit(status.code().unwrap_or(1));
}

/// What one launch of a first mate runs, derived from its profile. Every
/// wake derives it again from the profile read then.
struct Launch {
    harness: String,
    binary: String,
    route: Vec<String>,
    bundle_plan: Option<Value>,
}

/// Whether the session's own arguments choose `flag`, so the profile yields.
fn passes(passthrough: &[String], flag: &str) -> bool {
    passthrough
        .iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}

/// The harness, model, provider, permission mode, effort and Chrome setting a
/// first mate launches with, from its profile now.
fn launch_plan(
    store: &Store,
    profile: &Value,
    named: Option<&String>,
    passthrough: &[String],
    hosted: bool,
) -> Result<Launch> {
    let harness = match named {
        Some(name) => name.clone(),
        None => profile["first_mate"]["manifest"]
            .as_str()
            .and_then(crate::bundled::harness)
            .context("no first-mate harness configured; pass one: privateer start claude")?,
    };
    // The captain's model and provider for this ship's harness, unless the
    // session passes its own.
    let options = &profile["first_mate"]["options"];
    let own_harness = profile["first_mate"]["manifest"]
        .as_str()
        .and_then(crate::bundled::harness)
        .as_deref()
        == Some(harness.as_str());
    let mut route: Vec<String> = vec![];
    if own_harness && !passes(passthrough, "--model") && !passes(passthrough, "--provider") {
        if matches!(harness.as_str(), "pi" | "pi-signed" | "omp")
            && let Some(provider) = options["provider"].as_str()
        {
            route.extend(["--provider".to_owned(), provider.to_owned()]);
        }
        if matches!(
            harness.as_str(),
            "pi" | "pi-signed" | "omp" | "claude" | "codex" | "gemini" | "cursor"
        ) && let Some(model) = options["model"].as_str()
        {
            route.extend(["--model".to_owned(), model.to_owned()]);
        }
    }
    // A Claude ship's permission mode and effort, such as `auto` for a first
    // mate nobody is watching. Fleet YOLO still replaces the mode.
    if own_harness && harness == "claude" {
        for (key, flag) in [
            ("permission_mode", "--permission-mode"),
            ("effort", "--effort"),
        ] {
            if let Some(value) = options[key].as_str()
                && !passes(passthrough, flag)
            {
                route.extend([flag.to_owned(), value.to_owned()]);
            }
        }
    }
    // Claude in Chrome drives the captain's own signed-in browser, and Claude
    // may enable it for every session by default. A ship gets it only when its
    // options say so; other hosted sessions are kept out of the browser, and
    // the captain's own session keeps the captain's defaults.
    if harness == "claude"
        && !passes(passthrough, "--chrome")
        && !passes(passthrough, "--no-chrome")
    {
        if own_harness && options["chrome"] == true {
            route.push("--chrome".to_owned());
        } else if hosted {
            route.push("--no-chrome".to_owned());
        }
    }
    let binary = if harness == "opencode-v2" {
        "opencode".to_owned()
    } else {
        HARNESSES
            .iter()
            .find(|(_, id)| *id == harness)
            .map_or(harness.as_str(), |(binary, _)| binary)
            .to_owned()
    };
    // Bounded live starts are fresh: old history can retain facts from
    // resources no longer selected, and --continue cannot name the exact
    // session a plan digest was recorded for. Omitted selection keeps resumes.
    let bundle_plan = profile["first_mate"]
        .get("tool_bundles")
        .map(|selected| -> Result<Value> {
            ensure!(
                matches!(harness.as_str(), "pi" | "claude") && own_harness,
                "bounded tool bundles require the configured Pi or Claude harness"
            );
            crate::tool_bundles::resolve(
                store,
                selected,
                "first_mate",
                &harness,
                crate::plugins::Wait::Launch,
            )
        })
        .transpose()?;
    if hosted
        && bundle_plan.is_none()
        && matches!(harness.as_str(), "pi" | "pi-signed")
        && !passes(passthrough, "--session")
    {
        route.push("--continue".to_owned());
    }
    Ok(Launch {
        harness,
        binary,
        route,
        bundle_plan,
    })
}

/// What a harness needs in the ship folder and on disk before it runs: the
/// mail adapters it loads, and the settings it reads from the folder.
fn prepare_harness(harness: &str, plugins: &Path, folder: &Path, control: &Path) -> Result<()> {
    if matches!(harness, "pi" | "pi-signed" | "omp") {
        for (file, what) in [
            ("attended/mail.ts", "attended mail adapter"),
            // The captain's fleet commands, such as /stow, in the live session.
            ("attended/captain.ts", "Pi captain commands"),
        ] {
            ensure!(
                plugins.join(file).is_file(),
                "{what} missing from {}",
                plugins.display()
            );
        }
        if matches!(harness, "pi" | "pi-signed") {
            ensure!(
                plugins.join("attended/calm.ts").is_file(),
                "Pi calm extension missing"
            );
        }
    } else if harness == "claude" {
        ensure!(
            plugins.join("attended-claude/hooks/hooks.json").is_file(),
            "Claude attended mail plugin missing"
        );
    } else if harness == "opencode-v2" {
        crate::opencode_mail::install(folder, plugins)?;
    } else if harness == "cursor" {
        cursor_settings(folder, control)?;
    }
    Ok(())
}

fn dashboard_binary() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let image = crate::releases::running_image().ok()?;
    if let Some(release) = crate::releases::Release::from_executable(&image) {
        let dashboard = release.entrypoint("privateer-dashboard").ok()?;
        return (dashboard.is_file()
            && dashboard
                .metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0))
        .then_some(dashboard);
    }
    let beside = image.with_file_name("privateer-dashboard");
    let candidates =
        std::iter::once(beside).chain(std::env::var_os("PATH").into_iter().flat_map(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("privateer-dashboard"))
                .collect::<Vec<_>>()
        }));
    candidates.into_iter().find(|p| {
        p.is_file()
            && p.metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    })
}
/// Automatic host work never executes a PATH wrapper. Releases use their
/// immutable bundle; source/legacy installations must have an executable sibling.
pub(crate) fn hosted_dashboard_binary() -> Option<PathBuf> {
    let selected = dashboard_binary()?;
    let image = crate::releases::running_image().ok()?;
    if crate::releases::Release::from_executable(&image).is_some()
        || selected == image.with_file_name("privateer-dashboard")
    {
        // Herdr launch quotes an absolute path, with the same cwd as the host.
        return selected.canonicalize().ok();
    }
    None
}

fn open_dashboard(binary: &Path, home: Option<&Path>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // Only the bridge: boarding a first mate would attend its ship and hold
    // its mail, so that stays the captain's explicit choice (l or b).
    let mut command = std::process::Command::new(binary);
    if let Some(home) = home {
        command.arg("--home").arg(home);
    }
    Err(command.exec()).context("open the Privateer dashboard")
}

/// Bare `privateer` opens the installed dashboard. If it is unavailable,
/// ensure this ship's dispatcher and talk to its first mate, offering to create
/// the ship first when needed; without a terminal or `--yes`, print help.
pub fn board(home: Option<PathBuf>, yes: bool, caller: Caller) -> Result<()> {
    caller.require_operator("bare privateer")?;
    use std::io::{IsTerminal, Write};
    if crate::host::bridge::focus(home.as_deref()).unwrap_or(false) {
        return Ok(());
    }
    let dashboard = dashboard_binary();
    if home.is_none() {
        if let Some(binary) = &dashboard {
            return open_dashboard(binary, None);
        }
    }
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
    if let Some(binary) = dashboard {
        return open_dashboard(&binary, Some(&home));
    }
    start(&home, &[])
}

/// Run a human command. `home` is the discovered or given ship home.
pub fn run(home: Option<PathBuf>, verb: &str, args: &[String], caller: Caller) -> Result<()> {
    if matches!(verb, "init" | "start" | "service" | "watch") {
        caller.require_operator(verb)?;
    }
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
    let mut store = Store::open_as(&home, caller)?;
    if verb == "ship" {
        let flags = flags(
            args,
            &["scope", "folder", "harness", "objective", "machine", "to"],
        )?;
        let usage = "usage: privateer ship new NAME [--scope TEXT] [--objective TEXT] [--folder PATH] [--harness ID] [--machine NAME] | privateer ship retire NAME | privateer ship move NAME --to MACHINE|--here [--folder PATH]";
        let name = flags.positional.get(1).context(usage)?;
        match flags.positional.first().map(String::as_str) {
            Some("new") => {
                let mut request = json!({"name": name});
                for key in ["scope", "folder", "harness", "objective", "machine"] {
                    if let Some(value) = flags.values.get(key) {
                        request[key] = json!(value);
                    }
                }
                let made = crate::dispatch(&mut store, "ship.create", request)?;
                if let Some(machine) = made["machine"].as_str() {
                    println!(
                        "Aye. The ship {} is ready on {machine} at {}, crewed by {}.\n  dispatcher: {}",
                        made["ship"].as_str().unwrap_or(""),
                        made["folder"].as_str().unwrap_or(""),
                        made["harness"].as_str().unwrap_or(""),
                        made["dispatcher"].as_str().unwrap_or("")
                    );
                    return Ok(());
                }
                println!(
                    "Aye. The ship {} is ready at {}, crewed by {}.\n  dispatcher: {}\n  talk to it:  cd {} && privateer",
                    made["ship"].as_str().unwrap_or(""),
                    made["folder"].as_str().unwrap_or(""),
                    made["harness"].as_str().unwrap_or(""),
                    made["dispatcher"].as_str().unwrap_or(""),
                    made["folder"].as_str().unwrap_or("")
                );
            }
            Some("move") => {
                let to = match flags.values.get("to") {
                    Some(machine) => machine.clone(),
                    None if flags.switches.iter().any(|s| s == "here") => "here".into(),
                    None => bail!(usage),
                };
                let mut request = json!({"name": name, "to": to});
                if let Some(folder) = flags.values.get("folder") {
                    request["folder"] = json!(folder);
                }
                let moved = crate::dispatch(&mut store, "ship.move", request)?;
                println!(
                    "Aye. {name} moved from {} to {}, now at {}.\n  dispatcher: {}",
                    moved["from"].as_str().unwrap_or(""),
                    moved["to"].as_str().unwrap_or(""),
                    moved["folder"].as_str().unwrap_or(""),
                    moved["dispatcher"].as_str().unwrap_or("")
                );
                for note in moved["notes"].as_array().into_iter().flatten() {
                    println!("  note: {}", note.as_str().unwrap_or(""));
                }
                for directory in moved["directory"].as_array().into_iter().flatten() {
                    if let Some(error) = directory["skipped"].as_str() {
                        println!(
                            "  note: directory not exchanged with {}: {error}",
                            directory["machine"].as_str().unwrap_or("")
                        );
                    }
                }
                println!(
                    "  its old folder is kept at {}",
                    moved["folder_kept"].as_str().unwrap_or("")
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
    if verb == "project" {
        let flags = flags(args, &["ship"])?;
        let usage = "usage: privateer project add GIT_URL [NAME] [--ship SHIP]";
        ensure!(
            flags.positional.first().map(String::as_str) == Some("add"),
            usage
        );
        let url = flags.positional.get(1).context(usage)?;
        let mut request = json!({"url": url});
        if let Some(name) = flags.positional.get(2) {
            request["name"] = json!(name);
        }
        if let Some(ship) = flags.values.get("ship") {
            request["target"] = json!(ship);
        }
        let added = crate::dispatch(&mut store, "project.add", request)?;
        println!(
            "Cloned {} into {}'s projects/{}.",
            added["url"].as_str().unwrap_or(""),
            added["ship"].as_str().unwrap_or(""),
            added["project"].as_str().unwrap_or("")
        );
        return Ok(());
    }
    if verb == "watch" {
        if let [flag, job] = args
            && flag == "--job"
        {
            return crate::watch::crew(&mut store, job);
        }
        let ship = match args.first() {
            Some(ship) => ship.clone(),
            None => crate::presence::folder_ship(&store)
                .context("usage: privateer watch [SHIP], from a ship folder")?,
        };
        return crate::watch::run(&mut store, &ship);
    }
    if verb == "service" {
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
    let address = match store.caller() {
        Caller::Operator => "captain".to_owned(),
        Caller::Agent { actor } => actor.clone(),
    };
    let flags = flags(args, &["machine"])?;
    let p = &flags.positional;
    match verb {
        "status" => {
            let status = crate::dispatch(&mut store, "status", json!({}))?;
            let ships = crate::dispatch(&mut store, "ship.list", json!({}))?;
            println!("Home: {}", home.display());
            for ship in ships["ships"].as_array().into_iter().flatten() {
                let name = ship["name"].as_str().unwrap_or("");
                if matches!(store.caller(), Caller::Agent { .. }) && name != address {
                    continue;
                }
                if ship["location"] == "here" {
                    let bearings = crate::context::bearings(&store, name)?;
                    print!("{}", bearings["text"].as_str().unwrap_or(""));
                    if crate::presence::hosted(&store, name)? {
                        println!("{name}'s first mate is live in Herdr.");
                    } else if crate::presence::attended(&store, name)? {
                        println!("The captain's session is aboard {name}.");
                    }
                } else {
                    println!("Ship {name}: behind peer {}", ship["location"]["peer"]);
                }
            }
            let inbox = crate::dispatch(&mut store, "mail.inbox", json!({"address": address}))?;
            println!(
                "{}'s unread mail: {}   open requests: {}   undelivered mail: {}",
                if matches!(store.caller(), Caller::Operator) {
                    "Captain"
                } else {
                    &address
                },
                inbox["messages"].as_array().map_or(0, Vec::len),
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
                    .and_then(crate::bundled::harness)
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
                json!({"from": address, "to": to, "kind": kind, "body": body}),
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
                json!({"address": address, "all": all}),
            )?;
            let messages = inbox["messages"].as_array().cloned().unwrap_or_default();
            if messages.is_empty() {
                println!("No new mail, {address}.");
            }
            for message in &messages {
                print_mail(message);
            }
            // Shown is seen.
            if let Some(last) = messages.last().and_then(|m| m["seq"].as_i64()) {
                crate::dispatch(
                    &mut store,
                    "mail.ack",
                    json!({"address": address, "through_seq": last}),
                )?;
                println!("\nAnswer with: privateer reply <id> \"...\"");
            }
        }
        "reply" => {
            let id = p.first().context("usage: privateer reply ID TEXT")?;
            let body = text(p, 1, "privateer reply ID TEXT")?;
            let local = crate::dispatch(&mut store, "mail.show", json!({"id": id}));
            // The captain answers an open ask in whichever home asked it, by
            // its id or a unique prefix of it.
            if address == crate::mail::CAPTAIN
                && local.as_ref().map_or(true, |m| m["state"] == "open")
            {
                let sent =
                    crate::dispatch(&mut store, "captain.reply", json!({"id": id, "body": body}))?;
                println!(
                    "Sent reply to {} ({}).",
                    sent["to"].as_str().unwrap_or(""),
                    sent["id"].as_str().unwrap_or("")
                );
                return Ok(());
            }
            let message = local?;
            let kind = if message["state"] == "open" {
                "reply"
            } else {
                "notice"
            };
            crate::dispatch(
                &mut store,
                "mail.send",
                json!({"from": address, "to": message["from"], "kind": kind,
                    "correlation": id, "body": body}),
            )?;
            println!("Sent {kind} to {}.", message["from"].as_str().unwrap_or(""));
        }
        "dismiss" => {
            let id = p.first().context("usage: privateer dismiss ID [REASON]")?;
            let note = p[1..].join(" ");
            let closed = crate::dispatch(
                &mut store,
                "captain.dismiss",
                json!({"id": id, "note": note}),
            )?;
            println!(
                "Dismissed {} from {} without an answer.",
                closed["id"].as_str().unwrap_or(""),
                closed["ship"].as_str().unwrap_or("")
            );
        }
        "backlog" => {
            let ship = crate::presence::folder_ship(&store)
                .context("run privateer backlog in a ship's folder")?;
            if let [task, rank] = p.as_slice() {
                let rank = match rank.as_str() {
                    "clear" => Value::Null,
                    n => json!(
                        n.parse::<i64>()
                            .context("usage: privateer backlog [TASK RANK|clear]")?
                    ),
                };
                crate::dispatch(&mut store, "task.rank", json!({"id": task, "rank": rank}))?;
            } else {
                ensure!(p.is_empty(), "usage: privateer backlog [TASK RANK|clear]");
            }
            let backlog = crate::dispatch(&mut store, "task.backlog", json!({"ship": ship}))?;
            let tasks = backlog["backlog"].as_array().cloned().unwrap_or_default();
            if tasks.is_empty() {
                println!("{ship} has nothing waiting to start.");
            } else {
                println!("Backlog for {ship}, next first:");
                for task in tasks {
                    let rank = task["rank"]
                        .as_i64()
                        .map(|n| format!("#{n} "))
                        .unwrap_or_default();
                    println!(
                        "  {rank}{}: {}",
                        task["id"].as_str().unwrap_or(""),
                        task["title"].as_str().unwrap_or("")
                    );
                }
            }
        }
        "stow" => {
            let action = p.first().map(String::as_str).unwrap_or("start");
            let command = match action {
                "start" => "stow.start",
                "status" => "stow.status",
                "resume" => "stow.resume",
                _ => bail!("usage: privateer stow [status|resume] [--machine NAME | --all]"),
            };
            let mut request = json!({});
            if let Some(machine) = flags.values.get("machine") {
                request["machine"] = json!(machine);
            }
            // `--all` lifts every linked machine this one reaches, then this one.
            if flags.switches.iter().any(|s| s == "all") {
                request["all"] = json!(true);
            }
            let out = crate::dispatch(&mut store, command, request)?;
            print_stow(action, &out)?;
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
        _ => bail!("unknown command {verb}"),
    }
    Ok(())
}
