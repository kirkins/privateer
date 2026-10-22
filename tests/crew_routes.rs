mod support;

use serde_json::{Value, json};
use sha2::Digest;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, sync::Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

static IN_PROCESS_LOCK: Mutex<()> = Mutex::new(());

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        for name in ["codex", "claude"] {
            let path = root.join("bin").join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Not a Git checkout, so crew run in place without a Treehouse lease.
        fs::create_dir(root.join("engineering")).unwrap();
        let fleet = Self { _tmp: tmp, root };
        fleet.ok(
            None,
            &["init", "--ship", "engineering", "--harness", "codex"],
        );
        fleet.json(
            None,
            "task.create",
            json!({"id":"fix","charter_id":"engineering","owner":"engineering","title":"Fix it"}),
        );
        // These tests are about routing. Admission has its own tests, so the
        // host's live memory and disk, and the crew memory the bundled
        // manifests declare, never decide a launch here.
        fleet.json(
            None,
            "resource.configure",
            json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,
                "max_load_per_cpu":1024,"ramp_secs":0,
                "crew_memory_bytes":{"codex":1u64 << 20,"claude":1u64 << 20}}),
        );
        fleet
    }
    /// Wait until no launch is queued for capacity, so the next one is judged
    /// against the ledger as it now is.
    fn queue_drained(&self) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(12);
        loop {
            let status = self.json(None, "resource.status", json!({}));
            if status["queue"].as_array().unwrap().is_empty() {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "queue never drained: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    /// Wait until a job has settled either way.
    fn settled(&self, job: &str) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = self.json(None, "job.status", json!({"id":job}));
            if matches!(
                status["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled" | "timed_out")
            ) {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "{job} never settled: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    /// Wait until a job has stopped unsuccessfully, the only kind a retry takes.
    fn stopped(&self, job: &str) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = self.json(None, "job.status", json!({"id":job}));
            if matches!(
                status["status"].as_str(),
                Some("failed" | "cancelled" | "timed_out")
            ) {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "{job} never stopped: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    /// One dispatcher pass: the route scan runs in it, so what a run reported
    /// reaches the ledger before the next launch is judged.
    fn tick(&self) {
        let output = self
            .command(None, &["serve", "--json", "{\"once\":true}"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "serve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    /// Launch a crew on a route, wait for it and scan what it reported.
    fn run_and_scan(&self, id: &str, route: Option<&str>) -> Value {
        self.run_task_and_scan("fix", id, route)
    }
    /// The same, on another task than the fixture's root one.
    fn run_task_and_scan(&self, task: &str, id: &str, route: Option<&str>) -> Value {
        let mut start = json!({"task_id":task,"id":id});
        if let Some(route) = route {
            start["route"] = json!(route);
        }
        let job = self.json(Some("engineering"), "agent.start", start);
        self.settled(id);
        self.tick();
        job
    }
    fn command(&self, agent: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(self.root.join("engineering"))
            .args(args)
            .env("HOME", &self.root)
            .env("PATH", support::path_with(&self.root.join("bin")))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env(
                "PRIVATEER_PLUGINS",
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        if let Some(agent) = agent {
            c.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", agent);
        }
        c
    }
    fn run(&self, agent: Option<&str>, args: &[&str]) -> std::process::Output {
        self.command(agent, args).output().unwrap()
    }
    fn ok(&self, agent: Option<&str>, args: &[&str]) -> String {
        let o = self.run(agent, args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn json(&self, agent: Option<&str>, verb: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(agent, &[verb, "--json", &args.to_string()])).unwrap()
    }
    fn refused(&self, agent: Option<&str>, verb: &str, args: Value) -> String {
        let o = self.run(agent, &[verb, "--json", &args.to_string()]);
        assert!(!o.status.success(), "{verb} {args} was not refused");
        String::from_utf8_lossy(&o.stderr).into_owned()
    }
    fn routes(&self, routes: Value) {
        let file = self.root.join("routes.json");
        let temp = self
            .root
            .join(format!("routes-{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temp, routes.to_string()).unwrap();
        fs::rename(&temp, &file).unwrap();
        self.ok(None, &["routes", "set", file.to_str().unwrap()]);
    }
    /// The model a launched job's harness was given.
    fn model(&self, job: &Value) -> String {
        let job = self.json(None, "job.status", json!({"id":job["id"]}));
        let argv = job["command"].as_array().unwrap();
        let at = argv
            .iter()
            .position(|a| a == "--model")
            .expect("no --model");
        argv[at + 1].as_str().unwrap().to_owned()
    }
    fn back_off(&self, route: &str) {
        let ledger = rusqlite::Connection::open(self.root.join("ledger/resources.sqlite")).unwrap();
        ledger
            .execute(
                "INSERT INTO model_routes(route,failures,until_ms,last_error,updated_ms) VALUES(?1,1,?2,'429',0)",
                rusqlite::params![route, i64::MAX / 2],
            )
            .unwrap();
    }
    /// Record what the last readiness check found for a harness tool, as the
    /// dispatcher's check stores it, just now.
    fn harness(&self, tool: &str, ok: bool) {
        self.harness_checked(tool, ok, 0);
    }
    /// `harness`, as a check `age_ms` ago left it.
    fn harness_checked(&self, tool: &str, ok: bool, age_ms: i64) {
        let store =
            rusqlite::Connection::open(self.root.join("engineering/.privateer/privateer.sqlite"))
                .unwrap();
        store
            .execute(
                "INSERT OR REPLACE INTO machine_readiness(tool, level, ok, version, path, why,
                   install, needed_by, fingerprint, checked_ms)
                 VALUES(?1, 'warn', ?2, NULL, NULL, ?3, ?4, '[]', 'test', ?5)",
                rusqlite::params![
                    tool,
                    ok,
                    format!("{tool} is not on the dispatcher's PATH"),
                    format!("install {tool} so it is on the dispatcher's PATH"),
                    privateer::store::Store::now_ms() - age_ms
                ],
            )
            .unwrap();
    }
    /// Record a harness tool the last check found at a path but failing,
    /// with the check's own why.
    fn harness_failing(&self, tool: &str, why: &str) {
        let store =
            rusqlite::Connection::open(self.root.join("engineering/.privateer/privateer.sqlite"))
                .unwrap();
        store
            .execute(
                "INSERT OR REPLACE INTO machine_readiness(tool, level, ok, version, path, why,
                   install, needed_by, fingerprint, checked_ms)
                 VALUES(?1, 'warn', 0, '0.1.0', ?2, ?3, ?4, '[]', 'test', ?5)",
                rusqlite::params![
                    tool,
                    format!("/opt/bin/{tool}"),
                    why,
                    format!("install {tool} 2.0 or later"),
                    privateer::store::Store::now_ms()
                ],
            )
            .unwrap();
    }
    /// A custom harness manifest requiring `tool`, which no PATH has.
    fn absent_manifest(&self, tool: &str) -> String {
        let path = self.root.join("absent.json");
        fs::write(
            &path,
            json!({"schema_version": 1, "id": "absent", "executable": ["python3", "x.py"],
                "requires": ["python3", tool]})
            .to_string(),
        )
        .unwrap();
        path.to_string_lossy().into_owned()
    }
    /// Run against this fixture's store, ledger and routes file in-process.
    /// A process-wide lock serializes in-process readers so that environment
    /// variables pointing at the fleet registry and ledger do not race across
    /// test threads when the suite runs concurrently.
    fn in_process<T>(&self, run: impl FnOnce(&mut privateer::store::Store) -> T) -> T {
        // A test that panics inside the lock poisons it; recover rather than
        // fail every later test, since each caller sets both variables again.
        let _guard = IN_PROCESS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: the lock serializes every writer and reader of these
        // variables in this test binary, and other threads read the
        // environment only through std, which takes its own env lock.
        unsafe {
            std::env::set_var("PRIVATEER_FLEET", self.root.join("fleet.sqlite"));
            std::env::set_var("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"));
        }
        let mut store =
            privateer::store::Store::open(&self.root.join("engineering/.privateer")).unwrap();
        let result = run(&mut store);
        // SAFETY: as above, under the same lock.
        unsafe {
            std::env::remove_var("PRIVATEER_FLEET");
            std::env::remove_var("PRIVATEER_RESOURCE_HOME");
        }
        result
    }
}

fn codex(model: &str, cost: &str) -> Value {
    json!({"manifest":"bundled:codex","options":{"model":model,"sandbox":"danger-full-access"},"cost":cost})
}

/// A ranked route: a manifest with options, a cost, a tier and its uses.
fn tiered(manifest: &str, options: Value, cost: &str, tier: u64, uses: &[&str]) -> Value {
    json!({"manifest": manifest, "options": options, "cost": cost, "tier": tier, "uses": uses})
}

#[test]
fn launches_take_the_first_available_listed_route_and_errands_their_own_list() {
    let f = Fleet::new();
    let mut cheap = codex("model-cheap", "free");
    cheap["note"] = json!("trial runs only");
    f.routes(json!({
        "routes": {"cheap": cheap, "steady": codex("model-steady", "credits"),
                   "review": codex("model-review", "plan")},
        "crew": ["steady"], "errand": ["review"], "first_mate": ["review"],
        "ships": {"engineering": {"crew": ["cheap", "steady"], "first_mate": ["steady"]}}
    }));
    let start = || f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    // The ship's own order wins over the fleet's.
    assert_eq!(f.model(&start()), "model-cheap");
    f.back_off("codex/-/model-cheap");
    assert_eq!(f.model(&start()), "model-steady");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    assert!(
        bearings["text"].as_str().unwrap().contains(
            "cheap (free; trial runs only; unranked) backing off, steady (credits; unranked)"
        ),
        "{bearings}"
    );
    f.back_off("codex/-/model-steady");
    let refusal = f.refused(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert!(
        refusal.contains("every crew route for engineering is unavailable"),
        "{refusal}"
    );
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Review the fix."}),
    );
    let errand = f.json(None, "job.status", json!({"id":errand["errand"]}));
    assert!(
        errand["command"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "model-review"),
        "{errand}"
    );
}

/// A ship whose routes file lists first_mate routes but no crew or errand
/// list: its crew and errands run on the first mate's listed route, as its
/// first mate does, never on the harness's own default, since `init` leaves
/// the profile no model. Options naming their own model stay explicit, and
/// with every listed route unavailable they wait as the first mate does.
#[test]
fn crew_and_errands_with_no_list_of_their_own_take_the_first_mate_route() {
    let f = Fleet::new();
    let profile = f.json(None, "ship.show", json!({"name":"engineering"}))["first_mate"].clone();
    assert!(profile["options"].get("model").is_none(), "{profile}");
    f.routes(json!({
        "routes": {"mate": codex("model-mate", "plan"), "spare": codex("model-spare", "plan")},
        "first_mate": ["mate", "spare"]
    }));
    let crew = f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(f.model(&crew), "model-mate");
    assert_eq!(crew["route"]["name"], "mate", "{crew}");
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Review the fix."}),
    );
    assert_eq!(errand["route"]["name"], "mate", "{errand}");
    assert_eq!(f.model(&json!({"id":errand["errand"]})), "model-mate");

    // A crew whose options name a model keeps it.
    let own = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","options":{"model":"model-own"}}),
    );
    assert_eq!(f.model(&own), "model-own");

    // The first route backs off: both move to the next listed one.
    f.back_off("codex/-/model-mate");
    let crew = f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(f.model(&crew), "model-spare");
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Review it again."}),
    );
    assert_eq!(f.model(&json!({"id":errand["errand"]})), "model-spare");

    // None clear: they wait, with each route's reason.
    f.back_off("codex/-/model-spare");
    for refusal in [
        f.refused(Some("engineering"), "agent.start", json!({"task_id":"fix"})),
        f.refused(
            Some("engineering"),
            "errand.start",
            json!({"focus":"task:fix","brief":"And again."}),
        ),
    ] {
        assert!(
            refusal.contains("every first_mate route for engineering is unavailable"),
            "{refusal}"
        );
        assert!(
            refusal.contains("mate: model route codex/-/model-mate is backing off"),
            "{refusal}"
        );
        // Nothing queues the launch: it is the caller's to retry.
        assert!(refusal.contains("retry once one clears"), "{refusal}");
        assert!(!refusal.contains("waits for one to clear"), "{refusal}");
    }
}

/// Crew that take the first mate's listed route are judged against each
/// route's crew share through the list order: a first route past the crew
/// share, though inside the first mate's, passes them to the next listed one
/// instead of refusing them. An errand, judged as the first mate, keeps it.
#[test]
fn crew_on_the_first_mate_list_pass_a_route_past_the_crew_share() {
    let f = Fleet::new();
    let mut mate = codex("model-mate", "plan");
    mate["allowance"] = json!({"window_hours": 5, "weekly_utc": "Mon 00:00"});
    mate["tier"] = json!(1);
    f.routes(json!({
        "routes": {"mate": mate, "spare": codex("model-spare", "plan")},
        "first_mate": ["mate", "spare"]
    }));
    f.tick();
    let now = privateer::store::Store::now_ms();
    rusqlite::Connection::open(f.root.join("ledger/resources.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO route_usage(route, weekly_utilization, weekly_ends_ms, updated_ms)
             VALUES('codex/-/model-mate', 0.85, ?1, ?2)",
            rusqlite::params![now + 3_600_000, now],
        )
        .unwrap();
    let crew = f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(crew["route"]["name"], "spare", "{crew}");
    assert_eq!(f.model(&crew), "model-spare");
    let errand = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"Review the fix."}),
    );
    assert_eq!(errand["route"]["name"], "mate", "{errand}");
    assert_eq!(f.model(&json!({"id":errand["errand"]})), "model-mate");

    // Past the crew share on both: refused, and told nothing is queued.
    rusqlite::Connection::open(f.root.join("ledger/resources.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO route_usage(route, weekly_utilization, weekly_ends_ms, updated_ms)
             VALUES('codex/-/model-spare', 0.85, ?1, ?2)",
            rusqlite::params![now + 3_600_000, now],
        )
        .unwrap();
    let refusal = f.refused(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert!(
        refusal.contains("every first_mate route for engineering is unavailable"),
        "{refusal}"
    );
    assert!(refusal.contains("80% for crew"), "{refusal}");
    assert!(refusal.contains("retry once one clears"), "{refusal}");
}

#[test]
fn first_mate_route_lists_use_ship_override_and_reject_unknown_routes() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"steady": codex("model-steady", "credits"), "spare": codex("model-spare", "plan")},
        "first_mate": ["steady"],
        "ships": {"engineering": {"first_mate": ["spare"]}}
    }));
    let policy = f.json(None, "routes.show", json!({}));
    assert_eq!(
        policy["ships"]["engineering"]["first_mate"],
        json!(["spare"])
    );
    let error = f.refused(
        None,
        "routes.set",
        json!({"routes":{"steady":codex("model-steady", "credits")},"first_mate":["missing"]}),
    );
    assert!(
        error.contains("first_mate names unknown route missing"),
        "{error}"
    );
    // A free allowance's known daily reset is a UTC time of day, HH:MM.
    let mut free = codex("model-free", "free");
    free["allowance_resets_utc"] = json!("00:00");
    f.routes(json!({"routes": {"free": free.clone()}, "crew": ["free"]}));
    for wrong in ["+1:30", "24:00", "7:00"] {
        free["allowance_resets_utc"] = json!(wrong);
        let error = f.refused(
            None,
            "routes.set",
            json!({"routes": {"free": free.clone()}}),
        );
        assert!(error.contains("allowance_resets_utc"), "{error}");
    }
}

