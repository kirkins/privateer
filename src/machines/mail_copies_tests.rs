use super::*;
const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
#[test]
fn copy_metadata_is_bound_bounded_and_optional_for_old_direct_receivers() {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path()).unwrap();
    crate::mail::source::bind(store.connection(), ALPHA).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"survey","location":"here"}),
    )
    .unwrap();
    crate::mail::dispatch(
        &mut store,
        "mail.send",
        json!({"id":"report","from":"survey","to":"captain","kind":"notice","body":"Report"}),
    )
    .unwrap();
    let plain = json!({"id":"report","from":"survey","to":"captain","kind":"notice","body":"Report","payload":{}});
    let mut wire = plain.clone();
    direct(&store, &mut wire);
    assert_eq!(wire["mail_copy_v1"], true);
    assert!(matches!(provenance(&wire, &plain), Provenance::Copy(_)));
    let mut changed = plain.clone();
    changed["body"] = json!("Changed content");
    assert_eq!(provenance(&wire, &changed), Provenance::Unresolved);
    let mut invalid = wire.clone();
    invalid["mail_copies"]["report"]["origin"]["forged"] = json!(true);
    assert_eq!(provenance(&invalid, &plain), Provenance::Unresolved);
    let mut large = plain.clone();
    large["body"] = json!("x".repeat(crate::mail::MAX_MESSAGE_BYTES - 100));
    let original = large.clone();
    direct(&store, &mut large);
    assert_eq!(
        large, original,
        "optional evidence cannot overflow an old peer's mail limit"
    );
    let before = store.connection().total_changes();
    let mut copies = json!({});
    add(&mut copies, &store, &plain);
    assert_eq!(
        store.connection().total_changes(),
        before,
        "outbound evidence is read-only"
    );
    assert!(desk_intake::transport::json_size_bound(&copies) <= BYTES);
    let mut body = json!({"messages":[plain]});
    let original = body.clone();
    attach(&mut body, copies, 32);
    assert_eq!(
        body, original,
        "even the offer must be omitted at the enclosing boundary"
    );
    let agent = Store::open_as(
        t.path(),
        crate::store::Caller::Agent {
            actor: "survey".into(),
        },
    )
    .unwrap();
    let forged =
        json!({"id":"forged","from":"captain","to":"survey","kind":"notice","body":"Not admitted"});
    let error =
        crate::mail::deliver_with_provenance(&agent, &forged, &Provenance::Unresolved).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent survey cannot act as \"captain\""),
        "{error:#}"
    );
}
