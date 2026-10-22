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
 # A turn auto mode's classifier refused: the denial line, as the runtime
 # reads it, for the command the test chose.
 if 'classifier-model' in sys.argv:
  command=pathlib.Path(os.environ['CAPTURE_ARGV']).with_name('denied-command').read_text()
  print(json.dumps({'type':'native.permission_blocked','denials':[{'tool_name':'Bash','tool_input':{'command':command},'reason':'[Production Deploy]','classifier':True}]}))
  print(json.dumps({'type':'result','subtype':'success','is_error':True,'result':'Refused','session_id':'12345678-1234-4234-8234-123456789abc','permission_denials':[]}))
  sys.exit(0)
 # Like Claude Code 2.1.300 (tests/fixtures/claude): a denied tool use is in the
 # result's permission_denials unless the launch's policy allows it.
 allowed='bypassPermissions' in sys.argv or 'Bash(apify run *)' in sys.argv
 command='cd /srv && make deploy | tee deploy.log' if 'script-model' in sys.argv else 'apify run actor/x API_TOKEN='+SECRET
 denied=[] if allowed else [{'tool_name':'Bash','tool_use_id':'toolu_1','tool_input':{'command':command,'description':'Run the actor'}}]
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
            .env("PATH", support::path_with(&self.root.join("bin")))
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
fn the_captains_standing_authorization_reaches_that_ships_auto_mode_classifier() {
    let f = Fleet::new();
    let grant = "Making liaison's own Apify actors public in the Apify Store.";
    let allow = |f: &Fleet| -> Vec<Value> {
        f.ok(Some("liaison"), &["start", "claude"]);
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
        args.iter()
            .position(|a| a == "--settings")
            .map(|i| {
                serde_json::from_str::<Value>(&args[i + 1]).unwrap()["autoMode"]["allow"].clone()
            })
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    };
    let agent = f
        .command(
            Some("liaison"),
            &["permissions", "authorize", "liaison", grant],
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(!agent.status.success(), "a ship cannot authorize itself");
    f.ok(None, &["permissions", "authorize", "liaison", grant]);
    f.ok(
        None,
        &["permissions", "authorize", "elsewhere", "Something else."],
    );
    let given = allow(&f);
    assert_eq!(given[0], "$defaults");
    assert!(given.contains(&json!(format!("The captain has authorized this: {grant}"))));
    assert!(
        !given
            .iter()
            .any(|a| a.as_str().unwrap().contains("Something else"))
    );
    f.ok(None, &["permissions", "unauthorize", "liaison", grant]);
    assert!(
        !allow(&f)
            .iter()
            .any(|a| a.as_str().unwrap().contains(grant))
    );
}

#[test]
fn the_captains_approval_reaches_first_mate_launches_until_it_is_revoked() {
    let f = Fleet::new();
    let allow = |f: &Fleet| -> Vec<String> {
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
        args.iter()
            .position(|a| a == "--settings")
            .map(|i| {
                serde_json::from_str::<Value>(&args[i + 1]).unwrap()["autoMode"]["allow"].clone()
            })
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let asked = f
        .command(
            Some("liaison"),
            &[
                "mail.send",
                "--json",
                &json!({"id":"q-55","from":"liaison","to":"captain","kind":"request",
                    "task_id":"liaison-duty","body":"Build 55 passed CI.",
                    "payload":{"approves":{"action":"Submit build 55 to TestFlight"}}})
                .to_string(),
            ],
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(
        asked.status.success(),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    let answered = f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":"q-55","body":"Submit build 55 to TestFlight"}),
    );
    assert_eq!(answered["approval"]["recorded"], true, "{answered}");
    let approved = format!(
        "The captain approved this in reply {} to request q-55, for ship liaison only, \
         on task liaison-duty, until ",
        answered["id"].as_str().unwrap()
    );
    let carries = |allow: &[String]| {
        allow
            .iter()
            .any(|a| a.starts_with(&approved) && a.ends_with(" UTC: Submit build 55 to TestFlight"))
    };
    // A row too long to tell auto mode, planted past the send-time check, is
    // skipped and audited; the launch still builds its settings.
    let database = f.root.join("liaison/.privateer/privateer.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,created_ms,expires_ms,source)
             VALUES('approval-long','liaison','liaison-duty','q-long','r-long',?1,0,?2,'reply')",
            rusqlite::params!["x".repeat(600), i64::MAX / 2],
        )
        .unwrap();
    // A hosted or attended pane gets it at its next launch.
    f.ok(Some("liaison"), &["start", "claude"]);
    assert!(carries(&allow(&f)), "{:?}", allow(&f));
    assert!(!allow(&f).iter().any(|a| a.contains("r-long")));
    let skipped: i64 = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM audit WHERE kind='permissions.approval_skipped'
             AND payload LIKE '%sentence too long%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(skipped >= 1);
    // A crew's launch never does.
    assert!(
        !f.plan("liaison")
            .to_string()
            .contains("The captain approved this")
    );
    // A wake does.
    f.json(
        "liaison",
        "mail.send",
        json!({"id":"work","from":"captain","to":"liaison","kind":"request","body":"do the task"}),
    );
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let jobs = f.json("liaison", "job.list", json!({}))["jobs"].clone();
    let wake = jobs[0]["id"].as_str().unwrap().to_owned();
    f.wait("liaison", &wake);
    let woke = allow(&f);
    assert!(carries(&woke), "{woke:?}");
    // Revoked, the next launch no longer carries it.
    f.json(
        "liaison",
        "captain_approval.revoke",
        json!({"id":answered["approval"]["id"]}),
    );
    f.ok(Some("liaison"), &["start", "claude"]);
    assert!(!carries(&allow(&f)));
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
    // Another ship asking whether it is still blocked is not fresh input.
    f.init("storefront", "claude");
    let nudge = f.json("storefront","mail.send",json!({"from":"storefront","to":"liaison","kind":"notice","about":asks[0]["id"],"body":"still blocked?"}));
    assert_eq!(nudge["urgency"], "low", "{nudge}");
    f.json("storefront", "mail.flush", json!({}));
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    assert_eq!(
        f.jobs("liaison"),
        before,
        "a still-blocked notice woke the ship"
    );
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
fn a_script_shaped_first_mate_denial_offers_the_captains_standing_authorization() {
    let f = Fleet::new();
    let block = |command: String| {
        f.json(
            "liaison",
            "permissions.blocked",
            json!({"task_id": "liaison-duty", "tool": "Bash", "input": {"command": command}}),
        )["request_id"]
            .clone()
    };
    let first = block(format!(
        "cp registry.env /srv/app/.env && echo API_TOKEN={SECRET} >> /srv/app/.env"
    ));
    let ask = f.show("liaison", &first);
    let options: Vec<&str> = ask["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["authorize", "without", "stop"]);
    assert!(!ask.to_string().contains(SECRET));
    let suggested = ask["payload"]["block"]["authorize"].as_str().unwrap();
    assert!(
        ask["body"].as_str().unwrap().contains(&format!(
            "privateer permissions authorize liaison '{suggested}'"
        )),
        "{ask}"
    );
    let reply = |ask: &Value, answer: Value, relayed: bool| {
        let mut payload = json!({"answer": answer});
        if relayed {
            payload["relayed_by"] = json!("liaison");
        }
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":ask,
            "body":"Authorize","payload":payload})
    };
    let relayed = f.fails(
        "liaison",
        "mail.send",
        reply(&first, json!({"option_id":"authorize"}), true),
    );
    assert!(
        relayed.contains("Authorize must come from the captain directly"),
        "{relayed}"
    );
    // Wording the policy cannot keep is refused before it is answered.
    let refused = f.fails(
        "liaison",
        "mail.send",
        reply(
            &first,
            json!({"option_id":"authorize","text":"Copying credentials.\nAnd more."}),
            false,
        ),
    );
    assert!(refused.contains("one line"), "{refused}");
    assert_eq!(f.show("liaison", &first)["state"], "open");
    // The captain's own wording is recorded instead of the suggestion.
    let wording = "Copying the registry credentials into /srv/app for deploys.";
    f.json(
        "liaison",
        "mail.send",
        reply(
            &first,
            json!({"option_id":"authorize","text":wording}),
            false,
        ),
    );
    let policy: Value = serde_json::from_str(&f.ok(None, &["permissions", "show"])).unwrap();
    assert_eq!(policy["authorized"]["liaison"], json!([wording]));
    assert_eq!(f.show("liaison", &first)["state"], "answered");
    // A plain tap records the suggestion.
    let second = block("make deploy | tee deploy.log".to_owned());
    assert_ne!(second, first);
    let suggested = f.show("liaison", &second)["payload"]["block"]["authorize"]
        .as_str()
        .unwrap()
        .to_owned();
    f.json(
        "liaison",
        "mail.send",
        reply(&second, json!({"option_id":"authorize"}), false),
    );
    let policy: Value = serde_json::from_str(&f.ok(None, &["permissions", "show"])).unwrap();
    assert_eq!(policy["authorized"]["liaison"], json!([wording, suggested]));
}

#[test]
fn authorizing_one_ship_records_nothing_for_another_with_the_same_suggestion() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    let block = |ship: &str| {
        f.json(
            ship,
            "permissions.blocked",
            json!({"task_id": format!("{ship}-duty"), "tool": "Bash",
                "input": {"command": "make deploy | tee deploy.log"}}),
        )["request_id"]
            .clone()
    };
    let ours = block("liaison");
    let theirs = block("storefront");
    let suggested = |ship: &str, id: &Value| {
        f.show(ship, id)["payload"]["block"]["authorize"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        suggested("liaison", &ours),
        suggested("storefront", &theirs)
    );
    // The same suggestion on another ship is not the same denial.
    let held = f.show("storefront", &theirs);
    assert!(held["payload"]["attached_to"].is_null(), "{held}");
    assert_ne!(
        held["payload"]["block"]["fleet_key"],
        f.show("liaison", &ours)["payload"]["block"]["fleet_key"]
    );
    // An ask attached anyway (crafted, or from before) is not answered by it.
    let database = f.root.join("storefront/.privateer/privateer.sqlite");
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE mail SET payload=json_set(payload,'$.attached_to',?2,'$.attached_ship','liaison')
             WHERE id=?1",
            rusqlite::params![theirs.as_str().unwrap(), ours.as_str().unwrap()],
        )
        .unwrap();
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":ours,
            "body":"Authorize","payload":{"answer":{"option_id":"authorize"}}}),
    );
    let policy: Value = serde_json::from_str(&f.ok(None, &["permissions", "show"])).unwrap();
    assert_eq!(
        policy["authorized"]["liaison"],
        json!([suggested("liaison", &ours)])
    );
    assert!(policy["authorized"]["storefront"].is_null(), "{policy}");
    let kept = f.show("storefront", &theirs);
    assert_eq!(kept["state"], "open", "{kept}");
    assert!(kept["payload"]["attached_to"].is_null(), "{kept}");
}