#[test]
fn agents_launch_only_listed_routes_while_the_captain_may_launch_any() {
    let f = Fleet::new();
    f.routes(json!({"routes": {"steady": codex("model-steady", "credits"), "spare": codex("model-spare", "plan")},
                    "crew": ["steady"]}));
    let unlisted =
        json!({"task_id":"fix","manifest":"bundled:codex","options":{"model":"model-pricey"}});
    let refusal = f.refused(Some("engineering"), "agent.start", unlisted.clone());
    assert!(
        refusal.contains("is not one of engineering's crew routes"),
        "{refusal}"
    );
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"spare"}),
    );
    assert!(
        refusal.contains("route spare is not one of engineering's crew routes"),
        "{refusal}"
    );
    let listed =
        json!({"task_id":"fix","manifest":"bundled:codex","options":{"model":"model-steady"}});
    assert_eq!(
        f.model(&f.json(Some("engineering"), "agent.start", listed)),
        "model-steady"
    );
    assert_eq!(
        f.model(&f.json(None, "agent.start", unlisted)),
        "model-pricey"
    );
}

/// Crew reserve the memory their launch declares, as admission counts any
/// estimate: a crew that does not fit beside the crews already running is
/// refused, or waits for capacity.
#[test]
fn a_crew_launch_reserves_its_memory_and_waits_when_it_does_not_fit() {
    let f = Fleet::new();
    // Stub harnesses run for a while, so their reservations stay visible.
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nsleep 30\n").unwrap();
    let sized = |bytes: u64| {
        let mut route = codex("model-sized", "credits");
        route["options"]["memory_bytes"] = json!(bytes);
        route
    };
    f.routes(
        json!({"routes": {"small": sized(1 << 20), "big": sized(1 << 50)},
        "crew": ["small", "big"]}),
    );
    // The first crew runs alone, so it needs only the floor.
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-small","route":"small"}),
    );
    let job = f.json(None, "job.status", json!({"id":"crew-small"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 20, "{job}");
    let status = f.json(None, "resource.status", json!({}));
    let reserved = status["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == "crew-small")
        .unwrap_or_else(|| panic!("no reservation for crew-small: {status}"));
    assert_eq!(reserved["demand"]["memory_bytes"], 1u64 << 20, "{reserved}");
    // A crew that does not fit beside it is refused, or waits when asked.
    let refused = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"big"}),
    );
    assert!(refused.contains("admission refused"), "{refused}");
    assert!(refused.contains("estimated for this job"), "{refused}");
    let queued = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-big","route":"big","wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    let job = f.json(None, "job.status", json!({"id":"crew-big"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 50, "{job}");
    let status = f.json(None, "resource.status", json!({}));
    assert!(
        status["queue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["job_id"] == "crew-big"),
        "{status}"
    );
    f.json(None, "job.cancel", json!({"id":"crew-big"}));
    f.queue_drained();
    f.json(None, "job.cancel", json!({"id":"crew-small"}));
}

/// A manifest with no memory figure reserves the host's default.
#[test]
fn a_crew_manifest_without_a_memory_figure_reserves_the_default() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nsleep 30\n").unwrap();
    // This host's setting for codex would replace the default under test.
    f.json(
        None,
        "resource.configure",
        json!({"crew_memory_bytes":null,"crew_default_memory_bytes":1u64 << 50}),
    );
    let dir = f.root.join("custom/codex");
    fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("manifest.json");
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/codex/adapter.py");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"codex","executable":["python3", adapter],
            "timeout_ms":2000,"max_output_bytes":65536})
        .to_string(),
    )
    .unwrap();
    let mut holder = codex("model-holder", "credits");
    holder["options"]["memory_bytes"] = json!(1u64 << 20);
    f.routes(json!({"routes": {"holder": holder,
        "own": {"manifest": manifest, "cost": "credits",
            "options": {"model":"model-own","sandbox":"danger-full-access"}}},
        "crew": ["holder", "own"]}));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-holder","route":"holder"}),
    );
    let queued = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-default","route":"own","wait_for_capacity":true}),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    let job = f.json(None, "job.status", json!({"id":"crew-default"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 50, "{job}");
    f.json(None, "job.cancel", json!({"id":"crew-default"}));
    f.queue_drained();
    f.json(None, "job.cancel", json!({"id":"crew-holder"}));
}

/// Lowering the host's default changes the reservation of a crew whose
/// manifest declares no figure, and resource.status says where it came from.
#[test]
fn lowering_the_default_moves_a_crew_with_no_manifest_figure() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nsleep 30\n").unwrap();
    let dir = f.root.join("custom/codex");
    fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("manifest.json");
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/codex/adapter.py");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"codex","executable":["python3", adapter],
            "timeout_ms":2000,"max_output_bytes":65536})
        .to_string(),
    )
    .unwrap();
    f.json(
        None,
        "resource.configure",
        json!({"crew_memory_bytes":null,"crew_default_memory_bytes":1u64 << 20}),
    );
    let route = json!({"manifest": manifest, "cost": "credits",
        "options": {"model":"model-default","sandbox":"danger-full-access"}});
    f.routes(json!({"routes": {"own": route}, "crew": ["own"]}));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-lowered","route":"own"}),
    );
    let status = f.json(None, "resource.status", json!({}));
    let reserved = status["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["job_id"] == "crew-lowered")
        .unwrap_or_else(|| panic!("no reservation for crew-lowered: {status}"));
    assert_eq!(reserved["demand"]["memory_bytes"], 1u64 << 20, "{reserved}");
    let codex = status["crew_memory"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "codex")
        .unwrap_or_else(|| panic!("no crew memory for codex: {status}"));
    assert_eq!(codex["memory_bytes"], 1u64 << 20, "{codex}");
    assert_eq!(codex["source"], "default", "{codex}");
    f.json(None, "job.cancel", json!({"id":"crew-lowered"}));
}

/// A memory figure a caller passes with an explicit manifest is not a route's
/// figure, so it cannot shrink the crew's own reservation.
#[test]
fn a_caller_memory_figure_cannot_shrink_an_explicit_manifest_crew() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nsleep 30\n").unwrap();
    f.json(
        None,
        "resource.configure",
        json!({"crew_memory_bytes":null,"crew_default_memory_bytes":1u64 << 50}),
    );
    let dir = f.root.join("custom/codex");
    fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("manifest.json");
    let adapter = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins/codex/adapter.py");
    fs::write(
        &manifest,
        json!({"schema_version":1,"id":"codex","executable":["python3", adapter],
            "timeout_ms":2000,"max_output_bytes":65536})
        .to_string(),
    )
    .unwrap();
    // Alone on the host a crew needs only the floor, so it starts; its
    // reservation is the manifest's default, not the caller's figure.
    let started = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-explicit","manifest":manifest,
            "options":{"model":"model-own","memory_bytes":1u64 << 20}}),
    );
    assert_eq!(started["id"], "crew-explicit", "{started}");
    let job = f.json(None, "job.status", json!({"id":"crew-explicit"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 50, "{job}");
    // Cancel once the supervisor has recorded the payload: a job still
    // starting has no identity to signal yet.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.json(None, "job.status", json!({"id":"crew-explicit"}))["status"] == "starting"
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    f.json(None, "job.cancel", json!({"id":"crew-explicit"}));
}

/// The route's own figure beats this host's setting for its harness, which
/// beats the manifest's; a request's own memory raises the figure, never
/// lowers it.
#[test]
fn a_route_figure_beats_the_host_setting_which_beats_the_manifest() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nsleep 30\n").unwrap();
    f.json(
        None,
        "resource.configure",
        json!({"crew_memory_bytes":null}),
    );
    f.routes(json!({"routes": {"steady": codex("model-steady", "credits")}, "crew": ["steady"]}));
    // The bundled codex manifest declares no figure, so the 2 GiB default applies. It
    // runs alone, so only the floor counts for it.
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-manifest"}),
    );
    let job = f.json(None, "job.status", json!({"id":"crew-manifest"}));
    assert_eq!(job["estimate"]["memory_bytes"], 2u64 << 30, "{job}");
    // This host's setting for codex replaces the default.
    f.json(
        None,
        "resource.configure",
        json!({"crew_memory_bytes":{"codex":1u64 << 40}}),
    );
    let host = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-host","wait_for_capacity":true}),
    );
    assert_eq!(host["status"], "queued", "{host}");
    let job = f.json(None, "job.status", json!({"id":"crew-host"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 40, "{job}");
    f.json(None, "job.cancel", json!({"id":"crew-host"}));
    f.queue_drained();
    // The route's own figure replaces both.
    let mut sized = codex("model-sized", "credits");
    sized["options"]["memory_bytes"] = json!(1u64 << 41);
    f.routes(json!({"routes": {"sized": sized}, "crew": ["sized"]}));
    let route = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-route","wait_for_capacity":true}),
    );
    assert_eq!(route["status"], "queued", "{route}");
    let job = f.json(None, "job.status", json!({"id":"crew-route"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 41, "{job}");
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a != "memory_bytes")
    );
    f.json(None, "job.cancel", json!({"id":"crew-route"}));
    f.queue_drained();
    // A request asking for more memory than its route keeps the larger figure.
    let asked = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-asked","wait_for_capacity":true,
            "estimate":{"memory_bytes":1u64 << 50}}),
    );
    assert_eq!(asked["status"], "queued", "{asked}");
    let job = f.json(None, "job.status", json!({"id":"crew-asked"}));
    assert_eq!(job["estimate"]["memory_bytes"], 1u64 << 50, "{job}");
    f.json(None, "job.cancel", json!({"id":"crew-asked"}));
    f.queue_drained();
    f.json(None, "job.cancel", json!({"id":"crew-manifest"}));
}

