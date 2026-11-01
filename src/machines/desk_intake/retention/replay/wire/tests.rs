use super::*;
use crate::{mail::source, store::Store};
use std::time::Duration;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}
struct Fixture {
    dir: tempfile::TempDir,
    home: Store,
    source: Connection,
    desk: Connection,
    m: Manifest,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut home = Store::open(&dir.path().join("alpha")).unwrap();
        crate::mail::dispatch(
            &mut home,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        source::bind(home.connection(), ALPHA).unwrap();
        source::init_intake(home.connection()).unwrap();
        let source = Connection::open(dir.path().join("source.sqlite")).unwrap();
        crate::machines::init(&source).unwrap();
        source
            .execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER)")
            .unwrap();
        source
            .execute(
                "INSERT INTO ships VALUES('survey',?1,NULL)",
                [dir.path().join("alpha").to_str().unwrap()],
            )
            .unwrap();
        let desk = Connection::open(dir.path().join("desk.sqlite")).unwrap();
        crate::machines::init(&desk).unwrap();
        let m: Manifest = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,"owner_core":ALPHA,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"local"}]})).unwrap();
        let bound = format!("{}:{ALPHA}", binding(&m).unwrap());
        source::enrollment::begin(home.connection(), &bound).unwrap();
        let baseline = source::enrollment::open(&dir.path().join("alpha"), true).unwrap();
        source::init(&baseline).unwrap();
        source::init_intake(&baseline).unwrap();
        source::enrollment::freeze(home.connection(), &baseline, &bound, deadline()).unwrap();
        source::enrollment::publish(home.connection(), &baseline, &bound).unwrap();
        let f = Self {
            dir,
            home,
            source,
            desk,
            m,
        };
        f.capture();
        f
    }
    fn capture(&self) {
        home::capture_ready_until(
            &self.dir.path().join("alpha"),
            self.home.connection(),
            &self.source,
            &self.m,
            ALPHA,
            deadline(),
        )
        .unwrap();
    }
    fn send(&mut self, id: &str) {
        crate::mail::dispatch(
            &mut self.home,
            "mail.send",
            json!({"id":id,"from":"survey","to":"captain","kind":"notice","body":"Native content"}),
        )
        .unwrap();
        self.capture();
    }
    fn evidence(&self) -> proof::Evidence {
        reconcile::save(&self.source, &self.m, ALPHA, deadline()).unwrap();
        let cp = checkpoint(
            &self.source,
            &self.m,
            ALPHA,
            &uuid::Uuid::new_v4().to_string(),
        )
        .unwrap();
        proof::issue(&self.source, &self.m, &cp, deadline()).unwrap();
        let evidence = proof::outgoing(&self.source, &self.m, &cp, deadline())
            .unwrap()
            .unwrap();
        let mut after = 0;
        loop {
            let p = page(&self.source, &self.m, &cp, after, 50, BYTES).unwrap();
            accept(
                &self.desk,
                &self.m,
                &p,
                &Clock {
                    session: "alpha",
                    wall_ms: 1000,
                    tick_ms: 1000,
                },
            )
            .unwrap();
            after = p.through;
            if after == cp.upper {
                break;
            }
        }
        proof::receive(&self.desk, &self.m, ALPHA, Some(evidence.clone())).unwrap();
        evidence
    }
    fn wire(&self, e: &proof::Evidence) -> Value {
        outgoing(&self.source, &self.m, ALPHA, false, Some(e), deadline()).unwrap()
    }
    fn receipts(&self) -> Value {
        outgoing(&self.desk, &self.m, ALPHA, true, None, deadline()).unwrap()
    }
    fn adopt(&self, v: &Value) -> Result<()> {
        receive(&self.desk, &self.m, ALPHA, true, v)
    }
    fn deliver_receipts(&self, v: &Value) -> Result<()> {
        receive(&self.source, &self.m, ALPHA, false, v)
    }
}
fn count(c: &Connection, t: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
        .unwrap()
}
fn resign(v: &mut Value) {
    let b: Batch = serde_json::from_value(v["proof"].clone()).unwrap();
    v["proof"]["digest"] = json!(batch_digest(&b));
}