#[test]
fn a_crews_job_reported_blocked_never_offers_the_ships_authorization() {
    let f = Fleet::new();
    let mut options =
        f.json("liaison", "ship.show", json!({"name":"liaison"}))["first_mate"]["options"].clone();
    options["model"] = json!("script-model");
    assert_eq!(
        f.crew("liaison", "crew-one", json!({"options": options}))["status"],
        "failed"
    );
    let first = f.asks("liaison")[0].clone();
    assert_eq!(first["payload"]["block"]["scope"], "crew");
    // A free-text answer ends the ask without settling the denial.
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":first["id"],
            "body":"Look at it later"}),
    );
    // Its first mate reports the crew's job blocked: still the crew's ask.
    let raised = f.json(
        "liaison",
        "permissions.blocked",
        json!({"job_id": "crew-one"}),
    );
    assert_eq!(raised["duplicate"], false, "{raised}");
    let ask = f.show("liaison", &raised["request_id"]);
    assert_eq!(ask["payload"]["block"]["scope"], "crew", "{ask}");
    assert!(ask["payload"]["block"]["authorize"].is_null(), "{ask}");
    let options: Vec<&str> = ask["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["without", "stop"]);
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
    assert_eq!(told["payload"]["granted"], true, "{told}");
    assert!(
        told["body"].as_str().unwrap().contains("now allows"),
        "{told}"
    );
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
    // Its failed verdict names the ask and waits for the captain's answer.
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    let verdict = f.json("liaison", "mail.inbox", json!({"address":"liaison"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["payload"]["errand"] == id.as_str())
        .cloned()
        .expect("the errand's verdict");
    assert_eq!(verdict["urgency"], "low", "{verdict}");
    assert_eq!(verdict["payload"]["about"], asks[0]["id"], "{verdict}");
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

/// The ids of the asks `captain.waiting` shows as their own cards, and how
/// many items each carries.
fn waiting_cards(f: &Fleet) -> Vec<(Value, Value)> {
    f.json("liaison", "captain.waiting", json!({}))["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["mail"]["id"].clone(), w["attached"].clone()))
        .collect()
}

#[test]
fn a_denial_no_rule_narrows_never_joins_another_ships_ask() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    // Each ship's crew is denied a compound command: no rule narrows it.
    let mut options =
        f.json("liaison", "ship.show", json!({"name":"liaison"}))["first_mate"]["options"].clone();
    options["model"] = json!("script-model");
    for ship in ["liaison", "storefront"] {
        assert_eq!(
            f.crew(ship, &format!("{ship}-crew"), json!({"options": options}))["status"],
            "failed"
        );
    }
    let ours = f.asks("liaison")[0].clone();
    let theirs = f.asks("storefront")[0].clone();
    assert!(ours["payload"]["block"]["denials"][0]["rule"].is_null());
    for ask in [&ours, &theirs] {
        assert!(ask["payload"]["attached_to"].is_null(), "{ask}");
    }
    let cards = waiting_cards(&f);
    for ask in [&ours, &theirs] {
        assert!(cards.iter().any(|(id, _)| *id == ask["id"]), "{cards:?}");
    }
    // A tool the ship reports only as unknown is named by its tool alone, so
    // two ships' denials of it share a fleet key, but not an ask.
    let unknown = |ship: &str| {
        f.json(
            ship,
            "task.create",
            json!({"id": format!("{ship}-other"), "charter_id": ship, "owner": ship,
                "title": "Other work"}),
        );
        f.json(
            ship,
            "permissions.blocked",
            json!({"task_id": format!("{ship}-other"), "tool": "unknown", "input": {}}),
        )["request_id"]
            .clone()
    };
    let first = unknown("liaison");
    let second = unknown("storefront");
    let held = f.show("storefront", &second);
    assert_eq!(
        held["payload"]["block"]["fleet_key"],
        f.show("liaison", &first)["payload"]["block"]["fleet_key"]
    );
    assert!(held["payload"]["attached_to"].is_null(), "{held}");
    let cards = waiting_cards(&f);
    for id in [&first, &second] {
        assert!(cards.iter().any(|(card, _)| card == id), "{cards:?}");
    }
}

#[test]
fn the_same_derived_rule_on_two_ships_is_one_ask() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    let block = |ship: &str| {
        f.json(
            ship,
            "permissions.blocked",
            json!({"task_id": format!("{ship}-duty"), "tool": "Bash",
                "input": {"command": "kubectl apply -f web.yaml"}}),
        )["request_id"]
            .clone()
    };
    let ours = block("liaison");
    let theirs = block("storefront");
    let held = f.show("storefront", &theirs);
    assert_eq!(
        held["payload"]["block"]["denials"][0]["rule"],
        "Bash(kubectl apply *)"
    );
    assert_eq!(held["payload"]["attached_to"], ours, "{held}");
    let cards = waiting_cards(&f);
    assert!(!cards.iter().any(|(id, _)| *id == theirs), "{cards:?}");
    let card = cards.iter().find(|(id, _)| *id == ours).unwrap();
    assert_eq!(card.1, 1, "{cards:?}");
}

#[test]
fn a_policy_change_for_one_ship_leaves_another_ships_attached_ask_on_its_own() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    f.crew("liaison", "crew-one", json!({}));
    let ours = f.asks("liaison")[0]["id"].clone();
    let theirs = f.json(
        "storefront",
        "permissions.blocked",
        json!({"task_id": "storefront-duty", "tool": "Bash",
            "input": {"command": "make deploy | tee deploy.log"}}),
    )["request_id"]
        .clone();
    // Attached anyway, as an older runtime attached it.
    rusqlite::Connection::open(f.root.join("storefront/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET payload=json_set(payload,'$.attached_to',?2,'$.attached_ship','liaison','$.about',?2)
             WHERE id=?1",
            rusqlite::params![theirs.as_str().unwrap(), ours.as_str().unwrap()],
        )
        .unwrap();
    assert!(!waiting_cards(&f).iter().any(|(id, _)| *id == theirs));
    // The captain allows liaison's rule; it covers nothing storefront asked.
    f.ok(None, &["permissions", "allow", "Bash(apify run *)"]);
    assert_eq!(f.show("liaison", &ours)["end_reason"], "policy changed");
    let kept = f.show("storefront", &theirs);
    assert_eq!(kept["state"], "open", "{kept}");
    assert!(kept["payload"]["attached_to"].is_null(), "{kept}");
    assert!(kept["payload"]["about"].is_null(), "{kept}");
    assert!(waiting_cards(&f).iter().any(|(id, _)| *id == theirs));
}

#[test]
fn an_ask_attached_to_a_primary_a_policy_change_ended_elsewhere_is_detached_not_withdrawn() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    f.crew("liaison", "crew-one", json!({}));
    let ours = f.asks("liaison")[0]["id"].clone();
    let theirs = f.json(
        "storefront",
        "permissions.blocked",
        json!({"task_id": "storefront-duty", "tool": "Bash",
            "input": {"command": "make deploy | tee deploy.log"}}),
    )["request_id"]
        .clone();
    rusqlite::Connection::open(f.root.join("storefront/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET payload=json_set(payload,'$.attached_to',?2,'$.attached_ship','liaison')
             WHERE id=?1",
            rusqlite::params![theirs.as_str().unwrap(), ours.as_str().unwrap()],
        )
        .unwrap();
    // Liaison's ask ended for a policy change, and storefront's home sweeps
    // before liaison's home released what was attached to it.
    rusqlite::Connection::open(f.root.join("liaison/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET state='withdrawn', end_reason='policy changed' WHERE id=?1",
            [ours.as_str().unwrap()],
        )
        .unwrap();
    f.ok(Some("storefront"), &["serve", "--json", "{\"once\":true}"]);
    let kept = f.show("storefront", &theirs);
    assert_eq!(kept["state"], "open", "{kept}");
    assert!(kept["payload"]["attached_to"].is_null(), "{kept}");
}

#[test]
fn an_ask_attached_to_a_primary_its_session_got_past_while_its_home_was_unreachable_is_detached() {
    let f = Fleet::new();
    f.init("storefront", "claude");
    f.crew("liaison", "crew-one", json!({}));
    let ours = f.asks("liaison")[0]["id"].clone();
    let theirs = f.json(
        "storefront",
        "permissions.blocked",
        json!({"task_id": "storefront-duty", "tool": "Bash",
            "input": {"command": "make deploy | tee deploy.log"}}),
    )["request_id"]
        .clone();
    attach(&f, "storefront", &theirs, &ours, "liaison");
    // Liaison's pane got past its own block and withdrew its ask while
    // storefront's home could not be opened, so nothing there was detached.
    rusqlite::Connection::open(f.root.join("liaison/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "UPDATE mail SET state='withdrawn', end_reason=?2 WHERE id=?1",
            rusqlite::params![
                ours.as_str().unwrap(),
                "the session got past this block: the same Bash call succeeded on retry at 2026-10-20 00:45 UTC, so this decision is no longer needed"
            ],
        )
        .unwrap();
    // Storefront's sweep finds it ended: its own ask may still be blocked.
    f.ok(Some("storefront"), &["serve", "--json", "{\"once\":true}"]);
    let kept = f.show("storefront", &theirs);
    assert_eq!(kept["state"], "open", "{kept}");
    assert!(kept["payload"]["attached_to"].is_null(), "{kept}");
    assert!(kept["payload"]["about"].is_null(), "{kept}");
    assert!(waiting_cards(&f).iter().any(|(id, _)| *id == theirs));
}

#[test]
fn an_ask_an_unrelated_policy_change_ends_says_nothing_was_granted() {
    let f = Fleet::new();
    let mut options =
        f.json("liaison", "ship.show", json!({"name":"liaison"}))["first_mate"]["options"].clone();
    options["model"] = json!("script-model");
    f.crew("liaison", "crew-one", json!({"options": options}));
    let ask = f.asks("liaison")[0].clone();
    assert!(ask["payload"]["block"]["denials"][0]["rule"].is_null());
    // No rule names the denial, so any change ends the ask for one retry.
    f.ok(None, &["permissions", "allow", "Bash(other-tool *)"]);
    let ended = f.show("liaison", &ask["id"]);
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    let reason = ended["end_reason"].as_str().unwrap();
    assert_ne!(reason, "policy changed");
    assert!(
        reason.contains("nothing this ask named was granted"),
        "{reason}"
    );
    let told = f.json("liaison", "mail.inbox", json!({"address":"liaison"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["payload"]["block_cleared"] == true)
        .cloned()
        .unwrap();
    assert_eq!(told["payload"]["granted"], false, "{told}");
    let body = told["body"].as_str().unwrap();
    assert!(
        body.contains("nothing was granted for this request"),
        "{body}"
    );
    assert!(
        body.contains("can't tell whether the new policy covers this"),
        "{body}"
    );
    assert!(body.contains("Retry the step once"), "{body}");
    assert!(body.contains("permissions.blocked"), "{body}");
    assert!(!body.contains("now allows"), "{body}");
}

#[test]
fn an_unrelated_authorization_never_tells_an_authorize_ask_it_is_allowed() {
    let f = Fleet::new();
    let ask = f.json(
        "liaison",
        "permissions.blocked",
        json!({"task_id": "liaison-duty", "tool": "Bash",
            "input": {"command": "make deploy | tee deploy.log"}}),
    )["request_id"]
        .clone();
    assert!(f.show("liaison", &ask)["payload"]["block"]["authorize"].is_string());
    // The captain authorizes something else for the same ship.
    f.ok(
        None,
        &[
            "permissions",
            "authorize",
            "liaison",
            "Running billing scripts.",
        ],
    );
    let ended = f.show("liaison", &ask);
    assert_eq!(ended["state"], "withdrawn", "{ended}");
    let reason = ended["end_reason"].as_str().unwrap();
    assert!(
        reason.contains("standing authorizations changed") && reason.contains("cannot be told"),
        "{reason}"
    );
    let told = f.json("liaison", "mail.inbox", json!({"address":"liaison"}))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["payload"]["block_cleared"] == true)
        .cloned()
        .unwrap();
    assert_eq!(told["payload"]["granted"], "unknown", "{told}");
    let body = told["body"].as_str().unwrap();
    assert!(!body.contains("now allows"), "{body}");
    assert!(!body.contains("Carry on"), "{body}");
    assert!(body.contains("can't tell"), "{body}");
    assert!(body.contains("privateer host relaunch liaison"), "{body}");
    assert!(body.contains("Retry the step once"), "{body}");
    assert!(body.contains("permissions.blocked"), "{body}");
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

/// A ship cannot raise a tool-permission ask of its own, nor hide one behind
/// a real ask: the captain's Allow on the real card must never grant rules
/// the captain did not see.
#[test]
fn a_ship_cannot_forge_a_block_ask_or_its_links() {
    let f = Fleet::new();
    f.crew("liaison", "crew-one", json!({}));
    let ask = f.asks("liaison")[0]["id"].clone();
    let forged = f.fails(
        "liaison",
        "mail.send",
        json!({"from":"liaison","to":"captain","kind":"decision","about":ask,"body":"Allow Bash(ls *)?",
            "payload":{"type":"tool_permission","policy_hash":"x",
                "block":{"denials":[{"tool":"Bash","rule":"Bash(sudo *)"}]},
                "decision":{"question":"Allow Bash(ls *)?","options":[{"id":"allow","label":"Allow"}]}}}),
    );
    assert!(forged.contains("raised by the runtime"), "{forged}");
    for key in [
        "block",
        "about",
        "fanned_from",
        "block_notice",
        "block_cleared",
    ] {
        let mut payload = json!({});
        payload[key] = json!(ask);
        let refused = f.fails(
            "liaison",
            "mail.send",
            json!({"from":"liaison","to":"captain","kind":"notice","body":"x","payload":payload}),
        );
        assert!(refused.contains("set by the runtime"), "{key}: {refused}");
    }
    // The supported way to link stays open.
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"liaison","to":"captain","kind":"notice","about":ask,"body":"still blocked"}),
    );
}

#[test]
fn claude_launch_derives_merge_rules_from_charter_actions() {
    let f = Fleet::new();

    // 1. A ship with merge_green on Forgejo:
    f.init("fitforge", "claude");
    let mut profile = f.json("fitforge", "ship.show", json!({"name": "fitforge"}));
    profile["first_mate"]["batch_secs"] = json!(0);
    profile["first_mate"]["retry_backoff_secs"] = json!(0);
    f.json("fitforge", "ship.put", profile);
    f.json(
        "fitforge",
        "permissions.set",
        json!({
            "forges": {
                "forgejo": {
                    "publish_pr": "fakeforge prs create *",
                    "merge_green": "fakeforge prs merge *"
                }
            }
        }),
    );
    f.json(
        "fitforge",
        "charter.put",
        json!({
            "id": "fitforge",
            "objective": "Build and merge fitforge apps",
            "repo": f.root.join("fitforge").to_str().unwrap(),
            "actions": ["inspect", "spawn", "implement", "test", "publish_pr", "merge_green"],
            "forge": "forgejo",
        }),
    );

    f.ok(Some("fitforge"), &["start", "claude"]);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        args.contains(&"Bash(fakeforge prs merge *)".into()),
        "merge_green ship Claude launch argv carries the merge rule: {args:?}"
    );
    assert!(
        args.contains(&"Bash(fakeforge prs create *)".into()),
        "publish_pr carries the publish rule: {args:?}"
    );
    let settings_pos = args.iter().position(|a| a == "--settings").unwrap();
    let settings: Value = serde_json::from_str(&args[settings_pos + 1]).unwrap();
    let auto_allow = settings["autoMode"]["allow"].as_array().unwrap();
    assert_eq!(auto_allow[0], "$defaults");
    assert!(
        auto_allow
            .iter()
            .any(|v| v.as_str().unwrap().contains("merge_green")),
        "settings carries merge_green context"
    );
    let auto_env = settings["autoMode"]["environment"].as_array().unwrap();
    assert_eq!(auto_env[0], "$defaults");

    // 2. A ship without merge_green on Forgejo:
    f.init("browser-mac", "claude");
    let mut profile = f.json("browser-mac", "ship.show", json!({"name": "browser-mac"}));
    profile["first_mate"]["batch_secs"] = json!(0);
    profile["first_mate"]["retry_backoff_secs"] = json!(0);
    f.json("browser-mac", "ship.put", profile);
    f.json(
        "browser-mac",
        "charter.put",
        json!({
            "id": "browser-mac",
            "objective": "Browser automation",
            "repo": f.root.join("browser-mac").to_str().unwrap(),
            "actions": ["inspect", "spawn", "implement", "test", "publish_pr"],
            "forge": "forgejo",
        }),
    );

    f.ok(Some("browser-mac"), &["start", "claude"]);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        !args.contains(&"Bash(fakeforge prs merge *)".into()),
        "ship without merge_green must NOT carry the merge rule: {args:?}"
    );
    assert!(
        args.contains(&"Bash(fakeforge prs create *)".into()),
        "publish_pr carries publish rule: {args:?}"
    );
    let settings_pos = args.iter().position(|a| a == "--settings").unwrap();
    let settings: Value = serde_json::from_str(&args[settings_pos + 1]).unwrap();
    let auto_allow = settings["autoMode"]["allow"].as_array().unwrap();
    assert_eq!(auto_allow[0], "$defaults");
    assert!(
        !auto_allow
            .iter()
            .any(|v| v.as_str().unwrap().contains("merge_green")),
        "settings must NOT carry merge_green context when not granted"
    );

    // 3. A crew launch on the merge_green ship never carries the merge rule:
    f.crew("fitforge", "crew-1", json!({}));
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        !args.contains(&"Bash(fakeforge prs merge *)".into()),
        "crew launch never carries the merge rule: {args:?}"
    );
    assert!(
        !args.contains(&"Bash(fakeforge prs create *)".into()),
        "crew launch never carries the publish rule: {args:?}"
    );
    if let Some(pos) = args.iter().position(|a| a == "--settings") {
        let settings: Value = serde_json::from_str(&args[pos + 1]).unwrap();
        if let Some(auto_allow) = settings["autoMode"]["allow"].as_array() {
            assert!(
                !auto_allow
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("merge_green")),
                "crew launch settings never carry merge_green"
            );
        }
    }

    // 4. GitHub forge mapping:
    f.init("hub-ship", "claude");
    let mut profile = f.json("hub-ship", "ship.show", json!({"name": "hub-ship"}));
    profile["first_mate"]["batch_secs"] = json!(0);
    profile["first_mate"]["retry_backoff_secs"] = json!(0);
    f.json("hub-ship", "ship.put", profile);
    f.json(
        "hub-ship",
        "charter.put",
        json!({
            "id": "hub-ship",
            "objective": "GitHub tooling",
            "repo": f.root.join("hub-ship").to_str().unwrap(),
            "actions": ["inspect", "spawn", "implement", "test", "publish_pr", "merge_green"],
            "forge": "github",
        }),
    );
    f.ok(Some("hub-ship"), &["start", "claude"]);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        args.contains(&"Bash(gh pr merge *)".into()),
        "merge_green on GitHub carries gh pr merge: {args:?}"
    );
    assert!(
        args.contains(&"Bash(gh pr create *)".into()),
        "publish_pr on GitHub carries gh pr create: {args:?}"
    );

    // GitHub ship without merge_green
    f.json(
        "hub-ship",
        "charter.put",
        json!({
            "id": "hub-ship",
            "objective": "GitHub tooling",
            "repo": f.root.join("hub-ship").to_str().unwrap(),
            "actions": ["inspect", "spawn", "implement", "test", "publish_pr"],
            "forge": "github",
        }),
    );
    f.ok(Some("hub-ship"), &["start", "claude"]);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        !args.contains(&"Bash(gh pr merge *)".into()),
        "without merge_green, GitHub launch does NOT carry gh pr merge: {args:?}"
    );
    assert!(
        args.contains(&"Bash(gh pr create *)".into()),
        "publish_pr still carries gh pr create: {args:?}"
    );
}

