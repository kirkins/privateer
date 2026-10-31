use super::*;
#[path = "bounds_tests.rs"]
mod bounds_tests;
#[path = "concurrency_tests.rs"]
mod concurrency_tests;
#[path = "fence_tests.rs"]
mod fence_tests;
#[path = "review_tests.rs"]
mod review_tests;
use std::collections::BTreeSet;

const CORE: &str = "00000000-0000-4000-8000-00000000000a";
const BOX: &str = "00000000-0000-4000-8000-0000000000a1";

struct Fixture {
    conn: Connection,
    m: Manifest,
    c: DelegatedContext,
}
impl Fixture {
    fn new() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        super::super::clients::init(&conn).unwrap();
        crate::machines::init(&conn).unwrap();
        let m: Manifest = serde_json::from_value(json!({"domain":"alpha-domain","manifest_revision":1,
            "max_source_age_ms":900000,"owner_core":CORE,"generation":1,"incarnation":uuid(),"coordinator":CORE,
            "sources":[{"core":CORE,"name":"alpha","ships":["alpha","beta"],"intake_direction":"local"}]})).unwrap();
        coverage::install_placement_manifest(
            &conn,
            &m.domain,
            m.max_source_age_ms,
            &m.sources
                .iter()
                .map(|s| (s.core.clone(), s.ships.clone()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let c =
            DelegatedContext::resolved(uuid(), "door:alpha".into(), vec!["mail:read".into()], None);
        let f = Self { conn, m, c };
        f.grant(&f.c);
        f
    }
    fn grant(&self, c: &DelegatedContext) {
        self.conn.execute("INSERT OR REPLACE INTO door_clients(id,name,door,resource,owner,registration,scopes,ships,created_ms,updated_ms,token_hash) VALUES(?1,'alpha',?2,?3,'captain','alpha',?4,?5,0,0,?6)",params![c.client(),c.resource().strip_prefix("door:").unwrap(),c.resource(),serde_json::to_string(&c.capabilities()["scopes"]).unwrap(),serde_json::to_string(&c.capabilities()["ships"]).unwrap(),uuid().as_bytes()[..32].to_vec()]).unwrap();
    }
    fn add(&self, id: &str, order: i64, ship: &str, state: Option<&str>) {
        self.conn.execute("INSERT INTO desk_intake_fact(domain,owner_core,generation,incarnation,source,core,mailbox,id,source_order,content,state,unread,pending) VALUES(?1,?2,1,?3,?2,?2,?4,?5,?6,?7,?8,1,1) ON CONFLICT(domain,core,mailbox,id) DO UPDATE SET source_order=excluded.source_order,state=excluded.state",params![self.m.domain,CORE,self.m.incarnation,BOX,id,order,serde_json::to_string(&json!({"id":id,"from":ship,"to":"captain","body":id,"created_ms":0})).unwrap(),state]).unwrap();
    }
    fn run_as(&self, c: &DelegatedContext, op: Operation, now: i64) -> Result<Value> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        super::super::clients::subscription_admission(&tx, c)?;
        let out = run_admitted(&tx, c, &self.m, op, now)?;
        tx.commit()?;
        Ok(out)
    }
    fn run(&self, op: Operation) -> Value {
        self.run_as(&self.c, op, 100).unwrap()
    }
    fn open(&self) -> String {
        self.run(Operation::Open)["subscription"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn pull(&self, sub: &str, continuation: Option<String>, items: usize) -> Value {
        self.run(Operation::Pull {
            subscription: sub.into(),
            continuation,
            items,
        })
    }
    fn ack(&self, sub: &str, page: &Value, refs: Vec<String>) -> Operation {
        Operation::Ack {
            subscription: sub.into(),
            key: page["ack_key"].as_str().unwrap().into(),
            refs,
        }
    }
    fn count(&self, table: &str) -> i64 {
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}
fn references(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["ref"].as_str().unwrap().into())
        .collect()
}
fn ids(page: &Value) -> BTreeSet<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["origin"]["id"].as_str().unwrap().into())
        .collect()
}
fn next(page: &Value) -> Option<String> {
    page["continuation"].as_str().map(str::to_owned)
}

#[test]
fn immutable_snapshot_exact_history_closure_and_outgoing() {
    let f = Fixture::new();
    for (i, id) in ["old-unread", "old-open", "outgoing", "closure"]
        .iter()
        .enumerate()
    {
        f.add(
            id,
            i as i64 + 1,
            "alpha",
            Some(if *id == "old-open" { "open" } else { "closed" }),
        );
    }
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.from','captain','$.to','alpha') WHERE id='outgoing'",[]).unwrap();
    let sub = f.open();
    let first = f.pull(&sub, None, 1);
    // Intake mutates the projection's sort key during paging. The frozen
    // version must remain in this vector and not be skipped or duplicated.
    f.add("old-open", 50, "alpha", Some("closed"));
    f.add("new-intake", 51, "alpha", None);
    let mut all = ids(&first);
    let mut page = first;
    while let Some(token) = next(&page) {
        let replay = f.pull(&sub, Some(token.clone()), 1);
        page = f.pull(&sub, Some(token), 1);
        assert_eq!(replay, page);
        all.extend(ids(&page));
        if ids(&page).contains("old-open") {
            assert_eq!(page["items"][0]["state"], "open");
        }
    }
    assert_eq!(
        all,
        BTreeSet::from_iter(["old-unread", "old-open", "outgoing", "closure"].map(str::to_owned))
    );
    assert_eq!(f.count("desk_subscription_progress"), 0);
    assert_eq!(f.count("desk_intake_fact"), 5);
    assert!(!page["coverage"]["complete"].as_bool().unwrap());
}

#[test]
fn whole_batch_sparse_ack_show_cannot_skip_unseen_gap_and_receipt_replay() {
    let f = Fixture::new();
    for i in 1..=3 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    let sub = f.open();
    let show = f.run(Operation::Show {
        subscription: sub.clone(),
        origin: MailIdentity::new(CORE, BOX, "mail-3").unwrap(),
    });
    let refs = references(&show);
    let mut mixed = refs.clone();
    mixed.push(uuid());
    assert!(f.run_as(&f.c, f.ack(&sub, &show, mixed), 100).is_err());
    assert_eq!(
        f.conn
            .query_row("SELECT SUM(acked) FROM desk_subscription_ref", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let acked = f.run(f.ack(&sub, &show, refs.clone()));
    assert_eq!(acked, f.run(f.ack(&sub, &show, refs)));
    assert_eq!(f.count("desk_subscription_progress"), 0);
    let pull = f.pull(&sub, None, 50);
    assert_eq!(
        ids(&pull),
        BTreeSet::from_iter(["mail-1", "mail-2"].map(str::to_owned))
    );
    f.run(f.ack(&sub, &pull, references(&pull)));
    assert_eq!(f.count("desk_subscription_ref"), 0);
    let replay = f.pull(&sub, None, 50);
    assert!(ids(&replay).is_empty());
    assert_eq!(
        f.conn
            .query_row(
                "SELECT through_order FROM desk_subscription_progress",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    assert!(
        f.run_as(&f.c, f.ack(&sub, &show, vec![uuid()]), 100)
            .is_err()
    );
}

#[test]
fn independent_grants_doors_visibility_revoke_and_forged_refs() {
    let f = Fixture::new();
    f.add("alpha-mail", 1, "alpha", None);
    f.add("beta-mail", 2, "beta", None);
    let sub = f.open();
    let page = f.pull(&sub, None, 1);
    for resource in ["door:alpha", "door:beta"] {
        let other =
            DelegatedContext::resolved(uuid(), resource.into(), vec!["mail:read".into()], None);
        f.grant(&other);
        let opened = f.run_as(&other, Operation::Open, 100).unwrap();
        let other_sub = opened["subscription"].as_str().unwrap();
        assert_ne!(sub, other_sub);
        let other_page = f
            .run_as(
                &other,
                Operation::Pull {
                    subscription: other_sub.into(),
                    continuation: None,
                    items: 50,
                },
                100,
            )
            .unwrap();
        assert_eq!(ids(&other_page).len(), 2);
        assert!(
            f.run_as(
                &other,
                Operation::Ack {
                    subscription: other_sub.into(),
                    key: other_page["ack_key"].as_str().unwrap().into(),
                    refs: references(&page)
                },
                100
            )
            .is_err()
        );
    }
    let restricted = DelegatedContext::resolved(
        f.c.client().into(),
        f.c.resource().into(),
        vec!["mail:read".into()],
        Some(vec!["alpha".into()]),
    );
    f.grant(&restricted);
    assert!(
        f.run_as(
            &f.c,
            Operation::Resume {
                subscription: sub.clone()
            },
            100
        )
        .is_err()
    );
    assert!(
        f.run_as(
            &restricted,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: next(&page),
                items: 50
            },
            100
        )
        .is_err()
    );
    f.run_as(
        &restricted,
        Operation::Resume {
            subscription: sub.clone(),
        },
        100,
    )
    .unwrap();
    assert!(
        f.run_as(&restricted, f.ack(&sub, &page, references(&page)), 100)
            .is_err()
    );
    let fresh = f
        .run_as(
            &restricted,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: None,
                items: 50,
            },
            100,
        )
        .unwrap();
    assert_eq!(ids(&fresh), BTreeSet::from(["alpha-mail".into()]));
    let serialized = serde_json::to_string(&fresh["coverage"]).unwrap();
    assert!(!serialized.contains("beta") && !serialized.contains(CORE));
    assert_eq!(fresh["coverage"]["manifest_revision"], Value::Null);
    f.conn
        .execute(
            "UPDATE door_clients SET revoked_ms=1 WHERE id=?1",
            [f.c.client()],
        )
        .unwrap();
    assert!(
        f.run_as(&restricted, Operation::Resume { subscription: sub }, 100)
            .is_err()
    );
}

#[test]
fn pending_versions_survive_snapshot_expiry_and_receipts_expire_honestly() {
    let f = Fixture::new();
    f.add("old-open", 1, "alpha", Some("open"));
    let sub = f.open();
    let old = f.pull(&sub, None, 1);
    f.add("old-open", 2, "alpha", Some("closed"));
    let resumed = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: None,
                items: 50,
            },
            SNAPSHOT_TTL + 101,
        )
        .unwrap();
    assert_eq!(resumed["items"].as_array().unwrap().len(), 2);
    assert_eq!(references(&resumed)[0], references(&old)[0]);
    assert!(
        f.run_as(&f.c, f.ack(&sub, &old, references(&old)), RECEIPT_TTL + 101)
            .is_err()
    );
    assert_eq!(f.count("desk_subscription_progress"), 0);
}

#[test]
fn thread_visibility_byte_limits_snapshot_backpressure_and_no_legacy_mutations() {
    let f = Fixture::new();
    review_tests::seed_legacy(&f.conn);
    let legacy_before = review_tests::legacy_state(&f.conn);
    for i in 1..=3 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    f.conn.execute("UPDATE desk_intake_fact SET content=json_set(content,'$.correlation','mail-1') WHERE id='mail-2'",[]).unwrap();
    let sub = f.open();
    let thread = f.run(Operation::Thread {
        subscription: sub.clone(),
        origin: MailIdentity::new(CORE, BOX, "mail-1").unwrap(),
        continuation: None,
        items: 50,
    });
    assert_eq!(
        ids(&thread),
        BTreeSet::from_iter(["mail-1", "mail-2"].map(str::to_owned))
    );
    let pull = f.pull(&sub, None, 50);
    assert!(
        f.run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: None,
                items: 50
            },
            100
        )
        .is_err()
    );
    f.run(f.ack(&sub, &pull, references(&pull)));
    assert_eq!(f.count("desk_intake_fact"), 3);
    assert_eq!(legacy_before, review_tests::legacy_state(&f.conn));
    let big = Fixture::new();
    big.add("large", 1, "alpha", None);
    big.conn
        .execute(
            "UPDATE desk_intake_fact SET content=json_set(content,'$.body',?1)",
            ["x".repeat(BYTES)],
        )
        .unwrap();
    let sub = big.open();
    let err = big
        .run_as(
            &big.c,
            Operation::Pull {
                subscription: sub,
                continuation: None,
                items: 1,
            },
            100,
        )
        .unwrap_err();
    assert_eq!(err.to_string(), "oversized_item");
    assert_eq!(big.count("desk_subscription_ref"), 0);
}

