use super::*;
use crate::{mail::source, store::Store};
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
struct Fixture {
    _dir: tempfile::TempDir,
    r: Connection,
    h: Store,
    m: Manifest,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let r = Connection::open(dir.path().join("registry.sqlite")).unwrap();
        crate::machines::init(&r).unwrap();
        r.execute_batch("CREATE TABLE ships(name TEXT,home TEXT,retired_ms INTEGER); CREATE TABLE door_reader_identity(only INTEGER PRIMARY KEY,id TEXT)").unwrap();
        r.execute("INSERT INTO door_reader_identity VALUES(1,?1)", [ALPHA])
            .unwrap();
        let path = dir.path().join("alpha");
        let mut h = Store::open(&path).unwrap();
        source::bind(h.connection(), ALPHA).unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"survey","location":"here"}),
        )
        .unwrap();
        h.connection()
            .execute(
                "INSERT INTO peer_endpoints(name,config_json) VALUES('machine-beta',?1)",
                [json!({"transport":"collected"}).to_string()],
            )
            .unwrap();
        crate::mail::dispatch(
            &mut h,
            "ship.put",
            json!({"name":"beta","location":{"peer":"machine-beta"}}),
        )
        .unwrap();
        crate::mail::deliver_with_provenance(&h,&json!({"id":"legacy","from":"beta","to":"captain","kind":"notice","body":"Legacy history"}), &source::Provenance::Unresolved).unwrap();
        h.connection()
            .execute("UPDATE mail SET state='acknowledged' WHERE id='legacy'", [])
            .unwrap();
        source::observe(h.connection(), "legacy", None);
        r.execute(
            "INSERT INTO ships VALUES('survey',?1,NULL)",
            [path.to_str().unwrap()],
        )
        .unwrap();
        let m = serde_json::from_value(json!({"domain":"survey","manifest_revision":1,"max_source_age_ms":900000,"owner_core":ALPHA,"generation":1,"incarnation":uuid::Uuid::new_v4().to_string(),"coordinator":ALPHA,"sources":[{"core":ALPHA,"name":"alpha","ships":["survey"],"intake_direction":"local"}]})).unwrap();
        baseline::enroll_home(&path, &r, &m, ALPHA, deadline(), 100, || Ok(())).unwrap();
        let f = Self {
            _dir: dir,
            r,
            h,
            m,
            path,
        };
        f.capture();
        f
    }
    fn capture(&self) {
        for _ in 0..4 {
            home::capture_ready_until(
                &self.path,
                self.h.connection(),
                &self.r,
                &self.m,
                ALPHA,
                deadline(),
            )
            .unwrap();
        }
        reconcile::save(&self.r, &self.m, ALPHA, deadline()).unwrap();
    }
    fn args(&self) -> Value {
        json!({"digest":plan(&self.r,&self.m,ALPHA).unwrap()["digest"],"reason":"Leave closed legacy observations in history"})
    }
    fn apply(&self) {
        apply(&self.r, &Caller::Operator, &self.m, ALPHA, &self.args()).unwrap();
    }
    fn evidence(&self) -> proof::Evidence {
        self.capture();
        let cp = checkpoint(&self.r, &self.m, ALPHA, &uuid::Uuid::new_v4().to_string()).unwrap();
        proof::issue(&self.r, &self.m, &cp, deadline()).unwrap();
        let e = proof::outgoing(&self.r, &self.m, &cp, deadline())
            .unwrap()
            .unwrap();
        let p = page(&self.r, &self.m, &cp, 0, 50, BYTES).unwrap();
        accept(
            &self.r,
            &self.m,
            &p,
            &Clock {
                session: "alpha",
                wall_ms: 1000,
                tick_ms: 1000,
            },
        )
        .unwrap();
        proof::receive(&self.r, &self.m, ALPHA, Some(e.clone())).unwrap();
        e
    }
}
#[test]
fn closed_legacy_cut_is_read_only_digest_bound_revocable_and_never_projected() {
    let f = Fixture::new();
    let p = reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline()).unwrap();
    assert!(!p.candidate);
    let before = f.r.total_changes();
    f.r.pragma_update(None, "query_only", true).unwrap();
    let args = f.args();
    assert_eq!(before, f.r.total_changes());
    f.r.pragma_update(None, "query_only", false).unwrap();
    apply(&f.r, &Caller::Operator, &f.m, ALPHA, &args).unwrap();
    let e = f.evidence();
    assert!(proof::eligible(&f.r, &f.m, ALPHA, &e).unwrap());
    assert!(
        proof::local_current(&f.r, &f.m, ALPHA, deadline())
            .is_ok_and(|v| matches!(v, proof::LocalCurrent::Current))
    );
    assert_eq!(
        f.r.query_row("SELECT COUNT(*) FROM desk_intake_fact", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    revoke(
        &f.r,
        &Caller::Operator,
        &f.m,
        ALPHA,
        &json!({"reason":"Withdraw test boundary"}),
    )
    .unwrap();
    assert!(!proof::eligible(&f.r, &f.m, ALPHA, &e).unwrap());
    assert!(
        !reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
    assert_eq!(f.r.query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE stage IN ('legacy.apply','legacy.revoke')",[],|r|r.get::<_,i64>(0)).unwrap(),2);
}
#[test]
fn cut_status_and_public_coverage_are_separate_dated_and_revocable() {
    let f = Fixture::new();
    coverage::install_placement_manifest(
        &f.r,
        &f.m.domain,
        900000,
        &[(ALPHA.into(), vec!["survey".into()])],
    )
    .unwrap();
    let context = crate::door::scoped::DelegatedContext::from_gateway(&json!({
        "version":1,"client":uuid::Uuid::new_v4().to_string(),"resource":"door:alpha",
        "scopes":["mail:read"],"ships":null,"result_id":uuid::Uuid::new_v4().to_string()
    }))
    .unwrap();
    let clock = Clock {
        session: "alpha",
        wall_ms: 1000,
        tick_ms: 1000,
    };
    let snapshot = || completion::snapshot(&f.r, &f.m, ALPHA, &clock, deadline()).unwrap();
    let public = |status| coverage::subscription_coverage(&f.r, &f.m, &context, &[status]).unwrap();
    let absent = public(snapshot());
    coverage::validate(&absent, &[]).unwrap();
    assert!(absent["sources"][0].get("history").is_none());
    f.apply();
    let e = f.evidence();
    let before = f.r.total_changes();
    let status = snapshot();
    assert_eq!(f.r.total_changes(), before, "status is read only");
    let cut = &status["legacy_cut"];
    assert_eq!(cut["digest"], json!(e)["cut_digest"]);
    assert!(cut["cut_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(cut["homes"][0]["count"], 1);
    assert_eq!(cut["released_homes"], json!([]));
    assert_eq!(cut["proof_holds"], true, "{status}");
    let shown = public(status.clone());
    coverage::validate(&shown, &["survey", "alpha", "legacy"]).unwrap();
    let source = &shown["sources"][0];
    assert_eq!(source["history"], "legacy_cut");
    assert!(source["history_as_of"].as_i64().unwrap() > 0);
    for field in [
        "legacy_cut",
        "homes",
        "count",
        "digest",
        "released_homes",
        "cut_at_ms",
    ] {
        assert!(source.get(field).is_none(), "{shown}");
    }
    for bad in [json!({"count":1}), json!("alpha"), json!("/var/tmp/alpha")] {
        let mut invalid = shown.clone();
        invalid["sources"][0]["history"] = bad;
        assert!(coverage::validate(&invalid, &[]).is_err());
    }
    let mut undated = shown.clone();
    undated["sources"][0]
        .as_object_mut()
        .unwrap()
        .remove("history_as_of");
    assert!(coverage::validate(&undated, &[]).is_err());
    // The owner receives only sanitized cut metadata on the existing proof
    // sidecar; its observation date is its own checkpoint receipt clock.
    f.r.execute(
        "UPDATE door_reader_identity SET id=?1",
        ["00000000-0000-4000-8000-00000000000b"],
    )
    .unwrap();
    let remote = proof::legacy_status(&f.r, &f.m, ALPHA).unwrap().unwrap();
    assert_eq!(remote["observed_at"], 1000);
    assert_eq!(remote["homes"], cut["homes"]);
    proof::receive(&f.r, &f.m, ALPHA, None).unwrap();
    let withdrawn = proof::legacy_status(&f.r, &f.m, ALPHA).unwrap().unwrap();
    assert!(withdrawn["observed_at"].is_null());
    let withdrawn_public = public(json!({"id":ALPHA,"state":"history_gap","legacy_cut":withdrawn}));
    assert!(withdrawn_public["sources"][0].get("history").is_none());
    proof::receive(&f.r, &f.m, ALPHA, Some(e.clone())).unwrap();
    f.r.execute("UPDATE desk_intake_checkpoint SET barrier='newer'", [])
        .unwrap();
    assert!(
        proof::legacy_status(&f.r, &f.m, ALPHA).unwrap().unwrap()["observed_at"].is_null(),
        "old cut evidence must not borrow a newer barrier's date"
    );
    f.r.execute("UPDATE door_reader_identity SET id=?1", [ALPHA])
        .unwrap();
    revoke(
        &f.r,
        &Caller::Operator,
        &f.m,
        ALPHA,
        &json!({"reason":"Withdraw boundary"}),
    )
    .unwrap();
    let removed = public(snapshot());
    coverage::validate(&removed, &[]).unwrap();
    assert!(removed["sources"][0].get("history").is_none(), "{removed}");
    assert!(snapshot().get("legacy_cut").is_none());
    // Durable time is not lost when the bounded transport audit is trimmed.
    f.apply();
    f.r.execute("DELETE FROM desk_intake_audit", []).unwrap();
    assert!(
        summary(&f.r, &f.m, ALPHA)
            .unwrap()
            .unwrap()
            .cut_at_ms
            .is_some()
    );
}

#[test]
fn strict_v3_interop_omits_uncut_metadata_and_fails_closed_for_cut_metadata() {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OldV3 {
        #[serde(rename = "version")]
        _version: u8,
        #[serde(rename = "cut_digest")]
        _cut_digest: Option<String>,
        #[serde(rename = "checkpoint")]
        _checkpoint: Checkpoint,
        #[serde(rename = "vector")]
        _vector: reconcile::Proof,
        #[serde(rename = "valid")]
        _valid: bool,
    }
    let f = Fixture::new();
    f.apply();
    let cut_wire = json!(f.evidence());
    assert!(serde_json::from_value::<OldV3>(cut_wire.clone()).is_err());
    let mut uncut_wire = cut_wire;
    uncut_wire.as_object_mut().unwrap().remove("legacy");
    uncut_wire["cut_digest"] = Value::Null;
    assert!(serde_json::from_value::<OldV3>(uncut_wire).is_ok());
}

#[test]
fn stale_digest_integrity_and_agents_are_refused() {
    let f = Fixture::new();
    let args = f.args();
    let agent = Caller::Agent {
        actor: "survey".into(),
    };
    assert!(apply(&f.r, &agent, &f.m, ALPHA, &args).is_err());
    assert!(revoke(&f.r, &agent, &f.m, ALPHA, &json!({})).is_err());
    assert!(dispatch(&agent, "machine.desk.legacy.apply", &json!({})).is_err());
    f.r.execute("INSERT INTO desk_source_revision VALUES('survey',?1,1) ON CONFLICT(domain,source) DO UPDATE SET revision=revision+1",[ALPHA]).unwrap();
    assert!(
        apply(&f.r, &Caller::Operator, &f.m, ALPHA, &args)
            .unwrap_err()
            .to_string()
            .contains("digest mismatch")
    );
    gap(&f.r, &f.m, ALPHA, "integrity_gap").unwrap();
    assert!(plan(&f.r, &f.m, ALPHA).is_err());
}
#[test]
fn new_unresolved_above_mail_highwater_reopen_and_unread_block() {
    for state in ["open", "unread", "new"] {
        let f = Fixture::new();
        f.apply();
        let e = f.evidence();
        match state {
            "open" => {
                f.h.connection()
                    .execute("UPDATE mail SET state='open' WHERE id='legacy'", [])
                    .unwrap();
            }
            "unread" => {
                f.h.connection()
                    .execute("UPDATE mail SET state=NULL WHERE id='legacy'", [])
                    .unwrap();
            }
            _ => {
                crate::mail::deliver_with_provenance(&f.h,&json!({"id":"new","from":"beta","to":"captain","kind":"notice","body":"New unresolved"}), &source::Provenance::Unresolved).unwrap();
                f.h.connection()
                    .execute("UPDATE mail SET state='acknowledged' WHERE id='new'", [])
                    .unwrap();
            }
        }
        assert!(
            !reconcile::revalidate(
                &f.r,
                &f.m,
                ALPHA,
                &serde_json::from_value(json!(e)["vector"].clone()).unwrap(),
                deadline()
            )
            .unwrap(),
            "{state}"
        );
        f.capture();
        assert!(
            !reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline())
                .unwrap()
                .candidate,
            "{state}"
        );
    }
}
#[test]
fn legacy_must_be_closed_and_read_before_cut_and_registry_set_is_frozen() {
    let f = Fixture::new();
    f.h.connection()
        .execute("UPDATE mail SET state='open' WHERE id='legacy'", [])
        .unwrap();
    assert!(plan(&f.r, &f.m, ALPHA).is_err());
    f.h.connection()
        .execute("UPDATE mail SET state='acknowledged' WHERE id='legacy'", [])
        .unwrap();
    f.apply();
    assert!(!unresolved(&f.r, &f.m, ALPHA).unwrap());
    let raw: String =
        f.r.query_row("SELECT event FROM desk_home_receipt LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut event: Value = serde_json::from_str(&raw).unwrap();
    for field in ["unread", "pending"] {
        event[field] = json!(true);
        f.r.execute("UPDATE desk_home_receipt SET event=?1", [event.to_string()])
            .unwrap();
        assert!(unresolved(&f.r, &f.m, ALPHA).unwrap());
        event[field] = json!(false);
    }
    event["id"] = json!("replacement");
    f.r.execute("UPDATE desk_home_receipt SET event=?1", [event.to_string()])
        .unwrap();
    assert!(
        unresolved(&f.r, &f.m, ALPHA).unwrap(),
        "equal counts cannot replace frozen IDs"
    );
}
#[test]
fn old_loss_can_be_cut_but_later_loss_epoch_withdraws_local_proof() {
    let f = Fixture::new();
    gap(&f.r, &f.m, ALPHA, "history_gap").unwrap();
    f.r.execute(
        "INSERT INTO desk_intake_loss VALUES('survey',?1,'history_gap') ON CONFLICT(domain,source) DO UPDATE SET state='history_gap'",
        [ALPHA],
    )
    .unwrap();
    f.apply();
    let e = f.evidence();
    assert!(proof::eligible(&f.r, &f.m, ALPHA, &e).unwrap());
    f.r.execute("UPDATE desk_intake_loss SET state=state", [])
        .unwrap();
    assert!(!proof::eligible(&f.r, &f.m, ALPHA, &e).unwrap());
    assert!(
        !reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
}
#[test]
fn repeated_decisions_are_audited_and_stale_binding_can_still_be_revoked() {
    let mut f = Fixture::new();
    let args = f.args();
    let reason = json!({"reason":"Withdraw repeated boundary"});
    f.apply();
    revoke(&f.r, &Caller::Operator, &f.m, ALPHA, &reason).unwrap();
    apply(&f.r, &Caller::Operator, &f.m, ALPHA, &args).unwrap();
    f.m.incarnation = uuid::Uuid::new_v4().to_string();
    assert!(load(&f.r, &f.m, ALPHA).is_err());
    revoke(&f.r, &Caller::Operator, &f.m, ALPHA, &reason).unwrap();
    assert_eq!(f.r.query_row("SELECT COUNT(*) FROM desk_intake_audit WHERE stage IN ('legacy.apply','legacy.revoke')",[],|r|r.get::<_,i64>(0)).unwrap(),4);
}

#[test]
fn bounded_set_and_membership_are_preconditions_not_best_effort() {
    let f = Fixture::new();
    f.r.execute("UPDATE ships SET name='unexpected'", [])
        .unwrap();
    assert!(plan(&f.r, &f.m, ALPHA).is_err());
    f.r.execute("UPDATE ships SET name='survey'", []).unwrap();
    f.r.execute(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<2001)
        INSERT INTO desk_home_receipt(domain,source,intent,binding,event,receipt)
        SELECT domain,source,'extra-'||x,binding,json_set(event,'$.id','extra-'||x),receipt
        FROM (SELECT * FROM desk_home_receipt LIMIT 1),n",
        [],
    )
    .unwrap();
    assert!(
        plan(&f.r, &f.m, ALPHA)
            .unwrap_err()
            .to_string()
            .contains("bound")
    );
}

#[test]
fn pruned_legacy_pin_never_relabels_replacement_mail() {
    let f = Fixture::new();
    f.apply();
    f.h.connection()
        .execute("DELETE FROM mail WHERE id='legacy'", [])
        .unwrap();
    assert!(
        reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
    crate::mail::deliver_with_provenance(&f.h,&json!({"id":"legacy","from":"beta","to":"captain","kind":"notice","body":"Re-pinned replacement"}), &source::Provenance::Unresolved).unwrap();
    f.h.connection()
        .execute("UPDATE mail SET state='acknowledged' WHERE id='legacy'", [])
        .unwrap();
    assert!(
        !reconcile::inspect(&f.r, &f.m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
    assert_eq!(
        f.r.query_row("SELECT COUNT(*) FROM desk_intake_fact", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
