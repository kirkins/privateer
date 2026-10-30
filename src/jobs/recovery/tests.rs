use super::*;

// Synthetic timestamps are passed to the seam, not sleeps or assumptions about
// the host's wall/boot clock. Real process identities come from children we own.
const NOW: i64 = 1_000_000;
struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    path: PathBuf,
    request: Request,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("alpha");
        let store = Store::open(&home).unwrap();
        let dir = temp.path().join("ledger");
        fs::create_dir(&dir).unwrap();
        fs::write(
            home.join("config.json"),
            json!({"resource_home":dir}).to_string(),
        )
        .unwrap();
        let path = dir.join("resources.sqlite");
        let conn = ledger(&path).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('alpha',?1)",
                [json!({"location":"here"}).to_string()],
            )
            .unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let process = identity(child.id()).unwrap().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let key = reservation_key(store.home(), "job-alpha", "incarnation-alpha");
        let job = json!({"id":"job-alpha","task_id":"task-alpha","incarnation":"incarnation-alpha",
            "command":["true"],"cwd":home,"action":"test","heavy":true,"timeout_secs":60,
            "ledger":path,"status":"failed","created_ms":0,"finished_ms":10,
            "supervisor":process,"payload":process,"cancel_requested":false,"exit_code":1,
            "reason":null,"retry_of":null,"launch_ms":1,"spawned_ms":2});
        // Decode through the native type so fixture omissions cannot accidentally
        // test a record the runtime itself could not retain.
        let job: Job = serde_json::from_value(job).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                params![job.id, serde_json::to_string(&job).unwrap()],
            )
            .unwrap();
        let hold: Reservation =
            serde_json::from_value(json!({"key":key,"home":home,"job_id":job.id,
            "incarnation":job.incarnation,"heavy":true,"state":"failed","created_ms":0,
            "supervisor":process,"payload":process,"ship":"alpha","task":"task-alpha"}))
            .unwrap();
        conn.execute(
            "INSERT INTO reservations VALUES(?1,?2)",
            params![key, serde_json::to_string(&hold).unwrap()],
        )
        .unwrap();
        let reference = Target::open(&store, false)
            .unwrap()
            .exact(&store, &job.id)
            .unwrap()
            .0;
        let request = Request {
            reference,
            authority: Authority {
                grant: "grant-alpha".into(),
                resource: "resource-alpha".into(),
                grant_revision: "one".into(),
                resource_revision: "one".into(),
                visibility_revision: "one".into(),
                link_incarnation: None,
            },
            idempotency_key: "key-alpha".into(),
        };
        Self {
            _temp: temp,
            store,
            path,
            request,
        }
    }
    fn issue(&self) -> String {
        issue(
            &self.store,
            &self.request,
            NOW,
            &json!({"channel":"test_terminal"}),
        )
        .unwrap()["approval_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn apply(&self, id: &str) -> Result<Value> {
        apply(&self.store, &self.request, id, NOW + 1)
    }
    fn held(&self) -> bool {
        Connection::open(&self.path)
            .unwrap()
            .query_row("SELECT EXISTS(SELECT 1 FROM reservations)", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
    fn change_job(&mut self, f: impl FnOnce(&mut Job)) {
        let mut job = load(&self.store, "job-alpha").unwrap();
        f(&mut job);
        self.store
            .connection()
            .execute(
                "UPDATE jobs SET record=?1 WHERE id='job-alpha'",
                [serde_json::to_string(&job).unwrap()],
            )
            .unwrap();
    }
    fn refresh(&mut self) {
        self.request.reference = Target::open(&self.store, false)
            .unwrap()
            .exact(&self.store, "job-alpha")
            .unwrap()
            .0;
    }
}
fn denied<T: std::fmt::Debug>(result: Result<T>, code: &str) {
    assert_eq!(result.unwrap_err().to_string(), code);
}

#[test]
fn missing_expired_revoked_and_consumed_permits_preserve_holds() {
    let f = Fixture::new();
    denied(f.apply("missing"), "recovery_approval_required");
    let id = f.issue();
    denied(
        apply(&f.store, &f.request, &id, NOW + PERMIT_MS),
        "recovery_approval_stale",
    );
    dispatch(
        &f.store,
        "resource.recovery_permit.revoke",
        &json!({"approval_id":id}),
    )
    .unwrap();
    denied(f.apply(&id), "recovery_approval_stale");
    assert!(f.held());
    let g = Fixture::new();
    let id = g.issue();
    Connection::open(&g.path)
        .unwrap()
        .execute(
            "UPDATE resource_recovery_permits SET consumed_result='old-result' WHERE id=?1",
            [&id],
        )
        .unwrap();
    denied(g.apply(&id), "recovery_approval_stale");
    assert!(g.held());
}

#[test]
fn atomic_success_restart_and_replay_after_deletion_and_expiry() {
    let f = Fixture::new();
    let id = f.issue();
    let before = fs::read(f.store.home().join("config.json")).unwrap();
    let job = load(&f.store, "job-alpha").unwrap();
    let result = f.apply(&id).unwrap();
    assert!(!f.held());
    dispatch(
        &f.store,
        "resource.recovery_permit.revoke",
        &json!({"approval_id":id}),
    )
    .unwrap();
    let store = Store::open_existing_action(f.store.home(), Caller::Operator).unwrap();
    assert_eq!(
        apply(&store, &f.request, &id, NOW + PERMIT_MS + 1).unwrap(),
        result
    );
    assert_eq!(
        serde_json::to_value(load(&store, "job-alpha").unwrap()).unwrap(),
        serde_json::to_value(job).unwrap()
    );
    assert_eq!(fs::read(store.home().join("config.json")).unwrap(), before);
    let conn = Connection::open(&f.path).unwrap();
    let consumed: String = conn
        .query_row(
            "SELECT consumed_result FROM resource_recovery_permits WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(consumed, result["result_id"].as_str().unwrap());
    assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM resource_recovery_audit WHERE json_extract(record,'$.event')='permit_consumed')", [], |r| r.get::<_,bool>(0)).unwrap());
}

#[test]
fn conflicting_keys_and_modified_authority_do_not_consume_again() {
    let f = Fixture::new();
    let id = f.issue();
    let mut changed = f.request.clone();
    changed.authority.visibility_revision = "two".into();
    denied(
        apply(&f.store, &changed, &id, NOW + 1),
        "recovery_approval_stale",
    );
    f.apply(&id).unwrap();
    denied(
        apply(&f.store, &changed, &id, NOW + 2),
        "unsupported_arguments",
    );
    let mut fresh = f.request.clone();
    fresh.idempotency_key = "another-key".into();
    denied(
        apply(&f.store, &fresh, &id, NOW + 2),
        "recovery_approval_stale",
    );
}

#[test]
fn nonterminal_missing_identities_and_grace_are_never_eligible() {
    for status in ["starting", "running", "orphaned", "parked"] {
        let mut f = Fixture::new();
        let id = f.issue();
        f.change_job(|j| j.status = status.into());
        denied(f.apply(&id), "recovery_not_eligible");
        assert!(f.held());
    }
    let mut f = Fixture::new();
    let id = f.issue();
    f.change_job(|j| j.payload = None);
    denied(f.apply(&id), "recovery_identity_unknown");
    assert!(f.held());
    let mut f = Fixture::new();
    f.change_job(|j| j.launch_ms = Some(NOW));
    denied(
        issue(&f.store, &f.request, NOW, &json!({})),
        "recovery_not_eligible",
    );
    assert!(f.held());
}

#[test]
fn live_supervisor_payload_and_escaped_descendants_preserve_capacity() {
    for field in ["supervisor", "payload", "escaped"] {
        let mut f = Fixture::new();
        let id = f.issue();
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let p = identity(child.id()).unwrap().unwrap();
        f.change_job(|j| match field {
            "supervisor" => j.supervisor = Some(p),
            "payload" => j.payload = Some(p),
            _ => j.escaped.push(p),
        });
        let result = f.apply(&id);
        child.kill().unwrap();
        child.wait().unwrap();
        denied(result, "recovery_process_alive");
        assert!(f.held());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn inaccessible_namespace_and_malformed_identity_are_unknown() {
    for malformed in [false, true] {
        let mut f = Fixture::new();
        let id = f.issue();
        f.change_job(|j| {
            let p = j.supervisor.as_mut().unwrap();
            if malformed {
                p.started = "linux:invalid".into();
            } else {
                p.pid_namespace = Some("pid:[unknown]".into());
            }
        });
        denied(f.apply(&id), "recovery_identity_unknown");
        assert!(f.held());
    }
}

#[test]
fn reused_pid_is_not_the_recorded_process() {
    let mut f = Fixture::new();
    let mut reused = identity(std::process::id()).unwrap().unwrap();
    #[cfg(target_os = "linux")]
    {
        reused.started = format!("linux:{}:18446744073709551615", boot_id().unwrap());
    }
    #[cfg(not(target_os = "linux"))]
    {
        reused.started = "different-start".into();
    }
    f.change_job(|j| {
        j.supervisor = Some(reused.clone());
        j.payload = Some(reused);
    });
    let id = f.issue();
    f.apply(&id).unwrap();
    assert!(!f.held());
}

#[test]
fn changed_job_hold_generation_home_ledger_and_machine_refuse() {
    for field in [
        "home",
        "ledger",
        "machine",
        "job",
        "generation",
        "fingerprint",
        "task",
        "ship",
        "incarnation",
    ] {
        let f = Fixture::new();
        let id = f.issue();
        let mut request = f.request.clone();
        match field {
            "home" => request.reference.home.push('x'),
            "ledger" => request.reference.ledger.push('x'),
            "machine" => request.reference.machine_incarnation.push('x'),
            "job" => request.reference.job.push('x'),
            "generation" => request.reference.generation.push('x'),
            "fingerprint" => request.reference.fingerprint.push('x'),
            "task" => request.reference.task.push('x'),
            "ship" => request.reference.ship.push('x'),
            _ => request.reference.incarnation.push('x'),
        }
        let code = if ["home", "ledger", "machine", "ship"].contains(&field) {
            "recovery_target_stale"
        } else {
            "recovery_approval_stale"
        };
        denied(apply(&f.store, &request, &id, NOW + 1), code);
        assert!(f.held());
    }
    let mut f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    let (key, raw): (String, String) = conn
        .query_row("SELECT key,record FROM reservations", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    conn.execute("DELETE FROM reservations WHERE key=?1", [&key])
        .unwrap();
    conn.execute("INSERT INTO reservations VALUES(?1,?2)", params![key, raw])
        .unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    f.refresh();
    assert_ne!(
        f.request.reference.generation,
        serde_json::from_str::<Value>(
            &conn
                .query_row(
                    "SELECT binding FROM resource_recovery_permits WHERE id=?1",
                    [&id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap()
        )
        .unwrap()["reference"]["generation"]
    );
    assert!(f.held());
}

#[test]
fn changed_actual_home_job_and_ledger_are_fenced() {
    let mut f = Fixture::new();
    let id = f.issue();
    f.change_job(|j| j.incarnation = "replacement".into());
    denied(f.apply(&id), "recovery_target_stale");
    assert!(f.held());
    let f = Fixture::new();
    let id = f.issue();
    f.store
        .connection()
        .execute(
            "UPDATE metadata SET value='replacement' WHERE key='runtime_origin'",
            [],
        )
        .unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    assert!(f.held());
    let f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    conn.execute(
        "UPDATE resource_action_identity SET origin=?1",
        [Uuid::new_v4().to_string()],
    )
    .unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    assert!(f.held());
}

#[test]
fn audit_failure_rolls_back_deletion_consumption_and_result() {
    let f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_recovery_audit BEFORE INSERT ON resource_recovery_audit BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(f.apply(&id).is_err());
    assert!(f.held());
    assert!(
        conn.query_row(
            "SELECT consumed_result IS NULL FROM resource_recovery_permits WHERE id=?1",
            [&id],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert!(
        !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM resource_recovery_results)",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    conn.execute_batch("DROP TRIGGER fail_recovery_audit")
        .unwrap();
    f.apply(&id).unwrap();
}

#[test]
fn concurrent_equal_calls_coordinate_and_conflicting_calls_refuse() {
    let f = Fixture::new();
    let id = f.issue();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (home, request, id, barrier) = (
                f.store.home().to_owned(),
                f.request.clone(),
                id.clone(),
                barrier.clone(),
            );
            thread::spawn(move || {
                let store = Store::open_existing_action(&home, Caller::Operator).unwrap();
                store.connection().busy_timeout(LEDGER_BUSY).unwrap();
                barrier.wait();
                apply(&store, &request, &id, NOW + 1)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().unwrap().unwrap())
        .collect();
    assert_eq!(results[0], results[1]);
    assert!(!f.held());
    let mut changed = f.request.clone();
    changed.authority.grant_revision = "two".into();
    denied(
        apply(&f.store, &changed, &id, NOW + 2),
        "unsupported_arguments",
    );
}

#[test]
fn legacy_probes_actions_and_issuance_never_migrate() {
    let f = Fixture::new();
    let conn = Connection::open(&f.path).unwrap();
    conn.execute_batch("DROP TABLE resource_recovery_permits")
        .unwrap();
    let before: String = conn
        .query_row("SELECT group_concat(sql) FROM sqlite_master", [], |r| {
            r.get(0)
        })
        .unwrap();
    denied(
        dispatch(
            &f.store,
            "resource.recovery.reference",
            &json!({"job_id":"job-alpha"}),
        ),
        "recovery_target_stale",
    );
    denied(f.apply("missing"), "recovery_target_stale");
    denied(
        issue(&f.store, &f.request, NOW, &json!({})),
        "recovery_target_stale",
    );
    assert_eq!(
        before,
        conn.query_row::<String, _, _>("SELECT group_concat(sql) FROM sqlite_master", [], |r| r
            .get(0))
            .unwrap()
    );
    assert!(f.held());
}

#[test]
fn retention_is_bounded_and_audit_survives_result_expiry() {
    let f = Fixture::new();
    let id = f.issue();
    f.apply(&id).unwrap();
    let conn = Connection::open(&f.path).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    for n in 0..durable::AUDIT_KEEP + 5 {
        audit(&tx, NOW, json!({"event":"fixture","n":n})).unwrap();
    }
    let rows: i64 = tx
        .query_row("SELECT COUNT(*) FROM resource_recovery_audit", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(rows > 0 && rows <= durable::AUDIT_KEEP);
    prune(&tx, NOW + durable::RETENTION_MS + PERMIT_MS + 2).unwrap();
    assert!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_recovery_results)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert!(
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_recovery_audit)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    tx.commit().unwrap();
}

#[test]
fn result_and_permit_capacity_refuse_without_eviction() {
    let f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    for n in 0..durable::PER_GRANT {
        conn.execute(
            "INSERT INTO resource_recovery_results VALUES(?1,?2,'fixture',?3,'{}')",
            params![format!("key-{n}"), f.request.authority.grant, NOW],
        )
        .unwrap();
    }
    denied(f.apply(&id), "retry_capacity");
    assert!(f.held());
    let mut r = f.request.clone();
    for n in 1..durable::PER_GRANT {
        r.idempotency_key = format!("permit-key-{n}");
        issue(&f.store, &r, NOW, &json!({})).unwrap();
    }
    denied(issue(&f.store, &r, NOW, &json!({})), "retry_capacity");
    assert!(f.held());
}

#[test]
fn issuance_is_not_dispatched_and_requests_are_closed() {
    let mut f = Fixture::new();
    assert!(
        crate::dispatch(
            &mut f.store,
            "resource.recovery_permit.issue",
            json!({"request":f.request})
        )
        .is_err()
    );
    let mut request = json!(f.request);
    request["force"] = json!(true);
    denied(Request::parse(&request), "unsupported_arguments");
    let mut request = json!(f.request);
    request["authority"]["link_incarnation"] = json!("client-selected");
    denied(Request::parse(&request), "unsupported");
}

#[test]
fn a_payload_group_outliving_its_leader_keeps_the_hold() {
    let mut f = Fixture::new();
    let id = f.issue();
    let file = f._temp.path().join("child-pid");
    let mut leader = Command::new("sh")
        .args([
            "-c",
            "read ready; sleep 3600 & echo $! > \"$1\"",
            "alpha",
            file.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let recorded = identity(leader.id()).unwrap().unwrap();
    f.change_job(|j| j.payload = Some(recorded));
    leader.stdin.take().unwrap().write_all(b"go\n").unwrap();
    assert!(leader.wait().unwrap().success());
    let child: u32 = fs::read_to_string(file).unwrap().trim().parse().unwrap();
    let owned = identity(child).unwrap().unwrap();
    let result = f.apply(&id);
    if same_process(&owned).unwrap() {
        unsafe {
            libc::kill(child as i32, libc::SIGKILL);
        }
    }
    denied(result, "recovery_process_alive");
    assert!(f.held());
}

#[test]
fn saturated_descendant_evidence_is_not_absence() {
    let mut f = Fixture::new();
    let id = f.issue();
    f.change_job(|j| j.escaped = vec![j.payload.clone().unwrap(); 256]);
    denied(f.apply(&id), "recovery_identity_unknown");
    assert!(f.held());
}

#[test]
fn replay_rechecks_current_local_ownership_without_requiring_the_deleted_hold() {
    let f = Fixture::new();
    let id = f.issue();
    let result = f.apply(&id).unwrap();
    f.store
        .connection()
        .execute(
            "UPDATE ships SET record=?1 WHERE name='alpha'",
            [json!({"location":"elsewhere"}).to_string()],
        )
        .unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    f.store
        .connection()
        .execute(
            "UPDATE ships SET record=?1 WHERE name='alpha'",
            [json!({"location":"here"}).to_string()],
        )
        .unwrap();
    assert_eq!(f.apply(&id).unwrap(), result);
}

#[test]
fn identical_database_copy_at_a_new_inode_is_a_different_target() {
    let f = Fixture::new();
    let id = f.issue();
    // checkpoint before copying; no native initializer is invoked by recovery.
    Connection::open(&f.path)
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let replacement = f._temp.path().join("replacement.sqlite");
    fs::copy(&f.path, &replacement).unwrap();
    fs::rename(&replacement, &f.path).unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    assert!(f.held());
}

#[test]
fn policy_and_recovery_share_per_grant_and_global_result_bounds() {
    let f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    for n in 0..durable::PER_GRANT {
        tx.execute("INSERT INTO resource_action_results VALUES(?1,'resource-alpha',?2,'fixture',?3,'{}','{}')",params![f.request.authority.grant,format!("policy-{n}"),NOW]).unwrap();
    }
    tx.commit().unwrap();
    denied(f.apply(&id), "retry_capacity");
    assert!(f.held());
    conn.execute("DELETE FROM resource_action_results", [])
        .unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    for n in 0..durable::GLOBAL {
        tx.execute(
            "INSERT INTO resource_recovery_results VALUES(?1,?2,'fixture',?3,'{}')",
            params![format!("global-{n}"), format!("grant-{n}"), NOW],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert!(!durable::has_capacity(&conn, "new-grant").unwrap());
    denied(f.apply(&id), "retry_capacity");
    assert!(f.held());
}

#[test]
fn changed_hold_bytes_refuse_without_consuming_the_permit() {
    let f = Fixture::new();
    let id = f.issue();
    let conn = Connection::open(&f.path).unwrap();
    conn.execute(
        "UPDATE reservations SET record=json_set(record,'$.state','changed')",
        [],
    )
    .unwrap();
    denied(f.apply(&id), "recovery_target_stale");
    assert!(f.held());
    assert!(
        conn.query_row(
            "SELECT consumed_result IS NULL FROM resource_recovery_permits WHERE id=?1",
            [&id],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}

#[test]
fn corrupt_historical_results_are_unknown_not_new_execution() {
    let f = Fixture::new();
    let id = f.issue();
    f.apply(&id).unwrap();
    let conn = Connection::open(&f.path).unwrap();
    for raw in ["{", "{}"] {
        conn.execute("UPDATE resource_recovery_results SET result=?1", [raw])
            .unwrap();
        denied(f.apply(&id), "outcome_unknown");
        assert!(!f.held());
    }
    assert_eq!(
        closed(anyhow::anyhow!("canary path and process detail")).to_string(),
        "execution_failed"
    );
}

#[test]
fn policy_lookup_expires_recovery_results_but_preserves_recovery_audit() {
    let f = Fixture::new();
    let id = f.issue();
    f.apply(&id).unwrap();
    let conn = Connection::open(&f.path).unwrap();
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert!(
        durable::lookup(
            &tx,
            "grant-alpha",
            "resource-alpha",
            "new-key",
            NOW + durable::RETENTION_MS + 2
        )
        .unwrap()
        .is_none()
    );
    assert!(
        !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_recovery_results)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert!(tx.query_row("SELECT EXISTS(SELECT 1 FROM resource_recovery_audit WHERE json_extract(record,'$.event')='permit_consumed')", [], |r| r.get::<_,bool>(0)).unwrap());
    tx.commit().unwrap();
}
