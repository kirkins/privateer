//! One-way local transport, real gateway subprocesses, isolated registries.
mod support;
include!("fixtures/desk_placement_regressions.rs");
include!("fixtures/desk_intake_transport.rs");
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Machine {
    root: PathBuf,
    ship: &'static str,
    intake_budget_ms: Option<u64>,
}
impl Machine {
    fn new(root: PathBuf, ship: &'static str) -> Self {
        fs::create_dir_all(root.join(ship)).unwrap();
        let m = Self {
            root,
            ship,
            intake_budget_ms: None,
        };
        let o = m.run(&["init", "--ship", ship, "--harness", "codex"]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        m
    }
    fn registry(&self) -> PathBuf {
        self.root.join("fleet.sqlite")
    }
    /// Widen this machine's per-contact desk intake budget for a fixture
    /// whose property under test is the work a quiet contact does, not how
    /// fast the runner is. Production's fixed bound is the default; only a
    /// value at or above it is honoured, and the environment travels to the
    /// gateway subprocesses this machine's contacts spawn.
    fn with_intake_budget(mut self, ms: u64) -> Self {
        self.intake_budget_ms = Some(ms);
        self
    }
    fn command(&self) -> Command {
        let mut c = support::command(support::BIN);
        c.current_dir(self.root.join(self.ship))
            .env("HOME", &self.root)
            .env("PRIVATEER_FLEET", self.registry())
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME");
        if let Some(ms) = self.intake_budget_ms {
            c.env("PRIVATEER_DESK_INTAKE_BUDGET_MS", ms.to_string());
        }
        c
    }
    fn gateway(&self, command: &str, args: Value) -> Value {
        use std::io::Write;
        let mut child = self
            .command()
            .arg("gateway")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"id":"test","command":command,"args":args})
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, verb: &str, args: Value) -> Value {
        let o = self.run(&[verb, "--json", &args.to_string()]);
        assert!(
            o.status.success(),
            "{verb}: {} {}",
            String::from_utf8_lossy(&o.stderr),
            String::from_utf8_lossy(&o.stdout)
        );
        serde_json::from_slice(&o.stdout).unwrap()
    }
    fn fails(&self, verb: &str, args: Value, why: &str) {
        let o = self.run(&[verb, "--json", &args.to_string()]);
        assert!(!o.status.success(), "{verb} unexpectedly succeeded");
        assert!(
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stderr),
                String::from_utf8_lossy(&o.stdout)
            )
            .contains(why),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    fn link(&self, ours: &str, other: &Self, theirs: &str) {
        self.ok("machine.add",json!({"name":theirs,"target":"local-test","executable":support::BIN,"as":ours,"fleet":other.registry(),"transport":"local"}));
    }
    fn row(&self) -> Value {
        let c = Connection::open(self.registry()).unwrap();
        let s: String = c
            .query_row("SELECT record FROM desk_placement_record", [], |r| r.get(0))
            .unwrap();
        serde_json::from_str(&s).unwrap()
    }
    fn status(&self) -> Value {
        self.ok("machine.desk.status", json!({}))
    }
    fn accept(&self) {
        let r = self.row();
        self.ok("machine.desk.accept",json!({"proposal_id":r["proposal"]["proposal_id"],"manifest_hash":r["proposal"]["manifest_hash"]}));
    }
    fn collect(&self) {
        self.ok("machine.collect", json!({}));
    }
    fn save_row(&self, row: &Value) {
        Connection::open(self.registry())
            .unwrap()
            .execute(
                "UPDATE desk_placement_record SET proposal=?1, record=?2",
                rusqlite::params![
                    row["proposal"]["proposal_id"].as_str().unwrap(),
                    row.to_string()
                ],
            )
            .unwrap();
    }
    fn queued_mail(&self, id: &str, to: &str) {
        let sent = self.ok(
            "mail.send",
            json!({"id":id,"from":self.ship,"to":to,"kind":"notice","body":"Ordinary ship mail"}),
        );
        assert_eq!(sent["delivery"], "pending", "{sent}");
    }
    fn assert_mail(&self, id: &str) {
        assert_eq!(self.ok("mail.show", json!({"id":id}))["id"], id);
    }
    fn audit_count(&self, event: &str) -> i64 {
        Connection::open(self.registry())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM desk_placement_audit WHERE event=?1",
                [event],
                |r| r.get(0),
            )
            .unwrap()
    }
}
fn pair() -> (tempfile::TempDir, Machine, Machine) {
    let t = tempfile::tempdir().unwrap();
    let a = Machine::new(t.path().join("alpha"), "survey");
    let b = Machine::new(t.path().join("beta"), "relay");
    a.link("alpha", &b, "beta");
    (t, a, b)
}
fn proposal(m: &Machine, owner: &str, sources: &[&str]) -> Value {
    m.ok(
        "machine.desk.propose",
        json!({"domain":"survey-domain","owner":owner,"sources":sources}),
    )
}
fn assert_agreed(a: &Machine, b: &Machine) {
    let ar = a.row();
    let br = b.row();
    assert_eq!(ar["state"], "active");
    assert_eq!(br["state"], "active");
    assert_eq!(ar["certificate"], br["certificate"]);
    assert_eq!(ar["proposal"]["manifest"]["generation"], 1);
    assert!(!a.status()["desk_here"].as_bool().unwrap());
    let bs = b.status();
    assert_eq!(bs["desk_here"], true);
    assert_eq!(bs["coverage_complete"], false);
    let c = Connection::open(b.registry()).unwrap();
    assert_eq!(
        c.query_row("SELECT role FROM machines WHERE name='alpha'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "collected"
    );
    assert_eq!(
        c.query_row("SELECT machine FROM captain_desk", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "here"
    );
    assert_eq!(ar["local_role"], "source");
    assert_eq!(ar["intake_direction"], "push");
    assert_eq!(br["local_role"], "owner");
    assert_eq!(br["intake_direction"], "local");
}

#[test]
fn callable_proposal_reached_owner_explicit_here_and_restart() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    assert_eq!(a.row()["state"], "pending");
    assert_eq!(b.row()["state"], "pending");
    assert_eq!(b.status()["desk_here"], false);
    let p = a.row()["proposal"].clone();
    a.fails(
        "machine.desk.accept",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":"wrong"}),
        "manifest_hash mismatch",
    );
    a.accept();
    assert_eq!(a.row()["state"], "active"); // Certificate committed; reached peer not yet installed.
    assert_eq!(b.row()["state"], "pending");
    a.status();
    assert_agreed(&a, &b);
    // Each CLI/gateway is a new process. Exact certificate replay is idempotent.
    a.status();
    a.status();
    assert_agreed(&a, &b);
    a.fails(
        "machine.captain",
        json!({"machine":null}),
        "legacy_writer_fenced",
    );
    let away = b.run(&["afk", "out"]);
    assert!(!away.status.success());
    assert!(String::from_utf8_lossy(&away.stderr).contains("legacy_writer_fenced"));
    let c = Connection::open(a.registry()).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT COUNT(*) FROM door_coverage_source WHERE domain='survey-domain'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    proposal_rejected_owned(&a);
}
fn proposal_rejected_owned(a: &Machine) {
    a.fails(
        "machine.desk.propose",
        json!({"domain":"other-domain","owner":"beta","sources":["alpha","beta"]}),
        "restore_guard",
    );
}

#[test]
fn reached_proposal_decision_reply_without_callout_and_mail_is_not_approval() {
    let (_t, a, b) = pair();
    proposal(&b, "beta", &["alpha", "beta"]);
    assert_eq!(b.row()["state"], "pending");
    a.collect();
    let p = a.row()["proposal"].clone();
    // A delivered reply is not the operator's own reply. The mail import path
    // does not invoke answered(), even with an operator-looking payload.
    b.ok("mail.deliver",json!({"id":"imported-reply","from":"captain","to":"relay","kind":"reply","correlation":p["decision"],"body":"not approval","payload":{"answer":{"option_id":"accept"},"operator":true}}));
    assert!(a.row()["approval"].is_null());
    assert!(b.row()["approval"].is_null());
    let card = a.ok("mail.inbox", json!({"address":"captain"}));
    assert!(
        card.to_string().contains(p["decision"].as_str().unwrap()),
        "{card}"
    );
    a.ok("captain.reply",json!({"id":p["decision"],"body":"Accept this exact manifest","payload":{"answer":{"option_id":"accept"}}}));
    assert_eq!(a.row()["approval"], true);
    a.collect();
    a.collect();
    assert_agreed(&a, &b);
    assert_eq!(
        Connection::open(b.registry())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM machines WHERE role='collects'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn owner_coordinates_three_machine_relay_over_only_legal_contacts() {
    let (t, a, b) = pair();
    let g = Machine::new(t.path().join("gamma"), "lookout");
    g.link("gamma", &b, "beta");
    proposal(&a, "beta", &["alpha", "beta", "gamma"]);
    let p = a.row()["proposal"].clone();
    assert_eq!(p["manifest"]["coordinator"], b.status()["core"]);
    a.accept();
    // Gamma contacts only beta; beta has no callable links at all.
    g.collect();
    a.collect();
    g.collect();
    a.collect();
    g.collect();
    a.status();
    assert_agreed(&a, &b);
    assert_eq!(g.row()["state"], "active");
    assert_eq!(g.row()["certificate"], b.row()["certificate"]);
    assert_eq!(g.row()["intake_direction"], "push");
}

#[test]
fn missing_owner_intake_contact_rejects_before_staging() {
    let (t, a, b) = pair();
    let g = Machine::new(t.path().join("gamma"), "lookout");
    a.link("alpha", &g, "gamma");
    a.fails(
        "machine.desk.propose",
        json!({"domain":"survey-domain","owner":"beta","sources":["alpha","beta","gamma"]}),
        "no_legal_contact",
    );
    for m in [&a, &b, &g] {
        assert!(m.status()["records"].as_array().unwrap().is_empty());
    }
}

#[test]
fn concurrent_proposers_reserve_one_domain_without_dual_owners() {
    let (_t, a, b) = pair();
    let mut ac = a.command();
    let mut bc = b.command();
    let body =
        json!({"domain":"survey-domain","owner":"beta","sources":["alpha","beta"]}).to_string();
    let aa = ac
        .args(["machine.desk.propose", "--json", &body])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let bb = bc
        .args(["machine.desk.propose", "--json", &body])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ao = aa.wait_with_output().unwrap();
    let bo = bb.wait_with_output().unwrap();
    assert!(ao.status.success() || bo.status.success());
    // Either a single proposal wins, or each participant reserves a different
    // pending proposal. A conflict sacrifices availability, never elects twice.
    let ar = a.row();
    let br = b.row();
    if ar["proposal"] == br["proposal"] {
        a.accept();
        a.status();
        assert_agreed(&a, &b);
    } else {
        a.accept();
        b.accept();
        let _ = a.run(&["machine.collect", "--json", "{}"]);
        assert!(a.row()["certificate"].is_null());
        assert!(b.row()["certificate"].is_null());
        assert_eq!(a.status()["desk_here"], false);
        assert_eq!(b.status()["desk_here"], false);
    }
}

#[test]
fn registry_restore_cannot_rebootstrap_or_reactivate_an_unvalidated_owner() {
    let (_t, a, b) = pair();
    let backup = b.root.join("before.sqlite");
    Connection::open(b.registry())
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
        .unwrap();
    proposal(&a, "beta", &["alpha", "beta"]);
    a.accept();
    a.status();
    assert_agreed(&a, &b);
    let cert = b.row()["certificate"].clone();
    // Restore the registry, not the durable enrollment/certificate fence.
    fs::copy(backup, b.registry()).unwrap();
    let s = b.status();
    assert_eq!(s["restore_guard"], true);
    assert_eq!(s["desk_here"], false);
    b.fails(
        "machine.desk.propose",
        json!({"domain":"survey-domain","owner":"beta","sources":["alpha","beta"]}),
        "restore_guard",
    );
    // Its still-valid linked coordinator can replay the exact certificate.
    a.status();
    assert_eq!(b.row()["certificate"], cert);
    assert_agreed(&a, &b);
}

#[test]
fn callable_owner_records_collect_direction_without_requiring_source_callout() {
    let (_t, a, b) = pair();
    proposal(&b, "alpha", &["alpha", "beta"]);
    a.collect();
    b.accept();
    a.collect();
    a.collect();
    assert_eq!(a.row()["state"], "active");
    assert_eq!(b.row()["state"], "active");
    assert_eq!(a.row()["certificate"], b.row()["certificate"]);
    assert_eq!(a.row()["intake_direction"], "local");
    assert_eq!(b.row()["intake_direction"], "collect");
    assert_eq!(a.status()["desk_here"], true);
    assert_eq!(b.status()["desk_here"], false);
    assert_eq!(
        Connection::open(b.registry())
            .unwrap()
            .query_row("SELECT machine FROM captain_desk", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "alpha"
    );
}

#[test]
fn lost_commit_receipt_replays_exact_certificate_and_delegates_cannot_accept() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    a.accept();
    assert_eq!(b.row()["state"], "pending");
    let outgoing = a.gateway("desk.placement.status", json!({"machine":"beta"}));
    assert_eq!(outgoing["ok"], true, "{outgoing}");
    let args = json!({"machine":"alpha","desk_placement":outgoing["result"]["desk_placement"]});
    // Discard the first receipt after the real subprocess has committed. A new
    // gateway process must replay the same certificate, not select an owner.
    let _lost_receipt = b.gateway("desk.placement.commit", args.clone());
    assert_eq!(b.row()["state"], "active");
    let retried = b.gateway("desk.placement.commit", args);
    assert_eq!(retried["ok"], true, "{retried}");
    assert_agreed(&a, &b);
    let denied = b.gateway("fleet.delegated.v1",json!({"context":{"version":1,"client":"00000000-0000-4000-8000-000000000001","resource":"door:fixture","scopes":["fleet:read","mail:read","captain:presence"],"ships":null,"result_id":"00000000-0000-4000-8000-000000000002"},"command":"desk.placement.accept","args":{}}));
    assert_eq!(denied["ok"], false);
    assert_eq!(a.row()["certificate"], b.row()["certificate"]);
}

#[test]
fn unresolved_away_is_not_silently_imported_or_timestamp_merged() {
    let (_t, a, b) = pair();
    let away = b.run(&["afk", "out"]);
    assert!(
        away.status.success(),
        "{}",
        String::from_utf8_lossy(&away.stderr)
    );
    proposal(&a, "beta", &["alpha", "beta"]);
    a.accept();
    assert!(a.row()["certificate"].is_null());
    assert!(b.status()["records"].as_array().unwrap().is_empty());
    // A refused placement must not stop ship mail while away is unresolved.
    b.queued_mail("during-away-conflict", "survey");
    a.collect();
    a.assert_mail("during-away-conflict");
    // The refused participant was not fenced or moved. It can reconcile its
    // own away period, then receive the exact proposal and confirmation.
    assert!(b.run(&["back"]).status.success());
    a.status();
    a.status();
    assert_agreed(&a, &b);
}

#[test]
fn status_is_not_acceptance_and_stale_hashes_cannot_stage() {
    let (_t, a, b) = pair();
    proposal(&a, "beta", &["alpha", "beta"]);
    let mut frame =
        a.gateway("desk.placement.status", json!({"machine":"beta"}))["result"]["desk_placement"]
            .clone();
    frame["records"][0]["approval"] = json!(true);
    let refused = b.gateway(
        "desk.placement.status",
        json!({"machine":"alpha","desk_placement":frame}),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(b.row()["approval"].is_null());
    assert!(b.row()["accepted"].as_array().unwrap().is_empty());
    frame["records"][0]["proposal"]["manifest_hash"] = json!("stale");
    let refused = b.gateway(
        "desk.placement.accept",
        json!({"machine":"alpha","desk_placement":frame}),
    );
    assert_eq!(refused["ok"], false);
    assert!(b.row()["approval"].is_null());
}

#[test]
fn local_write_ahead_certificate_recovery_requires_exact_binding() {
    let t = tempfile::tempdir().unwrap();
    let a = Machine::new(t.path().join("alpha"), "survey");
    proposal(&a, "here", &["here"]);
    let p = a.row()["proposal"].clone();
    let backup = a.root.join("pending.sqlite");
    Connection::open(a.registry())
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_str().unwrap()])
        .unwrap();
    a.accept();
    let certificate = a.row()["certificate"].clone();
    // Model the external certificate commit / registry install boundary by
    // restarting with the pre-install registry and the synced certified guard.
    fs::copy(backup, a.registry()).unwrap();
    let status = a.status();
    assert_eq!(status["restore_guard"], true);
    assert_eq!(status["desk_here"], false);
    a.fails(
        "machine.desk.accept",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
        "restore_guard",
    );
    a.fails(
        "machine.desk.commit",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":"wrong"}),
        "exact recovery binding required",
    );
    a.ok(
        "machine.desk.commit",
        json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}),
    );
    assert_eq!(a.row()["certificate"], certificate);
    assert_eq!(a.status()["desk_here"], true);
}

#[test]
fn local_domain_uses_the_same_certificate_and_reject_card_aborts() {
    let t = tempfile::tempdir().unwrap();
    let a = Machine::new(t.path().join("alpha"), "survey");
    proposal(&a, "here", &["here"]);
    let p = a.row()["proposal"].clone();
    a.ok("captain.reply",json!({"id":p["decision"],"body":"Not this placement","payload":{"answer":{"option_id":"reject"}}}));
    assert_eq!(a.row()["state"], "aborted");
    proposal(&a, "here", &["here"]);
    a.accept();
    assert_eq!(a.row()["state"], "active");
    assert_eq!(a.status()["desk_here"], true);
}
