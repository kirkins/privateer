#[test]
fn retention_plan_is_agent_callable_without_home_startup_and_apply_is_operator_only() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path().join("alpha"), "alpha");
    let missing = m.root.join("beta");
    let db = Connection::open(m.registry()).unwrap();
    db.execute("INSERT INTO ships(name,home,created_by,created_ms,retired_ms) VALUES('beta',?1,'captain',1,2)", [missing.to_str().unwrap()]).unwrap();
    let args = json!({"home":missing,"ship":"beta","operation":"release"});
    let absent_home = m.root.join("gamma");
    let output = m.command().args(["--role","agent","--actor","alpha","--home",absent_home.to_str().unwrap(),"machine.desk.home.plan","--json",&args.to_string()]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!absent_home.exists(), "read-only plan must not initialize any home");
    assert!(!missing.exists());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["plan"]["allowed"], true);
    let apply = json!({"home":missing,"ship":"beta","digest":plan["digest"],"reason":"Retired test home is absent"});
    for command in ["machine.desk.home.release", "machine.desk.home.readmit"] {
        let out = m.command().args(["--role","agent","--actor","alpha",command,"--json",&apply.to_string()]).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("operator"));
    }
    m.ok("machine.desk.home.release", apply);
    assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_release WHERE ship='beta' AND unread IS NULL AND open IS NULL)", [], |r| r.get::<_, bool>(0)).unwrap());
    let args = json!({"home":missing,"ship":"beta","operation":"readmit"});
    let plan = m.ok("machine.desk.home.plan", args);
    assert_eq!(plan["plan"]["allowed"], false, "absent homes cannot be readmitted");
}
