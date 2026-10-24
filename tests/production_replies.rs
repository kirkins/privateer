//! Ordinary replies must not close typed deployment approval requests.
mod support;

use serde_json::{Value, json};
use std::fs;

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            temp: tempfile::tempdir().unwrap(),
        };
        let repo = fixture.temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fixture.ok("init", json!({}));
        fixture.ok(
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0,
                "actions":["inspect","implement","test","merge_green","deploy_production"]}),
        );
        fixture.ok(
            "task.create",
            json!({"id":"release","charter_id":"project","owner":"apps","title":"Release"}),
        );
        fixture.ok(
            "ship.put",
            json!({"name":"apps","scope":"Application work"}),
        );
        fixture
    }

    fn call(&self, command: &str, args: Value) -> Result<Value, String> {
        let output = support::isolated(self.temp.path())
            .arg("--home")
            .arg(self.temp.path().join("home"))
            .args([command, "--json", &args.to_string()])
            .output()
            .unwrap();
        if output.status.success() {
            Ok(serde_json::from_slice(&output.stdout).unwrap())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn ok(&self, command: &str, args: Value) -> Value {
        self.call(command, args).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        let conn =
            rusqlite::Connection::open(self.temp.path().join("home/privateer.sqlite")).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn sent_and_delivered_replies_leave_production_approval_open_for_a_real_grant() {
    for command in ["mail.send", "mail.deliver"] {
        for with_answer in [false, true] {
            let fixture = Fixture::new();
            fixture.ok(
                "mail.send",
                json!({"id":"ask","from":"apps","to":"captain","kind":"request",
                    "request_kind":"production_approval","task_id":"release","body":"Ship?",
                    "payload":{"artifact":"sha256:exact","environment":"production"}}),
            );
            let needs = fixture.ok("runtime.needs", json!({}));
            let version = needs["items"][0]["version"].clone();
            let before = fixture.ok("mail.show", json!({"id":"ask"}));
            let mut reply = json!({"id":"not-an-approval","from":"captain","to":"apps",
                "kind":"reply","correlation":"ask","body":"yes"});
            if with_answer {
                reply["payload"] = json!({"answer":"yes"});
            }
            let error = fixture.call(command, reply).unwrap_err();
            assert!(error.contains("production approval"), "{command}: {error}");
            assert!(error.contains("approval.grant"), "{error}");
            assert!(error.contains("approval.deny"), "{error}");
            // Every call reopens the fixture's home in a fresh subprocess.
            assert_eq!(fixture.ok("mail.show", json!({"id":"ask"})), before);
            assert_eq!(before["state"], "open");
            for table in ["approvals", "mail WHERE kind='reply'"] {
                assert_eq!(
                    fixture.count(table),
                    0,
                    "{command}, answer={with_answer}, {table}"
                );
            }
            let gate = json!({"task_id":"release","current_head":"sha256:exact",
                "tested_head":"sha256:exact","checks_passed":true,
                "deploys_production":true,"environment":"production"});
            let error = fixture.call("delivery.check", gate.clone()).unwrap_err();
            assert!(error.contains("approval"), "{error}");
            fixture.ok(
                "approval.grant",
                json!({"id":"real-grant","request_id":"ask","expected_version":version,
                    "task_id":"release","artifact":"sha256:exact","environment":"production",
                    "approver":"captain"}),
            );
            assert_eq!(
                fixture.ok("mail.show", json!({"id":"ask"}))["state"],
                "approved"
            );
            assert_eq!(
                fixture.ok("delivery.check", gate)["approval_id"],
                "real-grant"
            );
            // Neither entry point loses ordinary request/reply behavior.
            fixture.ok(
                "mail.send",
                json!({"id":"ordinary","from":"apps","to":"captain","kind":"request",
                    "body":"Continue?"}),
            );
            fixture.ok(
                command,
                json!({"id":"ordinary-reply","from":"captain","to":"apps","kind":"reply",
                    "correlation":"ordinary","body":"yes","payload":{"answer":"yes"}}),
            );
            assert_eq!(
                fixture.ok("mail.show", json!({"id":"ordinary"}))["state"],
                "answered"
            );
        }
    }
}
