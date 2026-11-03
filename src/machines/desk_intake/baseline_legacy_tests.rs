use super::*;

#[test]
fn retired_pre_source_home_upgrades_mail_and_wal_then_publishes_without_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha");
    fs::create_dir(&path).unwrap();
    let h = Connection::open(path.join("privateer.sqlite")).unwrap();
    // Pre-source mail schema: no newer lifecycle/delivery columns and rollback
    // journaling. Never open a Store: that would hide the retired-home failure.
    h.execute_batch(
        "CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
         CREATE TABLE mail_cursor(address TEXT PRIMARY KEY,through_seq INTEGER NOT NULL);
         CREATE TABLE mail(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,
           sender TEXT NOT NULL,recipient TEXT NOT NULL,kind TEXT NOT NULL,correlation TEXT,
           task_id TEXT,request_kind TEXT,body TEXT NOT NULL,payload TEXT NOT NULL,
           urgency TEXT NOT NULL,deadline_ms INTEGER,created_ms INTEGER NOT NULL,
           state TEXT,answered_by TEXT,answered_ms INTEGER);
         INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state)
         VALUES('legacy','survey','captain','request','Retained report','{}','normal',1,'open');",
    )
    .unwrap();
    source::init(&h).unwrap();
    let r = registry();
    let m = manifest();
    let bound = format!("{}:{SOURCE}", binding(&m).unwrap());
    assert!(
        enrollment::begin(&h, &bound)
            .unwrap_err()
            .to_string()
            .contains("requires WAL")
    );
    assert!(
        h.prepare(&format!("SELECT {} FROM mail", crate::mail::COLUMNS))
            .is_err()
    );
    r.execute(
        "INSERT INTO desk_home_inventory(home) VALUES(?1)",
        [path.to_string_lossy().as_ref()],
    )
    .unwrap();
    r.execute("INSERT INTO desk_baseline_home(domain,source,home,state,attempted_ms) VALUES(?1,?2,?3,'pending',0)", params![m.domain,SOURCE,path.to_string_lossy()]).unwrap();
    enroll(&path, &r, &m, enrollment::RETRY_MS).unwrap();
    assert_eq!(
        r.query_row("SELECT state FROM desk_baseline_home", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "published"
    );
    assert!(enrollment::published(&h).unwrap());
    let baseline = enrollment::open(&path, false).unwrap();
    assert_eq!(count(&baseline, "mail_source_import"), 1);
    drain(&path, &h, &r, &m);
    assert_eq!(count(&r, "desk_home_receipt"), 1);
    assert_eq!(count(&r, "desk_intake_loss"), 0);
    assert_eq!(count(&h, "mail"), 1);
    // Even after baseline import is quiet, deletion must release its unresolved
    // receipt. No new imports or home startup should be needed to trigger it.
    h.execute("DELETE FROM mail WHERE id='legacy'", []).unwrap();
    for _ in 0..3 {
        home::capture_ready_until(
            &path,
            &h,
            &r,
            &m,
            SOURCE,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
    }
    assert_eq!(count(&r, "desk_home_receipt"), 0);
    assert_eq!(count(&baseline, "mail_source_import"), 0);
}
