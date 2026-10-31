//! External restore fence tests run in a child test process so environment
//! changes cannot race unrelated registry tests. No production dispatch hook.
use super::*;

#[test]
fn certified_local_owner_and_restore_fence() {
    const CHILD: &str = "PRIVATEER_SUBSCRIPTION_FENCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "door::subscription::tests::fence_tests::certified_local_owner_and_restore_fence",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("PRIVATEER_FLEET", root.join("alpha.sqlite"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed;"),
            "{stdout}"
        );
        return;
    }
    let f = Fixture::new();
    let expected = Owner::of(&f.m);
    let before = f.conn.total_changes();
    assert_eq!(
        execute(&f.conn, &f.c, &expected, Operation::Open)["ok"],
        false
    );
    assert_eq!(f.conn.total_changes(), before);
    f.conn
        .execute_batch(
            "CREATE TABLE door_reader_identity(only INTEGER PRIMARY KEY,id TEXT NOT NULL);",
        )
        .unwrap();
    f.conn
        .execute("INSERT INTO door_reader_identity VALUES(1,?1)", [CORE])
        .unwrap();
    let proposal = json!({"proposal_id":uuid(),"manifest_hash":crate::core::hash(&serde_json::to_value(&f.m).unwrap()),"manifest":f.m,"decision":"alpha-decision"});
    let certificate = json!({"proposal":proposal,"accepted":[CORE]});
    let record = json!({"proposal":proposal,"local_role":"owner","intake_direction":"local","approval":true,"accepted":[CORE],"certificate":certificate,"state":"active"});
    let guard = json!({"proposal":proposal,"certificate":certificate});
    let guard_path = crate::fleet::registry_path()
        .unwrap()
        .with_extension("desk-placement-owner.json");
    std::fs::write(&guard_path, serde_json::to_vec(&guard).unwrap()).unwrap();
    f.conn
        .execute(
            "INSERT INTO desk_placement_record(domain,proposal,record) VALUES(?1,?2,?3)",
            params![
                f.m.domain,
                proposal["proposal_id"].as_str(),
                serde_json::to_string(&record).unwrap()
            ],
        )
        .unwrap();
    f.add("old-unread", 1, "alpha", None);
    let opened = execute(&f.conn, &f.c, &expected, Operation::Open);
    assert_eq!(opened["ok"], true, "{opened}");
    let sub = opened["subscription"].as_str().unwrap().to_owned();
    let page = execute(
        &f.conn,
        &f.c,
        &expected,
        Operation::Pull {
            subscription: sub.clone(),
            continuation: None,
            items: 50,
        },
    );
    assert_eq!(ids(&page).len(), 1);
    // Exercise the actual closed boundary, not only run_admitted's error:
    // an oversized item wins over the thread's row/window limit and exposes
    // no counts, refs or routing hints.
    for i in 2..=snapshot::PAGE_ROWS + 1 {
        f.add(&format!("mail-{i}"), i as i64, "alpha", None);
    }
    f.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','old-unread')",
            [],
        )
        .unwrap();
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1) WHERE id='old-unread'", ["x".repeat(16 * 1024 * 1024)]).unwrap();
    let counts = (
        f.count("desk_subscription_snapshot"),
        f.count("desk_subscription_receipt"),
        f.count("desk_subscription_ref"),
    );
    let refused = execute(
        &f.conn,
        &f.c,
        &expected,
        Operation::Thread {
            subscription: sub.clone(),
            origin: MailIdentity::new(CORE, BOX, "old-unread").unwrap(),
            continuation: None,
            items: ITEMS,
        },
    );
    let closed = coverage::unknown_domain();
    assert_eq!(
        refused,
        json!({"ok":false,"error":"oversized_item","coverage":closed})
    );
    coverage::validate(&refused["coverage"], &[]).unwrap();
    assert_eq!(
        counts,
        (
            f.count("desk_subscription_snapshot"),
            f.count("desk_subscription_receipt"),
            f.count("desk_subscription_ref"),
        )
    );
    let mut stale = expected.clone();
    stale.incarnation = uuid();
    assert_eq!(
        execute(
            &f.conn,
            &f.c,
            &stale,
            Operation::Resume {
                subscription: sub.clone()
            }
        )["ok"],
        false
    );
    let mut bad_guard = guard.clone();
    bad_guard["proposal"]["manifest"]["incarnation"] = json!(uuid());
    std::fs::write(&guard_path, serde_json::to_vec(&bad_guard).unwrap()).unwrap();
    assert_eq!(
        execute(
            &f.conn,
            &f.c,
            &expected,
            Operation::Ack {
                subscription: sub.clone(),
                key: page["ack_key"].as_str().unwrap().into(),
                refs: references(&page)
            }
        )["ok"],
        false
    );
    assert_eq!(f.count("desk_subscription_progress"), 0);
    std::fs::write(&guard_path, serde_json::to_vec(&guard).unwrap()).unwrap();
    // A matching certificate still cannot turn a remote source into an owner.
    f.conn
        .execute("UPDATE door_reader_identity SET id=?1", [uuid()])
        .unwrap();
    assert_eq!(
        execute(
            &f.conn,
            &f.c,
            &expected,
            Operation::Resume { subscription: sub }
        )["ok"],
        false
    );
}