#[test]
fn errand_launch_gets_no_forge_rules() {
    let f = Fleet::new();
    f.init("fitforge", "claude");
    let mut profile = f.json("fitforge", "ship.show", json!({"name": "fitforge"}));
    profile["first_mate"]["batch_secs"] = json!(0);
    profile["first_mate"]["retry_backoff_secs"] = json!(0);
    f.json("fitforge", "ship.put", profile);
    f.json(
        "fitforge",
        "permissions.set",
        json!({
            "forges": {
                "forgejo": {
                    "publish_pr": "fakeforge prs create *",
                    "merge_green": "fakeforge prs merge *"
                }
            }
        }),
    );
    f.json(
        "fitforge",
        "charter.put",
        json!({
            "id": "fitforge",
            "objective": "Build and merge fitforge apps",
            "repo": f.root.join("fitforge").to_str().unwrap(),
            "actions": ["inspect", "spawn", "implement", "test", "publish_pr", "merge_green"],
            "forge": "forgejo",
        }),
    );

    let errand = f.json(
        "fitforge",
        "errand.start",
        json!({"focus": "task:fitforge-duty", "brief": "Review the PR"}),
    );
    let id = errand["errand"].as_str().unwrap().to_owned();
    let _ = f.wait("fitforge", &id);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    assert!(
        !args.contains(&"Bash(fakeforge prs merge *)".into()),
        "errand launch must NOT carry merge rule: {args:?}"
    );
    assert!(
        !args.contains(&"Bash(fakeforge prs create *)".into()),
        "errand launch must NOT carry publish rule: {args:?}"
    );
    assert!(
        !args.contains(&"Bash(gh pr merge *)".into()),
        "errand launch must NOT carry gh merge rule: {args:?}"
    );
    assert!(
        !args.contains(&"Bash(gh pr create *)".into()),
        "errand launch must NOT carry gh publish rule: {args:?}"
    );
    if let Some(pos) = args.iter().position(|a| a == "--settings") {
        let settings: Value = serde_json::from_str(&args[pos + 1]).unwrap();
        if let Some(auto_allow) = settings["autoMode"]["allow"].as_array() {
            assert!(
                !auto_allow
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("merge_green")),
                "errand launch settings must NOT carry merge_green: {settings:?}"
            );
        }
    }
}

