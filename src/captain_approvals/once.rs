//! Runtime receipts for explicitly one-time approvals. A launch is not a
//! receipt, nor is an agent's report: only a successful exact retry recorded
//! by the hosted harness after it carried the grant can retire it. This is
//! the same workflow trust boundary as the runtime's refusal watcher, not OS
//! isolation against a process that writes its own transcript or database.
use super::*;
use std::path::Path;

pub(super) fn is_once(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM captain_approval_once WHERE approval_id=?1)",
        [id],
        |r| r.get(0),
    )?)
}

pub(super) fn used(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM captain_approval_once WHERE approval_id=?1 AND used_ms IS NOT NULL)",
        [id], |r| r.get(0),
    )?)
}

// Include process identity and the byte-exact carried set in the fence. A
// refreshed pane cannot lend its new approvals to a result of the old one.
type Pane = (i64, String, i64, String);
fn pane(store: &Store, ship: &str) -> Result<Option<Pane>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT pid,started,approvals_loaded_ms,approvals_ids FROM hosted_panes
         WHERE ship=?1 AND approvals_ids IS NOT NULL AND approvals_loaded_ms IS NOT NULL",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?)
}

/// Called by the runtime's hosted-pane watcher, never exposed as an agent
/// command. Unreadable or ambiguous evidence leaves the slot occupied.
pub(crate) fn observe_retries(
    store: &Store,
    ship: &str,
    projects: &Path,
    folder: &Path,
) -> Result<()> {
    let now = Store::now_ms();
    let rows: Vec<Row> = live(store, ship, now)?
        .into_iter()
        .filter(|r| r.one_time)
        .collect();
    let Some(before) = pane(store, ship)? else {
        return Ok(());
    };
    let carried: Vec<String> = serde_json::from_str(&before.3).unwrap_or_default();
    let Some(since) = rows
        .iter()
        .filter(|r| carried.contains(&r.id))
        .map(|r| r.created_ms)
        .min()
    else {
        return Ok(());
    };
    let Some(scan) =
        crate::claude_denials::since(projects, folder, since - MAX_TTL_SECS as i64 * 1000)
    else {
        return Ok(());
    };
    record_retries(store, ship, &before, &scan)
}

