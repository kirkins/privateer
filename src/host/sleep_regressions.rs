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
    woke(&store, "alpha", "mail", None).unwrap();
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

/// A resumed adapter restores seen=85 and presents nothing on a focus wake.
/// Rebinding its new epoch must not erase the receipt from the previous turn.
#[test]
fn accepted_receipts_survive_no_mail_wakes_and_custody_rebinding() {
    for holder in ["host:old", "host:new"] {
        let (_temp, mut store) = fixture();
        store.connection().execute_batch(
            "UPDATE ships SET record='{\"location\":\"here\",\"first_mate\":{\"host\":\"herdr\",\"duty_task\":\"alpha-duty\"}}';
             INSERT INTO mail(seq,id,sender,recipient,kind,body,payload,urgency,created_ms)
               VALUES(84,'earlier','captain','alpha','notice','action','{}','normal',1500);"
        ).unwrap();
        store.set_caller(Caller::Agent {
            actor: "alpha".into(),
        });
        crate::wake_reason::hosted(
            &store,
            &json!({
                "ship":"alpha", "holder":"host:old", "session":"beta", "trigger":"mail",
                "after_seq":83, "through_seq":85
            }),
        )
        .unwrap();
        assert_eq!(progress(&store), (85, 83));
        // A diagnostic report is deliberately greater than the real receipt.
        crate::wake_reason::hosted(
            &store,
            &json!({
                "ship":"alpha", "holder":"host:old", "session":"beta", "trigger":"delivery",
                "accepted_through":99, "decline_count":3, "paused":true,
                "decline_reason":"guard_paused", "busy":true, "turn_started_ms":1000
            }),
        )
        .unwrap();
        if holder != "host:old" {
            assert!(crate::presence::hand_over(&store, "alpha", "host:old", holder).unwrap());
        }
        let asleep = asleep_at_start(&store, "alpha", holder).unwrap();
        assert_eq!(asleep.through_seq, 85);
        assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), None);
        woke_at(&store, "alpha", "captain focus", None, 10_000).unwrap();
        crate::wake_reason::hosted(
            &store,
            &json!({
                "ship":"alpha", "holder":holder, "session":"beta", "trigger":"delivery_bind"
            }),
        )
        .unwrap();
        assert_eq!(progress(&store), (85, 83), "binding is not new acceptance");
        let diagnostics: (String, i64, i64, i64, bool, Option<String>, Option<i64>) = store
            .connection()
            .query_row(
                "SELECT holder,holder_since_ms,report_accepted,declines,paused,reason,busy_since_ms
             FROM hosted_delivery WHERE ship='alpha'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            diagnostics,
            (holder.into(), 10_000, 0, 0, false, None, None)
        );
        assert!(
            crate::host::delivery::alert(&store, 10_000)
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::host::delivery::alert(&store, 400_000)
                .unwrap()
                .is_empty(),
            "already accepted mail must not cause a false live-session stall"
        );
        store
            .connection()
            .execute(
                "INSERT INTO work_sessions(ship,holder,idle,checked_ms,idle_since_ms)
             VALUES('alpha',?1,1,400000,398000)",
                [holder],
            )
            .unwrap();
        let asleep = sleep_with_clock(&store, "alpha", holder, 1, || 400_000)
            .unwrap()
            .unwrap();
        assert_eq!(asleep.through_seq, 85);
        assert_eq!(
            alarm(&store, "alpha", &asleep).unwrap(),
            None,
            "no futile wake/sleep loop when the ack cursor lags acceptance"
        );
        store.connection().execute_batch(
            "INSERT INTO mail(seq,id,sender,recipient,kind,body,payload,urgency,created_ms)
               VALUES(86,'new-action','captain','alpha','notice','next action','{}','normal',400001);"
        ).unwrap();
        assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
        assert_eq!(progress(&store), (85, 83), "86 has not been accepted");
    }
}

#[test]
fn rebinding_a_new_holder_does_not_accept_pending_mail_or_hide_a_live_stall() {
    let (_temp, mut store) = fixture();
    store.connection().execute_batch(
        "UPDATE ships SET record='{\"location\":\"here\",\"first_mate\":{\"host\":\"herdr\",\"duty_task\":\"alpha-duty\"}}';"
    ).unwrap();
    crate::presence::hand_over(&store, "alpha", "host:old", "host:new").unwrap();
    woke_at(&store, "alpha", "captain focus", None, 10_000).unwrap();
    store.set_caller(Caller::Agent {
        actor: "alpha".into(),
    });
    crate::wake_reason::hosted(
        &store,
        &json!({
            "ship":"alpha", "holder":"host:new", "session":"beta", "trigger":"delivery_bind"
        }),
    )
    .unwrap();
    assert_eq!(progress(&store), (83, 83), "diagnostic 85 is not a receipt");
    assert!(
        crate::host::delivery::alert(&store, 10_000)
            .unwrap()
            .is_empty()
    );
    assert!(
        crate::host::delivery::alert(&store, 309_999)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        crate::host::delivery::alert(&store, 310_000).unwrap().len(),
        1,
        "genuinely unaccepted mail still alerts against an awake session"
    );
    let asleep = asleep_at_start(&store, "alpha", "host:new").unwrap();
    assert_eq!(asleep.through_seq, 83);
    assert_eq!(alarm(&store, "alpha", &asleep).unwrap(), Some("mail"));
    assert_eq!(progress(&store), (83, 83));
}

#[test]
fn a_failed_asleep_flag_clear_is_a_failed_wake() {
    let (_temp, store) = fixture();
    asleep_at_start(&store, "alpha", "host:old").unwrap();
    store
        .connection()
        .execute_batch(
            "CREATE TRIGGER fail_wake BEFORE UPDATE ON presence
         WHEN OLD.asleep_holder IS NOT NULL AND NEW.asleep_holder IS NULL
         BEGIN SELECT RAISE(ABORT,'injected asleep flag clear failure'); END;",
        )
        .unwrap();
    let error = woke_at(&store, "alpha", "mail", None, 10_000).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected asleep flag clear failure")
    );
    assert!(
        crate::presence::session(&store, "alpha")
            .unwrap()
            .unwrap()
            .asleep
    );
    let wakes: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM audit WHERE kind='host.woke'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        wakes, 0,
        "failed state change must not record a successful wake"
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