/// Liaison's first mate, its crews and its errands run on a model whose
/// every turn auto mode's classifier refuses.
fn classified() -> Fleet {
    let f = Fleet::new();
    let mut profile = f.json("liaison", "ship.show", json!({"name":"liaison"}));
    profile["first_mate"]["options"]["model"] = json!("classifier-model");
    f.json("liaison", "ship.put", profile);
    f
}

/// A wake whose Bash `command` the classifier refuses, and the ask it raises.
fn refused_wake(f: &Fleet, command: &str) -> Value {
    fs::write(f.root.join("denied-command"), command).unwrap();
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"request","body":"Release it"}),
    );
    f.ok(Some("liaison"), &["serve", "--json", "{\"once\":true}"]);
    for job in f.json("liaison", "job.list", json!({}))["jobs"]
        .as_array()
        .unwrap()
    {
        f.wait("liaison", job["id"].as_str().unwrap());
    }
    let ask = f
        .asks("liaison")
        .into_iter()
        .rfind(|a| f.show("liaison", &a["id"])["state"] == "open")
        .expect("the refused wake's ask");
    assert_eq!(ask["payload"]["block"]["scope"], "wake", "{ask}");
    ask
}

/// An ask's options, as (id, recommended).
fn offered(ask: &Value) -> Vec<(String, bool)> {
    ask["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["id"].as_str().unwrap().to_owned(),
                o["recommended"] == true,
            )
        })
        .collect()
}

