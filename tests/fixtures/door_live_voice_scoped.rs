// The bundled adapter against a real supervisor, not a fake runtime. Grants
// use the existing controlling-terminal helper, with no issuance bypass.
#[test]
fn scoped_voice_pairs_distinct_core_grants_and_refuses_wrong_door_after_restart() {
    use std::os::unix::process::CommandExt;
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/live-voice");
    let manifest = fleet.root.join("adapter/manifest.json");
    fs::write(
        &manifest,
        json!({"schema_version":1,"kind":"front-door-scoped","door_protocol":2,
            "id":"fake","config_required":true,
            "executable":[support::python(),plugin.join("door.py")]})
        .to_string(),
    )
    .unwrap();
    fleet.add();
    fleet.ok(&[
        "door",
        "add",
        "other",
        "--manifest",
        manifest.to_str().unwrap(),
        "--config",
        fleet.root.join("door.toml").to_str().unwrap(),
    ]);
    let mut devices = vec![];
    let mut ids = vec![];
    let mut secrets = vec![];
    for (label, scopes, door) in [
        ("alpha", "fleet:read", "fake"),
        ("beta", "ship:message", "fake"),
        ("gamma", "fleet:read", "other"),
    ] {
        let (metadata, token) = client_secret(
            &fleet,
            &[
                "door", "client", "issue", door, "--name", label, "--scopes", scopes, "--ships",
                if label == "gamma" { "alpha" } else { label },
            ],
            "issue",
        );
        let grant = metadata["clients"][0]["id"].as_str().unwrap().to_owned();
        let device = uuid::Uuid::new_v4().to_string();
        let directory = fleet.root.join(label);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let key = directory.join("core-key");
        fs::write(&key, &token).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        devices.push(json!({"id":device,"label":label,"grant":grant,"credential_file":key}));
        ids.push(grant);
        secrets.push(token);
    }
    // gamma's grant is restricted to alpha (an existing ship), but bound to
    // the wrong door. Do not create a hidden or nonexistent ship to issue it.
    let config = fleet.root.join("door.toml");
    let mut text = "access_mode = \"scoped\"\nport = 8766\n".to_owned();
    // Bind an ephemeral port for this isolated fixture, not an installation.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    text = text.replace("8766", &port.to_string());
    for row in &devices {
        text.push_str("\n[[devices]]\n");
        for key in ["id", "label", "grant", "credential_file"] {
            text.push_str(&format!("{key} = {}\n", row[key]));
        }
    }
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_live_voice_app_server.py");
    let control = fleet.root.join("voice-control");
    fs::create_dir_all(control.join("inject")).unwrap();
    fs::write(control.join("scenario.json"), "{}").unwrap();
    text = format!(
        "codex = [{}, {}]\n{}",
        json!(support::python()), json!(fake), text
    );
    fs::write(&config, text).unwrap();
    let state = fleet.root.join("state/doors/fake/state");
    let driver = fleet.root.join("voice-client.py");
    fs::write(
        &driver,
        r#"import http.client, json, os, pathlib, sys, time
root, state, mode = map(str, sys.argv[1:])
config = json.loads(pathlib.Path(root, 'devices.json').read_text())
port = int(pathlib.Path(state, 'url').read_text().strip().rsplit(':',1)[1].rstrip('/'))
def call(path, body=None, token=''):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=30)
    headers = {'X-Privateer-Token':token, 'Content-Type':'application/json'}
    conn.request('POST' if body is not None else 'GET', path,
                 None if body is None else json.dumps(body), headers)
    response = conn.getresponse(); data = json.loads(response.read()); conn.close()
    return response.status, data
def pair(row):
    code = pathlib.Path(state, 'scoped-devices', row['id'], 'pairing-code').read_text().strip()
    return call('/api/pair', {'code':code})
if mode == 'pair':
    assert call('/api/version')[1]['protocol'] == 2
    tokens = []
    for row in config[:2]:
        status, data = pair(row)
        assert status == 200, (status, data)
        assert data['device_id'] == row['id']
        tokens.append(data['token'])
    assert pair(config[2])[0] == 403
    for row, token in zip(config, tokens):
        status, data = call('/api/state', token=token)
        assert status == 200 and data['device_id'] == row['id']
        assert data['session_id'] is None and data['queued'] == 0 and data['mail_mode'] == 'disabled'
    path = pathlib.Path(root, 'page-tokens.json')
    path.write_text(json.dumps(tokens)); path.chmod(0o600)
    # Real HTTP -> fake app-server pipes -> scoped v2 -> real keyed core send.
    offer = ('v=0\r\no=- 7 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n'
             'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\n'
             'm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n')
    # Read runtime metadata through the real supervisor with alpha's fleet grant.
    # The fixture has no running dispatcher, so negative activity stays unknown.
    status, started = call('/api/session/start', {'offer':offer}, tokens[0])
    assert status == 200, (status, started)
    runtime_sid = started['session_id']
    assert call('/api/session/connected', {'session_id':runtime_sid}, tokens[0])[0] == 200
    for index, args in enumerate(({}, {'ship':'alpha'}, {'ship':'beta'})):
        turn = 'runtime-' + str(index)
        messages = [
            {'id':turn,'method':'item/tool/call','params':{'tool':'privateer_status',
                'turnId':turn,'arguments':args}},
            {'method':'turn/completed','params':{'turn':{'id':turn}}}]
        pathlib.Path(root, 'voice-control', 'inject', 'runtime.json').write_text(json.dumps(messages))
        deadline = time.monotonic() + 15
        while True:
            view = call('/api/state', token=tokens[0])[1]
            lines = [row['text'] for row in view['transcript'] if row['role'] == 'door']
            if len(lines) > index: break
            assert time.monotonic() < deadline, view
            time.sleep(.02)
        spoken = lines[-1]
        assert 'Mail disabled; not checked.' in spoken, spoken
        if args.get('ship') == 'beta':
            assert spoken == 'Runtime unavailable; could not check. Mail disabled; not checked.', spoken
        else:
            assert 'Runtime coverage: domain unknown; incomplete.' in spoken, spoken
            assert 'local/alpha: working could not check' in spoken, spoken
            assert 'Recorded configuration (not liveness)' in spoken, spoken
            assert 'Backlog and recorded running jobs do not establish workers' in spoken, spoken
            assert spoken.index('observed at') < spoken.index('active workers'), spoken
            assert 'beta' not in spoken, spoken
        for secret in ('core-key', 'credential_file', 'opaque-reference-canary'):
            assert secret not in spoken, spoken
    assert call('/api/session/stop', {'session_id':runtime_sid}, tokens[0])[0] == 200
    status, started = call('/api/session/start', {'offer':offer}, tokens[1])
    assert status == 200, (status, started)
    sid = started['session_id']
    assert call('/api/session/connected', {'session_id':sid}, tokens[1])[0] == 200
    body = 'Um, please inspect the hermetic beta fixture, exactly.'
    messages = [
        {'method':'thread/realtime/transcript/done','params':{'role':'user','text':body}},
        {'id':'draft-1','method':'item/tool/call','params':{'tool':'privateer_send',
            'turnId':'turn-1','arguments':{'to':'beta'}}}]
    inject = pathlib.Path(root, 'voice-control', 'inject', '001.json')
    inject.write_text(json.dumps(messages))
    deadline = time.monotonic() + 15
    while True:
        view = call('/api/state', token=tokens[1])[1]
        if view['draft']: break
        assert time.monotonic() < deadline, view
        time.sleep(.02)
    draft = view['draft']
    assert body in draft['readback'] and 'beta' in draft['readback']
    confirmation = {'session_id':sid,'id':draft['id'],'to':'beta'}
    assert call('/api/draft/confirm', confirmation, tokens[0])[0] == 409
    status, result = call('/api/draft/confirm', confirmation, tokens[1])
    assert status == 200 and result['state'] == 'accepted', (status, result)
    assert result['delivery_state'] == 'delivered', result
    assert call('/api/draft/confirm', confirmation, tokens[1])[0] == 409
    assert call('/api/session/stop', {'session_id':sid}, tokens[1])[0] == 200
    assert body not in pathlib.Path(state, 'scoped-sends.json').read_text()
    pathlib.Path(root, 'sent-id').write_text(result['mail_id'])
elif mode == 'restart':
    tokens = json.loads(pathlib.Path(root, 'page-tokens.json').read_text())
    status, view = call('/api/state', token=tokens[1])
    assert status == 200 and view['draft'] is None
    assert 'Outcome unknown' in view['outcome'], view
    assert len(view['send_history']) == 1
    assert pair(config[0])[0] == 403
    assert pair(config[1])[0] == 200
    assert pair(config[2])[0] == 403
print('scoped supervisor checks passed')
"#,
    )
    .unwrap();
    fs::write(
        fleet.root.join("devices.json"),
        serde_json::to_string(&devices).unwrap(),
    )
    .unwrap();
    let start = || {
        fleet
            .command(&fleet.root)
            .args(["door", "run", "fake"])
            .env("FAKE_VOICE_DIR", &control)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let check = |mode: &str| {
        fleet.wait("scoped voice URL", |_| state.join("url").exists().then_some(()));
        let output = support::command(support::python())
            .arg(&driver)
            .arg(&fleet.root)
            .arg(&state)
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let mut supervisor = start();
    check("pair");
    let mail_id = fs::read_to_string(fleet.root.join("sent-id")).unwrap();
    let stored = fleet.call("beta", "mail.show", json!({"id":mail_id}));
    assert_eq!(stored["id"], mail_id);
    assert_eq!(
        stored["body"],
        "Um, please inspect the hermetic beta fixture, exactly."
    );
    client_call(&fleet, "revoke", &ids[0]);
    unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) };
    supervisor.wait().unwrap();
    fs::remove_file(state.join("url")).unwrap();
    let mut supervisor = start();
    check("restart");
    unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) };
    supervisor.wait().unwrap();
    let log = fs::read_to_string(fleet.root.join("state/doors/fake/door.log")).unwrap();
    for token in secrets {
        assert!(!log.contains(&token));
    }
}

