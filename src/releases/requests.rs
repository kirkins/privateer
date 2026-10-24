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
    record_and_start(store, args, state, None)
}

/// Scoped captain administration uses the normal upgrade custody and gates,
/// but never the agent production-approval path or a generic dispatch.
pub(crate) fn start_delegated(
    store: &Store,
    expected: &Value,
    context: &crate::door::scoped::DelegatedContext,
) -> Result<Value> {
    if !context.has("fleet:admin") {
        return Err(crate::door::scoped::Denial::Scope.into());
    }
    store.caller().require_operator("upgrade.start")?;
    let commit = required(expected, "commit")?;
    let state = linked::plan(commit)?;
    ensure!(
        state == *expected,
        "upgrade targets changed; request a new dry run"
    );
    record_and_start(store, &json!({"commit":commit}), state, Some(context))
}

fn record_and_start(
    store: &Store,
    args: &Value,
    mut state: Value,
    context: Option<&crate::door::scoped::DelegatedContext>,
) -> Result<Value> {
    let commit = required(args, "commit")?;
    let id = uuid::Uuid::new_v4().to_string();
    let thread = format!("upgrade-request-{id}");
    let requested_by = match store.caller() {
        Caller::Operator => "captain",
        Caller::Agent { actor } => actor,
    };
    state["id"] = json!(id);
    state["requested_by"] = json!(if context.is_some() {
        "delegated-client"
    } else {
        requested_by
    });
    if let Some(context) = context {
        state["provenance"] = context.provenance();
    }
    state["request_home"] = json!(store.home());
    state["request_thread"] = json!(thread);
    state["requested_ms"] = json!(Store::now_ms());
    let agent = matches!(store.caller(), Caller::Agent { .. });
    if agent {
        // Record only runtime-derived intent. Never launch an operator worker
        // with this process's (agent-controlled) environment.
        state["dispatch"] = json!("pending");
        state["selected_at_request"] = json!(super::current()?.map(|release| release.commit));
        #[cfg(target_os = "linux")]
        {
            state["request_cgroup"] = json!(std::fs::read_to_string("/proc/self/cgroup")?);
        }
    } else {
        state["state_path"] = json!(linked::path("linked", &id)?);
    }
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
        let expires_ms: i64 = tx.query_row(
            "SELECT expires_ms FROM approvals WHERE id=?1",
            [approval],
            |row| row.get(0),
        )?;
        state["approval_expires_ms"] = json!(expires_ms);
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
    if agent {
        return Ok(state);
    }
    start(state).map_err(|error| {
        if context.is_some() {
            crate::door::scoped::Denial::OutcomeUnknown.into()
        } else {
            error
        }
    })
}