fn option<'a>(ask: &'a Value, id: &str) -> &'a Value {
    ask["payload"]["decision"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == id)
        .unwrap_or_else(|| panic!("no option {id}: {ask}"))
}

/// The captain's answer to `ask`: `option`, with `text` beside it, or as a
/// door sends a plain tap, its label.
fn tap(ask: &Value, option: &str, text: &str) -> Value {
    let label = self::option(ask, option)["label"].clone();
    let mut answer = json!({"option_id": option});
    if !text.is_empty() {
        answer["text"] = json!(text);
    }
    json!({"from":"captain","to":"liaison","kind":"reply","correlation":ask["id"],
        "body": if text.is_empty() { label } else { json!(text) },
        "payload":{"answer":answer}})
}

/// The captain's free-text answer, which ends an ask and approves nothing.
fn set_aside(f: &Fleet, ask: &Value) {
    f.json(
        "liaison",
        "mail.send",
        json!({"from":"captain","to":"liaison","kind":"reply","correlation":ask["id"],
            "body":"Look at it later"}),
    );
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A captain approval of liaison's for `make release`, created and expiring
/// at the times given.
fn plant_approval(f: &Fleet, id: &str, created_ms: i64, expires_ms: i64) {
    rusqlite::Connection::open(f.root.join("liaison/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,command_prefix,
               created_ms,expires_ms,source)
             VALUES(?1,'liaison','liaison-duty','q-'||?1,'r-'||?1,'Run make release','make release',?2,?3,'reply')",
            rusqlite::params![id, created_ms, expires_ms],
        )
        .unwrap();
}

fn approvals(f: &Fleet) -> Vec<Value> {
    f.json(
        "liaison",
        "captain_approval.list",
        json!({"ship":"liaison"}),
    )["approvals"]
        .as_array()
        .unwrap()
        .clone()
}

/// What an attended first-mate launch tells auto mode the captain allows.
fn launch_allows(f: &Fleet) -> Vec<String> {
    f.ok(Some("liaison"), &["start", "claude"]);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root.join("argv.json")).unwrap()).unwrap();
    args.iter()
        .position(|a| a == "--settings")
        .map(|i| serde_json::from_str::<Value>(&args[i + 1]).unwrap()["autoMode"]["allow"].clone())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_wakes_classifier_refusal_offers_the_captain_this_exact_step_for_24_hours() {
    let f = classified();
    let ask = refused_wake(&f, "make release --dry-run");
    let block = &ask["payload"]["block"];
    assert_eq!(block["denials"][0]["classifier"], true, "{ask}");
    assert_eq!(
        block["denials"][0]["head"],
        json!(["make", "release", "--dry-run"])
    );
    assert_eq!(
        block["approve"],
        json!({"action":"Run `make release --dry-run`","command_prefix":"make release --dry-run"})
    );
    assert!(block.get("approvals").is_none(), "{ask}");
    assert_eq!(
        offered(&ask),
        [
            ("authorize".to_owned(), false),
            ("approve".to_owned(), true),
            ("without".to_owned(), false),
            ("stop".to_owned(), false)
        ]
    );
    let approve = option(&ask, "approve");
    assert_eq!(
        approve["label"],
        "Approve for 24 h: Run `make release --dry-run`"
    );
    let what = approve["what_happens"].as_str().unwrap();
    assert!(
        what.contains("\"Run `make release --dry-run`\"") && what.contains("24 h from your tap"),
        "{what}"
    );
    // Relayed, it is refused before it answers anything.
    let mut relayed = tap(&ask, "approve", "");
    relayed["payload"]["relayed_by"] = json!("liaison");
    let refused = f.fails("liaison", "mail.send", relayed);
    assert!(
        refused.contains("must come from the captain directly"),
        "{refused}"
    );
    // A ship cannot answer as the captain at all.
    let agent = f
        .command(
            Some("liaison"),
            &["mail.send", "--json", &tap(&ask, "approve", "").to_string()],
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert_eq!(f.show("liaison", &ask["id"])["state"], "open");
    assert!(approvals(&f).is_empty());
    // The captain's own tap records one approval bound to the ask and reply.
    let reply = f.json("liaison", "mail.send", tap(&ask, "approve", ""));
    assert_eq!(reply["approval"]["recorded"], true, "{reply}");
    assert_eq!(reply["approval"]["applies"], "wake", "{reply}");
    assert_eq!(f.show("liaison", &ask["id"])["state"], "answered");
    let rows = approvals(&f);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row["source"], "block");
    assert_eq!(row["request_id"], ask["id"]);
    assert_eq!(row["reply_id"], reply["id"]);
    assert_eq!(row["action"], "Run `make release --dry-run`");
    assert_eq!(row["command_prefix"], "make release --dry-run");
    assert_eq!(
        row["expires_ms"].as_i64().unwrap() - row["created_ms"].as_i64().unwrap(),
        86_400_000
    );
    assert_eq!(row["live"], true);
    let told = launch_allows(&f);
    assert!(
        told.iter().any(|t| t.starts_with(&format!(
            "The captain approved this in reply {} to request {}, for ship liaison only",
            reply["id"].as_str().unwrap(),
            ask["id"].as_str().unwrap()
        )) && t.ends_with(
            ": Run `make release --dry-run` Only commands that begin exactly with: make release --dry-run"
        )),
        "{told:?}"
    );
    // Nothing in the fleet policy changed for it.
    let policy: Value = serde_json::from_str(&f.ok(None, &["permissions", "show"])).unwrap();
    assert!(policy["authorized"]["liaison"].is_null(), "{policy}");
}

