//! Real headless errand subprocesses, not invented busy flags. The fake host
//! models source-local display patches, never lifecycle authority.
use super::*;

fn aboard() -> (Place, Herdr) {
    let place = Place::new();
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
    host.metadata.lock().unwrap().insert(
        ("w2:p1".into(), "privateer".into()),
        json!({"title":"infra first mate", "display_agent":"infra", "tokens":{"other":"kept"}}),
    );
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
    let job = place.agent("job.status", json!({"id":id}));
    println!(
        "headless payload {pid} exited; job {id} status={} incarnation={}",
        job["status"], job["incarnation"]
    );
    if let Some(evidence) = std::env::var_os("ERRAND_LABELS_EVIDENCE") {
        let incarnation = job["incarnation"].as_str().unwrap();
        let dest = PathBuf::from(evidence).join(id).join(incarnation);
        fs::create_dir_all(&dest).unwrap();
        let logs = place
            .folder
            .join(".privateer/jobs")
            .join(id)
            .join(incarnation);
        for entry in fs::read_dir(logs).unwrap().flatten() {
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
            }
        }
        fs::write(
            dest.join("job.json"),
            serde_json::to_vec_pretty(&job).unwrap(),
        )
        .unwrap();
        fs::write(
            dest.join("lifecycle.json"),
            json!({"payload_pid":pid,"payload_exited":true,"job_status":job["status"]}).to_string(),
        )
        .unwrap();
    }
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

fn caption(host: &Herdr, pane: &str) -> Value {
    host.metadata
        .lock()
        .unwrap()
        .get(&(pane.into(), "privateer-errands".into()))
        .cloned()
        .unwrap_or(json!({}))["display_agent"]
        .clone()
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
    let normal = host
        .metadata
        .lock()
        .unwrap()
        .get(&("w2:p1".into(), "privateer".into()))
        .unwrap()
        .clone();
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
        host.metadata
            .lock()
            .unwrap()
            .get(&("w2:p1".into(), "privateer".into())),
        Some(&normal)
    );
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

