//! Agent requests reuse captain production approvals. Consumption and durable
//! request custody commit together; no caller waits for the upgrade it starts.
use super::linked;
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde_json::{Value, json};
use std::process::Command;

pub(crate) const ACCEPTED: crate::Accepted = &[
    ("upgrade.plan", &["commit"]),
    ("upgrade.start", &["commit", "task_id", "approval_id"]),
];

pub(crate) fn dispatch(store: &Store, command: &str, args: &Value) -> Result<Value> {
    let commit = required(args, "commit")?;
    let mut state = linked::plan(commit)?;
    if command == "upgrade.plan" {
        state["dry_run"] = json!(true);
        state["note"] = json!("probe, prepare all, then apply in listed order; nothing changed");
        return Ok(state);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let thread = format!("upgrade-request-{id}");
    let requested_by = match store.caller() {
        Caller::Operator => "captain",
        Caller::Agent { actor } => actor,
    };
    state["id"] = json!(id);
    state["requested_by"] = json!(requested_by);
    state["request_home"] = json!(store.home());
    state["request_thread"] = json!(thread);
    state["requested_ms"] = json!(Store::now_ms());
    let tx = store.write_transaction()?;
    let pending: i64 = tx.query_row(
        "SELECT COUNT(*) FROM metadata WHERE key LIKE 'upgrade-request:%'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        pending < 20,
        "too many pending upgrade requests; inspect outstanding rounds"
    );
    if let Caller::Agent { actor } = store.caller() {
        let task = required(args, "task_id")?;
        let approval = required(args, "approval_id")?;
        ensure!(
            store.task(task)?["owner"] == *actor,
            "upgrade task is not owned by requester"
        );
        ensure!(
            crate::mail::ship(store, actor)?.is_some(),
            "requester has no mailbox here"
        );
        let charter = store.active_charter(task)?;
        // The runtime-derived environment binds the whole ordered target set,
        // each link destination and this installation. Never trust caller text.
        let used = tx.execute(
            "UPDATE approvals SET consumed=1 WHERE id=?1 AND task_id=?2 AND artifact=?3
             AND environment=?4 AND charter_hash=?5 AND approver='captain' AND consumed=0
             AND expires_ms IS NOT NULL AND expires_ms>?6",
            params![
                approval,
                task,
                commit,
                state["environment"].as_str().unwrap(),
                crate::core::hash(&charter),
                Store::now_ms()
            ],
        )?;
        ensure!(
            used == 1,
            "upgrade requires an unused, unexpired captain production approval for this exact commit and machine set"
        );
        state["production_approval"] = json!(approval);
        state["task_id"] = json!(task);
        // This runtime-owned operation thread is returned immediately. Its
        // one terminal reply is delivered to the asking agent, including a wake.
        tx.execute(
            "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
             VALUES(?1,?2,'crew','request',?3,?4,?5,'normal',?6,'open')",
            params![thread, actor, task, format!("Upgrade to {commit}"), state.to_string(), Store::now_ms()],
        )?;
    }
    tx.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2)",
        params![format!("upgrade-request:{id}"), state.to_string()],
    )?;
    store.audit("upgrade.requested", &state)?;
    tx.commit()?;
    // Approval stays consumed even if spawning fails. Retrying requires a new
    // deployment approval; a crash can never turn it back into unused authority.
    match linked::spawn("linked", state.clone()) {
        Ok(started) => {
            if !started["outcome"].is_null() {
                notify(&started)?;
            }
            Ok(started)
        }
        Err(error) => {
            state["outcome"] = json!("failed");
            state["error"] = json!(format!("cannot start round: {error:#}"));
            notify(&state)?;
            Ok(state)
        }
    }
}

/// setsid does not escape a Linux cgroup. On a scoped installation ask the
/// user service manager to put the worker in a sibling transient scope. Scope
/// mode preserves this command's environment and file-backed standard streams.
/// Test installations explicitly disable service-manager access.
pub(super) fn runner(state: &mut Value) -> Result<Command> {
    let image = super::running_image()?;
    #[cfg(target_os = "linux")]
    if std::env::var("PRIVATEER_SERVICE").as_deref() != Ok("none") {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").context("read runner cgroup")?;
        let scoped = cgroup
            .lines()
            .any(|line| line.splitn(3, ':').nth(2).is_some_and(|p| p != "/"));
        if scoped {
            state["caller_cgroup"] = json!(cgroup);
            state["detachment"] = json!("systemd_scope");
            let mut command = Command::new("systemd-run");
            command
                .args(["--user", "--scope", "--collect", "--quiet", "--unit"])
                .arg(format!(
                    "privateer-upgrade-{}",
                    state["id"].as_str().context("round id")?
                ))
                .arg("--")
                .arg(image);
            return Ok(command);
        }
    }
    state["detachment"] = json!("session");
    Ok(Command::new(image))
}

