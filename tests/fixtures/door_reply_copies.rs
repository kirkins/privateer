#[test]
fn delegated_carried_ask_reply_refuses_wrong_mailbox_without_answering_owner() {
    let fleet = Fleet::with_ships(&[("alpha", true), ("beta", false)]);
    delegated_adapter(&fleet);
    let (grant, full) = delegated_issue(&fleet, "reader-writer", "mail:read,ship:message", None);
    let (_, narrow) = delegated_issue(&fleet, "alpha-only", "mail:read,ship:message", Some("alpha"));
    let (_, writer) = delegated_issue(&fleet, "writer", "ship:message", None);
    for ship in ["alpha", "beta"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        conn.execute("INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state) VALUES('carried-ask','beta','captain','request','Question from original owner','{}','normal',1,'open')", []).unwrap();
    }
    let supervisor = fleet.supervise();
    fleet.wait("copy reply hello", |e| e.first().cloned());
    let thread = delegated_ok(&fleet, &full, "thread", json!({"ship":"alpha","id":"carried-ask"}));
    assert_eq!(thread["items"][0]["mail"]["from"], "beta");
    let args = json!({"kind":"reply","correlation":"carried-ask","body":"Answer","idempotency_key":"copy-reply"});
    let refused = delegated_exchange(&fleet, &full, "send", args.clone());
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["code"], "reply_wrong_mailbox");
    assert!(refused["error"]["message"].as_str().unwrap().contains("normal recipient is the ship that sent the original ask"));
    // Hidden sender, unseen ask, and absent read authority keep the opaque refusal.
    for (token, attempt) in [
        (&narrow, args.clone()),
        (&writer, args.clone()),
        (&full, json!({"kind":"reply","correlation":"absent","body":"Answer"})),
    ] {
        let hidden = delegated_exchange(&fleet, token, "send", attempt);
        assert_eq!(hidden["ok"], false);
        assert_eq!(hidden["error"]["code"], "not_found");
    }
    for ship in ["alpha", "beta"] {
        let conn = rusqlite::Connection::open(fleet.home(ship).join("privateer.sqlite")).unwrap();
        assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE correlation='carried-ask' AND kind='reply')", [], |r| r.get::<_, bool>(0)).unwrap());
        assert_eq!(conn.query_row("SELECT state FROM mail WHERE id='carried-ask'", [], |r| r.get::<_, String>(0)).unwrap(), "open");
    }
    let registry = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    assert!(!registry.query_row("SELECT EXISTS(SELECT 1 FROM door_write_results WHERE command='send' AND grant_id=?1)", [grant["clients"][0]["id"].as_str().unwrap()], |r| r.get::<_, bool>(0)).unwrap(), "a no-effect refusal must not retain a write receipt");
    // Stale asks must not become actionable wrong-mailbox refusals, even when
    // reusing the key from the earlier no-effect refusal.
    let copy = rusqlite::Connection::open(fleet.home("alpha").join("privateer.sqlite")).unwrap();
    copy.execute("UPDATE mail SET state='answered' WHERE id='carried-ask'", []).unwrap();
    let stale = delegated_exchange(&fleet, &full, "send", args);
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["error"]["code"], "not_found");
    // The existing explicit runtime recipient route still answers only at the owner.
    let sent = delegated_ok(&fleet, &full, "send", json!({"to":"beta","kind":"reply","correlation":"carried-ask","body":"Answer at owner"}));
    assert_eq!(sent["ship"], "beta");
    assert_eq!(sent["mail"]["to"], "beta");
    stop(supervisor);
}
