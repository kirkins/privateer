//! Machine readiness: whether the tools Privateer itself runs are installed,
//! new enough, and on the PATH its dispatcher launches with, checked before
//! any launch needs them.
//!
//! What is needed comes from configuration, never from machine paths: git
//! always; Treehouse (config.json's `treehouse_binary`, or `treehouse`) when
//! an active charter's jobs can land in Git (`workspace::git_backed`: its
//! repo, a project under its `projects/`, or a held workspace); and the
//! executables each harness manifest `requires`, for every local ship's
//! first mate (blocking: that ship cannot wake) and every route in the
//! routes file (warning: that route cannot launch). A custom manifest that
//! declares no `requires` is reported unchecked, never missing.
//!
//! A check only stats binaries. It spawns the version and protocol probes
//! when a tool's fingerprint (PATH, resolved path, its size and modification
//! time) changed, or its last result is old: five minutes for a failure, six
//! hours for a pass. Results are kept per home in `machine_readiness`, which
//! runtime.needs, the machine owner's alerts and the ship's bearings read.
//! Readiness reports; it never refuses anything itself.
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `rev-parse --path-format=absolute`, which workspaces use, needs git 2.31.
const GIT_FLOOR: (u64, u64, u64) = (2, 31, 0);
/// The first Treehouse whose `get --lease --json` Privateer was qualified on.
const TREEHOUSE_FLOOR: (u64, u64, u64) = (3, 1, 2);
const GIT_INSTALL: &str = "install git 2.31 or later with your package manager";
const TREEHOUSE_INSTALL: &str =
    "install Treehouse 3.1.2 or later, see https://github.com/kunchenguid/treehouse#install";
/// A failure is probed again this soon, since an install clears it.
const FAIL_TTL_MS: i64 = 5 * 60_000;
const PASS_TTL_MS: i64 = 6 * 3_600_000;
/// The dispatcher looks at most this often.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// All of one check's probes finish within this.
const BUDGET: Duration = Duration::from_secs(30);
/// Who needs a tool when everything does.
const ALL: &str = "all";

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS machine_readiness(tool TEXT PRIMARY KEY,
           level TEXT NOT NULL CHECK(level IN ('block','warn')), ok INTEGER NOT NULL,
           version TEXT, path TEXT, why TEXT, install TEXT NOT NULL, needed_by TEXT NOT NULL,
           fingerprint TEXT NOT NULL, checked_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// Whether a home has the table: one opened without migrating may not.
pub(crate) fn available(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='machine_readiness')",
        [],
        |r| r.get(0),
    )?)
}

/// A name a manifest may require: an executable looked up on PATH, never a
/// path.
pub(crate) fn binary_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Probe {
    /// `--version` at or above the floor.
    Git,
    /// `--version` at or above the floor, and `get --help` naming the flags.
    Treehouse,
    /// On PATH, nothing run.
    Presence,
}

#[derive(Clone, Debug)]
struct Need {
    tool: String,
    /// A name to look up on PATH, or an absolute path.
    binary: String,
    probe: Probe,
    block: bool,
    needed_by: Vec<String>,
    install: String,
}

#[derive(Default, Debug)]
struct Needs {
    tools: Vec<Need>,
    /// Manifests that declare no `requires`.
    unchecked: Vec<String>,
}

impl Needs {
    /// One row per tool: blocking if anything it serves blocks, needed by
    /// everyone who needs it.
    fn add(&mut self, need: Need) {
        match self.tools.iter_mut().find(|t| t.tool == need.tool) {
            Some(known) => {
                known.block |= need.block;
                for who in need.needed_by {
                    if !known.needed_by.contains(&who) {
                        known.needed_by.push(who);
                    }
                }
                known.needed_by.sort();
            }
            None => self.tools.push(need),
        }
    }

    fn harness(&mut self, manifest: &str, block: bool, who: &str) {
        match requires(manifest) {
            Some(names) => {
                for name in names {
                    self.add(Need {
                        install: format!("install {name} so it is on the dispatcher's PATH"),
                        tool: name.clone(),
                        binary: name,
                        probe: Probe::Presence,
                        block,
                        needed_by: vec![who.to_owned()],
                    });
                }
            }
            None => {
                if !self.unchecked.iter().any(|m| m == manifest) {
                    self.unchecked.push(manifest.to_owned());
                }
            }
        }
    }
}

/// The executables a harness manifest declares, or None when it declares
/// none or cannot be read. Bundled manifests are read as shipped.
fn requires(reference: &str) -> Option<Vec<String>> {
    let raw = match reference.strip_prefix("bundled:") {
        Some(id) => crate::bundled::manifests()
            .iter()
            .find(|(name, _)| *name == id)?
            .1
            .as_bytes()
            .to_vec(),
        None => {
            use std::io::Read;
            let mut raw = Vec::new();
            std::fs::File::open(reference)
                .ok()?
                .take(65_537)
                .read_to_end(&mut raw)
                .ok()?;
            (raw.len() <= 65_536).then_some(raw)?
        }
    };
    let manifest: Value = serde_json::from_slice(&raw).ok()?;
    let names = manifest.get("requires")?.as_array()?;
    Some(
        names
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| binary_name(name))
            .map(str::to_owned)
            .collect(),
    )
}

/// What this home needs, from its charters, ships and the routes file.
fn needed(store: &Store) -> Result<Needs> {
    // A hand-edited routes file must not stop the check; launches refuse it.
    let policy = crate::crew_routes::for_store(store).ok().flatten();
    needed_from(store, policy.as_ref())
}

