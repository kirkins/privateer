use super::*;
use sha2::{Digest, Sha256};
use std::cell::RefCell;

type Hook = Box<dyn FnOnce(&Connection)>;
thread_local! { static AFTER_COMMIT: RefCell<Option<Hook>> = RefCell::new(None); }
pub(super) fn after_commit(conn: &Connection) {
    AFTER_COMMIT.with(|hook| {
        if let Some(run) = hook.borrow_mut().take() {
            run(conn);
        }
    });
}

#[test]
fn closed_read_shapes_and_canonical_identities() {
    let sub = "00000000-0000-4000-8000-000000000001";
    let origin = json!({"core":sub,"mailbox":sub,"id":"alpha"});
    for command in [
        Command::DeskOpen,
        Command::DeskResume,
        Command::DeskPull,
        Command::DeskShow,
        Command::DeskThread,
    ] {
        let mut args = if command == Command::DeskOpen {
            json!({})
        } else {
            json!({"subscription":sub})
        };
        if matches!(command, Command::DeskShow | Command::DeskThread) {
            args["origin"] = origin.clone();
        }
        assert!(operation(command, &args).is_ok());
        for key in [
            "schema",
            "grant",
            "domain",
            "owner",
            "cursor",
            "ack_key",
            "refs",
            "idempotency_key",
        ] {
            let mut bad = args.clone();
            bad[key] = json!("alpha");
            assert!(operation(command, &bad).is_err(), "{command:?} {bad}");
        }
        if command != Command::DeskOpen {
            for id in [
                "",
                "alpha",
                "00000000000040008000000000000001",
                "00000000-0000-4000-8000-00000000000A",
            ] {
                let mut bad = args.clone();
                bad["subscription"] = json!(id);
                assert!(operation(command, &bad).is_err());
            }
        }
    }
    for limit in [json!(0), json!(51), json!(1.5), json!("1"), Value::Null] {
        assert!(
            operation(
                Command::DeskPull,
                &json!({"subscription":sub,"limit":limit})
            )
            .is_err()
        );
    }
    for continuation in [json!(""), json!("x".repeat(257)), Value::Null, json!(2)] {
        assert!(
            operation(
                Command::DeskPull,
                &json!({"subscription":sub,"continuation":continuation})
            )
            .is_err()
        );
    }
    for (field, value) in [
        ("core", json!("alpha")),
        ("mailbox", json!("alpha")),
        ("id", json!("alpha/beta")),
        ("id", json!("a".repeat(161))),
        ("owner", json!("alpha")),
    ] {
        let mut bad = origin.clone();
        bad[field] = value;
        assert!(operation(Command::DeskShow, &json!({"subscription":sub,"origin":bad})).is_err());
    }
    assert!(Command::parse("desk.subscription.ack").is_none());
    for bytes in [256, 257] {
        let message = json!({"command":"desk.subscription.open","id":"a".repeat(bytes-2)});
        assert_eq!(
            crate::door::scoped::oversized_desk_id(&message),
            bytes > 256
        );
    }
    assert!(!crate::door::scoped::oversized_desk_id(
        &json!({"command":"inbox","id":"a".repeat(257)})
    ));
}

