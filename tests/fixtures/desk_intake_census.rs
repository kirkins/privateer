#[test]
fn intake_census_records_failed_retained_homes_without_blocking_real_gateway_mail() {
    for owner in ["alpha","beta"] {
        let (t,a,b)=pair();
        intake_enroll(&a,owner);
        let source=if owner=="alpha" {&b} else {&a};
        let missing=t.path().join("missing-home");
        let unreadable=t.path().join("unreadable-home");
        fs::create_dir_all(&unreadable).unwrap();
        fs::write(unreadable.join("privateer.sqlite"),"not a SQLite database").unwrap();
        for path in [&missing,&unreadable] {
            intake_db(source).execute("INSERT INTO desk_home_inventory(home) VALUES(?1)",[path.to_string_lossy().as_ref()]).unwrap();
        }
        intake_idle(source); // Enrollment failure must not skip the census.
        let raw: String=intake_db(source).query_row("SELECT proof FROM desk_source_reconciliation WHERE source=(SELECT id FROM door_reader_identity WHERE only=1)",[],|r|r.get(0)).unwrap();
        let proof: Value=serde_json::from_str(&raw).unwrap();
        assert_eq!(proof["history_proved"],false);
        for state in ["missing","unreadable"] {
            assert!(proof["homes"].as_array().unwrap().iter().any(|h|h["state"]==state),"{proof}");
        }
        assert!(!raw.contains(missing.to_str().unwrap()));
        intake_db(source).execute("INSERT INTO desk_home_inventory_gap VALUES(1,'history_gap')",[]).unwrap();
        intake_idle(source);
        let raw: String=intake_db(source).query_row("SELECT proof FROM desk_source_reconciliation",[],|r|r.get(0)).unwrap();
        let proof: Value=serde_json::from_str(&raw).unwrap();
        assert_eq!(proof["membership_history_gap"],true);
        assert_eq!(proof["membership_proved"],false);
        b.queued_mail("through-census-gap",a.ship);
        a.collect();
        a.assert_mail("through-census-gap");
        assert_eq!(a.status()["coverage_complete"],false);
        assert_eq!(b.status()["coverage_complete"],false);
    }
}