#[test]
fn a_hedged_tap_on_approve_records_nothing() {
    let f = classified();
    let ask = refused_wake(&f, "make release --dry-run");
    let reply = f.json(
        "liaison",
        "mail.send",
        tap(&ask, "approve", "yes but wait for CI"),
    );
    assert_eq!(reply["approval"]["recorded"], false, "{reply}");
    assert!(
        reply["approval"]["why"]
            .as_str()
            .unwrap()
            .contains("nothing was approved"),
        "{reply}"
    );
    assert!(approvals(&f).is_empty());
}

#[test]
fn a_crews_or_an_errands_classifier_refusal_offers_no_captain_approval() {
    let f = classified();
    fs::write(f.root.join("denied-command"), "make release --dry-run").unwrap();
    let now = now_ms();
    plant_approval(&f, "approval-live", now - 60_000, now + 3_600_000);
    let errand = f.json(
        "liaison",
        "errand.start",
        json!({"focus":"task:liaison-duty","brief":"Release it"}),
    );
    f.wait("liaison", errand["errand"].as_str().unwrap());
    let asks = f.asks("liaison");
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0]["payload"]["block"]["scope"], "errand");
    assert_eq!(
        asks[0]["payload"]["block"]["denials"][0]["classifier"],
        true
    );
    let ids: Vec<String> = offered(&asks[0]).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["without", "stop"], "{}", asks[0]);
    set_aside(&f, &asks[0]);
    assert_eq!(f.crew("liaison", "crew-one", json!({}))["status"], "failed");
    let ask = f
        .asks("liaison")
        .into_iter()
        .find(|a| a["id"] != asks[0]["id"])
        .expect("the crew's ask");
    assert_eq!(ask["payload"]["block"]["scope"], "crew");
    let ids: Vec<String> = offered(&ask).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["without", "stop"], "{ask}");
    for absent in ["approve", "approvals"] {
        assert!(ask["payload"]["block"].get(absent).is_none(), "{ask}");
    }
}