fn needed_from(store: &Store, policy: Option<&Value>) -> Result<Needs> {
    let mut needs = Needs::default();
    needs.add(Need {
        tool: "git".into(),
        binary: "git".into(),
        probe: Probe::Git,
        block: true,
        needed_by: vec![ALL.into()],
        install: GIT_INSTALL.into(),
    });
    let charters: Vec<(String, String)> = store
        .connection()
        .prepare("SELECT id, data FROM charters ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut git_charters = Vec::new();
    for (id, data) in charters {
        let Ok(charter) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let (true, Some(repo)) = (charter["active"] == true, charter["repo"].as_str())
            && crate::workspace::git_backed(store, Path::new(repo))?
        {
            git_charters.push(id);
        }
    }
    if !git_charters.is_empty() {
        let binary = crate::workspace::treehouse_name(store)?;
        needs.add(Need {
            tool: "treehouse".into(),
            binary,
            probe: Probe::Treehouse,
            block: true,
            needed_by: git_charters,
            install: TREEHOUSE_INSTALL.into(),
        });
    }
    for ship in crate::activation::ships(store)? {
        let (Some(name), Some(manifest)) = (
            ship["name"].as_str(),
            ship["first_mate"]["manifest"].as_str(),
        ) else {
            continue;
        };
        needs.harness(manifest, true, name);
    }
    if let Some(routes) = policy.and_then(|p| p["routes"].as_object()) {
        for (name, route) in routes {
            if let Some(manifest) = route["manifest"].as_str() {
                needs.harness(manifest, false, &format!("route:{name}"));
            }
        }
    }
    Ok(needs)
}

/// What one probe found.
#[derive(Debug)]
struct Outcome {
    ok: bool,
    version: Option<String>,
    path: Option<String>,
    why: Option<String>,
}

/// `binary`'s full path: an absolute path as it is, a name on `path`.
fn resolve(binary: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if Path::new(binary).is_absolute() {
        let meta = std::fs::metadata(binary).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| binary.into())
    } else {
        crate::capabilities::which_in(binary, path)
    }
}

/// What changes when a tool might: the PATH, where it resolves, and that
/// file's size and modification time.
fn fingerprint(need: &Need, path: &str, resolved: Option<&Path>) -> String {
    use std::os::unix::fs::MetadataExt;
    let file = resolved
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| json!([m.len(), m.mtime(), m.mtime_nsec(), m.ino()]));
    crate::core::hash(&json!([
        path,
        need.tool,
        need.binary,
        format!("{:?}", need.probe),
        resolved.map(|p| p.to_string_lossy().into_owned()),
        file
    ]))
}

/// The first `major.minor[.patch]` in `text`, a leading `v` allowed.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| c.is_whitespace() || c == '(' || c == ',')
        .find_map(|word| {
            let word = word.strip_prefix('v').unwrap_or(word);
            let mut parts = word.split('.');
            let number = |part: Option<&str>| -> Option<u64> {
                let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            };
            let major = number(parts.next())?;
            let minor = number(parts.next())?;
            let patch = number(parts.next()).unwrap_or(0);
            Some((major, minor, patch))
        })
}

fn shown((major, minor, patch): (u64, u64, u64)) -> String {
    format!("{major}.{minor}.{patch}")
}

fn argv(program: &Path, args: &[&str]) -> Vec<String> {
    std::iter::once(program.to_string_lossy().into_owned())
        .chain(args.iter().map(|a| (*a).to_owned()))
        .collect()
}

/// Run the probe for one resolved tool.
fn probe(need: &Need, program: &Path, path: &str, deadline: Instant) -> Outcome {
    let run = |args: &[&str]| {
        crate::capabilities::run_argv_output(&argv(program, args), path, deadline, true)
            .map_err(|why| format!("`{} {}` failed: {why}", need.tool, args.join(" ")))
    };
    let mut outcome = Outcome {
        ok: true,
        version: None,
        path: Some(program.to_string_lossy().into_owned()),
        why: None,
    };
    let floor = match need.probe {
        Probe::Presence => return outcome,
        Probe::Git => GIT_FLOOR,
        Probe::Treehouse => TREEHOUSE_FLOOR,
    };
    let found = match run(&["--version"]) {
        Ok(text) => version(&text),
        Err(why) => {
            outcome.ok = false;
            outcome.why = Some(why);
            return outcome;
        }
    };
    outcome.version = found.map(shown);
    if need.probe == Probe::Treehouse {
        match run(&["get", "--help"]) {
            Ok(help) if help.contains("--lease") && help.contains("--json") => {}
            Ok(_) => {
                outcome.ok = false;
                outcome.why = Some(format!(
                    "treehouse at {} is incompatible: its get has no --lease and --json, which Privateer needs",
                    program.display()
                ));
                return outcome;
            }
            Err(why) => {
                outcome.ok = false;
                outcome.why = Some(why);
                return outcome;
            }
        }
    }
    match found {
        Some(found) if found < floor => {
            outcome.ok = false;
            outcome.why = Some(format!(
                "{} {} at {} is incompatible: Privateer needs {} or later",
                need.tool,
                shown(found),
                program.display(),
                shown(floor)
            ));
        }
        Some(_) => {}
        // The protocol is there, so it is used, unqualified.
        None if need.probe == Probe::Treehouse => {
            outcome.why = Some(format!(
                "treehouse at {} printed no version, so it is used unqualified",
                program.display()
            ));
        }
        None => {
            outcome.ok = false;
            outcome.why = Some(format!(
                "`{} --version` printed no version, so it cannot be checked against {}",
                need.tool,
                shown(floor)
            ));
        }
    }
    outcome
}

