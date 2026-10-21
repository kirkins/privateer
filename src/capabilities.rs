//! What ships need from a machine, and what this machine offers.
//!
//! The captain keeps a vocabulary of capabilities (`xcode`, `kubectl`,
//! `login-claude`) in the fleet registry: a name, a note and at most one
//! probe. A ship declares the names it requires (`ship.requires`); a machine
//! offers a name when its probe succeeds here or the operator declared it,
//! unless the operator withheld it (`machine.offers`). The vocabulary is
//! defined on the linking machine and travels to the machines it reaches in
//! the directory exchange, where the copy it was sent is kept. A machine
//! linked by two keeps one linker's copy: of those heard within the last
//! hour, the first by name (so a later linker whose name sorts earlier takes
//! over); when none was heard lately, the one heard last. A linker that
//! stopped exchanging, such as one that removed the link only on its side,
//! goes quiet and stops defining.
//!
//! A probe is a binary on the login shell's PATH, a path that must exist, or
//! an argv. An argv probe runs with a cleared environment holding only `HOME`
//! and the login `PATH`, no stdin and its output discarded, in its own
//! process group, which is killed when the probe exits or outlives its five
//! seconds. Success is exit 0. A machine runs only the probes of its own
//! stored vocabulary: what arrives to be checked carries names, never probes.
//!
//! Detection is cached for at most ten minutes, for planning and for the
//! exchange; a move's preflight always probes live. The exchange probes
//! only within a short budget, and a probe it cuts short is not cached: a
//! dispatcher pass refreshes the cache with each probe's full time.
use crate::store::{Store, identifier, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The most capabilities one ship may require.
pub(crate) const MAX_REQUIRES: usize = 16;
/// The most terms a fleet's vocabulary holds.
const MAX_TERMS: usize = 128;
/// A note's length, in characters.
const MAX_NOTE: usize = 200;
/// An argv probe's length, and each argument's.
const MAX_ARGV: usize = 16;
const MAX_ARG: usize = 512;
/// One argv probe's limit.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Every probe of one check, together; well inside a move's slow call.
const CHECK_BUDGET: Duration = Duration::from_secs(30);
/// Probes refreshed for a directory exchange, together: a dispatcher pass
/// gives the whole exchange ten seconds, and the cache is only for planning.
const EXCHANGE_BUDGET: Duration = Duration::from_secs(2);
/// Probes run at once.
const PARALLEL: usize = 16;
/// How long a detected offer is trusted for planning.
const CACHE_MS: i64 = 600_000;
/// A linking machine unheard this long no longer defines the vocabulary
/// while another one is heard; each exchanges every five minutes.
const QUIET_MS: i64 = 3_600_000;
/// Why a probe stopped when the check's time, not its own, ran out: not an
/// answer, so never cached.
const CUT_SHORT: &str = "probe not finished within the check's time";
const NOT_RUN: &str = "probe not run: the check's time ran out";

pub(crate) fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS capabilities(name TEXT PRIMARY KEY,
           note TEXT NOT NULL DEFAULT '', probe TEXT, updated_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS offer_declarations(name TEXT PRIMARY KEY,
           stance TEXT NOT NULL CHECK(stance IN ('declared','withheld')), set_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS machine_offers(name TEXT PRIMARY KEY,
           met INTEGER NOT NULL, why TEXT, probed_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// How a machine detects a capability.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Probe {
    /// A binary on the login shell's PATH.
    Path(String),
    /// A path that exists.
    File(PathBuf),
    /// A command that exits 0.
    Argv(Vec<String>),
}

impl Probe {
    fn to_json(&self) -> Value {
        match self {
            Probe::Path(binary) => json!({"path": binary}),
            Probe::File(path) => json!({"file": path}),
            Probe::Argv(argv) => json!({"argv": argv}),
        }
    }

    /// A probe as stored or carried: one of path, file or argv.
    fn parse(value: &Value) -> Result<Option<Probe>> {
        if value.is_null() {
            return Ok(None);
        }
        let fields = value.as_object().filter(|o| o.len() == 1).context(
            "a probe is one of {\"path\": BINARY}, {\"file\": PATH} or {\"argv\": [...]}",
        )?;
        let (kind, detail) = fields.iter().next().context("empty probe")?;
        let clean = |text: &str| !text.is_empty() && !text.chars().any(char::is_control);
        Ok(Some(match kind.as_str() {
            "path" => {
                let binary = detail.as_str().context("a path probe names a binary")?;
                ensure!(
                    clean(binary) && !binary.contains('/') && binary.len() <= MAX_ARG,
                    "a path probe names one binary, not a path"
                );
                Probe::Path(binary.to_owned())
            }
            "file" => {
                let path = detail.as_str().context("a file probe names a path")?;
                ensure!(
                    clean(path) && path.starts_with('/') && path.len() <= 4096,
                    "a file probe names an absolute path"
                );
                Probe::File(PathBuf::from(path))
            }
            "argv" => {
                let argv: Vec<String> = detail
                    .as_array()
                    .context("an argv probe is a list of arguments")?
                    .iter()
                    .map(|a| a.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .context("an argv probe's arguments are strings")?;
                ensure!(
                    !argv.is_empty() && argv.len() <= MAX_ARGV,
                    "an argv probe has 1 to {MAX_ARGV} arguments"
                );
                ensure!(
                    clean(&argv[0]) && argv.iter().all(|a| a.len() <= MAX_ARG && !a.contains('\0')),
                    "an argv probe's arguments are at most {MAX_ARG} bytes, without NUL"
                );
                Probe::Argv(argv)
            }
            other => bail!("unknown probe kind {other}; use path, file or argv"),
        }))
    }
}

/// One term of the vocabulary.
#[derive(Clone, Debug)]
pub(crate) struct Term {
    pub(crate) name: String,
    note: String,
    probe: Option<Probe>,
}

impl Term {
    fn to_json(&self) -> Value {
        json!({"name": self.name, "note": self.note,
            "probe": self.probe.as_ref().map(Probe::to_json)})
    }

    fn parse(value: &Value) -> Result<Term> {
        let name = required(value, "name")?;
        identifier(name)?;
        let note = value["note"].as_str().unwrap_or("");
        ensure!(
            note.chars().count() <= MAX_NOTE,
            "a capability's note is limited to {MAX_NOTE} characters"
        );
        Ok(Term {
            name: name.to_owned(),
            note: note.to_owned(),
            probe: Probe::parse(&value["probe"])?,
        })
    }
}

pub(crate) fn vocabulary(conn: &Connection) -> Result<Vec<Term>> {
    let mut statement = conn.prepare("SELECT name, note, probe FROM capabilities ORDER BY name")?;
    let rows = statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (name, note, probe) = row?;
        let probe = match probe {
            Some(text) => Probe::parse(&serde_json::from_str(&text)?)?,
            None => None,
        };
        Ok(Term { name, note, probe })
    })
    .collect()
}

/// Whether the vocabulary holds `name`.
pub(crate) fn known(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM capabilities WHERE name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// The machine whose vocabulary this one keeps a copy of: of the machines
/// that link this one and were heard within `QUIET_MS`, the first by name;
/// when none was, the one heard last.
fn defined_on(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT name FROM machines WHERE role='collected'
             ORDER BY COALESCE(refreshed_ms, 0) >= ?1 DESC,
               CASE WHEN COALESCE(refreshed_ms, 0) >= ?1 THEN name END,
               refreshed_ms DESC, name
             LIMIT 1",
            [Store::now_ms() - QUIET_MS],
            |r| r.get(0),
        )
        .optional()?)
}

fn refuse_copy(conn: &Connection) -> Result<()> {
    if let Some(machine) = defined_on(conn)? {
        bail!(
            "this machine keeps the vocabulary {machine} sends it; define it on the linking machine"
        );
    }
    Ok(())
}

fn put(conn: &Connection, args: &Value) -> Result<Value> {
    refuse_copy(conn)?;
    let declared_only = args["declared_only"].as_bool().unwrap_or(false);
    let probe = args.get("probe").cloned().unwrap_or(Value::Null);
    ensure!(
        declared_only == probe.is_null(),
        "give a capability one probe (path, file or argv), or declared_only for one a probe cannot see"
    );
    let term = Term::parse(args)?;
    let exists = known(conn, &term.name)?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM capabilities", [], |r| r.get(0))?;
    ensure!(
        exists || (count as usize) < MAX_TERMS,
        "the vocabulary holds at most {MAX_TERMS} capabilities"
    );
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO capabilities(name, note, probe, updated_ms) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET note=excluded.note, probe=excluded.probe,
           updated_ms=excluded.updated_ms",
        params![
            term.name,
            term.note,
            term.probe.as_ref().map(|p| p.to_json().to_string()),
            Store::now_ms()
        ],
    )?;
    // What a changed probe detected no longer says anything.
    tx.execute("DELETE FROM machine_offers WHERE name=?1", [&term.name])?;
    tx.commit()?;
    Ok(term.to_json())
}

