use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

const ADAPTER: &str = r#"import json, sys
r = json.loads(sys.stdin.readline())
c, o = r["context"], r["options"]
argv = [sys.executable, o["mate"], json.dumps(c["control"]["cli_argv"]), c["owner"], c["brief"], o["mode"], o["log"]]
print(json.dumps({"id": r["id"], "version": 1, "ok": True, "result": {"argv": argv, "cwd": c["repo"], "env_names": []}}))
"#;

// A scripted first mate: reads its mailbox through the real CLI, answers open
// requests, acknowledges through the sequence its wake names, and reports
// usage the way Codex does. No model is involved.
const MATE: &str = r#"import json, pathlib, re, subprocess, sys, time
cli, ship, prompt = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
mode_file, log = pathlib.Path(sys.argv[4]), pathlib.Path(sys.argv[5])
with log.open("a") as f:
    f.write(json.dumps(prompt) + "\n")
mode = mode_file.read_text().strip() if mode_file.exists() else "normal"
if mode == "fail":
    sys.exit(1)
if mode.startswith("sleep"):
    time.sleep(float(mode.split()[1]))
def call(command, args):
    out = subprocess.run(cli + [command, "--json", json.dumps(args)], capture_output=True, text=True, check=True)
    return json.loads(out.stdout)
for m in call("mail.inbox", {"address": ship})["messages"]:
    if m["kind"] == "request" and m["state"] == "open":
        call("mail.send", {"from": ship, "to": m["from"], "kind": "reply", "correlation": m["id"], "body": "handled by " + ship})
through = int(re.search(r"through seq (\d+)", prompt).group(1))
call("mail.ack", {"address": ship, "through_seq": through})
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 1200, "cached_input_tokens": 1000, "output_tokens": 80}}))
"#;

struct Fleet {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    store: Store,
}

impl Fleet {
    fn new(settings: Value) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        let home = root.join("home");
        let mut store = Store::open(&home).unwrap();
        fs::write(
            home.join("config.json"),
            json!({"resource_home": root.join("ledger")}).to_string(),
        )
        .unwrap();
        fs::write(root.join("adapter.py"), ADAPTER).unwrap();
        fs::write(root.join("mate.py"), MATE).unwrap();
        fs::write(
            root.join("manifest.json"),
            json!({"schema_version":1,"id":"scripted-mate","executable":["python3", root.join("adapter.py")],"timeout_ms":5000,"max_output_bytes":1_048_576}).to_string(),
        )
        .unwrap();
        dispatch(
            &mut store,
            "charter.put",
            json!({"id":"fleet","objective":"Answer mail","repo":repo,"min_free_bytes":0,"job_timeout_secs":60}),
        )
        .unwrap();
        for (task, owner) in [("apps-duty", "apps"), ("infra-duty", "infra")] {
            dispatch(
                &mut store,
                "task.create",
                json!({"id":task,"charter_id":"fleet","owner":owner,"title":format!("{owner} duty")}),
            )
            .unwrap();
        }
        dispatch(
            &mut store,
            "ship.put",
            json!({"name":"apps","scope":"apps"}),
        )
        .unwrap();
        let mut fleet = Self {
            _temp: temp,
            root,
            home,
            store,
        };
        fleet.configure(settings);
        fleet
    }
    fn configure(&mut self, settings: Value) {
        let mut profile = json!({
            "manifest": self.root.join("manifest.json"), "duty_task": "infra-duty",
            "options": {"mate": self.root.join("mate.py"), "mode": self.root.join("mode"), "log": self.root.join("prompts.log")},
        });
        for (key, value) in settings.as_object().unwrap() {
            profile[key] = value.clone();
        }
        dispatch(
            &mut self.store,
            "ship.put",
            json!({"name":"infra","scope":"deploys","first_mate":profile}),
        )
        .unwrap();
    }
    fn mode(&self, mode: &str) {
        fs::write(self.root.join("mode"), mode).unwrap();
    }
    fn send(&mut self, args: Value) -> Value {
        dispatch(&mut self.store, "mail.send", args).unwrap()
    }
    fn tick(&self) -> Value {
        let output = Command::new(BIN)
            .args(["--home", self.home.to_str().unwrap(), "serve", "--json"])
            .arg(json!({"once": true}).to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "serve: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn infra(report: &Value) -> Value {
        report["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["ship"] == "infra")
            .cloned()
            .unwrap()
    }
    fn wakes(&mut self) -> Vec<Value> {
        dispatch(&mut self.store, "wake.list", json!({"ship":"infra"})).unwrap()["wakes"]
            .as_array()
            .unwrap()
            .clone()
    }
    /// Tick until the latest wake is no longer running.
    fn settle(&mut self) -> Value {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            self.tick();
            let latest = self.wakes()[0].clone();
            if latest["status"] != "running" {
                return latest;
            }
            assert!(Instant::now() < until, "wake never finished: {latest}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn prompts(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("prompts.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<String>(l).unwrap())
            .collect()
    }
}

fn now_settings() -> Value {
    json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0})
}

