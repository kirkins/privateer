use super::super::tests::Fixture;
use super::*;
mod followup_tests;
mod review_tests;

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
    for change in ["prune", "content", "provenance", "head", "missing_home"] {
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
fn transient_inventory_pressure_and_open_errors_leave_rebuild_pending() {
    for pressure in [true, false] {
        let mut f = fixture(false);
        let v = returned(&f);
        deliver(&f, &v).unwrap();
        f.send("pending");
        f.source
            .execute("DELETE FROM desk_replay_witness", [])
            .unwrap();
        let state = || {
            f.source
                .query_row("SELECT cursor,turn FROM desk_replay_cut_return", [], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .unwrap()
        };
        let before = state();
        let omissions = count(&f.source, "desk_replay_omission");
        if pressure {
            f.source.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<50) INSERT INTO ships SELECT 'extra','gamma-'||x,NULL FROM n", []).unwrap();
        } else {
            let db = f.dir.path().join("alpha/privateer.sqlite");
            std::fs::rename(&db, f.dir.path().join("gamma.sqlite")).unwrap();
            std::fs::create_dir(&db).unwrap();
        }
        let error = step(&f.source, &f.m, ALPHA, deadline()).unwrap_err();
        if pressure {
            assert!(error.to_string().contains("backpressure"));
        }
        assert_eq!(state().0, before.0);
        assert!(
            state().1 > before.1,
            "transient failures must yield the turn"
        );
        assert_eq!(count(&f.source, "desk_replay_omission"), omissions);
        let pin: Option<String> = f
            .source
            .query_row("SELECT rebuild_cut FROM desk_replay_import", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(pin.is_none());
        if pressure {
            f.source
                .execute("DELETE FROM ships WHERE name='extra'", [])
                .unwrap();
        } else {
            let db = f.dir.path().join("alpha/privateer.sqlite");
            std::fs::remove_dir(&db).unwrap();
            std::fs::rename(f.dir.path().join("gamma.sqlite"), &db).unwrap();
        }
        let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
        assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 1);
        assert_eq!(count(&retry, "desk_replay_witness"), 1);
        assert_eq!(count(&retry, "desk_replay_omission"), omissions);
    }
}

#[test]
fn transient_home_failures_yield_to_other_cuts_then_retry_without_replay_changes() {
    use crate::{mail::source, store::Store};
    for failure in ["open", "read", "enrollment"] {
        let mut f = Fixture::remote();
        let path = f.dir.path().join("beta");
        let mut beta = Store::open(&path).unwrap();
        crate::mail::dispatch(
            &mut beta,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        source::bind(beta.connection(), ALPHA).unwrap();
        source::init_intake(beta.connection()).unwrap();
        f.source
            .execute(
                "INSERT INTO desk_home_inventory(home) VALUES(?1)",
                [path.to_str().unwrap()],
            )
            .unwrap();
        let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
        source::enrollment::begin(beta.connection(), &bound).unwrap();
        let baseline = source::enrollment::open(&path, true).unwrap();
        source::init(&baseline).unwrap();
        source::init_intake(&baseline).unwrap();
        source::enrollment::freeze(beta.connection(), &baseline, &bound, deadline()).unwrap();
        source::enrollment::publish(beta.connection(), &baseline, &bound).unwrap();
        let capture_beta = |beta: &Store| {
            home::capture_ready_until(&path, beta.connection(), &f.source, &f.m, ALPHA, deadline())
                .unwrap();
        };
        capture_beta(&beta);
        let e = f.evidence();
        f.adopt(&f.wire(&e)).unwrap();
        enable(&f, 0);
        let mut v = returned(&f);
        assert_eq!(v["receipts"].as_array().unwrap().len(), 2);
        let alpha = crate::core::hash(&json!(f.dir.path().join("alpha")));
        // Force the broken home's cut to be selected first, independent of hashes.
        v["receipts"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|r| r["home"]["home"].as_str() != Some(&alpha));
        deliver(&f, &v).unwrap();
        crate::mail::dispatch(&mut beta, "mail.send", json!({"id":"beta-new","from":"survey","to":"captain","kind":"notice","body":"Native content"})).unwrap();
        capture_beta(&beta);
        f.send("alpha-new");
        f.source
            .execute("DELETE FROM desk_replay_witness", [])
            .unwrap();
        let state = || {
            let cuts: Vec<(i64, i64)> = f
                .source
                .prepare("SELECT cursor,turn FROM desk_replay_cut_return ORDER BY seq")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let pins: Vec<Option<String>> = f
                .source
                .prepare("SELECT rebuild_cut FROM desk_replay_import ORDER BY journal_order")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            (cuts, pins, count(&f.source, "desk_replay_omission"))
        };
        let before = state();
        let db = f.dir.path().join("alpha/privateer.sqlite");
        if failure == "open" {
            std::fs::rename(&db, f.dir.path().join("gamma.sqlite")).unwrap();
            std::fs::create_dir(&db).unwrap();
        } else if failure == "read" {
            f.home
                .connection()
                .execute_batch("ALTER TABLE mail_source_origin RENAME TO saved_origin")
                .unwrap();
        } else {
            f.home.connection().execute_batch("CREATE TABLE saved_snapshot AS SELECT * FROM mail_source_snapshot; DELETE FROM mail_source_snapshot;").unwrap();
        }
        assert!(
            step(&f.source, &f.m, ALPHA, deadline()).is_err(),
            "{failure}"
        );
        let after = state();
        assert_eq!(after.0[0].0, before.0[0].0);
        assert!(after.0[0].1 > before.0[0].1);
        assert_eq!(after.1, before.1, "tentative pin must roll back");
        assert_eq!(after.2, before.2);
        assert_eq!(count(&f.source, "desk_replay_witness"), 0);
        // The broken home stays broken through the next idle pass.
        let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
        assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 1);
        assert_eq!(count(&retry, "desk_replay_witness"), 1);
        assert!(retry.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_witness WHERE json_extract(body,'$.origin.id')='beta-new')", [], |r| r.get::<_, bool>(0)).unwrap());
        if failure == "open" {
            std::fs::remove_dir(&db).unwrap();
            std::fs::rename(f.dir.path().join("gamma.sqlite"), &db).unwrap();
        } else if failure == "read" {
            f.home
                .connection()
                .execute_batch("ALTER TABLE saved_origin RENAME TO mail_source_origin")
                .unwrap();
        } else {
            f.home.connection().execute_batch("INSERT INTO mail_source_snapshot SELECT * FROM saved_snapshot; DROP TABLE saved_snapshot;").unwrap();
        }
        assert_eq!(step(&retry, &f.m, ALPHA, deadline()).unwrap(), 1);
        assert_eq!(count(&retry, "desk_replay_witness"), 2);
        assert_eq!(count(&retry, "desk_replay_omission"), before.2);
    }
}

#[test]
fn missing_database_is_a_lasting_unavailable_range() {
    let mut f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.send("missing-db");
    f.source
        .execute("DELETE FROM desk_replay_witness", [])
        .unwrap();
    std::fs::rename(
        f.dir.path().join("alpha/privateer.sqlite"),
        f.dir.path().join("gamma.sqlite"),
    )
    .unwrap();
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 1);
    assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_omission WHERE reason='rebuild_unavailable')", [], |r| r.get::<_, bool>(0)).unwrap());
    let (cursor, order): (i64, i64) = f
        .source
        .query_row(
            "SELECT cursor,journal_order FROM desk_replay_cut_return,desk_replay_import",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cursor, order);
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
}

