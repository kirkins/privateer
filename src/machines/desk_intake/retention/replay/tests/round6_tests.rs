use super::*;

#[test]
fn already_seeded_quiet_capture_never_begins_a_registry_transaction() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let f = Fixture::new(false);
    let transactions = Arc::new(AtomicUsize::new(0));
    let seen = transactions.clone();
    // A full frozen-path contact calls seed_baseline twice. Deny every registry
    // transaction, not just writes, so this test cannot pass on a no-op BEGIN.
    f.r.authorizer(Some(move |ctx: AuthContext<'_>| {
        if matches!(ctx.action, AuthAction::Transaction { .. }) {
            seen.fetch_add(1, Ordering::SeqCst);
            return Authorization::Deny;
        }
        Authorization::Allow
    }));
    assert_eq!(f.capture()["imported"], 0);
    assert_eq!(transactions.load(Ordering::SeqCst), 0);
    f.r.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);

    // Also exercise a real competing writer: reads must succeed without a
    // registry busy error, with no timeout or timing-dependent wait.
    let blocker = Connection::open(f.dir.path().join("registry.sqlite")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    f.r.busy_timeout(Duration::ZERO).unwrap();
    assert_eq!(f.capture()["imported"], 0);
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn enrollment_published_after_seeding_imports_without_witness_then_recovers() {
    let mut f = Fixture::new(false);
    let path = f.dir.path().join("alpha");
    let bound = format!("{}:{ALPHA}", binding(&f.m).unwrap());
    let old_nonce = source::enrollment::published_nonce(f.h.connection(), &bound).unwrap();
    // Deterministically put publication between the contact's seed and its
    // per-import read. The new empty baseline has the same binding and upper;
    // only the enrollment nonce differs from the registry's seed.
    f.h.connection()
        .execute("DELETE FROM mail_source_snapshot", [])
        .unwrap();
    source::enrollment::begin(f.h.connection(), &bound).unwrap();
    let baseline = source::enrollment::open(&path, false).unwrap();
    source::enrollment::freeze(f.h.connection(), &baseline, &bound, deadline()).unwrap();
    source::enrollment::publish(f.h.connection(), &baseline, &bound).unwrap();
    assert_ne!(
        source::enrollment::published_nonce(f.h.connection(), &bound).unwrap(),
        old_nonce
    );
    f.send("after-reenrollment");
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    let imported = super::super::super::super::home::import_pending(
        &path,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    assert_eq!(imported.0, 1); // Ordinary receipt is not withheld.
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 1);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_omission").unwrap(), 1);

    f.capture(); // The next contact seeds the exact newly published identity.
    f.send("after-new-seed");
    f.capture();
    assert_eq!(f.witness().origin.id, "after-new-seed");
}

#[test]
fn changed_snapshot_upper_alone_cannot_use_the_previous_seed() {
    let mut f = Fixture::new(false);
    let path = f.dir.path().join("alpha");
    f.send("covered-by-later-upper");
    // Model a later baseline upper without changing the binding or nonce.
    // A second row is beyond BOTH uppers, but its observation is still not
    // judged against the old seed. No frozen-sidecar contact is used here.
    f.h.connection()
        .execute("UPDATE mail_source_snapshot SET upper_seq=upper_seq+1", [])
        .unwrap();
    f.send("beyond-later-upper");
    source::stage_deltas_until(f.h.connection(), None).unwrap();
    let imported = super::super::super::super::home::import_pending(
        &path,
        f.h.connection(),
        &f.r,
        &f.m,
        ALPHA,
        None,
    )
    .unwrap();
    assert_eq!(imported.0, 2);
    assert_eq!(count(&f.r, "desk_home_receipt").unwrap(), 2);
    assert_eq!(count(&f.r, "desk_replay_import").unwrap(), 0);
    assert_eq!(count(&f.r, "desk_replay_witness").unwrap(), 0);
}
