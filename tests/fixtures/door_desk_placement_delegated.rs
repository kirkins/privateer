#[test]
fn delegated_approval_answer_reply_cannot_create_generation_one_ownership() {
    let fleet = Fleet::new();
    delegated_adapter(&fleet);
    let (_, token) = delegated_issue(&fleet, "placement-reply", "ship:message,approval:answer", None);
    let run = |command: &str, args: Value| {
        let text = fleet.ok_in(&fleet.root.join("ships/ops"), &[command, "--json", &args.to_string()]);
        serde_json::from_str::<Value>(&text).unwrap()
    };
    let staged = run("machine.desk.propose", json!({"domain":"survey-domain","owner":"here","sources":["here"]}));
    let decision = staged["records"][0]["proposal"]["decision"].clone();
    // The coordinator writes to the first local home (liaison in this fixture).
    let card = fleet.call("liaison", "mail.show", json!({"id":decision}));
    let supervisor = fleet.supervise();
    fleet.wait("delegated hello", |e| e.first().cloned());
    let sent = delegated_ok(&fleet, &token, "send", json!({
        "to":card["from"],"kind":"reply","correlation":decision,
        "body":"A grant is not operator placement consent",
        "payload":{"answer":{"option_id":"accept"}}
    }));
    assert_eq!(sent["mail"]["kind"], "reply");
    let status = run("machine.desk.status", json!({}));
    assert!(status["records"][0]["approval"].is_null());
    assert!(status["records"][0]["certificate"].is_null());
    assert_eq!(status["records"][0]["state"], "pending");
    stop(supervisor);
}
