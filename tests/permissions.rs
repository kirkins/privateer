mod support;

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const SECRET: &str = "deadbeef0123456789abcdef0123456789abcdef";
const FAKE: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,sys,time
SECRET='deadbeef0123456789abcdef0123456789abcdef'
if os.getenv('CAPTURE_ARGV'):
 pathlib.Path(os.environ['CAPTURE_ARGV']).write_text(json.dumps(sys.argv[1:]))
if '--print' in sys.argv:
 print(json.dumps({'type':'system','subtype':'init','session_id':'12345678-1234-4234-8234-123456789abc'}))
 if 'spent-model' in sys.argv:
  print(json.dumps({'type':'rate_limit_event','rate_limit_info':{'status':'rejected','rateLimitType':'five_hour','resetsAt':int(time.time())+3*3600}}))
  print(json.dumps({'type':'result','subtype':'success','is_error':True,'result':"You've hit your limit · resets 3pm",'session_id':'12345678-1234-4234-8234-123456789abc','permission_denials':[]}))
  sys.exit(0)
 # Like Claude Code 2.1.300 (tests/fixtures/claude): a denied tool use is in the
 # result's permission_denials unless the launch's policy allows it.
 allowed='bypassPermissions' in sys.argv or 'Bash(apify run *)' in sys.argv
 denied=[] if allowed else [{'tool_name':'Bash','tool_use_id':'toolu_1','tool_input':{'command':'apify run actor/x API_TOKEN='+SECRET,'description':'Run the actor'}}]
 print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':'12345678-1234-4234-8234-123456789abc','permission_denials':denied}))
