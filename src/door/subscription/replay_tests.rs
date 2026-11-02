use super::*;
use crate::door::subscription::replay::{self, DeliveryClass};
#[path = "selection_tests.rs"]
mod selection_tests;

fn enable(f: &Fixture, state: &str, through: &str) {
    f.conn.execute_batch("CREATE TABLE IF NOT EXISTS door_desk_exposure(door TEXT PRIMARY KEY,enabled INTEGER NOT NULL,binding TEXT NOT NULL)").unwrap();
    let binding = json!({"domain":f.m.domain,"owner_core":f.m.owner_core,"generation":f.m.generation,
        "incarnation":f.m.incarnation,"manifest_revision":f.m.manifest_revision,
        "manifest_hash":crate::core::hash(&serde_json::to_value(&f.m).unwrap()),
        "mode":"fresh_replay","replay_policy":"open_and_new","switch_on_ms":100,
        "switch_on":[{"source":CORE,"state":state,"through":through,
        "barrier_mode":"local","received_at":100,"evidence_state":"current"}]});
    f.conn
        .execute(
            "INSERT OR REPLACE INTO door_desk_exposure VALUES('alpha',1,?1)",
            [binding.to_string()],
        )
        .unwrap();
}
fn sub(f: &Fixture, id: &str) -> Subscription {
    Subscription {
        id: id.into(),
        incarnation: f
            .conn
            .query_row(
                "SELECT incarnation FROM desk_subscription WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap(),
    }
}
// These classification fixtures write desk_intake_original directly on purpose;
// machines/desk_intake/original_tests.rs covers the apply_fact path.
fn original(f: &Fixture, id: &str, order: i64) {
    f.conn
        .execute(
            "INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order,source,first_kind) VALUES(?1,?2,1,?3,?2,?4,?5,?6,?2,'native')",
            params![f.m.domain, CORE, f.m.incarnation, BOX, id, order],
        )
        .unwrap();
}
fn saved_cut(f: &Fixture, binding: &Value, delayed: bool) -> String {
    let cut = json!({"checkpoint":{"binding":"alpha","source":CORE,"id":uuid(),"upper":10,"issued":1},
        "vector_digest":"alpha","home":{"home":"alpha","mailbox":BOX,"nonce":"gamma","baseline_upper":0,"delta_upper":0,"mail_upper":0},
        "received_at":100,"session":"alpha","tick_ms":100,"adopted_at":100,"age_at_adoption_ms":0,
        "max_source_age_ms":f.m.max_source_age_ms,"delayed":delayed,"switch_on_ms":100}).to_string();
    f.conn
        .execute(
            "INSERT INTO desk_exposure_cut VALUES(?1,?2,'alpha',?3,'gamma',10,?4)",
            params![crate::core::hash(binding), CORE, BOX, cut],
        )
        .unwrap();
    cut
}
fn class(f: &Fixture, sub: &str, id: &str) -> DeliveryClass {
    let o = intake::observation(&f.conn, &f.m, &MailIdentity::new(CORE, BOX, id).unwrap())
        .unwrap()
        .unwrap();
    replay::delivery_class(&f.conn, &self::sub(f, sub), &f.m, &o).unwrap()
}
fn reopen(f: &Fixture, path: &std::path::Path) -> Fixture {
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    Fixture {
        conn: Connection::open(path).unwrap(),
        m: f.m.clone(),
        c: DelegatedContext::from_gateway(&f.c.gateway()).unwrap(),
    }
}

#[test]
fn original_age_not_closure_order_or_raw_ack_and_no_selection_activation() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    f.add("old-open", 1, "alpha", Some("open"));
    original(&f, "old-open", 1);
    f.add("old-outgoing", 2, "alpha", None);
    original(&f, "old-outgoing", 2);
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.from','captain','$.to','alpha') WHERE id='old-outgoing'", []).unwrap();
    let s = f.open();
    f.add("old-open", 11, "alpha", Some("closed"));
    f.add("late-baseline", 12, "alpha", None);
    original(&f, "late-baseline", 12);
    f.add("legacy", 13, "alpha", None);
    assert_eq!(class(&f, &s, "old-open"), DeliveryClass::History);
    assert_eq!(class(&f, &s, "old-outgoing"), DeliveryClass::History);
    assert_eq!(class(&f, &s, "late-baseline"), DeliveryClass::Unknown);
    assert_eq!(class(&f, &s, "legacy"), DeliveryClass::Unknown);
    let p = f.pull(&s, None, ITEMS);
    assert_eq!(
        p["items"].as_array().unwrap().len(),
        4,
        "selection remains unchanged in PR4a"
    );
    f.run(f.ack(&s, &p, references(&p)));
    assert_eq!(class(&f, &s, "old-open"), DeliveryClass::History);
    assert_eq!(class(&f, &s, "late-baseline"), DeliveryClass::Unknown);
    // Reserve the final wire vocabulary without claiming the unproved class.
    assert_eq!(
        serde_json::to_value(DeliveryClass::SinceSwitchOn).unwrap(),
        "since_switch_on"
    );
}

