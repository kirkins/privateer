mod support;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const SESSION: &str = "46a81234-1234-4567-8123-123456789abc";
const FIRST: &str = "46a81234-1234-4567-8123-123456789abd";
const SECOND: &str = "46a81234-1234-4567-8123-123456789abe";
const HOLDER: &str = "start:fixture:46a81234-1234-4567-8123-123456789abf";

struct Cursor {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
    home: PathBuf,
}
impl Cursor {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let folder = root.join("captain's ship");
        fs::create_dir(&folder).unwrap();
        let s = Self {
            _temp: temp,
            home: folder.join(".privateer"),
            root,
            folder,
        };
        s.ok(&["init", "--harness", "cursor", "--ship", "infra"]);
        s.ok(&[
            "ship.attend",
            "--json",
            &json!({"ship":"infra","holder":HOLDER,"harness":"cursor","ttl_secs":90}).to_string(),
        ]);
        s.transcript("");
        s
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.args(args)
            .current_dir(&self.folder)
            .env("HOME", self.root.join("user"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"));
        c
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn ship(&self) -> String {
        serde_json::from_slice::<Value>(&fs::read(self.home.join("config.json")).unwrap()).unwrap()
            ["ship"]
            .as_str()
            .unwrap()
            .into()
    }
    fn payload(&self, event: &str, generation: &str) -> Value {
        json!({"conversation_id":SESSION,"session_id":SESSION,"generation_id":generation,
               "hook_event_name":event,"workspace_roots":[self.folder],"status":"completed","loop_count":0,
               "transcript_path":self.root.join(format!("user/.cursor/projects/project/agent-transcripts/{SESSION}/{SESSION}.jsonl"))})
    }
    fn spawn(&self, event: &str, payload: Value, env: &[(&str, &str)]) -> Child {
        let actor = self.ship();
        let mut c = self.command(&["hook", "cursor", event]);
        c.env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", actor)
            .env("PRIVATEER_ATTENDED_HARNESS", "cursor")
            .env("PRIVATEER_PRESENCE_HOLDER", HOLDER)
            .env("PRIVATEER_CURSOR_PARK_SECS", "1")
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child
    }
    fn hook(&self, event: &str, payload: Value) -> Value {
        Self::output(self.spawn(event, payload, &[]).wait_with_output().unwrap())
    }
    fn output(out: Output) -> Value {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn state(&self) -> Option<Value> {
        let db = rusqlite::Connection::open(self.home.join("privateer.sqlite")).unwrap();
        db.query_row("SELECT data FROM cursor_mail", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .unwrap_or(None)
        .map(|s| serde_json::from_str(&s).unwrap())
    }
    fn wait_park(&self) {
        let end = Instant::now() + Duration::from_secs(5);
        while !self.state().is_some_and(|s| s["phase"] == "park") {
            assert!(
                Instant::now() < end,
                "Cursor hook never parked: {:?}",
                self.state()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn mail(&self, n: usize) {
        let mut store = privateer::store::Store::open(&self.home).unwrap();
        for i in 0..n {
            privateer::dispatch(&mut store,"mail.send",json!({"id":format!("request-{i}"),"from":"captain","to":self.ship(),"kind":"request","body":"BODY-MUST-STAY-IN-SQLITE"})).unwrap();
        }
    }
    fn transcript(&self, message: &str) -> PathBuf {
        let p = self.root.join(format!(
            "user/.cursor/projects/project/agent-transcripts/{SESSION}/{SESSION}.jsonl"
        ));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p,format!("{}\n",json!({"role":"user","message":{"content":[{"type":"text","text":format!("<user_query>\n{message}\n</user_query>")}]}}))).unwrap();
        p
    }
}

#[test]
fn cursor_hook_coalesces_bursts_without_confusing_queue_admission_with_delivery() {
    let c = Cursor::new();
    let ship = c.ship();
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    c.hook("sessionStart", c.payload("sessionStart", SESSION)); // Fire-and-forget startup arrives late.
    assert_eq!(c.state().unwrap()["generation"], FIRST);
    c.mail(1000);
    let out = c.hook("stop", c.payload("stop", FIRST));
    let notice = out["followup_message"].as_str().unwrap();
    assert!(notice.len() < 8192);
    assert!(!notice.contains("BODY-MUST-STAY-IN-SQLITE"));
    assert!(notice.contains("1000 new mailbox arrivals"));
    let state = c.state().unwrap();
    assert_eq!(state["pending"], 1000);
    assert_eq!(
        state["seen"][0][1], 0,
        "queue admission must not be a delivery receipt"
    );
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_null());
    let transcript = c.transcript(notice);
    let mut stop = c.payload("stop", SECOND);
    stop["transcript_path"] = json!(transcript);
    assert!(c.hook("stop", stop.clone())["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["seen"][0][1], 1000);
    assert!(c.state().unwrap()["pending"].is_null());
    let inbox: Value = serde_json::from_str(&c.ok(&[
        "mail.inbox",
        "--json",
        &json!({"address":ship,"limit":1000}).to_string(),
    ]))
    .unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1000);
    assert!(
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["state"] == "open")
    );
    // Lost presentation cache and a replacement wrapper must recover from the
    // native record without acknowledging or replaying the unhandled request.
    rusqlite::Connection::open(c.home.join("privateer.sqlite"))
        .unwrap()
        .execute("DELETE FROM cursor_mail", [])
        .unwrap();
    let mut prompt = c.payload("beforeSubmitPrompt", SECOND);
    prompt["transcript_path"] = stop["transcript_path"].clone();
    c.hook("beforeSubmitPrompt", prompt);
    assert!(c.hook("stop", stop)["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["seen"][0][1], 1000);
}

#[test]
fn captain_input_and_presence_replacement_supersede_parked_hooks() {
    let c = Cursor::new();
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    let parked = c.spawn(
        "stop",
        c.payload("stop", FIRST),
        &[("PRIVATEER_CURSOR_PARK_SECS", "10")],
    );
    c.wait_park();
    c.hook(
        "beforeSubmitPrompt",
        c.payload("beforeSubmitPrompt", SECOND),
    );
    c.mail(1);
    assert!(Cursor::output(parked.wait_with_output().unwrap())["followup_message"].is_null());
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_null());
    assert!(c.hook("stop", c.payload("stop", SECOND))["followup_message"].is_string());
    let c = Cursor::new();
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    let parked = c.spawn(
        "stop",
        c.payload("stop", FIRST),
        &[("PRIVATEER_CURSOR_PARK_SECS", "10")],
    );
    c.wait_park();
    c.ok(&[
        "ship.attend",
        "--json",
        &json!({"ship":c.ship(),"holder":"start:replacement","harness":"cursor","ttl_secs":90})
            .to_string(),
    ]);
    c.mail(1);
    assert!(Cursor::output(parked.wait_with_output().unwrap())["followup_message"].is_null());
    assert!(c.hook("sessionEnd", c.payload("sessionEnd", FIRST))["followup_message"].is_null());
    let p: Value = serde_json::from_str(&c.ok(&[
        "ship.presence",
        "--json",
        &json!({"ship":c.ship()}).to_string(),
    ]))
    .unwrap();
    assert_eq!(p["lease"]["holder"], "start:replacement");
}

#[test]
fn error_and_abort_pause_wakes_until_a_real_captain_prompt() {
    for status in ["error", "aborted"] {
        let c = Cursor::new();
        c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
        let mut stop = c.payload("stop", FIRST);
        stop["status"] = json!(status);
        c.hook("stop", stop);
        c.mail(1);
        assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_null());
        assert_eq!(c.state().unwrap()["paused"], true);
        c.hook("sessionStart", c.payload("sessionStart", SESSION));
        assert_eq!(c.state().unwrap()["paused"], true);
        c.hook(
            "beforeSubmitPrompt",
            c.payload("beforeSubmitPrompt", SECOND),
        );
        assert!(c.hook("stop", c.payload("stop", SECOND))["followup_message"].is_string());
    }
}

#[test]
fn cursor_receipts_require_this_native_user_conversation_and_real_mail_sequences() {
    let c = Cursor::new();
    c.mail(1);
    let tag = json!({"home":c.home,"ship":c.ship(),"session":SESSION,"through":1});
    for (role, tag) in [
        ("assistant", tag.clone()),
        (
            "user",
            json!({"home":"/different","ship":c.ship(),"session":SESSION,"through":1}),
        ),
        (
            "user",
            json!({"home":c.home,"ship":c.ship(),"session":SECOND,"through":1}),
        ),
        (
            "user",
            json!({"home":c.home,"ship":c.ship(),"session":SESSION,"through":99999}),
        ),
    ] {
        let path = c.transcript("");
        fs::write(path,format!("{}\n",json!({"role":role,"message":{"content":[{"type":"text","text":format!("[privateer-mail {tag}]")}]}}))).unwrap();
        c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
        assert_eq!(c.state().unwrap()["seen"][0][1], 0);
    }
    // A correctly shaped receipt outside native Cursor storage cannot consume
    // notification progress. Nor can a symlink redirect the native file there.
    let outside = c.root.join(format!(
        "outside/agent-transcripts/{SESSION}/{SESSION}.jsonl"
    ));
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside,format!("{}\n",json!({"role":"user","message":{"content":[{"type":"text","text":format!("[privateer-mail {tag}]")}]}}))).unwrap();
    let mut payload = c.payload("stop", FIRST);
    payload["transcript_path"] = json!(outside);
    assert!(c.hook("stop", payload)["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["paused"], true);
    let native = c.transcript("");
    fs::remove_file(&native).unwrap();
    std::os::unix::fs::symlink(&outside, &native).unwrap();
    c.hook(
        "beforeSubmitPrompt",
        c.payload("beforeSubmitPrompt", SECOND),
    );
    assert!(c.hook("stop", c.payload("stop", SECOND))["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["seen"][0][1], 0);
    fs::remove_file(&native).unwrap();
    c.transcript("");
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_string());
}

#[test]
fn competing_stop_callbacks_emit_one_notification_and_old_sessions_stand_down() {
    let c = Cursor::new();
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    c.mail(1);
    let a = c.spawn("stop", c.payload("stop", FIRST), &[]);
    let b = c.spawn("stop", c.payload("stop", FIRST), &[]);
    let replies = [a, b]
        .into_iter()
        .map(|c| Cursor::output(c.wait_with_output().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        replies
            .iter()
            .filter(|r| r["followup_message"].is_string())
            .count(),
        1
    );
    let mut new = c.payload("sessionStart", SECOND);
    new["conversation_id"] = json!(SECOND);
    new["session_id"] = json!(SECOND);
    c.hook("sessionStart", new.clone());
    assert_eq!(
        c.state().unwrap()["session"],
        SESSION,
        "late startup cannot steal another native session"
    );
    new["hook_event_name"] = json!("beforeSubmitPrompt");
    c.hook("beforeSubmitPrompt", new);
    assert_eq!(c.state().unwrap()["session"], SECOND);
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_null());
    c.hook("sessionEnd", c.payload("sessionEnd", FIRST));
    assert_eq!(c.state().unwrap()["session"], SECOND);
    assert_eq!(c.state().unwrap()["phase"], "busy");
}

#[test]
fn unavailable_native_transcripts_pause_delivery_without_breaking_captain_prompts() {
    let c = Cursor::new();
    c.mail(1);
    let mut prompt = c.payload("beforeSubmitPrompt", FIRST);
    prompt["transcript_path"] = Value::Null;
    assert_eq!(c.hook("beforeSubmitPrompt", prompt)["continue"], true);
    let mut stop = c.payload("stop", FIRST);
    stop["transcript_path"] = Value::Null;
    assert!(c.hook("stop", stop)["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["paused"], true);
    assert!(c.state().unwrap()["pending"].is_null());
    c.hook(
        "beforeSubmitPrompt",
        c.payload("beforeSubmitPrompt", SECOND),
    );
    assert!(c.hook("stop", c.payload("stop", SECOND))["followup_message"].is_string());
}

#[test]
fn durable_acknowledgement_settles_a_receipt_evicted_during_a_long_native_turn() {
    let c = Cursor::new();
    c.mail(1);
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_string());
    c.ok(&[
        "mail.ack",
        "--json",
        &json!({"address":c.ship(),"through_seq":1}).to_string(),
    ]);
    c.ok(&["mail.send","--json",&json!({"id":"later-request","from":"captain","to":c.ship(),"kind":"request","body":"A new arrival after the recorded acknowledgement"}).to_string()]);
    let out = c.hook("stop", c.payload("stop", SECOND));
    assert!(
        out["followup_message"]
            .as_str()
            .unwrap()
            .contains("1 new mailbox arrivals")
    );
    assert_eq!(c.state().unwrap()["seen"][0][1], 1);
    assert_eq!(c.state().unwrap()["pending"], 2);
}

#[test]
fn a_cancelled_unconfirmed_notification_retries_only_after_captain_input() {
    let c = Cursor::new();
    c.mail(1);
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_string());
    assert!(c.hook("stop", c.payload("stop", FIRST))["followup_message"].is_null());
    assert_eq!(c.state().unwrap()["seen"][0][1], 0);
    c.hook(
        "beforeSubmitPrompt",
        c.payload("beforeSubmitPrompt", SECOND),
    );
    assert!(c.state().unwrap()["pending"].is_null());
    assert!(c.hook("stop", c.payload("stop", SECOND))["followup_message"].is_string());
    assert_eq!(c.state().unwrap()["seen"][0][1], 0);
}

#[test]
fn crew_foreign_harness_and_stale_identity_cannot_claim_or_drain_the_mailbox() {
    let c = Cursor::new();
    c.mail(1);
    for env in [
        vec![("PRIVATEER_JOB", "crew")],
        vec![("PRIVATEER_ATTENDED_HARNESS", "claude")],
        vec![("PRIVATEER_PRESENCE_HOLDER", "start:stale")],
    ] {
        assert!(
            Cursor::output(
                c.spawn(
                    "beforeSubmitPrompt",
                    c.payload("beforeSubmitPrompt", FIRST),
                    &env
                )
                .wait_with_output()
                .unwrap()
            )["followup_message"]
                .is_null()
        );
        assert!(c.state().is_none());
    }
    c.hook("beforeSubmitPrompt", c.payload("beforeSubmitPrompt", FIRST));
    let old = c.state().unwrap();
    let mut wrong = c.payload("beforeSubmitPrompt", SECOND);
    wrong["workspace_roots"] = json!(["/"]);
    let out = c
        .spawn("beforeSubmitPrompt", wrong, &[])
        .wait_with_output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["continue"],
        true
    );
    assert_eq!(c.state().unwrap(), old);
    // Cursor loads Claude-compatible hooks too. They must remain inert here.
    let mut child = c
        .command(&["hook", "claude", "Stop"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", c.ship())
        .env("PRIVATEER_ATTENDED_HARNESS", "cursor")
        .env("PRIVATEER_PRESENCE_HOLDER", HOLDER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"session_id":SESSION}).to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.stdout.is_empty() && out.stderr.is_empty() && out.status.success());
    let mut raw = c
        .command(&["hook", "claude", "Stop"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    raw.stdin.take().unwrap().write_all(json!({"session_id":SESSION,"conversation_id":SESSION,"cursor_version":"native-fixture"}).to_string().as_bytes()).unwrap();
    let out = raw.wait_with_output().unwrap();
    assert!(out.stdout.is_empty() && out.stderr.is_empty() && out.status.success());
    let presence: Value = serde_json::from_str(&c.ok(&[
        "ship.presence",
        "--json",
        &json!({"ship":c.ship()}).to_string(),
    ]))
    .unwrap();
    assert_eq!(presence["lease"]["holder"], HOLDER);
}

#[test]
fn cursor_start_preserves_existing_project_hooks_routes_and_native_arguments() {
    let c = Cursor::new();
    fs::create_dir_all(c.folder.join(".cursor")).unwrap();
    let config = c.folder.join(".cursor/hooks.json");
    fs::write(&config,json!({"version":1,"extra":"keep","hooks":{"stop":[{"command":"custom-command","timeout":9}]}}).to_string()).unwrap();
    let bin = c.root.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("cursor-agent");
    fs::write(&fake,r#"#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
Path(os.environ['CURSOR_TEST_RECORD']).write_text(json.dumps({'argv':sys.argv[1:],'actor':os.getenv('PRIVATEER_ACTOR'),'role':os.getenv('PRIVATEER_ROLE'),'route':os.getenv('CURSOR_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')}))
"#).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let record = c.root.join("native.json");
    for _ in 0..2 {
        let out = c
            .command(&[
                "start",
                "cursor",
                "--",
                "--trust",
                "--model",
                "native-choice",
            ])
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("CURSOR_TEST_RECORD", &record)
            .env("CURSOR_TEST_ROUTE", "existing-route")
            .env("CLAUDECODE", "foreign")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let cfg: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(cfg["extra"], "keep");
        assert_eq!(cfg["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert_eq!(cfg["hooks"]["stop"][0]["command"], "custom-command");
        assert!(cfg["hooks"]["stop"][1]["loop_limit"].is_null());
    }
    let native: Value = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert_eq!(
        native["argv"],
        json!(["--trust", "--model", "native-choice"])
    );
    assert_eq!(native["role"], "agent");
    assert_eq!(native["actor"], c.ship());
    assert_eq!(native["route"], "existing-route");
    assert!(native["foreign"].is_null());
    let presence: Value = serde_json::from_str(&c.ok(&[
        "ship.presence",
        "--json",
        &json!({"ship":c.ship()}).to_string(),
    ]))
    .unwrap();
    assert_eq!(presence["attended"], false);
    let other = c.root.join("other-hooks.json");
    fs::write(&other, "untouched").unwrap();
    fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(&other, &config).unwrap();
    assert!(
        !c.command(&["start", "cursor"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(other).unwrap(), "untouched");
}
