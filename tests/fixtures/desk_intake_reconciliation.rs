// Dated local-owner history and new captain-copy identities across actual
// one-way gateway subprocesses. Completion and aggregate readers stay off.
#[test]
fn intake_local_owner_history_and_explicit_captain_copies_in_both_directions() {
    for owner in ["alpha","beta"] {
        let (_t,a,b)=pair();
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
        intake_idle(desk);
        let raw: String=intake_db(desk).query_row("SELECT proof FROM desk_source_reconciliation WHERE source=(SELECT id FROM door_reader_identity WHERE only=1)",[],|r|r.get(0)).unwrap();
        let proof: Value=serde_json::from_str(&raw).unwrap();
        assert!(proof["homes"].as_array().unwrap().iter().any(|h|h["state"]=="imported"));
        assert_eq!(desk.status()["coverage_complete"],false);
    }
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
