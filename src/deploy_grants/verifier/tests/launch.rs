//! Production claim/start rechecks run in a re-exec with wholly private XDG
//! paths. Never overwrite the user's captain installation or emergency pause.
use super::*;
use crate::{
    deploy_grants::claims,
    jobs::deployment_admission::{admit_for_test, containment},
};
use rusqlite::OptionalExtension;
use std::os::{fd::AsRawFd, unix::fs::FileExt};

const NATIVE: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static int check(const char *path, const char *expected) {
    char bytes[16] = {0};
    int fd = open(path, O_RDONLY);
    if (fd < 0 || read(fd, bytes, 15) != (ssize_t)strlen(expected)) return 1;
    close(fd);
    return strcmp(bytes, expected);
}
int main(int argc, char **argv) {
    if (argc != 3 || check(argv[1], "alpha") ||
        check(getenv("PRIVATEER_ADAPTER_CONFIG"), "beta")) return 42;
    for (int fd = 3; fd < 1024; fd++) {
        if (fd == 7 || fd == 8) continue;
        if (fcntl(fd, F_GETFD) != -1 || errno != EBADF) return 43;
    }
    /* A fixed hex argument keeps this fixture's output path out of the
       supporting-input argv namespace. No production policy is relaxed. */
    char path[4096];
    size_t length = strlen(argv[2]);
    if (length % 2 || length / 2 >= sizeof(path)) return 44;
    for (size_t i = 0; i < length / 2; i++) {
        unsigned int value;
        if (sscanf(argv[2] + 2*i, "%2x", &value) != 1) return 44;
        path[i] = (char)value;
    }
    path[length / 2] = 0;
    int marker = open(path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (marker < 0 || write(marker, "executed", 8) != 8) return 44;
    close(marker);
    unsigned char prefix[4];
    if (read(0, prefix, 4) != 4) return 45;
    size_t length_in = ((size_t)prefix[0] << 24) | ((size_t)prefix[1] << 16) |
        ((size_t)prefix[2] << 8) | prefix[3];
    if (length_in == 0 || length_in > 16384) return 45;
    char metadata[16385];
    size_t offset = 0;
    while (offset < length_in) {
        ssize_t n = read(0, metadata + offset, length_in - offset);
        if (n <= 0) return 45;
        offset += (size_t)n;
    }
    metadata[length_in] = 0;
    if (!strstr(metadata, "\"schema_version\":1") || strstr(metadata, "path") ||
        strstr(metadata, "PRIVATEER_ADAPTER_CONFIG")) return 45;
    char artifact[32];
    offset = 0;
    for (;;) {
        ssize_t n = read(0, artifact + offset, sizeof(artifact) - offset);
        if (n < 0 || offset == sizeof(artifact)) return 45;
        if (n == 0) break;
        offset += (size_t)n;
    }
    if (offset != 14 || memcmp(artifact, "alpha artifact", 14)) return 45;
    const char reply[] = "\0\0\0\47{\"schema_version\":1,\"status\":\"success\"}";
    if (write(1, reply, sizeof(reply) - 1) != (ssize_t)(sizeof(reply) - 1)) return 46;
    return 0;
}
"#;

fn prepared_fixture(binary: &Path) -> Fixture {
    let fixture = Fixture::new(json!({}));
    private(
        &fixture.store.home().join("config.json"),
        json!({"resource_home":fixture.root.join("resources")}).to_string(),
    );
    let executable = fixture.root.join("native");
    fs::copy(binary, &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let file = fixture.root.join("support");
    let config = fixture.root.join("adapter-config");
    private(&file, b"alpha");
    private(&config, b"beta");
    let path = fixture.store.home().join("deployment/beta.json");
    let mut configuration: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // The marker is output, not a supporting input. Encode its path as an
    // opaque fixed fixture argument; absolute file argv is correctly refused.
    let marker = fixture.root.join("executed");
    let marker_hex: String = marker
        .to_str()
        .unwrap()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    configuration["adapter"] = json!({"id":"beta","executable":executable,"args":[file,marker_hex],"files":[file],"config":config});
    private(&path, configuration.to_string());
    let spec: String = fixture
        .store
        .connection()
        .query_row(
            "SELECT spec FROM standing_deploy_grants WHERE id='alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut grant: Value = serde_json::from_str(&spec).unwrap();
    let bindings = fixture.snapshot().bindings();
    grant["expected_revision"] = json!(1);
    grant["verifier"] = bindings["verifier"].clone();
    grant["adapter"] = bindings["adapter"].clone();
    crate::deploy_grants::put(&fixture.store, grant, |_| Ok(())).unwrap();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("privateer");
    directory(&config).unwrap();
    private(
        &config.join("deployment-installation.json"),
        fs::read(&fixture.installation).unwrap(),
    );
    private(
        &config.join("deployment-pause.json"),
        b"{\"installation_id\":\"alpha\",\"paused\":false}",
    );
    fixture
}

fn fresh(fixture: &Fixture) -> (String, claims::Claim) {
    let row = fixture.consume("alpha").unwrap();
    let id = row["id"].as_str().unwrap().to_owned();
    let claim = claims::claim(&fixture.store, &id).unwrap().claim.unwrap();
    assert!(claims::claim(&fixture.store, &id).unwrap().claim.is_none());
    (id, claim)
}

fn state(fixture: &Fixture, id: &str) -> String {
    fixture
        .store
        .connection()
        .query_row(
            "SELECT state FROM deployment_intents WHERE use_id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn consuming_spawn_bindings_and_fresh_claim_are_isolated() {
    if std::env::var_os("PRIVATEER_TEST_LAUNCH_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::consuming_spawn_bindings_and_fresh_claim_are_isolated",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .env_clear()
            .args(["--exact", &name, "--nocapture"])
            .env("PRIVATEER_TEST_LAUNCH_CHILD", "1")
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", root.path())
            .env("TMPDIR", root.path())
            .env("PRIVATEER_FLEET", root.path().join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", root.path().join("resources"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("RUST_TEST_THREADS", "1");
        if let Some(delegation) = std::env::var_os("PRIVATEER_TEST_CGROUP_DELEGATION") {
            child.env("PRIVATEER_TEST_CGROUP_DELEGATION", delegation);
        }
        let output = child.output().unwrap();
        assert!(output.status.success(), "isolated launch tests: {output:?}");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        return;
    }
    let _clock = FrozenWallClock::at(1_000_000);
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("alpha.c");
    let binary = root.path().join("alpha");
    private(&source, NATIVE);
    assert!(
        std::process::Command::new("cc")
            .args(["-static", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&binary)
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );

    let fixture = prepared_fixture(&binary);
    let (_id, claim) = fresh(&fixture);
    let adapter = claim.adapter().unwrap();
    assert_eq!(adapter.args()[1].to_str().unwrap(), "/proc/self/fd/7");
    assert_eq!(
        adapter.env()[1].to_str().unwrap(),
        "PRIVATEER_ADAPTER_CONFIG=/proc/self/fd/8"
    );
    for (file, expected) in adapter
        .files()
        .zip([b"alpha".as_slice(), b"beta".as_slice()])
    {
        assert_eq!(
            file.write_at(b"gamma", 0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        let mut bytes = vec![0; expected.len()];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, expected);
        assert!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) } & libc::F_SEAL_WRITE != 0
        );
    }
    private(&fixture.root.join("support"), b"gamma");
    private(&fixture.root.join("adapter-config"), b"gamma");
    assert!(
        claim.adapter().is_err(),
        "changed source bytes must not enter another adapter token"
    );
    let mut bytes = [0; 5];
    adapter
        .files()
        .next()
        .unwrap()
        .read_exact_at(&mut bytes, 0)
        .unwrap();
    assert_eq!(&bytes, b"alpha");

    for fault in [
        "none",
        "pause",
        "unreadable-pause",
        "malformed-pause",
        "wrong-installation",
        "charter",
        "task",
        "grant-pause",
        "custody",
        "config",
        "stale",
        "audit",
    ] {
        let fixture = prepared_fixture(&binary);
        let (id, claim) = fresh(&fixture);
        let mut custody = json!({"claim":claim.binding(), "containment_scope":"attempt_cgroup_only",
            "limitation":"outside_ipc_delegation_not_excluded"});
        let pause = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap())
            .join("privateer/deployment-pause.json");
        match fault {
            "pause" => private(&pause, b"{\"installation_id\":\"alpha\",\"paused\":true}"),
            "unreadable-pause" => { fs::remove_file(pause).unwrap(); },
            "malformed-pause" => private(&pause, b"{"),
            "wrong-installation" => private(&pause, b"{\"installation_id\":\"gamma\",\"paused\":false}"),
            "charter" => { fixture.store.connection().execute_batch("UPDATE charters SET data=json_set(data,'$.actions',json('[]'))").unwrap(); },
            "task" => { fixture.store.connection().execute_batch("UPDATE tasks SET status='done'").unwrap(); },
            "grant-pause" => fixture.store.connection().execute_batch("UPDATE standing_deploy_grants SET paused_ms=1,revision=revision+1 WHERE id='alpha'").unwrap(),
            "custody" => custody["claim"]["deployment_id"] = json!("gamma"),
            "config" => private(&fixture.root.join("adapter-config"), b"gamma"),
            "stale" => { fixture.store.connection().execute("UPDATE deployment_intents SET claim_identity='gamma' WHERE use_id=?1", [&id]).unwrap(); },
            "audit" => fixture.store.connection().execute_batch("CREATE TRIGGER fail_started BEFORE INSERT ON audit WHEN NEW.kind='deployment.started' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap(),
            _ => {}
        }
        let home = fixture.store.write_transaction().unwrap();
        assert_eq!(
            claim.check_pause().is_ok(),
            !matches!(
                fault,
                "pause" | "unreadable-pause" | "malformed-pause" | "wrong-installation"
            )
        );
        let result = containment::record_started_for_test(&claim, &fixture.store, &custody);
        if fault == "none" {
            result.unwrap();
            home.commit().unwrap();
        } else {
            assert!(result.is_err(), "{fault}");
            home.rollback().unwrap();
        }
        assert_eq!(
            state(&fixture, &id),
            if fault == "none" {
                "started"
            } else {
                "claimed"
            }
        );
        let payload: Option<String> = fixture
            .store
            .connection()
            .query_row(
                "SELECT record FROM deployment_payloads WHERE use_id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(payload.is_some(), fault == "none");
        if let Some(payload) = payload {
            assert_eq!(payload, custody.to_string());
            let reopened = Store::open(fixture.store.home()).unwrap();
            assert!(
                reopened
                    .connection()
                    .execute("DELETE FROM deployment_payloads WHERE use_id=?1", [&id])
                    .is_err()
            );
            assert!(
                reopened
                    .connection()
                    .execute(
                        "UPDATE deployment_payloads SET record='{}' WHERE use_id=?1",
                        [&id]
                    )
                    .is_err()
            );
        }
    }

    for fault in ["none", "host-changed", "host-missing", "charter"] {
        let fixture = prepared_fixture(&binary);
        let (_id, claim) = fresh(&fixture);
        let admitted = admit_for_test(&fixture.store).unwrap();
        let key = admitted.key().to_owned();
        let handoff = admitted.handoff().unwrap();
        let host = crate::jobs::shared_ledger(&fixture.store).unwrap();
        match fault {
            "host-changed" => {
                host.execute("UPDATE reservations SET record='{}' WHERE key=?1", [&key])
                    .unwrap();
            }
            "host-missing" => {
                host.execute("DELETE FROM reservations WHERE key=?1", [&key])
                    .unwrap();
            }
            "charter" => {
                fixture
                    .store
                    .connection()
                    .execute_batch("UPDATE charters SET data=json_set(data,'$.job_timeout_secs',1)")
                    .unwrap();
            }
            _ => {}
        }
        assert_eq!(
            handoff.validate_for_test(&fixture.store, &claim).is_ok(),
            fault == "none",
            "{fault}"
        );
    }

    abort_ledger_tests(&binary);
    final_release_tests(&binary);

    let Some(delegation) = std::env::var_os("PRIVATEER_TEST_CGROUP_DELEGATION") else {
        assert!(containment::qualify(root.path()).is_err());
        eprintln!(
            "unqualified environment: real consuming spawn refused; no positive containment claim"
        );
        return;
    };
    for fault in [
        "none",
        "host-insert",
        "home-commit",
        "locked-pause",
        "late-pause",
        "late-unreadable",
        "late-malformed",
        "late-grant-pause",
        "late-grant-revoke",
        "late-audit",
    ] {
        let fixture = prepared_fixture(&binary);
        let (id, claim) = fresh(&fixture);
        let admitted = admit_for_test(&fixture.store).unwrap();
        let key = admitted.key().to_owned();
        let handoff = admitted.handoff().unwrap();
        let host = crate::jobs::shared_ledger(&fixture.store).unwrap();
        if fault == "host-insert" {
            // The table is created by spawn. Create it first solely to install
            // the fault trigger; the real CAS/insert/commit still run in spawn.
            host.execute_batch("CREATE TABLE IF NOT EXISTS deployment_payloads(key TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TRIGGER fail_payload BEFORE INSERT ON deployment_payloads BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        } else if fault == "home-commit" {
            fixture.store.connection().execute_batch("CREATE TABLE deferred_fault(id TEXT REFERENCES deployment_uses(id) DEFERRABLE INITIALLY DEFERRED);
                CREATE TRIGGER fail_commit AFTER INSERT ON deployment_payloads BEGIN INSERT INTO deferred_fault VALUES('gamma'); END;").unwrap();
        }
        let mut boundary = containment::qualify(Path::new(&delegation))
            .expect("explicit isolated delegation must qualify");
        let pause = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap())
            .join("privateer/deployment-pause.json");
        let lifecycle = Store::open(fixture.store.home()).unwrap();
        containment::RELEASE_OBSERVER.with(|observer| {
            *observer.borrow_mut() = Some(Box::new(move |stage| {
                if (stage == "host_locked" && fault == "locked-pause")
                    || (stage == "before_release" && fault == "late-pause")
                {
                    private(&pause, b"{\"installation_id\":\"alpha\",\"paused\":true}");
                } else if stage == "before_release" && fault == "late-unreadable" {
                    fs::remove_file(&pause).unwrap();
                } else if stage == "before_release" && fault == "late-malformed" {
                    private(&pause, b"{");
                } else if stage == "before_release" && fault == "late-grant-pause" {
                    lifecycle.connection().execute_batch("UPDATE standing_deploy_grants SET paused_ms=1,revision=revision+1 WHERE id='alpha'").unwrap();
                } else if stage == "before_release" && fault == "late-grant-revoke" {
                    lifecycle.connection().execute_batch("UPDATE standing_deploy_grants SET revoked_ms=1,revision=revision+1 WHERE id='alpha'").unwrap();
                } else if stage == "before_release" && fault == "late-audit" {
                    private(&pause, b"{\"installation_id\":\"alpha\",\"paused\":true}");
                    lifecycle.connection().execute_batch("CREATE TRIGGER fail_abort BEFORE INSERT ON audit WHEN NEW.kind='deployment.release_refused' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
                }
            }));
        });
        let result = handoff.spawn(&fixture.store, claim, &mut boundary);
        containment::RELEASE_OBSERVER.with(|observer| *observer.borrow_mut() = None);
        assert_eq!(
            fixture.root.join("executed").exists(),
            fault == "none",
            "{fault}: unreleased adapter executed"
        );
        let late = fault.starts_with("late-");
        if fault == "none" {
            let report = result.unwrap();
            assert!(report.protocol_success, "{report:?}");
            assert!(report.cleanup_proven_empty);
            assert_eq!(report.artifact_bytes, 14);
            assert_eq!(
                report.artifact_sha256,
                format!("{:x}", Sha256::digest(b"alpha artifact"))
            );
            let status = report.wait_status.unwrap();
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        } else {
            assert!(result.is_err(), "{fault}");
            if fault == "late-audit" {
                assert!(
                    format!("{:#}", result.unwrap_err()).contains("deployment release refused")
                );
            }
        }
        assert_eq!(
            state(&fixture, &id),
            if fault == "none" || fault == "late-audit" {
                "started"
            } else if late {
                "denied"
            } else {
                "claimed"
            }
        );
        if late && fault != "late-audit" {
            let outcome: String = fixture
                .store
                .connection()
                .query_row(
                    "SELECT outcome FROM deployment_intents WHERE use_id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(outcome, "not_started");
            assert_release_audit(&fixture, &id, true);
        }
        let record: String = host
            .query_row(
                "SELECT record FROM reservations WHERE key=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(
            record["state"],
            if matches!(fault, "host-insert" | "locked-pause") {
                "deployment-handoff"
            } else {
                "deployment-started"
            }
        );
        if fault == "none" || late {
            let host_record: String = host
                .query_row(
                    "SELECT record FROM deployment_payloads WHERE key=?1",
                    [&key],
                    |r| r.get(0),
                )
                .unwrap();
            let home_record: String = fixture
                .store
                .connection()
                .query_row(
                    "SELECT record FROM deployment_payloads WHERE use_id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(host_record, home_record);
        }
        boundary.dispose_test_boundary();
    }
}

// Ordinary committed lifecycle reads, not positive containment evidence.
fn final_release_tests(binary: &Path) {
    for fault in [
        "none",
        "pause",
        "revoke",
        "revision",
        "missing",
        "unreadable",
    ] {
        let fixture = prepared_fixture(binary);
        let (_id, claim) = fresh(&fixture);
        let (held, metadata) = claim.protocol_input().unwrap();
        assert_eq!(held.metadata().unwrap().len(), 14);
        assert!(metadata["artifact"].get("path").is_none());
        assert!(metadata.get("bindings").is_none());
        let other = Store::open(fixture.store.home()).unwrap();
        match fault {
            "pause" => other
                .connection()
                .execute_batch("UPDATE standing_deploy_grants SET paused_ms=1 WHERE id='alpha'")
                .unwrap(),
            "revoke" => other
                .connection()
                .execute_batch("UPDATE standing_deploy_grants SET revoked_ms=1 WHERE id='alpha'")
                .unwrap(),
            "revision" => other
                .connection()
                .execute_batch(
                    "UPDATE standing_deploy_grants SET revision=revision+1 WHERE id='alpha'",
                )
                .unwrap(),
            "unreadable" => other
                .connection()
                .execute_batch("ALTER TABLE standing_deploy_grants RENAME TO unavailable_grants")
                .unwrap(),
            _ => {}
        }
        // Test a missing lookup via an empty standalone Store, without disabling
        // the real ledger's foreign-key constraints.
        if fault == "missing" {
            let empty = tempfile::tempdir().unwrap();
            let store = Store::open(empty.path()).unwrap();
            assert!(claim.check_grant_lifecycle(&store).is_err());
        } else {
            assert_eq!(
                claim.check_grant_lifecycle(&fixture.store).is_ok(),
                fault == "none",
                "{fault}"
            );
        }
    }
}

fn assert_release_audit(fixture: &Fixture, id: &str, empty: bool) {
    let payload: String = fixture
        .store
        .connection()
        .query_row(
            "SELECT payload FROM audit WHERE kind='deployment.release_refused'
         AND json_extract(payload,'$.deployment_id')=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    let detail = serde_json::from_str::<Value>(&payload).unwrap()["detail"].clone();
    assert_eq!(detail["release_attempted"], false);
    assert_eq!(detail["cleanup_proven_empty"], empty);
    assert_eq!(detail["capacity_retained"], true);
    assert_eq!(
        detail["outcome"],
        if empty { "not_started" } else { "unknown" }
    );
    assert_eq!(detail["containment_scope"], "attempt_cgroup_only");
    assert_eq!(detail["limitation"], "outside_ipc_delegation_not_excluded");
}

// Home-ledger transaction tests only. `empty` is an injected cleanup result,
// NOT evidence of kernel containment; the opt-in spawn tests prove that path.
fn abort_ledger_tests(binary: &Path) {
    for fault in ["empty", "unknown", "audit", "stale", "custody"] {
        let fixture = prepared_fixture(binary);
        let (id, claim) = fresh(&fixture);
        let custody = json!({"claim":claim.binding()});
        let tx = fixture.store.write_transaction().unwrap();
        containment::record_started_for_test(&claim, &fixture.store, &custody).unwrap();
        tx.commit().unwrap();
        let mut attempted = custody.clone();
        match fault {
            "audit" => fixture
                .store
                .connection()
                .execute_batch(
                    "CREATE TRIGGER fail_abort BEFORE INSERT ON audit
                 WHEN NEW.kind='deployment.release_refused'
                 BEGIN SELECT RAISE(ABORT,'fixture'); END;",
                )
                .unwrap(),
            "stale" => {
                fixture
                    .store
                    .connection()
                    .execute(
                        "UPDATE deployment_intents SET claim_identity='gamma' WHERE use_id=?1",
                        [&id],
                    )
                    .unwrap();
            }
            "custody" => attempted["claim"]["deployment_id"] = json!("gamma"),
            _ => {}
        }
        let empty = fault != "unknown";
        let result =
            containment::record_not_started_for_test(&claim, &fixture.store, &attempted, empty);
        let succeeded = matches!(fault, "empty" | "unknown");
        assert_eq!(result.is_ok(), succeeded, "{fault}");
        assert_eq!(
            state(&fixture, &id),
            if !succeeded {
                "started"
            } else if empty {
                "denied"
            } else {
                "unknown"
            }
        );
        let reopened = Store::open(fixture.store.home()).unwrap();
        let saved: String = reopened
            .connection()
            .query_row(
                "SELECT record FROM deployment_payloads WHERE use_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(saved, custody.to_string());
        assert!(
            reopened
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM deployment_uses WHERE id=?1)",
                    [&id],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        if succeeded {
            assert_release_audit(&fixture, &id, empty);
            // No stale witness/custody can repeat the abort transition.
            assert!(
                containment::record_not_started_for_test(&claim, &fixture.store, &custody, empty)
                    .is_err()
            );
            assert!(claims::claim(&fixture.store, &id).unwrap().claim.is_none());
        } else {
            let outcome: Option<String> = reopened
                .connection()
                .query_row(
                    "SELECT outcome FROM deployment_intents WHERE use_id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                outcome.is_none(),
                "{fault}: failed transaction changed outcome"
            );
        }
    }
}