#[test]
fn dropped_sidecar_page_receipt_restart_and_lost_adoption_receipt_keep_custody() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence(); // Ordinary page fully accepted, no adoption.
    let first = f.wire(&e);
    assert_eq!(first["proof"]["witnesses"].as_array().unwrap().len(), 1);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
    assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    // A new connection/restart can resend after the ordinary page is gone.
    let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    let second = outgoing(&retry, &f.m, ALPHA, false, Some(&e), deadline()).unwrap();
    assert_eq!(first, second);
    f.adopt(&second).unwrap();
    let receipts = f.receipts();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    // Lost receipt: repeat durable adoption, never ordinary acknowledgement.
    f.adopt(&first).unwrap();
    assert_eq!(f.receipts(), receipts);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
    f.deliver_receipts(&receipts).unwrap();
    assert_eq!(
        count(&f.source, "desk_replay_witness"),
        1,
        "retry window preserves confirmed evidence"
    );
    for _ in 0..3 {
        f.deliver_receipts(&receipts).unwrap();
    }
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    assert_eq!(count(&f.source, "desk_replay_import"), 0);
    assert_eq!(count(&f.source, "desk_replay_home"), 1);
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    // Late exact receipt after cleanup has no effect and succeeds.
    f.deliver_receipts(&receipts).unwrap();
}

#[test]
fn provisional_adoption_validates_entire_batch_before_receipting() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    for field in [
        "digest",
        "source",
        "journal_order",
        "nonce",
        "mail_order",
        "vector_digest",
        "home",
        "checkpoint",
    ] {
        let mut bad = wire.clone();
        match field {
            "digest" | "source" | "nonce" | "home" => {
                bad["proof"]["witnesses"][0][field] = json!("wrong")
            }
            "journal_order" | "mail_order" => bad["proof"]["witnesses"][0][field] = json!(i64::MAX),
            "vector_digest" => bad["proof"][field] = json!("wrong"),
            _ => {
                bad["proof"]["evidence"]["checkpoint"]["id"] =
                    json!(uuid::Uuid::new_v4().to_string())
            }
        }
        resign(&mut bad);
        assert!(f.adopt(&bad).is_err(), "{field}");
        assert_eq!(count(&f.desk, "desk_replay_adopted"), 0);
        assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    }
    // Reject partial batch commit if any witness has no immutable original.
    let mut bad = wire.clone();
    let mut missing = bad["proof"]["witnesses"][0].clone();
    missing["origin"]["id"] = json!("missing");
    missing["intent"] = json!("missing");
    bad["proof"]["witnesses"]
        .as_array_mut()
        .unwrap()
        .push(missing);
    resign(&mut bad);
    assert!(f.adopt(&bad).is_err());
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 0);
    f.adopt(&wire).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
}

#[test]
fn null_and_wrong_native_first_tuple_cannot_adopt_but_copy_first_is_only_provisional() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    let order: i64 = f
        .desk
        .query_row("SELECT first_order FROM desk_intake_original", [], |r| {
            r.get(0)
        })
        .unwrap();
    for sql in [
        "UPDATE desk_intake_original SET first_kind=NULL",
        "UPDATE desk_intake_original SET first_kind='native',first_order=first_order+1",
    ] {
        f.desk.execute_batch(sql).unwrap();
        assert!(f.adopt(&wire).is_err());
        assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    }
    f.desk
        .execute(
            "UPDATE desk_intake_original SET first_kind='copy',first_order=?1,source='beta'",
            [order],
        )
        .unwrap();
    f.adopt(&wire).unwrap();
    let saved: (String, i64, String) = f
        .desk
        .query_row(
            "SELECT first_source,first_order,first_kind FROM desk_replay_adopted",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(saved, ("beta".into(), order, "copy".into()));
    // No door cut/eligibility/pull state is created by provisional custody.
    assert_eq!(count(&f.desk, "desk_subscription_replay"), 0);
}

