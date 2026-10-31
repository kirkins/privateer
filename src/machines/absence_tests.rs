use super::*;

fn directories() -> Directories {
    Directories {
        links: vec![
            ("beta".into(), "beta:1".into()),
            ("gamma".into(), "gamma:1".into()),
        ],
        listed: vec![],
    }
}

fn pending(store: &Store) {
    store.connection().execute(
        "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,state,delivery)
         VALUES('survey','alpha','beta','request','Survey results','{}','normal',1,'open','pending')", [],
    ).unwrap();
}

fn observe(store: &Store, machine: &str, complete: bool, directories: &Directories) {
    let tx = store.write_transaction().unwrap();
    observe_absence(store, machine, complete, directories).unwrap();
    tx.commit().unwrap();
}

fn delivery(store: &Store) -> String {
    store
        .connection()
        .query_row("SELECT delivery FROM mail WHERE id='survey'", [], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn absence_requires_three_complete_exchanges_from_every_link_and_survives_restart() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path()).unwrap();
    pending(&store);
    let dirs = directories();
    // A fast link cannot stand in for a sleeping or partially readable one.
    for _ in 0..10 {
        observe(&store, "beta", true, &dirs);
        observe(&store, "gamma", false, &dirs);
    }
    assert_eq!(delivery(&store), "pending");
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        observe(&store, "gamma", true, &dirs);
        assert_eq!(delivery(&store), "pending");
    }
    drop(store);
    let store = Store::open(t.path()).unwrap();
    observe(&store, "gamma", true, &dirs);
    assert_eq!(delivery(&store), "undeliverable");
    let request = crate::mail::dispatch(
        &mut Store::open(t.path()).unwrap(),
        "mail.show",
        json!({"id":"survey"}),
    )
    .unwrap();
    assert_eq!(request["state"], "withdrawn");
    assert!(
        request["end_reason"]
            .as_str()
            .unwrap()
            .contains("recipient_unlisted")
    );
    assert!(
        request["end_reason"]
            .as_str()
            .unwrap()
            .contains("3 complete exchanges")
    );
    let closure: (String, String) = store
        .connection()
        .query_row(
            "SELECT delivery,delivery_error FROM mail WHERE id=?1",
            [request["ended_by"].as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(closure.0, "undeliverable");
    assert!(closure.1.contains("recipient_unlisted"));
    let unread: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM mail WHERE COALESCE(delivery,'received')='received'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        unread, 0,
        "the closure must not become phantom received mail"
    );
    let counters: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM mail_recipient_absence", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(counters, 0);

    // Reappearance after refusal cannot revive old mail.
    let mut dirs = dirs;
    dirs.listed.push("beta".into());
    observe(&store, "beta", true, &dirs);
    assert_eq!(delivery(&store), "undeliverable");
}

#[test]
fn a_listing_on_any_machine_resets_all_counts_even_on_a_partial_exchange() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path()).unwrap();
    pending(&store);
    let mut dirs = directories();
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        for name in ["beta", "gamma"] {
            observe(&store, name, true, &dirs);
        }
    }
    // A cached listing protects a name even if move activation deleted the
    // home's ships row. Reappearance on another peer resets BOTH counts.
    dirs.listed.push("beta".into());
    observe(&store, "gamma", false, &dirs);
    dirs.listed.clear();
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        for name in ["beta", "gamma"] {
            observe(&store, name, true, &dirs);
        }
        assert_eq!(delivery(&store), "pending");
    }
    observe(&store, "beta", true, &dirs);
    assert_eq!(delivery(&store), "pending");
    observe(&store, "gamma", true, &dirs);
    assert_eq!(delivery(&store), "undeliverable");
}

#[test]
fn a_replaced_link_does_not_inherit_observations_from_its_old_incarnation() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(t.path()).unwrap();
    pending(&store);
    let mut dirs = directories();
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        for name in ["beta", "gamma"] {
            observe(&store, name, true, &dirs);
        }
    }
    dirs.links[1].1 = "gamma:2".into();
    observe(&store, "beta", true, &dirs);
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        observe(&store, "gamma", true, &dirs);
        assert_eq!(delivery(&store), "pending");
    }
    observe(&store, "gamma", true, &dirs);
    assert_eq!(delivery(&store), "undeliverable");
}

