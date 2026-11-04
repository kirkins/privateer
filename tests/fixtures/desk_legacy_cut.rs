#[test]
fn legacy_apply_and_revoke_refuse_agents_before_home_startup() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path().join("alpha"), "survey");
    let absent = m.root.join("gamma");
    for command in ["machine.desk.legacy.apply", "machine.desk.legacy.revoke", "machine.desk.status"] {
        let out = m.command().args(["--role","agent","--actor","survey","--home",absent.to_str().unwrap(),command,"--json","{}"] ).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("operator"),"{}",String::from_utf8_lossy(&out.stderr));
        assert!(!absent.exists());
    }
}

#[test]
fn legacy_plan_is_agent_callable_without_opening_a_home_and_operator_can_apply() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path().join("alpha"),"survey").with_intake_budget(10_000);
    proposal(&m,"here",&["here"]);
    m.accept();
    intake_until_published(&m);
    for _ in 0..4 { intake_idle(&m); m.collect(); }
    let db = intake_db(&m);
    let source: String = db.query_row("SELECT id FROM door_reader_identity WHERE only=1",[],|r|r.get(0)).unwrap();
    let absent = m.root.join("gamma");
    let args = json!({"source":source});
    let before = intake_audits(&m);
    let out = m.command().args(["--role","agent","--actor","survey","--home",absent.to_str().unwrap(),"machine.desk.legacy.plan","--json",&args.to_string()]).output().unwrap();
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert!(!absent.exists());
    assert_eq!(intake_audits(&m),before);
    let plan: Value = serde_json::from_slice(&out.stdout).unwrap();
    m.ok("machine.desk.legacy.apply",json!({"source":source,"digest":plan["digest"],"reason":"Explicit closed history boundary"}));
    assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM desk_legacy_cut)",[],|r|r.get::<_,bool>(0)).unwrap());
    m.ok("machine.desk.legacy.revoke",json!({"source":source,"reason":"Withdraw boundary"}));
    assert!(!db.query_row("SELECT EXISTS(SELECT 1 FROM desk_legacy_cut)",[],|r|r.get::<_,bool>(0)).unwrap());
}

#[test]
fn pushed_reply_records_authenticated_origin_not_envelope_claim() {
    let (_dir, _a, b) = pair();
    let reply = json!({"id":"pushed-reply","from":"survey","to":"captain","kind":"reply","correlation":"old-ask","body":"Reply from alpha","via":"gamma"});
    let out = b.gateway("fleet.captain",json!({"machine":"alpha","messages":[reply]}));
    assert_eq!(out["ok"],true,"{out}");
    assert_eq!(out["result"]["delivered"],json!(["pushed-reply"]),"{out}");
    let home = b.root.join("relay/.privateer/privateer.sqlite");
    let c = Connection::open(home).unwrap();
    let origin: Option<String> = c.query_row("SELECT origin_machine FROM mail WHERE id='pushed-reply'",[],|r|r.get(0)).unwrap();
    assert_eq!(origin.as_deref(),Some("alpha"));
    let out = b.gateway("mail.deliver",json!({"id":"untrusted-reply","from":"survey","to":"relay","kind":"notice","body":"Unlinked origin claim","via":"gamma"}));
    assert_eq!(out["ok"],true,"{out}");
    let origin: Option<String> = c.query_row("SELECT origin_machine FROM mail WHERE id='untrusted-reply'",[],|r|r.get(0)).unwrap();
    assert_eq!(origin,None);
    // Local dispatch does not pass through the gateway's link validation.
    // Even a claim naming a linked machine is not an agent's provenance.
    let args = json!({"id":"agent-origin-claim","from":"survey","to":"relay","kind":"notice","body":"Local agent origin claim","via":"alpha"});
    let out = b.command().args(["--role","agent","--actor","survey","--home",b.root.join("relay/.privateer").to_str().unwrap(),"mail.deliver","--json",&args.to_string()]).output().unwrap();
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let origin: Option<String> = c.query_row("SELECT origin_machine FROM mail WHERE id='agent-origin-claim'",[],|r|r.get(0)).unwrap();
    assert_eq!(origin,None);
    // Runtime wait notices used to consume this same agent-controlled `via`
    // before the later origin filter. Refuse them before schedule mutation.
    for key in ["task_end", "wait_closed", "wait_subscribe", "wait_unsubscribe"] {
        let mut notice = args.clone();
        notice["id"] = json!(format!("agent-{key}"));
        notice["payload"] = json!({key:{"task":"beta-work","status":"done"}});
        let out = b.command().args(["--role","agent","--actor","survey","--home",b.root.join("relay/.privateer").to_str().unwrap(),"mail.deliver","--json",&notice.to_string()]).output().unwrap();
        assert!(!out.status.success(), "{key}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("operator"), "{}", String::from_utf8_lossy(&out.stderr));
    }
}