fn record_retries(
    store: &Store,
    ship: &str,
    before: &Pane,
    scan: &crate::claude_denials::Scan,
) -> Result<()> {
    let tx = store.write_transaction()?;
    if pane(store, ship)?.as_ref() != Some(before) {
        return Ok(());
    }
    // Authority can change during transcript I/O or while waiting for the
    // writer. Re-read grants and time under that same lock as their receipts.
    let now = Store::now_ms();
    let rows: Vec<Row> = live(store, ship, now)?
        .into_iter()
        .filter(|r| r.one_time)
        .collect();
    let carried: Vec<String> = serde_json::from_str(&before.3).unwrap_or_default();
    for denial in &scan.denials {
        let (Some(session), Some(passed_session), Some(started), Some(passed), Some(call)) = (
            denial["session"].as_str(),
            denial["passed_session"].as_str(),
            denial["passed_started_ms"].as_i64(),
            denial["passed_ms"].as_i64(),
            denial["passed_use_id"].as_str(),
        ) else {
            continue;
        };
        if session.is_empty()
            || session != passed_session
            || scan.session.as_deref() != Some(session)
            || started < before.2
            || passed < started
            || passed > now
            || call.is_empty()
            || denial["tool_name"] != "Bash"
            || denial["classifier"] != true
        {
            continue;
        }
        let Some(head) = denial["tool_input"]["command"]
            .as_str()
            .and_then(command_head)
        else {
            continue;
        };
        let evidence = crate::core::hash(
            &json!({"transcript": scan.transcript, "session": session, "call": call}),
        );
        let recorded: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM captain_approval_once WHERE evidence=?1)",
            [&evidence],
            |r| r.get(0),
        )?;
        if recorded {
            continue;
        }
        for row in &rows {
            if !carried.contains(&row.id)
                || state(&tx, row, now) != "live"
                || row.created_ms > before.2
                || started <= row.created_ms
                || passed >= row.expires_ms
                || denial["denied_ms"]
                    .as_i64()
                    .is_none_or(|ms| ms >= row.created_ms)
                || !row.prefix.as_deref().is_some_and(|p| covers(p, &head))
            {
                continue;
            }
            // The frozen request must still be the one that granted this
            // ship/task/prefix. Never infer once from old TTL rows.
            let request: Option<String> = tx
                .query_row(
                    "SELECT payload FROM mail WHERE id=?1 AND sender=?2 AND task_id=?3",
                    params![row.request, ship, row.task],
                    |r| r.get(0),
                )
                .optional()?;
            let valid = request
                .and_then(|p| serde_json::from_str::<Value>(&p).ok())
                .is_some_and(|p| {
                    p["approves"]["one_time"] == true
                        && p["approves"]["action"].as_str() == Some(&row.action)
                        && p["approves"]["command_prefix"].as_str() == row.prefix.as_deref()
                });
            if !valid {
                continue;
            }
            if tx.execute(
                "UPDATE captain_approval_once SET used_ms=?2,evidence=?3
                 WHERE approval_id=?1 AND used_ms IS NULL",
                params![row.id, passed, evidence],
            )? == 1
            {
                store.audit(
                    "permissions.approval_used",
                    &json!({"id":row.id,"ship":ship,
                    "task":row.task,"request":row.request,"used_ms":passed,"evidence":evidence,
                    "pane_pid":before.0,"pane_started":before.1}),
                )?;
                break; // One executed call never retires two approvals.
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dispatch, store::FrozenWallClock};

    fn base() -> i64 {
        crate::routes::iso_timestamp_ms("2026-10-20T00:00:00.000Z").unwrap()
    }

    fn fixture() -> (tempfile::TempDir, Store) {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("home")).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        dispatch(
            &mut store,
            "charter.put",
            json!({"id":"gamma","objective":"Publish beta","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        dispatch(
            &mut store,
            "task.create",
            json!({"id":"beta","charter_id":"gamma","owner":"alpha","title":"Publish beta"}),
        )
        .unwrap();
        dispatch(
            &mut store,
            "ship.put",
            json!({"name":"alpha","scope":"Publish beta"}),
        )
        .unwrap();
        (temp, store)
    }

    fn grant(store: &Store, n: usize, once: bool) -> Value {
        let id = format!("alpha-{n}");
        let ask = json!({"id":id,"from":"alpha","to":"captain","kind":"request","task_id":"beta",
            "state":"open","payload":{"approves":{"action":"Publish beta","command_prefix":"make release","one_time":once}}});
        store.connection().execute(
            "INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state)
             VALUES(?1,'alpha','captain','request','beta','Publish beta',?2,'normal',?3,'open')",
            params![id,ask["payload"].to_string(),base()],
        ).unwrap();
        record(
            store,
            &ask,
            &format!("gamma-{n}"),
            "yes",
            &json!({"operator":true}),
            base() + 3000,
            None,
        )
        .unwrap()
        .unwrap()
    }

    fn pane_with(store: &Store, ids: &[&str]) {
        store.connection().execute(
            "INSERT OR REPLACE INTO hosted_panes(ship,pid,started,executable,recorded_ms,approvals_ids,approvals_loaded_ms)
             VALUES('alpha',123,'alpha','alpha',?1,?2,?1)", params![base()+4000,json!(ids).to_string()],
        ).unwrap();
    }

    fn transcript(
        folder: &Path,
        projects: &Path,
        command: &str,
        session: &str,
        passed_second: u8,
        error: bool,
    ) {
        let dir = projects.join(crate::host::claude_project_name(folder));
        std::fs::create_dir_all(&dir).unwrap();
        let mut out = String::new();
        for (id, sec, failed, session) in [
            ("alpha", 1, true, "alpha"),
            ("beta", passed_second, error, session),
        ] {
            let at = format!("2026-10-20T00:00:{sec:02}.000Z");
            let call = json!({"cwd":folder,"entrypoint":"cli","type":"assistant","sessionId":session,"timestamp":at,
                "message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":command}}]}});
            let result = json!({"cwd":folder,"entrypoint":"cli","type":"user","sessionId":session,"timestamp":at,
                "message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":failed,
                "content":if failed {"Permission for this action was denied by the Claude Code auto mode classifier"} else {"Published beta"}}]}});
            out.push_str(&format!("{call}\n{result}\n"));
        }
        std::fs::write(dir.join("alpha.jsonl"), out).unwrap();
    }

    #[test]
    fn runtime_receipt_frees_only_the_used_once_slot_and_survives_restart() {
        let _clock = FrozenWallClock::at(base() + 10000);
        let (temp, mut store) = fixture();
        let once = grant(&store, 0, true);
        for n in 1..MAX_LIVE {
            grant(&store, n, false);
        }
        let id = once["id"].as_str().unwrap();
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE
        );
        let projects = temp.path().join("projects");
        let folder = temp.path().canonicalize().unwrap();
        pane_with(&store, &[id]);
        // Pickup is not use. Neither can an agent's command free this slot.
        store.set_caller(Caller::Agent {
            actor: "alpha".into(),
        });
        for command in [
            "captain_approval.consume",
            "captain_approval.revoke",
            "approval.consume",
        ] {
            assert!(
                dispatch(
                    &mut store,
                    command,
                    json!({"id":id,"task_id":"beta","artifact":"beta","environment":"gamma"})
                )
                .is_err()
            );
        }
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE
        );
        store.set_caller(Caller::Operator);
        for (command, session, sec, error, carried) in [
            ("make release", "alpha", 6, false, false), // not carried
            ("make releasex", "alpha", 6, false, true), // not the prefix
            ("make release", "gamma", 6, false, true),  // another session
            ("make release", "alpha", 2, false, true),  // before grant
            ("make release", "alpha", 6, true, true),   // refused again
        ] {
            let ids = if carried { vec![id] } else { vec![] };
            pane_with(&store, &ids);
            transcript(&folder, &projects, command, session, sec, error);
            observe_retries(&store, "alpha", &projects, &folder).unwrap();
            assert!(!used(store.connection(), id).unwrap());
        }
        pane_with(&store, &[id]);
        transcript(&folder, &projects, "make release", "alpha", 6, false);
        // Exercise the production watcher integration, not a marking API.
        crate::automode::alert_in(
            &store,
            &[("alpha".into(), "alpha".into(), String::new())],
            Some(&projects),
        )
        .unwrap();
        assert!(used(store.connection(), id).unwrap());
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE - 1
        );
        assert!(
            !recent(&store, "alpha", Store::now_ms())
                .unwrap()
                .iter()
                .any(|r| r.id == id)
        );
        let shown = described(
            &store,
            &select(store.connection(), "id=?1", [id], "id").unwrap(),
            Store::now_ms(),
        )
        .unwrap();
        assert_eq!(shown[0]["state"], "used");
        assert_eq!(shown[0]["one_time"], true);
        assert_eq!(shown[0]["used_ms"], base() + 6000);
        assert!(grant(&store, MAX_LIVE, true)["recorded"] == true);
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE
        );
        let home = store.home().to_path_buf();
        drop(store);
        let mut store = Store::open(&home).unwrap();
        observe_retries(&store, "alpha", &projects, &folder).unwrap();
        let count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='permissions.approval_used'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE
        );
        let standing = live(&store, "alpha", Store::now_ms())
            .unwrap()
            .into_iter()
            .find(|r| !r.one_time)
            .unwrap()
            .id;
        dispatch(
            &mut store,
            "captain_approval.revoke",
            json!({"id":standing}),
        )
        .unwrap();
        assert_eq!(
            live(&store, "alpha", Store::now_ms()).unwrap().len(),
            MAX_LIVE - 1
        );
    }

    #[test]
    fn cap_yes_tells_captain_and_agent_the_holders_without_recording_a_grant() {
        let _clock = FrozenWallClock::at(base() + 10000);
        let (_temp, mut store) = fixture();
        for n in 0..MAX_LIVE {
            grant(&store, n, false);
        }
        let denied = grant(&store, MAX_LIVE, true);
        assert_eq!(denied["recorded"], false);
        let sent = dispatch(&mut store, "mail.send", json!({"from":"captain","to":"alpha","kind":"reply","correlation":"alpha-8","body":"yes"})).unwrap();
        assert_eq!(sent["payload"]["approval"]["recorded"], false);
        let body:String=store.connection().query_row(
            "SELECT body FROM mail WHERE recipient='captain' AND kind='notice' AND json_extract(payload,'$.type')='approval_unavailable'",
            [],|r|r.get(0)).unwrap();
        assert!(
            body.contains("cap is 8")
                && body.contains("8 live")
                && body.contains("Publish beta")
                && body.contains("expires")
                && body.contains("privateer permissions approvals revoke ID"),
            "{body}"
        );
        for row in live(&store, "alpha", Store::now_ms()).unwrap() {
            assert!(body.contains(&row.id));
        }
        let mut payload = json!({"approves":{"action":"Publish beta","command_prefix":"make release","one_time":true}});
        let error = declare(
            &store,
            "beta",
            "alpha",
            "captain",
            "request",
            None,
            Some("beta"),
            &mut payload,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, body);
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 8);
        let id = live(&store, "alpha", Store::now_ms()).unwrap()[0]
            .id
            .clone();
        dispatch(&mut store, "captain_approval.revoke", json!({"id":id})).unwrap();
        assert_eq!(live(&store, "alpha", Store::now_ms()).unwrap().len(), 7);
        // A standing TTL approval remains live until its TTL, independently
        // of a matching successful retry (only once rows can be retired).
        drop(_clock);
        let _expired = FrozenWallClock::at(base() + DEFAULT_TTL_SECS as i64 * 1000 + 3001);
        assert!(live(&store, "alpha", Store::now_ms()).unwrap().is_empty());
    }

    #[test]
    fn an_in_pane_refresh_records_pickup_after_the_original_pane_started() {
        let clock = FrozenWallClock::at(base() + 4000);
        let (temp, store) = fixture();
        let alpha = grant(&store, 0, true);
        let id = alpha["id"].as_str().unwrap();
        pane_with(&store, &[]);
        store.connection().execute(
            "UPDATE hosted_panes SET pid=?1,recorded_ms=?2,approvals_loaded_ms=NULL WHERE ship='alpha'",
            params![std::process::id(), base()],
        ).unwrap();
        let set = launch_set(&store, "alpha", Store::now_ms()).unwrap();
        crate::host::upgrade::record_approvals(&store, "alpha", &set, true, &json!({})).unwrap();
        assert_eq!(pane(&store, "alpha").unwrap().unwrap().2, base() + 4000);
        drop(clock);
        let _clock = FrozenWallClock::at(base() + 10000);
        let projects = temp.path().join("projects");
        let folder = temp.path().canonicalize().unwrap();
        transcript(&folder, &projects, "make release", "alpha", 6, false);
        observe_retries(&store, "alpha", &projects, &folder).unwrap();
        assert!(used(store.connection(), id).unwrap());
    }

    #[test]
    fn one_executed_call_retires_only_one_overlapping_grant_even_after_replay() {
        let _clock = FrozenWallClock::at(base() + 10000);
        let (temp, store) = fixture();
        let alpha = grant(&store, 0, true);
        let beta = grant(&store, 1, true);
        let ids = [alpha["id"].as_str().unwrap(), beta["id"].as_str().unwrap()];
        pane_with(&store, &ids);
        let projects = temp.path().join("projects");
        let folder = temp.path().canonicalize().unwrap();
        transcript(&folder, &projects, "make release", "alpha", 6, false);
        for _ in 0..2 {
            observe_retries(&store, "alpha", &projects, &folder).unwrap();
            assert_eq!(live(&store, "alpha", Store::now_ms()).unwrap().len(), 1);
        }
        assert_eq!(
            ids.iter()
                .filter(|id| used(store.connection(), id).unwrap())
                .count(),
            1
        );
    }

    #[test]
    fn revoke_or_a_changed_pane_between_read_and_receipt_rejects_stale_evidence() {
        let _clock = FrozenWallClock::at(base() + 10000);
        let (temp, mut store) = fixture();
        let alpha = grant(&store, 0, true);
        let id = alpha["id"].as_str().unwrap();
        pane_with(&store, &[id]);
        let before = pane(&store, "alpha").unwrap().unwrap();
        let projects = temp.path().join("projects");
        let folder = temp.path().canonicalize().unwrap();
        transcript(&folder, &projects, "make release", "alpha", 6, false);
        let scan = crate::claude_denials::since(&projects, &folder, base()).unwrap();
        store
            .connection()
            .execute(
                "UPDATE hosted_panes SET started='beta' WHERE ship='alpha'",
                [],
            )
            .unwrap();
        record_retries(&store, "alpha", &before, &scan).unwrap();
        assert!(!used(store.connection(), id).unwrap());
        pane_with(&store, &[id]);
        dispatch(&mut store, "captain_approval.revoke", json!({"id":id})).unwrap();
        record_retries(&store, "alpha", &before, &scan).unwrap();
        assert!(!used(store.connection(), id).unwrap());
        let count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE kind='permissions.approval_used'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn once_requires_an_explicit_boolean_and_a_prefix() {
        let (_temp, store) = fixture();
        for approves in [
            json!({"action":"Publish beta","one_time":true}),
            json!({"action":"Publish beta","one_time":"yes","command_prefix":"make release"}),
        ] {
            let mut payload = json!({"approves":approves});
            assert!(
                declare(
                    &store,
                    "alpha",
                    "alpha",
                    "captain",
                    "request",
                    None,
                    Some("beta"),
                    &mut payload
                )
                .is_err()
            );
        }
        let mut payload = json!({"approves":{"action":"Publish beta","one_time":true,"command_prefix":"make release"}});
        let line = declare(
            &store,
            "alpha",
            "alpha",
            "captain",
            "request",
            None,
            Some("beta"),
            &mut payload,
        )
        .unwrap()
        .unwrap();
        assert!(line.contains("do this once"));
        assert_eq!(payload["approves"]["one_time"], true);
    }
}
