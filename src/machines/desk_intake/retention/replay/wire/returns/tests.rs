use super::super::tests::Fixture;
use super::*;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
fn deadline() -> Instant {
    Instant::now() + std::time::Duration::from_secs(86_400)
}
fn count(c: &Connection, t: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
        .unwrap()
}
fn enable(f: &Fixture, n: i64) {
    let now = transport::process_clock();
    let b = json!({"domain":f.m.domain,"owner_core":f.m.owner_core,"generation":f.m.generation,
        "incarnation":f.m.incarnation,"manifest_revision":f.m.manifest_revision,
        "manifest_hash":crate::core::hash(&json!(f.m)),"mode":"fresh_replay","replay_policy":"open_and_new",
        "switch_on_ms":now.wall_ms+n,"switch_on":[]});
    f.desk
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS door_desk_exposure(door TEXT PRIMARY KEY,binding TEXT)",
        )
        .unwrap();
    f.desk
        .execute(
            "INSERT INTO door_desk_exposure VALUES(?1,?2)",
            params![format!("door-{n}"), b.to_string()],
        )
        .unwrap();
    f.desk
        .execute(
            "UPDATE desk_intake_checkpoint SET received_at=?1,tick_ms=?2,session=?3,clock_valid=1",
            params![now.wall_ms, now.tick_ms, now.session],
        )
        .unwrap();
    cuts::capture(&f.desk, &f.m, &b).unwrap();
}
fn fixture(local: bool) -> Fixture {
    let f = if local {
        Fixture::local()
    } else {
        Fixture::remote()
    };
    let e = f.evidence();
    f.adopt(&f.wire(&e)).unwrap();
    enable(&f, 0);
    assert_eq!(count(&f.desk, "desk_exposure_cut"), 1);
    f
}
fn returned(f: &Fixture) -> Value {
    outgoing(&f.desk, &f.m, ALPHA, true).unwrap()
}
fn deliver(f: &Fixture, v: &Value) -> Result<()> {
    receive(&f.source, &f.m, ALPHA, false, v, deadline())
}
fn resign(v: &mut Value) {
    let r: Receipt = serde_json::from_value(v["receipts"][0].clone()).unwrap();
    v["receipts"][0]["cut_digest"] = json!(r.digest());
}

