//! Permissions cross a real gateway subprocess; all machine state is disposable.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
};

struct Machine {
    root: PathBuf,
}
impl Machine {
    fn new(root: PathBuf) -> Self {
        fs::create_dir_all(root.join("user")).unwrap();
        let m = Self { root };
        m.cli(&["init", "--json", "{}"]);
        m
    }
    fn command(&self) -> Command {
        let mut c = support::isolated(&self.root);
        c.current_dir(&self.root)
            .arg("--home")
            .arg(self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"));
        c
    }
    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn gateway(&self, verb: &str, args: Value, agent: bool) -> Value {
        let mut c = self.command();
        c.arg("gateway")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if agent {
            c.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "requester");
        }
        let mut child = c.spawn().unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "{}",
            json!({"id":"test", "command":verb, "args":args})
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn link(&self, remote: &Self, name: &str) {
        let script = remote.root.join("gateway");
        fs::write(&script, format!(r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
root=pathlib.Path({root})
os.environ.update(HOME=str(root/'user'), XDG_DATA_HOME=str(root/'data'), XDG_CONFIG_HOME=str(root/'config'), XDG_STATE_HOME=str(root/'state'), XDG_CACHE_HOME=str(root/'cache'), PRIVATEER_FLEET=str(root/'fleet.sqlite'), PRIVATEER_RESOURCE_HOME=str(root/'ledger'), PRIVATEER_SERVICE='none')
request=json.loads(sys.stdin.readline())
if request['command'].startswith('permissions.') and (root/'old').exists():
    print(json.dumps(dict(id=request['id'],ok=False,error='unknown gateway command '+request['command'])))
elif request['command'].startswith('permissions.') and (root/'malformed').exists():
    print(json.dumps(dict(id=request['id'],ok=True,result=dict(mode='yolo'))))
else:
    result=subprocess.run([{bin}, '--home', str(root/'home')]+sys.argv[1:],input=json.dumps(request)+'\n',text=True,stdout=subprocess.PIPE)
    sys.stdout.write(result.stdout)
    sys.exit(result.returncode)
"#, root=serde_json::to_string(&remote.root).unwrap(), bin=serde_json::to_string(support::BIN).unwrap())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        self.cli(&["machine.add", "--json", &json!({"name":name,"target":"unused","transport":"local","executable":script,"as":"desk"}).to_string()]);
    }
}
fn row<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap()
}

#[test]
fn status_and_on_off_cover_both_machines_and_preserve_local_policy() {
    let dir = tempfile::tempdir().unwrap();
    let desk = Machine::new(dir.path().join("desk"));
    let peer = Machine::new(dir.path().join("peer"));
    desk.link(&peer, "peer");
    peer.cli(&["permissions", "allow", "Bash(fixture *)"]);
    let before = fs::read(peer.root.join("fleet.permissions.json")).unwrap();
    let status = desk.cli(&["permissions", "yolo", "status"]);
    assert_eq!(status["machines"].as_array().unwrap().len(), 2);
    for name in ["desk", "peer"] {
        assert_eq!(row(&status, name)["mode"], "standard");
    }
    assert_eq!(row(&status, "desk")["local"], true);
    assert_eq!(
        before,
        fs::read(peer.root.join("fleet.permissions.json")).unwrap()
    );
    assert!(!desk.root.join("fleet.permissions.json").exists());
    for (args, mode) in [
        (vec!["permissions", "yolo", "on"], "yolo"),
        (vec!["permissions", "yolo", "off"], "standard"),
    ] {
        let result = desk.cli(&args);
        for name in ["desk", "peer"] {
            assert_eq!(row(&result, name)["mode"], mode);
            assert_eq!(row(&result, name)["changed"], true);
            assert_eq!(row(&result, name)["confirmed"], true);
        }
        assert_eq!(peer.cli(&["permissions"])["mode"], mode);
        assert_eq!(
            peer.cli(&["permissions"])["claude_allow"],
            json!(["Bash(fixture *)"])
        );
        let again = desk.cli(&args);
        for name in ["desk", "peer"] {
            assert_eq!(row(&again, name)["changed"], false);
        }
    }
    // The receiving gateway must not recurse through its reverse-only link.
    let reverse = peer.cli(&["permissions", "yolo", "status"]);
    assert_eq!(row(&reverse, "desk")["reason"], "reverse-only link");
}

#[test]
fn failures_and_old_gateways_stay_named_without_claiming_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let desk = Machine::new(dir.path().join("desk"));
    let old = Machine::new(dir.path().join("old"));
    let lost = Machine::new(dir.path().join("lost"));
    desk.link(&old, "old");
    desk.link(&lost, "lost");
    fs::write(old.root.join("old"), "").unwrap();
    fs::remove_file(lost.root.join("gateway")).unwrap();
    for args in [
        vec!["permissions", "yolo", "status"],
        vec!["permissions", "yolo"],
        vec!["permissions", "standard"],
    ] {
        let report = desk.cli(&args);
        assert_eq!(report["machines"].as_array().unwrap().len(), 3);
        for (name, reason) in [("old", "older version"), ("lost", "unreachable")] {
            let r = row(&report, name);
            assert_eq!(r["confirmed"], false);
            assert!(r["mode"].is_null() && r["changed"].is_null(), "{r}");
            assert_eq!(r["reason"], reason);
        }
    }
    assert_eq!(old.cli(&["permissions"])["mode"], "standard");
    fs::remove_file(old.root.join("old")).unwrap();
    fs::write(old.root.join("malformed"), "").unwrap();
    assert_eq!(
        row(&desk.cli(&["permissions", "yolo"]), "old")["confirmed"],
        false
    );
}

#[test]
fn gateway_refuses_agents_and_non_boolean_or_extra_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path().join("machine"));
    for (verb, args) in [
        ("permissions.status", json!({})),
        ("permissions.yolo", json!({"on":true})),
    ] {
        let reply = m.gateway(verb, args, true);
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("operator"),
            "{reply}"
        );
    }
    for args in [
        json!({}),
        json!({"on":"true"}),
        json!({"on":1}),
        json!({"on":true,"path":"/unused"}),
    ] {
        assert_eq!(m.gateway("permissions.yolo", args, false)["ok"], false);
    }
    assert_eq!(
        m.gateway("permissions.status", json!({"path":"/unused"}), false)["ok"],
        false
    );
    assert!(!m.root.join("fleet.permissions.json").exists());
}
