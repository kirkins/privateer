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