#[test]
fn window_and_receipt_limits_stop_without_evicting_pending() {
    let f = Fixture::new();
    for i in 1..=WINDOW + 1 {
        f.add(&format!("mail-{i}"), i, "alpha", None);
    }
    let sub = f.open();
    let mut p = f.pull(&sub, None, 50);
    for _ in 1..5 {
        p = f.pull(&sub, next(&p), 50);
    }
    p = f.pull(&sub, next(&p), 6);
    let failed = f
        .run_as(
            &f.c,
            Operation::Pull {
                subscription: sub.clone(),
                continuation: next(&p),
                items: 1,
            },
            100,
        )
        .unwrap_err();
    assert_eq!(failed.to_string(), "backpressure");
    assert_eq!(f.count("desk_subscription_ref"), WINDOW);
    assert_eq!(f.count("desk_intake_fact"), WINDOW + 1);
    let f = Fixture::new();
    let sub = f.open();
    // Real committed receipts are retained, unlike unused reservations.
    f.conn
        .execute("DELETE FROM desk_subscription_receipt", [])
        .unwrap();
    for _ in 0..RECEIPTS {
        f.conn.execute("INSERT INTO desk_subscription_receipt(key,subscription,expires,batch,outcome) VALUES(?1,?2,?3,'[]','{}')", params![uuid(),sub,RECEIPT_TTL]).unwrap();
    }
    assert!(
        f.run_as(&f.c, Operation::Resume { subscription: sub }, 100)
            .is_err()
    );
    assert_eq!(f.count("desk_subscription_receipt"), RECEIPTS);
}