/// A captain who moves a route to the front of the crew list changes where the
/// next crew without a route starts, even when an earlier crew on the same task
/// ran on the route that used to be first.
#[test]
fn a_reordered_crew_list_starts_the_next_crew_on_its_new_first_route() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"old": codex("model-old", "free"), "new": codex("model-new", "credits")},
        "crew": ["old", "new"]
    }));
    let first = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-old"}),
    );
    assert_eq!(f.model(&first), "model-old");
    f.queue_drained();
    f.routes(json!({
        "routes": {"old": codex("model-old", "free"), "new": codex("model-new", "credits")},
        "crew": ["new", "old"]
    }));
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("crew: new (credits; unranked), old (free; unranked)"),
        "{bearings}"
    );
    let second = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-new"}),
    );
    assert_eq!(f.model(&second), "model-new", "{second}");
}

/// A crew that failed on a route now backing off is not replayed on it by
/// job.retry; a fresh crew takes the captain's first available route.
#[test]
fn a_retried_crew_waits_for_its_backed_off_route_and_a_fresh_crew_takes_the_first_available() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/codex"), "#!/bin/sh\nexit 1\n").unwrap();
    f.routes(json!({
        "routes": {"old": codex("model-old", "free"), "new": codex("model-new", "credits")},
        "crew": ["old", "new"]
    }));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-first"}),
    );
    f.stopped("crew-first");
    f.back_off("codex/-/model-old");
    let refusal = f.refused(
        None,
        "job.retry",
        json!({"id":"crew-first","new_id":"crew-second"}),
    );
    assert!(refusal.contains("backing off"), "{refusal}");
    assert!(refusal.contains("start a fresh crew"), "{refusal}");
    let fresh = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-third"}),
    );
    assert_eq!(f.model(&fresh), "model-new", "{fresh}");
}

fn claude(model: &str) -> Value {
    json!({"manifest":"bundled:claude","options":{"model":model},"cost":"plan"})
}

/// The route a Claude crew reads its plan from, refused for new crew once the
/// account is past the share the route keeps for them, and left alone for a
/// first mate until the higher share it keeps for itself.
#[test]
fn a_reported_window_keeps_new_crew_off_the_route_and_leaves_the_first_mate_room() {
    let f = Fleet::new();
    // A fake Claude Code: it states the share of each plan window the account
    // has spent, the way the real one does on every run, and ends the run the
    // way a real one does.
    let script = |window: f64, week: f64| {
        let resets = privateer::store::Store::now_ms() / 1000;
        let session = "11111111-2222-3333-4444-555555555555";
        let init = json!({"type": "system", "subtype": "init", "session_id": session,
            "model": "claude-sonnet"});
        let event = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed",
            "unifiedWindows": {
                "five_hour": {"utilization": window, "resetsAt": resets + 3_600},
                "seven_day": {"utilization": week, "resetsAt": resets + 604_800}}}});
        let done = json!({"type": "result", "subtype": "success", "is_error": false,
            "session_id": session, "result": "ok"});
        let stream = format!("{init}\n{event}\n{done}\n");
        fs::write(f.root.join("stream.jsonl"), &stream).unwrap();
        fs::write(
            f.root.join("bin/claude"),
            "#!/bin/sh\ncat \"$(dirname \"$0\")/../stream.jsonl\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(f.root.join("bin/claude"), fs::Permissions::from_mode(0o700)).unwrap();
    };
    let mut shared = claude("sonnet");
    shared["allowance"] = json!({"window_hours": 5, "weekly_utc": "Mon 00:00"});
    shared["tier"] = json!(1);
    f.routes(json!({
        "routes": {"shared": shared, "other": codex("model-other", "plan")},
        "crew": ["shared", "other"],
        "first_mate": ["shared", "other"]
    }));
    // The first pass has nothing to read yet and leaves the scan cursor behind;
    // every run after this one is read by it.
    f.tick();
    // Nothing is over the crew share yet: the route takes the crew.
    script(0.5, 0.2);
    assert_eq!(
        f.model(&f.run_and_scan("crew-first", Some("shared"))),
        "sonnet"
    );
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(text.contains("window 50% used, week 20% used"), "{text}");

    // The weekly window passes the share kept for new crew. The next crew takes
    // the next route in its list; the refusal names the window and the share.
    script(0.5, 0.83);
    f.run_and_scan("crew-second", Some("shared"));
    assert_eq!(f.model(&f.run_and_scan("crew-third", None)), "model-other");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"shared"}),
    );
    assert!(refusal.contains("83% of its weekly window"), "{refusal}");
    assert!(refusal.contains("80% for crew"), "{refusal}");
    assert!(
        refusal.contains("take the next route in their list"),
        "{refusal}"
    );
    // The snapshot says which window is furthest spent, so a route that is
    // refused never looks simply clear. The account's reading is what it shows.
    rusqlite::Connection::open(f.root.join("ledger/resources.sqlite"))
        .unwrap()
        .execute(
            "UPDATE route_usage SET weekly_utilization=0.97 WHERE route='claude/-/*'",
            [],
        )
        .unwrap();
    let snapshot = f.json(None, "fleet.snapshot", json!({}));
    let routes = snapshot["machines"][0]["routes"].as_array().unwrap();
    let shared: Vec<&Value> = routes.iter().filter(|r| r["name"] == "shared").collect();
    assert_eq!(shared.len(), 1, "{snapshot}");
    assert_eq!(shared[0]["used"]["share"], 0.97, "{snapshot}");
    assert!(
        shared[0]["used"]["window"]
            .as_str()
            .unwrap()
            .starts_with("weekly window"),
        "{snapshot}"
    );
}

/// An expired window counts as 0, so crew are not refused once the window's
/// reset time has passed.
#[test]
fn an_expired_window_lets_crew_back_onto_the_route() {
    let f = Fleet::new();
    let mut shared = claude("sonnet");
    shared["allowance"] = json!({"window_hours": 5, "weekly_utc": "Mon 00:00"});
    shared["tier"] = json!(1);
    f.routes(json!({
        "routes": {"shared": shared, "other": codex("model-other", "plan")},
        "crew": ["shared", "other"]
    }));
    f.tick();
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let now = privateer::store::Store::now_ms();
    ledger
        .execute(
            "INSERT INTO route_usage(route, weekly_utilization, weekly_ends_ms, updated_ms)
             VALUES('claude/-/sonnet', 0.85, ?1, ?2)",
            rusqlite::params![now + 3_600_000, now],
        )
        .unwrap();
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"shared"}),
    );
    assert!(refusal.contains("85% of its weekly window"), "{refusal}");
    assert!(refusal.contains("80% for crew"), "{refusal}");

    // Once the reset time has passed, the window expires and counts as 0.
    ledger
        .execute(
            "UPDATE route_usage SET weekly_ends_ms=?1 WHERE route='claude/-/sonnet'",
            rusqlite::params![now - 1_000],
        )
        .unwrap();
    let start = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"shared"}),
    );
    assert_eq!(f.model(&start), "sonnet");
}

/// Claude window readings are account-wide, recorded under the account key
/// ("claude/-/*"), so a high reading on one model route refuses crew on another
/// model route of the same account.
#[test]
fn a_fresh_high_reading_on_one_claude_route_refuses_crew_on_another_claude_route_of_the_same_account()
 {
    let f = Fleet::new();
    let script = |window: f64, week: f64| {
        let resets = privateer::store::Store::now_ms() / 1000;
        let session = "11111111-2222-3333-4444-555555555555";
        let init = json!({"type": "system", "subtype": "init", "session_id": session,
            "model": "claude-sonnet"});
        let event = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed",
            "unifiedWindows": {
                "five_hour": {"utilization": window, "resetsAt": resets + 3_600},
                "seven_day": {"utilization": week, "resetsAt": resets + 604_800}}}});
        let done = json!({"type": "result", "subtype": "success", "is_error": false,
            "session_id": session, "result": "ok"});
        let stream = format!("{init}\n{event}\n{done}\n");
        fs::write(f.root.join("stream.jsonl"), &stream).unwrap();
        fs::write(
            f.root.join("bin/claude"),
            "#!/bin/sh\ncat \"$(dirname \"$0\")/../stream.jsonl\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(f.root.join("bin/claude"), fs::Permissions::from_mode(0o700)).unwrap();
    };
    let mut fast = claude("sonnet");
    fast["tier"] = json!(1);
    let mut deep = claude("opus");
    deep["tier"] = json!(2);
    f.routes(json!({
        "routes": {"fast": fast, "deep": deep, "other": codex("model-other", "plan")},
        "crew": ["fast", "deep", "other"]
    }));
    f.tick();

    // Run on `fast` reports 86% weekly utilization for the account.
    script(0.4, 0.86);
    f.run_and_scan("crew-fast", Some("fast"));

    // Both the route and the account key recorded the reading.
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let account_utilization: f64 = ledger
        .query_row(
            "SELECT weekly_utilization FROM route_usage WHERE route='claude/-/*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(account_utilization, 0.86);

    // Launching crew on `deep` is refused because its account is past the crew threshold.
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"deep"}),
    );
    assert!(refusal.contains("86% of its weekly window"), "{refusal}");
    assert!(refusal.contains("80% for crew"), "{refusal}");
}

