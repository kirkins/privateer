//! Handoff regressions: wake eligibility is not acceptance or acknowledgement.
use super::*;
use crate::store::Caller;

fn fixture() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).unwrap();
    store
        .connection()
        .execute_batch(
            "INSERT INTO ships(name,record) VALUES('alpha','{\"location\":\"here\"}');
         INSERT INTO presence(ship,holder,started_ms,holder_since_ms,expires_ms,presented_through)
           VALUES('alpha','host:old',1000,1000,9223372036854775807,83);
         INSERT INTO mail_cursor(address,through_seq) VALUES('alpha',83);
         INSERT INTO hosted_delivery(ship,holder,holder_since_ms,accepted,report_accepted,declines)
           VALUES('alpha','host:old',1000,83,85,3);
         INSERT INTO mail(seq,id,sender,recipient,kind,body,payload,urgency,created_ms)
           VALUES(85,'backlog','crew','alpha','notice','pending outcome','{}','normal',2000);
         INSERT INTO host_launches(ship,holder,failures,retry_ms,start_asleep,start_through)
           VALUES('alpha','host:old',0,0,1,85);",
        )
        .unwrap();
    (temp, store)
}

fn progress(store: &Store) -> (i64, i64) {
    store
        .connection()
        .query_row(
            "SELECT accepted,(SELECT through_seq FROM mail_cursor WHERE address='alpha')
         FROM hosted_delivery WHERE ship='alpha'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn asleep_upgrade_relaunch_wakes_for_pre_handoff_backlog_without_fabricating_progress() {
    let (_temp, mut store) = fixture();
    // Exercise the real fence; an explicit request avoids release/env clocks.
    store
        .connection()
        .execute(
            "INSERT INTO hosted_relaunches(ship,holder,force,requested_ms,expires_ms)
         VALUES('alpha','host:old',0,0,9223372036854775807)",
            [],
        )
        .unwrap();
    let (_, fenced, through) = crate::host::upgrade::fence(&store, "alpha", "host:old", 1800)
        .unwrap()
        .unwrap();
    assert_eq!(through, 83);
    let start = crate::host::upgrade::start_point("upgrade", 1800, through);
    crate::host::upgrade::retire(&store, "alpha", &fenced, "upgrade", start);
    crate::presence::claim(&store, "alpha", "host:new", 90, None).unwrap();
    // Dispatcher has a new holder; old binaries may still have carried 85.
    store
        .connection()
        .execute(
            "UPDATE host_launches SET holder='host:new',start_through=85 WHERE ship='alpha'",
            [],
        )
        .unwrap();
    let asleep = asleep_at_start(&store, "alpha", "host:new").unwrap();
    assert_eq!(asleep.through_seq, 83);
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
    assert_eq!(
        progress(&store),
        (83, 83),
        "wake eligibility handles no mail"
    );
    // A real adapter's accepted receipt, under current custody, is separate.
    woke(&store, "alpha", "mail", None);
    store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    crate::wake_reason::hosted(
        &store,
        &json!({"ship":"alpha","holder":"host:new",
        "session":"beta","trigger":"mail","after_seq":83,"through_seq":85}),
    )
    .unwrap();
    assert_eq!(
        progress(&store),
        (85, 83),
        "acceptance is not acknowledgement"
    );
    assert_eq!(
        alarm(
            &store,
            "alpha",
            &asleep_at_start(&store, "alpha", "host:new").unwrap()
        )
        .unwrap(),
        None
    );
}

#[test]
fn no_carried_boundary_and_old_acceptance_are_handled_without_new_receipts() {
    let (_temp, store) = fixture();
    store
        .connection()
        .execute_batch(
            "DELETE FROM presence;
         INSERT INTO presence(ship,holder,started_ms,holder_since_ms,expires_ms)
           VALUES('alpha','host:new',3000,3000,9223372036854775807);
         DELETE FROM mail_cursor;
         DELETE FROM host_launches;",
        )
        .unwrap();
    let asleep = asleep_at_start(&store, "alpha", "host:new").unwrap();
    assert_eq!(
        asleep.through_seq, 83,
        "old actual receipt is historical evidence"
    );
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
    let (holder, accepted): (String, i64) = store
        .connection()
        .query_row("SELECT holder,accepted FROM hosted_delivery", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((holder.as_str(), accepted), ("host:old", 83));
}

#[test]
fn new_actionable_mail_and_open_requests_wake_but_routine_low_notices_wait() {
    let (_temp, store) = fixture();
    store.connection().execute("DELETE FROM mail", []).unwrap();
    let asleep = asleep_at_start(&store, "alpha", "host:old").unwrap();
    store
        .connection()
        .execute_batch(
            "INSERT INTO mail(seq,id,sender,recipient,kind,body,payload,urgency,created_ms)
           VALUES(84,'low','captain','alpha','notice','routine','{}','low',2000);",
        )
        .unwrap();
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), None);
    store
        .connection()
        .execute("UPDATE mail SET urgency='normal'", [])
        .unwrap();
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
    // Even an accepted request remains open until answered or acknowledged.
    store
        .connection()
        .execute_batch(
            "UPDATE mail SET kind='request',state='open';
         UPDATE hosted_delivery SET accepted=84;
         UPDATE presence SET presented_through=84;",
        )
        .unwrap();
    let asleep = Asleep {
        holder: asleep.holder,
        through_seq: 84,
    };
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
    assert_eq!(progress(&store), (84, 83));
    store
        .connection()
        .execute("UPDATE mail_cursor SET through_seq=84", [])
        .unwrap();
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), None);
}