#[test]
fn mail_wakes_the_first_mate_which_answers_and_its_usage_is_recorded() {
    let mut fleet = Fleet::new(now_settings());
    let request = fleet.send(
        json!({"id":"staging","from":"apps","to":"infra","kind":"request","body":"Is staging healthy?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    assert_eq!(woke["through_seq"], request["seq"]);
    let wake = fleet.settle();
    assert_eq!(wake["status"], "succeeded", "{wake}");
    assert_eq!(wake["mode"], "fresh");
    assert_eq!(wake["usage"]["cached_input_tokens"], 1000);
    assert_eq!(wake["usage"]["output_tokens"], 80);
    // The first mate answered as its ship and acknowledged what it saw.
    let answered = dispatch(&mut fleet.store, "mail.show", json!({"id":"staging"})).unwrap();
    assert_eq!(answered["state"], "answered");
    let reply = dispatch(&mut fleet.store, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(reply["messages"][0]["body"], "handled by infra");
    let status = dispatch(&mut fleet.store, "status", json!({})).unwrap();
    assert_eq!(status["open_requests"], 0);
    // The wake text follows the stable brief, so the brief stays a cacheable prefix.
    let prompt = &fleet.prompts()[0];
    assert!(prompt.starts_with("Task infra-duty:"), "{prompt}");
    assert!(prompt.contains("New mail through seq"));
    assert!(prompt.contains("id staging from apps kind request"));
    // Nothing new: no further wake.
    let idle = Fleet::infra(&fleet.tick());
    assert_eq!(idle["skipped"], "nothing wake-worthy");
}

#[test]
fn an_awake_first_mate_is_not_woken_twice_and_the_next_wake_covers_only_new_mail() {
    let mut fleet = Fleet::new(now_settings());
    fleet.mode("sleep 1.5");
    fleet.send(json!({"id":"first","from":"apps","to":"infra","kind":"request","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.send(json!({"id":"second","from":"apps","to":"infra","kind":"request","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "first mate already awake"
    );
    // The tick that records the first wake as finished wakes the first mate
    // again for the mail that arrived meanwhile; settle waits for both.
    fleet.settle();
    let wakes = fleet.wakes();
    assert_eq!(wakes.len(), 2);
    assert!(wakes.iter().all(|w| w["status"] == "succeeded"));
    let prompts = fleet.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("id second"));
    assert!(!prompts[1].contains("id first"));
}

#[test]
fn routine_notices_wait_for_the_next_real_wake() {
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","urgency":"low","body":"FYI: docs updated"}),
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    fleet.send(
        json!({"id":"real","from":"apps","to":"infra","kind":"notice","body":"Deploy window moved"}),
    );
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    let prompt = &fleet.prompts()[0];
    assert!(prompt.contains("id fyi") && prompt.contains("id real"));
}

#[test]
fn batching_and_the_hourly_budget_defer_wakes() {
    let mut fleet = Fleet::new(json!({"batch_secs":3600,"max_delay_secs":3600}));
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["skipped"], "batching");
    fleet.configure(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":1}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "hourly wake budget spent"
    );
}

#[test]
fn a_failed_wake_backs_off_then_retries_the_same_mail() {
    let mut fleet =
        Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600}));
    fleet.mode("fail");
    fleet
        .send(json!({"id":"retry-me","from":"apps","to":"infra","kind":"request","body":"please"}));
    fleet.tick();
    let failed = fleet.settle();
    assert_eq!(failed["status"], "failed");
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "backing off after a failed wake"
    );
    fleet.configure(now_settings());
    fleet.mode("normal");
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(
        dispatch(&mut fleet.store, "mail.show", json!({"id":"retry-me"})).unwrap()["state"],
        "answered"
    );
}

#[test]
fn only_one_operator_dispatcher_runs_per_home() {
    let fleet = Fleet::new(now_settings());
    let agent = Command::new(BIN)
        .args([
            "--home",
            fleet.home.to_str().unwrap(),
            "serve",
            "--json",
            "{\"once\":true}",
        ])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    let mut background = Command::new(BIN)
        .args([
            "--home",
            fleet.home.to_str().unwrap(),
            "serve",
            "--json",
            "{\"interval_secs\":1}",
        ])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let second = Command::new(BIN)
        .args([
            "--home",
            fleet.home.to_str().unwrap(),
            "serve",
            "--json",
            "{\"once\":true}",
        ])
        .output()
        .unwrap();
    background.kill().unwrap();
    background.wait().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
}

#[test]
fn first_mate_profiles_are_validated() {
    let mut fleet = Fleet::new(now_settings());
    let manifest = fleet.root.join("manifest.json");
    for bad in [
        json!({"manifest": manifest, "duty_task": "apps-duty"}),
        json!({"manifest": "relative/manifest.json", "duty_task": "infra-duty"}),
        json!({"duty_task": "infra-duty"}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "batch_secs": -1}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "surprise": true}),
    ] {
        assert!(
            dispatch(
                &mut fleet.store,
                "ship.put",
                json!({"name":"infra","first_mate":bad})
            )
            .is_err(),
            "{bad}"
        );
    }
    let ship = dispatch(&mut fleet.store, "ship.show", json!({"name":"infra"})).unwrap();
    assert_eq!(ship["first_mate"]["max_wakes_per_hour"], 12);
}
