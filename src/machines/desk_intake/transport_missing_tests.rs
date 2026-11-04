use super::*;

#[test]
fn unavailable_retired_home_never_aborts_paging_or_creates_a_database() {
    for unavailable in ["missing", "corrupt", "symlink"] {
        let dir = tempfile::tempdir().unwrap();
        let retired = dir.path().join("alpha");
        std::fs::create_dir(&retired).unwrap();
        let retired_db = retired.join("privateer.sqlite");
        match unavailable {
            "corrupt" => std::fs::write(&retired_db, b"not a database").unwrap(),
            "symlink" => {
                std::os::unix::fs::symlink(dir.path().join("gamma.sqlite"), &retired_db).unwrap()
            }
            _ => (),
        }
        let path = dir.path().join("beta");
        let mut h = crate::store::Store::open(&path).unwrap();
        crate::mail::source::bind(h.connection(), SOURCE).unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        crate::mail::dispatch(&mut h, "mail.send", json!({"id":"retained","from":"survey","to":"captain","kind":"request","body":"Retained report"})).unwrap();
        let r = db();
        r.execute_batch("ALTER TABLE ships ADD COLUMN retired_ms INTEGER")
            .unwrap();
        r.execute(
            "INSERT INTO ships(home) VALUES(?1)",
            [path.to_string_lossy().as_ref()],
        )
        .unwrap();
        r.execute(
            "INSERT INTO ships(home,retired_ms) VALUES(?1,1)",
            [retired.to_string_lossy().as_ref()],
        )
        .unwrap();
        let c = contact_for_test(false);
        let deadline = Instant::now() + Duration::from_secs(10);
        super::super::baseline::enroll_home(&path, &r, &c.manifest, SOURCE, deadline, 100, || {
            Ok(())
        })
        .unwrap();
        state(&r, &c).unwrap();
        capture_one(&r, &c, deadline).unwrap(); // retired path: advance and skip
        let census = home::census(&r, &[]).unwrap();
        let missing = census.homes.iter().find(|h| h.path == retired).unwrap();
        assert!(missing.retired);
        assert_eq!(
            missing.state,
            if unavailable == "missing" {
                "missing"
            } else {
                "unreadable"
            }
        );
        assert_eq!(
            r.query_row("SELECT state FROM desk_intake_gap", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "unreadable"
        );
        capture_one(&r, &c, deadline).unwrap(); // other home still imports
        // Rotation returns to the unavailable home while producing the frame.
        let frame = source_frame(&r, &c, true, deadline).unwrap();
        let page = frame
            .page
            .expect("unavailable home must not abort the frame");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].fact.origin.id, "retained");
        if unavailable == "missing" {
            assert!(!retired_db.exists());
        } else if unavailable == "corrupt" {
            assert_eq!(std::fs::read(&retired_db).unwrap(), b"not a database");
        } else {
            assert!(
                retired_db
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(!dir.path().join("gamma.sqlite").exists());
        }
        assert_eq!(count(&r, "desk_intake_loss"), 0);
    }
}