#[test]
fn a_recent_approval_the_session_lacked_is_offered_and_its_tap_records_nothing() {
    let f = classified();
    let now = now_ms();
    // Given while the wake ran, so the wake never carried it.
    plant_approval(&f, "approval-given", now + 600_000, now + 3_600_000);
    // Given more than 24 h ago, and lapsed: never renewed in one tap.
    plant_approval(&f, "approval-old", now - 25 * 3_600_000, now - 3_600_000);
    let ask = refused_wake(&f, "make release --dry-run");
    assert_eq!(
        ask["payload"]["block"]["approvals"],
        json!(["approval-given"])
    );
    assert_eq!(
        offered(&ask),
        [
            ("authorize".to_owned(), false),
            ("as_approved_1".to_owned(), true),
            ("approve".to_owned(), false),
            ("without".to_owned(), false),
            ("stop".to_owned(), false)
        ]
    );
    let reuse = option(&ask, "as_approved_1");
    assert_eq!(reuse["label"], "As approved: Run make release");
    let what = reuse["what_happens"].as_str().unwrap();
    assert!(
        what.contains("\"Run make release\"")
            && what.contains(" UTC")
            && what.contains("nothing to relaunch"),
        "{what}"
    );
    let reply = f.json("liaison", "mail.send", tap(&ask, "as_approved_1", ""));
    assert_eq!(reply["approval"]["recorded"], false, "{reply}");
    assert_eq!(reply["approval"]["covered_by"], "approval-given", "{reply}");
    assert_eq!(reply["approval"]["applies"], "wake", "{reply}");
    assert_eq!(f.show("liaison", &ask["id"])["state"], "answered");
    assert_eq!(approvals(&f).len(), 2);
}

