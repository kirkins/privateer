use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

const SESSION: &str = "0b5c3b1e-7f6a-4d43-9a57-8e0d1b2c3d4e";

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join(".privateer")).unwrap();
        fs::write(store.home().join("config.json"), br#"{"ship":"alpha"}"#).unwrap();
        let record = json!({"location": "here", "first_mate": {"codex_session": SESSION, "duty_task": "duty"}});
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('alpha',?1)",
                [record.to_string()],
            )
            .unwrap();
        crate::presence::attend(&store, "alpha", "host:alpha", 90, None).unwrap();
        let dir = tmp.path().join(".codex/privateer-attended");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.join("launch.json"), b"{}").unwrap();
        fs::set_permissions(dir.join("launch.json"), fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            _tmp: tmp,
            store,
            dir,
        }
    }
    fn run(&mut self, caller: Caller, command: &str, args: Value) -> Result<Value> {
        self.store.set_caller(caller);
        crate::dispatch(&mut self.store, command, args)
    }
    fn operator(&mut self, command: &str, args: Value) -> Result<Value> {
        self.run(Caller::Operator, command, args)
    }
    fn agent(&mut self, command: &str, args: Value) -> Result<Value> {
        let actor = "alpha".to_owned();
        self.run(Caller::Agent { actor }, command, args)
    }
    fn reserve(&mut self, grant: &Value, nonce: &str) -> Result<Value> {
        self.agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION,
                "grant_id": grant["grant"]["grant_id"], "nonce": nonce}),
        )
    }
    fn poison(&self) -> Option<Value> {
        fs::read(self.dir.join("custody-poison.json"))
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }
}

fn nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[test]
fn codex_delivery_grant_and_revoke_are_the_captains_alone() {
    let mut f = Fixture::new();
    let args = json!({"ship": "alpha", "session": SESSION});
    let refused = f.agent("attended.codex.grant", args.clone()).unwrap_err();
    assert!(
        refused.to_string().contains("operator command"),
        "{refused}"
    );
    let refused = f
        .agent("attended.codex.revoke", json!({"ship": "alpha"}))
        .unwrap_err();
    assert!(
        refused.to_string().contains("operator command"),
        "{refused}"
    );
    let granted = f.operator("attended.codex.grant", args).unwrap();
    assert_eq!(granted["grant"]["budget"], DEFAULT_BUDGET);
    assert_eq!(granted["grant"]["remaining"], DEFAULT_BUDGET);
    // The ship's own agent may read its status, never another ship's.
    let own = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(own["grant"]["session"], SESSION);
    assert!(
        f.agent("attended.codex.status", json!({"ship": "beta"}))
            .is_err()
    );
}

