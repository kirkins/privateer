#[test]
fn scoped_desk_read_oversized_item_rolls_back_and_never_reads_ordinary_mailbox_rows() {
    let fleet = Fleet::with_ships(&[("alpha",true)]);
    delegated_adapter(&fleet);
    desk_exposure_owner(&fleet);
    fleet.call("alpha","door.desk.enable",desk_exposure_args());
    // A real ordinary mailbox request is NOT a desk-intake observation.
    fleet.call("alpha","mail.send",json!({"id":"ordinary-alpha","from":"alpha","to":"captain","kind":"request","body":"not intake"}));
    let (_,token) = delegated_issue(&fleet,"alpha","mail:read",None);
    let supervisor = fleet.supervise(); fleet.wait("desk oversize hello",|e|e.first().cloned());
    let open = delegated_ok(&fleet,&token,"desk.subscription.open",json!({}));
    let sub = open["subscription"].clone();
    let empty = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    assert_eq!(empty["items"],json!([]));
    desk_read_fact(&fleet,"alpha",1,"alpha",true,"request","open",&"alpha".repeat(50_000));
    let conn = rusqlite::Connection::open(fleet.root.join("state/fleet.sqlite")).unwrap();
    let custody = || ["desk_subscription_snapshot","desk_subscription_ref","desk_subscription_receipt","desk_subscription_exception","desk_subscription_pin"]
        .iter().map(|table|conn.query_row(&format!("SELECT COUNT(*) FROM {table}"),[],|r|r.get::<_,i64>(0)).unwrap()).collect::<Vec<_>>();
    let before = custody();
    let failure = delegated_ok(&fleet,&token,"desk.subscription.pull",json!({"subscription":sub}));
    assert_eq!(failure["error"]["code"],"oversized_item","{failure}");
    assert_eq!(failure["coverage"]["scope"],"desk-domain");
    assert_eq!(before,custody(),"no partial snapshot, pin or ref on refusal");
    stop(supervisor);
}
