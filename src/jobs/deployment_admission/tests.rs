use super::*;
use crate::store::FrozenWallClock;

const GIB: u64 = 1024 * 1024 * 1024;
fn fixture() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let mut store = Store::open(&root.join("alpha")).unwrap();
    fs::write(
        store.home().join("config.json"),
        json!({"resource_home":root.join("gamma")}).to_string(),
    )
    .unwrap();
    crate::dispatch(&mut store, "ship.put", json!({"name":"alpha"})).unwrap();
    let charter = json!({"id":"alpha","active":true,"repo":root,
        "actions":["deploy_nonproduction"],"job_timeout_secs":60});
    store
        .connection()
        .execute(
            "INSERT INTO charters(id,data) VALUES('alpha',?1)",
            [charter.to_string()],
        )
        .unwrap();
    store
        .connection()
        .execute_batch(
            "INSERT INTO tasks(id,charter_id,owner,title,status,depth)
        VALUES('alpha','alpha','alpha','beta','open',0)",
        )
        .unwrap();
    (dir, store)
}
fn estimate() -> Estimate {
    Estimate {
        cpus: Some(1.0),
        memory_bytes: Some(GIB),
        disk_bytes: Some(GIB),
    }
}
fn reading(path: &Path) -> Result<CapacityReading> {
    // Fixed observations exercise the real shared reserve transaction and
    // policy without load, free-space, wall-clock or boot-uptime assumptions.
    Ok(CapacityReading {
        host: admission::Host {
            cpus: 8.0,
            load: 0.0,
            memory_available_bytes: 128 * GIB,
            memory_total_bytes: 128 * GIB,
            disk_available_bytes: 256 * GIB,
            disk_total_bytes: 512 * GIB,
        },
        disk_key: admission::disk_key(path)?,
        available: 256 * GIB,
        total: 512 * GIB,
    })
}
fn admit_fake(store: &Store) -> Result<Admitted> {
    admit_with(store, "alpha", estimate(), 60, reading)
}
fn holds(store: &Store) -> Vec<Reservation> {
    reservations(&shared_ledger(store).unwrap()).unwrap()
}
fn no_deployment_or_job(store: &Store) {
    for table in ["jobs", "deployment_uses", "deployment_intents", "approvals"] {
        let exists: bool = store
            .connection()
            .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!exists, "{table} must stay untouched");
    }
    for command in ["deployment.start", "deployment.show", "deployment.list"] {
        assert!(crate::accepted_args(command).is_none());
    }
}

#[test]
fn admission_is_durable_counted_and_explicitly_compensated_without_spawning() {
    let _clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let reopened = Store::open(store.home()).unwrap();
    let held = holds(&reopened);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].key, admitted.key());
    assert_eq!(held[0].state, "deployment-reserved");
    assert_eq!(held[0].task.as_deref(), Some("alpha"));
    assert_eq!(held[0].ship.as_deref(), Some("alpha"));
    assert!(held[0].heavy && held[0].payload.is_none());
    assert!(same_process(held[0].supervisor.as_ref().unwrap()).unwrap());
    assert_eq!(
        held[0].demand,
        Some(Demand {
            cpus: 1.0,
            memory_bytes: GIB,
            disk_bytes: GIB
        })
    );
    assert!(held[0].disk_key.is_some());
    assert!(held[0].model_route.is_none());
    no_deployment_or_job(&reopened);
    admitted.release_unlaunched().unwrap();
    assert!(holds(&store).is_empty());
}

#[test]
fn explicit_estimates_and_bounded_timeout_are_required() {
    let (_dir, store) = fixture();
    for (estimate, timeout) in [
        (Estimate::default(), 60),
        (
            Estimate {
                cpus: Some(f64::NAN),
                ..estimate()
            },
            60,
        ),
        (
            Estimate {
                memory_bytes: None,
                ..estimate()
            },
            60,
        ),
        (
            Estimate {
                disk_bytes: None,
                ..estimate()
            },
            60,
        ),
        (estimate(), 0),
        (estimate(), 3601),
    ] {
        assert!(admit(&store, "alpha", estimate, timeout).is_err());
    }
    assert!(holds(&store).is_empty());
    no_deployment_or_job(&store);
}