/// Why a tool is not on `path`: plainly missing, or on the login shell's
/// PATH only, which a dispatcher started from that shell would record.
fn missing(need: &Need, login: Option<&str>) -> Outcome {
    let why = if Path::new(&need.binary).is_absolute() {
        format!("{} ({}) is not an executable file", need.binary, need.tool)
    } else if let Some(found) =
        login.and_then(|login| crate::capabilities::which_in(&need.binary, login))
    {
        format!(
            "{} is found at {} in your login shell but not on the dispatcher's PATH; run `privateer service start` from that shell",
            need.tool,
            found.display()
        )
    } else {
        format!("{} is not on the dispatcher's PATH", need.tool)
    };
    Outcome {
        ok: false,
        version: None,
        path: None,
        why: Some(why),
    }
}

/// Check what this home needs on `path`, the PATH a dispatcher launches
/// with. `live` probes everything now; otherwise only what changed or is old.
pub(crate) fn check(store: &Store, path: &str, live: bool) -> Result<Value> {
    let needs = needed(store)?;
    check_with(store, path, live, &needs, &crate::service::login_path)
}

fn check_with(
    store: &Store,
    path: &str,
    live: bool,
    needs: &Needs,
    login: &dyn Fn() -> String,
) -> Result<Value> {
    let now = Store::now_ms();
    let known: BTreeMap<String, (bool, String, i64)> = store
        .connection()
        .prepare("SELECT tool, ok, fingerprint, checked_ms FROM machine_readiness")?
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut due = vec![];
    let mut kept = vec![];
    for need in &needs.tools {
        let resolved = resolve(&need.binary, path);
        let print = fingerprint(need, path, resolved.as_deref());
        let fresh = known.get(&need.tool).is_some_and(|(ok, old, at)| {
            let ttl = if *ok { PASS_TTL_MS } else { FAIL_TTL_MS };
            *old == print && now - at < ttl
        });
        if live || !fresh {
            due.push((need, resolved, print));
        } else {
            kept.push(need);
        }
    }
    // The login shell is asked once, and only when something is missing.
    let login = due
        .iter()
        .any(|(need, resolved, _)| resolved.is_none() && !Path::new(&need.binary).is_absolute())
        .then(login);
    let deadline = Instant::now() + BUDGET;
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let running: Vec<_> = due
            .iter()
            .map(|(need, resolved, _)| {
                let login = login.as_deref();
                scope.spawn(move || match resolved {
                    Some(program) => probe(need, program, path, deadline),
                    None => missing(need, login),
                })
            })
            .collect();
        running
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| Outcome {
                    ok: false,
                    version: None,
                    path: None,
                    why: Some("the probe failed".into()),
                })
            })
            .collect()
    });
    let level = |need: &Need| if need.block { "block" } else { "warn" };
    let tx = store.write_transaction()?;
    for ((need, _, print), outcome) in due.iter().zip(&outcomes) {
        tx.execute(
            "INSERT OR REPLACE INTO machine_readiness(tool, level, ok, version, path, why, install,
               needed_by, fingerprint, checked_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                need.tool,
                level(need),
                outcome.ok,
                outcome.version,
                outcome.path,
                outcome.why,
                need.install,
                json!(need.needed_by).to_string(),
                print,
                now
            ],
        )?;
    }
    // Who needs a tool can change without anything to probe again.
    for need in kept {
        tx.execute(
            "UPDATE machine_readiness SET level=?2, install=?3, needed_by=?4 WHERE tool=?1",
            params![
                need.tool,
                level(need),
                need.install,
                json!(need.needed_by).to_string()
            ],
        )?;
    }
    for tool in known.keys() {
        if !needs.tools.iter().any(|need| &need.tool == tool) {
            tx.execute("DELETE FROM machine_readiness WHERE tool=?1", [tool])?;
        }
    }
    tx.commit()?;
    let failing: Vec<Value> = failing(store.connection())?
        .iter()
        .map(Row::to_json)
        .collect();
    let probed: Vec<&str> = due.iter().map(|(need, _, _)| need.tool.as_str()).collect();
    Ok(json!({"probed": probed, "failing": failing, "unchecked": needs.unchecked}))
}

/// The dispatcher's check, at most once a minute, on its own PATH: the one
/// its unit recorded and its launches inherit. The pass never waits on it:
/// the check runs on its own thread and the pass reports what is stored.
/// `start` is false in a one-pass dispatcher (`serve --once`), which only
/// reports: it would exit with the check unfinished, leaving its probes'
/// process groups behind, and waiting would slow every maintenance pass.
/// Its home is checked live when its dispatcher starts (`ensure_summary`).
pub(crate) fn tick(store: &Store, start: bool) -> Value {
    let path = std::env::var("PATH").unwrap_or_default();
    DISPATCHER.tick(store, &path, start)
}

static DISPATCHER: Checker = Checker::new(CHECK_EVERY);

/// A process's checks of its home: started at most once per `every`, at
/// most one at a time.
pub(crate) struct Checker {
    every: Duration,
    last: std::sync::Mutex<Option<Instant>>,
    running: std::sync::Mutex<Option<std::thread::JoinHandle<Value>>>,
}

impl Checker {
    const fn new(every: Duration) -> Self {
        Checker {
            every,
            last: std::sync::Mutex::new(None),
            running: std::sync::Mutex::new(None),
        }
    }

