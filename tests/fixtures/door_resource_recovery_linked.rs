#[test]
fn resource_recovery_linked_negotiation_loss_restart_and_release_fences() {
    let fleet = Fleet::with_ships(&[("survey", true)]);
    let ssh = fleet.root.join("fake-ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor argument do remote=$argument; done\nexec /bin/sh -c \"$remote\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let root = fleet.root.join("beta");
    let folder = root.join("relay");
    fs::create_dir_all(&folder).unwrap();
    let remote = fleet
        .command(&folder)
        .env("HOME", &root)
        .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"))
        .args(["init", "--ship", "relay", "--harness", "codex"])
        .output()
        .unwrap();
    assert!(
        remote.status.success(),
        "{}",
        String::from_utf8_lossy(&remote.stderr)
    );
    let home = fs::canonicalize(folder.join(".privateer")).unwrap();
    fleet.ok(&[
        "machine",
        "add",
        "beta",
        "fixture",
        "--executable",
        BIN,
        "--remote-fleet",
        root.join("fleet.sqlite").to_str().unwrap(),
        "--as",
        "alpha",
    ]);
    delegated_adapter(&fleet);
    let (grant, token) = delegated_issue(&fleet, "alpha", "fleet:read,fleet:admin", None);
    let grant = grant["clients"][0]["id"].as_str().unwrap();
    let mode = fleet.root.join("recovery-mode");
    let calls = fleet.root.join("recovery-calls");
    fs::write(&mode, "live").unwrap();
    fs::write(&ssh,format!(r#"#!/usr/bin/env python3
import json,pathlib,subprocess,sys,sqlite3
r=json.loads(sys.stdin.readline())
with open({calls:?},'a') as f:f.write(json.dumps(r)+'\n')
mode=pathlib.Path({mode:?}).read_text()
if r['command']!='fleet.resource-action.v1':sys.exit(73)
op=r['args']['operation']
if op=='capabilities' and mode in ('legacy','configure-only'):
 reply={{'id':r['id'],'ok':False,'error':'unsupported_arguments'}} if mode=='legacy' else {{'id':r['id'],'ok':True,'result':{{'schema':'resource.action.capabilities.v1','operations':['resource_configure']}}}}
 print(json.dumps(reply));sys.exit()
if op=='resource_recover' and mode=='scope':r['args']['context']['scopes']=[]
if op=='resource_recover' and mode=='named':r['args']['context']['ships']=['relay']
p=subprocess.run(['/bin/sh','-c',sys.argv[-1]],input=json.dumps(r)+'\n',text=True,capture_output=True)
if p.returncode:sys.stderr.write(p.stderr);sys.exit(p.returncode)
if op=='resource_recover':
 if mode=='loss':sys.exit()
 if mode=='malformed':print(json.dumps({{'id':r['id'],'ok':True,'result':{{'path':'secret-canary'}}}}));sys.exit()
 if mode in ('replace','release'):
  c=sqlite3.connect({registry:?})
  if mode=='replace':c.execute("UPDATE machines SET incarnation='changed' WHERE name='beta'")
  else:c.execute('UPDATE door_clients SET revoked_ms=0 WHERE id=?',({grant:?},))
  c.commit()
print(p.stdout,end='')
"#,calls=calls.to_str().unwrap(),mode=mode.to_str().unwrap(),registry=fleet.root.join("state/fleet.sqlite").to_str().unwrap(),grant=grant)).unwrap();
    let mut supervisor = fleet.supervise();
    fleet.wait("linked recovery hello", |e| e.first().cloned());
    let hold = recovery_hold(&fleet, &home, "relay", "alpha", &root.join("ledger"));
    let selector = json!({"machine":"beta","ship":"relay","job_id":"alpha"});
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    registry
        .execute("UPDATE machines SET role='collected' WHERE name='beta'", [])
        .unwrap();
    credential_request(
        &fleet,
        &token,
        "resource_recovery_reference",
        selector.clone(),
        "linked_provenance_unavailable",
    );
    credential_request(
        &fleet,
        &token,
        "resource_recover",
        json!({"machine":"beta","ship":"relay","reference":hold.reference,"approval_id":uuid::Uuid::new_v4().to_string(),"idempotency_key":"reverse"}),
        "linked_provenance_unavailable",
    );
    registry
        .execute("UPDATE machines SET role='collects' WHERE name='beta'", [])
        .unwrap();
    assert!(
        !calls.exists(),
        "reverse-only peer must never be called or spooled"
    );
    for setting in ["legacy", "configure-only"] {
        fs::write(&mode, setting).unwrap();
        credential_request(
            &fleet,
            &token,
            "resource_recovery_reference",
            selector.clone(),
            "unsupported",
        );
        let args = json!({"machine":"beta","ship":"relay","reference":hold.reference,"approval_id":uuid::Uuid::new_v4().to_string(),"idempotency_key":setting});
        credential_request(&fleet, &token, "resource_recover", args, "unsupported");
    }
    let logged = fs::read_to_string(&calls).unwrap();
    assert!(
        logged
            .lines()
            .map(|s| serde_json::from_str::<Value>(s).unwrap())
            .all(|v| v["args"]["operation"] == "capabilities")
    );
    fs::write(&mode, "live").unwrap();
    let metadata = delegated_ok(&fleet, &token, "resource_recovery_reference", selector);
    assert!(
        metadata["authority"]["link_incarnation"]
            .as_str()
            .is_some_and(|s| s.len() == 64)
    );
    let approval = recovery_permit(&fleet, &hold, &metadata, "alpha");
    // A local operator apply cannot impersonate the authenticated linked seam.
    let native = fleet.command(&fleet.root).args(["--home",home.to_str().unwrap(),"resource.recovery.apply","--json",&json!({"request":{"reference":metadata["reference"],"authority":metadata["authority"],"idempotency_key":"alpha"},"approval_id":approval}).to_string()]).output().unwrap();
    assert!(!native.status.success());
    assert!(String::from_utf8_lossy(&native.stderr).contains("unsupported"));
    let a = recovery_args(&metadata, &approval, "alpha");
    for (setting, reason) in [
        ("scope", "insufficient_scope"),
        ("named", "ship_not_allowed"),
    ] {
        fs::write(&mode, setting).unwrap();
        credential_request(&fleet, &token, "resource_recover", a.clone(), reason);
        assert!(recovery_is_held(&hold));
    }
    fs::write(&mode, "loss").unwrap();
    credential_request(
        &fleet,
        &token,
        "resource_recover",
        a.clone(),
        "outcome_unknown",
    );
    assert!(!recovery_is_held(&hold));
    stop(supervisor);
    supervisor = fleet.supervise();
    fs::write(&mode, "live").unwrap();
    let first = delegated_ok(&fleet, &token, "resource_recover", a.clone());
    assert_eq!(delegated_ok(&fleet, &token, "resource_recover", a), first);
    for setting in ["malformed", "replace", "release"] {
        fs::write(&mode, "live").unwrap();
        let hold = recovery_hold(&fleet, &home, "relay", setting, &root.join("ledger"));
        let metadata = delegated_ok(
            &fleet,
            &token,
            "resource_recovery_reference",
            json!({"machine":"beta","ship":"relay","job_id":setting}),
        );
        let approval = recovery_permit(&fleet, &hold, &metadata, setting);
        let a = recovery_args(&metadata, &approval, setting);
        fs::write(&mode, setting).unwrap();
        let reply = delegated_exchange(&fleet, &token, "resource_recover", a.clone());
        assert_eq!(reply["ok"], false, "{reply}");
        assert_eq!(
            reply["error"]["code"], "outcome_unknown",
            "{setting}: {reply}"
        );
        assert!(!reply.to_string().contains("secret-canary"));
        assert!(!recovery_is_held(&hold));
        fs::write(&mode, "live").unwrap();
        if setting == "malformed" {
            assert_eq!(
                delegated_ok(&fleet, &token, "resource_recover", a)["outcome"],
                "recovered"
            );
        } else if setting == "replace" {
            credential_request(
                &fleet,
                &token,
                "resource_recover",
                a,
                "unsupported_arguments",
            );
        }
        // Release revocation is intentionally last: current credentials are
        // mandatory even for the already committed historical result.
        else {
            credential_request(&fleet, &token, "resource_recover", a, "revoked_grant");
        }
    }
    stop(supervisor);
}
