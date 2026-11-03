use super::*;
use crate::deploy_grants::claims;
use std::{cell::Cell, rc::Rc};

struct ObserveHashes;
impl Drop for ObserveHashes {
    fn drop(&mut self) {
        config::HASH_OBSERVER.with(|observer| observer.take());
    }
}

#[test]
fn artifact_hashes_never_hold_the_home_writer_lock_during_consumption_or_claim() {
    let fixture = Fixture::new(json!({}));
    let database = fixture.store.home().join("privateer.sqlite");
    let reads = Rc::new(Cell::new(0));
    let observed = Rc::clone(&reads);
    config::HASH_OBSERVER.with(|observer| {
        *observer.borrow_mut() = Some(Box::new(move |file| {
            if file.metadata().unwrap().len() != b"alpha artifact".len() as u64 {
                return;
            }
            // A second writer must acquire immediately AT the byte-hash
            // boundary. No sleep, wall clock, load assumption or long timeout.
            let writer = rusqlite::Connection::open(&database).unwrap();
            writer.busy_timeout(Duration::ZERO).unwrap();
            writer
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .expect("retained artifact hashing must not hold the writer lock");
            observed.set(observed.get() + 1);
        }));
    });
    let _observer = ObserveHashes;
    let first = fixture.consume("alpha").unwrap();
    let after_consumption = reads.get();
    assert!(
        after_consumption > 0,
        "must observe a real retained-byte hash"
    );
    let outcome = fixture.claim(first["id"].as_str().unwrap()).unwrap();
    assert!(outcome.claim.is_some());
    assert!(
        reads.get() > after_consumption,
        "claim must freshly hash bytes"
    );
}

#[test]
fn verification_refuses_to_run_inside_a_home_transaction() {
    let fixture = Fixture::new(json!({}));
    let tx = fixture.store.write_transaction().unwrap();
    assert!(
        fixture
            .verify()
            .unwrap_err()
            .to_string()
            .contains("outside a transaction")
    );
    tx.rollback().unwrap();
}

#[test]
fn transaction_recheck_binds_the_hash_to_the_open_inode_and_metadata() {
    for change in ["replace", "mtime", "mode", "proof-digest", "proof-size"] {
        let fixture = Fixture::new(json!({}));
        let mut proof = fixture.proof();
        let path = PathBuf::from(proof.value()["artifact"]["path"].as_str().unwrap());
        match change {
            "replace" => {
                // Even identical bytes at a replacement inode are not this seal.
                fs::rename(&path, fixture.root.join("gamma")).unwrap();
                private(&path, b"alpha artifact");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
            }
            "mtime" => {
                let file = config::regular(&path, true).unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
                    .unwrap();
            }
            "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap(),
            "proof-digest" => proof.value["artifact"]["sha256"] = json!("b".repeat(64)),
            "proof-size" => proof.value["artifact"]["size_bytes"] = json!(1),
            _ => unreachable!(),
        }
        let tx = fixture.store.write_transaction().unwrap();
        assert!(fixture.recheck(&fixture.store, &proof).is_err(), "{change}");
        tx.rollback().unwrap();
    }
}

#[test]
fn started_transition_requires_both_the_current_state_and_exact_claim_identity() {
    let fixture = Fixture::new(json!({}));
    let first = fixture.consume("alpha").unwrap();
    let token = fixture
        .claim(first["id"].as_str().unwrap())
        .unwrap()
        .claim
        .unwrap();
    for state in ["pending", "started", "denied", "completed", "unknown"] {
        fixture
            .store
            .connection()
            .execute(
                "UPDATE deployment_intents SET state=?2 WHERE use_id=?1",
                rusqlite::params![token.id(), state],
            )
            .unwrap();
        assert!(
            claims::mark_started(&fixture.store, &token).is_err(),
            "{state}"
        );
    }
    fixture
        .store
        .connection()
        .execute(
            "UPDATE deployment_intents SET state='claimed',claim_identity='beta' WHERE use_id=?1",
            [token.id()],
        )
        .unwrap();
    assert!(claims::mark_started(&fixture.store, &token).is_err());
    fixture
        .store
        .connection()
        .execute(
            "UPDATE deployment_intents SET claim_identity=?2 WHERE use_id=?1",
            rusqlite::params![token.id(), token.identity()],
        )
        .unwrap();
    claims::mark_started(&fixture.store, &token).unwrap();
    assert!(
        claims::mark_started(&fixture.store, &token).is_err(),
        "no repeat start"
    );
    let state: String = fixture
        .store
        .connection()
        .query_row(
            "SELECT state FROM deployment_intents WHERE use_id=?1",
            [token.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "started");
}

#[test]
fn started_transition_is_operator_only_and_rolls_back_with_its_audit() {
    let mut fixture = Fixture::new(json!({}));
    let first = fixture.consume("alpha").unwrap();
    let token = fixture
        .claim(first["id"].as_str().unwrap())
        .unwrap()
        .claim
        .unwrap();
    fixture.store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    assert!(claims::mark_started(&fixture.store, &token).is_err());
    fixture.store.set_caller(Caller::Operator);
    fixture
        .store
        .connection()
        .execute_batch(
            "CREATE TRIGGER fail_started_audit BEFORE INSERT ON audit
         WHEN NEW.kind='deployment.started' BEGIN SELECT RAISE(ABORT,'fixture'); END",
        )
        .unwrap();
    assert!(claims::mark_started(&fixture.store, &token).is_err());
    let state: String = fixture
        .store
        .connection()
        .query_row(
            "SELECT state FROM deployment_intents WHERE use_id=?1",
            [token.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "claimed");
    fixture
        .store
        .connection()
        .execute_batch("DROP TRIGGER fail_started_audit")
        .unwrap();
    claims::mark_started(&fixture.store, &token).unwrap();
}
