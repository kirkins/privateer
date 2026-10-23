//! Operator-only, two-phase upgrades across directly linked machines. The
//! coordinator is detached too: upgrading its own dispatcher cannot kill it.
use super::{Release, apply, fetch, installation, running_image};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

mod gateway;
mod transfer;
pub(crate) use gateway::serve;

pub(super) fn data() -> Result<PathBuf> {
    installation(Release::from_executable(&running_image()?).as_ref())
}

pub(super) fn valid_id(id: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id),
        "invalid round or upload id"
    );
    Ok(())
}

fn path(kind: &str, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    ensure!(matches!(kind, "prepare" | "linked"), "invalid round kind");
    Ok(data()?
        .join(format!("upgrade-{kind}"))
        .join(format!("{id}.json")))
}

fn save(path: &Path, state: &Value) -> Result<()> {
    let tmp = path.with_extension("new");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(serde_json::to_string_pretty(state)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn lock(path: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another upgrade operation holds {}",
        path.display()
    );
    Ok(file)
}

fn prune(dir: &Path) -> Result<()> {
    let mut states: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| {
            let state: Value = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
            Some((state["requested_ms"].as_i64().unwrap_or(0), p))
        })
        .collect();
    states.sort();
    let excess = states.len().saturating_sub(19);
    for (_, p) in states.into_iter().take(excess) {
        let _ = fs::remove_file(p.with_extension("log"));
        fs::remove_file(p)?;
    }
    Ok(())
}

pub(super) fn spawn(kind: &str, mut state: Value) -> Result<Value> {
    let id = state["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let path = path(kind, &id)?;
    crate::store::private_dirs(path.parent().context("round directory")?)?;
    // Hold through spawn; the worker takes this same lock before doing work.
    let guard = lock(&data()?.join(format!("upgrade-{kind}.lock")))?;
    for entry in fs::read_dir(path.parent().unwrap())? {
        let p = entry?.path();
        if p.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let mut previous: Value = serde_json::from_slice(&fs::read(&p)?)?;
        if previous["outcome"].is_null() {
            ensure!(
                crate::store::Store::now_ms() - previous["requested_ms"].as_i64().unwrap_or(0)
                    >= 30_000,
                "another round is starting"
            );
            // Do not repeat a remote apply after losing its coordinator.
            // Reconcile every recorded custody id before accepting a retry.
            if kind == "linked" {
                for machine in previous["machines"].as_array().into_iter().flatten() {
                    if machine["outcome"] == "applying" {
                        let id = machine["apply"]["id"].as_str().context(
                            "previous apply reply was lost; inspect the machine before retrying",
                        )?;
                        let remote = call_machine(
                            machine,
                            "upgrade.progress",
                            json!({"kind":"apply","id":id}),
                        )?;
                        ensure!(
                            !remote["outcome"].is_null(),
                            "previous machine round {id} is still running"
                        );
                    }
                }
            }
            previous["outcome"] = json!("interrupted");
            save(&p, &previous)?;
        }
    }
    prune(path.parent().unwrap())?;
    state["id"] = json!(id);
    state["kind"] = json!(kind);
    state["requested_ms"] = json!(crate::store::Store::now_ms());
    state["outcome"] = Value::Null;
    let mut command = super::requests::runner(&mut state)?;
    save(&path, &state)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.with_extension("log"))?;
    for name in [
        "PRIVATEER_ROLE",
        "PRIVATEER_ACTOR",
        "PRIVATEER_JOB",
        "PRIVATEER_HOME",
        "PRIVATEER_PRESENCE_HOLDER",
        "PRIVATEER_WAKE",
        "PRIVATEER_CONTROL_CLI",
    ] {
        command.env_remove(name);
    }
    command
        .args(["upgrade", "_linked_worker", kind, &id])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // Reparent before exec: setsid alone leaves a wake supervisor in
            // the ancestry used by crew_scope, and is not detached custody.
            let parent = libc::getpid();
            let child = libc::fork();
            if child == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if child > 0 {
                libc::_exit(0);
            }
            while libc::getppid() == parent {
                libc::usleep(1_000);
            }
            Ok(())
        });
    }
    let child = command.spawn();
    drop(guard);
    match child {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(state)
        }
        Err(error) => {
            state["outcome"] = json!("failed");
            state["error"] = json!(format!("cannot start round: {error}"));
            save(&path, &state)?;
            Ok(state)
        }
    }
}

