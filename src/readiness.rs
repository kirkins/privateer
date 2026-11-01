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
//! declares no `requires` is reported unchecked, never missing. Herdr when a
//! local ship's first mate is hosted (warning: it falls back to headless
//! wakes), and ssh when this machine reaches a linked one over SSH (warning:
//! that link cannot be reached).
//!
//! A check only stats binaries. It spawns the version and protocol probes
//! when a tool's fingerprint (PATH, resolved path, its size and modification
//! time) changed, or its last result is old: five minutes for a failure, six
//! hours for a pass. Results are kept per home in `machine_readiness`, which
//! runtime.needs, the machine owner's alerts and the ship's bearings read.
//! Readiness reports; it never refuses anything itself.
use crate::store::Store;
use anyhow::{Context, Result};
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
const HERDR_INSTALL: &str = "install Herdr so herdr is on the dispatcher's PATH";
const SSH_INSTALL: &str = "install an OpenSSH client so ssh is on the dispatcher's PATH";
/// A failure is probed again this soon, since an install clears it.
const FAIL_TTL_MS: i64 = 5 * 60_000;
const PASS_TTL_MS: i64 = 6 * 3_600_000;
/// A failing row older than this was left by a checker that no longer runs
/// (a live one rewrites it every `FAIL_TTL_MS`), so route choice ignores it.
const STALE_FAIL_MS: i64 = 15 * 60_000;
/// The dispatcher looks at most this often.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// All of one check's probes finish within this.
const BUDGET: Duration = Duration::from_secs(30);
/// Who needs a tool when everything does.
const ALL: &str = "all";
/// A ship whose first mate a host keeps, in `needed_by`: `host:<ship>`.
const HOSTED: &str = "host:";
/// A linked machine this one reaches, in `needed_by`: `machine:<name>`.
const LINKED: &str = "machine:";
/// How long `machine.readiness` may spend checking live, the login shell's
/// lookup aside, so a linking machine's call returns well inside its timeout.
const REPORT_BUDGET: Duration = Duration::from_secs(15);

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
    /// Runs with this flag and exits 0; a version it prints on standard
    /// output is kept. Standard error is not read, so ssh's is never kept.
    Version(&'static str),
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

/// What this home needs, from its charters, ships, the routes file and the
/// machines this one reaches.
fn needed(store: &Store) -> Result<Needs> {
    // A hand-edited routes file must not stop the check; launches refuse it.
    let policy = crate::crew_routes::for_store(store).ok().flatten();
    let ssh = crate::peers::ssh_program().to_string_lossy().into_owned();
    needed_from(
        store,
        policy.as_ref(),
        &crate::machines::reached_over_ssh(),
        &ssh,
    )
}

/// `links` are the machines this one reaches over SSH, with `ssh`.
fn needed_from(
    store: &Store,
    policy: Option<&Value>,
    links: &[String],
    ssh: &str,
) -> Result<Needs> {
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
        let Some(name) = ship["name"].as_str() else {
            continue;
        };
        if let Some(manifest) = ship["first_mate"]["manifest"].as_str() {
            needs.harness(manifest, true, name);
        }
        if crate::host::hosted(&ship["first_mate"]) {
            needs.add(Need {
                tool: "herdr".into(),
                binary: "herdr".into(),
                probe: Probe::Version("--version"),
                block: false,
                needed_by: vec![format!("{HOSTED}{name}")],
                install: HERDR_INSTALL.into(),
            });
        }
    }
    if !links.is_empty() {
        needs.add(Need {
            tool: "ssh".into(),
            binary: ssh.into(),
            // OpenSSH has no --version; `-V` prints on standard error, so
            // the probe only sees that it runs.
            probe: Probe::Version("-V"),
            block: false,
            needed_by: links.iter().map(|m| format!("{LINKED}{m}")).collect(),
            install: SSH_INSTALL.into(),
        });
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
#[derive(Clone, Debug)]
struct Outcome {
    ok: bool,
    version: Option<String>,
    path: Option<String>,
    why: Option<String>,
}

/// `binary`'s full path: an absolute path as it is, a name on `path`.
fn resolve(binary: &str, path: &str) -> Option<PathBuf> {
    let python = (binary == "python3").then(crate::python::program);
    resolve_with_python(binary, path, python.as_deref())
}

fn resolve_with_python(
    binary: &str,
    path: &str,
    python: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let binary = if binary == "python3" {
        python.unwrap_or_else(|| std::ffi::OsStr::new(binary))
    } else {
        std::ffi::OsStr::new(binary)
    };
    if Path::new(binary).is_absolute() {
        let meta = std::fs::metadata(binary).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then(|| binary.into())
    } else {
        crate::capabilities::which_in(binary.to_str()?, path)
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

/// Run the probe for one resolved tool.
fn probe(need: &Need, program: &Path, path: &str, deadline: Instant) -> Outcome {
    let run = |args: &[&str]| {
        let argv: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        crate::capabilities::run_program_output(program, &argv, path, deadline, true)
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
        Probe::Version(flag) => {
            match run(&[flag]) {
                Ok(text) => outcome.version = version(&text).map(shown),
                Err(why) => {
                    outcome.ok = false;
                    outcome.why = Some(why);
                }
            }
            return outcome;
        }
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
/// `dispatcher` says whether `path` is the dispatcher's or this command's.
fn missing(need: &Need, login: Option<&str>, dispatcher: bool) -> Outcome {
    let on = if dispatcher {
        "the dispatcher's PATH"
    } else {
        "this command's PATH"
    };
    let why = if Path::new(&need.binary).is_absolute() {
        format!("{} ({}) is not an executable file", need.binary, need.tool)
    } else if let Some(found) =
        login.and_then(|login| crate::capabilities::which_in(&need.binary, login))
    {
        let advice = if dispatcher {
            "; run `privateer service start` from that shell"
        } else {
            ""
        };
        format!(
            "{} is found at {} in your login shell but not on {on}{advice}",
            need.tool,
            found.display()
        )
    } else {
        format!("{} is not on {on}", need.tool)
    };
    Outcome {
        ok: false,
        version: None,
        path: None,
        why: Some(why),
    }
}

/// Probe each resolved tool in parallel, by `deadline`, and say why each
/// unresolved one is missing.
fn run_all(
    probes: &[(&Need, Option<&Path>)],
    path: &str,
    login: Option<&str>,
    deadline: Instant,
) -> Vec<Outcome> {
    std::thread::scope(|scope| {
        let running: Vec<_> = probes
            .iter()
            .map(|(need, resolved)| {
                scope.spawn(move || match resolved {
                    Some(program) => probe(need, program, path, deadline),
                    None => missing(need, login, true),
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
    })
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
    let probes: Vec<(&Need, Option<&Path>)> = due
        .iter()
        .map(|(need, resolved, _)| (*need, resolved.as_deref()))
        .collect();
    let outcomes = run_all(&probes, path, login.as_deref(), Instant::now() + BUDGET);
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

static DISPATCHER: Checker = Checker::new(CHECK_EVERY, needed);

/// A process's checks of its home: started at most once per `every`, at
/// most one at a time.
pub(crate) struct Checker {
    every: Duration,
    /// What the home needs: `needed`, but for tests.
    needs: fn(&Store) -> Result<Needs>,
    last: std::sync::Mutex<Option<Instant>>,
    running: std::sync::Mutex<Option<std::thread::JoinHandle<Value>>>,
}

impl Checker {
    const fn new(every: Duration, needs: fn(&Store) -> Result<Needs>) -> Self {
        Checker {
            every,
            needs,
            last: std::sync::Mutex::new(None),
            running: std::sync::Mutex::new(None),
        }
    }

    /// Collect a finished check, start one when due, and report the stored
    /// rows. Null when there is nothing new to say.
    fn tick(&self, store: &Store, path: &str, start: bool) -> Value {
        // A panic while either was held leaves nothing half-written: each
        // is one value, so a poisoned lock is taken as it stands.
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
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
            let needs = self.needs;
            let spawned = std::thread::Builder::new()
                .name("readiness".into())
                .spawn(move || {
                    Store::open(&home)
                        .and_then(|store| {
                            let needs = needs(&store)?;
                            check_with(&store, &path, false, &needs, &crate::service::login_path)
                        })
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
    /// Where the tool was found; `None` when it was not.
    path: Option<String>,
    checked_ms: i64,
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
        let of = |prefix: &str| -> Vec<&str> {
            self.needed_by
                .iter()
                .filter_map(|who| who.strip_prefix(prefix))
                .collect()
        };
        let (routes, hosted, links) = (of("route:"), of(HOSTED), of(LINKED));
        let ships: Vec<&str> = self
            .needed_by
            .iter()
            .map(String::as_str)
            .filter(|who| {
                !["route:", HOSTED, LINKED]
                    .iter()
                    .any(|p| who.starts_with(p))
            })
            .collect();
        let mut parts = vec![];
        if !ships.is_empty() {
            parts.push(format!(
                "the first mate of {} cannot wake",
                ships.join(", ")
            ));
        }
        if !hosted.is_empty() {
            parts.push(format!(
                "the first mate of {} falls back to headless wakes",
                hosted.join(", ")
            ));
        }
        if !routes.is_empty() {
            let noun = if routes.len() == 1 { "route" } else { "routes" };
            parts.push(format!("{noun} {} cannot launch", routes.join(", ")));
        }
        if !links.is_empty() {
            let noun = if links.len() == 1 {
                "machine"
            } else {
                "machines"
            };
            parts.push(format!(
                "linked {noun} {} cannot be reached",
                links.join(", ")
            ));
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
            "SELECT tool, level, COALESCE(why, ''), install, needed_by, fingerprint, path,
               checked_ms
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
                path: r.get(6)?,
                checked_ms: r.get(7)?,
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

/// A tool a harness manifest requires that the last check found missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Missing {
    pub(crate) tool: String,
    pub(crate) install: String,
    /// Why a tool that was found fails its check; `None` when it was not
    /// found at all.
    pub(crate) failed: Option<String>,
}

/// The tools `manifest` requires that the last check found failing, read
/// from the stored rows without probing. A tool with no row yet (never
/// checked here) counts as installed, as does everything a manifest that
/// declares no `requires` runs, and so does a failing row older than
/// `STALE_FAIL_MS`, which no checker is refreshing.
pub(crate) fn missing_for(conn: &Connection, manifest: &str) -> Result<Vec<Missing>> {
    let Some(names) = requires(manifest) else {
        return Ok(vec![]);
    };
    let now = Store::now_ms();
    Ok(failing(conn)?
        .into_iter()
        .filter(|row| names.contains(&row.tool) && now - row.checked_ms < STALE_FAIL_MS)
        .map(|row| Missing {
            tool: row.tool,
            install: row.install,
            failed: row.path.is_some().then_some(row.why),
        })
        .collect())
}

/// What the harnesses `manifests` (each with who needs it) require that is
/// not on `path`, this command's PATH, now, each with why and its install
/// hint, and which manifests declare nothing to check. Checked live on the
/// bounded probes, asking the login shell (bounded too) only when something
/// is missing; the rows are left to the dispatcher, whose PATH may differ
/// from this one and whose own check follows within a minute.
pub(crate) fn harness_report(
    manifests: &[(String, String)],
    path: &str,
    login: &dyn Fn() -> String,
) -> Value {
    let mut needs = Needs::default();
    for (who, manifest) in manifests {
        needs.harness(manifest, false, who);
    }
    let resolved: Vec<(&Need, Option<PathBuf>)> = needs
        .tools
        .iter()
        .map(|need| (need, resolve(&need.binary, path)))
        .collect();
    let login = resolved
        .iter()
        .any(|(_, found)| found.is_none())
        .then(login);
    let deadline = Instant::now() + BUDGET;
    let missing: Vec<Value> = resolved
        .iter()
        .filter_map(|(need, found)| {
            let outcome = match found {
                Some(program) => probe(need, program, path, deadline),
                None => missing(need, login.as_deref(), false),
            };
            (!outcome.ok).then(|| {
                let why = format!(
                    "{}; the dispatcher's own check follows within a minute",
                    outcome.why.as_deref().unwrap_or("the check failed")
                );
                json!({"tool": need.tool, "why": why, "install": need.install,
                    "needed_by": need.needed_by})
            })
        })
        .collect();
    json!({"missing": missing, "unchecked": needs.unchecked})
}

/// `harness_report` on this process's PATH, the one a dispatcher started
/// from here records, for routes.set and a first-mate profile's ship.put.
pub(crate) fn harness_report_here(manifests: &[(String, String)]) -> Value {
    let path = std::env::var("PATH").unwrap_or_default();
    harness_report(manifests, &path, &crate::service::login_path)
}

/// Bearings lines for `ship`: each failing tool its own work needs.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    Ok(failing(store.connection())?
        .iter()
        .filter(|row| {
            row.needed_by
                .iter()
                .any(|who| who == ship || who == ALL || who.strip_prefix(HOSTED) == Some(ship))
        })
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
pub(crate) fn ensure_summary(
    home: &Path,
    path: Option<&str>,
    python: Option<&str>,
) -> Option<String> {
    if !home.join("privateer.sqlite").is_file() {
        return None;
    }
    let store = Store::open(home).ok()?;
    if let Some(path) = path {
        let mut needs = needed(&store).ok()?;
        if let Some(python) = python {
            for need in &mut needs.tools {
                if need.binary == "python3" {
                    need.binary = python.into();
                }
            }
        }
        check_with(&store, path, true, &needs, &crate::service::login_path).ok()?;
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

/// This machine's readiness, for the gateway's `machine.readiness`: for
/// each of `homes`, its stored rows while they are within their TTL (its
/// dispatcher keeps them so), or else a live check on the PATH a dispatcher
/// started here records, bounded and stored nowhere. Read-only: each home is
/// opened read-only and nothing is written. Paths are reported as they are.
pub(crate) fn machine_report(homes: &[PathBuf]) -> Value {
    let path = std::sync::OnceLock::new();
    machine_report_with(
        homes,
        &needed,
        &|| path.get_or_init(crate::service::login_path).clone(),
        REPORT_BUDGET,
    )
}

/// What a home needs, as `needed` reads it.
type NeedsOf<'a> = &'a dyn Fn(&Store) -> Result<Needs>;

/// `path` is asked once a home needs a live check; all live checks share
/// `budget`, counted from then.
fn machine_report_with(
    homes: &[PathBuf],
    needs: NeedsOf,
    path: &dyn Fn() -> String,
    budget: Duration,
) -> Value {
    let mut seen: Vec<&PathBuf> = vec![];
    let mut reports = vec![];
    let mut merged: Vec<Row> = vec![];
    let mut live = Live {
        deadline: None,
        budget,
        probed: vec![],
    };
    for home in homes {
        if seen.contains(&home) || !home.join("privateer.sqlite").is_file() {
            continue;
        }
        seen.push(home);
        match home_rows(home, needs, path, &mut live) {
            Ok((source, rows)) => {
                let tools: Vec<&str> = rows.iter().map(|row| row.tool.as_str()).collect();
                reports.push(json!({"home": home, "source": source, "failing": tools}));
                for row in rows {
                    merge(&mut merged, row);
                }
            }
            Err(error) => reports.push(json!({"home": home, "error": format!("{error:#}")})),
        }
    }
    merged.sort_by(|a, b| (&a.level, &a.tool).cmp(&(&b.level, &b.tool)));
    let failing: Vec<Value> = merged
        .iter()
        .map(|row| {
            let mut shown = row.to_json();
            shown["summary"] = json!(row.sentence());
            shown
        })
        .collect();
    json!({"machine": crate::machines::this_machine(), "homes": reports, "failing": failing})
}

/// What the live checks of one report share: one deadline, set by the
/// first, and each probe's outcome, so a tool several homes need on the same
/// PATH is probed once.
struct Live {
    deadline: Option<Instant>,
    budget: Duration,
    probed: Vec<((String, PathBuf, Probe), Outcome)>,
}

/// One home's failing rows, and whether they are its stored ones, were
/// checked live because they were missing or older than their TTL, or were
/// left unchecked because the report's time ran out before this home.
fn home_rows(
    home: &Path,
    needs: NeedsOf,
    path: &dyn Fn() -> String,
    live: &mut Live,
) -> Result<(&'static str, Vec<Row>)> {
    let store = Store::open_readonly(home, crate::store::Caller::Operator)?;
    let needs = needs(&store)?;
    let conn = store.connection();
    let now = Store::now_ms();
    let known: BTreeMap<String, (bool, i64)> = if available(conn)? {
        conn.prepare("SELECT tool, ok, checked_ms FROM machine_readiness")?
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        BTreeMap::new()
    };
    let fresh = needs.tools.iter().all(|need| {
        known
            .get(&need.tool)
            .is_some_and(|(ok, at)| now - at < if *ok { PASS_TTL_MS } else { FAIL_TTL_MS })
    });
    if fresh {
        return Ok(("stored", failing(conn)?));
    }
    // Past the shared deadline every probe would be cut short at once, which
    // says nothing about this home's tools.
    if live
        .deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Ok(("unchecked", vec![]));
    }
    let path = path();
    let budget = live.budget;
    let deadline = *live.deadline.get_or_insert_with(|| Instant::now() + budget);
    let resolved: Vec<(&Need, Option<PathBuf>)> = needs
        .tools
        .iter()
        .map(|need| (need, resolve(&need.binary, &path)))
        .collect();
    let known = |need: &Need, found: &Option<PathBuf>| {
        let found = found.as_ref()?;
        live.probed
            .iter()
            .find(|((tool, program, probe), _)| {
                *tool == need.tool && program == found && *probe == need.probe
            })
            .map(|(_, outcome)| outcome.clone())
    };
    let cached: Vec<Option<Outcome>> = resolved
        .iter()
        .map(|(need, found)| known(need, found))
        .collect();
    let probes: Vec<(&Need, Option<&Path>)> = resolved
        .iter()
        .zip(&cached)
        .filter(|(_, cached)| cached.is_none())
        .map(|((need, found), _)| (*need, found.as_deref()))
        .collect();
    // `path` already holds the login shell's PATH, so it is not asked again.
    let mut fresh = run_all(&probes, &path, None, deadline).into_iter();
    let mut outcomes = vec![];
    for ((need, found), cached) in resolved.iter().zip(cached) {
        let outcome = match cached {
            Some(outcome) => outcome,
            None => {
                let outcome = fresh.next().context("a probe gave no outcome")?;
                if let Some(found) = found {
                    live.probed.push((
                        (need.tool.clone(), found.clone(), need.probe),
                        outcome.clone(),
                    ));
                }
                outcome
            }
        };
        outcomes.push(outcome);
    }
    let mut rows: Vec<Row> = needs
        .tools
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| !outcome.ok)
        .map(|(need, outcome)| Row {
            tool: need.tool.clone(),
            level: if need.block { "block" } else { "warn" }.into(),
            why: outcome.why.unwrap_or_default(),
            install: need.install.clone(),
            needed_by: need.needed_by.clone(),
            fingerprint: String::new(),
            path: outcome.path,
            checked_ms: now,
        })
        .collect();
    rows.sort_by(|a, b| (&a.level, &a.tool).cmp(&(&b.level, &b.tool)));
    Ok(("live", rows))
}

/// One row per tool across homes: blocking if it blocks in any, needed by
/// everyone who needs it in each.
fn merge(rows: &mut Vec<Row>, row: Row) {
    match rows.iter_mut().find(|known| known.tool == row.tool) {
        Some(known) => {
            if row.level == "block" {
                known.level = row.level;
            }
            for who in row.needed_by {
                if !known.needed_by.contains(&who) {
                    known.needed_by.push(who);
                }
            }
            known.needed_by.sort();
        }
        None => rows.push(row),
    }
}

/// What `machine add` says of the machine it linked, from that machine's
/// `machine.readiness` answer: each failing tool with its install step. The
/// answer is another machine's, so only its known fields are kept, bounded.
pub(crate) fn linked_summary(report: &Value) -> Value {
    let text = |value: &Value| {
        value
            .as_str()
            .map(|s| s.chars().take(1_000).collect::<String>())
    };
    let failing: Vec<Value> = report["failing"]
        .as_array()
        .into_iter()
        .flatten()
        .take(50)
        .filter_map(|row| {
            let tool = text(&row["tool"])?;
            let (why, install) = (text(&row["why"]), text(&row["install"]));
            let summary = text(&row["summary"]).unwrap_or_else(|| {
                format!(
                    "{}. Install: {}.",
                    why.as_deref().unwrap_or("it fails its check"),
                    install.as_deref().unwrap_or("unknown")
                )
            });
            let needed_by: Vec<String> = row["needed_by"]
                .as_array()
                .into_iter()
                .flatten()
                .take(50)
                .filter_map(text)
                .collect();
            Some(
                json!({"tool": tool, "level": text(&row["level"]), "why": why,
                "install": install, "needed_by": needed_by, "summary": summary}),
            )
        })
        .collect();
    let homes = report["homes"].as_array().map_or(&[][..], Vec::as_slice);
    let unread = homes
        .iter()
        .filter(|home| home.get("error").is_some())
        .count();
    let mut parts: Vec<String> = failing
        .iter()
        .map(|row| {
            format!(
                "{} ({}): {}",
                row["tool"].as_str().unwrap_or_default(),
                row["level"].as_str().unwrap_or("unknown"),
                row["summary"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if unread > 0 {
        parts.push(format!("{unread} of its homes could not be read"));
    }
    let unchecked = homes
        .iter()
        .filter(|home| home["source"] == "unchecked")
        .count();
    if unchecked > 0 {
        parts.push(format!("{unchecked} of its homes were not checked in time"));
    }
    let status = if !failing.is_empty() {
        "failing"
    } else if unread > 0 || unchecked > 0 || homes.is_empty() {
        "unknown"
    } else {
        "ready"
    };
    let summary = match status {
        "ready" => "readiness: ready".to_owned(),
        "unknown" if homes.is_empty() => "readiness: unknown (it has no ship homes)".to_owned(),
        "unknown" => format!("readiness: unknown ({})", parts.join("; ")),
        _ => format!("readiness: {}", parts.join("; ")),
    };
    json!({"status": status, "failing": failing, "summary": summary})
}

/// The same when the linked machine could not say: too old to know
/// `machine.readiness`, or unreachable for it. `why` may carry the remote's
/// whole standard error, so only its start is kept.
pub(crate) fn linked_unknown(why: &str) -> Value {
    let why = why.trim();
    let why = if why.chars().count() > UNKNOWN_CHARS {
        format!("{}…", why.chars().take(UNKNOWN_CHARS).collect::<String>())
    } else {
        why.to_owned()
    };
    json!({"status": "unknown", "failing": [], "summary": format!("readiness: unknown ({why})")})
}

/// The most of a linked machine's error a summary keeps.
const UNKNOWN_CHARS: usize = 300;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn python_readiness_uses_the_pinned_program_but_other_tools_keep_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join("alpha-python");
        std::fs::write(&python, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            resolve_with_python("python3", "/alpha/shims", Some(python.as_os_str())),
            Some(python.clone())
        );
        assert_eq!(resolve_with_python("python3", "/alpha/shims", None), None);
        assert_eq!(
            resolve_with_python("beta", "/alpha/shims", Some(python.as_os_str())),
            None
        );
    }

    #[test]
    fn python_readiness_resolves_and_executes_non_utf8_recorded_paths() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join(std::ffi::OsStr::from_bytes(b"python-\xff"));
        std::fs::write(&python, "#!/bin/sh\nprintf 'Python 3.12.7\\n'\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let resolved =
            resolve_with_python("python3", "/alpha/shims", Some(python.as_os_str())).unwrap();
        assert_eq!(resolved, python);
        let need = Need {
            tool: "python3".into(),
            binary: "python3".into(),
            probe: Probe::Version("--version"),
            block: false,
            needed_by: vec![],
            install: String::new(),
        };
        let outcome = probe(
            &need,
            &resolved,
            "/alpha/shims",
            Instant::now() + Duration::from_secs(5),
        );
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.version.as_deref(), Some("3.12.7"));
    }

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

        /// A local ship whose first mate Herdr hosts, running `manifest`.
        fn hosted(&self, name: &str, manifest: &str) {
            let record = json!({"name": name, "location": "here",
                "first_mate": {"manifest": manifest, "duty_task": "duty", "host": "herdr"}});
            self.store
                .connection()
                .execute(
                    "INSERT OR REPLACE INTO ships(name, record) VALUES(?1, ?2)",
                    params![name, record.to_string()],
                )
                .unwrap();
        }

        /// A check, as a machine that reaches `links` over SSH makes it.
        fn check_links(&self, links: &[&str]) -> Value {
            let links: Vec<String> = links.iter().map(|l| (*l).to_owned()).collect();
            let needs = needed_from(&self.store, None, &links, "ssh").unwrap();
            check_with(&self.store, &self.path(), false, &needs, &String::new).unwrap()
        }

        fn rows_of(&self, tool: &str) -> i64 {
            self.store
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM machine_readiness WHERE tool=?1",
                    [tool],
                    |r| r.get(0),
                )
                .unwrap()
        }

        /// The machine report over `homes`, needing what a check without
        /// links does, its live checks on the test PATH.
        fn report(&self, homes: &[PathBuf], asked: &std::sync::atomic::AtomicUsize) -> Value {
            let path = || {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.path()
            };
            machine_report_with(
                homes,
                &|store: &Store| needed_from(store, None, &[], "ssh"),
                &path,
                Duration::from_secs(10),
            )
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
            let needs = needed_from(&self.store, policy, &[], "ssh").unwrap();
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
    fn route_choice_reads_a_harness_as_missing_only_from_a_failing_row() {
        let m = Machine::new();
        let manifest = m.manifest("fake", Some(&["fakeharness", "git"]));
        let custom = m.manifest("custom", None);
        let conn = m.store.connection();
        // Never checked: installed, as before readiness.
        assert!(missing_for(conn, &manifest).unwrap().is_empty());
        let policy = json!({"routes": {"cheap": {"manifest": manifest, "cost": "free"}}});
        m.check_on(&m.path(), false, Some(&policy), "");
        assert_eq!(
            missing_for(conn, &manifest).unwrap(),
            [Missing {
                tool: "fakeharness".into(),
                install: "install fakeharness so it is on the dispatcher's PATH".into(),
                failed: None,
            }]
        );
        assert!(missing_for(conn, &custom).unwrap().is_empty());
        assert!(
            missing_for(conn, "bundled:no-such-thing")
                .unwrap()
                .is_empty()
        );
        m.tool("fakeharness", "");
        m.check_on(&m.path(), false, Some(&policy), "");
        assert!(missing_for(conn, &manifest).unwrap().is_empty());
        // Installed but failing its check: the row's own why, not "missing".
        m.tool("git", "echo 'git version 2.20.0'");
        m.check_on(&m.path(), false, Some(&policy), "");
        let found = missing_for(conn, &manifest).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].tool, "git");
        let failed = found[0].failed.as_deref().unwrap();
        assert!(
            failed.starts_with("git 2.20.0 at ") && failed.contains("is incompatible"),
            "{failed}"
        );
        // A failing row no checker has rewritten for STALE_FAIL_MS is ignored.
        conn.execute(
            "UPDATE machine_readiness SET checked_ms=checked_ms-?1",
            [STALE_FAIL_MS],
        )
        .unwrap();
        assert!(missing_for(conn, &manifest).unwrap().is_empty());
    }

    #[test]
    fn a_harness_report_checks_live_and_stores_nothing() {
        let m = Machine::new();
        let fake = m.manifest("fake", Some(&["fakeharness", "git"]));
        let custom = m.manifest("custom", None);
        let login = m.root.join("login-bin");
        std::fs::create_dir(&login).unwrap();
        let file = login.join("fakeharness");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let manifests = [
            ("route:cheap".to_owned(), fake.clone()),
            ("alpha".to_owned(), fake),
            ("route:own".to_owned(), custom.clone()),
        ];
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let login_path = || {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            login.to_string_lossy().into_owned()
        };
        let report = harness_report(&manifests, &m.path(), &login_path);
        assert_eq!(report["unchecked"], json!([custom]), "{report}");
        let missing = report["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 1, "{report}");
        assert_eq!(missing[0]["tool"], "fakeharness");
        assert_eq!(missing[0]["needed_by"], json!(["alpha", "route:cheap"]));
        assert_eq!(
            missing[0]["install"],
            "install fakeharness so it is on the dispatcher's PATH"
        );
        assert_eq!(
            missing[0]["why"],
            format!(
                "fakeharness is found at {} in your login shell but not on this command's PATH; \
                 the dispatcher's own check follows within a minute",
                file.display()
            ),
            "{report}"
        );
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(m.failing_tools().is_empty());
        // Everything on the PATH: nothing missing, and no login shell asked.
        m.tool("fakeharness", "");
        let report = harness_report(&manifests, &m.path(), &login_path);
        assert_eq!(report["missing"], json!([]), "{report}");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(m.calls("fakeharness"), 0, "a presence check runs nothing");
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

    /// What a home needs on a machine with no links, whatever registry the
    /// test runs beside.
    fn unlinked(store: &Store) -> Result<Needs> {
        needed_from(store, None, &[], "ssh")
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
        let checker = Checker::new(Duration::ZERO, unlinked);
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
        let checker = Checker::new(Duration::ZERO, unlinked);
        let first = checker.tick(&m.store, &m.path(), true);
        let second = checker.tick(&m.store, &m.path(), true);
        assert_eq!(first["started"], true, "{first}");
        assert_eq!(second["started"], false, "{second}");
        assert_eq!(second["running"], true, "{second}");
        finished(&checker, &m);
        assert_eq!(m.calls("git"), 1);
    }

    #[test]
    fn a_poisoned_checker_still_reports_and_checks() {
        let m = Machine::new();
        m.tool("git", "echo 'git version 2.20.0'");
        m.check();
        let checker = Checker::new(Duration::ZERO, unlinked);
        // A panic while the lock was held poisons it.
        std::thread::scope(|scope| {
            let _ = scope
                .spawn(|| {
                    let _held = checker.last.lock().unwrap();
                    panic!("poison the lock");
                })
                .join();
        });
        assert!(checker.last.is_poisoned());
        let report = checker.tick(&m.store, &m.path(), true);
        assert_eq!(report["started"], true, "{report}");
        assert_eq!(report["failing"][0]["tool"], "git", "{report}");
        finished(&checker, &m);
    }

    #[test]
    fn a_one_pass_dispatcher_reports_stored_rows_without_starting_a_check() {
        let m = Machine::new();
        m.tool("git", "echo 'git version 2.20.0'");
        m.check();
        assert_eq!(m.calls("git"), 1);
        let checker = Checker::new(Duration::ZERO, unlinked);
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
        let shell = crate::service::tests::script(
            &shell,
            &format!(
                "#!/bin/sh\nsleep 10 &\necho $$ $! > '{}'\nwait\n",
                pids.display()
            ),
        );
        let bound = Duration::from_secs(1);
        let begun = Instant::now();
        let (path, why) = crate::service::login_path_from(Some(&shell), Instant::now() + bound);
        assert!(
            begun.elapsed() < Duration::from_secs(3),
            "the login shell held the lookup {:?}",
            begun.elapsed()
        );
        assert_eq!(why.as_deref(), Some("the login shell timed out"));
        // Only this process's PATH is left.
        assert_eq!(
            path,
            crate::service::login_path_from(None, Instant::now()).0
        );
        // The shell and what it started were killed. An orphan is reaped by
        // whatever adopted it, which may be slow to: a zombie is dead.
        for pid in std::fs::read_to_string(&pids).unwrap().split_whitespace() {
            let pid: u32 = pid.parse().unwrap();
            let until = Instant::now() + Duration::from_secs(3);
            while crate::jobs::process_identity(pid).unwrap().is_some() {
                assert!(Instant::now() < until, "{pid} outlived its deadline");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // A check that asks it records the missing tool instead of hanging.
        let manifest = m.manifest("fake", Some(&["privateer-no-such-harness"]));
        m.ship("alpha", &manifest);
        let needs = needed_from(&m.store, None, &[], "ssh").unwrap();
        let begun = Instant::now();
        check_with(&m.store, &m.path(), true, &needs, &|| {
            crate::service::login_path_from(Some(&shell), Instant::now() + bound).0
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
    fn herdr_is_needed_only_for_a_hosted_first_mate_and_only_warns() {
        let m = Machine::new();
        let custom = m.manifest("custom", None);
        m.ship("alpha", &custom);
        m.check();
        assert_eq!(m.rows_of("herdr"), 0, "no hosted first mate, no herdr");

        m.hosted("beta", &custom);
        let report = m.check();
        assert_eq!(m.failing_tools(), ["herdr"], "{report}");
        let rows = failing(m.store.connection()).unwrap();
        assert_eq!(rows[0].level, "warn");
        assert_eq!(rows[0].needed_by, ["host:beta"]);
        assert_eq!(m.why("herdr"), "herdr is not on the dispatcher's PATH");
        // A warning raises no owner alert, and blocks nothing.
        assert!(owner_problems(&m.store).unwrap().is_empty());
        assert!(
            missing_for(m.store.connection(), &custom)
                .unwrap()
                .is_empty()
        );
        let lines = bearings_lines(&m.store, "beta").unwrap();
        assert_eq!(
            lines,
            [
                "Machine: herdr is not on the dispatcher's PATH, so the first mate of beta \
              falls back to headless wakes. Install: install Herdr so herdr is on the \
              dispatcher's PATH."
            ]
        );
        assert!(bearings_lines(&m.store, "alpha").unwrap().is_empty());
        let needs = m.needs("", 40);
        assert_eq!(needs["items"][0]["source_id"], "herdr", "{needs}");
        assert_eq!(needs["items"][0]["status"], "warn", "{needs}");

        // Presence plus its version.
        m.tool("herdr", "echo 'herdr 0.4.2'");
        m.check();
        assert!(m.failing_tools().is_empty());
        let version: Option<String> = m
            .store
            .connection()
            .query_row(
                "SELECT version FROM machine_readiness WHERE tool='herdr'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version.as_deref(), Some("0.4.2"));
        assert_eq!(m.calls("herdr"), 1);
        // One that cannot even print its version is failing, still a warning.
        m.tool("herdr", "exit 3");
        m.check();
        assert_eq!(m.failing_tools(), ["herdr"]);
        assert_eq!(
            m.why("herdr"),
            "`herdr --version` failed: herdr exited with 3"
        );
        assert!(owner_problems(&m.store).unwrap().is_empty());
    }

    #[test]
    fn ssh_is_needed_only_with_links_and_only_warns() {
        let m = Machine::new();
        m.check_links(&[]);
        assert_eq!(m.rows_of("ssh"), 0, "no links, no ssh");

        let report = m.check_links(&["mac", "server"]);
        assert_eq!(m.failing_tools(), ["ssh"], "{report}");
        let rows = failing(m.store.connection()).unwrap();
        assert_eq!(rows[0].level, "warn");
        assert_eq!(rows[0].needed_by, ["machine:mac", "machine:server"]);
        assert_eq!(
            rows[0].sentence(),
            "ssh is not on the dispatcher's PATH, so linked machines mac, server cannot be \
             reached. Install: install an OpenSSH client so ssh is on the dispatcher's PATH."
        );
        assert!(owner_problems(&m.store).unwrap().is_empty());
        // A machine's links are no ship's own work.
        assert!(bearings_lines(&m.store, "mac").unwrap().is_empty());
        assert_eq!(m.needs("", 40)["items"][0]["status"], "warn");

        // OpenSSH prints its version on standard error and has no --version.
        m.tool("ssh", "[ \"$1\" = -V ] && echo 'OpenSSH_9.8p1' >&2");
        m.check_links(&["mac"]);
        assert!(m.failing_tools().is_empty());
        assert_eq!(
            std::fs::read_to_string(m.root.join("ssh.log")).unwrap(),
            "-V\n"
        );
        // Unlinked again, the row leaves.
        m.check_links(&[]);
        assert_eq!(m.rows_of("ssh"), 0);
    }

    #[test]
    fn the_machine_report_reads_fresh_rows_and_checks_old_ones_live_storing_nothing() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.check();
        assert_eq!((m.calls("git"), m.failing_tools().len()), (1, 1));
        let home = m.root.join("home");
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let calls = || asked.load(std::sync::atomic::Ordering::SeqCst);
        // Listed twice, and beside a folder that is no home: one home read.
        let homes = [home.clone(), home.clone(), m.root.join("nowhere")];

        let report = m.report(&homes, &asked);
        assert_eq!(report["homes"].as_array().unwrap().len(), 1, "{report}");
        assert_eq!(report["homes"][0]["source"], "stored", "{report}");
        assert_eq!(report["homes"][0]["home"], json!(home));
        assert_eq!(report["homes"][0]["failing"], json!(["treehouse"]));
        let row = &report["failing"][0];
        assert_eq!(row["tool"], "treehouse", "{report}");
        assert_eq!(row["level"], "block");
        assert_eq!(row["needed_by"], json!(["storefront"]));
        assert!(
            row["summary"]
                .as_str()
                .unwrap()
                .ends_with("Install: install Treehouse 3.1.2 or later, see https://github.com/kunchenguid/treehouse#install."),
            "{report}"
        );
        assert!(report["machine"].is_string());
        // Fresh rows: nothing probed, no PATH looked up.
        assert_eq!((m.calls("git"), calls()), (1, 0));

        // Rows older than their TTL are checked again, live, on the PATH a
        // dispatcher here records; the rows stay as the dispatcher left them.
        m.treehouse("v3.1.2", HELP);
        m.store
            .connection()
            .execute(
                "UPDATE machine_readiness SET checked_ms=checked_ms-?1",
                [PASS_TTL_MS],
            )
            .unwrap();
        let report = m.report(&homes, &asked);
        assert_eq!(report["homes"][0]["source"], "live", "{report}");
        assert_eq!(report["failing"], json!([]), "{report}");
        assert_eq!((m.calls("git"), calls()), (2, 1));
        assert_eq!(
            m.failing_tools(),
            ["treehouse"],
            "the report stored nothing"
        );

        // A home never checked is checked live too, and its why is the probe's.
        let other = Machine::new();
        other.tool("git", "echo 'git version 2.20.0'");
        let report = other.report(&[other.root.join("home")], &asked);
        assert_eq!(report["homes"][0]["source"], "live", "{report}");
        assert_eq!(report["failing"][0]["tool"], "git", "{report}");
        assert_eq!(other.rows_of("git"), 0);
    }

    #[test]
    fn homes_after_the_reports_deadline_read_unchecked_not_failing() {
        let m = Machine::new();
        m.tool("git", "/bin/sleep 2; echo 'git version 2.43.0'");
        let other = Machine::new();
        let homes = [m.root.join("home"), other.root.join("home")];
        let path = m.path();
        let report = machine_report_with(
            &homes,
            &|store: &Store| needed_from(store, None, &[], "ssh"),
            &|| path.clone(),
            Duration::from_millis(300),
        );
        assert_eq!(report["homes"][0]["source"], "live", "{report}");
        assert_eq!(report["homes"][1]["source"], "unchecked", "{report}");
        assert_eq!(report["homes"][1]["failing"], json!([]), "{report}");
        // Only the first home's probe, cut short, fails.
        assert_eq!(report["homes"][0]["failing"], json!(["git"]), "{report}");
        let unread = linked_summary(&json!({"homes": [{"home": "/h", "source": "unchecked",
            "failing": []}], "failing": []}));
        assert_eq!(unread["status"], "unknown", "{unread}");
        assert_eq!(
            unread["summary"],
            "readiness: unknown (1 of its homes were not checked in time)"
        );
    }

    #[test]
    fn a_tool_several_homes_need_is_probed_once_per_report() {
        let m = Machine::new();
        let other = Machine::new();
        let homes = [m.root.join("home"), other.root.join("home")];
        let path = m.path();
        let report = machine_report_with(
            &homes,
            &|store: &Store| needed_from(store, None, &[], "ssh"),
            &|| path.clone(),
            Duration::from_secs(10),
        );
        assert_eq!(report["homes"][0]["source"], "live", "{report}");
        assert_eq!(report["homes"][1]["source"], "live", "{report}");
        assert_eq!(m.calls("git"), 1, "{report}");
    }

    #[test]
    fn a_linked_machines_report_is_summarized_with_its_install_steps() {
        let m = Machine::new();
        m.git_charter("storefront");
        m.check();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let report = m.report(&[m.root.join("home")], &asked);
        let summary = linked_summary(&report);
        assert_eq!(summary["status"], "failing", "{summary}");
        assert_eq!(summary["failing"][0]["tool"], "treehouse");
        let text = summary["summary"].as_str().unwrap();
        assert!(
            text.starts_with(
                "readiness: treehouse (block): treehouse is not on the dispatcher's PATH"
            ) && text.contains("Install: install Treehouse"),
            "{text}"
        );
        assert_eq!(
            linked_summary(&json!({"homes": [{"home": "/h", "failing": []}], "failing": []}))["summary"],
            "readiness: ready"
        );
        let unread =
            linked_summary(&json!({"homes": [{"home": "/h", "error": "locked"}], "failing": []}));
        assert_eq!(unread["status"], "unknown", "{unread}");
        assert_eq!(linked_summary(&json!({}))["status"], "unknown");
        assert_eq!(
            linked_unknown("remote is older")["summary"],
            "readiness: unknown (remote is older)"
        );
        // A remote's whole standard error is cut to its start.
        let long = format!("ssh: connect failed\n{}", "x".repeat(65_536));
        let summary = linked_unknown(&long)["summary"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            summary.starts_with("readiness: unknown (ssh: connect failed\nxx"),
            "{summary}"
        );
        assert!(summary.ends_with("…)"), "{summary}");
        assert!(summary.chars().count() < 400, "{}", summary.len());
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
