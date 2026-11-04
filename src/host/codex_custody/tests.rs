use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    child: Child,
    daemon: Child,
    _socket: UnixListener,
    custody: Custody,
    record: Value,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path();
        let store = Store::open(folder.join(".privateer")).unwrap();
        fs::write(store.home().join("config.json"), br#"{"ship":"alpha"}"#).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO ships(name,record) VALUES('alpha','{\"location\":\"here\"}')",
                [],
            )
            .unwrap();
        crate::presence::attend(&store, "alpha", "host:alpha", 90, None).unwrap();
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let daemon_child = Command::new("sleep").arg("60").spawn().unwrap();
        let custody = Custody::new(&child, folder).unwrap();
        fs::create_dir_all(&custody.dir).unwrap();
        fs::set_permissions(&custody.dir, fs::Permissions::from_mode(0o700)).unwrap();
        let native_home = folder.join("codex");
        fs::create_dir_all(native_home.join("app-server-daemon")).unwrap();
        fs::set_permissions(
            native_home.join("app-server-daemon"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        atomic(
            &native_home.join("app-server-daemon"),
            "daemon.pid",
            &json!({
            "pid": daemon_child.id(), "processIdentity": identity(daemon_child.id()).unwrap()}),
        )
        .unwrap();
        let socket_path = folder.join("s");
        let socket = UnixListener::bind(&socket_path).unwrap();
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        let rendezvous = folder.join("r");
        symlink(&socket_path, &rendezvous).unwrap();
        let generation = uuid::Uuid::new_v4();
        let token = uuid::Uuid::new_v4();
        let session = uuid::Uuid::new_v4();
        let record = json!({"pid": child.id(), "process_identity": identity(child.id()).unwrap(),
            "generation": generation, "session": session, "holder": "host:alpha", "ship": "alpha",
            "home": store.home(), "cwd": folder, "codex_home": native_home,
            "hosted": true, "state": "bound", "delivery_enabled": false,
            "executable": executable(child.id()).unwrap()["path"],
            "binding": {"session": session, "fence_version": 1, "invalidation_token": token,
                "daemon_pid": daemon_child.id(), "daemon_identity": identity(daemon_child.id()).unwrap(),
                "socket": socket_path, "rendezvous": rendezvous}});
        atomic(&custody.dir, "launch.json", &record).unwrap();
        atomic(&custody.dir, "invalidate.json", &json!({"token": token})).unwrap();
        atomic(
            &custody.dir,
            "custody-poison.json",
            &json!({"token": uuid::Uuid::new_v4(), "generation": generation}),
        )
        .unwrap();
        Self {
            _tmp: tmp,
            store,
            child,
            daemon: daemon_child,
            _socket: socket,
            custody,
            record,
        }
    }
    fn observe(&mut self) -> Result<()> {
        self.custody.observe(&self.store, "alpha", "host:alpha")
    }
    fn save(&self) {
        atomic(&self.custody.dir, "launch.json", &self.record).unwrap();
    }
    fn held(&self) -> Value {
        read(&self.custody.dir.join("custody.json")).unwrap()
    }
    fn token(&self) -> Value {
        read(&self.custody.dir.join("custody-poison.json")).unwrap()["token"].clone()
    }
    fn assert_revoked(&self) {
        let held = self.held();
        assert_eq!(held["state"], "revoked");
        assert_ne!(held["token"], self.token());
        assert_eq!(held["delivery_enabled"], false);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.custody.retire("fixture_cleanup");
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn codex_custody_owned_child_exit_is_poisoned_before_reap() {
    let mut f = Fixture::new();
    f.observe().unwrap();
    let token = f.token();
    assert_eq!(f.held()["state"], "owned");
    f.observe().unwrap();
    assert_eq!(f.token(), token); // No renewal/rotation on a poll.
    f.child.kill().unwrap();
    let mut poll = libc::pollfd {
        fd: f.custody.fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);
    f.observe().unwrap();
    f.assert_revoked();
    assert!(!f.child.wait().unwrap().success());
    // Receipt and launch facts are not edited by a custody transition.
    assert_eq!(read(&f.custody.dir.join("launch.json")).unwrap(), f.record);
}

#[test]
fn codex_custody_rejects_pid_start_executable_uuid_and_generation_replacement() {
    for field in [
        "pid",
        "process_identity",
        "executable",
        "session",
        "generation",
        "holder",
        "codex_home",
    ] {
        let mut f = Fixture::new();
        f.observe().unwrap();
        if field == "executable" {
            // Modify the held executable inode, not only the display path.
            f.custody.held.as_mut().unwrap()["executable"]["inode"] = json!(0);
        } else {
            f.record[field] = json!("replacement");
            f.save();
        }
        assert!(f.observe().is_ok());
        f.assert_revoked();
    }
}

#[test]
fn codex_custody_daemon_or_socket_replacement_revokes_without_rpc() {
    for socket in [false, true] {
        let mut f = Fixture::new();
        f.observe().unwrap();
        if socket {
            let path = Path::new(f.record["binding"]["socket"].as_str().unwrap());
            fs::remove_file(path).unwrap();
            let _replacement = UnixListener::bind(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        } else {
            atomic(&Path::new(f.record["codex_home"].as_str().unwrap()).join("app-server-daemon"),
                "daemon.pid", &json!({"pid": std::process::id(), "processIdentity": identity(std::process::id()).unwrap()})).unwrap();
        }
        f.observe().unwrap();
        f.assert_revoked();
    }
}

#[test]
fn codex_custody_prelock_poison_survives_timeout_and_cannot_be_renewed() {
    let mut f = Fixture::new();
    f.observe().unwrap();
    let before = f.token();
    let lock = lock(&f.custody.dir).unwrap();
    assert!(
        revoke(&f.custody.dir, "barrier")
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert_ne!(before, f.token());
    assert_eq!(f.held()["state"], "owned"); // Token, NOT cached state, is the fence.
    drop(lock);
    f.observe().unwrap();
    f.assert_revoked();
    let token = f.token();
    f.observe().unwrap();
    assert_eq!(f.token(), token);
}

#[test]
fn codex_custody_holder_change_is_synchronous_and_stale_leave_is_harmless() {
    let mut f = Fixture::new();
    f.observe().unwrap();
    let before = f.token();
    assert!(!crate::presence::leave(&f.store, "alpha", "stale").unwrap());
    assert_eq!(f.token(), before);
    crate::presence::attend(&f.store, "alpha", "host:new", 90, None).unwrap();
    f.assert_revoked();
    assert!(live(&f.custody.fd).unwrap()); // Revocation is not a kill/cancellation.
}

#[test]
fn codex_custody_handover_and_release_revoke_in_the_writer_transaction() {
    for release in [false, true] {
        let mut f = Fixture::new();
        f.observe().unwrap();
        if release {
            assert!(crate::presence::leave(&f.store, "alpha", "host:alpha").unwrap());
        } else {
            assert!(
                crate::presence::hand_over(&f.store, "alpha", "host:alpha", "host:new").unwrap()
            );
        }
        f.assert_revoked();
    }
}

#[test]
fn codex_custody_supervision_loss_is_sticky_and_no_old_pid_reconstruction_exists() {
    let mut f = Fixture::new();
    f.observe().unwrap();
    f.custody.retire("supervision_lost");
    f.assert_revoked();
    let token = f.token();
    f.observe().unwrap();
    assert_eq!(f.token(), token);
    // Even a new owner of the same actual Child cannot reset poisoned custody.
    let mut replacement = Custody::new(&f.child, f._tmp.path()).unwrap();
    assert!(
        replacement
            .observe(&f.store, "alpha", "host:alpha")
            .is_err()
    );
    assert_eq!(f.token(), token);
}

#[test]
fn codex_custody_initial_publication_cannot_clear_prior_poison() {
    let mut f = Fixture::new();
    poison(&f.custody.dir, "callback_uuid_changed").unwrap();
    let token = f.token();
    assert!(f.observe().is_err());
    assert_eq!(f.token(), token);
    assert!(!f.custody.dir.join("custody.json").exists());
}

#[test]
fn codex_custody_wrapper_is_not_payload_and_exec_changes_are_detected() {
    let mut f = Fixture::new();
    let mut wrapper = Command::new("/bin/sh")
        .args(["-c", "read line; exec sleep 60"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut proof = Custody::new(&wrapper, f._tmp.path()).unwrap();
    f.record["pid"] = json!(wrapper.id());
    f.record["process_identity"] = identity(wrapper.id()).unwrap();
    // The expected executable is sleep; a still-live shell cannot qualify.
    f.save();
    proof.observe(&f.store, "alpha", "host:alpha").unwrap();
    assert!(proof.held.is_none());
    // The supervisor owns and explicitly stops/reaps this wrapper; no hidden
    // payload is inferred stopped just because a different wrapper has exited.
    wrapper.kill().unwrap();
    wrapper.wait().unwrap();
    proof.observe(&f.store, "alpha", "host:alpha").unwrap();
    assert!(proof.revoked);
}
