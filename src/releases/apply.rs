//! `privateer upgrade COMMIT --local [--dry-run]` and `privateer upgrade
//! rollback [--local] [--dry-run]`: switch this machine's `current` to a
//! release `upgrade fetch` published, restart the dispatchers and then the
//! front doors that run through it, and check each dispatcher came back on
//! that release; if one did not, `current` goes back and they restart again.
//! Hosted panes catch up on their own at their next safe point, and the MCP
//! broker's installed copy is never touched. Nothing is fetched or built.
//!
//! The round runs detached (`upgrade _round ID`, in its own session), so the
//! dispatcher restart cannot end the command that asked for it, and records
//! each step in `<data>/upgrade/ID.json` as it goes; the caller waits on that
//! file. Rounds, adoptions and prunes of one installation are serialised by
//! `<data>/upgrade.lock`.
use super::{
    Release, ShipOutcome, UpgradeRecord, installation, processes, read_history, running_image,
    selected,
};
use crate::service::{Managed, Restartable};
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str =
    "usage: privateer upgrade COMMIT --local [--dry-run] | rollback [--local] [--dry-run]";

/// How long a restarted dispatcher has to come back on its release.
const HEALTH: Duration = Duration::from_secs(60);

/// Round state files (and their logs) kept in `<data>/upgrade/`.
const KEEP_ROUNDS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The release to switch to; None for a rollback, which takes the
    /// previous recorded one.
    pub commit: Option<String>,
    pub dry_run: bool,
}

impl Options {
    /// `COMMIT --local [--dry-run]`. Only this machine is upgraded yet:
    /// `--local` says so, and linked machines come later.
    pub fn parse_upgrade(args: &[String]) -> Result<Self> {
        let (commit, flags) = args.split_first().context(USAGE)?;
        ensure!(
            super::valid_commit(commit),
            "upgrade takes a full lowercase commit, not {commit:?}"
        );
        let (local, dry_run) = flags_of(flags)?;
        ensure!(
            local,
            "pass --local: upgrade switches this machine only (linked machines are not upgraded yet)"
        );
        Ok(Self {
            commit: Some(commit.clone()),
            dry_run,
        })
    }

    /// `rollback [--local] [--dry-run]`.
    pub fn parse_rollback(args: &[String]) -> Result<Self> {
        let (_, dry_run) = flags_of(args)?;
        Ok(Self {
            commit: None,
            dry_run,
        })
    }
}

fn flags_of(args: &[String]) -> Result<(bool, bool)> {
    let (mut local, mut dry_run) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--local" => local = true,
            "--dry-run" => dry_run = true,
            _ => bail!("{USAGE}"),
        }
    }
    Ok((local, dry_run))
}

/// A running dispatcher or door in the round.
struct Target {
    /// The ship, or `door:NAME`.
    name: String,
    home: Option<PathBuf>,
    service: Restartable,
    how: &'static str,
}

/// What a round switches and restarts, read before anything changes.
struct Plan {
    data: PathBuf,
    release: Release,
    previous: Release,
    /// The interrupted round this one resumes, which already switched.
    resumes: Option<String>,
    /// Dispatcher units and agents that need the safe stop policy first.
    policies: Vec<(PathBuf, String, bool)>,
    dispatchers: Vec<Target>,
    doors: Vec<Target>,
    /// Services the round leaves alone, with why.
    left: Vec<Value>,
}

impl Plan {
    fn json(&self) -> Value {
        let names = |targets: &[Target]| -> Vec<Value> {
            targets
                .iter()
                .map(|t| json!({"name": t.name, "home": t.home, "managed": t.how}))
                .collect()
        };
        json!({
            "commit": self.release.commit,
            "release": self.release.root,
            "from": self.previous.commit,
            "resumes": self.resumes,
            "stop_policy": self.policies.iter().map(|(path, _, _)| path).collect::<Vec<_>>(),
            "dispatchers": names(&self.dispatchers),
            "doors": names(&self.doors),
            "left": self.left,
            "launcher_unversioned": super::unversioned_launchers(
                &running_image().unwrap_or_default(), &self.data),
        })
    }
}

/// Whether a unit's program follows `current`: it is the entry point, or a
/// launcher linked to it (as adopt leaves them).
fn follows(program: &Path, entry: &Path) -> bool {
    program == entry || fs::read_link(program).is_ok_and(|target| target == entry)
}