// Hosted delivery uses Linux process identity/pidfds; folder() refuses on
// macOS before starting the adapter. Test that explicit status separately.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_status_exposes_durable_pre_spawn_refusal_without_spending_or_resetting() {
    let mut f = Fixture::new();
    let granted = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION}),
        )
        .unwrap();
    let context = json!({"generation": "launch", "callback_generation": "callback",
        "session": SESSION, "holder": "host:alpha"});
    let mut launch = context.clone();
    launch["home"] = json!(f.store.home());
    launch["ship"] = json!("alpha");
    fs::write(f.dir.join("launch.json"), launch.to_string()).unwrap();
    let refusal = json!({
        "diagnostic": "codex-loaded-thread-fence-refused-before-invocation",
        "at_ms": 123, "context": context,
        "facts": {"loaded_count": 2, "threads_in_cwd": 2, "bound_uuid_loaded": true,
            "cursor_state": "null", "cwd_thread_is_bound": false},
        "clears_when": "a later pre-spawn fence passes, or a trusted rebind / hosted relaunch"
    });
    let ledger = json!({"version": 1, "home": f.store.home(), "ship": "alpha", "sessions": {},
        "runner": {"possibly_unattended_spent": 0, "declines": 0, "accepted_through": 0,
            "retry_after_ms": 0, "human_paused": false, "pre_spawn_fence": refusal}});
    let path = f.dir.join("receipts.json");
    fs::write(&path, ledger.to_string()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let original = fs::read(&path).unwrap();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["grant"], granted["grant"]);
    assert_eq!(report["grant"]["spent"], 0);
    assert_eq!(report["runner"]["paused"], true, "{report}");
    assert_eq!(report["runner"]["pre_spawn_fence"], refusal);
    assert_eq!(report["runner"]["diagnostic"], refusal["diagnostic"]);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_status_names_sticky_clear_argv_and_unprovable_cleanup() {
    let mut f = Fixture::new();
    let launch = json!({"home": f.store.home(), "ship": "alpha", "holder": "host:alpha",
        "session": SESSION, "control": "/opt/privateer"});
    fs::write(f.dir.join("launch.json"), launch.to_string()).unwrap();
    let sticky = json!([
        "codex-loaded-thread-fence-lost-after-invocation",
        "codex-custody-lost-after-invocation",
        "codex-queue-cleanup-unproven"
    ]);
    let ledger = json!({"version": 1, "home": f.store.home(), "ship": "alpha", "sessions": {},
        "runner": {"possibly_unattended_spent": 1, "declines": 0, "accepted_through": 0,
            "retry_after_ms": 0, "human_paused": false, "sticky": sticky}});
    let path = f.dir.join("receipts.json");
    fs::write(&path, ledger.to_string()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let original = fs::read(&path).unwrap();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["runner"]["diagnostics"], sticky);
    assert_eq!(report["runner"]["paused"], true);
    let commands = report["runner"]["clear_pause"].as_array().unwrap();
    assert_eq!(commands.len(), 3);
    for (index, command) in commands.iter().enumerate() {
        assert_eq!(command["diagnostic"], sticky[index]);
        assert_eq!(command["argv"][3], "attended.codex.clear_pause");
        let request: Value = serde_json::from_str(command["argv"][5].as_str().unwrap()).unwrap();
        assert_eq!(request["diagnostic"], sticky[index]);
        assert_eq!(request["session"], SESSION);
        assert_eq!(request["holder"], "host:alpha");
    }
    assert_eq!(commands[2]["supported"], false);
    assert!(
        commands[2]["refusal"]
            .as_str()
            .unwrap()
            .contains("no saved process identities")
    );
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn codex_delivery_status_explicitly_reports_unsupported_host() {
    let mut f = Fixture::new();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["runner"]["unavailable"], true, "{report}");
    assert_eq!(
        report["runner"]["reason"],
        "hosted Codex delivery is Linux-only"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_status_reports_missing_launch_and_failed_adapter() {
    let mut f = Fixture::new();
    fs::remove_file(f.dir.join("launch.json")).unwrap();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["runner"]["unavailable"], true, "{report}");
    assert_eq!(report["runner"]["reason"], "no local attended Codex launch");

    // An invalid launch makes the real adapter refuse (or, if Python is
    // absent, its spawn fails). Neither failure may silently drop runner.
    fs::write(f.dir.join("launch.json"), b"not JSON").unwrap();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["runner"]["unavailable"], true, "{report}");
    assert!(!report["runner"]["reason"].as_str().unwrap().is_empty());
}

#[test]
fn codex_delivery_status_reports_non_local_ship() {
    let mut f = Fixture::new();
    fs::write(f.store.home().join("config.json"), br#"{"ship":"beta"}"#).unwrap();
    let report = f
        .agent("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(report["runner"]["unavailable"], true, "{report}");
    let reason = report["runner"]["reason"].as_str().unwrap();
    assert!(
        reason.contains(if cfg!(target_os = "linux") {
            "ship folder"
        } else {
            "Linux-only"
        }),
        "{report}"
    );
}

#[test]
fn codex_delivery_grant_names_the_configured_session_and_a_bounded_budget() {
    let mut f = Fixture::new();
    let other = nonce();
    assert!(
        f.operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": other})
        )
        .is_err()
    );
    for budget in [0, 1001] {
        assert!(
            f.operator(
                "attended.codex.grant",
                json!({"ship": "alpha", "session": SESSION, "budget": budget})
            )
            .is_err()
        );
    }
    let upper = SESSION.to_uppercase();
    assert!(
        f.operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": upper})
        )
        .is_err()
    );
    let granted = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION, "budget": 1000}),
        )
        .unwrap();
    assert_eq!(granted["grant"]["budget"], 1000);
}