#[test]
fn removed_link_protection_survives_restart_and_clears_on_relink() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("fleet.sqlite");
    let registry = Connection::open(&path).unwrap();
    init(&registry).unwrap();
    registry
        .execute_batch("CREATE TABLE ships(name TEXT PRIMARY KEY, retired_ms INTEGER);")
        .unwrap();
    let beta = Machine {
        name: "beta".into(),
        role: "collected".into(),
        peer: json!({"name":"machine-beta","transport":"collected"}),
        known_as: None,
        refreshed_ms: None,
        next_contact_ms: None,
        failures: 0,
    };
    let gamma = Machine {
        name: "gamma".into(),
        role: "collected".into(),
        peer: json!({"name":"machine-gamma","transport":"collected"}),
        known_as: None,
        refreshed_ms: None,
        next_contact_ms: None,
        failures: 0,
    };
    let listing = Listing {
        scope: String::new(),
        consumes: json!([]),
        requires: json!([]),
        pinned: Value::Null,
    };
    record(&registry, &beta, &[("beta".into(), listing)]).unwrap();
    record(&registry, &gamma, &[]).unwrap();
    let home = t.path().join("alpha");
    let store = Store::open(&home).unwrap();
    pending(&store);
    // The retained names and link removal must commit or roll back together.
    registry
        .execute_batch(
            "CREATE TRIGGER reject_remove BEFORE DELETE ON machines
         BEGIN SELECT RAISE(ABORT,'injected removal failure'); END;",
        )
        .unwrap();
    assert!(forget_link(&registry, "beta").is_err());
    assert!(linked(&registry, "beta").unwrap());
    let protected: bool = registry
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM removed_machine_ships)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!protected);
    assert_eq!(cached(&registry, "beta").unwrap().len(), 1);
    registry
        .execute_batch("DROP TRIGGER reject_remove")
        .unwrap();
    forget_link(&registry, "beta").unwrap();
    drop(registry);
    drop(store);

    let registry = Connection::open(&path).unwrap();
    init(&registry).unwrap();
    let store = Store::open(&home).unwrap();
    let dirs = Directories::read(&registry).unwrap();
    assert_eq!(dirs.links.len(), 1);
    assert_eq!(dirs.listed, ["beta"]);
    for _ in 0..UNKNOWN_EXCHANGES + 2 {
        observe(&store, "gamma", true, &dirs);
        assert_eq!(delivery(&store), "pending");
    }

    // Re-link with an empty directory: the protection must really clear,
    // allowing normal all-link absence counting to start again.
    record(&registry, &beta, &[]).unwrap();
    let dirs = Directories::read(&registry).unwrap();
    assert!(dirs.listed.is_empty());
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        for name in ["beta", "gamma"] {
            observe(&store, name, true, &dirs);
        }
        assert_eq!(delivery(&store), "pending");
    }
    observe(&store, "beta", true, &dirs);
    assert_eq!(delivery(&store), "pending");
    observe(&store, "gamma", true, &dirs);
    assert_eq!(delivery(&store), "undeliverable");
}

#[test]
fn a_ship_put_resets_counts_even_if_the_row_disappears_before_the_next_exchange() {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path()).unwrap();
    pending(&store);
    let dirs = directories();
    for _ in 0..UNKNOWN_EXCHANGES - 1 {
        for name in ["beta", "gamma"] {
            observe(&store, name, true, &dirs);
        }
    }
    crate::peers::dispatch(
        &mut store,
        "peer.put",
        json!({"name":"machine-gamma","transport":"collected"}),
    )
    .unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"beta","location":{"peer":"machine-gamma"}}),
    )
    .unwrap();
    store
        .connection()
        .execute("DELETE FROM ships WHERE name='beta'", [])
        .unwrap();
    for name in ["beta", "gamma"] {
        observe(&store, name, true, &dirs);
    }
    assert_eq!(delivery(&store), "pending");
}

#[test]
fn bearings_show_newest_recent_refusals_not_old_history() {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path()).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"alpha","location":"here"}),
    )
    .unwrap();
    pending(&store);
    let now = Store::now_ms();
    store
        .connection()
        .execute(
            "UPDATE mail SET delivery='undeliverable',refused_ms=?1,delivery_error='old refusal'",
            [now - 8 * 24 * 60 * 60 * 1000],
        )
        .unwrap();
    // Old mail refused today belongs in bearings; age is refusal time, not send time.
    for i in 0..6 {
        store.connection().execute(
            "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,delivery,delivery_error,refused_ms)
             VALUES(?1,'alpha','beta','notice','Survey','{}','low',1,'undeliverable','recipient_unlisted',?2)",
            params![format!("recent-{i}"), now],
        ).unwrap();
    }
    let bearings = crate::context::bearings(&store, "alpha").unwrap();
    let refused = bearings["refused_mail"].as_array().unwrap();
    assert_eq!(refused.len(), 5);
    assert!(
        refused[0]
            .as_str()
            .unwrap()
            .starts_with("recent-5 to beta:"),
        "{bearings}"
    );
    assert!(
        refused[4]
            .as_str()
            .unwrap()
            .starts_with("recent-1 to beta:"),
        "{bearings}"
    );
    assert!(!bearings["text"].as_str().unwrap().contains("old refusal"));
}

#[test]
fn prune_audit_counters_and_refusal_roll_back_together() {
    let t = tempfile::tempdir().unwrap();
    let mut store = Store::open(t.path()).unwrap();
    let machine = Machine {
        name: "beta".into(),
        role: "collected".into(),
        peer: json!({"name":"machine-beta","transport":"collected"}),
        known_as: None,
        refreshed_ms: None,
        next_contact_ms: None,
        failures: 0,
    };
    let dirs = Directories {
        links: vec![("beta".into(), "beta:1".into())],
        listed: vec![],
    };
    crate::peers::dispatch(&mut store, "peer.put", machine.peer.clone()).unwrap();
    crate::mail::dispatch(
        &mut store,
        "ship.put",
        json!({"name":"beta","location":{"peer":"machine-beta"}}),
    )
    .unwrap();
    pending(&store);
    store
        .connection()
        .execute(
            "INSERT INTO mail_recipient_absence VALUES('beta','beta:1',?1)",
            [UNKNOWN_EXCHANGES - 1],
        )
        .unwrap();
    // Fail after deletion, audit, counter increment and request closure.
    store.connection().execute_batch(
        "CREATE TRIGGER reject_refusal BEFORE UPDATE OF delivery ON mail
         WHEN NEW.delivery='undeliverable' BEGIN SELECT RAISE(ABORT,'injected refusal failure'); END;",
    ).unwrap();
    assert!(install(&mut store, &machine, &[], true, Some(&dirs)).is_err());
    assert_eq!(delivery(&store), "pending");
    let remaining: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM ships WHERE name='beta'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 1);
    let passes: i64 = store
        .connection()
        .query_row("SELECT passes FROM mail_recipient_absence", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(passes, UNKNOWN_EXCHANGES - 1);
    let request: (String, Option<String>) = store
        .connection()
        .query_row(
            "SELECT state,ended_by FROM mail WHERE id='survey'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(request, ("open".into(), None));
    let dropped: bool = store
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM audit WHERE kind='ship.dropped')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!dropped);
}
