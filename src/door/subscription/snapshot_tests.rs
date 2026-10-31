use super::*;

#[test]
fn bounded_prefix_does_not_leapfrog_a_large_row_or_drop_earlier_pending_rows() {
    let secret = uuid();
    let envelope = serde_json::to_vec(&Snapshot {
        secret,
        rows: Vec::new(),
        older: false,
        predecessor: None,
    })
    .unwrap()
    .len();
    let observation = |order, size| Observation {
        origin: MailIdentity::new(
            "00000000-0000-4000-8000-00000000000a",
            "00000000-0000-4000-8000-0000000000a1",
            &format!("mail-{order}"),
        )
        .unwrap(),
        order,
        content: json!({"body":"x".repeat(size)}),
        state: None,
    };
    let mut prefix = Prefix {
        rows: Default::default(),
        bytes: envelope,
        row_limit: PAGE_ROWS,
        cutoff: None,
        tail: false,
    };
    prefix.retain(observation(2, 9 * 1024 * 1024)).unwrap();
    prefix.retain(observation(3, 8 * 1024 * 1024)).unwrap();
    prefix.retain(observation(4, 1)).unwrap();
    // Pending versions arrive after the projection, potentially before its
    // lowest order. They must displace the tail rather than be lost/refused.
    prefix.retain(observation(1, 8 * 1024 * 1024)).unwrap();
    prefix.retain(observation(1, 8 * 1024 * 1024)).unwrap();
    assert_eq!(prefix.rows.len(), 1);
    assert_eq!(prefix.rows.values().next().unwrap().0.order, 1);
    assert_eq!(prefix.cutoff.unwrap().2, 2);
    assert!(prefix.bytes <= SNAPSHOT_BYTES);
}
