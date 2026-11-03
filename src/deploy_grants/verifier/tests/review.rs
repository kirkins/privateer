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
fn all_input_hashes_never_hold_the_home_writer_lock_during_consumption_or_claim() {
    let fixture = Fixture::new(json!({}));
    let database = fixture.store.home().join("privateer.sqlite");
    let reads = Rc::new(Cell::new(0));
    let observed = Rc::clone(&reads);
    config::HASH_OBSERVER.with(|observer| {
        *observer.borrow_mut() = Some(Box::new(move |_file| {
            // A second writer must acquire immediately AT the byte-hash
            // boundary. No sleep, wall clock, load assumption or long timeout.
            let writer = rusqlite::Connection::open(&database).unwrap();
            writer.busy_timeout(Duration::ZERO).unwrap();
            writer
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
                .expect("artifact AND tool/config hashing must not hold the writer lock");
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
fn tool_rewrite_with_same_size_inode_mode_and_restored_mtime_checks_ctime() {
    let fixture = Fixture::new(json!({}));
    let proof = fixture.proof();
    let path = fixture.root.join("adapter.py");
    let before = fs::metadata(&path).unwrap();
    let mut content = fs::read(&path).unwrap();
    content[0] = b'#';
    fs::write(&path, content).unwrap();
    File::open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.len(), after.len());
    assert_eq!(before.mode(), after.mode());
    assert_eq!(
        (before.mtime(), before.mtime_nsec()),
        (after.mtime(), after.mtime_nsec())
    );
    // No sleep/retry to outrun a coarse filesystem timestamp. If that filesystem
    // did distinguish the rewrite, ctime is the ONLY stamp field detecting it.
    // Same-tick coarse-ctime rewrites are an explicitly documented seal limit;
    // execution/upload still hashes the exact private copy/stream it uses.
    if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
        let tx = fixture.store.write_transaction().unwrap();
        assert!(fixture.recheck(&fixture.store, &proof).is_err());
        tx.rollback().unwrap();
    }
}

#[test]
fn started_transition_requires_both_the_current_state_and_exact_claim_identity() {
    for state in [
        "pending",
        "started",
        "denied",
        "completed",
        "unknown",
        "claimed",
    ] {
        let fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let id = first["id"].as_str().unwrap();
        let token = fixture.claim(id).unwrap().claim.unwrap();
        fixture
            .store
            .connection()
            .execute(
                "UPDATE deployment_intents SET state=?2 WHERE use_id=?1",
                rusqlite::params![token.id(), state],
            )
            .unwrap();
        if state == "claimed" {
            fixture
                .store
                .connection()
                .execute(
                    "UPDATE deployment_intents SET claim_identity='beta' WHERE use_id=?1",
                    [id],
                )
                .unwrap();
        }
        assert!(
            claims::mark_started(&fixture.store, token).is_err(),
            "{state}"
        );
    }
    let fixture = Fixture::new(json!({}));
    let first = fixture.consume("alpha").unwrap();
    let id = first["id"].as_str().unwrap();
    let token = fixture.claim(id).unwrap().claim.unwrap();
    claims::mark_started(&fixture.store, token).unwrap();
    assert!(
        fixture.claim(id).unwrap().claim.is_none(),
        "no repeat start token"
    );
    let state: String = fixture
        .store
        .connection()
        .query_row(
            "SELECT state FROM deployment_intents WHERE use_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "started");
}

#[test]
fn started_transition_is_operator_only_and_rolls_back_with_its_audit() {
    for agent in [true, false] {
        let mut fixture = Fixture::new(json!({}));
        let first = fixture.consume("alpha").unwrap();
        let id = first["id"].as_str().unwrap();
        let token = fixture.claim(id).unwrap().claim.unwrap();
        if agent {
            fixture.store.set_caller(Caller::Agent {
                actor: "alpha".into(),
            });
        } else {
            fixture
                .store
                .connection()
                .execute_batch(
                    "CREATE TRIGGER fail_started_audit BEFORE INSERT ON audit
                 WHEN NEW.kind='deployment.started' BEGIN SELECT RAISE(ABORT,'fixture'); END",
                )
                .unwrap();
        }
        assert!(claims::mark_started(&fixture.store, token).is_err());
        let state: String = fixture
            .store
            .connection()
            .query_row(
                "SELECT state FROM deployment_intents WHERE use_id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "claimed");
    }
}