/// A parked crew is in-flight running work, so it may resume on its route at
/// 85% headroom (past the 80% new crew share), and is blocked only by holds.
#[test]
fn a_parked_crew_resumes_on_its_route_at_eighty_five_percent() {
    let f = Fleet::new();
    let mut shared = claude("sonnet");
    shared["crew_headroom_pct"] = json!(80);
    shared["first_mate_headroom_pct"] = json!(95);
    f.routes(json!({
        "routes": {"shared": shared},
        "crew": ["shared"]
    }));
    f.tick();
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let now = privateer::store::Store::now_ms();
    ledger
        .execute(
            "INSERT INTO route_usage(route, weekly_utilization, weekly_ends_ms, updated_ms)
             VALUES('claude/-/sonnet', 0.85, ?1, ?2)",
            rusqlite::params![now + 3_600_000, now],
        )
        .unwrap();
    // New crew start is refused at 85%:
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"shared"}),
    );
    assert!(refusal.contains("85% of its weekly window"), "{refusal}");

    // But a parked crew resuming on its route is permitted to finish:
    let can_resume = f
        .in_process(|store| {
            privateer::crew_routes::resume_route(
                store,
                "engineering",
                "bundled:claude",
                &json!({"model": "sonnet"}),
            )
        })
        .unwrap();
    assert!(can_resume, "parked crew at 85% must be allowed to resume");

    // Provider holds still prevent resume:
    ledger
        .execute(
            "INSERT INTO model_routes(route, failures, until_ms, last_error, updated_ms)
             VALUES('claude/-/sonnet', 1, ?1, '429', ?2)",
            rusqlite::params![now + 3_600_000, now],
        )
        .unwrap();
    let blocked_resume = f
        .in_process(|store| {
            privateer::crew_routes::resume_route(
                store,
                "engineering",
                "bundled:claude",
                &json!({"model": "sonnet"}),
            )
        })
        .unwrap();
    assert!(!blocked_resume, "a held route must not resume");
}

/// A route whose harness reports nothing is measured by the runs it counts
/// against the budget the captain set for its window. No default: a route that
/// states no budget is never refused on a share nobody knows.
#[test]
fn a_run_budget_refuses_new_crew_once_the_window_is_mostly_spent() {
    let f = Fleet::new();
    let mut budget = codex("model-budget", "plan");
    budget["allowance"] = json!({"window_hours": 5, "runs": 3});
    budget["tier"] = json!(1);
    f.routes(json!({
        "routes": {"budget": budget, "other": codex("model-other", "plan")},
        "crew": ["budget", "other"]
    }));
    f.tick();
    let ledger = || rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let runs = || {
        ledger()
            .query_row(
                "SELECT runs FROM route_usage WHERE route='codex/-/model-budget'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
    };
    // Two of three runs: inside the four fifths a route keeps, so the third
    // still starts there and takes the route.
    f.run_and_scan("crew-one", Some("budget"));
    f.run_and_scan("crew-two", Some("budget"));
    assert_eq!(runs(), 2);
    assert_eq!(
        f.model(&f.run_and_scan("crew-three", Some("budget"))),
        "model-budget"
    );
    assert_eq!(runs(), 3);
    // The window is spent: the next crew without a route takes the next one.
    let next = f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(f.model(&next), "model-other");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"budget"}),
    );
    assert!(refusal.contains("run budget (3 of 3 runs)"), "{refusal}");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    assert!(
        bearings["text"].as_str().unwrap().contains("3 of 3 runs"),
        "{bearings}"
    );
    // A new window counts from nothing: the count is not carried over.
    ledger()
        .execute(
            "UPDATE route_usage SET runs_window_ends_ms=1 WHERE route='codex/-/model-budget'",
            [],
        )
        .unwrap();
    assert_eq!(
        f.model(&f.run_and_scan("crew-four", Some("budget"))),
        "model-budget"
    );
    assert_eq!(runs(), 1);
}

#[test]
fn headroom_settings_and_run_budgets_are_bounded_and_agree_across_one_identity() {
    let f = Fleet::new();
    let file = f.root.join("bad-routes.json");
    let refuse = |routes: Value, wanted: &str| {
        fs::write(&file, routes.to_string()).unwrap();
        let o = f.run(None, &["routes", "set", file.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            !o.status.success() && stderr.contains(wanted),
            "{routes}: {stderr}"
        );
    };
    let mut budget = codex("model-a", "plan");
    budget["allowance"] = json!({"window_hours": 5, "runs": 3});
    budget["tier"] = json!(1);
    for (setting, value, wanted) in [
        (
            "crew_headroom_pct",
            json!(0),
            "must be a whole percent from 1 to 100",
        ),
        (
            "first_mate_headroom_pct",
            json!(101),
            "must be a whole percent from 1 to 100",
        ),
        (
            "first_mate_headroom_pct",
            json!(95.5),
            "must be a whole percent from 1 to 100",
        ),
    ] {
        let mut route = codex("model-a", "plan");
        route[setting] = value;
        refuse(json!({"routes": {"a": route}}), wanted);
    }
    let mut none = budget.clone();
    none["allowance"] = json!({"window_hours": 5, "runs": 0});
    refuse(
        json!({"routes": {"a": none}}),
        "runs must be a whole number",
    );
    let mut other = budget.clone();
    other["manifest"] = budget["manifest"].clone();
    other["allowance"] = json!({"window_hours": 5, "runs": 4});
    other["note"] = json!("same model, another budget");
    refuse(
        json!({"routes": {"a": budget, "b": other}}),
        "with different allowance shapes",
    );
}

/// A Claude crew's replay keeps the route its launch recorded, whose argv has
/// launcher prefixes ahead of the harness. A replay is refused while that route
/// backs off, and once the captain no longer lists it.
#[test]
fn a_retried_claude_crew_keeps_its_route_and_obeys_the_captain_lists() {
    let f = Fleet::new();
    fs::write(f.root.join("bin/claude"), "#!/bin/sh\nexit 1\n").unwrap();
    f.routes(json!({
        "routes": {"haiku": claude("haiku"), "sonnet": claude("sonnet")},
        "crew": ["haiku", "sonnet"]
    }));
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"claude-first"}),
    );
    f.stopped("claude-first");
    f.json(
        None,
        "job.retry",
        json!({"id":"claude-first","new_id":"claude-kept"}),
    );
    let kept = f.json(None, "job.status", json!({"id":"claude-kept"}));
    assert_eq!(kept["model_route"], json!("claude/-/haiku"), "{kept}");
    f.stopped("claude-kept");
    f.routes(json!({
        "routes": {"haiku": claude("haiku"), "sonnet": claude("sonnet")},
        "crew": ["sonnet"]
    }));
    let unlisted = f.refused(
        None,
        "job.retry",
        json!({"id":"claude-first","new_id":"claude-unlisted"}),
    );
    assert!(unlisted.contains("no longer one of"), "{unlisted}");
    assert!(unlisted.contains("start a fresh crew"), "{unlisted}");
    f.routes(json!({
        "routes": {"haiku": claude("haiku"), "sonnet": claude("sonnet")},
        "crew": ["haiku", "sonnet"]
    }));
    f.back_off("claude/-/haiku");
    let backing_off = f.refused(
        None,
        "job.retry",
        json!({"id":"claude-first","new_id":"claude-late"}),
    );
    assert!(backing_off.contains("backing off"), "{backing_off}");
}

/// A ship's own crew list replaces the fleet's, and the bearings and routes.show
/// say which list each kind takes.
#[test]
fn a_ships_own_crew_list_overrides_the_fleet_list_and_says_so() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"cheap": codex("model-cheap", "free"), "steady": codex("model-steady", "credits")},
        "crew": ["steady"],
        "errand": ["steady"],
        "ships": {"engineering": {"crew": ["cheap"]}}
    }));
    let crew = f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(f.model(&crew), "model-cheap");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("crew: cheap (free; unranked) (ship's own list)"),
        "{bearings}"
    );
    assert!(
        text.contains("errand: steady (credits; unranked) (fleet list)"),
        "{bearings}"
    );
    // With every route unranked a requirement could never be met, so only
    // naming a route is offered.
    assert!(text.contains(r#"or pass "route":"<name>")."#), "{text}");
    assert!(!text.contains(r#"pass "require""#), "{text}");
    let shown = f.json(None, "routes.show", json!({}));
    assert_eq!(shown["applied"]["engineering"]["crew"], json!("ship"));
    assert_eq!(shown["applied"]["engineering"]["errand"], json!("fleet"));
}

/// A route the captain shapes as a daily allowance is launched ahead of a
/// carry-over route while it is idle, a capped one is skipped and says until
/// when, and once the cap has passed it is neither shown nor honoured.
#[test]
fn a_daily_allowance_goes_first_until_capped_and_returns_when_its_cap_passes() {
    let f = Fleet::new();
    let mut flash = codex("model-flash", "plan");
    flash["allowance"] = json!({"daily_utc": "07:00"});
    flash["tier"] = json!(1);
    f.routes(json!({
        "routes": {"steady": codex("model-steady", "plan"), "flash": flash},
        "crew": ["steady", "flash"]
    }));
    let start = || f.json(Some("engineering"), "agent.start", json!({"task_id":"fix"}));
    assert_eq!(f.model(&start()), "model-flash");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("flash (plan; daily 07:00 UTC, tier 1, no uses)"),
        "{text}"
    );
    assert!(
        text.contains("use-it-or-lose-it allowances nearest their refresh"),
        "{text}"
    );
    assert!(
        text.contains("Idle allowances: flash (daily 07:00 UTC, refreshes in"),
        "{text}"
    );

    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let route = "codex/-/model-flash";
    let until = i64::MAX / 4;
    ledger
        .execute(
            "INSERT INTO model_routes(route,failures,until_ms,last_error,updated_ms) VALUES(?1,1,?2,'quota exhausted',0)",
            rusqlite::params![route, until],
        )
        .unwrap();
    ledger
        .execute(
            "INSERT INTO route_usage(route,strikes,capped_until_ms,updated_ms) VALUES(?1,0,?2,?3)",
            rusqlite::params![route, until, i64::MAX / 8],
        )
        .unwrap();
    assert_eq!(f.model(&start()), "model-steady");
    let refusal = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"flash"}),
    );
    assert!(refusal.contains("is capped until"), "{refusal}");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(text.contains("tier 1, no uses) capped until"), "{text}");
    assert!(!text.contains("Idle allowances"), "{text}");

    // The hold passes: the stale cap is neither honoured nor shown.
    ledger
        .execute("UPDATE model_routes SET until_ms=1", [])
        .unwrap();
    ledger
        .execute("UPDATE route_usage SET capped_until_ms=1", [])
        .unwrap();
    assert_eq!(f.model(&start()), "model-flash");
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    assert!(
        !bearings["text"].as_str().unwrap().contains("capped"),
        "{bearings}"
    );
    // The explicit route is honoured over the ranking.
    let steady = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"steady"}),
    );
    assert_eq!(f.model(&steady), "model-steady");
}