#[test]
fn return_is_immutable_quiet_bounded_rotating_and_keeps_closed_a_wire() {
    let mut f = fixture(false);
    let first = returned(&f);
    for n in 1..12 {
        enable(&f, n);
    }
    f.send("later");
    let e = f.evidence();
    f.adopt(&f.wire(&e)).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..2 {
        let v = returned(&f);
        assert_eq!(v["receipts"].as_array().unwrap().len(), 8);
        assert!(transport::json_size(&v) <= WIRE);
        for r in v["receipts"].as_array().unwrap() {
            assert!(transport::json_size(r) <= RECEIPT_BYTES);
            assert_eq!(
                r["checkpoint"], first["receipts"][0]["checkpoint"],
                "never send latest vector instead of immutable cut"
            );
            seen.insert(r["exposure"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(seen.len(), 12);
    let a = super::super::outgoing(&f.desk, &f.m, ALPHA, true, None, deadline()).unwrap();
    let mut bundle = json!({});
    bundle[super::super::KEY] = a.clone();
    bundle[KEY] = first;
    let frame = json!({"ordinary":"unchanged"});
    let internal = proof::with_sidecar(&frame, &Value::Null, &bundle);
    assert_eq!(internal["replay"], a);
    assert!(super::super::decode(&a).is_some());
    assert!(internal["cut_returns"].is_object());
    let mut unknown = internal["cut_returns"].clone();
    unknown["unknown"] = json!(true);
    assert!(decode(&unknown).is_none());
    let receipt = internal["cut_returns"]["receipts"][0].clone();
    assert!(decode(&json!({"version":1,"receipts":vec![receipt;9]})).is_none());
}

#[test]
fn enclosing_budget_drops_cut_returns_before_pages_and_resends_them() {
    let f = fixture(false);
    let returned = returned(&f);
    let r = returned["receipts"][0].clone();
    let cuts = json!({"version":1,"receipts":vec![r;8]});
    let a = super::super::marker();
    let frame = json!({"protocol":"fleet.door-mail.v1","page":{"retained":"custody"},"replay":a,"cut_returns":cuts});
    for limit in [16 * 1024, 64 * 1024, 1_048_576] {
        let mut body = json!({"messages":[{"body":"\"\\\n".repeat((limit-3000)/6)}]});
        let ordinary = body["messages"].clone();
        transport::attach(&f.desk, "alpha", &mut body, frame.clone(), limit);
        assert_eq!(body["messages"], ordinary);
        assert!(
            body.get(KEY).is_none(),
            "optional cut return should yield room first"
        );
        assert_eq!(body["desk_intake"]["page"], frame["page"]);
        assert!(body["desk_intake"].get("cut_returns").is_none());
        assert_eq!(body[super::super::KEY], a);
        let request = json!({"id":"\\\"".repeat(32),"command":"fleet.collect","args":body,"max_response_bytes":limit});
        assert!(transport::json_size(&request) <= limit);
    }
    let mut body = json!({"messages":[]});
    transport::attach(&f.desk, "alpha", &mut body, frame, 1_048_576);
    assert_eq!(body[KEY], cuts);
    assert_eq!(
        super::outgoing(&f.desk, &f.m, ALPHA, true).unwrap(),
        returned
    );
}

#[test]
fn returned_cut_checks_forgery_ahead_nonce_vector_and_conflict_without_recapture() {
    for change in [
        "forged", "ahead_j", "ahead_d", "ahead_m", "nonce", "home", "vector", "binding", "exposure",
    ] {
        let f = fixture(false);
        let mut v = returned(&f);
        match change {
            "forged" => v["receipts"][0]["cut_digest"] = json!("wrong"),
            "ahead_j" => v["receipts"][0]["checkpoint"]["upper"] = json!(i64::MAX),
            "ahead_d" => v["receipts"][0]["home"]["delta_upper"] = json!(i64::MAX),
            "ahead_m" => v["receipts"][0]["home"]["mail_upper"] = json!(i64::MAX),
            "nonce" => v["receipts"][0]["home"]["nonce"] = json!(uuid::Uuid::new_v4().to_string()),
            "home" => v["receipts"][0]["home"]["home"] = json!("f".repeat(64)),
            "vector" => v["receipts"][0]["vector_digest"] = json!("f".repeat(64)),
            "binding" => v["receipts"][0]["binding"] = json!("wrong"),
            _ => v["receipts"][0]["exposure"] = json!("not-a-binding-hash"),
        }
        if change != "forged" {
            resign(&mut v);
        }
        deliver(&f, &v).unwrap();
        assert_eq!(count(&f.source, "desk_replay_cut_return"), 0, "{change}");
        assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='replay.cut_return')",[],|r|r.get::<_,bool>(0)).unwrap());
    }
    let mut f = fixture(false);
    let v = returned(&f);
    deliver(&f, &v).unwrap();
    let before = f.source.total_changes();
    deliver(&f, &v).unwrap();
    assert_eq!(
        f.source.total_changes(),
        before,
        "identical resend is read-only"
    );
    f.send("witnessless");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    let mut other = v.clone();
    other["receipts"][0]["checkpoint"]["id"] = json!(uuid::Uuid::new_v4().to_string());
    resign(&mut other);
    deliver(&f, &other).unwrap();
    let (body, conflict): (String, bool) = f
        .source
        .query_row(
            "SELECT body,conflict FROM desk_replay_cut_return",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        v["receipts"][0]
    );
    assert!(conflict);
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 0);
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
}

#[test]
fn rebuild_is_incremental_restartable_and_uses_same_returned_cut_local_and_remote() {
    for local in [false, true] {
        let mut f = fixture(local);
        let v = returned(&f);
        deliver(&f, &v).unwrap();
        // Simulate witness pressure at import, not a fresh candidate remint.
        f.source
            .execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
            INSERT INTO desk_replay_witness SELECT 'filler','filler',CAST(x AS TEXT),'{}',0 FROM n",
                [WITNESSES],
            )
            .unwrap();
        for n in 0..52 {
            f.send(&format!("new-{n}"));
        }
        assert_eq!(count(&f.source, "desk_replay_import"), 52);
        assert_eq!(
            step(&f.source, &f.m, ALPHA, deadline()).unwrap(),
            1,
            "pressure must not move the candidate cursor"
        );
        let cursor: i64 = f
            .source
            .query_row("SELECT cursor FROM desk_replay_cut_return", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            cursor,
            v["receipts"][0]["checkpoint"]["upper"].as_i64().unwrap()
        );
        f.source
            .execute("DELETE FROM desk_replay_witness WHERE domain='filler'", [])
            .unwrap();
        let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
        assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 50);
        assert_eq!(count(&retry, "desk_replay_witness"), 50);
        assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 2);
        assert_eq!(count(&retry, "desk_replay_witness"), 52);
        let r: Receipt = serde_json::from_value(v["receipts"][0].clone()).unwrap();
        let pin = crate::core::hash(&json!(r.cut()));
        let all_pinned: bool = retry
            .query_row(
                "SELECT COUNT(*)=52 FROM desk_replay_import WHERE rebuild_cut=?1",
                [pin],
                |r| r.get(0),
            )
            .unwrap();
        assert!(all_pinned);
        assert!(
            count(&retry, "desk_replay_omission") > 0,
            "lasting pressure ranges survive recovery"
        );
        assert_eq!(count(&f.desk, "desk_subscription_ref"), 0);
        assert_eq!(count(&f.desk, "desk_subscription_progress"), 0);
    }
}

