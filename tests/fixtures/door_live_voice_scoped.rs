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
    assert_eq!(stored["body"], "Um, please inspect the hermetic beta fixture, exactly.");
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