#[test]
fn codex_delivery_reserve_spends_once_per_nonce_and_never_refunds() {
    let mut f = Fixture::new();
    let grant = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION, "budget": 2}),
        )
        .unwrap();
    let first = nonce();
    assert_eq!(f.reserve(&grant, &first).unwrap()["spent"], 1);
    // A lost response retried with the same nonce does not spend again.
    assert_eq!(f.reserve(&grant, &first).unwrap()["spent"], 1);
    let second = f.reserve(&grant, &nonce()).unwrap();
    assert_eq!(
        (second["spent"].clone(), second["remaining"].clone()),
        (json!(2), json!(0))
    );
    let exhausted = f.reserve(&grant, &nonce()).unwrap_err();
    assert!(
        exhausted.to_string().contains("budget exhausted"),
        "{exhausted}"
    );
    // Mailbox acks, relaunch and a new lease refund nothing.
    crate::presence::attend(&f.store, "alpha", "host:again", 90, None).unwrap();
    let again = f
        .agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:again", "session": SESSION,
            "grant_id": grant["grant"]["grant_id"], "nonce": nonce()}),
        )
        .unwrap_err();
    assert!(again.to_string().contains("budget exhausted"), "{again}");
    // Only a new captain grant refills; the old grant id stops working.
    let refill = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION, "budget": 3}),
        )
        .unwrap();
    assert!(
        f.agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:again", "session": SESSION,
            "grant_id": grant["grant"]["grant_id"], "nonce": nonce()})
        )
        .is_err()
    );
    let fresh = f
        .agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:again", "session": SESSION,
            "grant_id": refill["grant"]["grant_id"], "nonce": nonce()}),
        )
        .unwrap();
    assert_eq!(fresh["remaining"], 2);
}

#[test]
fn codex_delivery_reserve_requires_the_current_hosted_holder_and_grant() {
    let mut f = Fixture::new();
    let missing = f
        .agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION,
            "grant_id": nonce(), "nonce": nonce()}),
        )
        .unwrap_err();
    assert!(missing.to_string().contains("not granted"), "{missing}");
    let grant = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION}),
        )
        .unwrap();
    let stale = f
        .agent(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:stale", "session": SESSION,
            "grant_id": grant["grant"]["grant_id"], "nonce": nonce()}),
        )
        .unwrap_err();
    assert!(stale.to_string().contains("presence lease"), "{stale}");
    // The operator is not the hosted first mate; reserve is never theirs.
    assert!(
        f.operator(
            "attended.codex.reserve",
            json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION,
            "grant_id": grant["grant"]["grant_id"], "nonce": nonce()})
        )
        .is_err()
    );
    let status = f
        .operator("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(status["grant"]["spent"], 0);
}

#[test]
fn codex_delivery_revoke_poisons_custody_before_commit() {
    let mut f = Fixture::new();
    let grant = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION}),
        )
        .unwrap();
    assert!(f.poison().is_none());
    assert_eq!(
        f.operator("attended.codex.revoke", json!({"ship": "alpha"}))
            .unwrap()["revoked"],
        true
    );
    assert_eq!(f.poison().unwrap()["reason"], "delivery_disabled");
    assert!(f.reserve(&grant, &nonce()).is_err());
    // An unsafe poison path aborts the revoke: the grant survives, unfenced
    // disables are never reported.
    f.operator(
        "attended.codex.grant",
        json!({"ship": "alpha", "session": SESSION}),
    )
    .unwrap();
    fs::remove_file(f.dir.join("custody-poison.json")).unwrap();
    fs::create_dir(f.dir.join("custody-poison.json")).unwrap();
    assert!(
        f.operator("attended.codex.revoke", json!({"ship": "alpha"}))
            .is_err()
    );
    let status = f
        .operator("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert!(status["grant"].is_object());
}

#[test]
fn codex_delivery_low_budget_notice_reaches_the_captain_once() {
    let mut f = Fixture::new();
    let grant = f
        .operator(
            "attended.codex.grant",
            json!({"ship": "alpha", "session": SESSION, "budget": 3}),
        )
        .unwrap();
    for _ in 0..3 {
        f.reserve(&grant, &nonce()).unwrap();
    }
    let notices: i64 = f
        .store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE recipient='captain' AND id LIKE 'codex-turns-low-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1);
}