    /// Collect a finished check, start one when due, and report the stored
    /// rows. Null when there is nothing new to say.
    fn tick(&self, store: &Store, path: &str, start: bool) -> Value {
        let (Ok(mut last), Ok(mut running)) = (self.last.lock(), self.running.lock()) else {
            return Value::Null;
        };
        let checked = match running.take() {
            Some(handle) if handle.is_finished() => Some(
                handle
                    .join()
                    .unwrap_or_else(|_| json!({"error": "the check panicked"})),
            ),
            other => {
                *running = other;
                None
            }
        };
        let due = !last.is_some_and(|at| at.elapsed() < self.every);
        if !due && checked.is_none() {
            return Value::Null;
        }
        let busy = running.is_some();
        let mut report = json!({"started": false});
        if due && start && !busy {
            let home = store.home().to_path_buf();
            let path = path.to_owned();
            let spawned = std::thread::Builder::new()
                .name("readiness".into())
                .spawn(move || {
                    Store::open(&home)
                        .and_then(|store| check(&store, &path, false))
                        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}))
                });
            match spawned {
                Ok(handle) => {
                    *running = Some(handle);
                    report["started"] = json!(true);
                }
                Err(error) => report["error"] = json!(format!("{error}")),
            }
        }
        // A check still in flight when the next is due is followed by one as
        // soon as it finishes.
        if due && !(start && busy) {
            *last = Some(Instant::now());
        }
        report["running"] = json!(running.is_some());
        if let Some(checked) = checked {
            report["checked"] = checked;
        }
        report["failing"] = match failing(store.connection()) {
            Ok(rows) => json!(rows.iter().map(Row::to_json).collect::<Vec<_>>()),
            Err(error) => json!({"error": format!("{error:#}")}),
        };
        report
    }
}

/// One stored result that is failing.
struct Row {
    tool: String,
    level: String,
    why: String,
    install: String,
    needed_by: Vec<String>,
    fingerprint: String,
}

impl Row {
    /// What it stops.
    fn consequence(&self) -> String {
        if self.needed_by.iter().any(|who| who == ALL) {
            return "no task workspace or crew can start".into();
        }
        if self.tool == "treehouse" {
            return "crews on Git projects cannot start".into();
        }
        let (routes, ships): (Vec<&str>, Vec<&str>) = self
            .needed_by
            .iter()
            .map(String::as_str)
            .partition(|who| who.starts_with("route:"));
        let routes: Vec<&str> = routes
            .iter()
            .map(|r| r.trim_start_matches("route:"))
            .collect();
        let mut parts = vec![];
        if !ships.is_empty() {
            parts.push(format!(
                "the first mate of {} cannot wake",
                ships.join(", ")
            ));
        }
        if !routes.is_empty() {
            let noun = if routes.len() == 1 { "route" } else { "routes" };
            parts.push(format!("{noun} {} cannot launch", routes.join(", ")));
        }
        parts.join(", and ")
    }

    /// The whole sentence: what is wrong, what it stops, and the install step.
    fn sentence(&self) -> String {
        format!(
            "{}, so {}. Install: {}.",
            self.why,
            self.consequence(),
            self.install
        )
    }

    fn to_json(&self) -> Value {
        json!({"tool": self.tool, "level": self.level, "why": self.why,
            "install": self.install, "needed_by": self.needed_by})
    }
}

