// Dated local-owner history and new captain-copy identities across actual
// one-way gateway subprocesses. Only source completion is enabled; aggregate
// readers remain off.
#[test]
fn intake_local_owner_history_and_explicit_captain_copies_in_both_directions() {
    for owner in ["alpha","beta"] {
        let (_t,a,b)=completion_clock_pair();
        let (desk,source)=if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        intake_notice(source,"old-unread-notice");
        source.ok("mail.send",json!({"id":"old-open-request","from":source.ship,"to":"captain","kind":"request","body":"Old retained request"}));
        intake_enroll(&a,owner);
        desk.ok("mail.send",json!({"id":"owner-local-outgoing","from":"captain","to":desk.ship,"kind":"notice","body":"Local-only captain observation"}));
        intake_idle(desk);
        assert!(intake_fact_ids(desk).contains(&"owner-local-outgoing".to_string()));
        for _ in 0..10 {
            a.collect();
            if owner=="beta" {
                // Source calls the owner on its existing placement-driven push.
                intake_db(&a).execute("UPDATE machines SET next_contact_ms=0",[]).unwrap();
                let pass=a.run(&["serve","--json","{\"once\":true}"]);
                assert!(pass.status.success(),"{}",String::from_utf8_lossy(&pass.stderr));
            }
            let copied: bool=intake_home(desk).query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE id='old-unread-notice')",[],|r|r.get(0)).unwrap();
            if copied && intake_fact_ids(desk).contains(&"old-open-request".to_string()) { break; }
        }
        for id in ["old-unread-notice","old-open-request"] {
            desk.assert_mail(id);
            let original: String=intake_home(source).query_row("SELECT provenance FROM mail_source_origin WHERE id=?1",[id],|r|r.get(0)).unwrap();
            let copy: String=intake_home(desk).query_row("SELECT provenance FROM mail_source_origin WHERE id=?1",[id],|r|r.get(0)).unwrap();
            let original: Value=serde_json::from_str(&original).unwrap();
            let copy: Value=serde_json::from_str(&copy).unwrap();
            assert_eq!(copy["Copy"],original["Origin"],"{owner}/{id}");
            assert_eq!(intake_fact_ids(desk).iter().filter(|i|i.as_str()==id).count(),1);
        }
        let open: String=intake_home(source).query_row("SELECT state FROM mail WHERE id='old-open-request'",[],|r|r.get(0)).unwrap();
        assert_eq!(open,"open","projection must not close the original");
        source.ok("mail.send",json!({"id":"source-answer","from":"captain","to":source.ship,"kind":"reply","correlation":"old-open-request","body":"Answered at the original home"}));
        for _ in 0..10 {
            a.collect();
            if owner=="beta" {
                intake_db(&a).execute("UPDATE machines SET next_contact_ms=0",[]).unwrap();
                let pass=a.run(&["serve","--json","{\"once\":true}"]);
                assert!(pass.status.success(),"{}",String::from_utf8_lossy(&pass.stderr));
            }
            let closed: bool=intake_db(desk).query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_fact WHERE id='old-open-request' AND state='closed')",[],|r|r.get(0)).unwrap();
            if closed && intake_fact_ids(desk).contains(&"source-answer".to_string()) { break; }
        }
        assert!(intake_fact_ids(desk).contains(&"source-answer".to_string()));
        assert_eq!(intake_db(desk).query_row("SELECT state FROM desk_intake_fact WHERE id='old-open-request'",[],|r|r.get::<_,String>(0)).unwrap(),"closed");
        if owner=="beta" {
            desk.assert_mail("source-answer");
            let copy: String=intake_home(desk).query_row("SELECT provenance FROM mail_source_origin WHERE id='source-answer'",[],|r|r.get(0)).unwrap();
            let original: String=intake_home(source).query_row("SELECT provenance FROM mail_source_origin WHERE id='source-answer'",[],|r|r.get(0)).unwrap();
            let copy: Value=serde_json::from_str(&copy).unwrap();
            let original: Value=serde_json::from_str(&original).unwrap();
            assert_eq!(copy["Copy"],original["Origin"],"closure replies preserve explicit origin");
        }
        // Assert the desk's own saved proof only after owner-local ingestion
        // completes below: one bounded idle unit gives owner-local transport
        // at most 75 ms, so an early read can legitimately still show
        // baseline_pending or delta_pending homes instead of "imported".
        intake_idle(desk);
        let copies: Vec<String>=intake_db(desk).prepare("SELECT event FROM desk_home_copy").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        for raw in copies {
            let copy: Value=serde_json::from_str(&raw).unwrap();
            let origin=&copy["provenance"]["Copy"];
            let content: String=intake_db(desk).query_row("SELECT content FROM desk_intake_fact WHERE core=?1 AND mailbox=?2 AND id=?3",rusqlite::params![origin["core"].as_str().unwrap(),origin["mailbox"].as_str().unwrap(),origin["id"].as_str().unwrap()],|r|r.get(0)).unwrap();
            let mut origin_content: Value=serde_json::from_str(&content).unwrap();
            let mut copy_content=copy["content"].clone();
            // Ordinary delivery keeps its local timestamp. Proof compares
            // semantic content without changing either home or origin row.
            origin_content.as_object_mut().unwrap().remove("created_ms");
            copy_content.as_object_mut().unwrap().remove("created_ms");
            assert_eq!(copy_content,origin_content,"{owner}: real desk copy must preserve immutable semantic origin content");
        }
        let core=intake_external_source(desk);
        completion_settle(&a,desk,source,&core);
        // Explicit desk copies reconcile against the authoritative projection,
        // never replace its lifecycle and never add a duplicate origin fact.
        let local=desk.row()["proposal"]["manifest"]["owner_core"].as_str().unwrap().to_owned();
        for _ in 0..8 {
            intake_idle(desk);
            if completion_source(desk,&local)["complete"]==true { break; }
        }
        let local_status=completion_source(desk,&local);
        let reconciliation: String=intake_db(desk).query_row("SELECT proof FROM desk_source_reconciliation WHERE source=?1",[&local],|r|r.get(0)).unwrap();
        let received=intake_received_proof(desk,&local);
        assert_eq!(local_status["complete"],true,"{owner}: status={local_status}; saved={reconciliation}; received={received:?}");
        // The desk's own saved proof (its door_reader_identity is this owner
        // core) reaches an "imported" home only once the bounded owner-local
        // transport above finished; the early read raced those pending states.
        let proof: Value=serde_json::from_str(&reconciliation).unwrap();
        assert!(proof["homes"].as_array().unwrap().iter().any(|h|h["state"]=="imported"),"{owner}: saved proof never reached an imported home: {proof}");
        assert_eq!(intake_db(desk).query_row("SELECT COUNT(*) FROM desk_intake_fact WHERE id='old-unread-notice'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(desk.status()["coverage_complete"],false);
    }
}