// Only an operator command or the operator dispatcher calls this. Approval
// stays consumed even if spawning fails; never retry an ambiguous handoff.
fn start(mut state: Value) -> Result<Value> {
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
/// Test installations explicitly disable service-manager access. For queued
/// requests, both this environment and the scope decision belong to the
/// operator dispatcher, not the agent that recorded the request.
pub(super) fn runner(state: &mut Value) -> Result<Command> {
    let image = super::running_image()?;
    #[cfg(target_os = "linux")]
    if std::env::var("PRIVATEER_SERVICE").as_deref() != Ok("none") {
        let mut cgroup =
            std::fs::read_to_string("/proc/self/cgroup").context("read runner cgroup")?;
        if let Some(requester) = state["request_cgroup"].as_str() {
            cgroup.push('\n');
            cgroup.push_str(requester);
        }
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
    if state["requested_by"] != "captain" && state["provenance"]["origin"] != "scoped-door-v2" {
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
    store
        .caller()
        .require_operator("dispatch upgrade requests")?;
    let mut stmt = store.connection().prepare(
        "SELECT value FROM metadata WHERE key LIKE 'upgrade-request:%' ORDER BY key LIMIT 20",
    )?;
    let requests: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut recovered = 0;
    for request in requests {
        // A broken state path, spawn or notification must not starve later
        // requests. Keep diagnostics durable even when delivery itself fails.
        let result = serde_json::from_str(&request)
            .context("decode upgrade request")
            .and_then(|mut request| recover_one(store, &mut request));
        match result {
            Ok(count) => recovered += count,
            Err(error) => {
                eprintln!("privateer: cannot recover upgrade request: {error:#}");
                let recorded = (|| -> Result<()> {
                    let request: Value = serde_json::from_str(&request)?;
                    let id = request["id"].as_str().context("round id")?;
                    store.connection().execute(
                        "UPDATE metadata SET value=json_set(value,'$.recovery_error',?2) WHERE key=?1",
                        params![format!("upgrade-request:{id}"), format!("{error:#}")],
                    )?;
                    Ok(())
                })();
                if let Err(record_error) = recorded {
                    eprintln!("privateer: cannot record upgrade recovery error: {record_error}");
                }
            }
        }
    }
    Ok(recovered)
}

fn recover_one(store: &Store, request: &mut Value) -> Result<usize> {
    let id = request["id"].as_str().context("round id")?.to_owned();
    if !request["outcome"].is_null() {
        notify(request)?;
        return Ok(1);
    }
    if request["dispatch"] == "pending" {
        // Claim before spawning, under the same writer lock used by
        // admission. A crash after claim is ambiguous, never replayed.
        let tx = store.write_transaction()?;
        request["dispatch"] = json!("claimed");
        request["requested_ms"] = json!(Store::now_ms());
        let claimed = tx.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1 AND json_extract(value,'$.dispatch')='pending'",
                params![format!("upgrade-request:{id}"), request.to_string()],
            )?;
        tx.commit()?;
        if claimed == 0 {
            return Ok(0);
        }
        let dispatched = (|| -> Result<()> {
            ensure!(
                request["approval_expires_ms"]
                    .as_i64()
                    .is_some_and(|expires| expires > Store::now_ms()),
                "production approval expired before dispatch; nothing prepared; request a fresh approval"
            );
            let commit = request["commit"].as_str().context("commit")?;
            let selected = json!(super::current()?.map(|release| release.commit));
            // Commits are opaque ids, not sortable versions. Fail closed
            // on selection drift while queued (except the already-current
            // no-op), rather than let an old request undo a later switch.
            ensure!(
                selected == request["selected_at_request"] || selected == commit,
                "selected release changed while upgrade was queued; request a fresh approval"
            );
            // Use only the dispatcher's freshly derived machines/routes.
            let plan = linked::plan(commit)?;
            ensure!(
                plan["environment"] == request["environment"],
                "upgrade machine set or link destinations changed; request a fresh approval"
            );
            request["machines"] = plan["machines"].clone();
            request["state_path"] = json!(linked::path("linked", &id)?);
            store.connection().execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                params![format!("upgrade-request:{id}"), request.to_string()],
            )?;
            start(request.clone())?;
            Ok(())
        })();
        if let Err(error) = dispatched {
            request["outcome"] = json!("failed");
            request["error"] = json!(format!("{error:#}"));
            // Save before mailing so a delivery error can be recovered
            // without launching this already-claimed request again.
            store.connection().execute(
                "UPDATE metadata SET value=?2 WHERE key=?1",
                params![format!("upgrade-request:{id}"), request.to_string()],
            )?;
            eprintln!("privateer: upgrade request {id} failed: {error:#}");
            notify(request)?;
            return Ok(1);
        }
        return Ok(0);
    }
    // The requester/worker's installation can differ from the current
    // dispatcher's. Never infer an outcome from the latter's data dir.
    let state = request["state_path"]
        .as_str()
        .context("request has no recorded round state path")
        .and_then(|path| linked::progress_at(std::path::Path::new(path)));
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
            Some(request.clone())
        }
        Err(_) => None,
    };
    if let Some(state) = terminal {
        notify(&state)?;
        return Ok(1);
    }
    Ok(0)
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

    #[test]
    fn recovery_uses_the_recorded_installation_not_the_dispatchers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir
            .path()
            .join("other-installation/upgrade-linked")
            .join(format!("{id}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let state = json!({"id":id,"kind":"linked","state_path":path,"request_home":home,
            "request_thread":"thread","requested_by":"requester","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "outcome":"switched","requested_ms":0,"machines":[{"name":"local","summary":"local: switched"}]});
        std::fs::write(&path, state.to_string()).unwrap();
        let mut request = state.clone();
        request["outcome"] = Value::Null;
        store
            .connection()
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                params![format!("upgrade-request:{id}"), request.to_string()],
            )
            .unwrap();
        assert_eq!(recover(&store).unwrap(), 1);
        let (body, notices): (String, i64) = store.connection().query_row(
            "SELECT body,(SELECT COUNT(*) FROM mail WHERE recipient='captain') FROM mail WHERE kind='reply'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(body.contains("switched"));
        assert_eq!(notices, 0);
        assert_eq!(recover(&store).unwrap(), 0);
    }

    #[test]
    fn a_notification_error_is_recorded_without_blocking_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        let not_a_home = dir.path().join("file");
        std::fs::write(&not_a_home, "fixture").unwrap();
        let bad = json!({"id":"a-bad","request_home":not_a_home,"outcome":"failed"});
        let good = json!({"id":"b-good","request_home":home,"outcome":"already_current",
            "requested_by":"requester","request_thread":"thread","commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "machines":[{"name":"local","summary":"local: already_current"}]});
        for request in [bad, good] {
            store
                .connection()
                .execute(
                    "INSERT INTO metadata(key,value) VALUES(?1,?2)",
                    params![
                        format!("upgrade-request:{}", request["id"].as_str().unwrap()),
                        request.to_string()
                    ],
                )
                .unwrap();
        }
        assert_eq!(recover(&store).unwrap(), 1);
        let error: String = store.connection().query_row(
            "SELECT json_extract(value,'$.recovery_error') FROM metadata WHERE key='upgrade-request:a-bad'",
            [], |r| r.get(0),
        ).unwrap();
        assert!(!error.is_empty());
        let replies: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE id='upgrade-outcome-b-good'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(replies, 1);
        assert_eq!(recover(&store).unwrap(), 0);
    }

    #[cfg(feature = "dashboard")]
    #[test]
    fn existing_actions_refuse_an_unmigrated_production_expiry_schema() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        store
            .connection()
            .execute("ALTER TABLE approvals DROP COLUMN expires_ms", [])
            .unwrap();
        drop(store);
        let error = Store::open_existing_action(&home, Caller::Operator)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("incompatible existing action schema"),
            "{error}"
        );
        Store::open(&home).unwrap();
        Store::open_existing_action(&home, Caller::Operator).unwrap();
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
