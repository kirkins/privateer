use super::*;
use crate::jobs::deployment_admission::{admit_for_test, tests::fixture};

#[test]
fn host_custody_cas_and_append_only_record_commit_together_and_survive_reopen() {
    for fault in ["stale", "insert", "none"] {
        let (_root, store) = fixture();
        let handoff = admit_for_test(&store).unwrap().handoff().unwrap();
        init_host(&handoff.conn).unwrap();
        let key = handoff.reservation.key.clone();
        let mut launched = handoff.reservation.clone();
        launched.state = "deployment-started".into();
        launched.payload = Some(crate::jobs::identity(std::process::id()).unwrap().unwrap());
        if fault == "stale" {
            handoff
                .conn
                .execute("UPDATE reservations SET record='{}' WHERE key=?1", [&key])
                .unwrap();
        } else if fault == "insert" {
            handoff
                .conn
                .execute_batch(
                    "CREATE TRIGGER fail_payload BEFORE INSERT ON deployment_payloads
                BEGIN SELECT RAISE(ABORT,'fixture'); END;",
                )
                .unwrap();
        }
        let custody = json!({"claim":"alpha","hold":key,"payload":launched.payload,
            "containment_scope":"attempt_cgroup_only","limitation":"outside_ipc_delegation_not_excluded"});
        let tx = rusqlite::Transaction::new_unchecked(
            &handoff.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        let result = record_host(&tx, &handoff.record, &launched, &custody);
        if fault == "none" {
            result.unwrap();
            tx.commit().unwrap();
        } else {
            assert!(result.is_err());
            tx.rollback().unwrap();
        }
        let reopened = crate::jobs::shared_ledger(&store).unwrap();
        let raw: String = reopened
            .query_row(
                "SELECT record FROM reservations WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        if fault == "none" {
            assert_eq!(raw, serde_json::to_string(&launched).unwrap());
            let record: String = reopened
                .query_row(
                    "SELECT record FROM deployment_payloads WHERE key=?1",
                    [&key],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(record, custody.to_string());
            assert!(
                reopened
                    .execute(
                        "UPDATE deployment_payloads SET record='{}' WHERE key=?1",
                        [&key]
                    )
                    .is_err()
            );
            assert!(
                reopened
                    .execute("DELETE FROM deployment_payloads WHERE key=?1", [&key])
                    .is_err()
            );
        } else {
            assert_eq!(
                raw,
                if fault == "stale" {
                    "{}"
                } else {
                    &handoff.record
                }
            );
            assert!(
                !reopened
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM deployment_payloads WHERE key=?1)",
                        [&key],
                        |r| r.get::<_, bool>(0)
                    )
                    .unwrap()
            );
        }
        // This proves ledger custody/rollback, NOT fake cgroup containment.
    }
}