/// Plan a round onto `commit`. `own` is the round asking, when its runner
/// plans again under the lock. When `current` already selects `commit`, only
/// an interrupted round that switched to it can be resumed: its restarts run
/// again, from the release it came from.
fn plan(commit: &str, own: Option<&str>) -> Result<Plan> {
    let image = running_image()?;
    let data = installation(Release::from_executable(&image).as_ref())?;
    let release = Release::new(&data, commit)?;
    ensure!(
        fs::symlink_metadata(&release.root).is_ok(),
        "{} is missing; run `privateer upgrade fetch {commit}` first (upgrade never fetches or builds)",
        release.root.display()
    );
    super::fetch::existing(&release.root, "exists")?;
    let current = selected(&data)?
        .context("this machine has no current release; run `privateer upgrade adopt` first")?;
    let (previous, resumes, resumed) = if current.commit == commit {
        let round = interrupted(&data, own)
            .filter(|round| last_switch(round) == Some(("switch", commit)))
            .with_context(|| format!("current already selects {commit}; nothing to switch"))?;
        let from = round["from"]
            .as_str()
            .context("the interrupted round's release")?;
        // What it set out to restart, running or not by now.
        let names = round["steps"][0]["plan"]
            .as_object()
            .into_iter()
            .flat_map(|plan| ["dispatchers", "doors"].map(|key| &plan[key]))
            .flat_map(|targets| targets.as_array().into_iter().flatten())
            .filter_map(|target| target["name"].as_str().map(str::to_owned))
            .collect();
        (
            Release::new(&data, from)?,
            round["id"].as_str().map(str::to_owned),
            names,
        )
    } else {
        (current, None, Vec::new())
    };
    let entry = release.entrypoint("privateer")?;
    let real_data = fs::canonicalize(&data).unwrap_or_else(|_| data.clone());
    let launchers = super::adopt::launcher_dirs(&image, &data);
    let mut policies = Vec::new();
    let mut pinned = Vec::new();
    let mut left = Vec::new();
    for installed in crate::service::installed_services()? {
        let program = crate::service::program(&installed)?;
        if follows(&program, &entry) {
            let policy = crate::service::with_stop_policy(&installed)?;
            // An agent that keeps its process group ends its wakes when it
            // stops: refused now, not at its restart after the switch.
            let abandons = policy
                .as_deref()
                .unwrap_or(&installed.text)
                .contains("<key>AbandonProcessGroup</key><true/>");
            ensure!(
                !(installed.launchd && installed.dispatcher) || abandons,
                "{} does not abandon its process group, so restarting it would end its wakes; add <key>AbandonProcessGroup</key><true/> to it first",
                installed.path.display()
            );
            if let Some(text) = policy {
                policies.push((installed.path.clone(), text, installed.launchd));
            }
            continue;
        }
        // A unit adopt would repoint is not adopted yet; one pinned to
        // another privateer stays as it is, out of the round.
        let adoptable = program.starts_with(&data)
            || program.starts_with(&real_data)
            || launchers
                .iter()
                .any(|dir| program.parent() == Some(dir.as_path()));
        ensure!(
            !adoptable,
            "{} runs {}, not {}; run `privateer upgrade adopt` first",
            installed.path.display(),
            program.display(),
            entry.display()
        );
        left.push(json!({"unit": installed.path, "reason": format!(
            "it runs {}, which does not follow current; restart it by hand", program.display())}));
        pinned.push(installed.path);
    }
    let classify = |name: String,
                    home: Option<PathBuf>,
                    service: Restartable|
     -> Result<std::result::Result<Target, Value>> {
        let skip = |reason: String| Ok(Err(json!({"name": name, "reason": reason})));
        let running = service.running()?;
        if !running && !resumed.contains(&name) {
            return skip("not running; left stopped".into());
        }
        let how = match service.managed()? {
            Managed::Unit(path) | Managed::Agent(path) if pinned.contains(&path) => {
                return skip(format!("{} does not follow current", path.display()));
            }
            Managed::Unit(_) => "systemd",
            Managed::Agent(_) => "launchd",
            // One the interrupted round stopped is started again.
            Managed::Detached if !running => "detached",
            // Its restart signals the recorded process only while it holds
            // the lock and is this service's: anything else is left out
            // now, rather than failing its restart after the switch.
            Managed::Detached => match service.detached_pid() {
                Err(why) => return skip(format!("{why:#}")),
                Ok(pid) => match processes::image(pid) {
                    Some(image) if !image.starts_with(&real_data) && !image.starts_with(&data) => {
                        return skip(format!(
                            "it runs {}, not a release of this installation; restart it by hand",
                            image.display()
                        ));
                    }
                    _ => "detached",
                },
            },
        };
        Ok(Ok(Target {
            name,
            home,
            service,
            how,
        }))
    };
    let mut dispatchers = Vec::new();
    let conn = crate::fleet::registry()?;
    for member in crate::fleet::members(&conn)? {
        if !member.home.join("privateer.sqlite").is_file() {
            continue;
        }
        let service = crate::service::restartable_dispatcher(&member.home, &member.name);
        match classify(member.name.clone(), Some(member.home.clone()), service)? {
            Ok(target) => dispatchers.push(target),
            Err(why) => left.push(why),
        }
    }
    let mut doors = Vec::new();
    for name in crate::door::names()? {
        let service = crate::service::restartable_door(&name)?;
        match classify(format!("door:{name}"), None, service)? {
            Ok(target) => doors.push(target),
            Err(why) => left.push(why),
        }
    }
    Ok(Plan {
        data,
        release,
        previous,
        resumes,
        policies,
        dispatchers,
        doors,
        left,
    })
}

