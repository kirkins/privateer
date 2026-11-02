use super::*;

#[test]
fn aggregate_summary_includes_tail_ages_delays_and_unknown_clock_states() {
    let f = Fixture::remote();
    let b = binding_value(&f, 1000);
    import(&f, 1000);
    capture_at(&f.desk, &f.m, &b, false, &clock(1000)).unwrap();
    let original: SavedCut = serde_json::from_str(&saved(&f, &b)).unwrap();
    let key = crate::core::hash(&b);
    let now = clock(10_000);
    let mut inventory = vec![original.clone()];
    for n in 0..105 {
        let mut cut = original.clone();
        cut.home.home = format!("beta-{n:03}");
        cut.received_at = 9000;
        cut.tick_ms = 9000;
        match n {
            99 => {
                cut.delayed = true;
                cut.received_at = 100;
                cut.tick_ms = 100;
            }
            100 => cut.session = "beta".into(),
            101 => {
                cut.received_at = i64::MIN;
                cut.tick_ms = i64::MIN;
            }
            102 => {
                cut.received_at = 11_000;
                cut.tick_ms = 11_000;
            }
            103 => cut.tick_ms = 7999, // age/elapsed differ by 1001
            104 => cut.session.clear(),
            _ => {}
        }
        f.desk.execute(
            "INSERT INTO desk_exposure_cut(binding,source,home,mailbox,nonce,journal_upper,body) VALUES(?1,?2,?3,?4,?5,10,?6)",
            params![key, ALPHA, cut.home.home, cut.home.mailbox, cut.home.nonce, serde_json::to_string(&cut).unwrap()],
        ).unwrap();
        inventory.push(cut);
    }
    let result = summary(&f.desk, &key, ALPHA, &now).unwrap();
    assert_eq!(result["count"], inventory.len());
    assert_eq!(result["any_delayed"], true);
    assert_eq!(result["any_unproved"], true);
    assert_eq!(result["worst_age_ms"], 9900);
    assert_eq!(result["any_age_unknown"], true);
    let ages: Vec<_> = inventory.iter().map(|s| s.age(&now)).collect();
    assert_eq!(result["worst_age_ms"], json!(ages.iter().flatten().max()));
    assert_eq!(result["any_age_unknown"], ages.iter().any(Option::is_none));
    // No clock continuity: every cut is unknown, not age zero.
    let lost = Clock {
        session: "gamma",
        ..now
    };
    let result = summary(&f.desk, &key, ALPHA, &lost).unwrap();
    assert_eq!(result["worst_age_ms"], Value::Null);
    assert_eq!(result["any_age_unknown"], true);
    assert_eq!(
        summary(&f.desk, "absent", ALPHA, &lost).unwrap(),
        json!({"count":0,"any_delayed":false,"any_unproved":false,
            "worst_age_ms":null,"any_age_unknown":false})
    );
}