pub(super) fn check_detachment(state: &Value) -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(parent) = state["caller_cgroup"].as_str() {
        let actual = std::fs::read_to_string("/proc/self/cgroup")?;
        // Each controller's path must leave the caller's subtree; a different
        // descendant cgroup is not sufficient protection against its teardown.
        for line in parent.lines() {
            let fields: Vec<_> = line.splitn(3, ':').collect();
            if fields.len() != 3 || fields[2] == "/" {
                continue;
            }
            let mut found = false;
            for child in actual.lines() {
                let child: Vec<_> = child.splitn(3, ':').collect();
                if child.len() == 3 && child[1] == fields[1] {
                    found = true;
                    ensure!(
                        child[2] != fields[2] && !child[2].starts_with(&format!("{}/", fields[2])),
                        "upgrade worker did not leave the caller's cgroup; nothing switched"
                    );
                }
            }
            ensure!(found, "cannot verify the worker's cgroup controller");
        }
    }
    let _ = state;
    Ok(())
}

/// Idempotent transactional mail: a crash after the state save but before this
/// commit is recovered by the next dispatcher pass, never by replaying applies.
pub(super) fn notify(state: &Value) -> Result<()> {
    let Some(home) = state["request_home"].as_str() else {
        return Ok(());
    };
    ensure!(!state["outcome"].is_null(), "round has not ended");
    let store = Store::open(std::path::Path::new(home))?;
    let tx = store.write_transaction()?;
    let id = state["id"].as_str().context("round id")?;
    let outcome = state["outcome"].as_str().context("round outcome")?;
    let mut lines = vec![format!(
        "Upgrade {}: {outcome}",
        state["commit"].as_str().unwrap_or("unknown")
    )];
    for machine in state["machines"].as_array().into_iter().flatten() {
        lines.push(
            machine["summary"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "{}: not applied",
                        machine["name"].as_str().unwrap_or("unknown")
                    )
                }),
        );
    }
    if let Some(error) = state["error"].as_str() {
        lines.push(error.into());
    }
    let body = lines.join("\n");
    if state["requested_by"] != "captain" {
        let thread = state["request_thread"].as_str().context("request thread")?;
        tx.execute(
            "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,body,payload,urgency,created_ms)
             VALUES(?1,'crew',?2,'reply',?3,?4,?5,?6,'normal',?7) ON CONFLICT(id) DO NOTHING",
            params![format!("upgrade-outcome-{id}"), state["requested_by"].as_str(), thread,
                state["task_id"].as_str(), body, state.to_string(), Store::now_ms()],
        )?;
        tx.execute("UPDATE mail SET state='answered',answered_by=?2,answered_ms=?3 WHERE id=?1 AND state='open'",
            params![thread, format!("upgrade-outcome-{id}"), Store::now_ms()])?;
    }
    if matches!(
        outcome,
        "rolled_back" | "switched_with_failures" | "switched_with_skips" | "failed"
    ) {
        crate::mail::record_runtime(
            &store,
            &json!({"id":format!("upgrade-captain-{id}"),
            "from":"crew","to":"captain","kind":"notice","body":body,"payload":state,"urgency":"high"}),
        )?;
    }
    tx.execute(
        "DELETE FROM metadata WHERE key=?1",
        [format!("upgrade-request:{id}")],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn recover(store: &Store) -> Result<usize> {
    let mut stmt = store.connection().prepare(
        "SELECT value FROM metadata WHERE key LIKE 'upgrade-request:%' ORDER BY key LIMIT 20",
    )?;
    let requests: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut recovered = 0;
    for request in requests {
        let mut request: Value = serde_json::from_str(&request)?;
        let id = request["id"].as_str().context("round id")?;
        let state = linked::progress("linked", id);
        let terminal = match state {
            Ok(mut state) if state["outcome"] == "interrupted" => {
                state["outcome"] = json!("failed");
                Some(state)
            }
            Ok(state) if !state["outcome"].is_null() => Some(state),
            Ok(_) => None,
            Err(_) if Store::now_ms() - request["requested_ms"].as_i64().unwrap_or(0) >= 30_000 => {
                request["outcome"] = json!("failed");
                request["error"] = json!(
                    "round custody unavailable; inspect recorded machine rounds before recovery; approval remains consumed"
                );
                Some(request)
            }
            Err(_) => None,
        };
        if let Some(state) = terminal {
            notify(&state)?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_reply_is_once_and_captain_notices_only_the_four_bad_outcomes() {
        for outcome in [
            "switched",
            "already_current",
            "ready",
            "rolled_back",
            "switched_with_failures",
            "switched_with_skips",
            "failed",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let store = Store::open(&home).unwrap();
            let state = json!({"id":uuid::Uuid::new_v4().to_string(),"request_home":home,
                "request_thread":"thread","requested_by":"requester","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "outcome":outcome,"machines":[{"name":"local","summary":"local: fixture result"}]});
            notify(&state).unwrap();
            notify(&state).unwrap();
            let (replies, notices): (i64, i64) = store.connection().query_row(
                "SELECT SUM(kind='reply' AND recipient='requester' AND correlation='thread'),SUM(kind='notice' AND recipient='captain') FROM mail",
                [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert_eq!(replies, 1, "{outcome}");
            assert_eq!(
                notices,
                i64::from(matches!(
                    outcome,
                    "rolled_back" | "switched_with_failures" | "switched_with_skips" | "failed"
                )),
                "{outcome}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_worker_still_in_its_callers_cgroup_is_refused() {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let scoped = cgroup
            .lines()
            .any(|l| l.splitn(3, ':').nth(2).is_some_and(|p| p != "/"));
        assert_eq!(
            check_detachment(&json!({"caller_cgroup":cgroup})).is_err(),
            scoped
        );
    }
}
