//! Linked upgrades cross the real gateway and a local-transport link. Every
//! executable, HOME, registry, unit directory and release is disposable.
mod support;
use privateer::releases::fetch::{SUMS, machine_platform};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use support::BIN;
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn sums(root: &Path) {
    fn walk(root: &Path, dir: &Path, names: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                walk(root, &p, names);
            } else {
                names.push(p.strip_prefix(root).unwrap().to_str().unwrap().into());
            }
        }
    }
    let mut names = Vec::new();
    walk(root, root, &mut names);
    names.retain(|n| n != SUMS);
    names.sort();
    fs::write(
        root.join(SUMS),
        names
            .iter()
            .map(|n| {
                format!(
                    "{:x}  {n}\n",
                    Sha256::digest(fs::read(root.join(n)).unwrap())
                )
            })
            .collect::<String>(),
    )
    .unwrap();
}
fn release(data: &Path, commit: &str) {
    let root = data.join("releases").join(commit);
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::copy(BIN, root.join("bin/privateer")).unwrap();
    fs::copy(BROKER, root.join("bin/privateer-mcp-broker")).unwrap();
    executable(&root.join("bin/privateer-dashboard"), "#!/bin/sh\nexit 0\n");
    let plugins = root.join("share/privateer/plugins/pi");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("manifest.json"),
        r#"{"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}"#,
    )
    .unwrap();
    sums(&root);
}
struct Machine {
    root: PathBuf,
}
impl Machine {
    fn new(root: PathBuf, target: bool) -> Self {
        let m = Self { root };
        fs::create_dir_all(m.root.join("user")).unwrap();
        release(&m.data(), A);
        if target {
            release(&m.data(), B);
        }
        symlink(Path::new("releases").join(A), m.data().join("current")).unwrap();
        let output = m.command(&["init", "--json", "{}"]).output().unwrap();
        assert!(output.status.success(), "{}", text(&output));
        m
    }
    fn data(&self) -> PathBuf {
        self.root.join("data/privateer")
    }
    fn entry(&self) -> PathBuf {
        self.data().join("current/bin/privateer")
    }
    fn selected(&self) -> String {
        fs::read_link(self.data().join("current"))
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .into()
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = support::command(self.entry());
        cmd.current_dir(&self.root)
            .args(["--home"])
            .arg(self.root.join("home"))
            .args(args)
            .env("HOME", self.root.join("user"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_GLIBC", "glibc 2.39")
            .env("PRIVATEER_GH", self.root.join("no-gh"))
            .env_remove("PRIVATEER_UPGRADE_SOURCE")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        cmd
    }
    fn gateway(&self, command: &str, args: Value) -> Value {
        let mut child = self
            .command(&["gateway"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"id":"test","command":command,"args":args})
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn ok(&self, command: &str, args: Value) -> Value {
        let reply = self.gateway(command, args);
        assert_eq!(reply["ok"], true, "{reply}");
        reply["result"].clone()
    }
    fn wait(&self, kind: &str, round: Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            assert!(Instant::now() < deadline, "round timed out: {round}");
            let state = self.ok("upgrade.progress", json!({"kind":kind,"id":round["id"]}));
            if !state["outcome"].is_null() {
                return state;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fn link(&self, remote: &Machine, name: &str) {
        // The wrapper supplies the second machine's environment, just as an
        // SSH login would. It forwards to the actual gateway subprocess.
        let script = remote.root.join("gateway");
        executable(
            &script,
            &format!(
                r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
root=pathlib.Path({root})
desk=pathlib.Path({desk})
os.environ.update(HOME=str(root/'user'), XDG_DATA_HOME=str(root/'data'), XDG_CONFIG_HOME=str(root/'config'), XDG_STATE_HOME=str(root/'state'), XDG_CACHE_HOME=str(root/'cache'), PRIVATEER_FLEET=str(root/'fleet.sqlite'), PRIVATEER_RESOURCE_HOME=str(root/'ledger'), PRIVATEER_SERVICE='none', PRIVATEER_GH=str(root/'no-gh'), PRIVATEER_GLIBC='glibc 2.39')
os.environ.pop('PRIVATEER_UPGRADE_SOURCE',None)
request=json.loads(sys.stdin.readline())
command=request['command']
def ready(r):
    return any(json.loads(p.read_text()).get('outcome')=='ready' for p in (r/'data/privateer/upgrade-prepare').glob('*.json'))
with (root/'calls').open('a') as f:
    f.write(json.dumps(dict(command=command,desk_ready=ready(desk),remote_ready=ready(root),desk_current=os.readlink(desk/'data/privateer/current'),remote_current=os.readlink(root/'data/privateer/current'))) +'\n')
if command=='upgrade.probe' and (root/'old-gateway').exists():
    print(json.dumps(dict(id=request['id'],ok=False,error='unknown gateway command upgrade.probe')))
elif command=='upgrade.apply' and (root/'fail-apply').exists():
    print(json.dumps(dict(id=request['id'],ok=False,error='injected apply refusal')))
elif command=='upgrade.apply' and (root/'apply-outcome').exists():
    print(json.dumps(dict(id=request['id'],ok=True,result=dict(outcome=(root/'apply-outcome').read_text()))))
else:
    result=subprocess.run([str(root/'data/privateer/current/bin/privateer')]+sys.argv[1:],input=json.dumps(request)+'\n',text=True,stdout=subprocess.PIPE)
    sys.stdout.write(result.stdout)
    sys.exit(result.returncode)
"#,
                root = serde_json::to_string(&remote.root).unwrap(),
                desk = serde_json::to_string(&self.root).unwrap()
            ),
        );
        let out = self.command(&["machine.add","--json",&json!({"name":name,"target":"unused","transport":"local","executable":script,"as":"desk"}).to_string()]).output().unwrap();
        assert!(out.status.success(), "{}", text(&out));
    }
    fn upgrade(&self) -> Output {
        self.command(&["upgrade", B]).output().unwrap()
    }
    fn rpc(&self, verb: &str, args: Value, agent: bool) -> Output {
        let mut command = self.command(&[verb, "--json", &args.to_string()]);
        if agent {
            command
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "requester");
        }
        command.output().unwrap()
    }
    fn json(&self, verb: &str, args: Value, agent: bool) -> Value {
        let out = self.rpc(verb, args, agent);
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn request_setup(&self) {
        self.json(
            "ship.put",
            json!({"name":"requester","scope":"fixture upgrades"}),
            false,
        );
        self.json(
            "charter.put",
            json!({"id":"upgrade-charter","objective":"Upgrade disposable fixture machines",
                "repo":self.root,"actions":["inspect","publish_pr","merge_green"]}),
            false,
        );
        self.json("task.create", json!({"id":"upgrade-task","charter_id":"upgrade-charter","owner":"requester","title":"Upgrade fixture"}), false);
    }
    fn approve(&self, id: &str, commit: &str, environment: &Value) {
        self.json(
            "approval.grant",
            json!({"id":id,"task_id":"upgrade-task","artifact":commit,
            "environment":environment,"approver":"captain"}),
            false,
        );
    }
    fn request(&self, approval: Option<&str>) -> Output {
        let mut args = json!({"commit":B,"task_id":"upgrade-task"});
        if let Some(approval) = approval {
            args["approval_id"] = json!(approval);
        }
        self.rpc("upgrade.start", args, true)
    }
    fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.root.join("home/privateer.sqlite")).unwrap()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("calls"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
fn fixture(target: bool) -> (tempfile::TempDir, Machine, Machine) {
    let dir = tempfile::tempdir().unwrap();
    let desk = Machine::new(dir.path().join("desk"), true);
    let remote = Machine::new(dir.path().join("peer"), target);
    desk.link(&remote, "peer");
    (dir, desk, remote)
}

#[test]
fn linked_prepare_barrier_and_serial_apply_over_real_gateway() {
    let (_dir, desk, remote) = fixture(true);
    let probe = remote.ok("upgrade.probe", json!({"commit":B}));
    assert_eq!(probe["verified"], true);
    assert_eq!(probe["current_commit"], A);
    let out = desk.upgrade();
    assert!(out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["phase"], "all_prepared");
    assert_eq!(report["outcome"], "switched");
    assert_eq!(desk.selected(), B);
    assert_eq!(remote.selected(), B);
    let calls = remote.calls();
    let apply = calls
        .iter()
        .find(|c| c["command"] == "upgrade.apply")
        .unwrap();
    assert_eq!(apply["desk_ready"], true, "{calls:?}");
    assert_eq!(apply["remote_ready"], true);
    assert_eq!(apply["desk_current"], format!("releases/{A}"));
    assert_eq!(report["machines"][0]["name"], "peer");
    assert_eq!(report["machines"][1]["name"], "local");
    assert!(
        report["machines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["summary"].as_str().unwrap().contains("switched"))
    );
    let rolled = remote.wait("apply", remote.ok("upgrade.rollback", json!({})));
    assert_eq!(rolled["outcome"], "switched");
    assert_eq!(remote.selected(), A);
    assert_eq!(desk.selected(), B);
    let status = desk.command(&["upgrade", "status"]).output().unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["linked_round"]["id"], report["id"]);
}

#[test]
fn failed_remote_prepare_switches_nothing() {
    let (_dir, desk, remote) = fixture(true);
    fs::write(
        remote
            .data()
            .join("releases")
            .join(B)
            .join("bin/privateer-dashboard"),
        "corrupt",
    )
    .unwrap();
    let out = desk.upgrade();
    assert!(!out.status.success(), "{}", text(&out));
    assert_eq!(desk.selected(), A);
    assert_eq!(remote.selected(), A);
    assert!(
        !remote
            .calls()
            .iter()
            .any(|c| c["command"] == "upgrade.apply")
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["outcome"], "failed");
    assert!(
        report["machines"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("nothing switched")
    );
}

#[test]
fn local_prepare_failure_after_remote_ready_still_prevents_every_apply() {
    let (_dir, desk, remote) = fixture(true);
    fs::write(
        desk.data()
            .join("releases")
            .join(B)
            .join("bin/privateer-dashboard"),
        "corrupt",
    )
    .unwrap();
    let out = desk.upgrade();
    assert!(!out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["machines"][0]["prepare"]["outcome"], "ready");
    assert_eq!(desk.selected(), A);
    assert_eq!(remote.selected(), A);
    assert!(
        !remote
            .calls()
            .iter()
            .any(|c| c["command"] == "upgrade.apply")
    );
}

#[test]
fn old_gateway_is_explicitly_skipped() {
    let (_dir, desk, remote) = fixture(true);
    fs::write(remote.root.join("old-gateway"), "").unwrap();
    let out = desk.upgrade();
    assert!(out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["machines"][0]["outcome"], "skipped_old_gateway");
    assert_eq!(report["outcome"], "switched_with_skips");
    let status = desk.command(&["upgrade", "status"]).output().unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["linked_round"]["outcome"], "switched_with_skips");
    assert_eq!(remote.selected(), A);
    assert_eq!(desk.selected(), B);
    assert!(
        !remote
            .calls()
            .iter()
            .any(|c| c["command"] == "upgrade.prepare" || c["command"] == "upgrade.apply")
    );
}

#[test]
fn failed_apply_stops_and_leaves_already_switched_machines() {
    let (dir, desk, first) = fixture(true);
    let second = Machine::new(dir.path().join("second"), true);
    desk.link(&second, "second");
    fs::write(second.root.join("fail-apply"), "").unwrap();
    let out = desk.upgrade();
    assert!(!out.status.success(), "{}", text(&out));
    assert_eq!(first.selected(), B);
    assert_eq!(second.selected(), A);
    assert_eq!(desk.selected(), A);
    assert!(text(&out).contains("already switched machines left in place"));
    // Retrying must skip the first machine, not stop at its no-op refusal.
    fs::remove_file(second.root.join("fail-apply")).unwrap();
    let retried = desk.upgrade();
    assert!(retried.status.success(), "{}", text(&retried));
    let report: Value = serde_json::from_slice(&retried.stdout).unwrap();
    assert_eq!(report["machines"][0]["outcome"], "already_current");
    assert!(report["machines"][0]["prepare"].is_null());
    assert!(report["machines"][0]["apply"].is_null());
    assert_eq!(first.selected(), B);
    assert_eq!(second.selected(), B);
    assert_eq!(desk.selected(), B);
    assert_eq!(
        first
            .calls()
            .iter()
            .filter(|c| c["command"] == "upgrade.apply")
            .count(),
        1
    );
}

#[test]
fn already_current_machine_mid_order_does_not_stop_later_machines() {
    let (dir, desk, first) = fixture(true);
    let middle = Machine::new(dir.path().join("middle"), true);
    let last = Machine::new(dir.path().join("last"), true);
    desk.link(&middle, "q-middle");
    desk.link(&last, "z-last");
    let applied = middle.wait("apply", middle.ok("upgrade.apply", json!({"commit":B})));
    assert_eq!(applied["outcome"], "switched");
    // Exclude the machine.add directory exchange from this round's calls.
    fs::write(middle.root.join("calls"), "").unwrap();
    let out = desk.upgrade();
    assert!(out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["machines"][1]["outcome"], "already_current");
    assert!(
        report["machines"][1]["summary"]
            .as_str()
            .unwrap()
            .contains("already_current")
    );
    assert_eq!(report["machines"][2]["outcome"], "switched");
    assert_eq!(desk.selected(), B);
    assert_eq!(first.selected(), B);
    assert_eq!(middle.selected(), B);
    assert_eq!(last.selected(), B);
    assert!(
        middle
            .calls()
            .iter()
            .all(|c| c["command"] == "upgrade.probe")
    );
}

#[test]
fn already_current_but_corrupt_release_fails_before_any_apply() {
    let (_dir, desk, remote) = fixture(true);
    remote.wait("apply", remote.ok("upgrade.apply", json!({"commit":B})));
    fs::write(remote.root.join("calls"), "").unwrap();
    fs::write(
        remote
            .data()
            .join("releases")
            .join(B)
            .join("bin/privateer-dashboard"),
        "corrupt",
    )
    .unwrap();
    let out = desk.upgrade();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("current target release did not verify"));
    assert_eq!(desk.selected(), A);
    assert!(
        remote
            .calls()
            .iter()
            .all(|c| c["command"] == "upgrade.probe")
    );
}

#[test]
fn terminal_unsuccessful_apply_results_stop_later_machines() {
    let (dir, desk, first) = fixture(true);
    let second = Machine::new(dir.path().join("second"), true);
    desk.link(&second, "second");
    for outcome in ["rolled_back", "switched_with_failures"] {
        fs::write(second.root.join("apply-outcome"), outcome).unwrap();
        let out = desk.upgrade();
        assert!(!out.status.success(), "{}", text(&out));
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["machines"][1]["outcome"], outcome);
        assert_eq!(report["outcome"], "failed");
        assert!(text(&out).contains(&format!("apply ended {outcome}")));
        assert_eq!(first.selected(), B);
        assert_eq!(desk.selected(), A);
    }
}

#[test]
fn operator_can_close_stranded_round_after_inspection_but_not_live_coordinator() {
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let desk = Machine::new(dir.path().join("desk"), true);
    let states = desk.data().join("upgrade-linked");
    fs::create_dir_all(&states).unwrap();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let path = states.join(format!("{id}.json"));
    let mut state = json!({"id":id,"kind":"linked","requested_ms":1,"outcome":null,
        "machines":[{"name":"unreachable","outcome":"applying"}]});
    let write = |state: &Value| fs::write(&path, state.to_string()).unwrap();
    let interrupt = || {
        desk.command(&["upgrade", "interrupt-linked", id, "--inspected"])
            .output()
            .unwrap()
    };
    write(&state);
    let agent = desk
        .command(&[
            "--role",
            "agent",
            "--actor",
            "crew",
            "upgrade",
            "interrupt-linked",
            id,
            "--inspected",
        ])
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert!(text(&agent).contains("operator"));
    let unconfirmed = desk
        .command(&["upgrade", "interrupt-linked", id])
        .output()
        .unwrap();
    assert!(!unconfirmed.status.success());
    // No lock holder does not suffice when the recorded pid is live.
    state["pid"] = json!(std::process::id());
    write(&state);
    let live = interrupt();
    assert!(!live.status.success());
    assert!(text(&live).contains("still live"));
    state.as_object_mut().unwrap().remove("pid");
    write(&state);
    let guard = fs::File::create(desk.data().join("upgrade-linked.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let locked = interrupt();
    assert!(!locked.status.success());
    assert!(text(&locked).contains("another upgrade operation"));
    drop(guard);
    // Refuse the spawn-to-worker handoff window too.
    state["requested_ms"] = json!(privateer::store::Store::now_ms());
    write(&state);
    let starting = interrupt();
    assert!(!starting.status.success());
    assert!(text(&starting).contains("starting"));
    state["requested_ms"] = json!(1);
    // Both a lost custody id and an unreachable recorded id previously
    // required editing state by hand. The explicit command handles either.
    for apply in [
        Value::Null,
        json!({"id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"}),
    ] {
        state["machines"][0]["apply"] = apply;
        write(&state);
        let blocked = desk.upgrade();
        assert!(!blocked.status.success(), "{}", text(&blocked));
        let closed = interrupt();
        assert!(closed.status.success(), "{}", text(&closed));
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["outcome"], "interrupted");
        assert_eq!(saved["interrupted_by"], "operator");
        assert_eq!(saved["machines"], state["machines"]);
        assert!(
            !interrupt().status.success(),
            "cannot close a finished round"
        );
        let retry = desk.upgrade();
        assert!(retry.status.success(), "{}", text(&retry));
        assert_eq!(desk.selected(), B);
    }
}

#[test]
fn agents_are_refused_every_upgrade_gateway_command() {
    let dir = tempfile::tempdir().unwrap();
    let machine = Machine::new(dir.path().join("peer"), true);
    for command in [
        "upgrade.probe",
        "upgrade.prepare",
        "upgrade.apply",
        "upgrade.rollback",
        "upgrade.progress",
        "upgrade.chunk",
    ] {
        let mut child = machine
            .command(&["--role", "agent", "--actor", "crew", "gateway"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"id":"agent","command":command,"args":{}})
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let response: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(response["ok"], false, "{command}: {response}");
        assert!(
            response["error"].as_str().unwrap().contains("operator"),
            "{response}"
        );
    }
    assert_eq!(machine.selected(), A);
    assert!(!machine.data().join("upgrade").exists());
    assert!(!machine.data().join("upgrade-prepare").exists());
    assert!(!machine.data().join("upgrade-upload").exists());
}

fn platform() -> String {
    machine_platform(&String::from_utf8_lossy(
        &Command::new("uname").arg("-sm").output().unwrap().stdout,
    ))
    .unwrap()
    .into()
}
#[test]
fn chunk_bounds_corruption_stale_ids_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let remote = Machine::new(dir.path().join("peer"), false);
    let begin = json!({"action":"begin","commit":B,"platform":platform(),"digest":format!("{:x}",Sha256::digest(b"abc")),"total":3});
    let mut oversized = begin.clone();
    oversized["total"] = json!((1u64 << 30) + 1);
    let refused = remote.gateway("upgrade.chunk", oversized);
    assert_eq!(refused["ok"], false);
    assert!(refused["error"].as_str().unwrap().contains("1 GiB"));
    let upload = remote.ok("upgrade.chunk", begin.clone());
    let id = &upload["id"];
    assert_eq!(
        remote.gateway(
            "upgrade.chunk",
            json!({"action":"append","id":"../../outside","offset":0,"hex":"616263"})
        )["ok"],
        false
    );
    assert!(remote.data().join("upgrade-upload/incoming").exists());
    remote.ok(
        "upgrade.chunk",
        json!({"action":"append","id":id,"offset":0,"hex":"616264"}),
    );
    let bad = remote.gateway("upgrade.chunk", json!({"action":"finish","id":id}));
    assert_eq!(bad["ok"], false);
    assert!(bad["error"].as_str().unwrap().contains("digest"));
    assert!(!remote.data().join("upgrade-upload/incoming").exists());
    let upload = remote.ok("upgrade.chunk", begin.clone());
    assert_eq!(
        remote.gateway(
            "upgrade.chunk",
            json!({"action":"append","id":upload["id"],"offset":0,"hex":"00".repeat(192*1024+1)})
        )["ok"],
        false
    );
    assert!(!remote.data().join("upgrade-upload/incoming").exists());
    let upload = remote.ok("upgrade.chunk", begin.clone());
    assert_eq!(
        remote.gateway(
            "upgrade.chunk",
            json!({"action":"append","id":upload["id"],"offset":1,"hex":"61"})
        )["ok"],
        false
    );
    assert!(!remote.data().join("upgrade-upload/incoming").exists());
    // A completed upload is rechecked before unzip, even if changed at rest.
    let upload = remote.ok("upgrade.chunk", begin.clone());
    remote.ok(
        "upgrade.chunk",
        json!({"action":"append","id":upload["id"],"offset":0,"hex":"616263"}),
    );
    remote.ok(
        "upgrade.chunk",
        json!({"action":"finish","id":upload["id"]}),
    );
    // Stray active-id calls cannot discard a digest-checked artifact.
    for action in ["append", "finish", "unknown"] {
        let refused = remote.gateway(
            "upgrade.chunk",
            json!({"action":action,"id":upload["id"],"offset":3,"hex":"00"}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(
            fs::read(remote.data().join("upgrade-upload/incoming/artifact.zip")).unwrap(),
            b"abc"
        );
    }
    fs::write(
        remote.data().join("upgrade-upload/incoming/artifact.zip"),
        b"abd",
    )
    .unwrap();
    let failed = remote.wait(
        "prepare",
        remote.ok("upgrade.prepare", json!({"commit":B,"upload":upload["id"]})),
    );
    assert_eq!(failed["outcome"], "failed");
    assert!(failed["error"].as_str().unwrap().contains("digest"));
    assert!(!remote.data().join("upgrade-upload/incoming").exists());
    assert!(
        fs::read_dir(remote.data().join("staging"))
            .unwrap()
            .next()
            .is_none()
    );
    // A dropped sender cannot reserve the sole upload slot indefinitely.
    let stale = remote.ok("upgrade.chunk", begin.clone());
    let incoming = remote.data().join("upgrade-upload/incoming");
    fs::File::open(&incoming)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(31 * 60)),
        )
        .unwrap();
    let fresh = remote.ok("upgrade.chunk", begin);
    assert_ne!(stale["id"], fresh["id"]);
    remote.ok("upgrade.chunk", json!({"action":"abort","id":fresh["id"]}));
    assert_eq!(remote.selected(), A);
    assert!(!remote.data().join("releases").join(B).exists());
}

/// The desk downloads an API-identified zip; the remote has no gh at all.
#[test]
fn desk_pushes_chunked_artifact_to_machine_without_gh() {
    let (_dir, desk, remote) = fixture(false);
    let platform = platform();
    let artifact = desk.root.join("artifact.zip");
    let tarball = desk.root.join(format!("privateer-{platform}.tar.gz"));
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&tarball)
        .arg("-C")
        .arg(desk.data().join("releases"))
        .arg(B)
        .status()
        .unwrap();
    assert!(status.success());
    let status=Command::new("python3").args(["-I","-c","import sys,zipfile,pathlib; p=pathlib.Path(sys.argv[2]); z=zipfile.ZipFile(sys.argv[1],'w'); z.write(p,p.name); z.close()"]).arg(&artifact).arg(&tarball).status().unwrap();
    assert!(status.success());
    assert!(fs::metadata(&artifact).unwrap().len() > 192 * 1024);
    let job = if platform.starts_with("macos") {
        "macOS arm64 release"
    } else if platform.ends_with("x86_64") {
        "Linux x86_64 release"
    } else {
        "Linux aarch64 release"
    };
    let repo = "kirkins/privateer";
    let routes = json!({
        format!("repos/{repo}/actions/runs?head_sha={B}&event=push&branch=main&per_page=100"):{"workflow_runs":[{"id":1,"head_sha":B,"event":"push","head_branch":"main","path":".github/workflows/ci.yml","status":"completed","repository":{"full_name":repo},"head_repository":{"full_name":repo}}]},
        format!("repos/{repo}/actions/runs/1/jobs?per_page=100"):{"jobs":[{"name":job,"status":"completed","conclusion":"success"}]},
        format!("repos/{repo}/actions/runs/1/artifacts?name=privateer-{platform}-{B}"):{"artifacts":[{"id":2,"name":format!("privateer-{platform}-{B}"),"expired":false,"digest":format!("sha256:{:x}",Sha256::digest(fs::read(&artifact).unwrap()))}]}
    });
    fs::write(desk.root.join("routes.json"), routes.to_string()).unwrap();
    executable(
        &desk.root.join("no-gh"),
        &format!(
            r#"#!/usr/bin/env python3
import json, pathlib, sys
root=pathlib.Path({root})
if sys.argv[1:] == ['--version']:
    print('gh fake'); sys.exit(0)
endpoint=sys.argv[-1]
if endpoint.endswith('/artifacts/2/zip'):
    sys.stdout.buffer.write((root/'artifact.zip').read_bytes())
else:
    print(json.dumps(json.loads((root/'routes.json').read_text())[endpoint]))
"#,
            root = serde_json::to_string(&desk.root).unwrap()
        ),
    );
    let out = desk.upgrade();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(desk.selected(), B);
    assert_eq!(remote.selected(), B);
    let calls = remote.calls();
    assert!(
        calls
            .iter()
            .filter(|c| c["command"] == "upgrade.chunk")
            .count()
            > 3
    );
    assert!(!remote.data().join("upgrade-upload/incoming").exists());
    assert!(
        fs::read_dir(remote.data().join("staging"))
            .unwrap()
            .next()
            .is_none()
    );
    privateer::releases::fetch::verify(&remote.data().join("releases").join(B)).unwrap();
}

#[test]
fn agent_upgrade_requires_exact_unused_unexpired_captain_approval() {
    let (_dir, desk, _remote) = fixture(true);
    desk.request_setup();
    let plan = desk.json("upgrade.plan", json!({"commit":B}), true);
    assert_eq!(plan["dry_run"], true);
    assert!(!desk.request(None).status.success());
    desk.approve("wrong-commit", A, &plan["environment"]);
    assert!(!desk.request(Some("wrong-commit")).status.success());
    desk.approve("wrong-machines", B, &json!("upgrade:another-set"));
    assert!(!desk.request(Some("wrong-machines")).status.success());
    desk.approve("expired", B, &plan["environment"]);
    desk.database()
        .execute("UPDATE approvals SET expires_ms=0 WHERE id='expired'", [])
        .unwrap();
    assert!(!desk.request(Some("expired")).status.success());
    desk.approve("legacy", B, &plan["environment"]);
    desk.database()
        .execute("UPDATE approvals SET expires_ms=NULL WHERE id='legacy'", [])
        .unwrap();
    assert!(!desk.request(Some("legacy")).status.success());
    desk.json(
        "approval.grant",
        json!({"id":"not-captain","task_id":"upgrade-task","artifact":B,
        "environment":plan["environment"],"approver":"another-reviewer"}),
        false,
    );
    assert!(!desk.request(Some("not-captain")).status.success());
    desk.approve("stale-charter", B, &plan["environment"]);
    desk.database()
        .execute(
            "UPDATE approvals SET charter_hash='old-charter' WHERE id='stale-charter'",
            [],
        )
        .unwrap();
    assert!(!desk.request(Some("stale-charter")).status.success());
    assert_eq!(desk.selected(), A);
    desk.approve("one-deployment", B, &plan["environment"]);
    let out = desk.request(Some("one-deployment"));
    assert!(out.status.success(), "{}", text(&out));
    let started: Value = serde_json::from_slice(&out.stdout).unwrap();
    let round = desk.wait("linked", started.clone());
    assert_eq!(round["outcome"], "switched", "{round}");
    assert_eq!(round["production_approval"], "one-deployment");
    assert_eq!(round["requested_by"], "requester");
    assert!(!desk.request(Some("one-deployment")).status.success());
    let db = desk.database();
    let used: i64 = db
        .query_row(
            "SELECT consumed FROM approvals WHERE id='one-deployment'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(used, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let replies: i64 = db.query_row("SELECT COUNT(*) FROM mail WHERE kind='reply' AND recipient='requester' AND correlation=?1", [started["request_thread"].as_str().unwrap()], |r| r.get(0)).unwrap();
        if replies == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "outcome reply missing");
        std::thread::sleep(Duration::from_millis(30));
    }
    let (body, payload): (String, String) = db
        .query_row(
            "SELECT body,payload FROM mail WHERE kind='reply' AND recipient='requester'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        body.contains("peer: switched") && body.contains("local: switched"),
        "{body}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap()["id"],
        round["id"]
    );
    let notices: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE recipient='captain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 0);
    // Model a crash after the terminal state was saved but before the mail
    // transaction committed. Recovery must mail, never replay the switch.
    db.execute(
        "DELETE FROM mail WHERE id=?1",
        [format!("upgrade-outcome-{}", round["id"].as_str().unwrap())],
    )
    .unwrap();
    db.execute(
        "UPDATE mail SET state='open',answered_by=NULL,answered_ms=NULL WHERE id=?1",
        [started["request_thread"].as_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2)",
        rusqlite::params![
            format!("upgrade-request:{}", round["id"].as_str().unwrap()),
            round.to_string()
        ],
    )
    .unwrap();
    let recovery = desk
        .command(&["serve", "--json", "{\"once\":true}"])
        .output()
        .unwrap();
    assert!(recovery.status.success(), "{}", text(&recovery));
    let replies: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE kind='reply' AND recipient='requester'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(replies, 1);
    let recovery = desk
        .command(&["serve", "--json", "{\"once\":true}"])
        .output()
        .unwrap();
    assert!(recovery.status.success(), "{}", text(&recovery));
    let replies: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE kind='reply' AND recipient='requester'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(replies, 1);
}

#[test]
fn two_agent_requests_race_for_one_deployment_approval() {
    let (_dir, desk, _remote) = fixture(true);
    desk.request_setup();
    let plan = desk.json("upgrade.plan", json!({"commit":B}), true);
    desk.approve("raced", B, &plan["environment"]);
    let args = json!({"commit":B,"task_id":"upgrade-task","approval_id":"raced"}).to_string();
    let spawn = || {
        desk.command(&["upgrade.start", "--json", &args])
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "requester")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let one = spawn();
    let two = spawn();
    let one = one.wait_with_output().unwrap();
    let two = two.wait_with_output().unwrap();
    assert_eq!(
        usize::from(one.status.success()) + usize::from(two.status.success()),
        1,
        "one: {} two: {}",
        text(&one),
        text(&two)
    );
    let winner = if one.status.success() { one } else { two };
    let round = desk.wait("linked", serde_json::from_slice(&winner.stdout).unwrap());
    assert_eq!(round["outcome"], "switched", "{round}");
    let requests: i64 = desk
        .database()
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE recipient='crew' AND kind='request'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(requests, 1);
}

#[test]
fn changing_link_destination_invalidates_the_approved_machine_set() {
    let (_dir, desk, remote) = fixture(true);
    desk.request_setup();
    let plan = desk.json("upgrade.plan", json!({"commit":B}), true);
    desk.approve("before-retarget", B, &plan["environment"]);
    let conn = rusqlite::Connection::open(desk.root.join("fleet.sqlite")).unwrap();
    conn.execute(
        "UPDATE machines SET peer=json_set(peer,'$.executable','another-gateway') WHERE name='peer'",
        [],
    )
    .unwrap();
    let after = desk.json("upgrade.plan", json!({"commit":B}), true);
    assert_ne!(plan["environment"], after["environment"]);
    assert!(!desk.request(Some("before-retarget")).status.success());
    assert_eq!(desk.selected(), A);
    assert_eq!(remote.selected(), A);
}

#[test]
fn a_round_with_every_machine_current_is_already_current() {
    let (_dir, desk, remote) = fixture(true);
    assert!(desk.upgrade().status.success());
    let output = desk.upgrade();
    assert!(output.status.success(), "{}", text(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "already_current", "{report}");
    assert!(
        report["machines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["outcome"] == "already_current")
    );
    assert_eq!(
        remote
            .calls()
            .iter()
            .filter(|c| c["command"] == "upgrade.apply")
            .count(),
        1
    );
}
