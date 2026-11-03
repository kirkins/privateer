use super::*;

fn assert_failure(c: &Census, failure: Failure) {
    assert!(
        c.session_unverified.get(&failure).is_some_and(|n| *n > 0),
        "{failure:?}: {c:?}"
    );
    assert!(!c.activity_complete);
    c.validate().unwrap();
    let text = serde_json::to_string(c).unwrap();
    for secret in [
        "session-canary",
        "secret-title",
        "alpha.sock",
        "terminal_id",
        "sensitive-canary",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
}

#[test]
fn binding_failure_categories_are_observed_without_sensitive_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    for (category, raw) in [
        (Failure::BindingMissing, None),
        (Failure::BindingInvalid, Some("sensitive-canary".into())),
        (Failure::AdapterIdentityMissing, {
            let mut b = host.binding.clone();
            b.pane.agent_session = None;
            Some(serde_json::to_string(&b).unwrap())
        }),
        (Failure::SocketReplaced, {
            let mut b = host.binding.clone();
            b.socket_identity.1 += 1;
            Some(serde_json::to_string(&b).unwrap())
        }),
        (Failure::EndpointReplaced, {
            let mut b = host.binding.clone();
            b.pane.terminal_id = "sensitive-canary".into();
            Some(serde_json::to_string(&b).unwrap())
        }),
        (Failure::AdapterSessionReplaced, {
            let mut b = host.binding.clone();
            b.pane.agent_session = Some(json!("sensitive-canary"));
            Some(serde_json::to_string(&b).unwrap())
        }),
        (Failure::HostUnavailable, {
            let mut b = host.binding.clone();
            b.socket = dir.path().join("absent.sock");
            Some(serde_json::to_string(&b).unwrap())
        }),
    ] {
        let mut input = empty();
        input.lease = Some(("host:alpha".into(), i64::MAX, raw, None));
        assert_failure(&observe_now(&input), category);
    }
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), i64::MAX, Some(host.raw()), None));
    let (c, _, _) = observe(&input, "survey", Instant::now() - BUDGET, Some("alpha"));
    assert_failure(&c, Failure::ObservationBudgetExhausted);
    // Missing live adapter data is not mistaken for a replacement identity.
    host.pane.lock().unwrap().agent_session = None;
    assert_failure(&observe_now(&input), Failure::AdapterIdentityMissing);
}

#[test]
fn lifecycle_and_custody_categories_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    let mut input = empty();
    input.lease = Some(("host:alpha".into(), 0, Some(host.raw()), None));
    assert_failure(&observe_now(&input), Failure::LeaseExpired);
    input.lease = None;
    input.hosted_without_lease = true;
    assert_failure(&observe_now(&input), Failure::LeaseMissing);
    input.hosted_without_lease = false;
    input.session_gap = true;
    assert_failure(&observe_now(&input), Failure::SessionCustodyMissing);
    input.session_gap = false;
    let mut job = live_job("alpha");
    job["hosted"] = json!(true);
    input.jobs.push((job.to_string(), "crew".into()));
    assert_failure(&observe_now(&input), Failure::JobBindingMissing);
    input.panes.push(("alpha".into(), host.raw(), false, false));
    assert_failure(&observe_now(&input), Failure::JobIncarnationMismatch);
    input.jobs.clear();
    assert_failure(&observe_now(&input), Failure::SessionRoleUnknown);
    input.panes.clear();
    input.lease = Some((
        "host:alpha".into(),
        i64::MAX,
        Some(host.raw()),
        Some("host:alpha".into()),
    ));
    host.pane.lock().unwrap().agent_status = "working".into();
    assert_failure(&observe_now(&input), Failure::AsleepWorkingConflict);
    input.lease.as_mut().unwrap().3 = None;
    host.pane.lock().unwrap().agent_status = "sensitive-canary".into();
    assert_failure(&observe_now(&input), Failure::AdapterStatusUnknown);
}

#[test]
fn unverifiable_viewers_do_not_make_workers_or_activity_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let host = HostFixture::start(&dir.path().join("alpha.sock"));
    let mut missing_host = host.binding.clone();
    missing_host.socket = dir.path().join("absent.sock");
    for (category, raw) in [
        (Failure::BindingInvalid, "sensitive-canary".into()),
        (
            Failure::HostUnavailable,
            serde_json::to_string(&missing_host).unwrap(),
        ),
        (Failure::AdapterIdentityMissing, {
            let mut b = host.binding.clone();
            b.pane.agent_session = None;
            serde_json::to_string(&b).unwrap()
        }),
    ] {
        for worker in [false, true] {
            let mut input = empty();
            input.panes.push(("viewer".into(), raw.clone(), true, true));
            if worker {
                input
                    .jobs
                    .push((live_job("worker").to_string(), "crew".into()));
            }
            let c = observe_now(&input);
            assert!(
                c.workers_complete && c.activity_complete,
                "{category:?}: {c:?}"
            );
            assert!(!c.sessions_complete);
            assert_eq!(c.executing, worker);
            assert_eq!(c.active_workers, u64::from(worker));
            assert_eq!(c.sessions.unknown, 1);
            assert_eq!(c.reason, "identity_checked_census");
            assert_eq!(c.session_unverified.get(&category), Some(&1));
            let text = serde_json::to_string(&c).unwrap();
            for secret in [
                "sensitive-canary",
                "alpha.sock",
                "session-canary",
                "secret-title",
            ] {
                assert!(!text.contains(secret), "{text}");
            }
        }
    }
}

#[test]
fn categories_are_closed_bounded_and_backward_readable() {
    let mut c = Census::empty();
    let legacy = serde_json::to_value(&c).unwrap();
    assert!(legacy.get("session_unverified").is_none());
    assert!(serde_json::from_value::<Census>(legacy.clone()).is_ok());
    for _ in 0..1000 {
        c.unverified(Failure::BindingMissing);
    }
    assert_eq!(c.session_unverified[&Failure::BindingMissing], 402);
    c.validate().unwrap();
    let mut newer = legacy.clone();
    newer["session_unverified"] = json!({"future-category-canary":1,"binding_missing":2});
    let parsed: Census = serde_json::from_value(newer).unwrap();
    parsed.validate().unwrap();
    assert_eq!(parsed.session_unverified.len(), 1);
    assert_eq!(parsed.session_unverified[&Failure::BindingMissing], 2);
    assert!(
        !serde_json::to_string(&parsed)
            .unwrap()
            .contains("future-category-canary")
    );
    for value in [json!({"binding_missing":"invalid"}), json!([]), json!(null)] {
        let mut bad = legacy.clone();
        bad["session_unverified"] = value;
        assert!(serde_json::from_value::<Census>(bad).is_err());
    }
    for count in [0, 403, u64::MAX] {
        let mut bad = legacy.clone();
        bad["session_unverified"] = json!({"binding_missing":count});
        assert!(
            serde_json::from_value::<Census>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
