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
os.environ.update(HOME=str(root/'user'), XDG_DATA_HOME=str(root/'data'), XDG_CONFIG_HOME=str(root/'config'), XDG_STATE_HOME=str(root/'state'), PRIVATEER_FLEET=str(root/'fleet.sqlite'), PRIVATEER_RESOURCE_HOME=str(root/'ledger'), PRIVATEER_SERVICE='none', PRIVATEER_GH=str(root/'no-gh'), PRIVATEER_GLIBC='glibc 2.39')
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
}

#[test]
fn agents_are_refused_every_upgrade_gateway_command() {
    let dir = tempfile::tempdir().unwrap();
    for command in [
        "upgrade.probe",
        "upgrade.prepare",
        "upgrade.apply",
        "upgrade.rollback",
        "upgrade.progress",
        "upgrade.chunk",
    ] {
        let mut child = support::command(BIN)
            .env("HOME", dir.path())
            .env("PRIVATEER_FLEET", dir.path().join("fleet.sqlite"))
            .args(["--role", "agent", "--actor", "crew", "gateway"])
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
    assert!(!dir.path().join("fleet.sqlite").exists());
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