/// The release a rollback returns to: the one the newest successful upgrade
/// onto `current` came from. A rollback's own record is never followed, so
/// a second rollback steps further back rather than undoing the first.
fn rollback_target() -> Result<(String, String)> {
    let image = running_image()?;
    let data = installation(Release::from_executable(&image).as_ref())?;
    let current = selected(&data)?.context("this machine has no current release to roll back")?;
    let registry = crate::fleet::registry_path()?;
    read_history(&registry, -1)?
        .into_iter()
        .find(|r| {
            r.kind.as_deref() == Some("upgrade")
                && super::switched(r)
                && r.to_commit == current.commit
                && r.from_commit.is_some()
        })
        .map(|r| (r.from_commit.unwrap_or_default(), r.id))
        .with_context(|| {
            format!(
                "no recorded upgrade onto {} to roll back; switch with `privateer upgrade COMMIT --local`",
                current.commit
            )
        })
}

fn rounds(data: &Path) -> PathBuf {
    data.join("upgrade")
}

fn state_path(data: &Path, id: &str) -> PathBuf {
    rounds(data).join(format!("{id}.json"))
}

/// Write a round's state beside its file and rename it over.
fn save(path: &Path, state: &Value) -> Result<()> {
    let new = path.with_extension("json.new");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&new)?;
    file.write_all(serde_json::to_string_pretty(state)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&new, path)?;
    Ok(())
}

/// Keep the newest rounds' files only; never anything but `*.json` and
/// `*.log` directly in `<data>/upgrade/`.
fn trim_rounds(data: &Path) {
    let Ok(entries) = fs::read_dir(rounds(data)) else {
        return;
    };
    let mut states: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    states.sort();
    let excess = states.len().saturating_sub(KEEP_ROUNDS);
    for (_, path) in states.into_iter().take(excess) {
        let _ = fs::remove_file(path.with_extension("log"));
        let _ = fs::remove_file(path);
    }
}

/// The newest round's state other than `own`.
fn newest_round(data: &Path, own: Option<&str>) -> Option<Value> {
    fs::read_dir(rounds(data))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let state: Value = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((state["requested_ms"].as_i64().unwrap_or(0), state))
        })
        .filter(|(_, state)| own.is_none_or(|own| state["id"] != own))
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, state)| state)
}

/// The newest round, when it stopped without an outcome: killed, or its
/// machine went down. Its runner no longer holds the lock, unless `own` is
/// the runner asking (which holds it itself).
fn interrupted(data: &Path, own: Option<&str>) -> Option<Value> {
    let round = newest_round(data, own)?;
    let started = round["steps"].as_array().is_some_and(|s| !s.is_empty());
    let free = own.is_some() || !crate::service::held(&data.join("upgrade.lock")).unwrap_or(true);
    (round["outcome"].is_null() && started && free).then_some(round)
}

/// The round's last switch, `switch` or `rollback: switch`, and the release
/// it moved `current` to.
fn last_switch(round: &Value) -> Option<(&str, &str)> {
    round["steps"].as_array()?.iter().rev().find_map(|step| {
        let name = step["step"].as_str()?;
        if matches!(name, "switch" | "rollback: switch") && step["state"] == "done" {
            Some((name, step["to"].as_str()?))
        } else {
            None
        }
    })
}