// Hosted Codex delivery is Linux-only; macOS's python3 shim adds its own
// environment (SDKROOT, CPATH, __CF_USER_TEXT_ENCODING) to every child.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_ticker_runs_one_bounded_pass_at_a_time_with_a_clean_environment() {
    if !Path::new(PYTHON).exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("runner.py");
    fs::write(
        &script,
        "import json, os, sys, time\nfrom pathlib import Path\n\
         out = Path(sys.argv[2]).with_name('passes')\n\
         with out.open('a') as f: f.write(json.dumps({'argv': sys.argv[1:], 'env': sorted(os.environ)}) + '\\n')\n\
         time.sleep(0.3)\n",
    )
    .unwrap();
    let mut ticker = Ticker::new(script);
    ticker.poll(tmp.path());
    ticker.poll(tmp.path()); // In flight: no second pass.
    ticker.finish();
    let passes = fs::read_to_string(tmp.path().join("passes")).unwrap();
    let lines: Vec<Value> = passes
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["argv"][0], "tick");
    assert_eq!(
        lines[0]["argv"][1],
        tmp.path().join("launch.json").to_str().unwrap()
    );
    let env: Vec<&str> = lines[0]["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .filter(|name| !name.starts_with("LC_"))
        .collect();
    assert_eq!(env, ["LANG", "PATH"]);
    // The cadence holds a new pass back until five seconds have passed.
    ticker.poll(tmp.path());
    assert!(ticker.child.is_none());
}

// Hosted Codex delivery is Linux-only; macOS's python3 shim adds its own
// environment (SDKROOT, CPATH, __CF_USER_TEXT_ENCODING) to every child.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_ticker_runs_the_real_runner_under_isolated_python() {
    if !Path::new(PYTHON).exists() {
        return;
    }
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/attended-codex/codex_runner.py");
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("launch.json"),
        json!({"home": tmp.path().join("home"), "ship": "alpha", "state": "unbound"}).to_string(),
    )
    .unwrap();
    let stderr = tmp.path().join("stderr");
    let mut ticker = Ticker::new(script);
    ticker.capture_stderr(stderr.clone());
    ticker.poll(tmp.path());
    let (child, _) = ticker.child.as_mut().expect("runner was not spawned");
    let status = child.wait().unwrap();
    ticker.child = None;
    let output = fs::read_to_string(&stderr).unwrap();
    assert!(status.success(), "runner exited {status}: {output}");
    assert!(!output.contains("ModuleNotFoundError"), "{output}");
    assert!(!output.contains("Traceback"), "{output}");
}

// Hosted Codex delivery is Linux-only: on macOS these controls refuse first.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_operator_controls_refuse_agents_and_bad_resolutions() {
    let mut f = Fixture::new();
    let offer = json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION,
        "notification_id": nonce(), "operation": "abandon", "reason": "alpha"});
    for command in [
        "attended.codex.reconcile",
        "attended.codex.resolve",
        "attended.codex.clear_pause",
        "attended.codex.session",
    ] {
        let error = f.agent(command, json!({"ship": "alpha"})).unwrap_err();
        assert!(
            format!("{error:#}").contains("operator"),
            "{command}: {error:#}"
        );
    }
    for (key, value) in [
        ("operation", json!("ack")),
        ("reason", json!(" ")),
        ("reason", json!("x".repeat(513))),
        ("notification_id", json!("alpha")),
    ] {
        let mut bad = offer.clone();
        bad[key] = value;
        assert!(f.operator("attended.codex.resolve", bad).is_err());
    }
    let beta = json!({"ship": "beta", "holder": "host:alpha", "session": SESSION,
        "notification_id": nonce()});
    let error = f.operator("attended.codex.reconcile", beta).unwrap_err();
    assert!(format!("{error:#}").contains("ship folder"), "{error:#}");
}

// The real adapter child, its stdin/stdout protocol and clean environment.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_reconcile_runs_the_real_runner_and_reports_its_refusal() {
    if !Path::new(PYTHON).exists() {
        return;
    }
    let mut f = Fixture::new();
    let error = f
        .operator(
            "attended.codex.reconcile",
            json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION, "notification_id": nonce()}),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("adapter refused: no current hosted Codex binding"),
        "{error:#}"
    );
}

// The clear command uses the same real, isolated adapter boundary as resolve.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_clear_pause_validates_and_runs_the_real_adapter() {
    let mut f = Fixture::new();
    let request = json!({"ship": "alpha", "holder": "host:alpha", "session": SESSION,
        "diagnostic": "codex-loaded-thread-fence-lost-after-invocation", "reason": "restored watched UUID"});
    for (key, value, expected) in [
        ("diagnostic", json!("unknown"), "sticky pause"),
        ("reason", json!(" "), "1..512 bytes"),
        ("reason", json!("é".repeat(257)), "1..512 bytes"),
        ("session", json!("not-a-uuid"), "canonical lowercase UUID"),
    ] {
        let mut bad = request.clone();
        bad[key] = value;
        let error = f.operator("attended.codex.clear_pause", bad).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    if Path::new(PYTHON).exists() {
        let error = f
            .operator("attended.codex.clear_pause", request)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("adapter refused: no current hosted Codex binding"),
            "{error:#}"
        );
    }
}

