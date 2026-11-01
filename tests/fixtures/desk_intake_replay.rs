// Replay custody uses real gateway processes in both legal directions and the
// owner's own idle intake. Adoption never creates a subscription or selects mail.
#[test]
fn replay_witness_custody_crosses_collect_push_and_local_restart_boundaries() {
    for owner in ["alpha", "beta"] {
        let (_t,a,b) = pair();
        let a = a.with_intake_budget(10_000);
        let b = b.with_intake_budget(10_000);
        let (desk,source) = if owner == "alpha" { (&a,&b) } else { (&b,&a) };
        intake_enroll(&a,owner);
        let core = intake_external_source(desk);
        // Establish/seed the baseline before creating new native content.
        // Each unit is bounded protocol work, not a timing-based retry.
        for _ in 0..8 {
            a.collect(); intake_idle(source); a.collect();
            if intake_received_proof(desk,&core).is_some() { break; }
        }
        assert!(intake_received_proof(desk,&core).is_some());
        intake_notice(source,"native-after-baseline");
        let adopted = |m: &Machine,id: &str| intake_db(m).query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_replay_adopted WHERE id=?1)",[id],|r|r.get::<_,bool>(0)).unwrap();
        for _ in 0..8 {
            a.collect(); intake_idle(source); a.collect();
            if adopted(desk,"native-after-baseline") { break; }
        }
        let audits = || intake_db(desk).prepare("SELECT body FROM desk_intake_audit WHERE stage LIKE 'replay.%'").unwrap()
            .query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(adopted(desk,"native-after-baseline"),"{owner}: remote provisional adoption absent: {:?}",audits());
        // The final adoption receipt rides legal contacts; its bounded retry
        // window expires without any source witness loss before desk custody.
        for _ in 0..5 { a.collect(); }
        assert!(!intake_db(source).query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='native-after-baseline')",[],|r|r.get::<_,bool>(0)).unwrap());
        assert_eq!(intake_db(desk).query_row("SELECT first_kind FROM desk_intake_original WHERE id='native-after-baseline'",[],|r|r.get::<_,String>(0)).unwrap(),"native");
        let local: String = intake_db(desk).query_row("SELECT id FROM door_reader_identity WHERE only=1",[],|r|r.get(0)).unwrap();
        for _ in 0..8 {
            intake_idle(desk);
            if intake_received_proof(desk,&local).is_some() { break; }
        }
        assert!(intake_received_proof(desk,&local).is_some());
        intake_notice(desk,"native-local-after-baseline");
        for _ in 0..8 {
            intake_idle(desk);
            if adopted(desk,"native-local-after-baseline") { break; }
        }
        assert!(adopted(desk,"native-local-after-baseline"),"{owner}: local provisional adoption absent: {:?}",audits());
        assert_eq!(intake_db(desk).query_row("SELECT COUNT(*) FROM desk_subscription_replay",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
}

// Opt-in pinned old executable: the admitted gate records its SHA-256 and
// invokes this test explicitly. A logging proxy delegates every operation to
// a REAL old/new gateway process; it does not emulate either collect decoder.
#[test]
#[ignore = "requires PRIVATEER_TEST_OLD_BINARY pointing to a pre-replay binary"]
fn replay_sidecar_real_old_new_subprocesses_in_both_collect_directions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let old = std::env::var("PRIVATEER_TEST_OLD_BINARY").expect("old binary artifact required");
    assert!(std::path::Path::new(&old).is_absolute());
    for owner in ["alpha", "beta"] {
        for old_peer in ["alpha", "beta"] {
            let (t,a,b) = pair();
            intake_enroll(&a,owner);
            let legacy = if old_peer == "alpha" { &a } else { &b };
            symlink(&old,legacy.root.join("legacy-privateer")).unwrap();
            let executable = if old_peer == "beta" { old.as_str() } else { support::BIN };
            let log = t.path().join("collect-wire.jsonl");
            let proxy = t.path().join("gateway-proxy");
            fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nr=json.loads(sys.stdin.readline())\np=subprocess.run([{}]+sys.argv[1:],input=json.dumps(r)+'\\n',text=True,capture_output=True)\nwith open({},'a') as f:\n f.write(json.dumps({{'request':r,'response':p.stdout}})+'\\n')\nsys.stdout.write(p.stdout)\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(executable),json!(log))).unwrap();
            fs::set_permissions(&proxy,fs::Permissions::from_mode(0o700)).unwrap();
            intake_db(&a).execute("UPDATE machines SET peer=json_set(peer,'$.executable',?1) WHERE name='beta'",[proxy.to_str().unwrap()]).unwrap();
            b.queued_mail("from-beta",a.ship);
            a.collect(); a.assert_mail("from-beta");
            // The initiator is itself a real old/new CLI process; the linked
            // peer is the opposite real executable behind the logging proxy.
            let lines = fs::read_to_string(&log).unwrap();
            let mut checked = false;
            for line in lines.lines() {
                let entry: Value = serde_json::from_str(line).unwrap();
                if entry["request"]["command"] != "fleet.collect" { continue; }
                let request = &entry["request"]["args"];
                let response: Value = serde_json::from_str(entry["response"].as_str().unwrap().trim()).unwrap();
                assert!(response.get("error").is_none(),"{response}");
                let response = &response["result"];
                assert_eq!(request.get("desk_intake_replay_v1").is_some(),old_peer != "alpha");
                assert_eq!(response.get("desk_intake_replay_v1").is_some(),old_peer != "beta");
                for body in [request,response] {
                    if let Some(frame) = body["desk_intake"].as_object() {
                        assert!(!frame.contains_key("proof") && !frame.contains_key("replay"),"strict old frame: {frame:?}");
                    }
                    if let Some(evidence) = body["desk_intake_proof"].as_object() {
                        assert!(!evidence.contains_key("witnesses") && !evidence.contains_key("mail_upper"));
                    }
                }
                checked = true;
            }
            assert!(checked,"no real collect observed for owner={owner}, old={old_peer}");
            // No client, reader, reverse link or new route was admitted.
            assert_eq!(intake_db(&b).query_row("SELECT role FROM machines WHERE name='alpha'",[],|r|r.get::<_,String>(0)).unwrap(),"collected");
        }
    }
}
