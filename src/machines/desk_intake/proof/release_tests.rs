use super::*;
use crate::machines::desk_intake::release;
use crate::store::Caller;

#[test]
fn released_home_is_still_hashed_revalidated_and_requires_readmit_on_reappearance() {
    let (dir, r, h, m) = setup();
    capture(&h, &r, &m);
    let retired = dir.path().join("beta");
    r.execute(
        "INSERT INTO ships VALUES('beta',?1,1)",
        [retired.to_str().unwrap()],
    )
    .unwrap();
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    let before = reconcile::load(&r, &m, ALPHA).unwrap().unwrap();
    assert!(!before.candidate);
    // Simulate the last dated census retained before this home disappeared.
    let mut cached = serde_json::to_value(&before).unwrap();
    for h in cached["homes"].as_array_mut().unwrap() {
        if h["state"] == "missing" {
            h["unread"] = json!(7);
            h["open"] = json!(3);
            h["facts_cached"] = json!(true);
        }
    }
    r.execute(
        "UPDATE desk_source_reconciliation SET proof=?1",
        [cached.to_string()],
    )
    .unwrap();
    let mut args = json!({"home":retired,"ship":"beta","operation":"release","reason":"Retired test database is absent"});
    args["digest"] = release::plan(&r, &args).unwrap()["digest"].clone();
    let stamp = registry_stamp(&r, &m, ALPHA).unwrap();
    release::apply(&r, &Caller::Operator, "release", &args).unwrap();
    assert_ne!(stamp, registry_stamp(&r, &m, ALPHA).unwrap());
    reconcile::save(&r, &m, ALPHA, deadline()).unwrap();
    let p = reconcile::load(&r, &m, ALPHA).unwrap().unwrap();
    assert!(p.candidate);
    let value = serde_json::to_value(&p).unwrap();
    let before = serde_json::to_value(&before).unwrap();
    assert_eq!(
        value["inventory"], before["inventory"],
        "release does not drop inventory membership"
    );
    assert_eq!(value["homes"].as_array().unwrap().len(), 2);
    assert!(
        value["homes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["state"] == "released"
                && h["unread"] == 7
                && h["open"] == 3
                && h["facts_cached"] == true)
    );
    let (_, evidence) = issued(&r, &m);
    let evidence = evidence.unwrap();
    assert!(reconcile::revalidate(&r, &m, ALPHA, &p, deadline()).unwrap());
    assert!(evidence.valid);
    let returned = Store::open(&retired).unwrap();
    assert!(!reconcile::revalidate(&r, &m, ALPHA, &p, deadline()).unwrap());
    let fresh = reconcile::inspect(&r, &m, ALPHA, &[], deadline()).unwrap();
    assert!(!fresh.candidate);
    assert!(
        serde_json::to_value(fresh).unwrap()["homes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["state"] == "reappeared")
    );
    args["operation"] = json!("readmit");
    args["digest"] = release::plan(&r, &args).unwrap()["digest"].clone();
    release::apply(&r, &Caller::Operator, "readmit", &args).unwrap();
    assert!(
        !reconcile::revalidate(&r, &m, ALPHA, &p, deadline()).unwrap(),
        "old proof invalid after audited readmit"
    );
    drop(returned);
    // Readmit is not enrollment/import proof: the returned home remains a
    // blocker until the ordinary intake path proves it.
    assert!(
        !reconcile::inspect(&r, &m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
}

#[test]
fn release_cannot_discharge_integrity_loss_and_epoch_invalidates_old_stamp() {
    let (_dir, r, h, m) = setup();
    capture(&h, &r, &m);
    let old = registry_stamp(&r, &m, ALPHA).unwrap();
    r.execute(
        "INSERT INTO desk_intake_loss VALUES(?1,?2,'integrity_gap')",
        params![m.domain, ALPHA],
    )
    .unwrap();
    let first = registry_stamp(&r, &m, ALPHA).unwrap();
    assert_ne!(old, first);
    r.execute(
        "UPDATE desk_intake_loss SET state=state WHERE domain=?1 AND source=?2",
        params![m.domain, ALPHA],
    )
    .unwrap();
    assert_ne!(first, registry_stamp(&r, &m, ALPHA).unwrap());
    assert!(
        !reconcile::inspect(&r, &m, ALPHA, &[], deadline())
            .unwrap()
            .candidate
    );
}