// Hosted Codex delivery is Linux-only: on macOS these controls refuse first.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_session_refuses_without_bundled_codex_or_with_invalid_effort() {
    let mut f = Fixture::new();
    let error = f
        .operator("attended.codex.session", json!({"ship": "alpha"}))
        .unwrap_err();
    assert!(format!("{error:#}").contains("bundled:codex"), "{error:#}");
    let record = json!({"location": "here", "first_mate": {"manifest": "bundled:codex",
        "options": {"effort": "unsupported"}, "codex_session": SESSION, "duty_task": "duty"}});
    f.store
        .connection()
        .execute(
            "UPDATE ships SET record=?1 WHERE name='alpha'",
            [record.to_string()],
        )
        .unwrap();
    let error = f
        .operator("attended.codex.session", json!({"ship": "alpha"}))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("must be minimal, low, medium, high, xhigh or none"),
        "{error:#}"
    );
    // Refused before any reservation: no adapter child ran.
    assert!(!f.dir.join("reservation.json").exists());
}

#[test]
fn codex_delivery_effort_is_an_optional_supported_value_not_toml() {
    assert_eq!(effort(&json!({})).unwrap(), None);
    assert_eq!(effort(&json!({"effort": null})).unwrap(), None);
    for value in ["minimal", "low", "medium", "high", "xhigh", "none"] {
        assert_eq!(effort(&json!({"effort": value})).unwrap(), Some(value));
    }
    for value in [
        json!(true),
        json!(3),
        json!(""),
        json!("High"),
        json!("unsupported"),
        json!("high\"\nmodel_provider=\"other"),
        json!("high low"),
        json!("h".repeat(33)),
    ] {
        assert!(effort(&json!({"effort": value})).is_err());
    }
}

#[test]
fn codex_delivery_assign_sets_the_session_and_poisons_a_grant_for_another() {
    let mut f = Fixture::new();
    f.operator(
        "attended.codex.grant",
        json!({"ship": "alpha", "session": SESSION}),
    )
    .unwrap();
    let fresh = nonce();
    let folder = f
        .dir
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .canonicalize()
        .unwrap();
    let target = f.dir.join("reservation.json");
    let mut reserved = json!({"state": "reserved", "session": fresh, "generation": nonce()});
    assert!(assign(&f.store, "alpha", &folder, &Value::Null, &reserved).is_err());
    reserved["state"] = json!("verified");
    fs::write(
        &target,
        json!({"version": 1, "state": "verified", "session": fresh,
            "generation": reserved["generation"]})
        .to_string(),
    )
    .unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let (session, record) = assign(&f.store, "alpha", &folder, &Value::Null, &reserved).unwrap();
    assert_eq!(session, fresh);
    let stored_reservation: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert_eq!(stored_reservation["state"], "assigned");
    assert_eq!(record["first_mate"]["codex_session"], fresh.as_str());
    let stored: String = f
        .store
        .connection()
        .query_row("SELECT record FROM ships WHERE name='alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["first_mate"]["codex_session"], fresh.as_str());
    let status = f
        .operator("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert!(status["grant"].is_null());
    assert_eq!(f.poison().unwrap()["reason"], "delivery_disabled");
    let audit: String = f
        .store
        .connection()
        .query_row(
            "SELECT payload FROM audit WHERE kind='attended.codex.session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let audit: Value = serde_json::from_str(&audit).unwrap();
    assert_eq!(audit["previous"], SESSION);
    assert_eq!(audit["grant_revoked"], true);
    // Re-assigning the same session leaves a grant for it alone.
    f.operator(
        "attended.codex.grant",
        json!({"ship": "alpha", "session": fresh}),
    )
    .unwrap();
    assign(&f.store, "alpha", &folder, &Value::Null, &reserved).unwrap();
    let status = f
        .operator("attended.codex.status", json!({"ship": "alpha"}))
        .unwrap();
    assert_eq!(status["grant"]["session"], fresh.as_str());
}

// Failure after the writer commit must name the committed session, because the
// exact-UUID retry is what recovers it.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_assign_says_when_the_session_was_already_committed() {
    let f = Fixture::new();
    init(f.store.connection()).unwrap();
    let fresh = nonce();
    let generation = nonce();
    let folder = f
        .dir
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .canonicalize()
        .unwrap();
    let target = f.dir.join("reservation.json");
    fs::write(
        &target,
        json!({"version": 1, "state": "verified", "session": fresh,
            "generation": generation})
        .to_string(),
    )
    .unwrap();
    // Not private: the writer transaction commits first, then the assign
    // adapter refuses to read the record.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let reserved = json!({"state": "verified", "session": fresh, "generation": generation});
    let error = assign(&f.store, "alpha", &folder, &Value::Null, &reserved).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains(&format!("committed as {fresh}")), "{error}");
    assert!(error.contains("continues that exact session"), "{error}");
    let stored: String = f
        .store
        .connection()
        .query_row("SELECT record FROM ships WHERE name='alpha'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["first_mate"]["codex_session"], fresh.as_str());
}

// A pass that exits normally still loses whatever it left in its group.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_ticker_kills_a_finished_passs_whole_process_group() {
    if !Path::new(PYTHON).exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("runner.py");
    fs::write(
        &script,
        "import os, sys, time\nfrom pathlib import Path\n\
         pid = os.fork()\n\
         if pid == 0:\n    time.sleep(90)\n    os._exit(0)\n\
         Path(sys.argv[2]).with_name('member').write_text(str(pid))\n",
    )
    .unwrap();
    let mut ticker = Ticker::new(script);
    ticker.poll(tmp.path());
    let started = Instant::now();
    while ticker.child.is_some() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
        ticker.reap();
    }
    assert!(ticker.child.is_none());
    let member: libc::pid_t = fs::read_to_string(tmp.path().join("member"))
        .unwrap()
        .parse()
        .unwrap();
    let started = Instant::now();
    // Reparented and reaped by init once killed; signal 0 then fails.
    while unsafe { libc::kill(member, 0) } == 0 && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_ne!(unsafe { libc::kill(member, 0) }, 0, "group member survived");
}