#[test]
fn binding_survives_restart_disable_reenable_and_visibility_replay() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    f.add("old", 1, "alpha", Some("open"));
    original(&f, "old", 1);
    let s = f.open();
    let saved = replay::binding(&f.conn, &sub(&f, &s)).unwrap();
    let cut = saved_cut(&f, saved.as_ref().unwrap(), false);
    let dir = tempfile::tempdir().unwrap();
    let reopened = reopen(&f, &dir.path().join("alpha.sqlite"));
    reopened
        .conn
        .execute("UPDATE door_desk_exposure SET enabled=0", [])
        .unwrap();
    reopened.run(Operation::Resume {
        subscription: s.clone(),
    });
    assert_eq!(
        replay::binding(&reopened.conn, &sub(&reopened, &s)).unwrap(),
        saved
    );
    // Even a replacement later operator record cannot move a reader boundary.
    enable(&reopened, "established", &format!("{}:99", uuid()));
    let restricted = DelegatedContext::resolved(
        reopened.c.client().into(),
        "door:alpha".into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    reopened.grant(&restricted);
    reopened
        .run_as(
            &restricted,
            Operation::Resume {
                subscription: s.clone(),
            },
            100,
        )
        .unwrap();
    assert_eq!(
        replay::binding(&reopened.conn, &sub(&reopened, &s)).unwrap(),
        saved
    );
    assert_eq!(class(&reopened, &s, "old"), DeliveryClass::History);
    let kept: String = reopened
        .conn
        .query_row("SELECT body FROM desk_exposure_cut", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        kept, cut,
        "disable/re-enable/restriction and restart keep the exact cut"
    );
}

