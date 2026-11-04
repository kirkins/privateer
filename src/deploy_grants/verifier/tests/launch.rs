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
    if (argc != 2 || check(argv[1], "alpha") ||
        check(getenv("PRIVATEER_ADAPTER_CONFIG"), "beta")) return 42;
    for (int fd = 3; fd < 1024; fd++) {
        if (fd == 7 || fd == 8) continue;
        if (fcntl(fd, F_GETFD) != -1 || errno != EBADF) return 43;
    }
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
    configuration["adapter"] =
        json!({"id":"beta","executable":executable,"args":[file],"files":[file],"config":config});
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
        let result = claim.record_started(&fixture.store, &custody);
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

    let Some(delegation) = std::env::var_os("PRIVATEER_TEST_CGROUP_DELEGATION") else {
        assert!(containment::qualify(root.path()).is_err());
        eprintln!(
            "unqualified environment: real consuming spawn refused; no positive containment claim"
        );
        return;
    };
    for fault in ["none", "host-insert", "home-commit"] {
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
        let result = handoff.spawn(&fixture.store, claim, &mut boundary);
        if fault == "none" {
            let status = result.unwrap();
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        } else {
            assert!(result.is_err(), "{fault}");
        }
        assert_eq!(
            state(&fixture, &id),
            if fault == "none" {
                "started"
            } else {
                "claimed"
            }
        );
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
            if fault == "host-insert" {
                "deployment-handoff"
            } else {
                "deployment-started"
            }
        );
        if fault == "none" {
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