pub(super) fn progress(kind: &str, id: &str) -> Result<Value> {
    let mut state: Value = serde_json::from_slice(&fs::read(path(kind, id)?)?)?;
    let starting =
        crate::store::Store::now_ms() - state["requested_ms"].as_i64().unwrap_or(0) < 30_000;
    if state["outcome"].is_null()
        && !starting
        && !crate::service::held(&data()?.join(format!("upgrade-{kind}.lock")))?
    {
        state["outcome"] = json!("interrupted");
        state["error"] = json!("runner stopped; inspect recorded machine rounds before retrying");
    }
    Ok(state)
}

/// Close a stranded coordinator only after the operator has inspected its
/// machine rounds. This never cancels a remote apply or asserts it stopped.
pub fn interrupt(args: &[String]) -> Result<Value> {
    ensure!(
        args.len() == 2 && args[1] == "--inspected",
        "usage: privateer upgrade interrupt-linked ID --inspected"
    );
    let path = path("linked", &args[0])?;
    let _guard = lock(&data()?.join("upgrade-linked.lock"))?;
    let mut state: Value = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(state["outcome"].is_null(), "round already finished");
    ensure!(
        crate::store::Store::now_ms() - state["requested_ms"].as_i64().unwrap_or(0) >= 30_000,
        "round may still be starting; wait 30 seconds"
    );
    if let Some(pid) = state["pid"].as_i64() {
        let pid = i32::try_from(pid).context("invalid coordinator pid")?;
        ensure!(pid > 0, "invalid coordinator pid");
        // Conservative even with pid reuse: only ESRCH proves absence.
        let absent = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        ensure!(
            absent,
            "coordinator pid {pid} is still live or cannot be checked"
        );
    }
    state["outcome"] = json!("interrupted");
    state["completed_ms"] = json!(crate::store::Store::now_ms());
    state["interrupted_by"] = json!("operator");
    state["interruption_note"] = json!(
        "operator inspected machine rounds and marked coordinator interrupted; remote outcomes are unchanged"
    );
    save(&path, &state)?;
    Ok(state)
}

pub fn worker(kind: &str, id: &str) -> Result<Value> {
    let path = path(kind, id)?;
    // The spawner releases its lock immediately after spawn. Wait briefly
    // for that handoff, but never queue behind another live round.
    let deadline = Instant::now() + Duration::from_secs(2);
    let guard = loop {
        match lock(&data()?.join(format!("upgrade-{kind}.lock"))) {
            Ok(guard) => break guard,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error),
        }
    };
    let mut state: Value = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(state["outcome"].is_null(), "round already finished");
    ensure!(
        crate::store::Store::now_ms() - state["requested_ms"].as_i64().unwrap_or(0) < 30_000,
        "round startup expired; nothing applied by this worker"
    );
    state["pid"] = json!(std::process::id());
    state["session"] = json!(unsafe { libc::getsid(0) });
    save(&path, &state)?;
    let result = super::requests::check_detachment(&state).and_then(|()| match kind {
        "prepare" => gateway::prepare(&state).map(|report| {
            state["release"] = report;
        }),
        "linked" => execute(&path, &mut state),
        _ => unreachable!(),
    });
    match result {
        Ok(()) => {
            state["outcome"] = json!(if kind == "prepare" {
                "ready"
            } else if state["machines"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|machine| machine["outcome"] == "skipped_old_gateway")
            {
                "switched_with_skips"
            } else if state["machines"]
                .as_array()
                .is_some_and(|machines| !machines.is_empty()
                    && machines.iter().all(|m| m["outcome"] == "already_current"))
            {
                "already_current"
            } else {
                "switched"
            })
        }
        Err(error) => {
            state["outcome"] = json!("failed");
            state["error"] = json!(format!("{error:#}"));
        }
    }
    if let Some(machines) = state["machines"].as_array_mut() {
        for machine in machines {
            if machine["summary"].is_null() {
                let name = machine["name"].as_str().unwrap_or("unknown");
                machine["summary"] = json!(format!(
                    "{name}: not applied (round stopped before this machine switched)"
                ));
            }
        }
    }
    state["completed_ms"] = json!(crate::store::Store::now_ms());
    save(&path, &state)?;
    drop(guard);
    super::requests::notify(&state)?;
    Ok(state)
}

fn call_machine(machine: &Value, command: &str, args: Value) -> Result<Value> {
    let name = machine["name"].as_str().context("machine name")?;
    if name == "local" {
        serve(command, &args)
    } else {
        crate::machines::call_reached_bound(
            name,
            command,
            args,
            Some(Duration::from_secs(120)),
            machine["route_hash"].as_str(),
        )
    }
}

