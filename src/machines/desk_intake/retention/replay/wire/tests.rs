use super::*;
use crate::{mail::source, store::Store};
use std::time::Duration;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(86_400)
}
pub(super) struct Fixture {
    pub(super) dir: tempfile::TempDir,
    pub(super) home: Store,
    pub(super) source: Connection,
    pub(super) desk: Connection,
    pub(super) m: Manifest,
}
impl Fixture {
    fn new() -> Self {
        Self::with_owner(ALPHA)
    }
    pub(super) fn remote() -> Self {
        Self::with_owner("00000000-0000-4000-8000-00000000000b")
    }
    pub(super) fn local() -> Self {
        let mut f = Self::new();
        f.desk = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
        f
    }
    fn with_owner(owner: &str) -> Self {
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
        let mut m: Manifest = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,"owner_core":owner,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"local"}]})).unwrap();
        if owner != ALPHA {
            m.sources[0].intake_direction = "collect".into();
            let mut local = m.sources[0].clone();
            local.core = owner.into();
            local.name = "beta".into();
            local.ships.clear();
            local.intake_direction = "local".into();
            m.sources.push(local);
        }
        source
            .execute(
                "INSERT INTO desk_placement_record VALUES('survey','test',?1)",
                [json!({"state":"active","proposal":{"manifest":m}}).to_string()],
            )
            .unwrap();
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
    pub(super) fn capture(&self) {
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
    pub(super) fn send(&mut self, id: &str) {
        crate::mail::dispatch(
            &mut self.home,
            "mail.send",
            json!({"id":id,"from":"survey","to":"captain","kind":"notice","body":"Native content"}),
        )
        .unwrap();
        self.capture();
    }
    pub(super) fn evidence(&self) -> proof::Evidence {
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
        let mut after: i64 = self.desk.query_row(
            "SELECT COALESCE((SELECT through_seq FROM desk_intake_checkpoint WHERE domain=?1 AND source=?2),0)",
            params![self.m.domain,ALPHA], |r| r.get(0)).unwrap();
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
    pub(super) fn wire(&self, e: &proof::Evidence) -> Value {
        outgoing(&self.source, &self.m, ALPHA, false, Some(e), deadline()).unwrap()
    }
    fn receipts(&self) -> Value {
        outgoing(&self.desk, &self.m, ALPHA, true, None, deadline()).unwrap()
    }
    pub(super) fn adopt(&self, v: &Value) -> Result<()> {
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
fn provisional_adoption_rejects_bad_envelopes_but_receipts_individual_witnesses() {
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
        if matches!(field, "vector_digest" | "checkpoint") {
            assert!(f.adopt(&bad).is_err(), "{field}");
        } else {
            f.adopt(&bad).unwrap();
            assert_eq!(
                f.receipts()["receipts"][0]["rejected"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        assert_eq!(count(&f.desk, "desk_replay_adopted"), 0);
    }
    // A missing immutable original rejects only that intent, not its peers.
    let mut bad = wire.clone();
    let mut missing = bad["proof"]["witnesses"][0].clone();
    missing["origin"]["id"] = json!("missing");
    missing["intent"] = json!("missing");
    bad["proof"]["witnesses"]
        .as_array_mut()
        .unwrap()
        .push(missing);
    resign(&mut bad);
    f.adopt(&bad).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    let receipt = &f.receipts()["receipts"][0];
    assert_eq!(receipt["adopted"].as_array().unwrap().len(), 1);
    assert_eq!(receipt["rejected"][0]["intent"], "missing");
    assert_eq!(receipt["rejected"][0]["reason"], "missing_original");
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
        f.adopt(&wire).unwrap();
        assert_eq!(count(&f.desk, "desk_replay_adopted"), 0);
        assert_eq!(
            f.receipts()["receipts"][0]["rejected"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        f.desk
            .execute("DELETE FROM desk_replay_receipt", [])
            .unwrap();
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
fn persistent_pre_1a_null_kind_is_disclosed_released_and_never_blocks_good_witnesses() {
    let mut f = Fixture::new();
    for id in ["legacy-null-kind", "good-a", "good-b"] {
        f.send(id);
    }
    let e = f.evidence();
    f.desk
        .execute(
            "UPDATE desk_intake_original SET first_kind=NULL WHERE id='legacy-null-kind'",
            [],
        )
        .unwrap();
    let first = f.wire(&e);
    let rotated = f.wire(&e);
    assert_ne!(
        first["proof"]["digest"], rotated["proof"]["digest"],
        "small queues rotate too"
    );
    f.adopt(&first).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 2);
    let receipt = f.receipts();
    assert_eq!(
        receipt["receipts"][0]["adopted"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        receipt["receipts"][0]["rejected"][0]["reason"],
        "unknown_first_kind"
    );
    f.deliver_receipts(&receipt).unwrap();
    for _ in 0..3 {
        f.deliver_receipts(&receipt).unwrap();
    }
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    assert_eq!(count(&f.source, "desk_replay_import"), 0);
    assert_eq!(count(&f.source, "desk_replay_member"), 0);
    assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_replay_omission WHERE reason='rejected_unknown_first_kind')", [], |r| r.get::<_,bool>(0)).unwrap());
    // An ordinary lifecycle observation cannot re-mint the rejected M.
    f.capture();
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    f.send("good-later");
    let e = f.evidence();
    f.adopt(&f.wire(&e)).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 3);
    assert_eq!(count(&f.source, "desk_replay_home"), 1);
}

#[test]
fn replay_homes_use_capture_cache_and_quiet_contacts_send_cut_only_without_opens() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    f.wire(&e); // First contact warms this home's tuple.
    assert_eq!(count(&f.source, "desk_replay_home_cache"), 1);
    // A cached matching tuple needs no home opens at all, even after restart.
    std::fs::rename(
        f.dir.path().join("alpha"),
        f.dir.path().join("offline-alpha"),
    )
    .unwrap();
    let source = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
    let wire = outgoing(&source, &f.m, ALPHA, false, Some(&e), deadline()).unwrap();
    assert_eq!(wire["proof"]["homes"].as_array().unwrap().len(), 1);
    source
        .execute(
            "UPDATE desk_replay_home_cache SET body=json_set(body,'$.delta_upper',999999)",
            [],
        )
        .unwrap();
    assert!(outgoing(&source, &f.m, ALPHA, false, Some(&e), deadline()).is_err());
    source
        .execute("UPDATE desk_replay_witness SET adopted=1", [])
        .unwrap();
    // Restore the matching cache: quiet contacts still send cut-only proof,
    // and must not reopen the now-offline home.
    let home = &wire["proof"]["homes"][0];
    source
        .execute(
            "UPDATE desk_replay_home_cache SET body=?1",
            [home.to_string()],
        )
        .unwrap();
    let quiet = outgoing(&source, &f.m, ALPHA, false, Some(&e), deadline()).unwrap();
    assert_eq!(quiet["proof"]["homes"], wire["proof"]["homes"]);
    assert_eq!(quiet["proof"]["witnesses"], json!([]));
}

#[test]
fn multi_home_cache_refreshes_all_stale_tuples_in_one_contact_then_serves_without_opens() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    let beta = f.dir.path().join("beta");
    std::fs::create_dir(&beta).unwrap();
    f.home
        .connection()
        .execute(
            "VACUUM INTO ?1",
            [beta.join("privateer.sqlite").to_str().unwrap()],
        )
        .unwrap();
    f.source
        .execute(
            "INSERT INTO ships VALUES('survey',?1,NULL)",
            [beta.to_str().unwrap()],
        )
        .unwrap();
    // Isolate cache scheduling from coverage issuance: both consistent home
    // snapshots have the same tuple except for their opaque inventory handle.
    let mut value = json!(e);
    let mut second = value["vector"]["homes"][0].clone();
    second["home"] = json!(crate::core::hash(&json!(beta)));
    value["vector"]["homes"]
        .as_array_mut()
        .unwrap()
        .push(second);
    value["vector"]["registry_stamp"] =
        json!(proof::registry_stamp(&f.source, &f.m, ALPHA).unwrap());
    let e: proof::Evidence = serde_json::from_value(value).unwrap();
    let wire = f.wire(&e);
    assert_eq!(wire["proof"]["homes"].as_array().unwrap().len(), 2);
    assert_eq!(wire["proof"]["witnesses"].as_array().unwrap().len(), 1);
    assert_eq!(count(&f.source, "desk_replay_home_cache"), 2);
    std::fs::rename(
        f.dir.path().join("alpha"),
        f.dir.path().join("offline-alpha"),
    )
    .unwrap();
    std::fs::rename(beta, f.dir.path().join("offline-beta")).unwrap();
    assert_eq!(
        homes(&f.source, &f.m, ALPHA, &e, deadline()).unwrap().len(),
        2
    );
}

#[test]
fn busy_homes_with_advancing_delta_heads_send_replay_proof_each_contact() {
    let mut f = Fixture::new();
    let beta_path = f.dir.path().join("beta");
    let mut beta = Store::open(&beta_path).unwrap();
    crate::mail::dispatch(
        &mut beta,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    source::bind(beta.connection(), ALPHA).unwrap();
    source::init_intake(beta.connection()).unwrap();
    // A ship can have both its current and retained home in the census;
    // only its current registration participates in active membership.
    f.source
        .execute(
            "INSERT INTO ships VALUES('survey',?1,1)",
            [beta_path.to_str().unwrap()],
        )
        .unwrap();
    let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
    source::enrollment::begin(beta.connection(), &bound).unwrap();
    let baseline = source::enrollment::open(&beta_path, true).unwrap();
    source::init(&baseline).unwrap();
    source::init_intake(&baseline).unwrap();
    source::enrollment::freeze(beta.connection(), &baseline, &bound, deadline()).unwrap();
    source::enrollment::publish(beta.connection(), &baseline, &bound).unwrap();
    home::capture_ready_until(
        &beta_path,
        beta.connection(),
        &f.source,
        &f.m,
        ALPHA,
        deadline(),
    )
    .unwrap();
    for round in 0..3 {
        f.send(&format!("alpha-{round}"));
        crate::mail::dispatch(
            &mut beta,
            "mail.send",
            json!({"id":format!("beta-{round}"),"from":"survey","to":"captain","kind":"notice","body":"Native beta content"}),
        )
        .unwrap();
        home::capture_ready_until(
            &beta_path,
            beta.connection(),
            &f.source,
            &f.m,
            ALPHA,
            deadline(),
        )
        .unwrap();
        let e = f.evidence();
        let expected = json!(e);
        if round > 0 {
            let cached = f
                .source
                .prepare("SELECT body FROM desk_replay_home_cache")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(cached.len(), 2);
            for raw in cached {
                let h: Home = serde_json::from_str(&raw).unwrap();
                let current = expected["vector"]["homes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["home"] == h.home)
                    .unwrap();
                assert!(current["delta_upper"].as_i64().unwrap() > h.delta_upper);
            }
        }
        let wire = f.wire(&e);
        assert_eq!(wire["proof"]["homes"].as_array().unwrap().len(), 2);
        assert_eq!(wire["proof"]["witnesses"].as_array().unwrap().len(), 2);
        f.adopt(&wire).unwrap();
        f.deliver_receipts(&f.receipts()).unwrap();
        for id in [format!("alpha-{round}"), format!("beta-{round}")] {
            assert!(
                f.desk
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM desk_replay_adopted WHERE id=?1)",
                        [id],
                        |r| r.get::<_, bool>(0),
                    )
                    .unwrap()
            );
        }
    }
}

#[test]
fn duplicate_copy_first_origin_rejects_second_intent_and_records_omission() {
    let mut f = Fixture::new();
    f.send("copy-first");
    f.send("other");
    let e = f.evidence();
    let initial = f.wire(&e);
    let mut duplicate: Witness =
        serde_json::from_value(initial["proof"]["witnesses"][0].clone()).unwrap();
    let second: Witness = serde_json::from_value(initial["proof"]["witnesses"][1].clone()).unwrap();
    // Two independently journalled intents can assert the same copy-first
    // original. Retain both source witnesses so the negative receipt must
    // disclose an omission before either witness's custody is released.
    duplicate.intent = second.intent;
    duplicate.journal_order = second.journal_order;
    f.source
        .execute(
            "UPDATE desk_replay_witness SET body=?1 WHERE intent=?2",
            params![serde_json::to_string(&duplicate).unwrap(), duplicate.intent],
        )
        .unwrap();
    f.source.execute("UPDATE desk_replay_import SET origin=?1,digest=?2,delta_order=?3,mail_order=?4 WHERE intent=?5",
        params![serde_json::to_string(&duplicate.origin).unwrap(),duplicate.digest,duplicate.delta_order,duplicate.mail_order,duplicate.intent]).unwrap();
    f.desk
        .execute(
            "UPDATE desk_intake_original SET first_kind='copy',source='beta' WHERE id=?1",
            [&duplicate.origin.id],
        )
        .unwrap();
    let wire = f.wire(&e);
    let first = &wire["proof"]["witnesses"][0];
    let second = &wire["proof"]["witnesses"][1];
    assert_eq!(first["origin"], second["origin"]);
    f.adopt(&wire).unwrap();
    let receipts = f.receipts();
    let receipt = &receipts["receipts"][0];
    assert_eq!(receipt["adopted"], json!([first["intent"]]));
    assert_eq!(
        receipt["rejected"],
        json!([{"intent":second["intent"],"reason":"conflicting_witness"}])
    );
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    let saved: String = f
        .desk
        .query_row("SELECT body FROM desk_replay_adopted", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&saved).unwrap(), *first);
    f.deliver_receipts(&receipts).unwrap();
    assert!(f.source.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_replay_omission WHERE reason='rejected_conflicting_witness' AND first_order<=?1 AND last_order>=?1)",
        [second["journal_order"].as_i64().unwrap()], |r| r.get::<_,bool>(0),
    ).unwrap());
    for _ in 0..3 {
        f.deliver_receipts(&receipts).unwrap();
    }
    assert_eq!(count(&f.source, "desk_replay_witness"), 0);
    assert_eq!(count(&f.source, "desk_replay_import"), 0);
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
}

#[test]
fn outgoing_witness_must_match_its_committed_import_ledger_tuple() {
    let mut f = Fixture::new();
    f.send("new");
    let e = f.evidence();
    for field in ["journal_order", "delta_order", "mail_order"] {
        f.source.execute_batch("SAVEPOINT corrupt").unwrap();
        f.source
            .execute(
                &format!("UPDATE desk_replay_import SET {field}={field}+1"),
                [],
            )
            .unwrap();
        // For J, keep the row within this batch's checkpoint so the exact
        // tuple check, not the page-upper filter, is what rejects it.
        if field == "journal_order" {
            f.source
                .execute(
                    "UPDATE desk_replay_import SET journal_order=journal_order-2",
                    [],
                )
                .unwrap();
        }
        let wire = outgoing(&f.source, &f.m, ALPHA, false, Some(&e), deadline()).unwrap();
        assert!(
            wire["proof"].is_object(),
            "bad witness must not suppress cut-only proof"
        );
        assert_eq!(wire["proof"]["witnesses"], json!([]));
        assert!(f.source.query_row("SELECT EXISTS(SELECT 1 FROM desk_intake_audit WHERE stage='replay.witness' AND body LIKE '%committed import ledger%')",
            [], |r|r.get::<_,bool>(0)).unwrap());
        f.source
            .execute_batch("ROLLBACK TO corrupt; RELEASE corrupt")
            .unwrap();
    }
    assert!(f.wire(&e)["proof"].is_object());
}

#[test]
fn invalid_ledger_witness_does_not_block_its_valid_peer() {
    let mut f = Fixture::new();
    f.send("invalid");
    f.send("valid");
    let e = f.evidence();
    f.source.execute("UPDATE desk_replay_witness SET body=json_set(body,'$.digest','wrong') WHERE json_extract(body,'$.origin.id')='invalid'", []).unwrap();
    let wire = f.wire(&e);
    let witnesses = wire["proof"]["witnesses"].as_array().unwrap();
    assert_eq!(witnesses.len(), 1);
    assert_eq!(witnesses[0]["origin"]["id"], "valid");
    f.adopt(&wire).unwrap();
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
}

#[test]
fn bounded_bad_windows_advance_and_do_not_consume_valid_witness_slots() {
    for bad in [60, 120] {
        let mut f = Fixture::new();
        for n in 0..=bad {
            f.send(&format!("mail-{n}"));
        }
        let e = f.evidence();
        let rows = f
            .source
            .prepare("SELECT intent,body FROM desk_replay_witness ORDER BY intent")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for (n, (old, raw)) in rows.into_iter().enumerate() {
            let mut w: Witness = serde_json::from_str(&raw).unwrap();
            w.intent = format!("slot-{n:03}");
            f.source
                .execute(
                    "UPDATE desk_replay_import SET intent=?1 WHERE intent=?2",
                    params![w.intent, old],
                )
                .unwrap();
            if n < bad {
                w.digest = "wrong".into();
            }
            f.source
                .execute(
                    "UPDATE desk_replay_witness SET intent=?1,body=?2 WHERE intent=?3",
                    params![w.intent, serde_json::to_string(&w).unwrap(), old],
                )
                .unwrap();
        }
        let first = f.wire(&e);
        let wire = if bad > WITNESS_SCAN {
            assert_eq!(first["proof"]["witnesses"], json!([]));
            let retry = Connection::open(f.dir.path().join("source.sqlite")).unwrap();
            outgoing(&retry, &f.m, ALPHA, false, Some(&e), deadline()).unwrap()
        } else {
            first
        };
        let sent = wire["proof"]["witnesses"].as_array().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["intent"], format!("slot-{bad:03}"));
    }
}