#[test]
fn allowance_shapes_need_a_tier_one_shape_per_model_and_no_legacy_reset() {
    let f = Fleet::new();
    let file = f.root.join("bad-routes.json");
    let refuse = |routes: Value, wanted: &str| {
        fs::write(&file, routes.to_string()).unwrap();
        let o = f.run(None, &["routes", "set", file.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            !o.status.success() && stderr.contains(wanted),
            "{routes}: {stderr}"
        );
    };
    let mut untiered = codex("model-a", "plan");
    untiered["allowance"] = json!({"daily_utc": "07:00"});
    refuse(json!({"routes": {"a": untiered}}), "must state its tier");
    let mut both = codex("model-a", "free");
    both["allowance"] = json!({"daily_utc": "07:00"});
    both["tier"] = json!(1);
    both["allowance_resets_utc"] = json!("07:00");
    refuse(
        json!({"routes": {"a": both}}),
        "move the time into allowance.daily_utc",
    );
    let mut daily = codex("model-a", "plan");
    daily["allowance"] = json!({"daily_utc": "07:00"});
    daily["tier"] = json!(1);
    let mut rolling = daily.clone();
    rolling["allowance"] = json!({"window_hours": 5});
    refuse(
        json!({"routes": {"a": daily, "b": rolling}}),
        "with different allowance shapes",
    );
    let mut tiered = codex("model-a", "plan");
    tiered["tier"] = json!(10);
    refuse(json!({"routes": {"a": tiered}}), "tier must be");
}

#[test]
fn uses_need_a_tier_and_known_words_and_strength_shows_in_bearings_and_profile() {
    let f = Fleet::new();
    let file = f.root.join("bad-routes.json");
    let refuse = |routes: Value, wanted: &str| {
        fs::write(&file, routes.to_string()).unwrap();
        let o = f.run(None, &["routes", "set", file.to_str().unwrap()]);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(
            !o.status.success() && stderr.contains(wanted),
            "{routes}: {stderr}"
        );
    };
    let with = |uses: Value| {
        let mut opus = codex("model-opus", "plan");
        opus["tier"] = json!(1);
        opus["uses"] = uses;
        json!({"routes": {"opus": opus}})
    };
    let mut untiered = codex("model-a", "plan");
    untiered["uses"] = json!(["review"]);
    refuse(json!({"routes": {"a": untiered}}), "must state its tier");
    refuse(with(json!([])), "non-empty list");
    refuse(with(json!(["chat"])), "uses chat is not one of");
    refuse(with(json!(["review", "review"])), "lists use review twice");

    let mut opus = codex("model-opus", "plan");
    opus["tier"] = json!(1);
    opus["uses"] = json!(["review", "implement"]);
    opus["note"] = json!("Claude Code");
    let mut luna = codex("model-luna", "credits");
    luna["tier"] = json!(4);
    luna["uses"] = json!(["implement"]);
    let mut plain = codex("model-plain", "free");
    plain["note"] = json!("no tier at all");
    f.routes(json!({
        "routes": {"opus": opus, "luna": luna, "plain": plain},
        "crew": ["plain", "luna", "opus"]
    }));
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("plain (free; no tier at all; unranked)"),
        "{text}"
    );
    assert!(
        text.contains("luna (credits; tier 4, uses implement)"),
        "{text}"
    );
    assert!(
        text.contains("opus (plan; Claude Code; tier 1, uses review/implement)"),
        "{text}"
    );
    // Ranked routes steer launches to a requirement, which keeps the order,
    // rather than a route name, which skips it.
    assert!(
        text.contains(r#"pass "require":{"use":"<use>","tier":<n>}"#),
        "{text}"
    );
    assert!(text.contains("a named route skips the order"), "{text}");
    let profile = f.json(None, "ship.profile", json!({"ship":"engineering"}));
    assert_eq!(
        profile["routes"]["routes"]["opus"],
        json!({"cost":"plan","tier":1,"uses":["review","implement"]}),
        "{profile}"
    );
    assert_eq!(
        profile["routes"]["routes"]["plain"]["tier"],
        Value::Null,
        "{profile}"
    );

    // An unranked route never meets a requirement.
    let refused = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"plain","require":{"tier":2}}),
    );
    assert!(refused.contains("route plain is unranked"), "{refused}");
    let refused = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","route":"plain","require":{"use":"implement"}}),
    );
    assert!(refused.contains("route plain is unranked"), "{refused}");
}

#[test]
fn routes_set_warns_about_strength_prefixes_and_errand_routes_without_review() {
    let f = Fleet::new();
    let mut opus = codex("model-opus", "plan");
    opus["tier"] = json!(1);
    opus["uses"] = json!(["review", "implement"]);
    opus["note"] = json!("STRENGTH 1/top; Claude Code");
    let mut luna = codex("model-luna", "credits");
    luna["tier"] = json!(4);
    luna["uses"] = json!(["implement"]);
    let reply = f.json(
        None,
        "routes.set",
        json!({
            "routes": {"opus": opus, "luna": luna},
            "errand": ["opus", "luna"],
            "review": ["opus", "luna"]
        }),
    );
    let warnings: Vec<&str> = reply["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert_eq!(warnings.len(), 3, "{reply}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("route opus note starts with STRENGTH")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("route luna is on an errand list but has no review use")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w
                .contains("route luna is on a review list but is not tier 1 with the review use")),
        "{warnings:?}"
    );
}

/// The fleet snapshot lists the machine's routes in the states bearings show:
/// available, idle, backing off and capped, each hold with its end and its
/// classified error, and a hold no listed route names after them.
#[test]
fn the_fleet_snapshot_shows_each_route_in_the_state_bearings_show() {
    let f = Fleet::new();
    let mut flash = codex("model-flash", "free");
    flash["allowance"] = json!({"daily_utc": "07:00"});
    flash["tier"] = json!(1);
    let mut capped = codex("model-capped", "plan");
    capped["allowance"] = json!({"window_hours": 5});
    capped["tier"] = json!(1);
    f.routes(json!({
        "routes": {"steady": codex("model-steady", "plan"), "flash": flash,
            "busy": codex("model-busy", "credits"), "capped": capped},
        "crew": ["steady", "flash", "busy", "capped"]
    }));
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let until = i64::MAX / 4;
    for (route, error) in [
        ("codex/-/model-busy", "429 rate limited"),
        ("codex/-/model-capped", "usage limit reached"),
        ("codex/-/model-gone", "quota exhausted"),
    ] {
        ledger
            .execute(
                "INSERT INTO model_routes(route,failures,until_ms,last_error,updated_ms) VALUES(?1,1,?2,?3,0)",
                rusqlite::params![route, until, error],
            )
            .unwrap();
    }
    ledger
        .execute(
            "INSERT INTO route_usage(route,strikes,window_utilization,capped_until_ms,updated_ms)
             VALUES('codex/-/model-capped',0,0.97,?1,0)",
            [until],
        )
        .unwrap();

    let snapshot = f.json(None, "fleet.snapshot", json!({}));
    let routes = snapshot["machines"][0]["routes"].as_array().unwrap();
    let states: Vec<(Value, Value, Value)> = routes
        .iter()
        .map(|r| (r["name"].clone(), r["state"].clone(), r["until_ms"].clone()))
        .collect();
    assert_eq!(
        states,
        vec![
            (json!("busy"), json!("backing_off"), json!(until)),
            (json!("capped"), json!("capped"), json!(until)),
            (json!("flash"), json!("idle"), Value::Null),
            (json!("steady"), json!("available"), Value::Null),
            (Value::Null, json!("backing_off"), json!(until)),
        ],
        "{snapshot}"
    );
    assert_eq!(routes[0]["error"], "429 rate limited");
    assert_eq!(routes[0]["route"], "codex/-/model-busy");
    assert_eq!(routes[0]["cost"], "credits");
    assert_eq!(routes[0]["allowance"], Value::Null);
    assert_eq!(routes[0]["tier"], Value::Null);
    assert_eq!(routes[1]["allowance"]["shape"], "5h window");
    assert_eq!(routes[1]["allowance"]["window_used"], 0.97);
    assert_eq!(routes[1]["tier"], 1);
    assert_eq!(routes[2]["allowance"]["shape"], "daily 07:00 UTC");
    assert_eq!(routes[2]["allowance"]["last_used_ms"], Value::Null);
    assert_eq!(routes[2]["tier"], 1);
    assert_eq!(routes[3]["tier"], Value::Null);
    assert_eq!(routes[4]["route"], "codex/-/model-gone");
    assert_eq!(routes[4]["tier"], Value::Null);
    assert_eq!(routes[4]["error"], "quota exhausted");

    // Bearings read the same states.
    let bearings = f.json(None, "ship.bearings", json!({"ship":"engineering"}));
    let text = bearings["text"].as_str().unwrap();
    assert!(
        text.contains("busy (credits; unranked) backing off"),
        "{text}"
    );
    assert!(
        text.contains("tier 1, no uses, window 97% used) capped until"),
        "{text}"
    );
    assert!(
        text.contains("Idle allowances: flash (daily 07:00 UTC"),
        "{text}"
    );

    // The routes are part of the section: a hold ending is a change.
    let changed = snapshot["machines"][0]["changed_ms"].as_i64().unwrap();
    let again = f.json(None, "fleet.snapshot", json!({}));
    assert_eq!(again["machines"][0]["changed_ms"], changed, "{again}");
    ledger
        .execute(
            "UPDATE model_routes SET until_ms=1 WHERE route='codex/-/model-busy'",
            [],
        )
        .unwrap();
    let cleared = f.json(None, "fleet.snapshot", json!({}));
    assert_eq!(cleared["machines"][0]["routes"][0]["state"], "available");
    assert!(cleared["machines"][0]["changed_ms"].as_i64().unwrap() > changed);
}