#[test]
fn adoption_budget_failure_is_atomic_and_source_receipts_are_exact() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    f.desk.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_adopted SELECT 'filler','alpha','mailbox',CAST(x AS TEXT),'binding','alpha',1,'native','{}' FROM n",[ADOPTED]).unwrap();
    assert!(f.adopt(&wire).unwrap_err().to_string().contains("budget"));
    assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    f.desk
        .execute("DELETE FROM desk_replay_adopted WHERE domain='filler'", [])
        .unwrap();
    f.adopt(&wire).unwrap();
    let receipts = f.receipts();
    let mut wrong = receipts.clone();
    wrong["receipts"][0]["source"] = json!("beta");
    assert!(f.deliver_receipts(&wrong).is_err());
    let mut wrong = receipts.clone();
    wrong["receipts"][0]["digest"] = json!("unknown");
    f.deliver_receipts(&wrong).unwrap();
    let adopted: bool = f
        .source
        .query_row("SELECT adopted FROM desk_replay_witness", [], |r| r.get(0))
        .unwrap();
    assert!(!adopted);
    f.deliver_receipts(&receipts).unwrap();
    assert!(
        f.source
            .query_row("SELECT adopted FROM desk_replay_witness", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
}

#[test]
fn interrupted_adoption_and_receipt_transactions_keep_proof_custody_atomic() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    f.desk
        .execute_batch(
            "CREATE TRIGGER fail_replay_receipt BEFORE INSERT ON desk_replay_receipt
        BEGIN SELECT RAISE(ABORT,'receipt unavailable'); END",
        )
        .unwrap();
    assert!(f.adopt(&wire).is_err());
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 0);
    assert_eq!(count(&f.desk, "desk_replay_vector"), 0);
    assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    assert_eq!(
        count(&f.desk, "desk_intake_original"),
        1,
        "ordinary page already committed"
    );
    f.desk
        .execute_batch("DROP TRIGGER fail_replay_receipt")
        .unwrap();
    f.adopt(&wire).unwrap();
    let receipts = f.receipts();
    f.source
        .execute_batch(
            "CREATE TRIGGER fail_replay_release BEFORE INSERT ON desk_replay_release
        BEGIN SELECT RAISE(ABORT,'release unavailable'); END",
        )
        .unwrap();
    assert!(f.deliver_receipts(&receipts).is_err());
    assert!(
        !f.source
            .query_row("SELECT adopted FROM desk_replay_witness", [], |r| r
                .get::<_, bool>(0))
            .unwrap()
    );
    assert_eq!(count(&f.source, "desk_replay_batch"), 1);
    f.source
        .execute_batch("DROP TRIGGER fail_replay_release")
        .unwrap();
    f.deliver_receipts(&receipts).unwrap();
}

#[test]
fn superseded_transport_metadata_retires_but_never_unconfirmed_witnesses() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let wire = f.wire(&e);
    f.adopt(&wire).unwrap();
    f.m.manifest_revision += 1;
    retire(&f.source, &f.m).unwrap();
    retire(&f.desk, &f.m).unwrap();
    assert_eq!(count(&f.source, "desk_replay_batch"), 0);
    assert_eq!(count(&f.source, "desk_replay_send"), 0);
    assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    assert_eq!(count(&f.desk, "desk_replay_vector"), 0);
    assert_eq!(count(&f.source, "desk_replay_witness"), 1);
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    assert_eq!(count(&f.source, "desk_replay_home"), 1);
}