#[test]
fn a_and_b1_closed_replay_objects_reject_unknown_fields_including_receipts() {
    let mut f = Fixture::new();
    f.send("native");
    let e = f.evidence();
    f.adopt(&f.wire(&e)).unwrap();
    let wire = f.receipts();
    assert!(decode(&wire).is_some());
    for nested in [false, true] {
        let mut v = wire.clone();
        if nested {
            v["receipts"][0]["cuts"] = json!([]);
        } else {
            v["cuts"] = json!([]);
        }
        assert!(
            decode(&v).is_none(),
            "A/B1 parser must reject the entire optional envelope"
        );
    }
}

#[test]
fn stored_body_rejection_does_not_reserve_origin_ahead_of_valid_peer() {
    let mut f = Fixture::new();
    f.send("copy-first");
    let e = f.evidence();
    f.desk
        .execute("UPDATE desk_intake_original SET first_kind='copy'", [])
        .unwrap();
    let wire = f.wire(&e);
    f.adopt(&wire).unwrap();
    let original = wire["proof"]["witnesses"][0].clone();
    let mut retry = wire;
    let mut conflicting = original.clone();
    conflicting["intent"] = json!("conflicting-intent");
    retry["proof"]["witnesses"] = json!([conflicting, original]);
    resign(&mut retry);
    f.adopt(&retry).unwrap();
    let receipts = f.receipts();
    let receipt = &receipts["receipts"][0];
    assert_eq!(receipt["adopted"], json!([original["intent"]]));
    assert_eq!(
        receipt["rejected"],
        json!([{"intent":"conflicting-intent","reason":"conflicting_witness"}])
    );
    assert_eq!(count(&f.desk, "desk_replay_adopted"), 1);
    let stored: String = f
        .desk
        .query_row("SELECT body FROM desk_replay_adopted", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), original);
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
    assert!(transport::json_size(&first) <= WIRE);
    assert!(transport::json_size(&second) <= WIRE);
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
        let budget = transport::json_size(&mail)
            + transport::json_size(&old_wire)
            + transport::json_size(&json!(e))
            + transport::json_size(&marker())
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
    f.source.execute_batch("DROP TABLE desk_replay_send; DROP TABLE desk_replay_batch; DROP TABLE desk_replay_release; DROP TABLE desk_replay_adopted; DROP TABLE desk_replay_receipt; DROP TABLE desk_replay_vector; DROP TABLE desk_replay_member; DROP TABLE desk_replay_home_cache;").unwrap();
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