#[test]
fn a_requirement_filters_and_the_cheapest_that_meets_it_runs() {
    let f = Fleet::new();
    let mut cheap = codex("model-cheap", "free");
    cheap["tier"] = json!(4);
    cheap["uses"] = json!(["implement"]);

    let mut carry = codex("model-carry", "plan");
    carry["tier"] = json!(1);
    carry["uses"] = json!(["review", "implement"]);

    let mut idle = codex("model-idle", "plan");
    idle["allowance"] = json!({"daily_utc": "07:00"});
    idle["tier"] = json!(1);
    idle["uses"] = json!(["review", "implement"]);

    // In the captain's list order: cheap, carry, idle.
    // carry and idle both meet require (tier <= 1, review).
    // cheap is cheaper but does not meet review at tier 1.
    // idle has an allowance (class 0), which beats carry-over (class 2),
    // so idle must be chosen over carry even though carry is listed first.
    f.routes(json!({
        "routes": {
            "cheap": cheap,
            "carry": carry,
            "idle": idle,
        },
        "errand": ["cheap", "carry", "idle"],
    }));

    let reply = f.json(
        Some("engineering"),
        "errand.start",
        json!({
            "focus": "task:fix",
            "brief": "review the changes",
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert_eq!(reply["status"], "running");
    assert_eq!(reply["route"]["name"], "idle");
    assert_eq!(
        reply["route"]["why"],
        "cheapest on engineering's errand list meeting review at tier <= 1: idle"
    );

    // Now with carry and cheap only (no allowance route), carry meets it.
    f.routes(json!({
        "routes": {
            "cheap": codex("model-cheap", "free"),
            "carry": carry,
        },
        "errand": ["cheap", "carry"],
    }));

    let reply2 = f.json(
        Some("engineering"),
        "errand.start",
        json!({
            "focus": "task:fix",
            "brief": "review the changes again",
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert_eq!(reply2["route"]["name"], "carry");
    assert_eq!(
        reply2["route"]["why"],
        "cheapest on engineering's errand list meeting review at tier <= 1: carry"
    );
}

#[test]
fn an_explicit_route_below_the_requirement_is_refused() {
    let f = Fleet::new();
    let mut luna = codex("model-luna", "credits");
    luna["tier"] = json!(4);
    luna["uses"] = json!(["implement"]);

    let mut opus = codex("model-opus", "plan");
    opus["tier"] = json!(1);
    opus["uses"] = json!(["review", "implement"]);

    f.routes(json!({
        "routes": {"luna": luna, "opus": opus},
        "crew": ["luna", "opus"],
        "errand": ["luna", "opus"],
    }));

    // Explicit route below requirement:
    let refused_route = f.refused(
        Some("engineering"),
        "agent.start",
        json!({
            "task_id": "fix",
            "route": "luna",
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert!(
        refused_route.contains("route luna is tier 4 with uses [implement]; this launch needs review at tier <= 1; routes on engineering's crew list that meet it: opus"),
        "{refused_route}"
    );

    // Explicit manifest below requirement:
    let refused_manifest = f.refused(
        Some("engineering"),
        "agent.start",
        json!({
            "task_id": "fix",
            "manifest": "bundled:codex",
            "options": {"model": "model-luna"},
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert!(
        refused_manifest.contains("route luna is tier 4 with uses [implement]; this launch needs review at tier <= 1; routes on engineering's crew list that meet it: opus"),
        "{refused_manifest}"
    );
}

#[test]
fn a_requirement_with_no_routes_file_is_refused() {
    let f = Fleet::new();
    // f has no routes configured.
    let refused_agent = f.refused(
        Some("engineering"),
        "agent.start",
        json!({
            "task_id": "fix",
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert!(
        refused_agent.contains("the captain has listed no routes to choose from"),
        "{refused_agent}"
    );

    let refused_errand = f.refused(
        Some("engineering"),
        "errand.start",
        json!({
            "focus": "task:fix",
            "brief": "review",
            "require": {"use": "review", "tier": 1},
        }),
    );
    assert!(
        refused_errand.contains("the captain has listed no routes to choose from"),
        "{refused_errand}"
    );
}

#[test]
fn a_crew_launch_cannot_forge_its_route_why() {
    let f = Fleet::new();
    let mut steady = codex("model-steady", "credits");
    steady["tier"] = json!(2);
    steady["uses"] = json!(["implement"]);
    f.routes(json!({
        "routes": {"steady": steady},
        "crew": ["steady"],
    }));
    let reply = f.json(
        Some("engineering"),
        "agent.start",
        json!({
            "task_id": "fix",
            "route_why": "forged route why from caller",
        }),
    );
    assert_ne!(reply["route"]["why"], "forged route why from caller");
    assert_eq!(
        reply["route"]["why"],
        "first in allowance order on engineering's crew list: steady (no requirement)"
    );
}

#[test]
fn an_operator_passing_require_with_an_unlisted_manifest_is_refused_as_unlisted() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"steady": codex("model-steady", "credits")},
        "crew": ["steady"],
    }));
    let unlisted = json!({
        "task_id": "fix",
        "manifest": "bundled:codex",
        "options": {"model": "model-pricey"},
        "require": {"tier": 1},
    });
    let refusal = f.refused(None, "agent.start", unlisted);
    assert!(
        refusal.contains("is not one of engineering's crew routes"),
        "unexpected refusal: {refusal}"
    );
    assert!(
        !refusal.contains("the captain has listed no routes to choose from"),
        "must not say no routes to choose from: {refusal}"
    );
}

/// The reviewer a review takes: a captain-set `review` list whose candidates
/// meet review at tier 1, a crew's model excluded with no exception, a
/// first-mate overlap allowed in a fresh session, and a crew job whose model
/// cannot be named refusing the choice until the ship declares its route.
#[test]
fn a_reviewer_comes_from_the_review_list_clear_of_the_authors() {
    let pi_sol = || {
        tiered(
            "bundled:pi",
            json!({"model": "gpt-6.1-sol", "provider": "chatgpt-home"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let opus = || {
        tiered(
            "bundled:claude",
            json!({"model": "claude-opus-5-5"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let quick = || {
        tiered(
            "bundled:codex",
            json!({"model": "model-quick"}),
            "free",
            3,
            &["implement"],
        )
    };
    let reviewer = |f: &Fleet| {
        f.in_process(|store| {
            let authors = privateer::review::authors::compute(
                store,
                "engineering",
                "feature/not-ours",
                0,
                &[],
            )
            .unwrap();
            privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
        })
    };

    // No review list at all: refuse, never fall back to another list.
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"steady": codex("model-steady", "credits")},
        "crew": ["steady"]
    }));
    let refused = format!("{:#}", reviewer(&f).unwrap_err());
    assert!(
        refused.contains("the captain has listed no review routes for engineering"),
        "{refused}"
    );

    // A review list whose routes do not meet review at tier 1 names each one
    // with its tier and uses.
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"quick": quick(), "sol": pi_sol(), "opus": opus()},
        "crew": ["quick"],
        "review": ["quick"]
    }));
    let refused = format!("{:#}", reviewer(&f).unwrap_err());
    assert!(
        refused.contains("no review route for engineering meets review at tier <= 1"),
        "{refused}"
    );
    assert!(
        refused.contains("quick: tier 3 with uses [implement]"),
        "{refused}"
    );

    // An Opus crew wrote some of it: Opus is excluded with no exception and
    // Sol reviews, fresh, with the author set naming the crew job.
    f.routes(json!({
        "routes": {
            "quick": quick(),
            "writer": tiered("bundled:claude", json!({"model": "claude-opus-5-5"}), "plan", 2, &["implement"]),
            "sol": pi_sol(),
            "opus": opus()
        },
        "crew": ["quick", "writer"],
        "review": ["sol", "opus"]
    }));
    let crew = f.run_and_scan("crew-writer", Some("writer"));
    assert_eq!(crew["id"], "crew-writer", "{crew}");
    let chosen = reviewer(&f).expect("sol is clear of the authors");
    assert_eq!(chosen.choice.name, "sol");
    assert_eq!(chosen.overlap, None);
    assert!(chosen.fresh, "every review is a new session");
    assert_eq!(
        chosen.choice.identity.as_deref(),
        Some("pi/chatgpt-home/gpt-6.1-sol")
    );
    assert!(
        chosen.choice.why.contains("clear of the code's authors"),
        "{}",
        chosen.choice.why
    );
    // The review list reaches bearings and the ship profile like any list.
    let bearings = f.json(None, "ship.bearings", json!({"ship": "engineering"}));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("review: sol (plan; tier 1, uses review/implement)"),
        "{bearings}"
    );
    let profile = f.json(None, "ship.profile", json!({"ship": "engineering"}));
    assert_eq!(profile["routes"]["review"]["names"], json!(["sol", "opus"]));

    // Nothing left after exclusions: refuse, naming the tier, uses and the
    // conflicting job of the one candidate.
    f.routes(json!({
        "routes": {
            "quick": quick(),
            "writer": tiered("bundled:claude", json!({"model": "claude-opus-5-5"}), "plan", 2, &["implement"]),
            "sol": pi_sol(),
            "opus": opus()
        },
        "crew": ["quick", "writer"],
        "review": ["opus"]
    }));
    let refused = format!("{:#}", reviewer(&f).unwrap_err());
    assert!(
        refused.contains("every review route for engineering is excluded by the code's authors"),
        "{refused}"
    );
    assert!(
        refused.contains("opus (tier 1 with uses [review, implement])"),
        "{refused}"
    );
    assert!(
        refused.contains("crew job(s) crew-writer ran on its model claude-opus-5-5"),
        "{refused}"
    );

    // A model only the first mate wrote on is allowed, and the overlap is
    // recorded with the fresh session.
    let f = Fleet::new();
    f.routes(json!({
        "routes": {
            "quick": tiered("bundled:codex", json!({"model": "gpt-6-luna"}), "free", 3, &["implement"]),
            "opus": opus()
        },
        "crew": ["quick"],
        "first_mate": ["opus"],
        "review": ["opus"]
    }));
    f.run_and_scan("crew-quick", Some("quick"));
    let chosen = reviewer(&f).expect("a first-mate overlap is allowed");
    assert_eq!(chosen.choice.name, "opus");
    assert_eq!(chosen.overlap.as_deref(), Some("first_mate"));
    assert!(chosen.fresh);
    assert!(
        chosen
            .choice
            .why
            .contains("overlapping only the first mate's models"),
        "{}",
        chosen.choice.why
    );

    // A crew job whose model cannot be named (here a bare alias) refuses the
    // choice, naming the job, until the ship declares that job's route.
    let f = Fleet::new();
    f.routes(json!({
        "routes": {
            "alias": tiered("bundled:claude", json!({"model": "opus"}), "plan", 3, &["implement"]),
            "sol": pi_sol()
        },
        "crew": ["alias"],
        "review": ["sol"]
    }));
    f.run_and_scan("crew-alias", Some("alias"));
    let (unknown, refused) = f.in_process(|store| {
        let authors =
            privateer::review::authors::compute(store, "engineering", "feature/not-ours", 0, &[])
                .unwrap();
        let refused = privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"));
        (authors.unknown_jobs.clone(), refused.unwrap_err())
    });
    assert_eq!(unknown, vec!["crew-alias".to_owned()]);
    assert!(refused.contains("cannot choose a reviewer"), "{refused}");
    assert!(refused.contains("crew-alias"), "{refused}");
    assert!(refused.contains("author_routes"), "{refused}");
    // Declaring that job's route clears the unknown and adds the model.
    let chosen = f
        .in_process(|store| {
            let authors = privateer::review::authors::compute(
                store,
                "engineering",
                "feature/not-ours",
                0,
                &[privateer::review::authors::Declaration {
                    job_id: "crew-alias".to_owned(),
                    route: "claude/-/claude-opus-5-5".to_owned(),
                }],
            )
            .unwrap();
            privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
        })
        .expect("the declaration stands for the job");
    assert_eq!(chosen.choice.name, "sol");
}

#[test]
fn a_workspace_branch_scopes_the_authors_and_a_declaration_to_their_jobs() {
    let pi_sol = || {
        tiered(
            "bundled:pi",
            json!({"model": "gpt-6.1-sol", "provider": "chatgpt-home"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let bare_alias = || {
        tiered(
            "bundled:claude",
            json!({"model": "opus"}),
            "plan",
            2,
            &["implement"],
        )
    };
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"writer": bare_alias(), "sol": pi_sol()},
        "crew": ["writer"],
        "review": ["sol"]
    }));
    // A child of the fixture's task, whose workspace branch a PR would carry,
    // and a task outside that tree.
    f.json(
        None,
        "task.create",
        json!({"id":"child","owner":"engineering","title":"Child","parent":"fix"}),
    );
    f.json(
        None,
        "task.create",
        json!({"id":"elsewhere","charter_id":"engineering","owner":"engineering",
               "title":"Elsewhere"}),
    );
    let crew = f.run_task_and_scan("child", "crew-child", Some("writer"));
    assert_eq!(crew["id"], "crew-child", "{crew}");
    f.run_task_and_scan("elsewhere", "crew-elsewhere", Some("writer"));

    let (authors, chosen) = f.in_process(|store| {
        // The branch the child task's workspace pushes:
        // `privateer/<home_hash>/<hash(task)>/<generation>`.
        let hash = |s: &str| format!("{:x}", sha2::Sha256::digest(s.as_bytes()))[..16].to_owned();
        let branch = format!(
            "privateer/{}/{}/g3n3r4t10n",
            hash(&store.home().to_string_lossy()),
            hash("child")
        );
        let authors =
            privateer::review::authors::compute(store, "engineering", &branch, 0, &[]).unwrap();
        // The task tree, not the whole home, is the scope: only the job on
        // the child is in it, and its bare alias leaves it unknown.
        assert_eq!(authors.unknown_jobs, vec!["crew-child".to_owned()]);
        assert!(!authors.crew_jobs.contains_key("claude-opus-5-5"));
        // A declaration naming a different model than the job is refused,
        // naming the job: this one names another harness entirely.
        let refused = privateer::review::authors::compute(
            store,
            "engineering",
            &branch,
            0,
            &[privateer::review::authors::Declaration {
                job_id: "crew-child".to_owned(),
                route: "pi/chatgpt-home/gpt-6.1-sol".to_owned(),
            }],
        )
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
        .unwrap_err();
        assert!(refused.contains("crew-child"), "{refused}");
        assert!(refused.contains("does not fit"), "{refused}");
        // So no route slips past the gate: the choice still refuses on the
        // unknown job until the declaration fits it.
        let refused = privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
            .unwrap_err();
        assert!(refused.contains("cannot choose a reviewer"), "{refused}");
        assert!(refused.contains("crew-child"), "{refused}");
        // The fitting declaration stands for that job alone, adds its model,
        // and clears exactly it.
        let authors = privateer::review::authors::compute(
            store,
            "engineering",
            &branch,
            0,
            &[privateer::review::authors::Declaration {
                job_id: "crew-child".to_owned(),
                route: "claude/-/claude-opus-5-5".to_owned(),
            }],
        )
        .unwrap();
        assert!(authors.unknown_jobs.is_empty());
        let reviewer =
            privateer::crew_routes::choose_reviewer(store, "engineering", &authors).unwrap();
        (authors, reviewer)
    });
    assert_eq!(
        authors.crew_jobs["claude-opus-5-5"],
        vec!["crew-child".to_owned()]
    );
    assert_eq!(chosen.choice.name, "sol");
    assert_eq!(chosen.overlap, None);
}

/// A declaration's alias fit proves no version: a job that ran on the bare
/// alias opus, declared as claude-opus-4-7, still excludes every reviewer
/// whose model carries opus, whatever its version. A second declaration for
/// the same job is refused, and a crew job with no route at all still
/// refuses the choice until its own declaration stands, clearing no alias it
/// cannot name.
#[test]
fn a_declared_alias_excludes_every_model_that_carries_it() {
    let pi_sol = || {
        tiered(
            "bundled:pi",
            json!({"model": "gpt-6.1-sol", "provider": "chatgpt-home"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let opus = || {
        tiered(
            "bundled:claude",
            json!({"model": "claude-opus-5-5"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let alias = || {
        tiered(
            "bundled:claude",
            json!({"model": "opus"}),
            "plan",
            3,
            &["implement"],
        )
    };
    let declared = || privateer::review::authors::Declaration {
        job_id: "crew-alias".to_owned(),
        route: "claude/-/claude-opus-4-7".to_owned(),
    };
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"writer": alias(), "sol": pi_sol(), "opus": opus()},
        "crew": ["writer"],
        "review": ["opus", "sol"]
    }));
    f.run_and_scan("crew-alias", Some("writer"));

    // The ship declares the job as claude-opus-4-7. The fit holds, since the
    // declared model carries the alias, but proves no version, so the alias
    // is kept beside the model and the Opus 5.5 reviewer stays excluded
    // though its model is not the declared one; Sol, which carries no opus,
    // reviews.
    let (aliases, chosen) = f.in_process(|store| {
        let authors = privateer::review::authors::compute(
            store,
            "engineering",
            "feature/not-ours",
            0,
            &[declared()],
        )
        .unwrap();
        assert_eq!(authors.crew_aliases.len(), 1);
        let chosen = privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .expect("sol carries no declared alias");
        (authors.crew_aliases, chosen)
    });
    assert_eq!(
        aliases,
        std::collections::BTreeMap::from([("opus".to_owned(), vec!["crew-alias".to_owned()])])
    );
    assert_eq!(chosen.choice.name, "sol");
    assert_eq!(chosen.overlap, None);

    // With the Opus route the only review route left, the refusal names the
    // job and the alias its model carries.
    f.routes(json!({
        "routes": {"writer": alias(), "sol": pi_sol(), "opus": opus()},
        "crew": ["writer"],
        "review": ["opus"]
    }));
    let refused = f.in_process(|store| {
        let authors = privateer::review::authors::compute(
            store,
            "engineering",
            "feature/not-ours",
            0,
            &[declared()],
        )
        .unwrap();
        privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
            .unwrap_err()
    });
    assert!(
        refused.contains("every review route for engineering is excluded by the code's authors"),
        "{refused}"
    );
    assert!(refused.contains("crew-alias"), "{refused}");
    assert!(refused.contains("alias opus"), "{refused}");

    // A second declaration for the same job is refused outright, and a crew
    // job with no route at all (no model_route, no readable argv) refuses the
    // choice, naming the job, until its own declaration stands; once it does,
    // it keeps no alias of its own, and the alias rule from the other job's
    // declaration still decides the choice.
    let refused = f.in_process(|store| {
        privateer::review::authors::compute(
            store,
            "engineering",
            "feature/not-ours",
            0,
            &[
                declared(),
                privateer::review::authors::Declaration {
                    job_id: "crew-alias".to_owned(),
                    route: "claude/-/claude-opus-5-5".to_owned(),
                },
            ],
        )
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
        .unwrap_err()
    });
    assert!(refused.contains("crew-alias"), "{refused}");
    assert!(refused.contains("declared twice"), "{refused}");

    let refused = f.in_process(|store| {
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES('crew-none',?1)",
                rusqlite::params![
                    json!({
                        "id": "crew-none", "task_id": "fix", "action": "spawn",
                        "created_ms": 1, "finished_ms": 2,
                        "model_route": null, "command": []
                    })
                    .to_string()
                ],
            )
            .unwrap();
        let authors =
            privateer::review::authors::compute(store, "engineering", "feature/not-ours", 0, &[])
                .unwrap();
        assert!(authors.unknown_jobs.contains(&"crew-none".to_owned()));
        privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
            .unwrap_err()
    });
    assert!(refused.contains("cannot choose a reviewer"), "{refused}");
    assert!(refused.contains("crew-none"), "{refused}");
    assert!(refused.contains("author_routes"), "{refused}");

    // Both declared: the no-route job's declaration stands with no alias of
    // its own, and the Opus reviewer is still excluded by the alias job's
    // declaration alone.
    let refused = f.in_process(|store| {
        let authors = privateer::review::authors::compute(
            store,
            "engineering",
            "feature/not-ours",
            0,
            &[
                declared(),
                privateer::review::authors::Declaration {
                    job_id: "crew-none".to_owned(),
                    route: "codex/-/gpt-6-luna".to_owned(),
                },
            ],
        )
        .unwrap();
        assert!(authors.unknown_jobs.is_empty());
        assert_eq!(
            authors.crew_aliases,
            std::collections::BTreeMap::from([("opus".to_owned(), vec!["crew-alias".to_owned()])])
        );
        privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
            .unwrap_err()
    });
    assert!(
        refused.contains("every review route for engineering is excluded by the code's authors"),
        "{refused}"
    );
    assert!(refused.contains("alias opus"), "{refused}");
}

/// Two route sets in a row each replace the routes file, and an in-process read
/// right after each sees the routes just set (route loading has no cache).
#[test]
fn consecutive_routes_sets_are_each_seen_by_in_process_reads() {
    let pi_sol = || {
        tiered(
            "bundled:pi",
            json!({"model": "gpt-6.1-sol", "provider": "chatgpt-home"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let opus = || {
        tiered(
            "bundled:claude",
            json!({"model": "claude-opus-5-5"}),
            "plan",
            1,
            &["review", "implement"],
        )
    };
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"sol": pi_sol(), "opus": opus()},
        "crew": ["sol"],
        "review": ["opus", "sol"]
    }));
    // Second rewrite immediately after the first, replacing the review list with only opus:
    f.routes(json!({
        "routes": {"sol": pi_sol(), "opus": opus()},
        "crew": ["sol"],
        "review": ["opus"]
    }));
    let chosen = f.in_process(|store| {
        let authors =
            privateer::review::authors::compute(store, "engineering", "feature/not-ours", 0, &[])
                .unwrap();
        let reviewer =
            privateer::crew_routes::choose_reviewer(store, "engineering", &authors).unwrap();
        reviewer.choice.name
    });
    assert_eq!(chosen, "opus");
    let profile = f.json(None, "ship.profile", json!({"ship": "engineering"}));
    assert_eq!(profile["routes"]["review"]["names"], json!(["opus"]));
}

/// A codex whose first stream is refused with the free tier's rate limit on
/// the given models, so a crew on one of them fails at once with a throttle.
fn throttling_codex(f: &Fleet, models: &[&str]) {
    let script = format!(
        "#!/usr/bin/env python3\nimport sys\nthrottled = {models:?}\n\
         if \"--model\" in sys.argv and sys.argv[sys.argv.index(\"--model\") + 1] in throttled:\n\
         \x20   print('{{\"type\":\"error\",\"error\":{{\"name\":\"UnknownError\",\"data\":\
         {{\"message\":\"Rate limit exceeded. Please try again later.\"}}}}}}', flush=True)\n\
         \x20   raise SystemExit(2)\nraise SystemExit(0)\n"
    );
    let path = f.root.join("bin/codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// How many jobs this fixture records as relaunches of a throttled crew.
fn relaunches(f: &Fleet) -> i64 {
    f.in_process(|store| {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM jobs
                   WHERE json_extract(record,'$.relaunch_of') IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    })
}

#[test]
fn a_crew_throttled_before_any_work_is_relaunched_once_on_the_next_route() {
    let f = Fleet::new();
    throttling_codex(&f, &["model-free"]);
    f.routes(json!({
        "routes": {"free": codex("model-free", "free"), "paid": codex("model-paid", "plan")},
        "crew": ["free", "paid"]
    }));
    // The first pass has nothing to read yet and leaves the scan cursor
    // behind; every run after this one is read by it.
    f.tick();
    let job = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"throttled"}),
    );
    assert_eq!(f.model(&job), "model-free");
    f.stopped("throttled");
    // The pass that holds the throttled route relaunches the same crew on the
    // next one in the list, once.
    f.tick();
    f.settled("crew-relaunch-throttled");
    let relaunched = f.json(None, "job.status", json!({"id":"crew-relaunch-throttled"}));
    assert_eq!(relaunched["status"], "succeeded", "{relaunched}");
    assert_eq!(relaunched["relaunch_of"], "throttled");
    assert_eq!(f.model(&relaunched), "model-paid");
    // The failed job's record says where its work went, and the route is held.
    let failed = f.json(None, "job.status", json!({"id":"throttled"}));
    let reason = failed["reason"].as_str().unwrap();
    assert!(
        reason.contains("relaunched as crew-relaunch-throttled"),
        "{failed}"
    );
    let ledger = rusqlite::Connection::open(f.root.join("ledger/resources.sqlite")).unwrap();
    let error: String = ledger
        .query_row(
            "SELECT COALESCE(last_error,'') FROM model_routes WHERE route='codex/-/model-free'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(error.contains("Rate limit"), "{error}");
    // The ship was told, in mail from crew under the task's focus.
    let inbox = f.json(
        Some("engineering"),
        "mail.inbox",
        json!({"address":"engineering","all":true,"limit":100}),
    );
    let notice = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "crew-relaunch-throttled")
        .expect("no relaunch notice");
    let body = notice["body"].as_str().unwrap();
    assert!(notice["task_id"] == "fix", "{notice}");
    assert!(body.contains("codex/-/model-free"), "{body}");
    assert!(body.contains("Rate limit"), "{body}");
    assert!(body.contains("crew-relaunch-throttled"), "{body}");
    assert!(body.contains("codex/-/model-paid"), "{body}");
    // Another pass relaunches nothing more.
    f.tick();
    assert_eq!(relaunches(&f), 1);
}

#[test]
fn a_relaunch_that_throttles_too_is_never_relaunched_again() {
    let f = Fleet::new();
    throttling_codex(&f, &["model-free", "model-paid"]);
    f.routes(json!({
        "routes": {"free": codex("model-free", "free"), "paid": codex("model-paid", "plan")},
        "crew": ["free", "paid"]
    }));
    f.tick();
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"throttled"}),
    );
    f.stopped("throttled");
    f.tick();
    // The relaunch lands on the paid route and throttles there too: it ends
    // failed for the first mate, not relaunched a second time.
    f.stopped("crew-relaunch-throttled");
    f.tick();
    assert_eq!(relaunches(&f), 1);
    let relaunched = f.json(None, "job.status", json!({"id":"crew-relaunch-throttled"}));
    assert_eq!(relaunched["status"], "failed", "{relaunched}");
    assert_eq!(f.model(&relaunched), "model-paid");
}

#[test]
fn a_throttle_whose_route_hold_has_not_landed_yet_waits_for_the_next_pass() {
    let f = Fleet::new();
    throttling_codex(&f, &["model-free"]);
    f.routes(json!({
        "routes": {"free": codex("model-free", "free"), "paid": codex("model-paid", "plan")},
        "crew": ["free", "paid"]
    }));
    f.tick();
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"throttled"}),
    );
    f.stopped("throttled");
    // The pass runs before the scan has held the route (here, without it):
    // the crew is left alone, and the pass tries it again rather than
    // dropping it.
    f.in_process(|store| privateer::crew_relaunch::throttled(store).unwrap());
    assert_eq!(relaunches(&f), 0);
    // Once the hold lands, as the scan's own pass would land it, the next
    // pass relaunches the crew on the next route in the list.
    f.back_off("codex/-/model-free");
    f.tick();
    f.settled("crew-relaunch-throttled");
    assert_eq!(relaunches(&f), 1);
    let relaunched = f.json(None, "job.status", json!({"id":"crew-relaunch-throttled"}));
    assert_eq!(f.model(&relaunched), "model-paid");
}

/// A codex whose first launch on the free model is refused with the free
/// tier's rate limit and whose later launches on it succeed, so one scan
/// pass can hold a route and then clear it; the paid model always throttles.
fn once_throttling_codex(f: &Fleet) {
    let script = r##"#!/usr/bin/env python3
import json, pathlib, sys
model = sys.argv[sys.argv.index('--model') + 1]
runs = pathlib.Path('codex-runs')
n = int(runs.read_text()) if runs.exists() else 0
runs.write_text(str(n + 1))
if model == 'model-paid' or (model == 'model-free' and n == 0):
    print(json.dumps({'type': 'error', 'error': {'name': 'UnknownError',
        'data': {'message': 'Rate limit exceeded. Please try again later.'}}}), flush=True)
    raise SystemExit(2)
raise SystemExit(0)
"##;
    let path = f.root.join("bin/codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_throttle_a_same_pass_success_cleared_is_skipped_and_blocks_no_later_job() {
    let f = Fleet::new();
    once_throttling_codex(&f);
    f.routes(json!({
        "routes": {"free": codex("model-free", "free"), "paid": codex("model-paid", "plan")},
        "crew": ["free", "paid"]
    }));
    f.tick();
    // The first crew throttles on the free route, and a second crew succeeds
    // on it before any pass reads either, so the pass that holds the route
    // for the first also clears it for the second: the hold is gone for good.
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"throttled"}),
    );
    f.stopped("throttled");
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"after","route":"free"}),
    );
    f.settled("after");
    // A later crew throttles on the paid route, whose hold stands.
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"later","route":"paid"}),
    );
    f.stopped("later");
    f.tick();
    // The throttled crew is decided for good — skipped, since no hold exists
    // — and does not block the later throttled crew behind it, which is
    // relaunched (back onto the free route, whose hold the success cleared).
    f.settled("crew-relaunch-later");
    assert_eq!(relaunches(&f), 1);
    let failed = f.json(None, "job.status", json!({"id":"throttled"}));
    assert!(
        !failed["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("relaunched")
    );
    let relaunched = f.json(None, "job.status", json!({"id":"crew-relaunch-later"}));
    assert_eq!(relaunched["relaunch_of"], "later");
    assert_eq!(relaunched["status"], "succeeded", "{relaunched}");
    assert_eq!(f.model(&relaunched), "model-free");
    // The skipped job is never revisited: no later pass relaunches it.
    f.tick();
    assert_eq!(relaunches(&f), 1);
}