// The bound path end to end under isolated Python: presence and mail through
// a stand-in control CLI, no daemon, so the pass records unavailable evidence.
#[cfg(target_os = "linux")]
#[test]
fn codex_delivery_ticker_runs_the_real_runner_on_a_bound_launch() {
    if !Path::new(PYTHON).exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("project");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let control = tmp.path().join("control");
    crate::service::tests::script(
        &control,
        "#!/usr/bin/python3\nimport json, sys\n\
         answers = {'ship.presence': {'attended': True, 'lease': {'holder': 'host:alpha', 'expires_ms': 10**18}},\n\
         'mail.pending': {'address': 'alpha', 'after_seq': 0, 'acknowledged_through': 0, 'count': 0,\n\
         'through_seq': 0, 'actionable': 0, 'headers': [], 'attention': None}}\n\
         if len(sys.argv) > 3: print(json.dumps(answers[sys.argv[3]]))\n",
    );
    let codex_home = tmp.path().join("codex");
    fs::create_dir(&codex_home).unwrap();
    let launch = json!({"home": tmp.path().join("home"), "ship": "alpha", "state": "bound",
        "hosted": true, "session": SESSION, "holder": "host:alpha", "cwd": tmp.path(),
        "codex_home": codex_home, "control": control, "path_env": {"HOME": tmp.path()}});
    fs::write(dir.join("launch.json"), launch.to_string()).unwrap();
    fs::set_permissions(dir.join("launch.json"), fs::Permissions::from_mode(0o600)).unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/attended-codex/codex_runner.py");
    let stderr = tmp.path().join("stderr");
    let mut ticker = Ticker::new(script);
    ticker.capture_stderr(stderr.clone());
    ticker.poll(&dir);
    let (child, _) = ticker.child.as_mut().expect("runner was not spawned");
    let status = child.wait().unwrap();
    ticker.child = None;
    let output = fs::read_to_string(&stderr).unwrap();
    assert!(status.success(), "runner exited {status}: {output}");
    let ledger: Value =
        serde_json::from_slice(&fs::read(dir.join("receipts.json")).unwrap()).unwrap();
    assert!(ledger["sessions"][SESSION].is_object(), "{ledger}");
    assert_eq!(
        ledger["runner"]["diagnostic"],
        "codex-native-evidence-unavailable"
    );
}