#[test]
fn timeout_is_capped_by_the_current_charter_in_the_shared_admission_path() {
    for (cap, requested, expected) in [
        (Some(7), 60, 7),
        (None, 3600, 900),
        (Some(7200), 3600, 3600),
    ] {
        let (_dir, store) = fixture();
        store
            .connection()
            .execute(
                "UPDATE charters SET data=json_set(data,'$.job_timeout_secs',?1)",
                [cap],
            )
            .unwrap();
        let admitted = admit_with(&store, "alpha", estimate(), requested, reading).unwrap();
        assert_eq!(holds(&store)[0].timeout_ms, Some(expected * 1000));
        admitted.release_unlaunched().unwrap();
    }
    let (_dir, store) = fixture();
    store
        .connection()
        .execute_batch("UPDATE charters SET data=json_set(data,'$.job_timeout_secs',0)")
        .unwrap();
    assert!(admit_fake(&store).is_err());
    assert!(holds(&store).is_empty());
}

#[test]
fn dedicated_charter_local_owner_operator_and_transaction_gates_cannot_be_bypassed() {
    for sql in [
        "UPDATE tasks SET status='done'",
        "UPDATE tasks SET owner='beta'",
        "UPDATE charters SET data=json_set(data,'$.active',json('false'))",
        "UPDATE charters SET data=json_set(data,'$.actions',json('[\"publish_pr\",\"deploy_production\"]'))",
        "UPDATE ships SET record=json_set(record,'$.location',json('{\"peer\":\"beta\"}'))",
    ] {
        let (_dir, store) = fixture();
        store.connection().execute_batch(sql).unwrap();
        assert!(admit_fake(&store).is_err(), "{sql}");
        assert!(holds(&store).is_empty());
        no_deployment_or_job(&store);
    }
    let (_dir, mut store) = fixture();
    store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    assert!(admit_fake(&store).is_err());
    store.set_caller(Caller::Operator);
    let tx = store.write_transaction().unwrap();
    assert!(admit_fake(&store).is_err());
    tx.rollback().unwrap();
    assert!(holds(&store).is_empty());
    for action in ["deploy_nonproduction", "deploy_production"] {
        let result = prepare_start(
            &store,
            json!({"task_id":"alpha","command":["false"],"action":action}),
            None,
            false,
        );
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("ordinary jobs require")
        );
    }
}

