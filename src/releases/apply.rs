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
    Release, ShipOutcome, UpgradeRecord, installation, read_history, running_image, selected,
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

/// The image a live process runs, as the kernel (Linux) or `ps` reports it.
fn process_image(pid: i32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let image = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let text = image.to_string_lossy();
        Some(PathBuf::from(
            text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned(),
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        // Only a full path says which release; a bare name says nothing.
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        path.is_absolute()
            .then(|| fs::canonicalize(&path).unwrap_or(path))
    }
}

fn plan(commit: &str) -> Result<Plan> {
    let image = running_image()?;
    let data = installation(Release::from_executable(&image).as_ref())?;
    let release = Release::new(&data, commit)?;
    ensure!(
        fs::symlink_metadata(&release.root).is_ok(),
        "{} is missing; run `privateer upgrade fetch {commit}` first (upgrade never fetches or builds)",
        release.root.display()
    );
    super::fetch::existing(&release.root, "exists")?;
    let previous = selected(&data)?
        .context("this machine has no current release; run `privateer upgrade adopt` first")?;
    ensure!(
        previous.commit != commit,
        "current already selects {commit}; nothing to switch"
    );
    let entry = release.entrypoint("privateer")?;
    let real_data = fs::canonicalize(&data).unwrap_or_else(|_| data.clone());
    let launchers = super::adopt::launcher_dirs(&image, &data);
    let mut policies = Vec::new();
    let mut pinned = Vec::new();
    let mut left = Vec::new();
    for installed in crate::service::installed_services()? {
        let program = crate::service::program(&installed)?;
        if follows(&program, &entry) {
            if let Some(text) = crate::service::with_stop_policy(&installed)? {
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
        if !service.running()? {
            return skip("not running; left stopped".into());
        }
        let how = match service.managed()? {
            Managed::Unit(path) | Managed::Agent(path) if pinned.contains(&path) => {
                return skip(format!("{} does not follow current", path.display()));
            }
            Managed::Unit(_) => "systemd",
            Managed::Agent(_) => "launchd",
            Managed::Detached => {
                let pid = fs::read_to_string(service.pid_file())
                    .ok()
                    .and_then(|p| p.trim().parse::<i32>().ok());
                let Some(pid) = pid else {
                    return skip(
                        "running, but not started by Privateer (no pid file); restart it by hand"
                            .into(),
                    );
                };
                match process_image(pid) {
                    Some(image) if image.starts_with(&real_data) || image.starts_with(&data) => {
                        "detached"
                    }
                    Some(image) => {
                        return skip(format!(
                            "it runs {}, not a release of this installation; restart it by hand",
                            image.display()
                        ));
                    }
                    None => "detached",
                }
            }
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
                && r.outcome.as_deref() == Some("switched")
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

/// The newest round's state, and whether it is still running, for `upgrade
/// status`.
pub(super) fn last_round(data: &Path) -> Value {
    let Ok(entries) = fs::read_dir(rounds(data)) else {
        return Value::Null;
    };
    let newest = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let state: Value = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((state["requested_ms"].as_i64().unwrap_or(0), state))
        })
        .max_by_key(|(ms, _)| *ms);
    let Some((_, mut state)) = newest else {
        return Value::Null;
    };
    let lock_held = crate::service::held(&data.join("upgrade.lock")).unwrap_or(false);
    state["in_progress"] = json!(state["outcome"].is_null() && lock_held);
    state
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
    let plan = plan(&commit)?;
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
    fn step(&mut self, step: Value) -> Result<()> {
        self.state["steps"]
            .as_array_mut()
            .context("round steps")?
            .push(step);
        save(&self.path, &self.state)
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

/// Wait for a restarted dispatcher to hold its lock again under a new
/// generation, and report the release that generation runs. A dispatcher
/// from before releases recorded their image only proves it came back.
fn came_back(
    target: &Target,
    before: &Option<String>,
    commit: &str,
    until: Instant,
) -> Result<String> {
    let home = target.home.as_deref().context("a dispatcher has a home")?;
    loop {
        let generation = metadata(home, "dispatcher_generation");
        if target.service.running()? && generation.is_some() && generation != *before {
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
            return Ok(format!("running {commit}"));
        }
        ensure!(
            Instant::now() < until,
            "did not come back within {} s",
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
) -> Result<Vec<String>> {
    let mut before = Vec::new();
    for target in targets {
        let home = target.home.as_deref().context("a dispatcher has a home")?;
        before.push(metadata(home, "dispatcher_generation"));
    }
    let mut restarted = Vec::new();
    for target in targets {
        let result = target.service.restart();
        round.step(
            json!({"step": format!("{phase}: restart dispatcher"), "name": target.name,
            "state": if result.is_ok() {"done"} else {"failed"},
            "detail": match &result { Ok(how) => how.clone(), Err(e) => format!("{e:#}") }}),
        )?;
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
        )?;
        if health.is_err() {
            failed.push(target.name.clone());
        }
    }
    Ok(failed)
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

fn run(round: &mut Round, data: &Path) -> Result<()> {
    let _lock = super::adopt::lock(data)?;
    let started_ms = Store::now_ms();
    let commit = round.state["commit"]
        .as_str()
        .context("round commit")?
        .to_owned();
    let kind = round.state["kind"].as_str().unwrap_or("upgrade").to_owned();
    let rollback_of = round.state["rollback_of"].as_str().map(str::to_owned);
    let actor = round.state["actor"]
        .as_str()
        .unwrap_or("operator")
        .to_owned();
    // Planned again under the lock: what runs may have changed meanwhile.
    let plan = plan(&commit)?;
    round.state["from"] = json!(plan.previous.commit);
    round.step(json!({"step": "plan", "state": "done", "plan": plan.json()}))?;

    // A dispatcher's unit must stop its main process alone before it is
    // restarted, so its wakes keep running; systemd reads the change first.
    let mut reload = false;
    for (unit, text, launchd) in &plan.policies {
        let mut name = unit.file_name().unwrap_or_default().to_os_string();
        name.push(".new");
        let new = unit.with_file_name(name);
        let mut file = fs::File::create(&new)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&new, unit)?;
        reload |= !launchd;
        round.step(json!({"step": "stop policy", "unit": unit, "state": "done"}))?;
    }
    if reload {
        crate::service::daemon_reload()?;
        round.step(json!({"step": "daemon-reload", "state": "done"}))?;
    }

    switch(data, &commit)?;
    round.step(
        json!({"step": "switch", "state": "done", "from": plan.previous.commit, "to": commit}),
    )?;
    let failed = restart_dispatchers(round, &plan.dispatchers, &commit, "switch")?;
    let mut ships: Vec<ShipOutcome> = Vec::new();
    let mut summary: Vec<String> = Vec::new();
    let mut history = Vec::new();
    let id = round.state["id"].as_str().unwrap_or_default().to_owned();
    let record = |kind: &str,
                  outcome: &str,
                  from: &str,
                  to: &str,
                  of: Option<String>,
                  ships: Vec<ShipOutcome>,
                  id: String| UpgradeRecord {
        id,
        kind: Some(kind.into()),
        outcome: Some(outcome.into()),
        rollback_of: of,
        from_commit: Some(from.into()),
        to_commit: to.into(),
        actor: actor.clone(),
        started_ms,
        completed_ms: Store::now_ms().max(started_ms),
        ships,
    };
    let conn = crate::fleet::registry()?;
    if !failed.is_empty() {
        // Back to the release that ran, and the same restarts again.
        let previous = plan.previous.commit.clone();
        switch(data, &previous)?;
        round.step(
            json!({"step": "rollback: switch", "state": "done", "from": commit, "to": previous}),
        )?;
        let still = restart_dispatchers(round, &plan.dispatchers, &previous, "rollback")?;
        for target in &plan.dispatchers {
            let outcome = if failed.contains(&target.name) {
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
            summary.push(format!("{}: left running on {previous}", target.name));
        }
        let outcome = if still.is_empty() {
            "rolled_back"
        } else {
            "failed"
        };
        let upgrade = record(
            &kind,
            outcome,
            &plan.previous.commit,
            &commit,
            rollback_of,
            ships.clone(),
            id.clone(),
        );
        super::record_upgrade(&conn, &upgrade)?;
        let rollback = record(
            "rollback",
            if still.is_empty() {
                "switched"
            } else {
                "failed"
            },
            &commit,
            &previous,
            Some(id.clone()),
            ships,
            uuid::Uuid::new_v4().to_string(),
        );
        super::record_upgrade(&conn, &rollback)?;
        history.push(upgrade.id);
        history.push(rollback.id.clone());
        round.state["outcome"] = json!(outcome);
        round.state["rollback"] = json!({"id": rollback.id, "rollback_of": id, "to": previous});
        round.state["error"] = json!(format!(
            "{} did not come back on {commit}; current is back on {previous}",
            failed.join(", ")
        ));
        round.state["summary"] = json!(summary);
        round.state["history"] = json!(history);
        return Ok(());
    }

    // Doors after the dispatchers: they only carry the captain's mail.
    let until = Instant::now() + health_bound();
    let mut doors = Vec::new();
    for target in &plan.doors {
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
        round.step(json!({"step": "restart door", "name": target.name,
            "state": if result.is_ok() {"done"} else {"failed"},
            "detail": match &result { Ok(how) => how.clone(), Err(e) => format!("{e:#}") }}))?;
        doors.push((target.name.clone(), result.map_err(|e| format!("{e:#}"))));
    }

    // Each hosted pane catches up on its own; this only says where it is.
    let mut panes: Vec<(String, &'static str)> = Vec::new();
    let conn_members = crate::fleet::members(&conn)?;
    for member in &conn_members {
        if !member.home.join("privateer.sqlite").is_file() {
            continue;
        }
        if let Ok(store) = Store::open_readonly(&member.home, Caller::Operator) {
            panes.extend(crate::host::upgrade::pane_outcomes(&store).unwrap_or_default());
        }
    }
    let short = &commit[..12];
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
    for (name, result) in &doors {
        let outcome = match result {
            Ok(_) => format!("door restarted on {short}"),
            Err(e) => format!("door restart failed: {e}"),
        };
        summary.push(format!("{name}: {outcome}"));
        ships.push(ShipOutcome {
            ship: name.clone(),
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
    let brokers = crate::mcp_broker::differing(&plan.release.binary("privateer-mcp-broker")?)?;
    for name in &brokers {
        summary.push(format!(
            "MCP broker {name}: its installed copy differs from {short}'s; when its windows are idle, the captain runs `privateer mcp-broker stop --name {name}` and then `install`"
        ));
    }
    let upgrade = record(
        &kind,
        "switched",
        &plan.previous.commit,
        &commit,
        rollback_of,
        ships,
        id,
    );
    super::record_upgrade(&conn, &upgrade)?;
    history.push(upgrade.id);
    round.state["outcome"] = json!("switched");
    round.state["summary"] = json!(summary);
    round.state["brokers"] = json!(brokers);
    round.state["history"] = json!(history);
    Ok(())
}