/// The newest round's state, and whether it is still running, for `upgrade
/// status`; one that stopped without an outcome says so, and how to go on.
pub(super) fn last_round(data: &Path) -> Value {
    let Some(mut state) = newest_round(data, None) else {
        return Value::Null;
    };
    let lock_held = crate::service::held(&data.join("upgrade.lock")).unwrap_or(false);
    // A runner takes the lock before its first step.
    let starting = state["steps"].as_array().is_some_and(|s| s.is_empty())
        && Store::now_ms() - state["requested_ms"].as_i64().unwrap_or(0) < 30_000;
    let unfinished = state["outcome"].is_null();
    state["in_progress"] = json!(unfinished && (lock_held || starting));
    if unfinished && !lock_held && !starting {
        let from = state["from"].as_str().unwrap_or_default().to_owned();
        let note = match last_switch(&state) {
            Some(("switch", to)) => format!(
                "this round stopped before it finished, with current on {to} and its dispatchers perhaps partly restarted; `privateer upgrade {to} --local` resumes its restarts, or `privateer upgrade {from} --local` switches back"
            ),
            Some((_, to)) => format!(
                "this round stopped while rolling back, with current back on {to}; its dispatchers may still run another release: restart each by hand (`privateer service stop` and `start`)"
            ),
            None => "this round stopped before it switched anything".into(),
        };
        state["interrupted"] = json!(true);
        state["note"] = json!(note);
    }
    state
}

/// Close the interrupted round a new one resumes.
fn close_interrupted(data: &Path, id: &str, by: &str) {
    let path = state_path(data, id);
    let Some(mut state) = fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
    else {
        return;
    };
    state["outcome"] = json!("interrupted");
    state["resumed_by"] = json!(by);
    if let Err(error) = save(&path, &state) {
        eprintln!("cannot close the interrupted round {id}: {error:#}");
    }
}