#[test]
fn capacity_refusal_and_fleet_stow_leave_no_use_or_queue_entry() {
    let _clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let conn = shared_ledger(&store).unwrap();
    admission::configure(&conn, &json!({"max_heavy_jobs":1})).unwrap();
    let first = admit_fake(&store).unwrap();
    assert!(
        admit_fake(&store)
            .unwrap_err()
            .downcast_ref::<Refused>()
            .is_some()
    );
    assert_eq!(holds(&store).len(), 1);
    first.release_unlaunched().unwrap();
    conn.execute(
        "INSERT INTO stows(id,started_ms,ships) VALUES('beta',1,'[]')",
        [],
    )
    .unwrap();
    assert!(
        admit(&store, "alpha", estimate(), 60)
            .unwrap_err()
            .downcast_ref::<Refused>()
            .is_some()
    );
    assert!(holds(&store).is_empty());
    assert!(
        !conn
            .query_row("SELECT EXISTS(SELECT 1 FROM queue)", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    no_deployment_or_job(&store);
}

#[test]
fn native_floor_charter_cap_and_existing_queue_priority_apply_to_deployments() {
    let _clock = FrozenWallClock::at(10_000);
    for policy in ["host-floor", "charter-floor", "charter-cap", "queue"] {
        let (_dir, store) = fixture();
        let conn = shared_ledger(&store).unwrap();
        let mut first = None;
        match policy {
            "host-floor" => {
                admission::configure(&conn, &json!({"min_free_disk_bytes":300 * GIB})).unwrap();
            }
            "charter-floor" => {
                store
                    .connection()
                    .execute(
                        "UPDATE charters SET data=json_set(data,'$.min_free_bytes',?1)",
                        [300 * GIB],
                    )
                    .unwrap();
            }
            "charter-cap" => {
                store
                    .connection()
                    .execute_batch("UPDATE charters SET data=json_set(data,'$.max_heavy_jobs',1)")
                    .unwrap();
                first = Some(admit_fake(&store).unwrap());
            }
            "queue" => {
                let entry = Queued {
                    key: "beta".into(),
                    home: store.home().to_owned(),
                    job_id: "beta".into(),
                    incarnation: "gamma".into(),
                    created_ms: 10_000,
                    supervisor: identity(std::process::id()).unwrap(),
                    kind: None,
                    capacity: Some(QueuedDemand {
                        heavy: true,
                        demand: Some(Demand {
                            cpus: 1.0,
                            memory_bytes: GIB,
                            disk_bytes: GIB,
                        }),
                    }),
                    blocked: Some(Blocker::HeavySlot),
                    blocked_ms: Some(10_000),
                };
                conn.execute(
                    "INSERT INTO queue(key,record) VALUES(?1,?2)",
                    params![entry.key, serde_json::to_string(&entry).unwrap()],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            admit_fake(&store)
                .unwrap_err()
                .downcast_ref::<Refused>()
                .is_some(),
            "{policy}"
        );
        assert_eq!(holds(&store).len(), usize::from(first.is_some()));
        if let Some(first) = first {
            first.release_unlaunched().unwrap();
        }
        no_deployment_or_job(&store);
    }
}

#[test]
fn token_drop_and_unknown_owner_preserve_capacity_for_manual_recovery() {
    let _clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    drop(admitted);
    let reopened = Store::open(store.home()).unwrap();
    assert_eq!(holds(&reopened).len(), 1, "drop must never free a hold");
    let mut held = holds(&reopened).remove(0);
    // Mismatching start identity proves this is not the current process; it
    // does NOT prove there is no unrecorded payload. No real process is killed.
    held.supervisor.as_mut().unwrap().started = "gamma".into();
    held.created_ms = -100_000;
    let conn = shared_ledger(&reopened).unwrap();
    conn.execute(
        "UPDATE reservations SET record=?2 WHERE key=?1",
        params![key, serde_json::to_string(&held).unwrap()],
    )
    .unwrap();
    assert!(recover(&reopened, &json!({"key":key})).is_err());
    assert_eq!(holds(&reopened).len(), 1);
    recover(&reopened, &json!({"key":key,"force":true})).unwrap();
    assert!(holds(&reopened).is_empty());
    no_deployment_or_job(&reopened);
}

#[test]
fn operator_can_recover_a_dropped_pre_handoff_hold_while_admitting_process_lives() {
    let clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    drop(admitted);
    let mut reopened = Store::open(store.home()).unwrap();
    assert!(same_process(holds(&reopened)[0].supervisor.as_ref().unwrap()).unwrap());
    // Even force does not bypass the grace period.
    assert!(
        crate::dispatch(
            &mut reopened,
            "resource.recover",
            json!({"key":key,"force":true})
        )
        .is_err()
    );
    drop(clock);
    let _clock = FrozenWallClock::at(100_000);
    assert!(crate::dispatch(&mut reopened, "resource.recover", json!({"key":key})).is_err());
    reopened.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    assert!(
        crate::dispatch(
            &mut reopened,
            "resource.recover",
            json!({"key":key,"force":true})
        )
        .is_err()
    );
    reopened.set_caller(Caller::Operator);
    let result = crate::dispatch(
        &mut reopened,
        "resource.recover",
        json!({"key":key,"force":true}),
    )
    .unwrap();
    assert_eq!(result["released"], key);
    assert_eq!(result["payload_absence_proven"], false);
    assert!(holds(&reopened).is_empty());
    let conn = shared_ledger(&reopened).unwrap();
    let audit: String = conn.query_row("SELECT record FROM resource_recovery_audit WHERE json_extract(record,'$.reservation.key')=?1", [&key], |r| r.get(0)).unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["event"], "deployment_reserved_released");
    assert_eq!(audit["by"], "operator");
    assert_eq!(audit["payload_absence_proven"], false);
    assert!(audit["reservation"]["supervisor"].is_object());
    no_deployment_or_job(&reopened);
}

#[test]
fn operator_recovery_invalidates_a_still_held_token_and_repairs_failed_compensation() {
    let clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    drop(clock);
    let _clock = FrozenWallClock::at(100_000);
    recover(&store, &json!({"key":key,"force":true})).unwrap();
    assert!(
        admitted.release_unlaunched().is_err(),
        "a recovered token must be stale"
    );
    assert!(holds(&store).is_empty());

    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    let conn = shared_ledger(&store).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_release BEFORE DELETE ON reservations BEGIN SELECT RAISE(ABORT,'beta'); END").unwrap();
    assert!(admitted.release_unlaunched().is_err());
    conn.execute_batch("DROP TRIGGER fail_release").unwrap();
    // Advance only the injected clock, never wait for wall-clock age.
    drop(_clock);
    let _clock = FrozenWallClock::at(200_000);
    recover(&store, &json!({"key":key,"force":true})).unwrap();
    assert!(holds(&store).is_empty());
}

#[test]
fn recovery_refuses_changed_ambiguous_foreign_and_payload_custody_and_is_audited_atomically() {
    for change in [
        "handoff",
        "payload",
        "escaped",
        "home",
        "job-record",
        "stale-record",
        "audit-failure",
        "delete-failure",
    ] {
        let clock = FrozenWallClock::at(10_000);
        let (_dir, store) = fixture();
        let admitted = admit_fake(&store).unwrap();
        let mut held = holds(&store).remove(0);
        let key = held.key.clone();
        let original = serde_json::to_string(&held).unwrap();
        let conn = shared_ledger(&store).unwrap();
        match change {
            "handoff" => held.state = "deployment-handoff".into(),
            "payload" => held.payload = held.supervisor.clone(),
            "escaped" => held.escaped.push(held.supervisor.clone().unwrap()),
            "home" => held.home = store.home().join("beta"),
            "job-record" => { store.connection().execute("INSERT INTO jobs(id,record) VALUES(?1,'{}')", [&held.job_id]).unwrap(); },
            "stale-record" => held.timeout_ms = Some(1),
            "audit-failure" => conn.execute_batch("CREATE TRIGGER fail_recovery_audit BEFORE INSERT ON resource_recovery_audit BEGIN SELECT RAISE(ABORT,'beta'); END").unwrap(),
            "delete-failure" => conn.execute_batch("CREATE TRIGGER fail_release BEFORE DELETE ON reservations BEGIN SELECT RAISE(ABORT,'beta'); END").unwrap(),
            _ => unreachable!(),
        }
        conn.execute(
            "UPDATE reservations SET record=?2 WHERE key=?1",
            params![key, serde_json::to_string(&held).unwrap()],
        )
        .unwrap();
        drop(clock);
        let _clock = FrozenWallClock::at(100_000);
        let result = if change == "stale-record" {
            let stale = serde_json::from_str(&original).unwrap();
            recover_reserved(&store, &conn, &original, &stale, true)
        } else {
            recover(&store, &json!({"key":key,"force":true}))
        };
        assert!(result.is_err(), "{change}");
        assert_eq!(holds(&store).len(), 1, "{change}");
        assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM resource_recovery_audit WHERE json_extract(record,'$.reservation.key')=?1)", [&key], |r| r.get::<_,bool>(0)).unwrap(), "{change}");
        drop(admitted);
    }
}

#[test]
fn changed_custody_or_failed_delete_never_releases_another_hold() {
    for change in ["state", "payload", "delete-failure"] {
        let (_dir, store) = fixture();
        let admitted = admit_fake(&store).unwrap();
        let conn = shared_ledger(&store).unwrap();
        if change == "delete-failure" {
            conn.execute_batch(
                "CREATE TRIGGER fail_release BEFORE DELETE ON reservations
                BEGIN SELECT RAISE(ABORT,'beta'); END",
            )
            .unwrap();
        } else {
            let mut held = holds(&store).remove(0);
            if change == "state" {
                held.state = "deployment-handoff".into();
            } else {
                held.payload = held.supervisor.clone();
            }
            conn.execute(
                "UPDATE reservations SET record=?2 WHERE key=?1",
                params![admitted.key(), serde_json::to_string(&held).unwrap()],
            )
            .unwrap();
        }
        assert!(admitted.release_unlaunched().is_err(), "{change}");
        assert_eq!(holds(&store).len(), 1);
    }
}

#[test]
fn handoff_commits_exact_custody_and_drop_never_compensates_it() {
    let clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let mut expected = holds(&store).remove(0);
    expected.state = "deployment-handoff".into();
    let handoff = admitted.handoff().unwrap();
    assert_eq!(handoff.record, serde_json::to_string(&expected).unwrap());
    assert_eq!(handoff.reservation.key, expected.key);
    let raw: String = handoff
        .conn
        .query_row(
            "SELECT record FROM reservations WHERE key=?1",
            [&expected.key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, handoff.record);
    drop(handoff);
    drop(clock);
    let _clock = FrozenWallClock::at(100_000);
    let reopened = Store::open(store.home()).unwrap();
    assert_eq!(serde_json::to_string(&holds(&reopened)[0]).unwrap(), raw);
    assert!(recover(&reopened, &json!({"key":expected.key,"force":true})).is_err());
    assert_eq!(holds(&reopened).len(), 1);
    no_deployment_or_job(&reopened);
}

#[test]
fn a_recovered_hold_can_never_yield_a_handoff_token() {
    let clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    drop(clock);
    let _clock = FrozenWallClock::at(100_000);
    recover(&store, &json!({"key":key,"force":true})).unwrap();
    assert!(admitted.handoff().is_err());
    assert!(holds(&store).is_empty());
    no_deployment_or_job(&store);
}

#[test]
fn handoff_compares_the_entire_hold_and_failed_writes_produce_no_token() {
    for change in [
        "state",
        "payload",
        "escaped",
        "supervisor",
        "timeout",
        "task",
        "update-failure",
    ] {
        let (_dir, store) = fixture();
        let admitted = admit_fake(&store).unwrap();
        let mut held = holds(&store).remove(0);
        let conn = shared_ledger(&store).unwrap();
        match change {
            "state" => held.state = "deployment-handoff".into(),
            "payload" => held.payload = held.supervisor.clone(),
            "escaped" => held.escaped.push(held.supervisor.clone().unwrap()),
            "supervisor" => held.supervisor.as_mut().unwrap().started = "gamma".into(),
            "timeout" => held.timeout_ms = Some(1),
            "task" => held.task = Some("beta".into()),
            "update-failure" => conn
                .execute_batch(
                    "CREATE TRIGGER fail_handoff BEFORE UPDATE ON reservations
                BEGIN SELECT RAISE(ABORT,'beta'); END",
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let raw = serde_json::to_string(&held).unwrap();
        if change != "update-failure" {
            conn.execute(
                "UPDATE reservations SET record=?2 WHERE key=?1",
                params![held.key, raw],
            )
            .unwrap();
        }
        assert!(admitted.handoff().is_err(), "{change}");
        let after: String = conn
            .query_row(
                "SELECT record FROM reservations WHERE key=?1",
                [&held.key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, raw, "{change}");
        no_deployment_or_job(&store);
    }
}

#[test]
fn handoff_and_operator_recovery_have_exactly_one_winner() {
    use std::sync::{Arc, Barrier};
    let _clock = FrozenWallClock::at(10_000);
    let (_dir, store) = fixture();
    let admitted = admit_fake(&store).unwrap();
    let key = admitted.key().to_owned();
    let home = store.home().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let ready = Arc::clone(&barrier);
    let handoff = std::thread::spawn(move || {
        ready.wait();
        admitted.handoff().is_ok()
    });
    let recovered = std::thread::spawn(move || {
        let _clock = FrozenWallClock::at(100_000);
        let store = Store::open(&home).unwrap();
        barrier.wait();
        recover(&store, &json!({"key":key,"force":true})).is_ok()
    });
    let handed_off = handoff.join().unwrap();
    assert_ne!(handed_off, recovered.join().unwrap());
    let held = holds(&store);
    if handed_off {
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].state, "deployment-handoff");
    } else {
        assert!(held.is_empty());
    }
    no_deployment_or_job(&store);
}

#[test]
fn deployment_state_with_an_ordinary_id_fails_closed_in_deployment_recovery() {
    let _clock = FrozenWallClock::at(100_000);
    for state in [
        "deployment-reserved",
        "deployment-handoff",
        "deployment-unknown",
    ] {
        let (_dir, mut store) = fixture();
        let admitted = admit_fake(&store).unwrap();
        let mut held = holds(&store).remove(0);
        let old_key = held.key.clone();
        held.job_id = "alpha".into();
        held.key = reservation_key(store.home(), &held.job_id, &held.incarnation);
        held.state = state.into();
        held.created_ms = -100_000;
        // Ordinary recovery could release this absent-owner, payload-less row
        // with force. Deployment-state routing must reject the malformed id.
        held.supervisor.as_mut().unwrap().started = "gamma".into();
        let raw = serde_json::to_string(&held).unwrap();
        let conn = shared_ledger(&store).unwrap();
        conn.execute(
            "UPDATE reservations SET key=?2,record=?3 WHERE key=?1",
            params![old_key, held.key, raw],
        )
        .unwrap();
        let error = crate::dispatch(
            &mut store,
            "resource.recover",
            json!({"key":held.key,"force":true}),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("deployment handoff or payload custody"),
            "{error:#}"
        );
        let after: String = conn
            .query_row(
                "SELECT record FROM reservations WHERE key=?1",
                [&held.key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, raw);
        assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM resource_recovery_audit WHERE json_extract(record,'$.reservation.key')=?1)", [&held.key], |r| r.get::<_,bool>(0)).unwrap());
        drop(admitted);
        no_deployment_or_job(&store);
    }
}

#[test]
fn homes_sharing_a_host_serialize_their_deployment_reservations() {
    use std::sync::{Arc, Barrier};
    let (_dir, store) = fixture();
    let mut second = Store::open(&store.home().parent().unwrap().join("beta")).unwrap();
    fs::copy(
        store.home().join("config.json"),
        second.home().join("config.json"),
    )
    .unwrap();
    crate::dispatch(&mut second, "ship.put", json!({"name":"alpha"})).unwrap();
    let charter: String = store
        .connection()
        .query_row("SELECT data FROM charters WHERE id='alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    second
        .connection()
        .execute(
            "INSERT INTO charters(id,data) VALUES('alpha',?1)",
            [charter],
        )
        .unwrap();
    second
        .connection()
        .execute_batch(
            "INSERT INTO tasks(id,charter_id,owner,title,status,depth)
        VALUES('alpha','alpha','alpha','beta','open',0)",
        )
        .unwrap();
    admission::configure(
        &shared_ledger(&store).unwrap(),
        &json!({"max_heavy_jobs":1}),
    )
    .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [store.home(), second.home()]
        .into_iter()
        .map(|home| {
            let home = home.to_owned();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let _clock = FrozenWallClock::at(10_000);
                let store = Store::open(&home).unwrap();
                barrier.wait();
                // Drop retains the winner's hold, so the loser cannot see freed
                // capacity merely because the winner's thread finished first.
                admit_fake(&store).is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count(),
        1
    );
    assert_eq!(holds(&store).len(), 1);
    no_deployment_or_job(&store);
}