#[test]
fn reopen_recarries_committed_refs_and_owner_incarnation_is_not_translated() {
    let f = Fixture::new();
    f.add("mail-1", 1, "alpha", None);
    let sub = f.open();
    let page = f.pull(&sub, None, 1);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alpha.sqlite");
    // VACUUM INTO exercises file-backed reopen without needing the backup feature.
    f.conn
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let mut reopened = Fixture {
        conn: Connection::open(path).unwrap(),
        m: f.m.clone(),
        c: DelegatedContext::from_gateway(&f.c.gateway()).unwrap(),
    };
    // Both initial outputs may have vanished. Resume still exposes a durable
    // start token instead of demanding a client-supplied sequence.
    reopened.pull(&sub, None, 1);
    let resumed = reopened.run(Operation::Resume {
        subscription: sub.clone(),
    });
    let token = resumed["pending_snapshots"][0]["continuation"]
        .as_str()
        .unwrap();
    let replay = reopened.pull(&sub, Some(token.into()), 1);
    assert_eq!(references(&page), references(&replay));
    let outcome = reopened.run(reopened.ack(&sub, &page, references(&page)));
    assert_eq!(
        outcome,
        reopened.run(reopened.ack(&sub, &page, references(&page)))
    );
    reopened.m.incarnation = uuid();
    assert!(
        reopened
            .run_as(&reopened.c, Operation::Resume { subscription: sub }, 100)
            .is_err()
    );
}
