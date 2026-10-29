// Source proof metadata rides the existing one-way transport. No aggregate
// reader, reverse connection, owner inference or production clock is added.
fn intake_received_proof(desk: &Machine, source: &str) -> Option<Value> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = intake_db(desk).query_row(
        "SELECT body FROM desk_intake_proof WHERE source=?1 AND valid=1",[source],|r|r.get(0)).optional().unwrap().flatten();
    raw.map(|raw|serde_json::from_str(&raw).unwrap())
}

#[test]
fn intake_barrier_vectors_cross_real_gateways_revalidate_and_preserve_old_peer_mail() {
    use std::os::unix::fs::PermissionsExt;
    for owner in ["alpha","beta"] {
        let (t,a,b)=pair();
        let a=a.with_intake_budget(10_000);
        let b=b.with_intake_budget(10_000);
        let (desk,source)=if owner=="alpha" {(&a,&b)} else {(&b,&a)};
        // Two homes exercise quiet revalidation without rotating the existing
        // contact capture cursor. Both are in the explicit bootstrap manifest.
        let _extra = Machine::new(source.root.clone(),"gamma");
        intake_notice(source,"proof-old-unread");
        intake_enroll(&a,owner);
        let core=intake_external_source(desk);
        // Each iteration does one explicit idle census and legal protocol
        // contacts. This is bounded protocol progress, never a wall-time wait.
        for _ in 0..8 {
            a.collect();
            intake_idle(source);
            a.collect();
            if intake_received_proof(desk,&core).is_some() { break; }
        }
        let proof=intake_received_proof(desk,&core).unwrap_or_else(|| {
            let saved: Vec<String>=intake_db(source).prepare("SELECT proof FROM desk_source_reconciliation").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            let loss: Vec<String>=intake_db(source).prepare("SELECT state FROM desk_intake_loss").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            panic!("{owner}: no received proof; saved={saved:?}; loss={loss:?}")
        });
        assert_eq!(proof["version"],1);
        assert_eq!(proof["vector"]["candidate"],true);
        assert_eq!(proof["checkpoint"]["source"],core);
        let cp=&proof["checkpoint"];
        let matched: bool=intake_db(desk).query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_checkpoint WHERE source=?1 AND binding=?2 AND barrier=?3 AND upper_seq=?4 AND through_seq=upper_seq)",rusqlite::params![core,cp["binding"].as_str().unwrap(),cp["id"].as_str().unwrap(),cp["upper"].as_i64().unwrap()],|r|r.get(0)).unwrap();
        assert!(matched,"vector is tied to the advertised completed barrier");
        assert!(intake_fact_ids(desk).contains(&"proof-old-unread".to_string()));
        assert!(!proof.to_string().contains(t.path().to_str().unwrap()),"no home paths on the wire");
        assert_eq!(desk.status()["coverage_complete"],false);
        // Settle the real final receipt before measuring both registries in
        // both desk directions. Count writes, not elapsed time/audit totals.
        a.collect(); a.collect();
        for machine in [source,desk] {
            let db=intake_db(machine);
            db.execute_batch("CREATE TABLE proof_quiet_writes(n INTEGER); INSERT INTO proof_quiet_writes VALUES(0)").unwrap();
            for table in ["desk_intake_transport","desk_source_proof","desk_intake_proof","desk_source_event","desk_source_head","desk_source_cleanup","desk_source_barrier","desk_source_issuance","desk_intake_gap","desk_intake_loss","desk_source_revision","desk_source_proof_work","desk_intake_audit"] {
                for event in ["INSERT","UPDATE","DELETE"] {
                    db.execute_batch(&format!("CREATE TRIGGER quiet_{table}_{event} AFTER {event} ON {table} BEGIN UPDATE proof_quiet_writes SET n=n+1; END")).unwrap();
                }
            }
        }
        for _ in 0..5 { a.collect(); }
        for machine in [source,desk] {
            assert_eq!(intake_db(machine).query_row("SELECT n FROM proof_quiet_writes",[],|r|r.get::<_,i64>(0)).unwrap(),0,"{owner}: quiet multi-home collect writes no optional source/desk state");
        }
        // A later mail transaction invalidates the saved census. New pages
        // still flow, but cannot reuse the dated vector as a fresh proof.
        intake_notice(source,"proof-post-census");
        a.collect(); a.collect();
        assert!(intake_received_proof(desk,&core).is_none());
        intake_until(&a,desk,"proof-post-census");
        for _ in 0..6 {
            intake_idle(source); a.collect(); a.collect();
            if intake_received_proof(desk,&core).is_some() { break; }
        }
        assert!(intake_received_proof(desk,&core).is_some());
        // Local-source idle frames must invalidate proof even with no new
        // source event/page. A newly retained missing home is not observed.
        let local_core: String=intake_db(desk).query_row("SELECT id FROM door_reader_identity WHERE only=1",[],|r|r.get(0)).unwrap();
        for _ in 0..6 {
            intake_idle(desk);
            if intake_received_proof(desk,&local_core).is_some() { break; }
        }
        assert!(intake_received_proof(desk,&local_core).is_some(),"local source should also bind its vector");
        let unavailable=t.path().join("unavailable-local-alpha");
        intake_db(desk).execute("INSERT INTO desk_home_inventory(home) VALUES(?1)",[unavailable.to_string_lossy().as_ref()]).unwrap();
        intake_idle(desk);
        assert!(intake_received_proof(desk,&local_core).is_none(),"empty/error local frames cannot retain old proof eligibility");
        assert_eq!(desk.status()["coverage_complete"],false);
        // Model a b2b-i peer's exact strict Frame boundary while forwarding all
        // real gateway mail/page operations. Only the new outer sidecar is
        // unavailable. An accidental new field inside Frame fails this shim.
        let proxy=t.path().join("b2b-i-gateway");
        let log=t.path().join("legacy-intake-frames.jsonl");
        fs::write(&proxy,format!("#!{}\nimport json,subprocess,sys\nr=json.loads(sys.stdin.readline())\na=r.get('args',{{}})\na.pop('desk_intake_proof',None)\nf=a.get('desk_intake')\nif isinstance(f,dict) and 'status' not in f:\n assert set(f)<=set(['protocol','binding','source','direction','role','page','ack','complete']),f\n with open({},'a') as out: out.write(json.dumps(f)+'\\n')\np=subprocess.run([{}]+sys.argv[1:],input=json.dumps(r)+'\\n',text=True,capture_output=True)\nfor line in p.stdout.splitlines():\n v=json.loads(line)\n if isinstance(v.get('result'),dict): v['result'].pop('desk_intake_proof',None)\n print(json.dumps(v))\nsys.stderr.write(p.stderr)\nsys.exit(p.returncode)\n",support::python().display(),json!(log.to_str().unwrap()),json!(support::BIN))).unwrap();
        fs::set_permissions(&proxy,fs::Permissions::from_mode(0o755)).unwrap();
        let raw: String=intake_db(&a).query_row("SELECT peer FROM machines WHERE name='beta'",[],|r|r.get(0)).unwrap();
        let mut peer: Value=serde_json::from_str(&raw).unwrap(); peer["executable"]=json!(proxy);
        intake_db(&a).execute("UPDATE machines SET peer=?1 WHERE name='beta'",[peer.to_string()]).unwrap();
        b.queued_mail("mail-through-b2b-i-peer",a.ship);
        a.collect(); a.assert_mail("mail-through-b2b-i-peer");
        assert!(intake_received_proof(desk,&core).is_none(),"missing sidecar cannot retain eligibility");
        let dated: String=intake_db(desk).query_row("SELECT body FROM desk_intake_proof WHERE source=?1",[&core],|r|r.get(0)).unwrap();
        assert!(!dated.is_empty(),"retain last dated vector when the peer becomes unsupported");
        assert!(!fs::read_to_string(log).unwrap().is_empty());
        intake_notice(source,"page-through-b2b-i-peer");
        intake_until(&a,desk,"page-through-b2b-i-peer");
        assert_eq!(desk.status()["coverage_complete"],false);
        assert_eq!(intake_db(&b).query_row("SELECT role FROM machines WHERE name='alpha'",[],|r|r.get::<_,String>(0)).unwrap(),"collected");
    }
}
