// Real supervised door and operator-authenticated gateways. Permits are issued
// through a real controlling terminal, never inserted as a test bypass.
struct RecoveryHold {
    home: PathBuf,
    ledger: PathBuf,
    reference: Value,
    key: String,
}
fn recovery_hold(fleet: &Fleet, home: &Path, ship: &str, id: &str, dir: &Path) -> RecoveryHold {
    use std::os::unix::process::CommandExt;
    fs::create_dir_all(dir).unwrap();
    fs::write(
        home.join("config.json"),
        json!({"resource_home":dir}).to_string(),
    )
    .unwrap();
    let out = fleet
        .command(&fleet.root)
        .env("PRIVATEER_RESOURCE_HOME", dir)
        .args([
            "--home",
            home.to_str().unwrap(),
            "resource.status",
            "--json",
            "{}",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let home = fs::canonicalize(home).unwrap();
    let ledger = fs::canonicalize(dir).unwrap().join("resources.sqlite");
    let mut child = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    #[cfg(target_os = "linux")]
    let process = {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let ticks = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        json!({"pid":pid,"started":format!("linux:{}:{ticks}",boot.trim()),"pid_namespace":fs::read_link("/proc/self/ns/pid").unwrap(),"zombie":false})
    };
    #[cfg(not(target_os = "linux"))]
    let process = {
        let out = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .env("TZ", "UTC0")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(out.status.success());
        json!({"pid":pid,"started":String::from_utf8(out.stdout).unwrap().split_whitespace().collect::<Vec<_>>().join(" "),"pid_namespace":null,"zombie":false})
    };
    child.kill().unwrap();
    child.wait().unwrap();
    let incarnation = format!("incarnation-{id}");
    let key = format!("{}:{id}:{incarnation}", home.display());
    // Explicit epoch-old custody, not an assumption about host boot uptime.
    let job = json!({"id":id,"task_id":format!("task-{id}"),"incarnation":incarnation,"command":["secret-canary"],"cwd":home,"action":"test","heavy":true,"timeout_secs":60,"ledger":ledger,"status":"failed","created_ms":0,"finished_ms":10,"supervisor":process,"payload":process,"cancel_requested":false,"exit_code":1,"reason":null,"retry_of":null,"launch_ms":1,"spawned_ms":2});
    let db = rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
    db.execute(
        "INSERT INTO jobs(id,record) VALUES(?1,?2)",
        rusqlite::params![id, job.to_string()],
    )
    .unwrap();
    let hold = json!({"key":key,"home":home,"job_id":id,"incarnation":incarnation,"heavy":true,"state":"failed","created_ms":0,"supervisor":process,"payload":process,"ship":ship,"task":format!("task-{id}")});
    rusqlite::Connection::open(&ledger)
        .unwrap()
        .execute(
            "INSERT INTO reservations VALUES(?1,?2)",
            rusqlite::params![key, hold.to_string()],
        )
        .unwrap();
    let out = fleet
        .command(&fleet.root)
        .args([
            "--home",
            home.to_str().unwrap(),
            "resource.recovery.reference",
            "--json",
            &json!({"job_id":id}).to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    RecoveryHold {
        home,
        ledger,
        reference: serde_json::from_slice::<Value>(&out.stdout).unwrap()["reference"].clone(),
        key,
    }
}
fn recovery_permit(fleet: &Fleet, hold: &RecoveryHold, metadata: &Value, key: &str) -> String {
    let request = json!({"request":{"reference":metadata["reference"],"authority":metadata["authority"],"idempotency_key":key}});
    let path = fleet
        .root
        .join(format!("request-{}.json", uuid::Uuid::new_v4()));
    fs::write(&path, request.to_string()).unwrap();
    // Adapt the existing portable three-channel terminal fixture to the fresh
    // random challenge, without echoing it through any client/core interface.
    let helper = CLIENT_TERMINAL_HELPER.replace("import errno, os,", "import re, errno, os,")
        .replace("if fd == master and not confirmed and prompt in text:","match = re.search(rb'Type ([0-9a-f]{8}) to confirm:', text)\n            if fd == master and not confirmed and match:")
        .replace("(sys.argv[1] + '\\n').encode()", "match[1] + b'\\n'");
    let env = fleet
        .command(&fleet.root)
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect::<Vec<_>>();
    let out = support::command(support::python())
        .envs(env)
        .args([
            "-c",
            &helper,
            "unused",
            BIN,
            "--home",
            hold.home.to_str().unwrap(),
            "resource.recovery_permit.issue",
            "--file",
            path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let result = terminal_helper_result(out);
    assert_eq!(result["status"], 0, "{result}");
    serde_json::from_str::<Value>(result["out"].as_str().unwrap()).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .into()
}
fn recovery_args(metadata: &Value, approval: &str, key: &str) -> Value {
    json!({"machine":metadata["machine"],"ship":metadata["ship"],"reference":metadata["reference"],"approval_id":approval,"idempotency_key":key})
}
fn recovery_is_held(hold: &RecoveryHold) -> bool {
    rusqlite::Connection::open(&hold.ledger)
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE key=?1)",
            [&hold.key],
            |r| r.get(0),
        )
        .unwrap()
}
#[test]
fn resource_recovery_supervised_authority_permits_replay_and_closed_refusals() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let hold = recovery_hold(
        &fleet,
        &fleet.home("survey"),
        "survey",
        "alpha",
        &fleet.root.join("ledger"),
    );
    delegated_adapter(&fleet);
    let (_, admin) = delegated_issue(&fleet, "alpha", "fleet:admin,fleet:read", None);
    let (_, other) = delegated_issue(&fleet, "beta", "fleet:admin,fleet:read", None);
    let (_, named) = delegated_issue(&fleet, "gamma", "fleet:admin,fleet:read", Some("survey"));
    let (_, read) = delegated_issue(&fleet, "delta", "fleet:read", None);
    let (_, admin_only) = delegated_issue(&fleet, "epsilon", "fleet:admin", None);
    let supervisor = fleet.supervise();
    fleet.wait("recovery hello", |e| e.first().cloned());
    let selector = json!({"machine":"local","ship":"survey","job_id":"alpha"});
    credential_request(
        &fleet,
        &admin_only,
        "resource_recovery_reference",
        selector.clone(),
        "insufficient_scope",
    );
    let metadata = delegated_ok(
        &fleet,
        &admin,
        "resource_recovery_reference",
        selector.clone(),
    );
    for (token, ship) in [(&named, "hidden"), (&read, "missing")] {
        credential_request(
            &fleet,
            token,
            "resource_recovery_reference",
            json!({"machine":"local","ship":ship,"job_id":"alpha"}),
            "recovery_target_stale",
        );
    }
    assert_eq!(metadata["reference"], hold.reference);
    assert_eq!(
        delegated_ok(&fleet, &read, "resource_recovery_reference", selector)["reference"],
        hold.reference
    );
    let missing = uuid::Uuid::new_v4().to_string();
    let a = recovery_args(&metadata, &missing, "alpha");
    credential_request(
        &fleet,
        &read,
        "resource_recover",
        a.clone(),
        "insufficient_scope",
    );
    credential_request(
        &fleet,
        &named,
        "resource_recover",
        a.clone(),
        "ship_not_allowed",
    );
    credential_request(
        &fleet,
        &admin,
        "resource_recover",
        a.clone(),
        "recovery_approval_required",
    );
    for command in [
        "resource.recover",
        "resource.recovery.apply",
        "resource.recovery.reference",
        "resource.recovery_permit.issue",
        "resource.recovery_permit.revoke",
        "fleet.resource-action.v1",
        "dispatch",
        "rpc",
    ] {
        credential_request(&fleet, &admin, command, a.clone(), "unknown_command");
    }
    for field in [
        "key",
        "pid",
        "path",
        "force",
        "payload",
        "command",
        "_binding",
        "incarnation",
        "target",
        "approval",
        "yes",
    ] {
        let mut bad = a.clone();
        bad[field] = json!("secret-canary");
        credential_request(
            &fleet,
            &admin,
            "resource_recover",
            bad,
            "unsupported_arguments",
        );
    }
    let approval = recovery_permit(&fleet, &hold, &metadata, "alpha");
    let a = recovery_args(&metadata, &approval, "alpha");
    credential_request(
        &fleet,
        &other,
        "resource_recover",
        a.clone(),
        "recovery_approval_stale",
    );
    let first = delegated_ok(&fleet, &admin, "resource_recover", a.clone());
    assert_eq!(first["outcome"], "recovered");
    assert!(!recovery_is_held(&hold));
    // Replay precedes hold/job lookup and permit expiry/revocation checks.
    let ledger = rusqlite::Connection::open(&hold.ledger).unwrap();
    ledger
        .execute(
            "UPDATE resource_recovery_permits SET expires_ms=?1,revoked_ms=0",
            [privateer::store::Store::now_ms() - 1],
        )
        .unwrap();
    rusqlite::Connection::open(hold.home.join("privateer.sqlite"))
        .unwrap()
        .execute("DELETE FROM jobs WHERE id='alpha'", [])
        .unwrap();
    assert_eq!(
        delegated_ok(&fleet, &admin, "resource_recover", a.clone()),
        first
    );
    stop(supervisor);
    let supervisor = fleet.supervise();
    assert_eq!(
        delegated_ok(&fleet, &admin, "resource_recover", a.clone()),
        first
    );
    let mut conflict = a;
    conflict["idempotency_key"] = json!("beta");
    credential_request(
        &fleet,
        &admin,
        "resource_recover",
        conflict,
        "recovery_approval_stale",
    );
    let audit: String = ledger.query_row("SELECT record FROM resource_recovery_audit WHERE json_extract(record,'$.event')='permit_consumed'",[],|r|r.get(0)).unwrap();
    assert!(audit.contains("delete_exact_terminal_reservation"));
    assert!(!audit.contains("secret-canary"));
    assert!(!first.to_string().contains("secret-canary"));
    stop(supervisor);
}

#[test]
fn resource_recovery_door_expired_revoked_changed_hold_job_home_and_ledger() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    delegated_adapter(&fleet);
    let (_, admin) = delegated_issue(&fleet, "alpha", "fleet:admin,fleet:read", None);
    let supervisor = fleet.supervise();
    fleet.wait("recovery refusals hello", |e| e.first().cloned());
    for mode in [
        "expired",
        "revoked",
        "hold",
        "job",
        "home",
        "ledger",
        "alive",
        "unknown",
        "nonterminal",
        "capacity",
    ] {
        let hold = recovery_hold(
            &fleet,
            &fleet.home("survey"),
            "survey",
            mode,
            &fleet.root.join("ledger"),
        );
        let metadata = delegated_ok(
            &fleet,
            &admin,
            "resource_recovery_reference",
            json!({"machine":"local","ship":"survey","job_id":mode}),
        );
        let approval = recovery_permit(&fleet, &hold, &metadata, mode);
        let db = rusqlite::Connection::open(&hold.ledger).unwrap();
        let home = rusqlite::Connection::open(hold.home.join("privateer.sqlite")).unwrap();
        let code = match mode {
            "expired" => {
                db.execute(
                    "UPDATE resource_recovery_permits SET expires_ms=?2 WHERE id=?1",
                    rusqlite::params![approval, privateer::store::Store::now_ms() - 1],
                )
                .unwrap();
                "recovery_approval_stale"
            }
            "revoked" => {
                fleet.call(
                    "survey",
                    "resource.recovery_permit.revoke",
                    json!({"approval_id":approval}),
                );
                "recovery_approval_stale"
            }
            "hold" => {
                db.execute("UPDATE reservations SET record=json_set(record,'$.state','changed') WHERE key=?1",[&hold.key]).unwrap();
                "recovery_target_stale"
            }
            "job" => {
                home.execute(
                    "UPDATE jobs SET record=json_set(record,'$.incarnation','changed') WHERE id=?1",
                    [mode],
                )
                .unwrap();
                "recovery_target_stale"
            }
            "home" => {
                home.execute(
                    "UPDATE metadata SET value='changed' WHERE key='runtime_origin'",
                    [],
                )
                .unwrap();
                "recovery_target_stale"
            }
            "ledger" => {
                db.execute(
                    "UPDATE resource_recovery_identity SET incarnation=?1",
                    [uuid::Uuid::new_v4().to_string()],
                )
                .unwrap();
                "recovery_target_stale"
            }
            "nonterminal" => {
                home.execute(
                    "UPDATE jobs SET record=json_set(record,'$.status','running') WHERE id=?1",
                    [mode],
                )
                .unwrap();
                "recovery_not_eligible"
            }
            "capacity" => {
                for n in 0..256 {
                    db.execute(
                        "INSERT INTO resource_recovery_results VALUES(?1,?2,'synthetic',?3,'{}')",
                        rusqlite::params![
                            format!("capacity-{n}"),
                            metadata["authority"]["grant"].as_str().unwrap(),
                            privateer::store::Store::now_ms()
                        ],
                    )
                    .unwrap();
                }
                "retry_capacity"
            }
            "alive" | "unknown" => {
                // Change evidence in the job, without changing the exact hold:
                // the native seam must inspect their union after approval.
                let process = if mode == "unknown" {
                    Value::Null
                } else {
                    #[cfg(target_os = "linux")]
                    {
                        let pid = std::process::id();
                        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
                        json!({"pid":pid,"started":format!("linux:{}:{}",fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap()),"pid_namespace":fs::read_link("/proc/self/ns/pid").unwrap(),"zombie":false})
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let out = Command::new("/bin/ps")
                            .args(["-p", &std::process::id().to_string(), "-o", "lstart="])
                            .env("TZ", "UTC0")
                            .env("LC_ALL", "C")
                            .output()
                            .unwrap();
                        json!({"pid":std::process::id(),"started":String::from_utf8(out.stdout).unwrap().split_whitespace().collect::<Vec<_>>().join(" "),"pid_namespace":null,"zombie":false})
                    }
                };
                home.execute(
                    "UPDATE jobs SET record=json_set(record,'$.supervisor',json(?2)) WHERE id=?1",
                    rusqlite::params![mode, process.to_string()],
                )
                .unwrap();
                if mode == "alive" {
                    "recovery_process_alive"
                } else {
                    "recovery_identity_unknown"
                }
            }
            _ => unreachable!(),
        };
        // Flush fixture edits before comparing database bytes. WAL checkpoint
        // bookkeeping is not a recovery write and must not race this baseline.
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let before = fs::read(&hold.ledger).unwrap();
        let reply = delegated_exchange(
            &fleet,
            &admin,
            "resource_recover",
            recovery_args(&metadata, &approval, mode),
        );
        assert_eq!(reply["ok"], false, "{mode}: {reply}");
        assert_eq!(reply["error"]["code"], code, "{mode}: {reply}");
        assert_eq!(
            fs::read(&hold.ledger).unwrap(),
            before,
            "{mode}: denial changed ledger bytes"
        );
        assert!(recovery_is_held(&hold));
        assert!(!reply.to_string().contains("secret-canary"));
    }
    stop(supervisor);
}