// PR6b: actual voice adapter -> scoped supervisor -> independent linked gateway.
#[test]
fn scoped_voice_recorded_detail_crosses_linked_gateways_without_text_or_actions() {
    let fleet = Fleet::with_ships(&[("alpha", true)]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(&ssh, "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let root = fleet.root.join("beta");
    let folder = root.join("gamma");
    fs::create_dir_all(&folder).unwrap();
    let peer_call = |args: &[&str]| {
        let out = fleet.command(&folder).env("HOME", &root)
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger")).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    peer_call(&["init", "--ship", "gamma", "--harness", "codex"]);
    let home = folder.join(".privateer");
    fs::write(home.join("config.json"), json!({"resource_home":root.join("ledger")}).to_string()).unwrap();
    peer_call(&["resource.status", "--json", "{}"]);
    let peer = rusqlite::Connection::open(home.join("privateer.sqlite")).unwrap();
    peer.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('gamma-task','gamma','gamma','title-canary','open',0)", []).unwrap();
    for (id, state) in [("11111111-1111-4111-8111-111111111111", "resumed"), ("22222222-2222-4222-8222-222222222222", "queued"), ("33333333-3333-4333-8333-333333333333", "parked")] {
        peer.execute("INSERT INTO jobs(id,record) VALUES(?1,?2)", rusqlite::params![id, json!({"id":id,"incarnation":"alpha-incarnation","task_id":"gamma-task","status":state,"created_ms":100,"finished_ms":if state=="resumed" {Some(200)} else {None},"action":"command","command":["argv-canary"],"cwd":"/path-canary","resume_of":if state=="queued" {Some("11111111-1111-4111-8111-111111111111")} else {None}}).to_string()]).unwrap();
    }
    peer.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES('alpha-progress','gamma','gamma','report','gamma-task','progress-canary',?1,'normal',100,'open')", [json!({"report":"progress","job_id":"11111111-1111-4111-8111-111111111111","incarnation":"alpha-incarnation"}).to_string()]).unwrap();
    peer.execute("INSERT INTO mail(id,sender,recipient,kind,task_id,body,payload,urgency,created_ms,state) VALUES('gamma-ask','gamma','captain','request','gamma-task','question-canary','{}','high',100,'open')", []).unwrap();
    fleet.ok(&["machine", "add", "beta", "fixture", "--executable", BIN, "--remote-fleet", root.join("fleet.sqlite").to_str().unwrap(), "--as", "alpha"]);
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/live-voice");
    let manifest = fleet.root.join("adapter/manifest.json");
    fs::write(&manifest, json!({"schema_version":1,"kind":"front-door-scoped","door_protocol":2,"id":"fake","config_required":true,"executable":[support::python(),plugin.join("door.py")]}).to_string()).unwrap();
    fleet.add();
    let (metadata, secret) = client_secret(&fleet, &["door", "client", "issue", "fake", "--name", "alpha", "--scopes", "fleet:read,mail:read", "--ships", "gamma"], "issue");
    let grant = metadata["clients"][0]["id"].as_str().unwrap();
    let device = uuid::Uuid::new_v4().to_string();
    let private = fleet.root.join("device-key");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let key = private.join("core-key");
    fs::write(&key, &secret).unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_live_voice_app_server.py");
    fs::write(fleet.root.join("door.toml"), format!("access_mode = \"scoped\"\nport = {port}\ncodex = [{}, {}]\n[[devices]]\nid = {}\nlabel = \"alpha\"\ngrant = {}\ncredential_file = {}\n", json!(support::python()), json!(fake), json!(device), json!(grant), json!(key))).unwrap();
    let control = fleet.root.join("voice-control");
    fs::create_dir_all(control.join("inject")).unwrap();
    fs::write(control.join("scenario.json"), "{}").unwrap();
    let calls = fleet.root.join("gateway-calls");
    // Record only the fixed operation/args, never grant credentials.
    fs::write(&ssh, format!(r#"#!/usr/bin/env python3
import json, subprocess, sys
r=json.loads(sys.stdin.readline())
assert r['command']=='fleet.inspection.v1', r['command']
with open({calls:?},'a') as f: f.write(json.dumps({{'operation':r['args']['operation'],'args':r['args']['args']}})+'\n')
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
sys.stdout.write(p.stdout); sys.exit(p.returncode)
"#, calls=calls.to_str().unwrap())).unwrap();
    let state = fleet.root.join("state/doors/fake/state");
    let driver = fleet.root.join("detail-client.py");
    fs::write(&driver, r#"import http.client, json, pathlib, sqlite3, sys, time
root, state, device, home = map(pathlib.Path, sys.argv[1:])
port = int((state/'url').read_text().strip().rsplit(':',1)[1].rstrip('/'))
token = ''
def call(path, body=None):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=30)
    conn.request('POST' if body is not None else 'GET',path,None if body is None else json.dumps(body),
                 {'X-Privateer-Token':token,'Content-Type':'application/json'})
    r=conn.getresponse(); data=json.loads(r.read()); conn.close()
    assert r.status==200,(r.status,data)
    return data
code=(state/'scoped-devices'/device/'pairing-code').read_text().strip()
token=call('/api/pair',{'code':code})['token']
offer=('v=0\r\no=- 7 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n'
       'm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\n'
       'm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n')
sid=call('/api/session/start',{'offer':offer})['session_id']
call('/api/session/connected',{'session_id':sid})
count=0
speeches=[]
origins=set()
before_home=(home/'privateer.sqlite').read_bytes()
before_ledger=(root/'beta'/'ledger'/'resources.sqlite').read_bytes()
def tool(name,args,ok=True):
    global count
    count+=1; turn='detail-'+str(count)
    messages=[{'id':turn,'method':'item/tool/call','params':{'tool':'privateer_'+name,'turnId':turn,'arguments':args}},
              {'method':'turn/completed','params':{'turn':{'id':turn}}}]
    (root/'voice-control'/'inject'/'detail.json').write_text(json.dumps(messages))
    deadline=time.monotonic()+20
    while True:
        view=call('/api/state')
        lines=[row['text'] for row in view['transcript'] if row['role']=='door']
        records=[json.loads(line) for line in (root/'voice-control'/'received.jsonl').read_text().splitlines()]
        replies=[r['result'] for r in records if r.get('id')==turn and 'result' in r]
        if len(lines)>=count and replies: break
        assert time.monotonic()<deadline,(view,replies)
        time.sleep(.02)
    result=replies[-1]; speech=lines[-1]
    assert result['success']==ok,(result,speech)
    if name=='resource_recover':
        assert 'Unsupported in scoped voice; nothing was sent or approved.' in speech,speech
        return speech,{}
    assert 'Mail disabled; not checked.' in speech,speech
    assert 'Progress or question text is withheld' in speech,speech
    assert len(speech)<=7800
    for canary in ('progress-canary','question-canary','title-canary','argv-canary','path-canary'):
        assert canary not in json.dumps(result)+speech,(canary,result)
    metadata=json.loads(result['contentItems'][1]['text'].split(': ',1)[1]) if ok else {}
    speeches.append(speech)
    origins.update(r['origin'] for r in metadata.get('references',[])+metadata.get('work_references',[]))
    return speech,metadata
speech,page=tool('ship_jobs',{'ship':'gamma','limit':1})
assert 'Retained terminal history only; prior pruned history unknown' in speech,speech
assert 'state resumed' in speech and 'Resumed is lineage, not success' in speech,speech
assert 'state since' in speech and 'basis state transition' in speech,speech
ref=page['references'][0]
assert ref['id']=='11111111-1111-4111-8111-111111111111' and ref['ship']=='gamma',ref
assert ref['origin'] not in speech and ref['incarnation'] not in speech
speech,exact=tool('job_inspect',{'ship':'gamma','reference':ref})
assert exact['references'][0]==ref,(exact,ref)
speech,second=tool('ship_jobs',{'ship':'gamma','limit':1,'continuation':page['continuation']})
assert second['references'][0]['id']=='22222222-2222-4222-8222-222222222222',second
assert 'Wait capacity' in speech or 'Wait disk' in speech,speech
assert 'time unknown, age unknown, basis state transition' in speech,speech
speech,_=tool('ship_jobs',{'ship':'gamma','limit':1,'continuation':page['continuation'],'state':'completed'},False)
assert 'Recorded detail unavailable; could not check' in speech,speech
speech,_=tool('ship_jobs',{'ship':'gamma','state':'parked'})
assert 'Wait job' in speech,speech
speech,_=tool('ship_work',{'ship':'gamma'})
assert 'Recorded task, task state' in speech and 'not workers' in speech,speech
speech,_=tool('machine_capacity',{'machine':'beta','ship':'gamma'})
assert 'Shared host policy, headroom, readings, totals and routes withheld' in speech,speech
assert 'could not check shared capacity' in speech,speech
speech,_=tool('runtime_needs',{'ship':'gamma'})
assert 'Needs partial; aggregate mail coverage could not check' in speech,speech
assert 'Recorded need' in speech,speech
assert origins, 'exact origin refs were not exercised'
for spoken in speeches:
    for identifier in origins | {'11111111-1111-4111-8111-111111111111', '22222222-2222-4222-8222-222222222222',
                                  '33333333-3333-4333-8333-333333333333', 'alpha-progress', 'gamma-ask',
                                  'alpha-incarnation', 'gamma-task'}:
        assert identifier not in spoken,(identifier,spoken)
assert (home/'privateer.sqlite').read_bytes()==before_home, 'read mutated home'
assert (root/'beta'/'ledger'/'resources.sqlite').read_bytes()==before_ledger, 'read mutated ledger'
# Exact incarnation changes are a refusal, never a raw-ID fallback.
c=sqlite3.connect(home/'privateer.sqlite')
c.execute("UPDATE jobs SET record=json_set(record,'$.incarnation','beta-incarnation') WHERE id='11111111-1111-4111-8111-111111111111'"); c.commit(); c.close()
tool('job_inspect',{'ship':'gamma','reference':ref},False)
tool('ship_jobs',{'ship':'gamma','include_text':True},False)
tool('resource_recover',{'machine':'beta','ship':'gamma'},False)
call('/api/session/stop',{'session_id':sid})
print('linked voice detail checks passed')
"#).unwrap();
    let mut supervisor = fleet.command(&fleet.root).args(["door", "run", "fake"])
        .env("FAKE_VOICE_DIR", &control).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    fleet.wait("detail voice URL", |_| state.join("url").exists().then_some(()));
    let output = support::command(support::python()).arg(&driver).arg(&fleet.root).arg(&state)
        .arg(&device).arg(&home).output().unwrap();
    unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) };
    supervisor.wait().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let calls = fs::read_to_string(calls).unwrap();
    let rows: Vec<Value> = calls.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    for row in &rows {
        assert!(["ship_jobs", "job_inspect", "ship_work", "machine_capacity", "runtime_needs"].contains(&row["operation"].as_str().unwrap()));
        if row["operation"] != "machine_capacity" { assert_eq!(row["args"]["include_text"], false); }
    }
    for operation in ["ship_jobs", "job_inspect", "ship_work", "machine_capacity", "runtime_needs"] {
        assert!(rows.iter().any(|r| r["operation"]==operation), "{operation}: {calls}");
    }
}
