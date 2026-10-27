#[test]
fn placement_decision_control_card_survives_legacy_fence_under_real_supervisor() {
    let fleet = Fleet::new();
    fs::write(fleet.root.join("adapter/door.py"), r#"import json, os, sys
log = open(os.environ['FAKE_DOOR_LOG'], 'a')
def note(v):
    log.write(json.dumps(v)+'\n'); log.flush()
note({'hello':json.loads(sys.stdin.readline())})
for line in sys.stdin:
    event=json.loads(line); note(event)
    if event.get('control') == 'desk_placement':
        mail=event['mail']
        print(json.dumps({'type':'delivered','ref':event['ref']}),flush=True)
        print(json.dumps({'type':'request','id':'confirm','command':'send','args':{
            'to':mail['from'],'kind':'reply','correlation':mail['id'],'body':'Confirm this exact manifest',
            'payload':{'answer':{'option_id':'accept'}}}}),flush=True)
"#).unwrap();
    fleet.add();
    let run = |args: &[&str]| fleet.ok_in(&fleet.root.join("ships/ops"),args);
    run(&["mail.send", "--json", &json!({"id":"retained-unread","from":"ops","to":"captain","kind":"notice","body":"Must not be acknowledged by the control card"}).to_string()]);
    // The supervisor must also work when no v1 cursor has ever existed. Its
    // control-card acknowledgment creates neither a cursor nor a digest.
    run(&["machine.desk.propose", "--json", &json!({"domain":"survey-domain","owner":"here","sources":["here"]}).to_string()]);
    let supervisor = fleet.supervise();
    fleet.wait("placement card confirmation", |events| events.iter().find(|e|e["id"] == "confirm" && e["ok"] == true).cloned());
    let status: Value = serde_json::from_str(&run(&["machine.desk.status","--json","{}"])) .unwrap();
    assert_eq!(status["records"][0]["state"],"active");
    assert_eq!(status["desk_here"],true);
    assert!(!fleet.entries().iter().any(|e|e["mail"]["id"] == "retained-unread"));
    stop(supervisor);
    for ship in ["liaison","ops"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='door_cursor'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
}