#[test]
fn intake_direct_mail_deliver_peer_passes_copy_fields_to_the_real_gateway() {
    use std::os::unix::fs::PermissionsExt;
    let (t,a,b)=pair();
    let proxy=t.path().join("pass-through-gateway");
    let log=t.path().join("delivered-requests.jsonl");
    // Unlike the legacy shim, forward every argument byte-for-byte. Record
    // the actual mail.deliver request to prove the optional fields reached
    // the real binary through its configured local peer transport.
    fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nraw=sys.stdin.readline()\nr=json.loads(raw)\nif r.get('command')=='mail.deliver':\n with open({},'a') as f: f.write(raw)\np=subprocess.run([{}]+sys.argv[1:],input=raw,text=True,capture_output=True)\nsys.stdout.write(p.stdout)\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(log.to_str().unwrap()),json!(support::BIN))).unwrap();
    fs::set_permissions(&proxy,fs::Permissions::from_mode(0o755)).unwrap();
    let peer_name: String=intake_home(&a).query_row("SELECT json_extract(record,'$.location.peer') FROM ships WHERE name=?1",[b.ship],|r|r.get(0)).unwrap();
    let raw: String=intake_home(&a).query_row("SELECT config_json FROM peer_endpoints WHERE name=?1",[peer_name],|r|r.get(0)).unwrap();
    let mut peer: Value=serde_json::from_str(&raw).unwrap();
    peer["executable"]=json!(proxy);
    a.ok("peer.put",peer);
    a.ok("mail.send",json!({"id":"direct-captain-copy","from":"captain","to":b.ship,"kind":"notice","body":"Direct metadata stays optional"}));
    a.ok("mail.flush",json!({}));
    b.assert_mail("direct-captain-copy");
    let requests: Vec<Value>=fs::read_to_string(log).unwrap().lines().map(|s|serde_json::from_str(s).unwrap()).collect();
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(request["command"],"mail.deliver");
        assert_eq!(request["args"]["mail_copy_v1"],true);
        assert!(request["args"]["mail_copies"]["direct-captain-copy"]["origin"].is_object(),"{request}");
    }
    let original: String=intake_home(&a).query_row("SELECT provenance FROM mail_source_origin WHERE id='direct-captain-copy'",[],|r|r.get(0)).unwrap();
    let copy: String=intake_home(&b).query_row("SELECT provenance FROM mail_source_origin WHERE id='direct-captain-copy'",[],|r|r.get(0)).unwrap();
    let original: Value=serde_json::from_str(&original).unwrap();
    let copy: Value=serde_json::from_str(&copy).unwrap();
    assert!(original["Origin"].is_object());
    assert_eq!(copy["Copy"],original["Origin"]);
    assert_eq!(intake_home(&b).query_row("SELECT COUNT(*) FROM mail WHERE id='direct-captain-copy'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
}

#[test]
fn intake_malformed_copy_sidecar_is_optional_and_payload_cannot_forge_it() {
    let (_t,a,b)=pair();
    intake_enroll(&a,"beta");
    let message=json!({"id":"bad-copy","from":a.ship,"to":"captain","kind":"notice","body":"Mail survives malformed evidence","payload":{"mail_copies":{"origin":"forged"}}});
    let row=a.row();
    let m=&row["proposal"]["manifest"];
    let tuple=json!({"domain":m["domain"],"owner_core":m["owner_core"],"generation":m["generation"],"incarnation":m["incarnation"],"manifest_hash":row["proposal"]["manifest_hash"]});
    let out=b.gateway("fleet.captain",json!({"machine":"alpha","messages":[message],"desk_owner":tuple,"mail_copy_v1":true,"mail_copies":{"bad-copy":{"origin":"malformed","envelope":"wrong"}}}));
    assert!(out.get("error").is_none(),"{out}");
    b.assert_mail("bad-copy");
    assert_eq!(intake_home(&b).query_row("SELECT provenance FROM mail_source_origin WHERE id='bad-copy'",[],|r|r.get::<_,String>(0)).unwrap(),"\"Unresolved\"");
}
