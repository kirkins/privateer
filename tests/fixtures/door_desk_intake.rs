// Transport matrix with a real v2 adapter/supervisor on each side. Aggregate
// readers are deliberately not activated by intake; ordinary sends still work.
#[test]
fn intake_one_way_desk_and_door_matrix_keeps_existing_send_and_selected_scope() {
    for owner in ["alpha", "beta"] {
        for door_side in ["alpha", "beta"] {
            let alpha = Fleet::with_ships(&[("survey", true)]);
            let beta = Fleet::with_ships(&[("relay", true)]);
            let run = |f: &Fleet, verb: &str, args: Value| -> Value {
                let home = f.home(if f.root == alpha.root { "survey" } else { "relay" });
                serde_json::from_str(&f.ok(&["--home",home.to_str().unwrap(),verb,"--json",&args.to_string()])).unwrap()
            };
            run(&alpha,"machine.add",json!({"name":"beta","target":"local-test","executable":BIN,
                "as":"alpha","fleet":beta.root.join("state/fleet.sqlite"),"transport":"local"}));
            let (source,source_ship,desk) = if owner == "alpha" {(&beta,"relay",&alpha)} else {(&alpha,"survey",&beta)};
            let notice = source.call(source_ship,"mail.send",json!({"id":"snapshot-source","from":source_ship,"to":"captain","kind":"notice","body":"Uncollected source observation"}));
            assert_eq!(notice["id"],"snapshot-source");
            run(&alpha,"machine.desk.propose",json!({"domain":"survey-domain","owner":owner,"sources":["alpha","beta"]}));
            let status = run(&alpha,"machine.desk.status",json!({}));
            let p = &status["records"][0]["proposal"];
            run(&alpha,"machine.desk.accept",json!({"proposal_id":p["proposal_id"],"manifest_hash":p["manifest_hash"]}));
            run(&alpha,"machine.desk.status",json!({}));
            let db = rusqlite::Connection::open(desk.root.join("state/fleet.sqlite")).unwrap();
            let has_source = || db.query_row("SELECT COUNT(*) FROM desk_intake_fact WHERE id='snapshot-source'",[],|r|r.get::<_,i64>(0)).unwrap();
            assert_eq!(has_source(),0);
            for _ in 0..5 {
                run(&alpha,"machine.collect",json!({}));
                if has_source() == 1 { break; }
            }
            assert_eq!(has_source(),1,"{owner}/{door_side}");
            assert_eq!(run(desk,"machine.desk.status",json!({}))["coverage_complete"],false);
            let door = if door_side == "alpha" {&alpha} else {&beta};
            let ship = if door_side == "alpha" {"survey"} else {"relay"};
            delegated_adapter(door);
            let (_,token) = delegated_issue(door,"survey-client","ship:message,mail:read,fleet:read",Some(ship));
            let supervisor = door.supervise();
            door.wait("intake matrix hello",|e|e.first().cloned());
            let sent = delegated_ok(door,&token,"send",json!({"to":ship,"kind":"notice","body":"Ordinary captain send after intake"}));
            assert!(sent["mail"]["id"].is_string(),"{sent}");
            assert_eq!(sent["mail"]["from"],"captain");
            // Existing selected-mailbox v2 reads stay available. Intake does
            // not turn them into PR 3 aggregate subscriptions or a fleet-wide
            // completeness claim, and it does not create a reverse route.
            let inbox = delegated_ok(door,&token,"inbox",json!({"ship":ship}));
            assert_eq!(inbox["coverage"]["scope"],"selected-mailbox","{inbox}");
            assert_eq!(inbox["coverage"]["other_machines"],"not_queried","{inbox}");
            assert_eq!(run(desk,"machine.desk.status",json!({}))["subscriptions"],false);
            stop(supervisor);
            assert_eq!(db.query_row("SELECT role FROM machines WHERE name=?1",[if owner == "alpha" {"beta"} else {"alpha"}],|r|r.get::<_,String>(0)).unwrap(),if owner == "alpha" {"collects"} else {"collected"});
        }
    }
}
