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
        fleet
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
        "crew": ["steady"], "errand": ["review"],
        "ships": {"engineering": {"crew": ["cheap", "steady"]}}
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