#[test]
fn empty_candidate_window_only_advances_turn_without_inventory_or_home() {
    let f = fixture(false);
    deliver(&f, &returned(&f)).unwrap();
    f.source
        .execute_batch("DROP TABLE ships; DROP TABLE desk_home_inventory;")
        .unwrap();
    std::fs::rename(f.dir.path().join("alpha"), f.dir.path().join("gamma")).unwrap();
    let before: (i64, i64) = f
        .source
        .query_row("SELECT cursor,turn FROM desk_replay_cut_return", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 0);
    let after: (i64, i64) = f
        .source
        .query_row("SELECT cursor,turn FROM desk_replay_cut_return", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(after.0, before.0);
    assert!(after.1 > before.1);
    assert_eq!(count(&f.source, "desk_replay_omission"), 0);
}

#[test]
fn two_exposures_share_the_same_home_cut_pin_without_false_omissions() {
    let mut f = fixture(false);
    enable(&f, 1);
    let v = returned(&f);
    assert_eq!(v["receipts"].as_array().unwrap().len(), 2);
    let a: Receipt = serde_json::from_value(v["receipts"][0].clone()).unwrap();
    let b: Receipt = serde_json::from_value(v["receipts"][1].clone()).unwrap();
    assert_ne!(a.cut_digest, b.cut_digest);
    assert_eq!(json!(a.cut()), json!(b.cut()));
    deliver(&f, &v).unwrap();
    f.send("shared");
    for _ in 0..2 {
        // Force each exposure to exercise rebuild, not the already-saved fast path.
        f.source
            .execute("DELETE FROM desk_replay_witness", [])
            .unwrap();
        assert_eq!(step(&f.source, &f.m, ALPHA, deadline()).unwrap(), 1);
        assert_eq!(count(&f.source, "desk_replay_witness"), 1);
        assert_eq!(count(&f.source, "desk_replay_omission"), 0);
    }
    let pin: String = f
        .source
        .query_row("SELECT rebuild_cut FROM desk_replay_import", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pin, crate::core::hash(&json!(a.cut())));
}

#[test]
fn retained_replay_batch_checks_cut_after_source_proof_reclamation() {
    for change in ["none", "vector", "checkpoint", "home"] {
        // Quiet cut-only proofs have no retained batch. Exercise the actual
        // witness-bearing batch custody path before capturing this cut.
        let mut f = Fixture::remote();
        f.send("before-cut");
        let e = f.evidence();
        f.adopt(&f.wire(&e)).unwrap();
        enable(&f, 0);
        let mut v = returned(&f);
        f.source
            .execute("DELETE FROM desk_source_proof", [])
            .unwrap();
        assert!(count(&f.source, "desk_replay_batch") > 0);
        f.send("advanced");
        match change {
            "vector" => v["receipts"][0]["vector_digest"] = json!("f".repeat(64)),
            "checkpoint" => v["receipts"][0]["checkpoint"]["issued"] = json!(i64::MAX),
            "home" => v["receipts"][0]["home"]["mail_upper"] = json!(2),
            _ => {}
        }
        resign(&mut v);
        deliver(&f, &v).unwrap();
        assert_eq!(
            count(&f.source, "desk_replay_cut_return"),
            i64::from(change == "none"),
            "{change}"
        );
    }
    // The historical check honestly lapses after both custody records expire.
    let f = fixture(false);
    let mut v = returned(&f);
    f.source
        .execute("DELETE FROM desk_source_proof", [])
        .unwrap();
    f.source
        .execute("DELETE FROM desk_replay_batch", [])
        .unwrap();
    v["receipts"][0]["vector_digest"] = json!("f".repeat(64));
    resign(&mut v);
    deliver(&f, &v).unwrap();
    assert_eq!(count(&f.source, "desk_replay_cut_return"), 1);
}

#[test]
fn envelope_trims_receipts_before_advancing_cursor_and_sends_pending_suffix() {
    let f = fixture(false);
    for n in 1..8 {
        enable(&f, n);
    }
    // Make each receipt exactly the individual cap; all eight plus envelope
    // metadata cannot fit, despite every individual receipt being valid.
    let raw: String = f
        .desk
        .query_row("SELECT body FROM desk_exposure_cut LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut cut: cuts::SavedCut = serde_json::from_str(&raw).unwrap();
    let mut r: Receipt = serde_json::from_value(returned(&f)["receipts"][0].clone()).unwrap();
    let padding = RECEIPT_BYTES - transport::json_size(&json!(r));
    cut.home.mailbox.push_str(&"x".repeat(padding));
    r.home = cut.home.clone();
    assert_eq!(transport::json_size(&json!(r)), RECEIPT_BYTES);
    f.desk
        .execute(
            "UPDATE desk_exposure_cut SET mailbox=?1,body=?2",
            params![cut.home.mailbox, serde_json::to_string(&cut).unwrap()],
        )
        .unwrap();
    let first = returned(&f);
    assert_eq!(first["receipts"].as_array().unwrap().len(), 7);
    assert!(decode(&first).is_some());
    let last: Receipt = serde_json::from_value(first["receipts"][6].clone()).unwrap();
    let cursor: String = f
        .desk
        .query_row("SELECT cursor FROM desk_replay_cut_send", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&cursor).unwrap(),
        json!(last.key())
    );
    let second = returned(&f);
    assert!(decode(&second).is_some());
    let seen: std::collections::BTreeSet<_> = [first, second]
        .iter()
        .flat_map(|v| v["receipts"].as_array().unwrap())
        .map(|r| r["exposure"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(seen.len(), 8, "the trimmed receipt remains pending");
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