fn failing(conn: &Connection) -> Result<Vec<Row>> {
    if !available(conn)? {
        return Ok(vec![]);
    }
    let rows = conn
        .prepare(
            "SELECT tool, level, COALESCE(why, ''), install, needed_by, fingerprint
             FROM machine_readiness WHERE ok=0 ORDER BY level, tool",
        )?
        .query_map([], |r| {
            let needed_by: String = r.get(4)?;
            Ok(Row {
                tool: r.get(0)?,
                level: r.get(1)?,
                why: r.get(2)?,
                install: r.get(3)?,
                needed_by: serde_json::from_str(&needed_by).unwrap_or_default(),
                fingerprint: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The runtime.needs item for a failing tool, read inside the observation's
/// transaction.
pub(crate) fn need_item(conn: &Connection, tool: &str) -> Result<Value> {
    let row = failing(conn)?
        .into_iter()
        .find(|row| row.tool == tool)
        .ok_or_else(|| anyhow::anyhow!("readiness row vanished"))?;
    let checked: Option<i64> = conn
        .query_row(
            "SELECT checked_ms FROM machine_readiness WHERE tool=?1",
            [tool],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json!({
        "kind": "blocker",
        "evidence": "machine_tool_missing",
        "status": row.level,
        "tool": row.tool,
        "summary": row.sentence(),
        "install": row.install,
        "needed_by": row.needed_by,
        "created_ms": checked,
        "version": crate::core::hash(&json!([row.tool, row.fingerprint, row.why])),
        "actions": [],
    }))
}

/// Bearings lines for `ship`: each failing tool its own work needs.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    Ok(failing(store.connection())?
        .iter()
        .filter(|row| row.needed_by.iter().any(|who| who == ship || who == ALL))
        .map(|row| format!("Machine: {}", row.sentence()))
        .collect())
}

/// The owner alert's problems: each blocking tool that is failing, keyed
/// `tool:<name>`, read from the last check without probing.
pub(crate) fn owner_problems(store: &Store) -> Result<Vec<(String, String)>> {
    Ok(failing(store.connection())?
        .iter()
        .filter(|row| row.level == "block")
        .map(|row| (format!("tool:{}", row.tool), row.sentence()))
        .collect())
}

/// Summarize what fails in `home`, for the output of init, ship.create and
/// service start: checked live on `path`, the PATH a dispatcher about to
/// start records, or with no `path` (one is already running, on its own
/// PATH) what that dispatcher last stored, without probing or saving.
/// Never refuses: a home that cannot be opened or checked gets no summary.
pub(crate) fn ensure_summary(home: &Path, path: Option<&str>) -> Option<String> {
    if !home.join("privateer.sqlite").is_file() {
        return None;
    }
    let store = Store::open(home).ok()?;
    if let Some(path) = path {
        check(&store, path, true).ok()?;
    }
    let failing = failing(store.connection()).ok()?;
    if failing.is_empty() {
        return None;
    }
    let parts: Vec<String> = failing
        .iter()
        .map(|row| {
            format!(
                "{} ({}): {}, so {}; {}",
                row.tool,
                row.level,
                row.why,
                row.consequence(),
                row.install
            )
        })
        .collect();
    Some(format!("Readiness: {}", parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Machine {
        _temp: tempfile::TempDir,
        root: PathBuf,
        bin: PathBuf,
        store: Store,
    }

    impl Machine {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let bin = root.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let store = Store::open(&root.join("home")).unwrap();
            let machine = Machine {
                _temp: temp,
                root,
                bin,
                store,
            };
            machine.tool("git", "echo 'git version 2.43.0'");
            machine
        }

        fn path(&self) -> String {
            self.bin.to_string_lossy().into_owned()
        }

        /// A fake executable on the test PATH that runs `body` as sh, and
        /// logs each invocation's arguments to `<name>.log`.
        fn tool(&self, name: &str, body: &str) {
            let log = self.root.join(format!("{name}.log"));
            let file = self.bin.join(name);
            // A fresh file each time, so its fingerprint changes.
            let _ = std::fs::remove_file(&file);
            std::fs::write(
                &file,
                format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n", log.display()),
            )
            .unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn calls(&self, name: &str) -> usize {
            std::fs::read_to_string(self.root.join(format!("{name}.log")))
                .map(|log| log.lines().count())
                .unwrap_or(0)
        }

        fn treehouse(&self, version: &str, help: &str) {
            self.tool(
                "treehouse",
                &format!("case \"$1\" in --version) echo '{version}';; get) echo '{help}';; esac"),
            );
        }

        /// An active charter laid out as ships are: its repo is the ship
        /// folder, which is no Git work tree, and the Git repository is a
        /// project under it, `<folder>/projects/<name>/.git`.
        fn git_charter(&self, id: &str) {
            let folder = self.root.join(format!("ship-{id}"));
            std::fs::create_dir_all(folder.join("projects").join(id).join(".git")).unwrap();
            self.charter(id, &folder);
        }

        fn charter(&self, id: &str, repo: &Path) {
            let charter = json!({"id": id, "active": true, "repo": repo, "actions": []});
            self.store
                .connection()
                .execute(
                    "INSERT OR REPLACE INTO charters(id, data) VALUES(?1, ?2)",
                    params![id, charter.to_string()],
                )
                .unwrap();
        }

        /// A local ship whose first mate runs `manifest`.
        fn ship(&self, name: &str, manifest: &str) {
            let record = json!({"name": name, "location": "here",
                "first_mate": {"manifest": manifest, "duty_task": "duty"}});
            self.store
                .connection()
                .execute(
                    "INSERT OR REPLACE INTO ships(name, record) VALUES(?1, ?2)",
                    params![name, record.to_string()],
                )
                .unwrap();
        }

        /// A custom manifest requiring `names`, or declaring nothing.
        fn manifest(&self, id: &str, names: Option<&[&str]>) -> String {
            let path = self.root.join(format!("{id}.json"));
            let mut manifest =
                json!({"schema_version": 1, "id": id, "executable": ["python3", "x.py"]});
            if let Some(names) = names {
                manifest["requires"] = json!(names);
            }
            std::fs::write(&path, manifest.to_string()).unwrap();
            path.to_string_lossy().into_owned()
        }

        fn check_on(&self, path: &str, live: bool, policy: Option<&Value>, login: &str) -> Value {
            let needs = needed_from(&self.store, policy).unwrap();
            let login = login.to_owned();
            check_with(&self.store, path, live, &needs, &move || login.clone()).unwrap()
        }

        fn check(&self) -> Value {
            self.check_on(&self.path(), false, None, "")
        }

        fn failing_tools(&self) -> Vec<String> {
            failing(self.store.connection())
                .unwrap()
                .into_iter()
                .map(|row| row.tool)
                .collect()
        }

        fn why(&self, tool: &str) -> String {
            self.store
                .connection()
                .query_row(
                    "SELECT COALESCE(why, '') FROM machine_readiness WHERE tool=?1",
                    [tool],
                    |r| r.get(0),
                )
                .unwrap()
        }

        fn needs(&self, after: &str, limit: u64) -> Value {
            crate::needs::query(&self.store, &json!({"after": after, "limit": limit})).unwrap()
        }
    }

    const HELP: &str = "Usage: treehouse get [--lease] [--json]";

    #[test]
    fn a_missing_treehouse_blocks_until_a_qualified_one_appears() {
        let m = Machine::new();
        m.git_charter("storefront");
        let report = m.check();
        assert_eq!(report["failing"][0]["tool"], "treehouse", "{report}");
        assert_eq!(report["failing"][0]["level"], "block");
        assert_eq!(
            m.why("treehouse"),
            "treehouse is not on the dispatcher's PATH"
        );

        let needs = m.needs("", 40);
        let item = needs["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["source"] == "readiness")
            .unwrap_or_else(|| panic!("{needs}"))
            .clone();
        assert_eq!(item["source_id"], "treehouse");
        assert_eq!(item["kind"], "blocker");
        assert_eq!(item["evidence"], "machine_tool_missing");
        assert_eq!(item["status"], "block");
        assert_eq!(item["actions"], json!([]));
        let summary = item["summary"].as_str().unwrap();
        assert!(
            summary.contains("https://github.com/kunchenguid/treehouse#install"),
            "{summary}"
        );
        println!("runtime.needs item: {item}");

        let lines = bearings_lines(&m.store, "storefront").unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(
                "Machine: treehouse is not on the dispatcher's PATH, so crews on Git projects cannot start. Install: install Treehouse"
            ),
            "{lines:?}"
        );
        println!("bearings line: {}", lines[0]);
        assert!(bearings_lines(&m.store, "other").unwrap().is_empty());
        m.store
            .connection()
            .execute(
                "INSERT INTO ships(name, record) VALUES('storefront', ?1)",
                [json!({"name": "storefront", "location": "here"}).to_string()],
            )
            .unwrap();
        let context = crate::context::bearings(&m.store, "storefront").unwrap();
        assert!(
            context["text"]
                .as_str()
                .unwrap()
                .contains("Machine: treehouse"),
            "{context}"
        );

        // Installed: the fingerprint changes, so no five minutes' wait.
        m.treehouse("v3.1.2", HELP);
        let report = m.check();
        assert_eq!(report["failing"], json!([]), "{report}");
        assert!(bearings_lines(&m.store, "storefront").unwrap().is_empty());
        assert!(
            !m.needs("", 40)["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["source"] == "readiness")
        );
    }

    #[test]
    fn an_old_treehouse_or_one_without_the_protocol_is_incompatible() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.treehouse("treehouse v3.0.0", HELP);
        m.check();
        assert_eq!(m.failing_tools(), ["treehouse"]);
        let why = m.why("treehouse");
        assert!(
            why.contains("3.0.0") && why.contains("needs 3.1.2 or later"),
            "{why}"
        );

        m.treehouse("v3.1.2", "Usage: treehouse get [--json]");
        m.check();
        assert_eq!(m.failing_tools(), ["treehouse"]);
        let why = m.why("treehouse");
        assert!(why.contains("no --lease and --json"), "{why}");

        // The protocol without a readable version is used, unqualified.
        m.treehouse("dev build", HELP);
        m.check();
        assert!(m.failing_tools().is_empty());
        assert!(m.why("treehouse").contains("unqualified"));
    }

    #[test]
    fn git_below_its_floor_blocks_everything() {
        let m = Machine::new();
        m.tool("git", "echo 'git version 2.30.9'");
        m.check();
        assert_eq!(m.failing_tools(), ["git"]);
        assert!(m.why("git").contains("needs 2.31.0 or later"));
        let lines = bearings_lines(&m.store, "any-ship").unwrap();
        assert!(
            lines[0].contains("no task workspace or crew can start"),
            "{lines:?}"
        );
        m.tool("git", "echo 'git version 2.43.0 (Apple Git-146)'");
        m.check();
        assert!(m.failing_tools().is_empty());
    }

    #[test]
    fn a_route_harness_warns_and_a_first_mate_harness_blocks_its_ship() {
        let m = Machine::new();
        let manifest = m.manifest("fake", Some(&["fakeharness"]));
        let policy = json!({"routes": {"cheap": {"manifest": manifest, "cost": "free"}}});
        let report = m.check_on(&m.path(), false, Some(&policy), "");
        assert_eq!(report["failing"][0]["tool"], "fakeharness", "{report}");
        assert_eq!(report["failing"][0]["level"], "warn");
        assert_eq!(report["failing"][0]["needed_by"], json!(["route:cheap"]));
        let needs = m.needs("", 40);
        let item = &needs["items"][0];
        assert_eq!(item["status"], "warn", "{needs}");
        assert!(
            item["summary"]
                .as_str()
                .unwrap()
                .contains("route cheap cannot launch")
        );
        // A warning raises no owner alert.
        assert!(owner_problems(&m.store).unwrap().is_empty());

        m.ship("alpha", &manifest);
        m.ship("beta", "bundled:no-such-thing");
        m.check_on(&m.path(), false, Some(&policy), "");
        let rows = failing(m.store.connection()).unwrap();
        assert_eq!(rows[0].level, "block");
        assert_eq!(rows[0].needed_by, ["alpha", "route:cheap"]);
        assert_eq!(bearings_lines(&m.store, "alpha").unwrap().len(), 1);
        assert!(bearings_lines(&m.store, "beta").unwrap().is_empty());
        assert!(
            bearings_lines(&m.store, "alpha").unwrap()[0]
                .contains("the first mate of alpha cannot wake, and route cheap cannot launch")
        );
    }

    #[test]
    fn a_manifest_without_requires_is_unchecked_not_missing() {
        let m = Machine::new();
        let custom = m.manifest("custom", None);
        m.ship("alpha", &custom);
        let report = m.check();
        assert_eq!(report["unchecked"], json!([custom]));
        assert!(m.failing_tools().is_empty());
    }

    #[test]
    fn bundled_manifests_require_python_and_their_harness() {
        let m = Machine::new();
        m.ship("alpha", "bundled:claude");
        m.tool("python3", "true");
        m.check();
        assert_eq!(m.failing_tools(), ["claude"]);
        for (id, raw) in crate::bundled::manifests() {
            let names = requires(&format!("bundled:{id}")).unwrap();
            assert_eq!(names.len(), 2, "{id}: {raw}");
            assert_eq!(names[0], "python3", "{id}");
        }
    }

    #[test]
    fn an_unchanged_fingerprint_spawns_no_probe() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.treehouse("v3.1.2", HELP);
        m.check();
        assert_eq!((m.calls("git"), m.calls("treehouse")), (1, 2));
        m.check();
        m.check();
        assert_eq!((m.calls("git"), m.calls("treehouse")), (1, 2));
        // A live check probes regardless.
        m.check_on(&m.path(), true, None, "");
        assert_eq!((m.calls("git"), m.calls("treehouse")), (2, 4));
        // So does an old result.
        m.store
            .connection()
            .execute(
                "UPDATE machine_readiness SET checked_ms=checked_ms-?1",
                [PASS_TTL_MS],
            )
            .unwrap();
        m.check();
        assert_eq!((m.calls("git"), m.calls("treehouse")), (3, 6));
    }

    #[test]
    fn a_tool_only_on_the_login_path_names_the_fix() {
        let m = Machine::new();
        m.git_charter("storefront");
        let login = m.root.join("login-bin");
        std::fs::create_dir(&login).unwrap();
        let file = login.join("treehouse");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        m.check_on(&m.path(), false, None, &login.to_string_lossy());
        let why = m.why("treehouse");
        assert!(
            why.contains(&format!("found at {} in your login shell", file.display()))
                && why.contains("run `privateer service start` from that shell"),
            "{why}"
        );
    }

    #[test]
    fn a_home_without_a_git_charter_needs_no_treehouse() {
        let m = Machine::new();
        let plain = m.root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        m.charter("notes", &plain);
        let report = m.check();
        assert_eq!(report["probed"], json!(["git"]), "{report}");
        let rows: i64 = m
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM machine_readiness WHERE tool='treehouse'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        // A tool no longer needed leaves the table.
        m.git_charter("storefront");
        m.check();
        assert_eq!(m.failing_tools(), ["treehouse"]);
        m.store
            .connection()
            .execute("DELETE FROM charters WHERE id='storefront'", [])
            .unwrap();
        m.check();
        assert!(m.failing_tools().is_empty());
    }

    #[test]
    fn a_charter_whose_projects_live_elsewhere_needs_treehouse_once_it_holds_a_workspace() {
        let m = Machine::new();
        let folder = m.root.join("ship-notes");
        std::fs::create_dir_all(folder.join("projects").join(".staging").join(".git")).unwrap();
        m.charter("notes", &folder);
        // A clone still being staged is not a project yet.
        m.check();
        assert!(m.failing_tools().is_empty());
        let conn = m.store.connection();
        conn.execute(
            "INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('t','notes','p','t','open',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspaces(task_id, record) VALUES('t', ?1)",
            [json!({"charter_repo": folder}).to_string()],
        )
        .unwrap();
        m.check();
        assert_eq!(m.failing_tools(), ["treehouse"]);
    }

    #[test]
    fn runtime_needs_pages_readiness_items_by_source_and_id() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.tool("git", "echo 'git version 2.30.0'");
        m.check();
        assert_eq!(m.failing_tools(), ["git", "treehouse"]);
        let first = m.needs("", 1);
        assert_eq!(first["items"][0]["source_id"], "git", "{first}");
        assert_eq!(first["next_after"], "readiness:git");
        let second = m.needs("readiness:git", 1);
        assert_eq!(second["items"][0]["source_id"], "treehouse", "{second}");
        assert_eq!(second["next_after"], Value::Null);
        // An oversized row makes the observation unavailable, not quiet.
        m.store
            .connection()
            .execute(
                "UPDATE machine_readiness SET why=?1 WHERE tool='git'",
                ["x".repeat(70_000)],
            )
            .unwrap();
        let refused = m.needs("", 40);
        assert_eq!(refused["available"], false, "{refused}");
    }

    #[test]
    fn the_owner_alert_raises_and_clears_a_blocking_tool() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.check();
        let problems = owner_problems(&m.store).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].0, "tool:treehouse");
        assert!(problems[0].1.contains("Install: install Treehouse"));
        m.treehouse("v3.1.2", HELP);
        m.check();
        assert!(owner_problems(&m.store).unwrap().is_empty());
    }

    /// Holds `home`'s serve lock, as a running dispatcher does.
    fn hold_serve_lock(home: &Path) -> std::fs::File {
        use std::os::fd::AsRawFd;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(home.join("serve.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock
    }

    #[test]
    fn a_running_dispatcher_keeps_its_rows_when_started_from_a_bare_path() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.treehouse("v3.1.2", HELP);
        m.check();
        assert!(m.failing_tools().is_empty());
        let home = m.root.join("home");
        let _running = hold_serve_lock(&home);
        // A non-login SSH shell: nothing of the dispatcher's tools on PATH.
        let empty = m.root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let report =
            crate::service::ensure_with_path(&home, "storefront", &empty.to_string_lossy())
                .unwrap();
        assert_eq!(report, "already running");
        assert!(m.failing_tools().is_empty(), "{:?}", m.failing_tools());
    }

    #[test]
    fn a_running_dispatcher_keeps_its_failures_when_started_from_a_fuller_path() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.check();
        assert_eq!(m.failing_tools(), ["treehouse"]);
        let home = m.root.join("home");
        let _running = hold_serve_lock(&home);
        // This shell has Treehouse; the running dispatcher's PATH does not.
        let fuller = m.root.join("fuller");
        std::fs::create_dir(&fuller).unwrap();
        std::fs::copy(m.bin.join("git"), fuller.join("git")).unwrap();
        std::fs::write(
            fuller.join("treehouse"),
            format!(
                "#!/bin/sh\ncase \"$1\" in --version) echo v3.1.2;; get) echo '{HELP}';; esac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            fuller.join("treehouse"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let report =
            crate::service::ensure_with_path(&home, "storefront", &fuller.to_string_lossy())
                .unwrap();
        assert_eq!(m.failing_tools(), ["treehouse"]);
        // The report says what the running dispatcher last found.
        assert!(
            report.starts_with("already running. Readiness: treehouse (block)"),
            "{report}"
        );
    }

    /// Tick `checker` until its background check reports, without starting
    /// another, so nothing outlives the test's home.
    fn finished(checker: &Checker, m: &Machine) -> Value {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let report = checker.tick(&m.store, &m.path(), false);
            if report["checked"].is_object() {
                return report;
            }
            assert!(Instant::now() < until, "the check never finished");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn the_dispatcher_pass_never_waits_on_a_stuck_probe() {
        let m = Machine::new();
        m.tool("git", "/bin/sleep 3; echo 'git version 2.43.0'");
        let checker = Checker::new(Duration::ZERO);
        let begun = Instant::now();
        let report = checker.tick(&m.store, &m.path(), true);
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "the pass waited {:?} on a probe: {report}",
            begun.elapsed()
        );
        assert_eq!(report["started"], true, "{report}");
        // Meanwhile it reports what was stored before: nothing yet.
        assert_eq!(report["failing"], json!([]), "{report}");
        // The check finishes on its own and stores its rows.
        let report = finished(&checker, &m);
        assert_eq!(report["checked"]["probed"], json!(["git"]), "{report}");
        let ok: bool = m
            .store
            .connection()
            .query_row(
                "SELECT ok FROM machine_readiness WHERE tool='git'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn two_ticks_while_a_check_runs_start_one_check() {
        let m = Machine::new();
        m.tool("git", "/bin/sleep 3; echo 'git version 2.43.0'");
        let checker = Checker::new(Duration::ZERO);
        let first = checker.tick(&m.store, &m.path(), true);
        let second = checker.tick(&m.store, &m.path(), true);
        assert_eq!(first["started"], true, "{first}");
        assert_eq!(second["started"], false, "{second}");
        assert_eq!(second["running"], true, "{second}");
        finished(&checker, &m);
        assert_eq!(m.calls("git"), 1);
    }

    #[test]
    fn a_one_pass_dispatcher_reports_stored_rows_without_starting_a_check() {
        let m = Machine::new();
        m.tool("git", "echo 'git version 2.20.0'");
        m.check();
        assert_eq!(m.calls("git"), 1);
        let checker = Checker::new(Duration::ZERO);
        let report = checker.tick(&m.store, &m.path(), false);
        assert_eq!(report["started"], false, "{report}");
        assert_eq!(report["running"], false, "{report}");
        assert_eq!(report["failing"][0]["tool"], "git", "{report}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(m.calls("git"), 1);
    }

    #[test]
    fn a_hanging_login_shell_is_killed_at_its_deadline() {
        let m = Machine::new();
        let shell = m.root.join("shell");
        let pids = m.root.join("pids");
        // A login shell whose rc file never returns, holding a child too.
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\nsleep 10 &\necho $$ $! > '{}'\nwait\n",
                pids.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shell = shell.to_string_lossy().into_owned();
        let bound = Duration::from_secs(1);
        let begun = Instant::now();
        let path = crate::service::login_path_from(Some(&shell), Instant::now() + bound);
        assert!(
            begun.elapsed() < Duration::from_secs(3),
            "the login shell held the lookup {:?}",
            begun.elapsed()
        );
        // Only this process's PATH is left.
        assert_eq!(path, crate::service::login_path_from(None, Instant::now()));
        // The shell and what it started were killed.
        for pid in std::fs::read_to_string(&pids).unwrap().split_whitespace() {
            let pid: libc::pid_t = pid.parse().unwrap();
            let until = Instant::now() + Duration::from_secs(3);
            while unsafe { libc::kill(pid, 0) } == 0 {
                assert!(Instant::now() < until, "{pid} outlived its deadline");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // A check that asks it records the missing tool instead of hanging.
        let manifest = m.manifest("fake", Some(&["privateer-no-such-harness"]));
        m.ship("alpha", &manifest);
        let needs = needed_from(&m.store, None).unwrap();
        let begun = Instant::now();
        check_with(&m.store, &m.path(), true, &needs, &|| {
            crate::service::login_path_from(Some(&shell), Instant::now() + bound)
        })
        .unwrap();
        assert!(begun.elapsed() < Duration::from_secs(3));
        assert_eq!(m.failing_tools(), ["privateer-no-such-harness"]);
        assert_eq!(
            m.why("privateer-no-such-harness"),
            "privateer-no-such-harness is not on the dispatcher's PATH"
        );
    }

    #[test]
    fn versions_are_read_from_common_shapes() {
        assert_eq!(version("git version 2.43.0"), Some((2, 43, 0)));
        assert_eq!(version("v3.1.2"), Some((3, 1, 2)));
        assert_eq!(version("treehouse 3.1"), Some((3, 1, 0)));
        assert_eq!(
            version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
        assert_eq!(version("2.45.1.windows.1"), Some((2, 45, 1)));
        assert_eq!(version("dev build"), None);
        assert!(binary_name("claude") && binary_name("cursor-agent"));
        assert!(!binary_name("/usr/bin/claude") && !binary_name("a b") && !binary_name(""));
    }
}