/// `upgrade COMMIT --local` and `upgrade rollback`: check the plan, then run
/// the round detached and wait for its result. `--dry-run` prints the plan.
pub fn start(options: &Options, actor: &str) -> Result<Value> {
    let (kind, commit, rollback_of) = match &options.commit {
        Some(commit) => ("upgrade", commit.clone(), None),
        None => {
            let (commit, of) = rollback_target()?;
            ("rollback", commit, Some(of))
        }
    };
    let plan = plan(&commit, None)?;
    if options.dry_run {
        let mut report = plan.json();
        report["kind"] = json!(kind);
        report["rollback_of"] = json!(rollback_of);
        report["dry_run"] = json!(true);
        report["note"] = json!("dry run: nothing was changed");
        return Ok(report);
    }
    // Fail at once while another round holds the lock; the runner takes it
    // for itself.
    drop(super::adopt::lock(&plan.data)?);
    let id = uuid::Uuid::new_v4().to_string();
    crate::store::private_dirs(&rounds(&plan.data))?;
    trim_rounds(&plan.data);
    let path = state_path(&plan.data, &id);
    save(
        &path,
        &json!({"id": id, "kind": kind, "commit": commit, "from": plan.previous.commit,
            "rollback_of": rollback_of, "actor": actor, "requested_ms": Store::now_ms(),
            "steps": [], "outcome": null}),
    )?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.with_extension("log"))?;
    let mut runner = Command::new(running_image()?);
    for name in [
        "PRIVATEER_ROLE",
        "PRIVATEER_ACTOR",
        "PRIVATEER_JOB",
        "PRIVATEER_HOME",
        "PRIVATEER_PRESENCE_HOLDER",
        "PRIVATEER_WAKE",
        "PRIVATEER_CONTROL_CLI",
    ] {
        runner.env_remove(name);
    }
    runner
        .args(["upgrade", "_round", &id])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        runner.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match runner.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut state: Value = serde_json::from_slice(&fs::read(&path)?)?;
            state["outcome"] = json!("failed");
            state["error"] = json!(format!("cannot start the upgrade round: {error}"));
            save(&path, &state)?;
            return Ok(state);
        }
    };
    // The round outlives this command; this only waits for its result.
    loop {
        let state: Value = serde_json::from_slice(&fs::read(&path)?)?;
        if !state["outcome"].is_null() {
            let _ = child.wait();
            return Ok(state);
        }
        if let Some(status) = child.try_wait()? {
            let state: Value = serde_json::from_slice(&fs::read(&path)?)?;
            if !state["outcome"].is_null() {
                return Ok(state);
            }
            bail!(
                "the upgrade round exited ({status}) without a result; see {}",
                path.with_extension("log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A round's progress, saved after every step.
struct Round {
    path: PathBuf,
    state: Value,
}

impl Round {
    /// Record a step. A save that fails does not stop the round (its final
    /// save reports), so no step can leave `current` moved without the
    /// round's outcome.
    fn step(&mut self, step: Value) {
        if let Some(steps) = self.state["steps"].as_array_mut() {
            steps.push(step);
        }
        if let Err(error) = save(&self.path, &self.state) {
            eprintln!("cannot save the round's state: {error:#}");
        }
    }
}

/// Point `current` at `releases/<commit>`: `current.new`, renamed over it.
fn switch(data: &Path, commit: &str) -> Result<()> {
    let current = data.join("current");
    let new = data.join("current.new");
    match fs::symlink_metadata(&new) {
        Ok(meta) if meta.is_dir() => bail!("{} is a directory; remove it", new.display()),
        Ok(_) => fs::remove_file(&new)?,
        Err(_) => {}
    }
    symlink(Path::new("releases").join(commit), &new)?;
    fs::rename(&new, &current)?;
    Ok(())
}

fn metadata(home: &Path, key: &str) -> Option<String> {
    let store = Store::open_readonly(home, Caller::Operator).ok()?;
    store
        .connection()
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .ok()
}

/// How long restarted services have to come back (`PRIVATEER_UPGRADE_HEALTH_SECS`
/// in tests).
fn health_bound() -> Duration {
    std::env::var("PRIVATEER_UPGRADE_HEALTH_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| (1..=600).contains(s))
        .map(Duration::from_secs)
        .unwrap_or(HEALTH)
}

/// Wait for a restarted dispatcher to hold its lock under a new generation
/// that runs `commit` and has finished a dispatcher pass on it. A generation
/// that lets go of its lock, or is replaced by another (a unit restarting a
/// release that keeps crashing), fails at once. A dispatcher from before
/// releases recorded their image only proves it came back.
fn came_back(
    target: &Target,
    before: &Option<String>,
    commit: &str,
    until: Instant,
) -> Result<String> {
    let home = target.home.as_deref().context("a dispatcher has a home")?;
    let mut seen: Option<String> = None;
    loop {
        let generation = metadata(home, "dispatcher_generation");
        if generation.is_some() && generation != *before {
            ensure!(
                seen.is_none() || seen == generation,
                "came back, then started again on its own: it does not stay up on {commit}"
            );
            seen.clone_from(&generation);
            // A generation is recorded only once its lock is held, so a
            // free lock means it has exited.
            ensure!(
                target.service.running()?,
                "came back, then stopped before finishing a pass"
            );
            let release = metadata(home, "dispatcher_release")
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .filter(|r| r["generation"].as_str() == generation.as_deref());
            let Some(release) = release else {
                return Ok("came back (its release does not report its image)".into());
            };
            let image = release["image"].as_str().unwrap_or_default();
            let running = Release::from_executable(Path::new(image)).map(|r| r.commit);
            ensure!(
                running.as_deref() == Some(commit),
                "came back running {image}, not release {commit}"
            );
            if !release["passed_ms"].is_null() {
                return Ok(format!("running {commit}, a pass done"));
            }
        }
        ensure!(
            Instant::now() < until,
            "{} within {} s",
            if seen.is_some() {
                "came back but finished no pass"
            } else {
                "did not come back"
            },
            health_bound().as_secs()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Restart every dispatcher and check each comes back on `commit`; the
/// failures, by name.
fn restart_dispatchers(
    round: &mut Round,
    targets: &[Target],
    commit: &str,
    phase: &str,
) -> Vec<String> {
    let before: Vec<Option<String>> = targets
        .iter()
        .map(|t| {
            t.home
                .as_deref()
                .and_then(|home| metadata(home, "dispatcher_generation"))
        })
        .collect();
    let mut restarted = Vec::new();
    for target in targets {
        let result = target.service.restart();
        round.step(
            json!({"step": format!("{phase}: restart dispatcher"), "name": target.name,
            "state": if result.is_ok() {"done"} else {"failed"},
            "detail": match &result { Ok(how) => how.clone(), Err(e) => format!("{e:#}") }}),
        );
        restarted.push(result.is_ok());
    }
    let until = Instant::now() + health_bound();
    let mut failed = Vec::new();
    for ((target, before), restarted) in targets.iter().zip(&before).zip(restarted) {
        let health = if restarted {
            came_back(target, before, commit, until)
        } else {
            Err(anyhow::anyhow!("its restart failed"))
        };
        round.step(
            json!({"step": format!("{phase}: health"), "name": target.name,
            "state": if health.is_ok() {"ok"} else {"failed"},
            "detail": match &health { Ok(how) => how.clone(), Err(e) => format!("{e:#}") }}),
        );
        if health.is_err() {
            failed.push(target.name.clone());
        }
    }
    failed
}

/// Restart doors and wait for each to hold its lock again; each result, in
/// order.
fn restart_doors(
    round: &mut Round,
    targets: &[&Target],
    phase: &str,
) -> Vec<std::result::Result<String, String>> {
    let until = Instant::now() + health_bound();
    let mut results = Vec::new();
    for target in targets {
        let result = target.service.restart().and_then(|how| {
            while !target.service.running()? {
                ensure!(
                    Instant::now() < until,
                    "did not come back within {} s",
                    health_bound().as_secs()
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(how)
        });
        round.step(
            json!({"step": format!("{phase}restart door"), "name": target.name,
            "state": if result.is_ok() {"done"} else {"failed"},
            "detail": match &result { Ok(how) => how.clone(), Err(e) => format!("{e:#}") }}),
        );
        results.push(result.map_err(|e| format!("{e:#}")));
    }
    results
}

/// `upgrade _round ID`: the detached round itself.
pub fn round(id: &str) -> Result<Value> {
    let image = running_image()?;
    let data = installation(Release::from_executable(&image).as_ref())?;
    ensure!(
        !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
        "bad round id"
    );
    let path = state_path(&data, id);
    let state: Value = serde_json::from_slice(&fs::read(&path).context("no such upgrade round")?)?;
    ensure!(state["outcome"].is_null(), "that round has finished");
    let mut round = Round { path, state };
    round.state["pid"] = json!(std::process::id());
    round.state["started_ms"] = json!(Store::now_ms());
    let result = run(&mut round, &data);
    if let Err(error) = result {
        round.state["outcome"] = json!("failed");
        round.state["error"] = json!(format!("{error:#}"));
    }
    round.state["completed_ms"] = json!(Store::now_ms());
    save(&round.path, &round.state)?;
    Ok(round.state)
}

/// What every history record of a round shares.
struct Meta {
    id: String,
    kind: String,
    rollback_of: Option<String>,
    actor: String,
    started_ms: i64,
}

impl Meta {
    fn record(
        &self,
        kind: &str,
        outcome: &str,
        (from, to): (&str, &str),
        rollback_of: Option<String>,
        ships: Vec<ShipOutcome>,
        id: String,
    ) -> UpgradeRecord {
        UpgradeRecord {
            id,
            kind: Some(kind.into()),
            outcome: Some(outcome.into()),
            rollback_of,
            from_commit: Some(from.into()),
            to_commit: to.into(),
            actor: self.actor.clone(),
            started_ms: self.started_ms,
            completed_ms: Store::now_ms().max(self.started_ms),
            ships,
        }
    }
}

fn run(round: &mut Round, data: &Path) -> Result<()> {
    let _lock = super::adopt::lock(data)?;
    let commit = round.state["commit"]
        .as_str()
        .context("round commit")?
        .to_owned();
    let meta = Meta {
        id: round.state["id"].as_str().unwrap_or_default().to_owned(),
        kind: round.state["kind"].as_str().unwrap_or("upgrade").to_owned(),
        rollback_of: round.state["rollback_of"].as_str().map(str::to_owned),
        actor: round.state["actor"]
            .as_str()
            .unwrap_or("operator")
            .to_owned(),
        started_ms: Store::now_ms(),
    };
    // Planned again under the lock: what runs may have changed meanwhile.
    let plan = plan(&commit, Some(&meta.id))?;
    // Opened before anything moves: once `current` has, the round's outcome
    // must reach the history.
    let conn = crate::fleet::registry()?;
    super::init(&conn)?;
    round.state["from"] = json!(plan.previous.commit);
    round.state["resumes"] = json!(plan.resumes);
    round.step(json!({"step": "plan", "state": "done", "plan": plan.json()}));

    // A dispatcher's unit must stop its main process alone before it is
    // restarted, so its wakes keep running.
    for (unit, text, _) in &plan.policies {
        let mut name = unit.file_name().unwrap_or_default().to_os_string();
        name.push(".new");
        let new = unit.with_file_name(name);
        let mut file = fs::File::create(&new)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&new, unit)?;
        round.step(json!({"step": "stop policy", "unit": unit, "state": "done"}));
    }
    // systemd stops a unit as it last read it, and `restart` checks the
    // file: every unit is read again first, so one changed on disk (by this
    // round, or by hand) stops as its file says.
    let units = plan.policies.iter().any(|(_, _, launchd)| !launchd)
        || plan
            .dispatchers
            .iter()
            .chain(&plan.doors)
            .any(|t| t.how == "systemd");
    if units {
        crate::service::daemon_reload()?;
        round.step(json!({"step": "daemon-reload", "state": "done"}));
    }

    match &plan.resumes {
        Some(interrupted) => {
            close_interrupted(data, interrupted, &meta.id);
            round.step(json!({"step": "switch", "state": "done",
                "from": plan.previous.commit, "to": commit,
                "detail": format!("already switched by the interrupted round {interrupted}; its restarts run again")}));
        }
        None => {
            switch(data, &commit)?;
            round.step(
                json!({"step": "switch", "state": "done", "from": plan.previous.commit, "to": commit}),
            );
        }
    }
    // `current` has moved. Every step from here records its own failure and
    // goes on, and whatever keeps the round from finishing sends `current`
    // back and restarts what moved, with the history saying so.
    let failed = restart_dispatchers(round, &plan.dispatchers, &commit, "switch");
    if !failed.is_empty() {
        let why = format!("{} did not come back on {commit}", failed.join(", "));
        roll_back(round, data, &plan, &conn, &meta, why, &failed, &[]);
        return Ok(());
    }
    // Doors after the dispatchers: they only carry the captain's mail.
    let doors: Vec<&Target> = plan.doors.iter().collect();
    let results = restart_doors(round, &doors, "");
    if let Err(error) = finish(round, &plan, &conn, &meta, &results) {
        let moved: Vec<&Target> = doors
            .into_iter()
            .zip(&results)
            .filter(|(_, result)| result.is_ok())
            .map(|(target, _)| target)
            .collect();
        let why = format!("the round could not finish on {commit}: {error:#}");
        roll_back(round, data, &plan, &conn, &meta, why, &[], &moved);
    }
    Ok(())
}

/// The summary of a round whose dispatchers came back, and its history.
fn finish(
    round: &mut Round,
    plan: &Plan,
    conn: &rusqlite::Connection,
    meta: &Meta,
    doors: &[std::result::Result<String, String>],
) -> Result<()> {
    let commit = &plan.release.commit;
    let short = &commit[..12];
    let mut ships: Vec<ShipOutcome> = Vec::new();
    let mut summary: Vec<String> = Vec::new();
    // Each hosted pane catches up on its own; this only says where it is.
    let mut panes: Vec<(String, &'static str)> = Vec::new();
    match crate::fleet::members(conn) {
        Ok(members) => {
            for member in &members {
                if !member.home.join("privateer.sqlite").is_file() {
                    continue;
                }
                if let Ok(store) = Store::open_readonly(&member.home, Caller::Operator) {
                    panes.extend(crate::host::upgrade::pane_outcomes(&store).unwrap_or_default());
                }
            }
        }
        Err(error) => summary.push(format!("hosted panes: not read ({error:#})")),
    }
    let mut names: Vec<String> = plan.dispatchers.iter().map(|t| t.name.clone()).collect();
    for (ship, _) in &panes {
        if !names.contains(ship) {
            names.push(ship.clone());
        }
    }
    for name in names {
        let mut parts = Vec::new();
        if let Some(target) = plan.dispatchers.iter().find(|t| t.name == name) {
            parts.push(format!("dispatcher restarted ({}) on {short}", target.how));
        }
        if let Some((_, pane)) = panes.iter().find(|(ship, _)| *ship == name) {
            parts.push(
                match *pane {
                    "pending" => "pane relaunch pending at its next safe point",
                    "catching_up" => "pane busy: catching up when its turn ends",
                    "current" => "pane on the current release",
                    _ => "pane unversioned: not relaunched",
                }
                .to_owned(),
            );
        }
        let outcome = parts.join("; ");
        summary.push(format!("{name}: {outcome}"));
        ships.push(ShipOutcome {
            ship: name,
            outcome,
        });
    }
    let mut unrestarted = Vec::new();
    for (target, result) in plan.doors.iter().zip(doors) {
        let outcome = match result {
            Ok(_) => format!("door restarted on {short}"),
            Err(e) => {
                unrestarted.push(target.name.clone());
                format!("door restart failed: {e}")
            }
        };
        summary.push(format!("{}: {outcome}", target.name));
        ships.push(ShipOutcome {
            ship: target.name.clone(),
            outcome,
        });
    }
    for left in &plan.left {
        if let Some(name) = left["name"].as_str() {
            summary.push(format!(
                "{name}: left: {}",
                left["reason"].as_str().unwrap_or_default()
            ));
        }
    }
    let brokers = plan
        .release
        .binary("privateer-mcp-broker")
        .and_then(|broker| crate::mcp_broker::differing(&broker));
    let brokers = match brokers {
        Ok(brokers) => brokers,
        Err(error) => {
            summary.push(format!(
                "MCP brokers: not compared with {short}'s ({error:#}); the captain checks them"
            ));
            Vec::new()
        }
    };
    for name in &brokers {
        summary.push(format!(
            "MCP broker {name}: its installed copy differs from {short}'s; when its windows are idle, the captain runs `privateer mcp-broker stop --name {name}` and then `install`"
        ));
    }
    let outcome = if unrestarted.is_empty() {
        "switched"
    } else {
        "switched_with_failures"
    };
    let upgrade = meta.record(
        &meta.kind,
        outcome,
        (&plan.previous.commit, commit),
        meta.rollback_of.clone(),
        ships,
        meta.id.clone(),
    );
    super::record_upgrade(conn, &upgrade)?;
    round.state["outcome"] = json!(outcome);
    if !unrestarted.is_empty() {
        round.state["error"] = json!(format!(
            "{} did not restart; current selects {commit}, and the dispatchers run it",
            unrestarted.join(", ")
        ));
    }
    round.state["summary"] = json!(summary);
    round.state["brokers"] = json!(brokers);
    round.state["history"] = json!([upgrade.id]);
    Ok(())
}

/// Send `current` back to the release the round came from and restart the
/// dispatchers, and the `doors` that moved, on it again; record the round
/// and its rollback in the history as far as the history can be written.
#[allow(clippy::too_many_arguments)]
fn roll_back(
    round: &mut Round,
    data: &Path,
    plan: &Plan,
    conn: &rusqlite::Connection,
    meta: &Meta,
    why: String,
    unhealthy: &[String],
    doors: &[&Target],
) {
    let commit = &plan.release.commit;
    let previous = &plan.previous.commit;
    let mut errors = vec![why];
    let mut history = Vec::new();
    let mut save_record = |record: &UpgradeRecord, errors: &mut Vec<String>| {
        match super::record_upgrade(conn, record) {
            Ok(()) => history.push(record.id.clone()),
            Err(error) => errors.push(format!("its history was not recorded: {error:#}")),
        }
    };
    if let Err(error) = switch(data, previous) {
        round.step(json!({"step": "rollback: switch", "state": "failed",
            "from": commit, "to": previous, "detail": format!("{error:#}")}));
        errors.push(format!(
            "current could not go back to {previous} ({error:#}) and still selects {commit}"
        ));
        let upgrade = meta.record(
            &meta.kind,
            "failed",
            (previous, commit),
            meta.rollback_of.clone(),
            Vec::new(),
            meta.id.clone(),
        );
        save_record(&upgrade, &mut errors);
        round.state["outcome"] = json!("failed");
        round.state["error"] = json!(errors.join("; "));
        round.state["history"] = json!(history);
        return;
    }
    round
        .step(json!({"step": "rollback: switch", "state": "done", "from": commit, "to": previous}));
    let still = restart_dispatchers(round, &plan.dispatchers, previous, "rollback");
    let doors_back = restart_doors(round, doors, "rollback: ");
    let mut ships = Vec::new();
    let mut summary = Vec::new();
    for target in &plan.dispatchers {
        let outcome = if unhealthy.contains(&target.name) {
            format!("failed its health check on {commit}; rolled back to {previous}")
        } else {
            format!("restarted, then rolled back to {previous}")
        };
        let outcome = if still.contains(&target.name) {
            format!("{outcome}, and did not come back on it either: see the round's steps")
        } else {
            outcome
        };
        summary.push(format!("{}: dispatcher {outcome}", target.name));
        ships.push(ShipOutcome {
            ship: target.name.clone(),
            outcome,
        });
    }
    for target in &plan.doors {
        let back = doors
            .iter()
            .position(|door| door.name == target.name)
            .map(|i| &doors_back[i]);
        let outcome = match back {
            None => format!("left running on {previous}"),
            Some(Ok(_)) => format!("door restarted on {commit}, then again on {previous}"),
            Some(Err(e)) => format!("door restarted on {commit}, then failed to restart: {e}"),
        };
        summary.push(format!("{}: {outcome}", target.name));
        ships.push(ShipOutcome {
            ship: target.name.clone(),
            outcome,
        });
    }
    let back = still.is_empty() && doors_back.iter().all(|r| r.is_ok());
    let outcome = if back { "rolled_back" } else { "failed" };
    let upgrade = meta.record(
        &meta.kind,
        outcome,
        (previous, commit),
        meta.rollback_of.clone(),
        ships.clone(),
        meta.id.clone(),
    );
    save_record(&upgrade, &mut errors);
    let rollback = meta.record(
        "rollback",
        if back { "switched" } else { "failed" },
        (commit, previous),
        Some(meta.id.clone()),
        ships,
        uuid::Uuid::new_v4().to_string(),
    );
    save_record(&rollback, &mut errors);
    errors.insert(1, format!("current is back on {previous}"));
    round.state["outcome"] = json!(outcome);
    round.state["rollback"] = json!({"id": rollback.id, "rollback_of": meta.id, "to": previous});
    round.state["error"] = json!(errors.join("; "));
    round.state["summary"] = json!(summary);
    round.state["history"] = json!(history);
}