#[test]
fn unavailable_rebuilds_are_lasting_ranges_not_replacement_cuts() {
    for change in [
        "prune",
        "content",
        "provenance",
        "nonce",
        "head",
        "missing_home",
    ] {
        let mut f = fixture(false);
        let v = returned(&f);
        deliver(&f, &v).unwrap();
        f.send("unavailable");
        f.source
            .execute("DELETE FROM desk_replay_witness", [])
            .unwrap();
        match change {
            "prune" => {
                f.home
                    .connection()
                    .execute("DELETE FROM mail WHERE id='unavailable'", [])
                    .unwrap();
            }
            "content" => {
                f.home
                    .connection()
                    .execute("UPDATE mail SET body='changed' WHERE id='unavailable'", [])
                    .unwrap();
            }
            "provenance" => {
                f.home.connection().execute("UPDATE mail_source_origin SET provenance='\"Unresolved\"' WHERE id='unavailable'",[]).unwrap();
            }
            "nonce" => {
                f.home
                    .connection()
                    .execute("DELETE FROM mail_source_snapshot", [])
                    .unwrap();
            }
            "head" => {
                f.home
                    .connection()
                    .execute(
                        "UPDATE sqlite_sequence SET seq=0 WHERE name='mail_source_outbox'",
                        [],
                    )
                    .unwrap();
            }
            _ => {
                f.source.execute("DELETE FROM ships", []).unwrap();
            }
        }
        assert_eq!(
            step(&f.source, &f.m, ALPHA, deadline()).unwrap(),
            1,
            "{change}"
        );
        assert_eq!(count(&f.source, "desk_replay_witness"), 0, "{change}");
        assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_omission WHERE reason='rebuild_unavailable' AND first_order<=last_order)",[],|r|r.get::<_,bool>(0)).unwrap(),"{change}");
        assert_eq!(
            count(&f.source, "desk_replay_import"),
            1,
            "never evict custody"
        );
    }
}

#[test]
fn b1_registry_upgrade_is_additive_idempotent_and_uses_indexed_work() {
    let mut f = fixture(false);
    f.send("kept");
    let before: String = f
        .source
        .query_row("SELECT body FROM desk_replay_witness", [], |r| r.get(0))
        .unwrap();
    f.source.execute_batch("DROP TABLE desk_replay_cut_return; DROP TABLE desk_replay_cut_send;
        DROP INDEX desk_replay_rebuild; DELETE FROM desk_replay_budget WHERE name='desk_replay_cut_return';").unwrap();
    assert!(!super::super::ready(&f.source).unwrap());
    crate::machines::init(&f.source).unwrap();
    assert!(super::super::ready(&f.source).unwrap());
    let after: String = f
        .source
        .query_row("SELECT body FROM desk_replay_witness", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    let changes = f.source.total_changes();
    crate::machines::init(&f.source).unwrap();
    assert_eq!(changes, f.source.total_changes());
    let plan = f.source.prepare("EXPLAIN QUERY PLAN SELECT intent,journal_order FROM desk_replay_import INDEXED BY desk_replay_rebuild WHERE domain=?1 AND source=?2 AND binding=?3 AND home=?4 AND mailbox=?5 AND nonce=?6 AND journal_order>?7 ORDER BY journal_order LIMIT 50").unwrap()
        .query_map(params![f.m.domain,ALPHA,binding(&f.m).unwrap(),"alpha","beta","gamma",0],|r|r.get::<_,String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap().join(" ");
    assert!(
        plan.contains("desk_replay_rebuild")
            && plan.contains("journal_order>?")
            && !plan.contains("TEMP"),
        "{plan}"
    );
}

#[test]
fn return_storage_cap_backpressures_without_eviction_or_ordinary_custody_changes() {
    let f = fixture(false);
    let v = returned(&f);
    f.source.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
        INSERT INTO desk_replay_cut_return(domain,source,binding,exposure,home,mailbox,nonce,body,cursor)
        SELECT 'filler','filler','filler',CAST(x AS TEXT),'filler','filler','filler','{}',0 FROM n",[RECORDS]).unwrap();
    let before = count(&f.source, "desk_source_event");
    assert!(
        deliver(&f, &v)
            .unwrap_err()
            .to_string()
            .contains("backpressure")
    );
    assert_eq!(count(&f.source, "desk_replay_cut_return"), RECORDS);
    assert_eq!(count(&f.source, "desk_source_event"), before);
}