/// A route whose harness the last readiness check found missing is passed
/// over like one backing off, and said so; one a launch names is refused,
/// never swapped; with every route missing, the refusal names each tool and
/// how to install it. A tool never checked counts as installed.
#[test]
fn a_route_whose_harness_is_not_installed_is_passed_over_for_the_next() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"opus": claude("claude-opus-5-5"), "steady": codex("model-steady", "credits")},
        "crew": ["opus", "steady"]
    }));
    let start = |id: &str, extra: Value| {
        let mut args = json!({"task_id": "fix", "id": id});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        args
    };
    // No row for claude yet: the choice is what it always was.
    let first = f.json(
        Some("engineering"),
        "agent.start",
        start("crew-1", json!({})),
    );
    assert_eq!(first["route"]["name"], "opus", "{first}");
    assert_eq!(
        first["route"]["why"],
        "first in allowance order on engineering's crew list: opus (no requirement)"
    );
    f.harness("claude", false);
    let second = f.json(
        Some("engineering"),
        "agent.start",
        start("crew-2", json!({})),
    );
    assert_eq!(second["route"]["name"], "steady", "{second}");
    assert_eq!(
        second["route"]["why"],
        "first in allowance order on engineering's crew list: steady (no requirement); \
         passed over opus: unavailable: claude not installed"
    );
    let named = f.refused(
        Some("engineering"),
        "agent.start",
        start("crew-3", json!({"route": "opus"})),
    );
    assert!(
        named.contains("route opus is unavailable: claude not installed"),
        "{named}"
    );
    assert!(
        named.contains("install claude so it is on the dispatcher's PATH"),
        "{named}"
    );
    // An agent naming the harness directly is refused the same way.
    let direct = f.refused(
        Some("engineering"),
        "agent.start",
        start(
            "crew-4",
            json!({"manifest": "bundled:claude", "options": {"model": "claude-opus-5-5"}}),
        ),
    );
    assert!(
        direct.contains("route opus is unavailable: claude not installed"),
        "{direct}"
    );
    f.harness("codex", false);
    let none = f.refused(
        Some("engineering"),
        "agent.start",
        start("crew-5", json!({})),
    );
    for wanted in [
        "every crew route for engineering is unavailable",
        "opus: unavailable: claude not installed",
        "steady: unavailable: codex not installed",
        "not installed on this machine: claude: install claude so it is on the dispatcher's PATH; \
         codex: install codex so it is on the dispatcher's PATH; see runtime.needs readiness",
    ] {
        assert!(none.contains(wanted), "{wanted}: {none}");
    }
    // Installed again, the route is taken again.
    f.harness("claude", true);
    let back = f.json(
        Some("engineering"),
        "agent.start",
        start("crew-6", json!({})),
    );
    assert_eq!(back["route"]["name"], "opus", "{back}");
    for job in ["crew-1", "crew-2", "crew-6"] {
        f.settled(job);
    }
}