fn remove(conn: &Connection, name: &str) -> Result<Value> {
    refuse_copy(conn)?;
    identifier(name)?;
    ensure!(
        known(conn, name)?,
        "the vocabulary has no capability {name}"
    );
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    for table in ["capabilities", "offer_declarations", "machine_offers"] {
        tx.execute(&format!("DELETE FROM {table} WHERE name=?1"), [name])?;
    }
    tx.commit()?;
    Ok(json!({"removed": name}))
}

fn list(conn: &Connection) -> Result<Value> {
    Ok(json!({
        "capabilities": vocabulary(conn)?.iter().map(Term::to_json).collect::<Vec<_>>(),
        "defined_on": defined_on(conn)?.unwrap_or_else(|| "here".into()),
    }))
}

/// The vocabulary as the exchange carries it to a machine this one reaches.
pub(crate) fn carried(conn: &Connection) -> Result<Value> {
    Ok(json!(
        vocabulary(conn)?
            .iter()
            .map(Term::to_json)
            .collect::<Vec<_>>()
    ))
}

/// Keep the vocabulary a linking machine sent. A peer too old to send one
/// sends nothing, which changes nothing; a list replaces the copy whole.
/// Only the machine this one keeps its copy from (`defined_on`) is heard: a
/// machine linked by two others would otherwise flip between their
/// vocabularies on every exchange.
pub(crate) fn absorb(conn: &Connection, from: &str, sent: &Value) -> Result<()> {
    let Some(sent) = sent.as_array() else {
        return Ok(());
    };
    if defined_on(conn)?.as_deref() != Some(from) {
        return Ok(());
    }
    ensure!(
        sent.len() <= MAX_TERMS,
        "a vocabulary holds at most {MAX_TERMS} capabilities"
    );
    let terms = sent.iter().map(Term::parse).collect::<Result<Vec<_>>>()?;
    let before = vocabulary(conn)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM capabilities", [])?;
    for term in &terms {
        tx.execute(
            "INSERT OR REPLACE INTO capabilities(name, note, probe, updated_ms) VALUES(?1, ?2, ?3, ?4)",
            params![
                term.name,
                term.note,
                term.probe.as_ref().map(|p| p.to_json().to_string()),
                Store::now_ms()
            ],
        )?;
        let unchanged = before
            .iter()
            .any(|old| old.name == term.name && old.probe == term.probe);
        if !unchanged {
            tx.execute("DELETE FROM machine_offers WHERE name=?1", [&term.name])?;
        }
    }
    tx.execute(
        "DELETE FROM machine_offers WHERE name NOT IN (SELECT name FROM capabilities)",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// The full path of `binary` in one of `paths`' directories, as `on_path_in`
/// finds it.
fn which_in(binary: &str, paths: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(paths)
        .map(|dir| dir.join(binary))
        .find(|candidate| {
            candidate.is_file()
                && candidate
                    .metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        })
}

/// Run one probe, until `deadline` at the latest. `Err` says why it failed.
fn run(probe: &Probe, path: &str, deadline: Instant) -> std::result::Result<(), String> {
    match probe {
        Probe::Path(binary) => crate::cli::on_path_in(binary, path)
            .then_some(())
            .ok_or_else(|| format!("{binary} is not on PATH")),
        Probe::File(file) => file
            .exists()
            .then_some(())
            .ok_or_else(|| format!("{} does not exist", file.display())),
        Probe::Argv(argv) => run_argv(argv, path, deadline),
    }
}

fn run_argv(argv: &[String], path: &str, deadline: Instant) -> std::result::Result<(), String> {
    let program = if argv[0].contains('/') {
        PathBuf::from(&argv[0])
    } else {
        which_in(&argv[0], path).ok_or_else(|| format!("{} is not on PATH", argv[0]))?
    };
    let mut command = Command::new(&program);
    command
        .args(&argv[1..])
        .env_clear()
        .env("PATH", path)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    // Named as the captain wrote it, without the directories.
    let shown = Path::new(&argv[0])
        .file_name()
        .map_or_else(|| argv[0].clone(), |n| n.to_string_lossy().into_owned());
    let mut child = command
        .spawn()
        .map_err(|error| format!("{shown} could not start: {error}"))?;
    let pid = child.id();
    let own = Instant::now() + PROBE_TIMEOUT;
    let cut = deadline < own;
    let deadline = deadline.min(own);
    let outcome = loop {
        match crate::jobs::exited_unreaped(pid) {
            Ok(Some(_)) => break None,
            Ok(None) if Instant::now() >= deadline => {
                break Some(if cut { CUT_SHORT } else { "probe timed out" }.to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Some(format!("{error:#}")),
        }
    };
    // Signal before reaping: the unreaped child still holds its group's id, so
    // whatever it forked is killed with it and nothing else can be.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    let status = child.wait();
    if let Some(why) = outcome {
        return Err(why);
    }
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(match status.code() {
            Some(code) => format!("{shown} exited with {code}"),
            None => format!("{shown} was killed by a signal"),
        }),
        Err(error) => Err(format!("{error}")),
    }
}

/// What the operator declared or withheld here.
fn declarations(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement =
        conn.prepare("SELECT name, stance FROM offer_declarations ORDER BY name")?;
    let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether this machine offers each named term, and why not. `live` probes
/// every detected term now; otherwise a detection younger than ten minutes
/// is used. Probes run in parallel within `budget`.
fn evaluate(
    conn: &Connection,
    names: &[String],
    live: bool,
    budget: Duration,
) -> Result<Vec<(String, std::result::Result<&'static str, String>)>> {
    let vocabulary = vocabulary(conn)?;
    let declared = declarations(conn)?;
    let now = Store::now_ms();
    let mut verdicts: Vec<(String, Option<std::result::Result<&'static str, String>>)> = vec![];
    let mut to_probe: Vec<(usize, Probe)> = vec![];
    for name in names {
        let stance = declared
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s.as_str());
        let verdict = match vocabulary.iter().find(|t| &t.name == name) {
            None => Some(Err("not in this machine's vocabulary".to_owned())),
            Some(_) if stance == Some("withheld") => Some(Err("withheld here".to_owned())),
            Some(_) if stance == Some("declared") => Some(Ok("declared")),
            Some(Term { probe: None, .. }) => Some(Err("not declared here".to_owned())),
            Some(Term {
                probe: Some(probe), ..
            }) => {
                let cached: Option<(bool, Option<String>)> = if live {
                    None
                } else {
                    conn.query_row(
                        "SELECT met, why FROM machine_offers WHERE name=?1 AND probed_ms>=?2",
                        params![name, now - CACHE_MS],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?
                };
                match cached {
                    Some((true, _)) => Some(Ok("detected")),
                    Some((false, why)) => Some(Err(why.unwrap_or_else(|| "not detected".into()))),
                    None => {
                        to_probe.push((verdicts.len(), probe.clone()));
                        None
                    }
                }
            }
        };
        verdicts.push((name.clone(), verdict));
    }
    if !to_probe.is_empty() {
        let path = crate::service::login_path();
        let budget = Instant::now() + budget;
        for batch in to_probe.chunks(PARALLEL) {
            let results: Vec<(usize, std::result::Result<(), String>)> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|(index, probe)| {
                            let path = &path;
                            scope.spawn(move || {
                                if Instant::now() >= budget {
                                    return (*index, Err(NOT_RUN.to_owned()));
                                }
                                (*index, run(probe, path, budget))
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or((usize::MAX, Err("probe panicked".into())))
                        })
                        .collect()
                });
            for (index, result) in results {
                let Some((name, verdict)) = verdicts.get_mut(index) else {
                    continue;
                };
                // A probe the budget stopped said nothing about this machine.
                let unanswered = matches!(&result, Err(why) if why == CUT_SHORT || why == NOT_RUN);
                if unanswered {
                    *verdict = Some(result.map(|()| "detected"));
                    continue;
                }
                conn.execute(
                    "INSERT INTO machine_offers(name, met, why, probed_ms) VALUES(?1, ?2, ?3, ?4)
                     ON CONFLICT(name) DO UPDATE SET met=excluded.met, why=excluded.why,
                       probed_ms=excluded.probed_ms",
                    params![
                        &*name,
                        result.is_ok(),
                        result.as_ref().err(),
                        Store::now_ms()
                    ],
                )?;
                *verdict = Some(result.map(|()| "detected"));
            }
        }
    }
    Ok(verdicts
        .into_iter()
        .map(|(name, verdict)| {
            (
                name,
                verdict.unwrap_or_else(|| Err("probe not run".to_owned())),
            )
        })
        .collect())
}

/// Requirement names as a check receives them: names only, never probes.
pub(crate) fn names(value: &Value) -> Result<Vec<String>> {
    let Some(list) = value.as_array() else {
        ensure!(value.is_null(), "requires is a list of capability names");
        return Ok(vec![]);
    };
    ensure!(
        list.len() <= MAX_REQUIRES,
        "a ship requires at most {MAX_REQUIRES} capabilities"
    );
    let mut names: Vec<String> = vec![];
    for name in list {
        let name = name
            .as_str()
            .context("requires holds capability names, nothing else")?;
        identifier(name)?;
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Check required names against this machine, probing live: `{met, unmet:
/// [{need, why}]}`.
pub(crate) fn check(conn: &Connection, requires: &[String]) -> Result<Value> {
    let (mut met, mut unmet) = (vec![], vec![]);
    for (name, verdict) in evaluate(conn, requires, true, CHECK_BUDGET)? {
        match verdict {
            Ok(_) => met.push(json!(name)),
            Err(why) => unmet.push(json!({"need": name, "why": why})),
        }
    }
    Ok(json!({"met": met, "unmet": unmet}))
}

/// What this machine offers, for the exchange: a cached detection when it is
/// fresh enough. `None` when it cannot be read, so the field is omitted.
pub(crate) fn offered(conn: &Connection) -> Option<Value> {
    let names: Vec<String> = vocabulary(conn).ok()?.into_iter().map(|t| t.name).collect();
    let verdicts = evaluate(conn, &names, false, EXCHANGE_BUDGET).ok()?;
    Some(json!(
        verdicts
            .into_iter()
            .filter(|(_, v)| v.is_ok())
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
    ))
}

/// Probe what the cache no longer knows, each probe with its full time, so
/// the exchange's short budget finds fresh answers. Called from a dispatcher
/// pass at most every few minutes; best effort.
pub(crate) fn refresh(conn: &Connection) {
    if let Ok(vocabulary) = vocabulary(conn) {
        let names: Vec<String> = vocabulary.into_iter().map(|t| t.name).collect();
        let _ = evaluate(conn, &names, false, CHECK_BUDGET);
    }
}

/// A linked machine's reported offers, bounded to names.
pub(crate) fn parse_offered(reported: &Value) -> Option<Value> {
    let list = reported.as_array()?;
    Some(json!(
        list.iter()
            .filter_map(Value::as_str)
            .filter(|name| identifier(name).is_ok())
            .take(MAX_TERMS)
            .collect::<Vec<_>>()
    ))
}

/// `machine.offers {add?, remove?, withhold?}`: declare what a probe cannot
/// see, withhold what should not be used here, or drop either, then show
/// what this machine offers, probed now.
pub(crate) fn machine_offers(args: &Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let list = |key: &str| -> Result<Vec<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(vec![]),
            Some(value) => value
                .as_array()
                .with_context(|| format!("{key} is a list of capability names"))?
                .iter()
                .map(|v| {
                    let name = v.as_str().with_context(|| format!("{key} holds names"))?;
                    identifier(name)?;
                    Ok(name.to_owned())
                })
                .collect(),
        }
    };
    let (add, remove, withhold) = (list("add")?, list("remove")?, list("withhold")?);
    for name in add.iter().chain(&withhold) {
        ensure!(
            known(&conn, name)?,
            "the vocabulary has no capability {name}; see capability.list"
        );
    }
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    for name in &remove {
        tx.execute("DELETE FROM offer_declarations WHERE name=?1", [name])?;
    }
    for (names, stance) in [(&add, "declared"), (&withhold, "withheld")] {
        for name in names {
            tx.execute(
                "INSERT INTO offer_declarations(name, stance, set_ms) VALUES(?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET stance=excluded.stance, set_ms=excluded.set_ms",
                params![name, stance, Store::now_ms()],
            )?;
        }
    }
    tx.commit()?;
    let names: Vec<String> = vocabulary(&conn)?.into_iter().map(|t| t.name).collect();
    let offers: Vec<Value> = evaluate(&conn, &names, true, CHECK_BUDGET)?
        .into_iter()
        .map(|(name, verdict)| match verdict {
            Ok(how) => json!({"name": name, "offered": true, "how": how}),
            Err(why) => json!({"name": name, "offered": false, "why": why}),
        })
        .collect();
    Ok(json!({"offers": offers}))
}

/// `capability.*`: the vocabulary. Anyone may read it; the operator keeps it.
pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    match command {
        "capability.list" => list(&conn),
        "capability.put" => {
            store.caller().require_operator(command)?;
            put(&conn, &args)
        }
        "capability.remove" => {
            store.caller().require_operator(command)?;
            remove(&conn, required(&args, "name")?)
        }
        _ => bail!("unknown capability command {command}"),
    }
}

/// `privateer capability put NAME (--probe-path BIN | --probe-file PATH |
/// --probe-argv A,B,C | --declared-only) [--note TEXT] | list | remove NAME`
pub fn cli(args: &[String]) -> Result<()> {
    let usage = "usage: privateer capability put NAME (--probe-path BINARY | --probe-file PATH | --probe-argv ARG,ARG,... | --declared-only) [--note TEXT] | list | remove NAME";
    let flags = crate::cli::flags(args, &["probe-path", "probe-file", "probe-argv", "note"])?;
    let conn = crate::fleet::registry()?;
    let result = match flags.positional.first().map(String::as_str) {
        Some("put") => {
            let name = flags.positional.get(1).context(usage)?;
            let mut probes = vec![];
            if let Some(binary) = flags.values.get("probe-path") {
                probes.push(json!({"path": binary}));
            }
            if let Some(path) = flags.values.get("probe-file") {
                probes.push(json!({"file": path}));
            }
            if let Some(argv) = flags.values.get("probe-argv") {
                probes.push(json!({"argv": argv.split(',').collect::<Vec<_>>()}));
            }
            let declared_only = flags.switches.iter().any(|s| s == "declared-only");
            ensure!(
                probes.len() + usize::from(declared_only) == 1,
                "{usage}\ngive exactly one probe, or --declared-only"
            );
            let mut request = json!({"name": name, "declared_only": declared_only,
                "probe": probes.pop()});
            if let Some(note) = flags.values.get("note") {
                request["note"] = json!(note);
            }
            put(&conn, &request)?
        }
        Some("list") => list(&conn)?,
        Some("remove") => remove(&conn, flags.positional.get(1).context(usage)?)?,
        _ => bail!(usage),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE machines(name TEXT PRIMARY KEY, role TEXT NOT NULL, refreshed_ms INTEGER);",
        )
        .unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn a_probe_is_one_of_path_file_or_argv() {
        assert!(
            Probe::parse(&json!({"path": "xcodebuild"}))
                .unwrap()
                .is_some()
        );
        assert!(Probe::parse(&json!({"path": "/usr/bin/xcodebuild"})).is_err());
        assert!(Probe::parse(&json!({"file": "relative"})).is_err());
        assert!(Probe::parse(&json!({"argv": []})).is_err());
        assert!(Probe::parse(&json!({"argv": ["a", 1]})).is_err());
        assert!(Probe::parse(&json!({"path": "a", "file": "/b"})).is_err());
        assert!(Probe::parse(&json!({"shell": "true"})).is_err());
        assert_eq!(Probe::parse(&Value::Null).unwrap(), None);
    }

    #[test]
    fn a_term_needs_one_probe_or_declared_only() {
        let conn = registry();
        assert!(put(&conn, &json!({"name": "xcode"})).is_err());
        assert!(
            put(
                &conn,
                &json!({"name": "xcode", "declared_only": true, "probe": {"path": "xcodebuild"}})
            )
            .is_err()
        );
        put(&conn, &json!({"name": "phone", "declared_only": true})).unwrap();
        put(&conn, &json!({"name": "sh", "probe": {"path": "sh"}})).unwrap();
        let listed = list(&conn).unwrap();
        assert_eq!(
            listed["capabilities"].as_array().unwrap().len(),
            2,
            "{listed}"
        );
        assert_eq!(listed["defined_on"], "here");
    }

    #[test]
    fn requirements_are_names_only() {
        assert!(names(&json!([{"name": "x", "probe": {"argv": ["touch", "/tmp/x"]}}])).is_err());
        assert!(names(&json!("xcode")).is_err());
        let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        assert!(names(&json!(many)).is_err());
        assert_eq!(names(&json!(["a", "a", "b"])).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn the_linked_side_keeps_the_copy_it_was_sent() {
        let conn = registry();
        conn.execute(
            "INSERT INTO machines(name, role) VALUES('laptop', 'collected')",
            [],
        )
        .unwrap();
        let refused = put(&conn, &json!({"name": "x", "declared_only": true})).unwrap_err();
        assert!(format!("{refused:#}").contains("define it on the linking machine"));
        absorb(
            &conn,
            "laptop",
            &json!([{"name": "xcode", "note": "", "probe": {"path": "xcodebuild"}}]),
        )
        .unwrap();
        assert!(known(&conn, "xcode").unwrap());
        // An older peer sends no vocabulary, which changes nothing.
        absorb(&conn, "laptop", &Value::Null).unwrap();
        assert!(known(&conn, "xcode").unwrap());
        absorb(&conn, "laptop", &json!([])).unwrap();
        assert!(!known(&conn, "xcode").unwrap());
    }

    #[test]
    fn a_machine_linked_by_two_keeps_only_its_defining_linkers_vocabulary() {
        let conn = registry();
        conn.execute_batch(
            "INSERT INTO machines(name, role) VALUES('desk', 'collected');
             INSERT INTO machines(name, role) VALUES('studio', 'collected');",
        )
        .unwrap();
        let desk = json!([{"name": "xcode", "note": "", "probe": {"path": "xcodebuild"}}]);
        let studio = json!([{"name": "docker", "note": "", "probe": {"path": "docker"}}]);
        absorb(&conn, "desk", &desk).unwrap();
        absorb(&conn, "studio", &studio).unwrap();
        assert!(known(&conn, "xcode").unwrap());
        assert!(!known(&conn, "docker").unwrap());
        assert_eq!(list(&conn).unwrap()["defined_on"], "desk");
    }

    #[test]
    fn a_quiet_linker_stops_defining_the_vocabulary() {
        let conn = registry();
        let now = Store::now_ms();
        // desk sorts first but removed the link on its side long ago.
        conn.execute(
            "INSERT INTO machines(name, role, refreshed_ms) VALUES('desk', 'collected', ?1),
               ('studio', 'collected', ?2)",
            params![now - 2 * QUIET_MS, now],
        )
        .unwrap();
        assert_eq!(defined_on(&conn).unwrap().as_deref(), Some("studio"));
        let studio = json!([{"name": "docker", "note": "", "probe": {"path": "docker"}}]);
        absorb(&conn, "studio", &studio).unwrap();
        assert!(known(&conn, "docker").unwrap());
        // A later linker heard now whose name sorts earlier takes over.
        conn.execute(
            "INSERT INTO machines(name, role, refreshed_ms) VALUES('alpha', 'collected', ?1)",
            [now],
        )
        .unwrap();
        assert_eq!(defined_on(&conn).unwrap().as_deref(), Some("alpha"));
        // With every linker quiet, the one heard last.
        conn.execute(
            "UPDATE machines SET refreshed_ms=refreshed_ms-?1",
            [3 * QUIET_MS],
        )
        .unwrap();
        conn.execute(
            "UPDATE machines SET refreshed_ms=?1 WHERE name='studio'",
            [now - 2 * QUIET_MS],
        )
        .unwrap();
        assert_eq!(defined_on(&conn).unwrap().as_deref(), Some("studio"));
    }

    #[test]
    fn a_probe_the_exchange_cut_short_is_not_cached() {
        let conn = registry();
        put(
            &conn,
            &json!({"name": "slow", "probe": {"argv": ["sleep", "2"]}}),
        )
        .unwrap();
        let names = vec!["slow".to_owned()];
        let verdicts = evaluate(&conn, &names, false, Duration::from_millis(200)).unwrap();
        assert_eq!(verdicts[0].1, Err(CUT_SHORT.to_owned()));
        let cached: i64 = conn
            .query_row("SELECT COUNT(*) FROM machine_offers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cached, 0, "a cut-short probe is no answer");
        // With its full time, it answers, and the answer is cached.
        refresh(&conn);
        let met: bool = conn
            .query_row(
                "SELECT met FROM machine_offers WHERE name='slow'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(met);
    }

    #[test]
    fn offers_are_detected_declared_and_withheld() {
        let conn = registry();
        put(&conn, &json!({"name": "shell", "probe": {"file": "/"}})).unwrap();
        put(
            &conn,
            &json!({"name": "absent", "probe": {"file": "/no/such/path"}}),
        )
        .unwrap();
        put(&conn, &json!({"name": "phone", "declared_only": true})).unwrap();
        let wanted: Vec<String> = ["shell", "absent", "phone", "typo"]
            .map(str::to_owned)
            .to_vec();
        let checked = check(&conn, &wanted).unwrap();
        assert_eq!(checked["met"], json!(["shell"]), "{checked}");
        let why: Vec<&str> = checked["unmet"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["why"].as_str().unwrap())
            .collect();
        assert_eq!(
            why,
            [
                "/no/such/path does not exist",
                "not declared here",
                "not in this machine's vocabulary"
            ]
        );
        conn.execute_batch(
            "INSERT INTO offer_declarations VALUES('phone','declared',1);
             INSERT INTO offer_declarations VALUES('shell','withheld',1);",
        )
        .unwrap();
        let checked = check(&conn, &wanted).unwrap();
        assert_eq!(checked["met"], json!(["phone"]), "{checked}");
        assert_eq!(
            checked["unmet"][0],
            json!({"need": "shell", "why": "withheld here"})
        );
    }

    #[test]
    fn an_argv_probe_succeeds_only_on_exit_zero_and_times_out() {
        let path = "/usr/bin:/bin";
        let far = Instant::now() + CHECK_BUDGET;
        assert!(run_argv(&["true".into()], path, far).is_ok());
        assert_eq!(
            run_argv(&["false".into()], path, far).unwrap_err(),
            "false exited with 1"
        );
        let started = Instant::now();
        let soon = Instant::now() + Duration::from_millis(300);
        assert_eq!(
            run_argv(&["sleep".into(), "30".into()], path, soon).unwrap_err(),
            CUT_SHORT
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            run_argv(&["no-such-probe-binary".into()], path, far)
                .unwrap_err()
                .contains("not on PATH")
        );
    }
}