#[test]
fn errand_captions_refuse_replaced_ambiguous_expired_and_pinned_foreign_endpoints_then_rebind() {
    let (place, host) = aboard();
    let id = start(&place, "thread:review");
    let binding: String = place
        .database()
        .query_row("SELECT binding FROM presence WHERE ship='infra'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let original = host.panes.lock().unwrap()[1].clone();
    let mut expected = captions(&host).len();
    for (field, replacement) in [
        ("terminal_id", json!("foreign-terminal")),
        ("workspace_id", json!("foreign-workspace")),
        ("tab_id", json!("foreign-tab")),
        ("agent", json!("foreign-harness")),
    ] {
        host.panes.lock().unwrap()[1][field] = replacement;
        place.tick();
        assert_eq!(captions(&host).len(), expected, "refuse changed {field}");
        assert_eq!(cache_count(&place), 0);
        host.panes.lock().unwrap()[1] = original.clone();
        place.tick();
        expected += 1;
        assert_eq!(captions(&host).len(), expected);
    }
    place
        .database()
        .execute(
            "UPDATE presence SET binding='malformed' WHERE ship='infra'",
            [],
        )
        .unwrap();
    place.tick();
    assert_eq!(captions(&host).len(), expected, "malformed binding");
    assert_eq!(cache_count(&place), 0);
    place
        .database()
        .execute(
            "UPDATE presence SET binding=?1 WHERE ship='infra'",
            [&binding],
        )
        .unwrap();
    place.tick();
    expected += 1;
    let holder: String = place
        .database()
        .query_row("SELECT holder FROM presence WHERE ship='infra'", [], |r| {
            r.get(0)
        })
        .unwrap();
    place.database().execute("UPDATE presence SET holder='host:33333333-3333-4333-8333-333333333333' WHERE ship='infra'", []).unwrap();
    place.tick();
    assert_eq!(
        captions(&host).len(),
        expected,
        "replacement holder lacks matching launch custody"
    );
    assert_eq!(cache_count(&place), 0);
    place
        .database()
        .execute("UPDATE presence SET holder=?1 WHERE ship='infra'", [holder])
        .unwrap();
    place.tick();
    expected += 1;
    place.database().execute("INSERT INTO presence(ship,holder,started_ms,expires_ms,binding) SELECT 'ambiguous','other',started_ms,expires_ms,binding FROM presence WHERE ship='infra'", []).unwrap();
    place.tick();
    assert_eq!(
        captions(&host).len(),
        expected,
        "second binding claiming endpoint"
    );
    assert_eq!(cache_count(&place), 0);
    place
        .database()
        .execute("DELETE FROM presence WHERE ship='ambiguous'", [])
        .unwrap();
    place.tick();
    expected += 1;

    let socket = place.root.join("config/herdr/herdr.sock");
    let old_socket = place.root.join("original.sock");
    fs::rename(&socket, &old_socket).unwrap();
    let replacement_host = Herdr::start(&socket, vec![]);
    // Direct settlement avoids unrelated sidebar/host reconciliation calls.
    let mut store = privateer::store::Store::open(&place.folder.join(".privateer")).unwrap();
    privateer::errand::settle(&mut store).unwrap();
    assert!(
        replacement_host.requests.lock().unwrap().is_empty(),
        "wrong socket incarnation never contacted"
    );
    assert_eq!(captions(&host).len(), expected);
    assert_eq!(cache_count(&place), 0);
    fs::rename(&socket, place.root.join("replacement.sock")).unwrap();
    fs::rename(old_socket, &socket).unwrap();
    place.tick();
    expected += 1;

    host.panes.lock().unwrap().push(original.clone());
    place.tick();
    assert_eq!(captions(&host).len(), expected, "duplicate pane/terminal");
    host.panes.lock().unwrap().pop();
    // This fixture still has mail for the hosted mate, so a normal maintenance
    // tick correctly skips. Settlement also runs the ordinary caption
    // reconciler; the publisher may drop stale custody without an immediate
    // replay, so use its observed count as the baseline for the pinned checks.
    privateer::errand::settle(&mut store).unwrap();
    expected = captions(&host).len();

    let mut pinned: Value = serde_json::from_str(&binding).unwrap();
    pinned["pane"]["agent_session"] = json!({"id":"pinned"});
    place
        .database()
        .execute(
            "UPDATE presence SET binding=?1 WHERE ship='infra'",
            [pinned.to_string()],
        )
        .unwrap();
    host.panes.lock().unwrap()[1]["agent_session"] = json!({"id":"not-pinned"});
    privateer::errand::settle(&mut store).unwrap();
    assert_eq!(captions(&host).len(), expected, "pinned session mismatch");
    assert_eq!(cache_count(&place), 0);
    host.panes.lock().unwrap()[1]["agent_session"] = json!({"id":"pinned"});
    privateer::errand::settle(&mut store).unwrap();
    expected += 1;
    assert_eq!(captions(&host).len(), expected);

    let expiry: i64 = place
        .database()
        .query_row(
            "SELECT expires_ms FROM presence WHERE ship='infra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    place
        .database()
        .execute("UPDATE presence SET expires_ms=0", [])
        .unwrap();
    // Reconcile directly through errand settlement in a real dispatch call,
    // without a dispatcher legitimately attempting a new hosted launch.
    let mut store = privateer::store::Store::open(&place.folder.join(".privateer")).unwrap();
    privateer::errand::settle(&mut store).unwrap();
    assert_eq!(
        captions(&host).len(),
        expected,
        "expired lease cannot authorize a mutation"
    );
    assert_eq!(cache_count(&place), 0);
    place
        .database()
        .execute("UPDATE presence SET expires_ms=?1", [expiry])
        .unwrap();

    // Pane closure drops old presentation custody. A fresh binding to a fresh
    // exact terminal reconciles, never sends a clear to the reused old pane.
    host.panes.lock().unwrap().remove(1);
    place.tick();
    assert_eq!(captions(&host).len(), expected);
    assert_eq!(cache_count(&place), 0);
    let fresh = json!({"pane_id":"w3:p1","terminal_id":"fresh-terminal","workspace_id":"w3","tab_id":"w3:t1","agent":"pi","agent_status":"working","agent_session":{"id":"fresh"}});
    host.panes.lock().unwrap().push(fresh.clone());
    pinned["pane"] = fresh;
    place
        .database()
        .execute(
            "UPDATE presence SET binding=?1,started_ms=started_ms+1 WHERE ship='infra'",
            [pinned.to_string()],
        )
        .unwrap();
    place.tick();
    expected += 1;
    assert_eq!(captions(&host).len(), expected);
    assert_eq!(caption(&host, "w3:p1"), "infra · reviewing review");
    assert_eq!(
        captions(&host).last().unwrap()["params"]["pane_id"],
        "w3:p1"
    );
    finish(&place, &id);
    assert!(caption(&host, "w3:p1").is_null());
    place.stop_native("infra");
}

#[test]
fn errand_metadata_refusal_is_bounded_retried_and_never_breaks_lifecycle_or_replays_after_settle() {
    let (place, host) = aboard();
    host.reject_caption.store(true, Ordering::SeqCst);
    let id = start(&place, "thread:retry");
    assert_eq!(captions(&host).len(), 1);
    assert!(caption(&host, "w2:p1").is_null());
    for _ in 0..3 {
        place.tick();
    }
    assert_eq!(
        captions(&host).len(),
        1,
        "failed publication is not acked or flooded"
    );
    let record: String = place
        .database()
        .query_row("SELECT record FROM errand_captions", [], |r| r.get(0))
        .unwrap();
    let record: Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["dirty"], true);
    assert!(record["acknowledged"].is_null());
    let terminal = host.panes.lock().unwrap()[1]["terminal_id"].clone();
    host.panes.lock().unwrap()[1]["terminal_id"] = json!("reused-during-retry");
    place.tick();
    assert_eq!(captions(&host).len(), 1);
    assert_eq!(
        cache_count(&place),
        0,
        "retry backoff must not retain stale endpoint custody"
    );
    host.panes.lock().unwrap()[1]["terminal_id"] = terminal;
    place.tick();
    assert_eq!(
        captions(&host).len(),
        2,
        "restored exact endpoint gets fresh presentation intent"
    );
    host.reject_caption.store(false, Ordering::SeqCst);
    place
        .database()
        .execute(
            "UPDATE errand_captions SET record=json_set(record,'$.retry_ms',0)",
            [],
        )
        .unwrap();
    place.tick();
    assert_eq!(captions(&host).len(), 3);
    assert_eq!(caption(&host, "w2:p1"), "infra · reviewing retry");
    // A failed clear is not an acknowledgement, and never retries the older
    // active caption, including after fresh dispatcher processes.
    host.reject_caption.store(true, Ordering::SeqCst);
    finish(&place, &id);
    assert_eq!(captions(&host).len(), 4);
    assert_eq!(captions(&host)[3]["params"]["clear_display_agent"], true);
    place.tick();
    assert_eq!(captions(&host).len(), 4);
    host.reject_caption.store(false, Ordering::SeqCst);
    place
        .database()
        .execute(
            "UPDATE errand_captions SET record=json_set(record,'$.retry_ms',0)",
            [],
        )
        .unwrap();
    place.tick();
    assert_eq!(captions(&host).len(), 5);
    assert!(caption(&host, "w2:p1").is_null());
    assert_eq!(cache_count(&place), 0);
    assert!(host.calls("pane.report_agent").is_empty());
    place.stop_native("infra");
}