#[test]
fn an_approval_that_expired_within_24_hours_is_renewed_by_its_tap() {
    let f = classified();
    let now = now_ms();
    plant_approval(&f, "approval-lapsed", now - 2 * 3_600_000, now - 3_600_000);
    let ask = refused_wake(&f, "make release --dry-run");
    assert_eq!(
        ask["payload"]["block"]["approvals"],
        json!(["approval-lapsed"])
    );
    let what = option(&ask, "as_approved_1")["what_happens"]
        .as_str()
        .unwrap();
    assert!(
        what.starts_with("Renews captain approval approval-lapsed"),
        "{what}"
    );
    let reply = f.json("liaison", "mail.send", tap(&ask, "as_approved_1", ""));
    assert_eq!(reply["approval"]["recorded"], true, "{reply}");
    let renewed = approvals(&f)
        .into_iter()
        .find(|r| r["id"] == reply["approval"]["id"])
        .unwrap();
    assert_eq!(renewed["action"], "Run make release");
    assert_eq!(renewed["command_prefix"], "make release");
    assert_eq!(renewed["request_id"], ask["id"]);
    assert_eq!(renewed["reply_id"], reply["id"]);
    assert_eq!(renewed["source"], "block");
    assert_eq!(renewed["live"], true);
    assert_eq!(
        renewed["expires_ms"].as_i64().unwrap() - renewed["created_ms"].as_i64().unwrap(),
        3_600_000
    );
    let audit = f.json("liaison", "audit.list", json!({"limit":100}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "permissions.approval_recorded")
        .cloned()
        .unwrap();
    assert_eq!(audit["payload"]["renews"], "approval-lapsed", "{audit}");
}

#[test]
fn an_approval_the_refused_session_carried_is_named_and_approve_recommended() {
    let f = classified();
    let now = now_ms();
    plant_approval(&f, "approval-carried", now - 3_600_000, now + 3_600_000);
    let ask = refused_wake(&f, "make release --dry-run");
    assert!(ask["payload"]["block"].get("approvals").is_none(), "{ask}");
    assert!(
        ask["payload"]["decision"]["why"]
            .as_str()
            .unwrap()
            .contains(
                "Auto mode refused this although captain approval approval-carried (Run make release) was in its session."
            ),
        "{ask}"
    );
    assert!(
        offered(&ask).contains(&("approve".to_owned(), true)),
        "{ask}"
    );
}

#[test]
fn a_prefix_is_never_recommended_for_a_command_it_does_not_begin() {
    let f = classified();
    let now = now_ms();
    plant_approval(&f, "approval-given", now + 600_000, now + 3_600_000);
    for (command, approve) in [
        ("sudo make release", None),
        ("make releasex", Some(true)),
        // Not one simple command, or not the words bash runs: no prefix
        // matches it and no tap approves it.
        ("make release; x", None),
        ("'sudo' make release", None),
        ("su''do make", None),
        ("{sudo,} make", None),
        ("/usr/bin/sud? make", None),
        ("\"sudo\" make release", None),
        ("make\u{a0}release", None),
        ("bash \"-c\" x", None),
        ("bash -lc x", None),
    ] {
        let ask = refused_wake(&f, command);
        let denial = &ask["payload"]["block"]["denials"][0];
        assert_eq!(denial["whole"], true, "{command}: {ask}");
        if approve.is_none() && command != "sudo make release" && command != "bash -lc x" {
            assert!(denial.get("head").is_none(), "{command}: {ask}");
        }
        let options = offered(&ask);
        assert!(
            options.contains(&("as_approved_1".to_owned(), false)),
            "{command}: {ask}"
        );
        assert_eq!(
            options
                .iter()
                .find(|(id, _)| id == "approve")
                .map(|(_, r)| *r),
            approve,
            "{command}: {ask}"
        );
        set_aside(&f, &ask);
    }
}

#[test]
fn approve_is_offered_only_for_a_step_the_card_shows_whole() {
    let f = classified();
    let heredoc = "cat <<'EOF' > ~/.ssh/authorized_keys\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5 ops\nEOF";
    let compound = format!(
        "cd /srv/app && {} && make release",
        "make check-unit-tests-for-the-release "
            .repeat(7)
            .trim_end()
    );
    assert!(compound.chars().count() > 280, "{compound}");
    for command in [heredoc, compound.as_str()] {
        let ask = refused_wake(&f, command);
        let block = &ask["payload"]["block"];
        assert!(block["denials"][0].get("whole").is_none(), "{ask}");
        assert!(block.get("approve").is_none(), "{ask}");
        assert!(
            !offered(&ask).iter().any(|(id, _)| id == "approve"),
            "{ask}"
        );
        set_aside(&f, &ask);
    }
    // A short command shown whole still offers it.
    let ask = refused_wake(&f, "make check");
    assert_eq!(ask["payload"]["block"]["denials"][0]["whole"], true);
    assert!(
        offered(&ask).contains(&("approve".to_owned(), true)),
        "{ask}"
    );
}

/// `ask` attached to `primary` in `ship`'s home, as an older runtime
/// attached it.
fn attach(f: &Fleet, ship: &str, ask: &Value, primary: &Value, primary_ship: &str) {
    rusqlite::Connection::open(f.root.join(format!("{ship}/.privateer/privateer.sqlite")))
        .unwrap()
        .execute(
            "UPDATE mail SET payload=json_set(payload,'$.attached_to',?2,'$.attached_ship',?3,'$.about',?2)
             WHERE id=?1",
            rusqlite::params![
                ask.as_str().unwrap(),
                primary.as_str().unwrap(),
                primary_ship
            ],
        )
        .unwrap();
}

#[test]
fn an_approve_fanned_to_the_same_ships_attached_ask_records_nothing_more() {
    let f = classified();
    let ask = refused_wake(&f, "make release --dry-run");
    // The same card of liaison's, on other work, attached to the first: its
    // options match, so the captain's answer is passed on to it.
    f.json(
        "liaison",
        "task.create",
        json!({"id": "liaison-other", "charter_id": "liaison", "owner": "liaison",
            "title": "Other work"}),
    );
    rusqlite::Connection::open(f.root.join("liaison/.privateer/privateer.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO mail(id,sender,recipient,kind,correlation,task_id,request_kind,body,payload,
               urgency,deadline_ms,created_ms,state,delivery)
             SELECT 'ask-copy',sender,recipient,kind,correlation,'liaison-other',request_kind,body,
               json_set(payload,'$.attached_to',id,'$.attached_ship',sender,'$.about',id,
                 '$.block.task_id','liaison-other'),
               urgency,deadline_ms,created_ms,state,delivery FROM mail WHERE id=?1",
            [ask["id"].as_str().unwrap()],
        )
        .unwrap();
    let other = json!("ask-copy");
    assert_eq!(f.show("liaison", &other)["state"], "open");
    let reply = f.json("liaison", "mail.send", tap(&ask, "approve", ""));
    assert_eq!(reply["approval"]["recorded"], true, "{reply}");
    let copy = f.show("liaison", &other);
    assert_eq!(copy["state"], "answered", "{copy}");
    let fanned = f.show("liaison", &copy["answered_by"]);
    assert!(fanned["payload"]["fanned_from"].is_string(), "{fanned}");
    assert_eq!(
        fanned["payload"]["answer"]["option_id"], "approve",
        "{fanned}"
    );
    assert_eq!(fanned["payload"]["approval"]["recorded"], false, "{fanned}");
    assert!(
        fanned["payload"]["approval"]["why"]
            .as_str()
            .unwrap()
            .contains("only the captain's own reply approves"),
        "{fanned}"
    );
    assert_eq!(approvals(&f).len(), 1);
}

#[test]
fn an_approve_detaches_another_ships_attached_ask_and_approves_nothing_for_it() {
    let f = classified();
    f.init("storefront", "claude");
    let ask = refused_wake(&f, "make release --dry-run");
    let theirs = f.json(
        "storefront",
        "permissions.blocked",
        json!({"task_id": "storefront-duty", "tool": "Bash",
            "input": {"command": "make deploy | tee deploy.log"}}),
    )["request_id"]
        .clone();
    attach(&f, "storefront", &theirs, &ask["id"], "liaison");
    assert!(!waiting_cards(&f).iter().any(|(id, _)| *id == theirs));
    let reply = f.json("liaison", "mail.send", tap(&ask, "approve", ""));
    assert_eq!(reply["approval"]["recorded"], true, "{reply}");
    let kept = f.show("storefront", &theirs);
    assert_eq!(kept["state"], "open", "{kept}");
    assert!(kept["payload"]["attached_to"].is_null(), "{kept}");
    assert!(kept["payload"]["about"].is_null(), "{kept}");
    assert!(waiting_cards(&f).iter().any(|(id, _)| *id == theirs));
    let theirs_approved = f.json(
        "storefront",
        "captain_approval.list",
        json!({"ship":"storefront"}),
    );
    assert_eq!(theirs_approved["approvals"], json!([]), "{theirs_approved}");
}