"#;

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}
impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        for name in ["claude", "codex"] {
            let p = root.join("bin").join(name);
            fs::write(&p, FAKE).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let f = Self { _tmp: tmp, root };
        f.init("liaison", "claude");
        // Admission has its own tests. Crews here start on whatever memory the
        // test machine has, so neither the floor nor a crew's estimate may refuse them.
        f.ok(
            Some("liaison"),
            &[
                "resource.configure",
                "--json",
                &json!({"min_available_memory_bytes":0,"crew_default_memory_bytes":1u64 << 20,
                    "crew_memory_bytes":{"codex":1u64 << 20,"claude":1u64 << 20}})
                .to_string(),
            ],
        );
        f
    }
    fn command(&self, ship: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(ship.map(|s| self.root.join(s)).unwrap_or(self.root.clone()))
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
            .env("CAPTURE_ARGV", self.root.join("argv.json"))
            .env(
                "PRIVATEER_PLUGINS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        c
    }
    fn ok(&self, ship: Option<&str>, args: &[&str]) -> String {
        let o = self.command(ship, args).output().unwrap();
        assert!(
            o.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn json(&self, ship: &str, verb: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(Some(ship), &[verb, "--json", &args.to_string()])).unwrap()
    }
    fn init(&self, name: &str, harness: &str) {
        let folder = self.root.join(name);
        fs::create_dir_all(&folder).unwrap();
        self.ok(Some(name), &["init", "--ship", name, "--harness", harness]);
        let mut profile = self.json(name, "ship.show", json!({"name":name}));
        profile["first_mate"]["batch_secs"] = json!(0);
        profile["first_mate"]["retry_backoff_secs"] = json!(0);
        self.json(name, "ship.put", profile);
    }
    fn fails(&self, ship: &str, verb: &str, args: Value) -> String {
        let o = self
            .command(Some(ship), &[verb, "--json", &args.to_string()])
            .output()
            .unwrap();
        assert!(
            !o.status.success(),
            "{verb} succeeded: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        String::from_utf8_lossy(&o.stderr).into_owned()
    }
    /// A crew on the ship's duty task, run to its end.
    fn crew(&self, ship: &str, id: &str, extra: Value) -> Value {
        let profile = self.json(ship, "ship.show", json!({"name":ship}));
        let mut args = json!({"id":id,"task_id":format!("{ship}-duty"),
            "manifest":profile["first_mate"]["manifest"],"options":profile["first_mate"]["options"]});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        self.json(ship, "agent.start", args);
        self.wait(ship, id)
    }
    fn captain_mail(&self, ship: &str) -> Vec<Value> {
        self.json(ship, "mail.inbox", json!({"address":"captain"}))["messages"]
            .as_array()
            .unwrap()
            .clone()
    }
    fn asks(&self, ship: &str) -> Vec<Value> {
        self.captain_mail(ship)
            .into_iter()
            .filter(|m| m["kind"] == "decision")
            .collect()
    }
    fn show(&self, ship: &str, id: &Value) -> Value {
        self.json(ship, "mail.show", json!({"id":id}))
    }
    fn jobs(&self, ship: &str) -> usize {
        self.json(ship, "job.list", json!({}))["jobs"]
            .as_array()
            .unwrap()
            .len()
    }
    fn plan(&self, name: &str) -> Value {
        let s = self.json(name, "ship.show", json!({"name":name}));
        self.json(name,"plugin.command",json!({"task_id":format!("{name}-duty"),"manifest":s["first_mate"]["manifest"],"options":s["first_mate"]["options"]}))
    }
    fn wait(&self, name: &str, id: &str) -> Value {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let j = self.json(name, "job.status", json!({"id":id}));
            if !matches!(
                j["status"].as_str(),
                Some("running" | "starting" | "launching" | "queued")
            ) {
                return j;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[test]
fn captain_policy_applies_to_old_and_future_ships_without_editing_harness_settings() {
    let f = Fleet::new();
    let settings = f.root.join("liaison/.claude/settings.json");
    let before = fs::read(&settings).unwrap();
    let standard = f.plan("liaison").to_string();
    assert!(standard.contains("Bash(git *)") && standard.contains("Bash(gh *)"));
    f.ok(None, &["permissions", "allow", "Bash(forge-helper *)"]);
    assert!(
        f.plan("liaison")
            .to_string()
            .contains("Bash(forge-helper *)")
    );
    f.ok(None, &["permissions", "yolo"]);
    f.init("engineering", "codex");
    assert!(f.plan("liaison").to_string().contains("bypassPermissions"));
    let plan = f.plan("engineering");
    let argv = plan["argv"].as_array().unwrap();
    assert!(
        argv.iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
    );
    assert!(!argv.iter().any(|a| a == "--sandbox"));
    assert_eq!(
        f.json("engineering", "ship.show", json!({"name":"engineering"}))["first_mate"]["options"]
            ["sandbox"],
        "workspace-write"
    );
    f.ok(None, &["permissions", "standard"]);
    assert!(!f.plan("liaison").to_string().contains("bypassPermissions"));
    assert!(
        f.plan("engineering")
            .to_string()
            .contains("workspace-write")
    );
    assert_eq!(fs::read(&settings).unwrap(), before);
    for name in ["liaison", "engineering"] {
        assert!(
            !f.json(name, "charter.show", json!({"id":name}))["actions"]
                .as_array()
                .unwrap()
                .contains(&json!("deploy_production"))
        );
    }
    let mode = fs::metadata(f.root.join("fleet.permissions.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}

#[test]
fn agents_cannot_switch_fleet_mode_or_add_rules() {
    let f = Fleet::new();
    for args in [
        vec!["permissions", "yolo"],
        vec!["permissions", "allow", "Bash(*)"],
        vec!["permissions.set", "--json", "{\"mode\":\"yolo\"}"],
    ] {
        let o = f
            .command(Some("liaison"), &args)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(!o.status.success());
    }
    assert!(!f.root.join("fleet.permissions.json").exists());
}

#[test]
fn attended_launches_receive_the_same_fleet_policy() {
    let f = Fleet::new();
    f.ok(None, &["permissions", "yolo"]);
    f.ok(
        Some("liaison"),
        &["start", "claude", "--", "--permission-mode", "plan"],
    );
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        args.windows(2)
            .any(|v| v == ["--permission-mode", "bypassPermissions"])
    );
    assert!(!args.contains(&"plan".into()));
    assert!(args.contains(&"Bash(git *)".into()));
    assert!(args.contains(&"Bash(gh *)".into()));
    // The attended session is the first mate: it delegates through crew jobs.
    let blocked = args.iter().position(|a| a == "--disallowedTools").unwrap();
    assert!(args[blocked..].contains(&"Workflow".into()));
}

#[test]
fn a_first_mates_own_denial_is_one_decision_and_pauses_automatic_retries() {
    let f = Fleet::new();
    f.json("liaison","mail.send",json!({"id":"request","from":"captain","to":"liaison","kind":"request","body":"do the task"}));
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let woke = f.json("liaison", "job.list", json!({}))["jobs"].clone();
    assert_eq!(woke.as_array().unwrap().len(), 1, "{woke}");
    let wake = woke[0]["id"].as_str().unwrap();
    assert_eq!(f.wait("liaison", wake)["status"], "failed");
    let asks = f.asks("liaison");
    assert_eq!(asks.len(), 1, "{asks:?}");
    let block = &asks[0]["payload"]["block"];
    assert_eq!(block["scope"], "wake");
    assert_eq!(block["job_id"], wake);
    // The card shows the rule and a preview, never the planted secret.
    assert!(
        !f.captain_mail("liaison")
            .iter()
            .any(|m| m.to_string().contains(SECRET))
    );
    assert_eq!(
        block["denials"][0]["preview"],
        "apify run actor/x API_TOKEN=•••"
    );
    let before = f.jobs("liaison");
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(f.jobs("liaison"), before);
    // A blocked outcome must not freeze fresh captain input. It earns one
    // new attempt, then pauses again rather than repeatedly spending inference.
    f.json("liaison","mail.send",json!({"id":"new-input","from":"captain","to":"liaison","kind":"request","body":"fresh guidance"}));
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let fresh = f.json("liaison", "job.list", json!({}))["jobs"].clone();
    assert_eq!(fresh.as_array().unwrap().len(), before + 1);
    for j in fresh.as_array().unwrap() {
        f.wait("liaison", j["id"].as_str().unwrap());
    }
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(f.jobs("liaison"), before + 1);
    assert_eq!(f.asks("liaison").len(), 1);
    f.ok(None, &["permissions", "yolo"]);
    assert_eq!(
        f.show("liaison", &asks[0]["id"])["end_reason"],
        "policy changed"
    );
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let jobs = f.json("liaison", "job.list", json!({}))["jobs"].clone();
    assert_eq!(jobs.as_array().unwrap().len(), before + 2);
    for j in jobs.as_array().unwrap() {
        f.wait("liaison", j["id"].as_str().unwrap());
    }
}

#[test]
fn a_crew_denial_becomes_one_typed_decision_and_parks_its_task_quietly() {
    let f = Fleet::new();
    assert_eq!(f.crew("liaison", "crew-one", json!({}))["status"], "failed");
    let asks = f.asks("liaison");
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = &asks[0];
    let block = &ask["payload"]["block"];
    assert_eq!(block["scope"], "crew");
    assert_eq!(block["job_id"], "crew-one");
    assert_eq!(block["denials"][0]["tool"], "Bash");
    assert_eq!(block["denials"][0]["rule"], "Bash(apify run *)");
    assert!(ask["body"].as_str().unwrap().contains("crew-one"));
    let options: Vec<&str> = ask["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["allow", "without", "stop"]);
    assert!(!ask.to_string().contains(SECRET));
    // The crew's outcome names the ask and waits for the next real wake.
    let outcome = f.json("liaison", "mail.inbox", json!({"address":"liaison"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["from"] == "crew")
        .cloned()
        .unwrap();
    assert_eq!(outcome["urgency"], "low", "{outcome}");
    assert_eq!(outcome["payload"]["about"], ask["id"]);
    assert!(
        outcome["body"]
            .as_str()
            .unwrap()
            .contains(ask["id"].as_str().unwrap())
    );
    let before = f.jobs("liaison");
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(f.jobs("liaison"), before, "a parked ship was woken");
    let bearings = f.json("liaison", "ship.bearings", json!({"ship":"liaison"}));
    assert!(
        bearings.to_string().contains("Waiting on captain decision"),
        "{bearings}"
    );
    // Another crew would only hit the same denial; `replace` tries another way.
    let refused = f.fails("liaison", "agent.start", json!({"id":"crew-two","task_id":"liaison-duty",
        "manifest": f.json("liaison","ship.show",json!({"name":"liaison"}))["first_mate"]["manifest"]}));
    assert!(refused.contains("waits on captain decision"), "{refused}");
    assert_eq!(
        f.crew("liaison", "crew-three", json!({"replace":true}))["status"],
        "failed"
    );
    assert_eq!(f.asks("liaison").len(), 1, "a retry asked again");
    assert_eq!(f.show("liaison", &ask["id"])["state"], "open");
}

#[test]
fn only_the_operators_own_allow_adds_the_rule() {
    let f = Fleet::new();
    f.ok(
        Some("liaison"),
        &[
            "init",
            "--ship",
            "liaison",
            "--harness",
            "claude",
            "--liaison",
        ],
    );
    f.crew("liaison", "crew-one", json!({}));
    let ask = f.asks("liaison")[0]["id"].clone();
    let reply = |payload: Value| json!({"from":"captain","to":"liaison","kind":"reply","correlation":ask,"body":"Allow","payload":payload});
    // The marker is the runtime's, and a relayed answer never carries it.
    let forged = f.fails(
        "liaison",
        "mail.send",
        reply(json!({"answer":{"option_id":"allow"},"operator":true})),
    );
    assert!(
        forged.contains("payload.operator is set by the runtime"),
        "{forged}"
    );
    let relayed = f.fails(
        "liaison",
        "mail.send",
        reply(json!({"answer":{"option_id":"allow"},"relayed_by":"liaison"})),
    );
    assert!(
        relayed.contains("Allow must come from the captain directly"),
        "{relayed}"
    );
    f.json(
        "liaison",
        "mail.send",
        json!({"id":"said","from":"captain","to":"liaison","kind":"notice","body":"allow apify"}),
    );
    let relay = f
        .command(
            Some("liaison"),
            &[
                "captain.relay",
                "--json",
                &json!({"request":ask,"source":"said","body":"Allow"}).to_string(),
            ],
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(
        relay.status.success(),
        "{}",
        String::from_utf8_lossy(&relay.stderr)
    );
    assert_eq!(f.show("liaison", &ask)["state"], "answered");
    assert!(!f.ok(None, &["permissions", "show"]).contains("apify"));
    // The next denial is a new ask; the operator's own Allow adds the rule.
    f.crew("liaison", "crew-two", json!({}));
    let asks = f.asks("liaison");
    let second = asks.iter().find(|m| m["state"] == "open").unwrap()["id"].clone();
    assert_ne!(second, ask);
    f.json("liaison","mail.send",json!({"from":"captain","to":"liaison","kind":"reply","correlation":second,"body":"Allow","payload":{"answer":{"option_id":"allow"}}}));
    assert!(
        f.ok(None, &["permissions", "show"])
            .contains("Bash(apify run *)")
    );
    assert_eq!(
        f.crew("liaison", "crew-three", json!({}))["status"],
        "succeeded"
    );
}

#[test]
fn an_unrelated_policy_change_keeps_the_ask_and_a_covering_one_ends_it() {
    let f = Fleet::new();
    f.crew("liaison", "crew-one", json!({}));
    let ask = f.asks("liaison")[0].clone();
    f.ok(None, &["permissions", "allow", "Bash(other-tool *)"]);
    let kept = f.show("liaison", &ask["id"]);
    assert_eq!(kept["state"], "open", "{kept}");
    assert_ne!(
        kept["payload"]["policy_hash"],
        ask["payload"]["policy_hash"]
    );
    f.ok(None, &["permissions", "allow", "Bash(apify run *)"]);
    let ended = f.show("liaison", &ask["id"]);
    assert_eq!(
        (ended["state"].as_str(), ended["end_reason"].as_str()),
        (Some("withdrawn"), Some("policy changed"))
    );
    // The task was not scheduled, so its owner is told to carry on.
    let told = f.json("liaison", "mail.inbox", json!({"address":"liaison"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["payload"]["block_cleared"] == true)
        .cloned()
        .unwrap();
    assert_eq!(told["urgency"], "normal");
    assert_eq!(
        f.crew("liaison", "crew-two", json!({}))["status"],
        "succeeded"
    );
}

#[test]
fn an_errand_denial_asks_without_parking_the_task() {
    let f = Fleet::new();
    let errand = f.json(
        "liaison",
        "errand.start",
        json!({"focus":"task:liaison-duty","brief":"Review the step"}),
    );
    let id = errand["errand"].as_str().unwrap().to_owned();
    assert_eq!(f.wait("liaison", &id)["status"], "failed");
    let asks = f.asks("liaison");
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["payload"]["block"]["scope"], "errand");
    // The main task is not held: a crew still starts, and its own denial of
    // the same tool parks the task on the same ask.
    assert_eq!(f.crew("liaison", "crew-one", json!({}))["status"], "failed");
    assert_eq!(f.asks("liaison").len(), 1);
    let refused = f.fails("liaison", "agent.start", json!({"id":"crew-two","task_id":"liaison-duty",
        "manifest": f.json("liaison","ship.show",json!({"name":"liaison"}))["first_mate"]["manifest"]}));
    assert!(refused.contains("waits on captain decision"), "{refused}");
}

#[test]
fn relays_and_the_same_denial_elsewhere_join_the_open_ask_and_share_its_answer() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    // Storefront asked liaison for the work that is about to be blocked.
    f.json("storefront","mail.send",json!({"id":"req1","from":"storefront","to":"liaison","kind":"request","body":"Run the apify actor"}));
    f.json("storefront", "mail.flush", json!({}));
    f.crew("liaison", "crew-one", json!({}));
    let ask = f.asks("liaison")[0]["id"].clone();
    f.json("liaison", "mail.flush", json!({}));
    let told = f.json("storefront", "mail.inbox", json!({"address":"storefront"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["payload"]["about"] == ask)
        .cloned()
        .expect("storefront hears whom the captain has");
    assert_eq!(told["urgency"], "low");
    // Storefront relays the block anyway, citing what it was told.
    let relay = f.json(
        "storefront",
        "mail.send",
        json!({"from":"storefront","to":"captain","kind":"request",
        "correlation":told["id"],"body":"liaison is blocked on apify, please allow it"}),
    );
    assert_eq!(relay["attached_to"], ask, "{relay}");
    assert!(
        relay["note"]
            .as_str()
            .unwrap()
            .contains(&format!("added to {}", ask.as_str().unwrap()))
    );
    // A "still blocked?" notice to the blocked ship does not wake it.
    let nudge = f.json("storefront","mail.send",json!({"from":"storefront","to":"liaison","kind":"notice","about":ask,"body":"still blocked?"}));
    assert_eq!(nudge["urgency"], "low");
    // The same denial on another ship is its own ask, attached to the first.
    f.crew("storefront", "front-crew", json!({}));
    let theirs = f.asks("storefront");
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0]["payload"]["attached_to"], ask);
    let waiting = f.json("liaison", "captain.waiting", json!({}));
    let items = waiting["waiting"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{waiting}");
    assert_eq!(items[0]["mail"]["id"], ask);
    assert_eq!(items[0]["attached"], 2);
    // One answer reaches every attached item, in the home that holds it.
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":ask,
        "body":"Continue without it","payload":{"answer":{"option_id":"without"}}}),
    );
    for id in [&relay["id"], &theirs[0]["id"]] {
        let item = f.show("storefront", id);
        assert_eq!(item["state"], "answered", "{item}");
        assert!(item["answered_by"].is_string(), "{item}");
        let answer = f.show("storefront", &item["answered_by"]);
        assert!(answer["payload"]["fanned_from"].is_string(), "{answer}");
    }
    let answered = f.show("storefront", &theirs[0]["id"]);
    assert_eq!(
        f.show("storefront", &answered["answered_by"])["payload"]["answer"]["option_id"],
        "without"
    );
    // The captain chose to do without: a repeat opens nothing new.
    f.crew("liaison", "crew-two", json!({}));
    assert_eq!(f.asks("liaison").len(), 1);
}

#[test]
fn a_spent_claude_window_backs_off_every_claude_model_until_its_reset() {
    let f = Fleet::new();
    let profile = f.json("liaison", "ship.show", json!({"name":"liaison"}));
    let launch = |id: &str, model: &str| {
        let mut options = profile["first_mate"]["options"].clone();
        options["model"] = json!(model);
        let args = json!({"id":id,"task_id":"liaison-duty","manifest":profile["first_mate"]["manifest"],"options":options});
        f.command(
            Some("liaison"),
            &["agent.start", "--json", &args.to_string()],
        )
        .output()
        .unwrap()
    };
    // The first pass only starts the dispatcher's scan of finished runs.
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    // Claude exits 0 here; the error result alone must fail the run.
    assert!(launch("spent", "spent-model").status.success());
    assert_eq!(f.wait("liaison", "spent")["status"], "failed");
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let routes = f.json("liaison", "resource.status", json!({}))["model_routes"].clone();
    assert_eq!(routes[0]["route"], "claude/-/*", "{routes}");
    let wait_ms = routes[0]["until_ms"].as_i64().unwrap()
        - std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
    assert!((3 * 3_600_000 - wait_ms).abs() < 60_000, "{wait_ms}");
    let other = launch("other", "claude-haiku-5-5");
    assert!(!other.status.success());
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("Claude five_hour limit reached"),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
}
