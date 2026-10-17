//! Real headless errand subprocesses, not invented busy flags. The fake host
//! models source-local display patches, never lifecycle authority.
use super::*;

fn aboard() -> (Place, Herdr) {
    host(Place::new())
}

/// Host `place`'s first mate in a fake Herdr.
fn host(place: Place) -> (Place, Herdr) {
    let socket = place.root.join("config/herdr/herdr.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let host = Herdr::start(
        &socket,
        vec![
            ("PATH".into(), place.path.clone()),
            ("HOME".into(), place.root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                place.root.join("config").display().to_string(),
            ),
        ],
    );
    assert_eq!(place.tick()["hosted"]["pane"], "w2:p1");
    until("native hosted payload", || {
        place.root.join("pi-pid").exists()
    });
    (place, host)
}

fn start(place: &Place, focus: &str) -> String {
    let report = place.agent(
        "errand.start",
        json!({"focus":focus,"brief":"PRIVATE BRIEF must not appear in sidebar"}),
    );
    assert_eq!(report["status"], "running");
    let id = report["errand"].as_str().unwrap().to_owned();
    until("actual headless errand payload", || {
        place.root.join(format!("{id}-pid")).exists()
    });
    id
}

fn finish(place: &Place, id: &str) {
    let pid: i32 = fs::read_to_string(place.root.join(format!("{id}-pid")))
        .unwrap()
        .parse()
        .unwrap();
    fs::write(place.root.join(format!("{id}-release")), "").unwrap();
    until("headless job success", || {
        let job = place.agent("job.status", json!({"id":id}));
        match job["status"].as_str().unwrap() {
            "succeeded" => true,
            "failed" | "cancelled" | "timed_out" | "orphaned" => {
                place.wake_diagnostics("errand terminal failure", id, &job);
                panic!("errand fixture job failed: {job}");
            }
            _ => false,
        }
    });
    until(
        "actual errand payload exit before fixture cleanup",
        || unsafe { libc::kill(pid, 0) } == -1,
    );
    place.tick();
    let list = place.agent("errand.list", json!({}));
    let errand = list["errands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(errand["status"], "succeeded", "{errand}");
}

fn captions(host: &Herdr) -> Vec<Value> {
    host.calls("pane.report_metadata")
        .into_iter()
        .filter(|r| r["params"]["source"] == "privateer-errands")
        .collect()
}

/// What the last caption sent to `pane` shows: null once cleared.
fn caption(host: &Herdr, pane: &str) -> Value {
    captions(host)
        .into_iter()
        .rfind(|r| r["params"]["pane_id"] == pane)
        .map_or(Value::Null, |r| r["params"]["display_agent"].clone())
}

fn cache_count(place: &Place) -> i64 {
    place
        .database()
        .query_row("SELECT COUNT(*) FROM errand_captions", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn headless_errand_captions_start_settle_and_survive_fresh_dispatchers_without_state_authority() {
    let (place, host) = aboard();
    place.json("task.create", json!({"id":"dashboard-102","charter_id":"infra","owner":"infra","title":"PRIVATE task title"}));
    let native_pid = place.native_pid();
    let panes = host.panes.lock().unwrap().clone();
    let first = start(&place, "task:dashboard-102");
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing dashboard-102");
    assert_eq!(captions(&host).len(), 1);
    assert_eq!(
        place.native_pid(),
        native_pid,
        "errands did not replace the hosted mate"
    );
    assert_eq!(
        host.calls("workspace.create").len(),
        1,
        "errands remain headless"
    );
    assert_eq!(place.wakes(), 0);
    assert_eq!(host.panes.lock().unwrap().as_slice(), panes.as_slice());

    // Herdr learning the native session after pre-spawn binding capture is
    // permitted by the existing unpinned Binding::matches contract.
    host.panes.lock().unwrap()[1]["agent_session"] = json!({"id":"native-session"});
    host.panes.lock().unwrap()[1]["agent"] = json!("pi");
    for _ in 0..3 {
        place.tick();
    } // separate real dispatcher processes, same durable ack
    assert_eq!(captions(&host).len(), 1);
    let second = start(&place, "thread:another-focus");
    assert_eq!(
        caption(&host, "w2:p1"),
        "infra · reviewing dashboard-102 +1"
    );
    for _ in 0..2 {
        place.tick();
    }
    assert_eq!(captions(&host).len(), 2);
    place
        .database()
        .execute(
            "UPDATE ships SET record=json_remove(record,'$.first_mate.host') WHERE name='infra'",
            [],
        )
        .unwrap();
    place.tick();
    assert_eq!(
        captions(&host).len(),
        3,
        "hosting-off clears only the still-bound live source"
    );
    assert!(caption(&host, "w2:p1").is_null());
    assert_eq!(cache_count(&place), 0);
    place.database().execute("UPDATE ships SET record=json_set(record,'$.first_mate.host','herdr') WHERE name='infra'", []).unwrap();
    place.tick();
    assert_eq!(captions(&host).len(), 4);
    assert_eq!(
        caption(&host, "w2:p1"),
        "infra · reviewing dashboard-102 +1"
    );
    finish(&place, &first);
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing another-focus");
    finish(&place, &second);
    assert!(caption(&host, "w2:p1").is_null());
    assert_eq!(captions(&host).len(), 6);
    assert_eq!(cache_count(&place), 0);
    for _ in 0..2 {
        place.tick();
    }
    assert_eq!(captions(&host).len(), 6);
    assert_eq!(
        host.panes.lock().unwrap()[1]["agent_session"],
        json!({"id":"native-session"})
    );
    assert_eq!(host.panes.lock().unwrap()[1]["agent_status"], "unknown");
    assert!(host.calls("pane.report_agent").is_empty());
    assert!(host.calls("pane.release_agent").is_empty());
    assert!(host.calls("pane.focus").is_empty());
    assert_eq!(host.calls("pane.send_input").len(), 1);
    for request in captions(&host) {
        let p = request["params"].as_object().unwrap();
        assert_eq!(p["source"], "privateer-errands");
        assert!(
            p.keys().all(
                |k| ["pane_id", "source", "display_agent", "clear_display_agent"]
                    .contains(&k.as_str())
            ),
            "{p:?}"
        );
        assert!(!request.to_string().contains("PRIVATE"));
    }
    place.stop_native("infra");
}

#[test]
fn a_terminal_duty_or_cancelled_ancestor_suppresses_errand_captions() {
    let (place, host) = aboard();
    place.json(
        "task.create",
        json!({"id":"caption-work","charter_id":"infra","owner":"infra","title":"Errand work"}),
    );
    let errand = start(&place, "task:caption-work");
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing caption-work");

    // The same hosted lease and running errand remain, but the live mate's
    // duty becomes terminal. Reconciliation may clear its old source, then
    // must not keep or republish an errand caption.
    place
        .database()
        .execute("UPDATE tasks SET status='done' WHERE id='infra-duty'", [])
        .unwrap();
    place.tick();
    assert!(caption(&host, "w2:p1").is_null());
    let count_after_terminal = captions(&host).len();
    assert_eq!(
        captions(&host).last().unwrap()["params"]["clear_display_agent"],
        true
    );

    // An open duty below a cancelled ancestor is equally inactive.
    place
        .database()
        .execute("UPDATE tasks SET status='open' WHERE id='infra-duty'", [])
        .unwrap();
    place.json(
        "task.create",
        json!({"id":"caption-ancestor","charter_id":"infra","owner":"infra","title":"Ancestor"}),
    );
    place
        .database()
        .execute(
            "UPDATE tasks SET parent='caption-ancestor' WHERE id='infra-duty'",
            [],
        )
        .unwrap();
    place
        .database()
        .execute(
            "UPDATE tasks SET status='cancelled' WHERE id='caption-ancestor'",
            [],
        )
        .unwrap();
    place.tick();
    assert!(caption(&host, "w2:p1").is_null());
    assert_eq!(captions(&host).len(), count_after_terminal);
    assert_eq!(cache_count(&place), 0);
    finish(&place, &errand);
    place.stop_native("infra");
}

/// Apply one hazard: its tick must publish no caption and retain no custody.
/// Undoing it must resume the caption on the exact endpoint.
fn refused(
    place: &Place,
    host: &Herdr,
    expected: &mut usize,
    what: &str,
    hazard: impl FnOnce(),
    undo: impl FnOnce(),
) {
    hazard();
    place.tick();
    assert_eq!(captions(host).len(), *expected, "refused {what}");
    assert_eq!(cache_count(place), 0, "refused {what}");
    undo();
    place.tick();
    *expected += 1;
    assert_eq!(captions(host).len(), *expected, "restored {what}");
}

#[test]
fn errand_captions_refuse_unsafe_endpoints_then_rebind() {
    let (place, host) = aboard();
    let id = start(&place, "thread:review");
    let original = host.panes.lock().unwrap()[1].clone();
    let mut expected = captions(&host).len();
    // The pane is no longer the bound one, or runs another agent.
    for (field, replacement) in [
        ("terminal_id", json!("foreign-terminal")),
        ("workspace_id", json!("foreign-workspace")),
        ("tab_id", json!("foreign-tab")),
        ("agent", json!("foreign-harness")),
    ] {
        refused(
            &place,
            &host,
            &mut expected,
            field,
            || host.panes.lock().unwrap()[1][field] = replacement,
            || host.panes.lock().unwrap()[1] = original.clone(),
        );
    }
    // The lease is not the launched one, has expired, or another presence
    // claims the same pane.
    for (what, hazard, undo) in [
        (
            "replacement holder",
            "UPDATE presence SET holder='host:33333333-3333-4333-8333-333333333333' WHERE ship='infra'",
            "UPDATE presence SET holder=(SELECT holder FROM host_launches WHERE ship='infra') WHERE ship='infra'",
        ),
        (
            "expired lease",
            "UPDATE presence SET expires_ms=-expires_ms WHERE ship='infra'",
            "UPDATE presence SET expires_ms=-expires_ms WHERE ship='infra'",
        ),
        (
            "second claim",
            "INSERT INTO presence(ship,holder,started_ms,expires_ms,binding) SELECT 'ambiguous','other',started_ms,expires_ms,binding FROM presence WHERE ship='infra'",
            "DELETE FROM presence WHERE ship='ambiguous'",
        ),
    ] {
        let run = |sql| place.database().execute(sql, []).unwrap();
        refused(
            &place,
            &host,
            &mut expected,
            what,
            || {
                run(hazard);
            },
            || {
                run(undo);
            },
        );
    }

    // A replaced socket incarnation is never sent a caption.
    let socket = place.root.join("config/herdr/herdr.sock");
    let old_socket = place.root.join("original.sock");
    fs::rename(&socket, &old_socket).unwrap();
    let replacement_host = Herdr::start(&socket, vec![]);
    place.tick();
    assert!(replacement_host.calls("pane.report_metadata").is_empty());
    assert_eq!(cache_count(&place), 0);
    fs::rename(&socket, place.root.join("replacement.sock")).unwrap();
    fs::rename(old_socket, &socket).unwrap();

    // A closed pane drops custody without a clear; a fresh binding to a fresh
    // pane gets the caption.
    host.panes.lock().unwrap().remove(1);
    place.tick();
    assert_eq!(captions(&host).len(), expected);
    assert_eq!(cache_count(&place), 0);
    let fresh = json!({"pane_id":"w3:p1","terminal_id":"fresh-terminal","workspace_id":"w3","tab_id":"w3:t1","agent":"pi","agent_status":"working"});
    host.panes.lock().unwrap().push(fresh.clone());
    let binding: String = place
        .database()
        .query_row("SELECT binding FROM presence WHERE ship='infra'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut binding: Value = serde_json::from_str(&binding).unwrap();
    binding["pane"] = fresh;
    place
        .database()
        .execute(
            "UPDATE presence SET binding=?1,started_ms=started_ms+1 WHERE ship='infra'",
            [binding.to_string()],
        )
        .unwrap();
    place.tick();
    assert_eq!(captions(&host).len(), expected + 1);
    assert_eq!(caption(&host, "w3:p1"), "infra · reviewing review");
    finish(&place, &id);
    assert!(caption(&host, "w3:p1").is_null());
    place.stop_native("infra");
}

#[test]
fn an_unconfirmed_caption_is_sent_again_each_pass_and_never_breaks_the_errand() {
    let (place, host) = aboard();
    host.reject_caption.store(true, Ordering::SeqCst);
    let id = start(&place, "thread:retry");
    assert_eq!(captions(&host).len(), 1);
    place.tick();
    assert_eq!(captions(&host).len(), 2);
    host.reject_caption.store(false, Ordering::SeqCst);
    place.tick();
    assert_eq!(captions(&host).len(), 3);
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing retry");
    place.tick();
    assert_eq!(
        captions(&host).len(),
        3,
        "a confirmed caption is not resent"
    );

    // An unconfirmed clear is retried as a clear, never the older caption.
    host.reject_caption.store(true, Ordering::SeqCst);
    finish(&place, &id);
    place.tick();
    let sent = captions(&host);
    assert_eq!(sent.len(), 5);
    assert!(
        sent[3..]
            .iter()
            .all(|r| r["params"]["clear_display_agent"] == true)
    );
    host.reject_caption.store(false, Ordering::SeqCst);
    place.tick();
    assert_eq!(captions(&host).len(), 6);
    assert_eq!(cache_count(&place), 0);
    place.tick();
    assert_eq!(captions(&host).len(), 6);
    place.stop_native("infra");
}

#[test]
fn a_hosted_claude_first_mate_shows_errand_captions() {
    let place = Place::new();
    fs::copy(place.root.join("bin/pi"), place.root.join("bin/claude")).unwrap();
    let mut first_mate = place.json("ship.show", json!({"name":"infra"}))["first_mate"].take();
    first_mate["manifest"] = json!("bundled:claude");
    first_mate["options"] = json!({"model":"claude-opus-5-5"});
    place.json("ship.put", json!({"name":"infra","first_mate":first_mate}));
    let trust =
        json!({"projects": {place.folder.to_str().unwrap(): {"hasTrustDialogAccepted": true}}});
    fs::write(place.root.join(".claude.json"), trust.to_string()).unwrap();
    let (place, host) = host(place);
    // Herdr names a Claude Code pane's agent `claude`.
    host.panes.lock().unwrap()[1]["agent"] = json!("claude");
    let id = start(&place, "thread:review");
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing review");
    // The fake writes no Claude result, so the errand fails; its caption clears.
    fs::write(place.root.join(format!("{id}-release")), "").unwrap();
    until("the Claude errand ends", || {
        place.agent("job.status", json!({"id":id}))["status"] != "running"
    });
    place.tick();
    assert!(caption(&host, "w2:p1").is_null());
    place.stop_native("infra");
}