/// A harness that is installed but fails its check is refused with that
/// check's why, never as "not installed".
#[test]
fn a_harness_that_fails_its_check_is_refused_with_the_check_s_own_why() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"opus": claude("claude-opus-5-5"), "steady": codex("model-steady", "credits")},
        "crew": ["opus", "steady"]
    }));
    let why = "claude 0.1.0 at /opt/bin/claude is incompatible: Privateer needs 2.0.0 or later";
    f.harness_failing("claude", why);
    let named = f.refused(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-1", "route": "opus"}),
    );
    assert!(
        named.contains(&format!("route opus is unavailable: {why}")),
        "{named}"
    );
    assert!(
        named.contains("to fix on this machine: claude: install claude 2.0 or later"),
        "{named}"
    );
    assert!(!named.contains("not installed"), "{named}");
    let passed = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-2"}),
    );
    assert_eq!(passed["route"]["name"], "steady", "{passed}");
    assert!(
        passed["route"]["why"]
            .as_str()
            .unwrap()
            .ends_with(&format!("passed over opus: unavailable: {why}")),
        "{passed}"
    );
    f.settled("crew-2");
}

/// A failing row nobody has checked again for 15 minutes means no checker
/// runs: it no longer passes a route over, so a tool installed since is used.
#[test]
fn a_stale_failing_harness_row_no_longer_passes_a_route_over() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"opus": claude("claude-opus-5-5"), "steady": codex("model-steady", "credits")},
        "crew": ["opus", "steady"]
    }));
    f.harness_checked("claude", false, 60_000);
    let fresh = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-1"}),
    );
    assert_eq!(fresh["route"]["name"], "steady", "{fresh}");
    f.harness_checked("claude", false, 16 * 60_000);
    let stale = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-2"}),
    );
    assert_eq!(stale["route"]["name"], "opus", "{stale}");
    for job in ["crew-1", "crew-2"] {
        f.settled(job);
    }
}

/// A parked crew whose route's harness is missing here starts fresh, so the
/// fresh start takes the first route still offered.
#[test]
fn a_parked_crew_does_not_resume_on_a_route_whose_harness_is_missing() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {"opus": claude("claude-opus-5-5"), "steady": codex("model-steady", "credits")},
        "crew": ["opus", "steady"]
    }));
    let resume = |f: &Fleet| {
        f.in_process(|store| {
            privateer::crew_routes::resume_route(
                store,
                "engineering",
                "bundled:claude",
                &json!({"model": "claude-opus-5-5"}),
            )
        })
        .unwrap()
    };
    assert!(resume(&f), "an installed route resumes");
    f.harness("claude", false);
    assert!(
        !resume(&f),
        "a route whose harness is missing must not resume"
    );
}

#[test]
fn a_review_route_whose_harness_is_not_installed_is_passed_over() {
    let f = Fleet::new();
    f.routes(json!({
        "routes": {
            "opus": tiered("bundled:claude", json!({"model": "claude-opus-5-5"}), "plan", 1, &["review"]),
            "sol": tiered("bundled:codex", json!({"model": "gpt-6.1-sol"}), "credits", 1, &["review"]),
        },
        "review": ["opus", "sol"]
    }));
    let reviewer = |f: &Fleet| {
        f.in_process(|store| {
            let authors = privateer::review::authors::compute(
                store,
                "engineering",
                "feature/not-ours",
                0,
                &[],
            )
            .unwrap();
            privateer::crew_routes::choose_reviewer(store, "engineering", &authors)
        })
    };
    assert_eq!(reviewer(&f).unwrap().choice.name, "opus");
    f.harness("claude", false);
    assert_eq!(reviewer(&f).unwrap().choice.name, "sol");
    f.harness("codex", false);
    let refused = format!("{:#}", reviewer(&f).unwrap_err());
    for wanted in [
        "every review route for engineering is unavailable",
        "opus: unavailable: claude not installed",
        "sol: unavailable: codex not installed",
        "see runtime.needs readiness",
    ] {
        assert!(refused.contains(wanted), "{wanted}: {refused}");
    }
}

/// Setting a route or a first mate on a harness that is not installed
/// succeeds and says what to install, rather than waiting for a launch.
#[test]
fn routes_set_and_a_first_mate_profile_flag_a_harness_that_is_not_installed() {
    let f = Fleet::new();
    let manifest = f.absent_manifest("privateer-absent-harness");
    let reply = f.json(
        None,
        "routes.set",
        json!({
            "routes": {"absent": {"manifest": manifest, "cost": "plan"},
                       "steady": codex("model-steady", "credits")},
            "crew": ["absent", "steady"]
        }),
    );
    assert_eq!(
        reply["readiness"]["missing"],
        json!([{"tool": "privateer-absent-harness",
            "why": "privateer-absent-harness is not on this command's PATH; \
                the dispatcher's own check follows within a minute",
            "install": "install privateer-absent-harness so it is on the dispatcher's PATH",
            "needed_by": ["route:absent"]}]),
        "{reply}"
    );
    let shown = f.json(None, "routes.show", json!({}));
    assert_eq!(shown["routes"]["absent"]["manifest"], manifest, "{shown}");
    // Nothing missing, nothing listed.
    let clean = f.json(
        None,
        "routes.set",
        json!({"routes": {"steady": codex("model-steady", "credits")}, "crew": ["steady"]}),
    );
    assert_eq!(clean["readiness"]["missing"], json!([]), "{clean}");

    let mut profile =
        f.json(None, "ship.show", json!({"name": "engineering"}))["first_mate"].clone();
    profile["manifest"] = json!(manifest);
    let put = f.json(
        None,
        "ship.put",
        json!({"name": "engineering", "first_mate": profile}),
    );
    assert_eq!(
        put["readiness"]["missing"],
        json!([{"tool": "privateer-absent-harness",
            "why": "privateer-absent-harness is not on this command's PATH; \
                the dispatcher's own check follows within a minute",
            "install": "install privateer-absent-harness so it is on the dispatcher's PATH",
            "needed_by": ["engineering"]}]),
        "{put}"
    );
    let ship = f.json(None, "ship.show", json!({"name": "engineering"}));
    assert_eq!(ship["first_mate"]["manifest"], manifest, "{ship}");
}
