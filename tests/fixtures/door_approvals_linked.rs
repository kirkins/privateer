// Prepare-only linked approvals cross a supervised scoped door and actual
// gateway subprocesses. Fault wrappers change transport/auth, never fake consent.
struct LinkedChat {
    fleet: Fleet,
    remote: PathBuf,
    mode: PathBuf,
    calls: PathBuf,
}
impl LinkedChat {
    fn new() -> Self {
        let fleet = Fleet::with_ships(&[("alpha", true), ("gamma", false)]);
        let remote = fleet.root.join("beta");
        let home = remote.join("beta");
        fs::create_dir_all(&home).unwrap();
        fs::write(fleet.root.join("fake-ssh"), "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
        fs::set_permissions(fleet.root.join("fake-ssh"), fs::Permissions::from_mode(0o755)).unwrap();
        let output = fleet.command(&home).env("HOME", &remote)
            .env("PRIVATEER_FLEET", remote.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", remote.join("ledger"))
            .args(["init", "--ship", "beta", "--harness", "codex"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        fleet.ok(&["machine", "add", "beta", "fixture", "--executable", BIN,
            "--remote-fleet", remote.join("fleet.sqlite").to_str().unwrap(), "--as", "alpha"]);
        let result = Self { mode: fleet.root.join("chat-mode"), calls: fleet.root.join("chat-calls"), fleet, remote };
        result.remote_call("mail.send", json!({"id":"ask","from":"beta","to":"captain","kind":"request",
            "task_id":"beta-duty","body":"body-canary",
            "payload":{"approves":{"action":"Submit build 55","command_prefix":"make release","ttl_secs":86400}}}));
        fs::write(&result.mode, "live").unwrap();
        result
    }
    fn remote_call(&self, command: &str, args: Value) -> Value {
        let output = self.fleet.command(&self.remote.join("beta")).env("HOME", &self.remote)
            .env("PRIVATEER_FLEET", self.remote.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.remote.join("ledger"))
            .args([command, "--json", &args.to_string()]).output().unwrap();
        assert!(output.status.success(), "{command}: {}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn gateway(&self, request: &Value, agent: bool) -> Value {
        use std::io::Write;
        let mut command = self.fleet.command(&self.remote.join("beta"));
        command.env("HOME", &self.remote)
            .env("PRIVATEER_FLEET", self.remote.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.remote.join("ledger"));
        if agent { command.args(["--role", "agent", "--actor", "beta"]); }
        let mut child = command.arg("gateway").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn remote_store(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.remote.join("beta/.privateer/privateer.sqlite")).unwrap()
    }
    fn registry(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.fleet.root.join("state/fleet.sqlite")).unwrap()
    }
    fn wrapper(&self, grant: &str) {
        fs::write(self.fleet.root.join("fake-ssh"), format!(r#"#!/usr/bin/env python3
import json, subprocess, sys, pathlib, sqlite3, os
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f: f.write(json.dumps(r)+'\n')
if r['command'] != 'fleet.approval.v1': sys.exit(73)
mode=pathlib.Path({mode:?}).read_text()
if mode=='down': sys.exit(73)
if mode=='old':
 print(json.dumps(dict(id=r['id'],ok=False,error='unknown gateway command fleet.approval.v1'))); sys.exit()
if mode=='scope': r['args']['context']['scopes']=[]
if mode=='hidden': r['args']['context']['ships']=[]
if mode=='extra': r['args']['command']='captain.reply'
env=dict(os.environ, PRIVATEER_RESOURCE_HOME={ledger:?})
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True,env=env)
if p.returncode: sys.exit(p.returncode)
v=json.loads(p.stdout)
if mode=='malformed' and v['ok']: v['result']['prepared']['grant']['payload']='secret-canary'
if mode in ('revoke','pause','rotate','restrict','link','directory'):
 with sqlite3.connect({registry:?}) as db:
  if mode=='revoke': db.execute('UPDATE door_clients SET revoked_ms=1 WHERE id=?',({grant:?},))
  if mode=='pause': db.execute('UPDATE door_clients SET paused=1 WHERE id=?',({grant:?},))
  if mode=='rotate': db.execute('UPDATE door_clients SET token_hash=zeroblob(32) WHERE id=?',({grant:?},))
  if mode=='restrict': db.execute('UPDATE door_clients SET ships=? WHERE id=?',('["alpha"]',{grant:?}))
  if mode=='link': db.execute("UPDATE machines SET incarnation='gamma' WHERE name='beta'")
  if mode=='directory': db.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','beta','Build')")
print(json.dumps(v))
"#, calls=self.calls.to_str().unwrap(), mode=self.mode.to_str().unwrap(),
            ledger=self.remote.join("ledger").to_str().unwrap(), registry=self.fleet.root.join("state/fleet.sqlite").to_str().unwrap(), grant=grant)).unwrap();
    }
    fn count(&self) -> usize { fs::read_to_string(&self.calls).unwrap_or_default().lines().count() }
    fn prepare(&self) -> Value { json!({"mode":"prepare","ship":"beta","id":"ask"}) }
    fn assert_open(&self) {
        assert_eq!(self.remote_call("mail.show", json!({"id":"ask"}))["state"], "open");
        assert!(self.remote_call("captain_approval.list", json!({"ship":"beta"}))["approvals"].as_array().unwrap().is_empty());
    }
}

#[test]
fn chat_linked_prepare_is_origin_only_durable_and_never_answers() {
    let linked = LinkedChat::new();
    let fleet = &linked.fleet;
    delegated_adapter(fleet);
    let (grant, token) = delegated_issue(fleet, "beta", "approval:answer", Some("beta"));
    linked.wrapper(grant["clients"][0]["id"].as_str().unwrap());
    // A same-ID desk copy must never become an origin, nor shadow the selected
    // beta ask. Its sensitive payload is deliberately not used for discovery.
    let desk = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    desk.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state,delivery)
        VALUES('ask','beta','captain','request','beta-duty','desk-canary','{}','normal',100,'open','received')", []).unwrap();
    let supervisor = fleet.supervise();
    fleet.wait("linked chat hello", |e| e.first().cloned());
    let before = linked.count();
    chat_refused(fleet, &token, json!({"mode":"prepare","id":"ask"}), "approval_unavailable");
    assert_eq!(linked.count(), before, "no omitted-origin fanout");
    let prepared = delegated_ok(fleet, &token, "approval", linked.prepare());
    assert_eq!(prepared["ship"], "beta");
    assert_eq!(prepared["origin_machine"], "beta");
    assert_eq!(prepared["answer_available"], false);
    assert_eq!(prepared["origin_home"], linked.remote.join("beta/.privateer").to_str().unwrap());
    assert_eq!(prepared["grant"]["command_prefix"], "make release");
    for secret in ["body-canary", "desk-canary", &token] { assert!(!prepared.to_string().contains(secret)); }
    let peer = linked.remote_store();
    let content: String = peer.query_row("SELECT content FROM door_approval_challenges WHERE digest=?1", [prepared["digest"].as_str()], |r| r.get(0)).unwrap();
    let content: Value = serde_json::from_str(&content).unwrap();
    let host = linked.registry();
    let (binding, incarnation): (String, String) = host.query_row("SELECT binding,incarnation FROM door_linked_approval_routes WHERE digest=?1",
        [prepared["digest"].as_str()], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(content["link"], serde_json::from_str::<Value>(&binding).unwrap());
    assert_eq!(content["link"]["link"], incarnation);
    let before = linked.count();
    let mut answer = chat_answer(&prepared, "approve");
    for omit_ship in [false, true] {
        if omit_ship { answer.as_object_mut().unwrap().remove("ship"); }
        chat_refused(fleet, &token, answer.clone(), "approval_unavailable");
    }
    assert_eq!(linked.count(), before, "linked answers never contact gateway");
    linked.assert_open();
    // Preparation replaces the old challenge and pin; neither restarts nor
    // a same-endpoint link replacement may make the original challenge local.
    let next = delegated_ok(fleet, &token, "approval", linked.prepare());
    assert_ne!(next["digest"], prepared["digest"]);
    assert!(!host.query_row("SELECT EXISTS(SELECT 1 FROM door_linked_approval_routes WHERE digest=?1)", [prepared["digest"].as_str()], |r| r.get::<_, bool>(0)).unwrap());
    linked.remote_call("mail.send", json!({"id":"decision","from":"beta","to":"captain","kind":"decision",
        "task_id":"beta-duty","body":"Choose build","payload":{"approves":{"action":"Submit build 55"},
        "decision":{"question":"Submit now?","options":[
            {"id":"submit","label":"Submit","what_happens":"Submit build 55","approves":true},
            {"id":"hold","label":"Hold","what_happens":"Wait","approves":false}]}}}));
    let decision = delegated_ok(fleet, &token, "approval", json!({"mode":"prepare","id":"decision","ship":"beta"}));
    assert_eq!(decision["question"], "Submit now?");
    assert_eq!(decision["options"][0]["what_happens"], "Submit build 55");
    assert_eq!(decision["options"][1]["approves"], false);
    assert_eq!(decision["answer_available"], false);
    let before_restart = linked.count();
    stop(supervisor);
    fs::write(fleet.root.join("requests.jsonl"), "").unwrap(); fs::write(&fleet.log, "").unwrap();
    let supervisor = fleet.supervise(); fleet.wait("linked chat restart", |e| e.first().cloned());
    chat_refused(fleet, &token, chat_answer(&next, "deny"), "approval_unavailable");
    assert_eq!(linked.count(), before_restart, "restart does not dispatch an answer");
    let calls = fs::read_to_string(&linked.calls).unwrap();
    assert!(!calls.contains(&token));
    let mut gateway_request: Value = serde_json::from_str(calls.lines().next().unwrap()).unwrap();
    assert_eq!(linked.gateway(&gateway_request, true)["ok"], false, "agent cannot enter operator gateway");
    gateway_request["args"]["args"] = chat_answer(&next, "approve");
    let denied = linked.gateway(&gateway_request, false);
    assert_eq!(denied["denial_code"], "approval_unavailable", "{denied}");
    // The selected ship moved off this gateway: it must refuse locally, not
    // recurse through a directory entry or some other local ask with this ID.
    linked.remote_store().execute("UPDATE mail SET sender='gamma' WHERE id='ask'", []).unwrap();
    gateway_request["args"]["args"] = linked.prepare();
    assert_eq!(linked.gateway(&gateway_request, false)["denial_code"], "approval_unavailable");
    linked.remote_store().execute("UPDATE mail SET sender='beta' WHERE id='ask'", []).unwrap();
    assert!(calls.lines().all(|s| serde_json::from_str::<Value>(s).unwrap()["command"] == "fleet.approval.v1"));
    linked.assert_open(); stop(supervisor);
}

#[test]
fn chat_linked_prepare_refuses_hidden_ambiguous_reverse_sensitive_copies_and_bad_peers() {
    let linked = LinkedChat::new(); let fleet = &linked.fleet;
    delegated_adapter(fleet);
    let (grant, token) = delegated_issue(fleet, "beta", "approval:answer", Some("beta"));
    let (_, hidden) = delegated_issue(fleet, "alpha", "approval:answer", Some("alpha"));
    linked.wrapper(grant["clients"][0]["id"].as_str().unwrap());
    let supervisor = fleet.supervise(); fleet.wait("linked refusals hello", |e| e.first().cloned());
    let before = linked.count();
    chat_refused(fleet, &hidden, linked.prepare(), "ship_not_allowed");
    let registry = linked.registry();
    registry.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('gamma','beta','Build')", []).unwrap();
    chat_refused(fleet, &token, linked.prepare(), "approval_unavailable");
    registry.execute("DELETE FROM machine_ships WHERE machine='gamma'", []).unwrap();
    registry.execute("UPDATE machines SET role='collected' WHERE name='beta'", []).unwrap();
    chat_refused(fleet, &token, linked.prepare(), "approval_unavailable");
    registry.execute("UPDATE machines SET role='collects' WHERE name='beta'", []).unwrap();
    assert_eq!(linked.count(), before, "no forbidden route contacted");
    for (mode, reason) in [("old","approval_unavailable"),("down","approval_unavailable"),
        ("scope","insufficient_scope"),("hidden","ship_not_allowed"),("extra","approval_unavailable"),("malformed","approval_unavailable")] {
        fs::write(&linked.mode, mode).unwrap(); chat_refused(fleet, &token, linked.prepare(), reason);
    }
    fs::write(&linked.mode, "live").unwrap();
    let peer = linked.remote_store();
    peer.execute("UPDATE mail SET delivery='received' WHERE id='ask'", []).unwrap();
    chat_refused(fleet, &token, linked.prepare(), "approval_unavailable");
    peer.execute("UPDATE mail SET delivery=NULL,request_kind='production_approval' WHERE id='ask'", []).unwrap();
    chat_refused(fleet, &token, linked.prepare(), "trusted_confirmation_required");
    peer.execute("UPDATE mail SET request_kind=NULL,payload=json_set(payload,'$.future_kind',1) WHERE id='ask'", []).unwrap();
    chat_refused(fleet, &token, linked.prepare(), "approval_unavailable");
    linked.assert_open(); stop(supervisor);
}

#[test]
fn chat_linked_prepare_rechecks_host_authority_and_route_after_real_contact() {
    for mode in ["revoke", "pause", "rotate", "restrict", "link", "directory"] {
        let linked = LinkedChat::new(); let fleet = &linked.fleet;
        delegated_adapter(fleet);
        let (grant, token) = delegated_issue(fleet, "beta", "approval:answer", Some("beta"));
        linked.wrapper(grant["clients"][0]["id"].as_str().unwrap());
        fs::write(&linked.mode, mode).unwrap();
        let supervisor = fleet.supervise(); fleet.wait("linked release hello", |e| e.first().cloned());
        let response = delegated_exchange(fleet, &token, "approval", linked.prepare());
        assert_eq!(response["ok"], false, "{mode}: {response}");
        assert!(response.get("result").is_none(), "{mode}: {response}");
        assert!(!response.to_string().contains("Submit build 55"), "{mode}: {response}");
        let before = linked.count();
        if matches!(mode, "revoke" | "pause" | "rotate" | "restrict") {
            assert_eq!(delegated_exchange(fleet, &token, "approval", linked.prepare())["ok"], false);
            assert_eq!(linked.count(), before, "{mode}: no revoked/narrowed recontact");
        }
        let registry = linked.registry();
        assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM door_linked_approval_routes)", [], |r| r.get::<_, bool>(0)).unwrap());
        // The response really came from an origin commit, not a fake refusal.
        assert!(linked.remote_store().query_row("SELECT EXISTS(SELECT 1 FROM door_approval_challenges)", [], |r| r.get::<_, bool>(0)).unwrap());
        linked.assert_open(); stop(supervisor);
    }
}
