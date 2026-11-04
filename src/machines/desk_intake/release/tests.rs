use super::*;
use crate::store::Store;

fn setup() -> (tempfile::TempDir, Connection, Value) {
    let dir = tempfile::tempdir().unwrap();
    let c = Connection::open(dir.path().join("registry.sqlite")).unwrap();
    crate::machines::init(&c).unwrap();
    c.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
        .unwrap();
    let home = dir.path().join("alpha");
    c.execute(
        "INSERT INTO ships VALUES('alpha',?1,1)",
        [home.to_str().unwrap()],
    )
    .unwrap();
    let args = json!({"home":home,"ship":"alpha","operation":"release","reason":"Retired test home is absent"});
    (dir, c, args)
}
fn bound(c: &Connection, args: &Value) -> Value {
    let mut args = args.clone();
    args["digest"] = plan(c, &args).unwrap()["digest"].clone();
    args
}

#[test]
fn refuses_active_present_symlink_corrupt_and_agent_and_planning_is_read_only() {
    for kind in [
        "active",
        "present",
        "symlink",
        "corrupt",
        "directory",
        "agent",
    ] {
        let (_dir, c, args) = setup();
        let home = Path::new(args["home"].as_str().unwrap());
        match kind {
            "active" => {
                c.execute("UPDATE ships SET retired_ms=NULL", []).unwrap();
            }
            "present" => {
                Store::open(home).unwrap();
            }
            "symlink" => {
                std::fs::create_dir(home).unwrap();
                std::os::unix::fs::symlink(
                    home.join("absent.sqlite"),
                    home.join("privateer.sqlite"),
                )
                .unwrap();
            }
            "corrupt" => {
                std::fs::create_dir(home).unwrap();
                std::fs::write(home.join("privateer.sqlite"), b"corrupt").unwrap();
            }
            "directory" => {
                std::fs::create_dir_all(home.join("privateer.sqlite")).unwrap();
            }
            _ => (),
        }
        let before = c.total_changes();
        c.pragma_update(None, "query_only", true).unwrap();
        let args = bound(&c, &args);
        assert_eq!(before, c.total_changes());
        c.pragma_update(None, "query_only", false).unwrap();
        let caller = if kind == "agent" {
            Caller::Agent {
                actor: "alpha".into(),
            }
        } else {
            Caller::Operator
        };
        assert!(apply(&c, &caller, "release", &args).is_err(), "{kind}");
        assert!(
            !c.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_release)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap()
        );
    }
}

#[test]
fn digest_binds_registry_ship_home_and_both_filesystem_checks() {
    let (_dir, c, args) = setup();
    let original = bound(&c, &args);
    c.execute("INSERT INTO desk_home_inventory VALUES('beta')", [])
        .unwrap();
    assert!(
        apply(&c, &Caller::Operator, "release", &original)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
    );
    let args = bound(&c, &args);
    let mut wrong = args.clone();
    wrong["ship"] = json!("beta");
    assert!(apply(&c, &Caller::Operator, "release", &wrong).is_err());
    let home = Path::new(args["home"].as_str().unwrap());
    Store::open(home).unwrap();
    assert!(apply(&c, &Caller::Operator, "release", &args).is_err());
    std::fs::remove_file(home.join("privateer.sqlite")).unwrap();
    // Force reappearance at the registry transaction boundary, after plan
    // validation. The filesystem recheck rolls back all release evidence.
    let db = home.join("privateer.sqlite");
    c.update_hook(Some(
        move |_: rusqlite::hooks::Action, _: &str, table: &str, _: i64| {
            if table == "desk_home_release" {
                std::fs::write(&db, b"corrupt").unwrap();
            }
        },
    ));
    let args = bound(&c, &args);
    assert!(apply(&c, &Caller::Operator, "release", &args).is_err());
    assert!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_home_loss_boundary)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}

#[test]
fn legacy_loss_writes_bump_epoch_without_rewriting_loss_and_init_repairs_triggers() {
    let (dir, c, _args) = setup();
    let old = Connection::open(dir.path().join("registry.sqlite")).unwrap();
    old.execute(
        "INSERT INTO desk_intake_loss VALUES('alpha','beta','integrity_gap')",
        [],
    )
    .unwrap();
    let epoch = || {
        c.query_row(
            "SELECT epoch FROM desk_intake_loss_epoch WHERE domain='alpha' AND source='beta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    let first = epoch();
    old.execute("INSERT INTO desk_intake_loss VALUES('alpha','beta','integrity_gap') ON CONFLICT(domain,source) DO UPDATE SET state=excluded.state", []).unwrap();
    assert!(epoch() > first);
    let before = epoch();
    old.execute("INSERT INTO desk_intake_loss VALUES('alpha','beta','history_gap') ON CONFLICT(domain,source) DO UPDATE SET state=excluded.state WHERE state<>'integrity_gap'", []).unwrap();
    assert!(
        epoch() > before,
        "even a sticky-loss no-op upsert bumps the epoch"
    );
    c.execute_batch("DROP TRIGGER desk_loss_insert_epoch")
        .unwrap();
    super::super::init(&c).unwrap();
    assert!(ready(&c).unwrap());
    assert_eq!(
        c.query_row("SELECT state FROM desk_intake_loss", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "integrity_gap"
    );
    let before = epoch();
    old.execute("UPDATE desk_intake_loss SET state=state", [])
        .unwrap();
    assert!(epoch() > before);
}

#[test]
fn release_and_readmit_keep_dated_boundary_and_audits() {
    let (_dir, c, args) = setup();
    let home = Path::new(args["home"].as_str().unwrap());
    c.execute(
        "INSERT INTO desk_home_missing VALUES(?1,123)",
        [home.to_str().unwrap()],
    )
    .unwrap();
    let key = crate::core::hash(&json!(home));
    c.execute(
        "INSERT INTO desk_source_reconciliation VALUES('alpha','beta',?1)",
        [json!({"homes":[{"home":key,"unread":7,"open":3}]}).to_string()],
    )
    .unwrap();
    apply(&c, &Caller::Operator, "release", &bound(&c, &args)).unwrap();
    let row: (i64, Option<i64>, Option<i64>) = c
        .query_row(
            "SELECT first_seen_missing_ms,unread,open FROM desk_home_release",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (123, Some(7), Some(3)));
    assert_eq!(home::census(&c, &[]).unwrap().homes[0].state, "released");
    assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM desk_home_loss_boundary WHERE actor='captain' AND released_ms>=first_seen_missing_ms)", [], |r| r.get::<_, bool>(0)).unwrap());
    Store::open(home).unwrap();
    assert_eq!(home::census(&c, &[]).unwrap().homes[0].state, "reappeared");
    let mut args = args;
    args["operation"] = json!("readmit");
    let args = bound(&c, &args);
    assert!(
        apply(
            &c,
            &Caller::Agent {
                actor: "alpha".into()
            },
            "readmit",
            &args
        )
        .is_err()
    );
    apply(&c, &Caller::Operator, "readmit", &args).unwrap();
    assert_eq!(home::census(&c, &[]).unwrap().homes[0].state, "retained");
    for stage in ["home.release", "home.readmit"] {
        assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage=?1 AND json_extract(body,'$.actor')='captain')", [stage], |r| r.get::<_, bool>(0)).unwrap());
    }
    assert!(
        c.query_row(
            "SELECT EXISTS(SELECT 1 FROM desk_home_loss_boundary WHERE unread=7 AND open=3)",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
}