fn wait(machine: &Value, kind: &str, mut round: Value) -> Result<Value> {
    let name = machine["name"].as_str().context("machine name")?;
    if !round["outcome"].is_null() {
        return Ok(round);
    }
    let id = round["id"]
        .as_str()
        .context("gateway returned no round id")?
        .to_owned();
    let deadline =
        Instant::now() + Duration::from_secs(if kind == "prepare" { 90 * 60 } else { 30 * 60 });
    loop {
        ensure!(
            Instant::now() < deadline,
            "{name}: {kind} timed out; round {id} may still be running"
        );
        round = call_machine(machine, "upgrade.progress", json!({"kind":kind,"id":id}))?;
        if !round["outcome"].is_null() {
            return Ok(round);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// No implicit source path is sent to another machine. Each machine's
/// operator configures PRIVATEER_UPGRADE_SOURCE there when a build is needed.
pub fn start(args: &[String]) -> Result<Value> {
    let commit = args.first().context("upgrade needs a commit")?;
    ensure!(
        super::valid_commit(commit),
        "upgrade needs an exact lowercase commit"
    );
    ensure!(
        args[1..].iter().all(|a| a == "--dry-run"),
        "usage: privateer upgrade COMMIT [--local] [--dry-run]"
    );
    let state = plan(commit)?;
    let names: Vec<_> = state["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].clone())
        .collect();
    if args.iter().any(|a| a == "--dry-run") {
        return Ok(
            json!({"dry_run":true,"commit":commit,"machines":names,"note":"probe, prepare all, then apply in listed order; nothing changed"}),
        );
    }
    let initial = spawn("linked", state)?;
    let id = initial["id"].as_str().context("round id")?;
    loop {
        let report = progress("linked", id)?;
        if !report["outcome"].is_null() {
            return Ok(report);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn plan(commit: &str) -> Result<Value> {
    ensure!(
        super::valid_commit(commit),
        "upgrade needs an exact lowercase commit"
    );
    let conn = crate::fleet::registry()?;
    ensure!(
        crate::machines::desk_here(&conn),
        "run a linked upgrade from the captain's desk machine"
    );
    let mut names = crate::machines::upgrade_targets(&conn)?;
    names.push("local".into());
    // Include link destinations, not just display names: retargeting a link
    // must not silently reuse approval for another environment.
    let mut targets = Vec::new();
    for name in &names {
        if name == "local" {
            targets.push(json!({"name":name,"installation":data()?}));
        } else {
            let target: String =
                conn.query_row("SELECT peer FROM machines WHERE name=?1", [name], |r| {
                    r.get(0)
                })?;
            targets.push(json!({"name":name,"link":serde_json::from_str::<Value>(&target)?}));
        }
    }
    let environment = format!("upgrade:{}", crate::core::hash(&json!(targets)));
    Ok(json!({"commit":commit,"environment":environment,
        "machines":targets.iter().map(|target| {
            let mut machine = json!({"name":target["name"]});
            if !target["link"].is_null() { machine["route_hash"] = json!(crate::core::hash(&target["link"])); }
            machine
        }).collect::<Vec<_>>()}))
}

fn execute(path: &Path, state: &mut Value) -> Result<()> {
    let commit = state["commit"].as_str().context("commit")?.to_owned();
    ensure!(
        plan(&commit)?["environment"] == state["environment"],
        "upgrade machine set or link destinations changed; request a fresh approval"
    );
    let count = state["machines"].as_array().context("machines")?.len();
    // Probe ALL before preparing any. Only the precise unknown-command
    // refusal means an old gateway; connection/authentication errors fail.
    for i in 0..count {
        let name = state["machines"][i]["name"].as_str().unwrap().to_owned();
        match call_machine(
            &state["machines"][i],
            "upgrade.probe",
            json!({"commit":commit}),
        ) {
            Ok(probe) => {
                // Local apply refuses a no-op. Verify already-current peers
                // at the barrier, but send them neither prepare nor apply.
                if probe["current_commit"] == commit {
                    ensure!(
                        probe["verified"] == true,
                        "{name}: current target release did not verify"
                    );
                    state["machines"][i]["outcome"] = json!("already_current");
                    state["machines"][i]["summary"] =
                        json!(format!("{name}: already_current ({commit})"));
                }
                state["machines"][i]["probe"] = probe;
            }
            Err(error)
                if error
                    .to_string()
                    .contains("unknown gateway command upgrade.probe") =>
            {
                state["machines"][i]["outcome"] = json!("skipped_old_gateway");
                state["machines"][i]["summary"] = json!(format!(
                    "{name}: skipped (gateway lacks upgrade.probe; install this build by hand)"
                ));
            }
            Err(error) => {
                state["machines"][i]["error"] = json!(error.to_string());
                save(path, state)?;
                return Err(error);
            }
        }
        save(path, state)?;
    }
    for i in 0..count {
        if matches!(
            state["machines"][i]["outcome"].as_str(),
            Some("skipped_old_gateway" | "already_current")
        ) {
            continue;
        }
        let name = state["machines"][i]["name"].as_str().unwrap().to_owned();
        ensure!(
            plan(&commit)?["environment"] == state["environment"],
            "upgrade targets changed during preparation"
        );
        let result = (|| -> Result<Value> {
            let probe = &state["machines"][i]["probe"];
            let upload = if probe["exists"] == false && probe["gh"] == false {
                match probe["platform"].as_str() {
                    Some(platform) => transfer::push(&state["machines"][i], &commit, platform)?,
                    None => None,
                }
            } else {
                None
            };
            let started = call_machine(
                &state["machines"][i],
                "upgrade.prepare",
                json!({"commit":commit,"upload":upload}),
            )?;
            state["machines"][i]["prepare"] = started.clone();
            save(path, state)?;
            let prepared = wait(&state["machines"][i], "prepare", started)?;
            ensure!(
                prepared["outcome"] == "ready",
                "{name}: prepare failed: {prepared}"
            );
            // Fresh verification is mandatory, even if the release existed
            // before this round, and precedes *every* apply call.
            let checked = call_machine(
                &state["machines"][i],
                "upgrade.probe",
                json!({"commit":commit}),
            )?;
            ensure!(
                checked["verified"] == true,
                "{name}: prepared release did not verify"
            );
            Ok(prepared)
        })();
        match result {
            Ok(prepared) => {
                state["machines"][i]["prepare"] = prepared;
                state["machines"][i]["summary"] = json!(format!("{name}: prepared; not switched"));
            }
            Err(error) => {
                state["machines"][i]["error"] = json!(format!("{error:#}"));
                state["machines"][i]["summary"] =
                    json!(format!("{name}: prepare failed; nothing switched anywhere"));
                save(path, state)?;
                return Err(error);
            }
        }
        save(path, state)?;
    }
    state["phase"] = json!("all_prepared");
    save(path, state)?; // Fail closed before the first switch.
    for i in 0..count {
        if matches!(
            state["machines"][i]["outcome"].as_str(),
            Some("skipped_old_gateway" | "already_current")
        ) {
            continue;
        }
        let name = state["machines"][i]["name"].as_str().unwrap().to_owned();
        ensure!(
            plan(&commit)?["environment"] == state["environment"],
            "upgrade targets changed before apply"
        );
        // Persist intent first: a lost reply is an unknown outcome, never a
        // reason to move on or retry an apply blindly.
        state["machines"][i]["outcome"] = json!("applying");
        save(path, state)?;
        let result = call_machine(
            &state["machines"][i],
            "upgrade.apply",
            json!({"commit":commit}),
        )
        .and_then(|round| {
            state["machines"][i]["apply"] = round.clone();
            save(path, state)?;
            wait(&state["machines"][i], "apply", round)
        });
        match result {
            Ok(round) => {
                let outcome = round["outcome"].as_str().unwrap_or("unknown").to_owned();
                state["machines"][i]["apply"] = round;
                state["machines"][i]["outcome"] = json!(outcome);
                state["machines"][i]["summary"] = json!(format!("{name}: {outcome}"));
                save(path, state)?;
                ensure!(
                    outcome == "switched",
                    "{name}: apply ended {outcome}; stopping; already switched machines left in place"
                );
            }
            Err(error) => {
                state["machines"][i]["error"] = json!(format!("{error:#}"));
                state["machines"][i]["summary"] = json!(format!(
                    "{name}: apply result unknown; stopping; inspect its round before recovery"
                ));
                save(path, state)?;
                bail!("{name}: {error:#}; already switched machines left in place");
            }
        }
    }
    Ok(())
}

pub(super) fn latest(data: &Path) -> Value {
    fs::read_dir(data.join("upgrade-linked"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .filter_map(|e| serde_json::from_slice::<Value>(&fs::read(e.path()).ok()?).ok())
        .max_by_key(|v| v["requested_ms"].as_i64().unwrap_or(0))
        .map(|state| {
            state["id"]
                .as_str()
                .and_then(|id| progress("linked", id).ok())
                .unwrap_or(state)
        })
        .unwrap_or(Value::Null)
}