#[test]
fn credential_and_binding_races_withhold_pages_without_erasing_custody() {
    const CHILD: &str = "PRIVATEER_DESK_READ_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "door::desk_exposure::reads::tests::credential_and_binding_races_withhold_pages_without_erasing_custody", "--nocapture"])
            .env(CHILD, "1").env("PRIVATEER_FLEET", dir.path().join("alpha.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", dir.path().join("resources"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let conn = crate::fleet::registry().unwrap();
    crate::door::clients::init(&conn).unwrap();
    let core = crate::door::reader_link::machine_id_in(&conn).unwrap();
    let m: Manifest = serde_json::from_value(json!({"domain":"beta-domain","manifest_revision":1,
        "max_source_age_ms":900000,"owner_core":core,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),
        "coordinator":core,"sources":[{"core":core,"name":"alpha","ships":["alpha"],"intake_direction":"local"}]})).unwrap();
    let proposal = json!({"proposal_id":uuid::Uuid::new_v4().to_string(),"manifest_hash":crate::core::hash(&serde_json::to_value(&m).unwrap()),"manifest":m,"decision":"alpha-decision"});
    let certificate = json!({"proposal":proposal,"accepted":[core]});
    let record = json!({"proposal":proposal,"local_role":"owner","intake_direction":"local","approval":true,"accepted":[core],"certificate":certificate,"state":"active"});
    conn.execute(
        "INSERT INTO desk_placement_record VALUES(?1,?2,?3)",
        params![
            m.domain,
            proposal["proposal_id"].as_str(),
            record.to_string()
        ],
    )
    .unwrap();
    std::fs::write(
        crate::fleet::registry_path()
            .unwrap()
            .with_extension("desk-placement-owner.json"),
        json!({"proposal":proposal,"certificate":certificate}).to_string(),
    )
    .unwrap();
    coverage::install_placement_manifest(
        &conn,
        &m.domain,
        m.max_source_age_ms,
        &[(core.clone(), vec!["alpha".into()])],
    )
    .unwrap();
    install(&conn).unwrap();
    let binding = Binding {
        domain: m.domain.clone(),
        owner_core: core.clone(),
        generation: 1,
        incarnation: m.incarnation.clone(),
        manifest_revision: 1,
        manifest_hash: crate::core::hash(&serde_json::to_value(&m).unwrap()),
        mode: "fresh_replay".into(),
        replay_policy: "open_and_new".into(),
        switch_on_ms: 1,
        switch_on: vec![],
    };
    conn.execute(
        "INSERT INTO door_desk_exposure VALUES('beta',1,?1)",
        [serde_json::to_string(&binding).unwrap()],
    )
    .unwrap();
    let mailbox = uuid::Uuid::new_v4().to_string();
    conn.execute("INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending) VALUES(?1,?2,1,?3,?2,?2,?4,'alpha',1,?5,'open',1,1)",params![m.domain,core,m.incarnation,mailbox,json!({"id":"alpha","from":"alpha","to":"captain","kind":"request","body":"alpha"}).to_string()]).unwrap();
    for mutation in ["disable", "revoke", "restrict", "rotate", "pause"] {
        let grant = uuid::Uuid::new_v4().to_string();
        let token = format!("alpha-{grant}");
        conn.execute("UPDATE door_desk_exposure SET enabled=1", [])
            .unwrap();
        conn.execute("INSERT INTO door_clients(id,name,door,resource,owner,registration,scopes,created_ms,updated_ms,token_hash) VALUES(?1,'alpha','beta','door:beta','captain','alpha','[\"mail:read\"]',0,0,?2)",params![grant,Sha256::digest(token.as_bytes()).to_vec()]).unwrap();
        let c = crate::door::clients::resolve(&conn, "beta", &token, &["mail:read"], None)
            .unwrap()
            .unwrap();
        let opened = request(
            "beta",
            &token,
            None,
            &c,
            Command::DeskOpen,
            &json!({}),
            &json!("alpha"),
        )
        .unwrap();
        let sub = opened["subscription"].as_str().unwrap();
        let grant_copy = grant.clone();
        AFTER_COMMIT.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |conn| match mutation {
                "disable" => {
                    conn.execute("UPDATE door_desk_exposure SET enabled=0", [])
                        .unwrap();
                }
                "revoke" => {
                    conn.execute(
                        "UPDATE door_clients SET revoked_ms=1 WHERE id=?1",
                        [&grant_copy],
                    )
                    .unwrap();
                }
                "restrict" => {
                    conn.execute(
                        "UPDATE door_clients SET ships='[\"gamma\"]' WHERE id=?1",
                        [&grant_copy],
                    )
                    .unwrap();
                }
                "rotate" => {
                    conn.execute(
                        "UPDATE door_clients SET token_hash=?2 WHERE id=?1",
                        params![grant_copy, Sha256::digest(b"beta").to_vec()],
                    )
                    .unwrap();
                }
                "pause" => {
                    conn.execute(
                        "UPDATE door_clients SET paused=1 WHERE id=?1",
                        [&grant_copy],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }))
        });
        let result = request(
            "beta",
            &token,
            None,
            &c,
            Command::DeskPull,
            &json!({"subscription":sub}),
            &json!("alpha"),
        );
        assert!(result.is_err(), "{mutation}: {result:?}");
        let retained: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM desk_subscription_ref WHERE subscription=?1",
                [sub],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            retained, 1,
            "release loss cannot undo served custody: {mutation}"
        );
        let snapshots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM desk_subscription_snapshot WHERE subscription=?1",
                [sub],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(snapshots, 1);
        // The same change already committed before admission cannot reserve any
        // further page, even when the old context is supplied.
        let next = request(
            "beta",
            &token,
            None,
            &c,
            Command::DeskPull,
            &json!({"subscription":sub}),
            &json!("beta"),
        );
        assert!(next.is_err() || next.unwrap()["ok"] == false);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM desk_subscription_snapshot WHERE subscription=?1",
                [sub],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            snapshots
        );
    }
}