#[test]
fn delayed_cut_reason_is_not_public_history_and_has_no_effect_on_pull_or_ack() {
    let f = Fixture::new();
    enable(&f, "switch_on_pending", "");
    f.add("interval", 1, "alpha", None);
    original(&f, "interval", 1);
    let s = f.open();
    let b = replay::binding(&f.conn, &sub(&f, &s)).unwrap().unwrap();
    let cut = saved_cut(&f, &b, true);
    let o = MailIdentity::new(CORE, BOX, "interval").unwrap();
    assert_eq!(
        intake::replay_class(&f.conn, &f.m, &b, &o).unwrap(),
        "delayed_cut"
    );
    assert_eq!(class(&f, &s, "interval"), DeliveryClass::Unknown);
    let page = f.pull(&s, None, ITEMS);
    assert_eq!(ids(&page), BTreeSet::from(["interval".into()]));
    let before = f.conn.total_changes();
    assert_eq!(class(&f, &s, "interval"), DeliveryClass::Unknown);
    assert_eq!(f.conn.total_changes(), before);
    f.run(f.ack(&s, &page, references(&page)));
    assert!(
        f.pull(&s, None, ITEMS)["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(class(&f, &s, "interval"), DeliveryClass::Unknown);
    assert_eq!(
        f.conn
            .query_row("SELECT body FROM desk_exposure_cut", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        cut
    );
}

#[test]
fn pending_and_missing_bindings_never_turn_late_baseline_into_new_after_restart() {
    for state in ["switch_on_pending", "established"] {
        let f = Fixture::new();
        enable(
            &f,
            state,
            if state == "established" {
                "not-a-journal-boundary"
            } else {
                ""
            },
        );
        f.add("late-baseline", 1, "alpha", Some("open"));
        original(&f, "late-baseline", 1);
        let s = f.open();
        let saved = replay::binding(&f.conn, &sub(&f, &s)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let reopened = reopen(&f, &dir.path().join("alpha.sqlite"));
        enable(&reopened, "established", &format!("{}:20", uuid()));
        reopened.run(Operation::Resume {
            subscription: s.clone(),
        });
        assert_eq!(
            class(&reopened, &s, "late-baseline"),
            DeliveryClass::Unknown
        );
        assert_eq!(
            replay::binding(&reopened.conn, &sub(&reopened, &s)).unwrap(),
            saved
        );
    }
    let f = Fixture::new();
    f.add("legacy", 1, "alpha", None);
    original(&f, "legacy", 1);
    let s = f.open(); // No binding at reader creation.
    enable(&f, "established", &format!("{}:10", uuid()));
    assert_eq!(f.open(), s);
    assert_eq!(class(&f, &s, "legacy"), DeliveryClass::Unknown);
    assert_eq!(replay::binding(&f.conn, &sub(&f, &s)).unwrap(), None);
}

#[test]
fn stale_or_unreadable_operator_binding_does_not_change_reader_admission() {
    for change in ["revision", "hash", "malformed_json", "malformed_binding"] {
        let mut f = Fixture::new();
        enable(&f, "established", &format!("{}:10", uuid()));
        f.add("old", 1, "alpha", Some("open"));
        original(&f, "old", 1);
        match change {
            "revision" => f.m.manifest_revision += 1,
            "hash" => f.m.max_source_age_ms += 1,
            _ => {
                f.conn
                    .execute(
                        "UPDATE door_desk_exposure SET binding=?1",
                        [if change == "malformed_json" {
                            "{"
                        } else {
                            "{}"
                        }],
                    )
                    .unwrap();
            }
        }
        let s = f.open();
        assert_eq!(replay::binding(&f.conn, &sub(&f, &s)).unwrap(), None);
        let stored: Option<String> = f
            .conn
            .query_row(
                "SELECT binding FROM desk_subscription_replay WHERE subscription=?1",
                [&s],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, None, "{change} must persist SQL NULL");
        assert_eq!(class(&f, &s, "old"), DeliveryClass::Unknown);
        assert_eq!(
            ids(&f.pull(&s, None, ITEMS)),
            BTreeSet::from(["old".into()])
        );
    }
}

#[test]
fn classifier_rejects_mismatched_captured_owner_tuple_on_its_own() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    f.add("old", 1, "alpha", None);
    original(&f, "old", 1);
    let s = f.open();
    assert_eq!(class(&f, &s, "old"), DeliveryClass::History);
    let o = intake::observation(&f.conn, &f.m, &MailIdentity::new(CORE, BOX, "old").unwrap())
        .unwrap()
        .unwrap();
    for field in ["domain", "owner_core", "generation", "incarnation"] {
        let mut m = f.m.clone();
        match field {
            "domain" => m.domain = "beta-domain".into(),
            "owner_core" => m.owner_core = uuid(),
            "generation" => m.generation += 1,
            "incarnation" => m.incarnation = uuid(),
            _ => unreachable!(),
        }
        assert_eq!(
            replay::delivery_class(&f.conn, &sub(&f, &s), &m, &o).unwrap(),
            DeliveryClass::Unknown,
            "{field} mismatch must not classify history"
        );
    }
}

#[test]
fn retirement_removes_only_the_retired_readers_replay_evidence() {
    for reason in ["owner_change", "revoked", "expired"] {
        let mut f = Fixture::new();
        enable(&f, "established", &format!("{}:10", uuid()));
        let retired = f.open();
        let other =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        f.grant(&other);
        let other_id = f.run_as(&other, Operation::Open, 100).unwrap()["subscription"]
            .as_str()
            .unwrap()
            .to_owned();
        let other_sub = sub(&f, &other_id);
        let saved = replay::binding(&f.conn, &other_sub).unwrap();
        assert!(saved.is_some());
        match reason {
            "owner_change" => {
                f.m.generation += 1;
                assert_ne!(f.open(), retired);
            }
            _ => {
                let column = if reason == "revoked" {
                    "revoked_ms"
                } else {
                    "expires_ms"
                };
                f.conn
                    .execute(
                        &format!("UPDATE door_clients SET {column}=100 WHERE id=?1"),
                        [f.c.client()],
                    )
                    .unwrap();
                // A live reader's admission collects terminal grants, even
                // though the retired grant can no longer enter the engine.
                assert_eq!(
                    f.run_as(&other, Operation::Open, 100).unwrap()["subscription"],
                    other_id
                );
            }
        }
        for table in ["desk_subscription", "desk_subscription_replay"] {
            let key = if table == "desk_subscription" {
                "id"
            } else {
                "subscription"
            };
            let present = |id: &str| -> bool {
                f.conn
                    .query_row(
                        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {key}=?1)"),
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            assert!(!present(&retired), "{reason}: retired row in {table}");
            assert!(present(&other_id), "{reason}: unrelated row in {table}");
        }
        assert_eq!(replay::binding(&f.conn, &other_sub).unwrap(), saved);
    }
}

#[test]
fn populated_v3_to_v5_keeps_evidence_custody_and_older_writer_rows_unknown() {
    let f = Fixture::new();
    enable(&f, "established", &format!("{}:10", uuid()));
    f.add("old-open", 1, "alpha", Some("open"));
    original(&f, "old-open", 1);
    let s = f.open();
    let page = f.pull(&s, None, ITEMS);
    let refs = references(&page);
    let tables = [
        "door_clients",
        "desk_subscription",
        "desk_subscription_replay",
        "desk_subscription_ref",
        "desk_subscription_progress",
        "desk_subscription_receipt",
        "desk_subscription_snapshot",
        "desk_intake_original",
        "door_desk_exposure",
    ];
    let state = |conn: &Connection| -> Vec<Value> {
        tables
            .iter()
            .map(|table| {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .unwrap();
                let columns = stmt.column_count();
                json!(
                    stmt.query_map([], |r| Ok((0..columns)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()))
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap()
                )
            })
            .collect()
    };
    f.conn.execute_batch("DROP TABLE door_client_schema_v5; DROP TABLE desk_exposure_cut; DROP TABLE desk_exposure_upgrade;
        CREATE TABLE door_client_schema_v3(singleton INTEGER PRIMARY KEY CHECK(singleton=1));").unwrap();
    let before = state(&f.conn);
    super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(state(&f.conn), before);
    assert_eq!(class(&f, &s, "old-open"), DeliveryClass::History);
    assert_eq!(
        f.count("desk_exposure_cut"),
        0,
        "4a coverage does not become a replay cut"
    );
    let b = replay::binding(&f.conn, &sub(&f, &s)).unwrap().unwrap();
    let kept_cut = saved_cut(&f, &b, false);
    let changes = f.conn.total_changes();
    super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(f.conn.total_changes(), changes, "restart is idempotent");
    // Simulated v3 writer: its marker remains, and nullable immutable fields
    // are left absent. Re-upgrade must not synthesize missing birth proof.
    f.conn
        .execute_batch("DROP TABLE door_client_schema_v5;")
        .unwrap();
    f.add("older-writer", 11, "alpha", None);
    f.conn.execute("INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order)
        VALUES(?1,?2,1,?3,?2,?4,'older-writer',11)",params![f.m.domain,CORE,f.m.incarnation,BOX]).unwrap();
    super::super::super::clients::init(&f.conn).unwrap();
    assert_eq!(class(&f, &s, "older-writer"), DeliveryClass::Unknown);
    assert_eq!(
        f.conn
            .query_row("SELECT body FROM desk_exposure_cut", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        kept_cut
    );
    let resumed = f.run(Operation::Resume {
        subscription: s.clone(),
    });
    let token = resumed["pending_snapshots"][0]["continuation"]
        .as_str()
        .unwrap();
    assert_eq!(references(&f.pull(&s, Some(token.into()), ITEMS)), refs);
    f.run(f.ack(&s, &page, refs));
    assert_eq!(
        ids(&f.pull(&s, None, ITEMS)),
        BTreeSet::from(["older-writer".into()])
    );
}

#[test]
fn populated_v2_upgrade_preserves_grants_reader_refs_and_lost_page() {
    let f = Fixture::new();
    f.add("old-open", 1, "alpha", Some("open"));
    let s = f.open();
    let page = f.pull(&s, None, ITEMS);
    let refs = references(&page);
    let state = |f: &Fixture| -> Vec<Value> {
        [
            "door_clients",
            "desk_subscription",
            "desk_subscription_ref",
            "desk_subscription_progress",
            "desk_subscription_receipt",
            "desk_subscription_snapshot",
        ]
        .iter()
        .map(|table| {
            let mut stmt = f
                .conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    Ok((0..columns)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>())
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            json!(rows)
        })
        .collect()
    };
    f.conn.execute_batch("DROP TABLE door_client_schema_v5; DROP TABLE desk_subscription_replay; CREATE TABLE door_client_schema_v2(singleton INTEGER PRIMARY KEY CHECK(singleton=1));").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let upgraded = reopen(&f, &dir.path().join("alpha.sqlite"));
    let before = state(&upgraded);
    super::super::super::clients::init(&upgraded.conn).unwrap();
    assert_eq!(state(&upgraded), before);
    let schema: i64 = upgraded
        .conn
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let changes = upgraded.conn.total_changes();
    super::super::super::clients::init(&upgraded.conn).unwrap();
    assert_eq!(changes, upgraded.conn.total_changes());
    assert_eq!(
        schema,
        upgraded
            .conn
            .pragma_query_value::<i64, _>(None, "schema_version", |r| r.get(0))
            .unwrap()
    );
    assert_eq!(
        replay::binding(&upgraded.conn, &sub(&upgraded, &s)).unwrap(),
        None
    );
    enable(&upgraded, "established", &format!("{}:10", uuid()));
    let resumed = upgraded.run(Operation::Resume {
        subscription: s.clone(),
    });
    assert_eq!(resumed["pending_snapshots"].as_array().unwrap().len(), 1);
    let token = resumed["pending_snapshots"][0]["continuation"]
        .as_str()
        .unwrap();
    assert_eq!(
        references(&upgraded.pull(&s, Some(token.into()), ITEMS)),
        refs
    );
    upgraded.run(upgraded.ack(&s, &page, refs));
    assert!(
        upgraded.pull(&s, None, ITEMS)["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        replay::binding(&upgraded.conn, &sub(&upgraded, &s)).unwrap(),
        None
    );
}

/// A cut table a development build made without `journal_upper` is rebuilt
/// on startup, even with every table present: cuts whose body names their
/// upper bound keep it, and a cut that cannot be ordered is dropped.
#[test]
fn an_old_cut_table_is_rebuilt_with_its_upper_bounds() {
    let f = Fixture::new();
    let kept = json!({"checkpoint": {"upper": 7}}).to_string();
    f.conn
        .execute_batch(
            "DROP TABLE desk_exposure_cut;
             CREATE TABLE desk_exposure_cut(
               binding TEXT NOT NULL, source TEXT NOT NULL, home TEXT NOT NULL,
               mailbox TEXT NOT NULL, nonce TEXT NOT NULL, body TEXT NOT NULL,
               PRIMARY KEY(binding,source,home,mailbox,nonce));",
        )
        .unwrap();
    for (nonce, body) in [("kept", kept.as_str()), ("unordered", "{}")] {
        f.conn
            .execute(
                "INSERT INTO desk_exposure_cut VALUES('b','s','h','m',?1,?2)",
                params![nonce, body],
            )
            .unwrap();
    }
    crate::door::subscription::init(&f.conn).unwrap();
    let rows: Vec<(String, i64, String)> = f
        .conn
        .prepare("SELECT nonce, journal_upper, body FROM desk_exposure_cut")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![("kept".to_owned(), 7, kept)]);
    let changes = f.conn.total_changes();
    crate::door::subscription::init(&f.conn).unwrap();
    assert_eq!(
        f.conn.total_changes(),
        changes,
        "a current table is left alone"
    );
}