#[test]
fn rotating_unadopted_service_wraps_instead_of_starving_old_witnesses() {
    let mut f = Fixture::new();
    for n in 0..60 {
        f.send(&format!("native-{n:03}"));
    }
    let e = f.evidence();
    let first = f.wire(&e);
    let second = f.wire(&e);
    let intents = |v: &Value| {
        v["proof"]["witnesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["intent"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(intents(&first).union(&intents(&second)).count(), 60);
    assert_eq!(
        count(&f.source, "desk_replay_witness"),
        60,
        "sending never adopts"
    );
    assert!(transport::json_size_bound(&first) <= WIRE);
    assert!(transport::json_size_bound(&second) <= WIRE);
}

#[test]
fn complete_envelope_at_request_edge_drops_replay_before_coverage_or_page() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let replay = f.wire(&e);
    for limit in [1_048_576, 64 * 1024] {
        let intake = json!({"page":{"body":"page custody"},"proof":e,"replay":replay});
        let old_wire = json!({"page":{"body":"page custody"}});
        let mut mail =
            json!({"machine":"beta","messages":[{"body":"\\n\\\"\\\\\u{0001}".repeat(100)}]});
        let budget = transport::json_size_bound(&mail)
            + transport::json_size_bound(&old_wire)
            + transport::json_size_bound(&json!(e))
            + transport::json_size_bound(&marker())
            + 128
            + 1024;
        let ordinary =
            mail["messages"][0]["body"].as_str().unwrap().to_owned() + &"x".repeat(limit - budget);
        mail["messages"][0]["body"] = json!(ordinary);
        let saved_mail = mail["messages"].clone();
        transport::attach(&f.source, "alpha", &mut mail, intake, limit);
        assert_eq!(mail["messages"], saved_mail);
        assert_eq!(mail["desk_intake"], old_wire);
        assert_eq!(mail["desk_intake_proof"], json!(e));
        assert!(mail[KEY]["proof"].is_null(), "replay drops first");
        assert_eq!(mail[KEY]["version"], 1);
        let response = json!({"id":"\\\"".repeat(128),"result":mail});
        assert!(serde_json::to_vec(&response).unwrap().len() <= limit);
        let mut args = response["result"].clone();
        args["max_response_bytes"] = json!(limit);
        let request = json!({"id":"\\\"".repeat(128),"command":"fleet.collect","args":args});
        let size = serde_json::to_vec(&request).unwrap().len() + 1;
        assert!(size <= limit);
        assert!(
            limit - size < 2048,
            "test is near the actual enclosing limit"
        );
    }
}

#[test]
fn registry_only_upgrade_preserves_1a_witnesses_marks_and_old_binary_nullable_inserts() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let witnesses: String = f
        .source
        .query_row("SELECT body FROM desk_replay_witness", [], |r| r.get(0))
        .unwrap();
    let mark: i64 = f
        .source
        .query_row("SELECT mail_order FROM desk_replay_home", [], |r| r.get(0))
        .unwrap();
    f.source.execute_batch("DROP TABLE desk_replay_send; DROP TABLE desk_replay_batch; DROP TABLE desk_replay_release; DROP TABLE desk_replay_adopted; DROP TABLE desk_replay_receipt; DROP TABLE desk_replay_vector;").unwrap();
    assert!(!ready(&f.source).unwrap());
    crate::machines::init(&f.source).unwrap();
    assert!(ready(&f.source).unwrap());
    assert_eq!(
        f.source
            .query_row("SELECT body FROM desk_replay_witness", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        witnesses
    );
    assert_eq!(
        f.source
            .query_row("SELECT mail_order FROM desk_replay_home", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        mark
    );
    let before = f.source.total_changes();
    crate::machines::init(&f.source).unwrap();
    assert_eq!(before, f.source.total_changes(), "idempotent restart");
    let wire = f.wire(&e);
    f.adopt(&wire).unwrap();
    // A rolled-back binary can still insert legacy originals without the
    // nullable 1a supplying-source/kind; those are never backfilled/adopted.
    f.desk.execute("INSERT INTO desk_intake_original(domain,owner_core,generation,incarnation,core,mailbox,id,first_order)
        SELECT domain,owner_core,generation,incarnation,core,mailbox,'legacy',first_order FROM desk_intake_original LIMIT 1",[]).unwrap();
    crate::machines::init(&f.desk).unwrap();
    assert!(f.desk.query_row("SELECT source IS NULL AND first_kind IS NULL FROM desk_intake_original WHERE id='legacy'",[],|r|r.get::<_,bool>(0)).unwrap());
}

#[test]
fn malformed_missing_and_unsupported_replay_are_not_adoption() {
    let f = Fixture::new();
    for v in [
        Value::Null,
        json!({}),
        json!({"version":2,"proof":null,"receipts":[]}),
        json!({"version":1,"proof":null,"receipts":[],"extra":true}),
        json!({"version":1,"proof":"x".repeat(WIRE),"receipts":[]}),
    ] {
        assert!(decode(&v).is_none());
        f.adopt(&v).unwrap();
        assert_eq!(count(&f.desk, "desk_replay_receipt"), 0);
    }
    assert!(decode(&marker()).is_some());
}
