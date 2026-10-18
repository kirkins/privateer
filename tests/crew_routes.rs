mod support;

use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

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
    /// Wait until a job has stopped, so a retry of it is allowed.
    fn stopped(&self, job: &str) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = self.json(None, "job.status", json!({"id":job}));
            if !matches!(status["status"].as_str(), Some("starting" | "running")) {
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "{job} never stopped: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    fn command(&self, agent: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(self.root.join("engineering"))
            .args(args)
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
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
        fs::write(&file, routes.to_string()).unwrap();
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
}

fn codex(model: &str, cost: &str) -> Value {
    json!({"manifest":"bundled:codex","options":{"model":model,"sandbox":"danger-full-access"},"cost":cost})
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
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("cheap (free; trial runs only) backing off, steady (credits)"),
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
    // The bundled codex manifest declares 2 GiB and nothing overrides it. It
    // runs alone, so only the floor counts for it.
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-manifest"}),
    );
    let job = f.json(None, "job.status", json!({"id":"crew-manifest"}));
    assert_eq!(job["estimate"]["memory_bytes"], 2u64 << 30, "{job}");
    // This host's setting for codex replaces the manifest's figure.
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
            .contains("crew: new (credits), old (free)"),
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
