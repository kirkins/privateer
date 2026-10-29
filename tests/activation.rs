mod support;

use privateer::dispatch;
use serde_json::{Value, json};
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use support::{Fleet, now_settings};

fn native_routes(fleet: &Fleet, hosted: bool) {
    fs::write(
        fleet.home.join("config.json"),
        json!({"resource_home":fleet.root.join("ledger"),"ship":"infra"}).to_string(),
    )
    .unwrap();
    let mut manifests = Vec::new();
    for harness in ["own", "fallback"] {
        let directory = fleet.root.join(harness);
        fs::create_dir_all(&directory).unwrap();
        let manifest = directory.join("manifest.json");
        fs::write(
            &manifest,
            json!({"schema_version":1,"id":harness,"executable":[std::env::var("PYTHON").unwrap_or_else(|_| support::python().display().to_string()),fleet.root.join("adapter.py")]}).to_string(),
        ).unwrap();
        manifests.push(manifest);
    }
    let base_options = json!({"mate":fleet.root.join("mate.py"),"mode":fleet.root.join("mode"),"log":fleet.root.join("prompts.log")});
    let mut profile = fleet.ok("ship.show", json!({"name":"infra"}), None)["first_mate"].clone();
    profile["manifest"] = json!(manifests[0]);
    profile["options"] = base_options.clone();
    profile["options"]["model"] = json!("own-model");
    let mut spent_options = base_options.clone();
    spent_options["model"] = json!("spent-model");
    let mut fallback_options = base_options;
    fallback_options["model"] = json!("fallback-model");
    if hosted {
        profile["host"] = json!("herdr");
    }
    fleet.ok(
        "ship.put",
        json!({"name":"infra","first_mate":profile}),
        None,
    );
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "spent": {"manifest":manifests[1],"options":spent_options,"cost":"plan"},
                "fallback": {"manifest":manifests[1],"options":fallback_options,"cost":"plan"}
            },
            "first_mate": ["spent", "fallback"]
        }),
        None,
    );
}

fn mate_options(fleet: &Fleet, model: &str) -> Value {
    let mut options = json!({"mate":fleet.root.join("mate.py"),"mode":fleet.root.join("mode"),
        "log":fleet.root.join("prompts.log")});
    options["model"] = json!(model);
    options
}

fn native_routes_with_explicit_fallback(fleet: &Fleet, automatic_fallback: bool) {
    native_routes(fleet, false);
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "spent": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(fleet, "spent-model"),"cost":"plan",
                    "explicit_only":true},
                "fallback": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(fleet, "fallback-model"),"cost":"plan",
                    "explicit_only":!automatic_fallback}
            },
            "first_mate": ["spent", "fallback"]
        }),
        None,
    );
}

/// The same fleet with the first mate's own route named in the routes file and
/// a run budget on its window: these harnesses report no usage of their own, so
/// the runs Privateer counts are the only thing it can measure them by.
fn native_routes_with_budget(fleet: &Fleet, runs: u64) {
    native_routes(fleet, false);
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "own": {"manifest":fleet.root.join("own/manifest.json"),
                    "options":mate_options(fleet, "own-model"),"cost":"plan",
                    "allowance":{"window_hours":5,"runs":runs},"tier":1},
                "spent": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(fleet, "spent-model"),"cost":"plan"},
                "fallback": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(fleet, "fallback-model"),"cost":"plan"}
            },
            "first_mate": ["own", "spent", "fallback"]
        }),
        None,
    );
}

/// The runs a route's window has spent, in the shared ledger.
fn spend_runs(fleet: &Fleet, route: &str, runs: i64, window_ms: i64) {
    fs::create_dir_all(fleet.root.join("ledger")).unwrap();
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    privateer::routes::init(&ledger).unwrap();
    let now = privateer::store::Store::now_ms();
    ledger
        .execute(
            "INSERT OR REPLACE INTO route_usage(route, runs, runs_window_ends_ms, updated_ms)
             VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![route, runs, now + window_ms, now],
        )
        .unwrap();
}

fn back_off(fleet: &Fleet, route: &str) {
    fs::create_dir_all(fleet.root.join("ledger")).unwrap();
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    ledger.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_routes(route TEXT PRIMARY KEY,failures INTEGER NOT NULL,until_ms INTEGER NOT NULL,last_error TEXT,updated_ms INTEGER NOT NULL)",
    ).unwrap();
    ledger.execute(
        "INSERT INTO model_routes(route,failures,until_ms,last_error,updated_ms) VALUES(?1,1,?2,'429',0)",
        rusqlite::params![route, i64::MAX / 2],
    ).unwrap();
}

fn wake_model(fleet: &mut Fleet) -> String {
    let wake = fleet.wakes()[0].clone();
    let job = dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
    let argv = job["command"].as_array().unwrap();
    argv.iter()
        .filter_map(Value::as_str)
        .find(|arg| ["own-model", "spent-model", "fallback-model"].contains(arg))
        .expect("selected model in plugin argv")
        .to_owned()
}

#[test]
fn a_backed_off_first_mate_uses_its_first_clear_fallback_for_a_headless_wake() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    back_off(&fleet, "own/-/own-model");
    back_off(&fleet, "fallback/-/spent-model");
    fleet.send(json!({"id":"fallback","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    // The route it passed over is named, with why.
    assert_eq!(
        report["route"]["why"],
        "first available on infra's first_mate list: fallback (no requirement); \
         passed over spent: backing off or past its first-mate share",
        "{report}"
    );
    assert_eq!(wake_model(&mut fleet), "fallback-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
}

#[test]
fn a_backed_off_first_mate_never_automatically_launches_an_explicit_only_route() {
    for automatic_fallback in [true, false] {
        let mut fleet = Fleet::new(now_settings());
        native_routes_with_explicit_fallback(&fleet, automatic_fallback);
        back_off(&fleet, "own/-/own-model");
        fleet.send(json!({"id":"waiting","from":"apps","to":"infra",
            "kind":"request","body":"Please handle this."}));
        let report = Fleet::infra(&fleet.tick());
        if automatic_fallback {
            assert_eq!(report["status"], "running", "{report}");
            assert_eq!(report["route"]["name"], "fallback", "{report}");
            assert_eq!(wake_model(&mut fleet), "fallback-model");
            assert_eq!(fleet.settle()["status"], "succeeded");
        } else {
            assert!(report["skipped"].is_string(), "{report}");
            assert!(fleet.wakes().is_empty(), "{report}");
            assert!(fleet.prompts().is_empty());
            assert_eq!(
                dispatch(&mut fleet.store, "mail.show", json!({"id":"waiting"})).unwrap()["state"],
                "open"
            );
        }
    }
}

/// A first mate keeps its own route while its plan has room for it, and moves
/// to the captain's first_mate order in a fresh session once the plan is past
/// the share it keeps for itself. Nothing is stopped: only the next wake moves.
#[test]
fn a_first_mate_moves_to_its_fallback_before_its_own_route_is_spent() {
    let mut fleet = Fleet::new(now_settings());
    native_routes_with_budget(&fleet, 20);
    let wake_for = |fleet: &mut Fleet, id: &str| {
        fleet.send(
            json!({"id":id,"from":"apps","to":"infra","kind":"request","body":"Please handle this."}),
        );
        Fleet::infra(&fleet.tick())
    };
    // Nothing spent: the first mate wakes on its own route.
    assert_eq!(wake_for(&mut fleet, "first")["status"], "running");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");

    // Four fifths of the window is spent: past what crew are refused at, and
    // the first mate keeps its reserve.
    spend_runs(&fleet, "own/-/own-model", 16, 5 * 3_600_000);
    assert_eq!(wake_for(&mut fleet, "second")["status"], "running");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");

    // Past nineteen twentieths the next wake moves to the fallback, in a fresh
    // session: a resumed session belongs to the route it was warmed on.
    spend_runs(&fleet, "own/-/own-model", 20, 5 * 3_600_000);
    let moved = wake_for(&mut fleet, "third");
    assert_eq!(moved["status"], "running", "{moved}");
    assert_eq!(moved["mode"], "fresh", "{moved}");
    assert_eq!(wake_model(&mut fleet), "spent-model");
    assert_eq!(fleet.settle()["status"], "succeeded");

    // The plan renews, and the route is used again.
    spend_runs(&fleet, "own/-/own-model", 0, 5 * 3_600_000);
    assert_eq!(wake_for(&mut fleet, "fourth")["status"], "running");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
}

/// Tick until the ship reports a wake that `wanted` accepts, and return that
/// report. A retry is decided in the same pass that settles the attempt it
/// replaces, so a test watches the reports rather than the calls.
fn next_wake(fleet: &mut Fleet, wanted: impl Fn(&Value) -> bool) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    let mut seen = vec![];
    loop {
        let report = Fleet::infra(&fleet.tick());
        if report["status"] == "running" && wanted(&report) {
            return report;
        }
        seen.push(report.clone());
        assert!(Instant::now() < until, "no such wake: {seen:#?}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// The first prompt that says `needle`, waiting for the harness to write it:
/// a wake is recorded before its job is, so a report of a running wake does
/// not mean the harness has read its brief yet. With its place in the log, so
/// the prompt after it can be found.
fn prompt_with(fleet: &Fleet, needle: &str) -> (usize, String) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let prompts = fleet.prompts();
        if let Some(at) = prompts.iter().position(|prompt| prompt.contains(needle)) {
            return (at, prompts[at].to_owned());
        }
        assert!(
            Instant::now() < until,
            "no prompt saying {needle}: {prompts:#?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// The prompt after the first `from` prompts in the log.
fn next_prompt(fleet: &Fleet, from: usize) -> String {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let prompts = fleet.prompts();
        if prompts.len() > from {
            return prompts[from].to_owned();
        }
        assert!(
            Instant::now() < until,
            "no prompt after {from}: {prompts:#?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// The mail `id` once it is answered, waiting for the wake that answers it: a
/// wake is reported running before its mate has run, so its reply can land
/// after the report. Returns the mail as it stands at the bound.
fn answered_within(fleet: &mut Fleet, id: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let mail = dispatch(&mut fleet.store, "mail.show", json!({"id": id})).unwrap();
        if mail["state"] == "answered" || Instant::now() >= until {
            return mail;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// A wake the provider refused is retried once, at once, on the fallback,
/// without the mail that attempt already answered. Both waits are long enough
/// that only the retry may pass them.
#[test]
fn a_wake_the_provider_refused_is_retried_once_on_the_fallback_without_its_answered_mail() {
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600,"max_wakes_per_hour":1}),
    );
    native_routes(&fleet, false);
    fleet.send(
        json!({"id":"answer-me","from":"apps","to":"infra","kind":"request","body":"the answered one"}),
    );
    fleet.send(
        json!({"id":"still-open","from":"apps","to":"infra","kind":"request","body":"the open one"}),
    );
    // The attempt answers the first request, then the provider ends the run.
    fleet.mode("half-throttle");
    fleet.tick();
    let failed = fleet.wakes()[0].clone();
    assert_eq!(wake_model(&mut fleet), "own-model");

    // The next pass settles it and retries at once, on the fallback rather
    // than the route that just failed, outside both waits.
    let retry = next_wake(&mut fleet, |report| report["retry_of"].is_string());
    assert_eq!(retry["retry_of"], failed["id"].as_str().unwrap(), "{retry}");
    assert_eq!(wake_model(&mut fleet), "spent-model");
    assert_eq!(
        dispatch(&mut fleet.store, "mail.show", json!({"id":"answer-me"})).unwrap()["state"],
        "answered"
    );
    let (_, prompt) = prompt_with(
        &fleet,
        &format!("one retry of wake {}", failed["id"].as_str().unwrap()),
    );
    assert!(prompt.contains("It has already answered"), "{prompt}");
    assert!(
        prompt.contains("It has already answered: answer-me."),
        "{prompt}"
    );
    // The request that attempt answered is not in the mail the retry reads.
    assert!(!prompt.contains("the answered one"), "{prompt}");
    assert!(prompt.contains("the open one"), "{prompt}");
    // It records why: a retry, of the mail it still has to answer.
    let retried = fleet
        .wakes()
        .into_iter()
        .find(|w| w["id"] == retry["wake"])
        .unwrap();
    assert_eq!(retried["reason"], "retry", "{retried}");
    assert_eq!(retried["cause"]["causes"][0]["mail_id"], "still-open");
    assert_eq!(retried["cause"]["causes"].as_array().unwrap().len(), 1);
    // It answers the open one itself, and the provider refuses it again.
    assert_eq!(fleet.settle()["status"], "failed");
    assert_eq!(
        dispatch(&mut fleet.store, "mail.show", json!({"id":"still-open"})).unwrap()["state"],
        "answered"
    );
}

#[test]
fn a_provider_retry_skips_explicit_only_routes_and_waits_when_none_are_automatic() {
    for automatic_fallback in [true, false] {
        let mut fleet = Fleet::new(json!({
            "batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600,"max_wakes_per_hour":1
        }));
        native_routes_with_explicit_fallback(&fleet, automatic_fallback);
        for id in ["answer-me", "still-open"] {
            fleet.send(json!({"id":id,"from":"apps","to":"infra",
                "kind":"request","body":format!("request {id}")}));
        }
        fleet.mode("half-throttle");
        fleet.tick();
        let failed = fleet.wakes()[0].clone();
        assert_eq!(wake_model(&mut fleet), "own-model");

        if automatic_fallback {
            let retry = next_wake(&mut fleet, |report| report["retry_of"].is_string());
            assert_eq!(retry["retry_of"], failed["id"], "{retry}");
            assert_eq!(retry["route"]["name"], "fallback", "{retry}");
            assert_eq!(wake_model(&mut fleet), "fallback-model");
            assert_eq!(fleet.settle()["status"], "failed");
            assert_eq!(fleet.wakes().len(), 2);
        } else {
            let settled = fleet.settle();
            assert_eq!(settled["id"], failed["id"], "{settled}");
            assert_eq!(settled["status"], "failed", "{settled}");
            let report = Fleet::infra(&fleet.tick());
            assert!(report["skipped"].is_string(), "{report}");
            assert!(report["retry_of"].is_null(), "{report}");
            assert_eq!(fleet.wakes().len(), 1);
            assert_eq!(fleet.prompts().len(), 1);
            assert_eq!(
                dispatch(&mut fleet.store, "mail.show", json!({"id":"still-open"})).unwrap()["state"],
                "open"
            );
        }
    }
}

/// The retry is once: a wake that is itself a retry is never retried again, and
/// the wake after it is an ordinary one on the route its profile names.
#[test]
fn a_retried_wake_is_not_retried_again_and_the_next_wake_waits() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    for id in ["one", "two", "three"] {
        fleet.send(
            json!({"id":id,"from":"apps","to":"infra","kind":"request","body":format!("request {id}")}),
        );
    }
    // Every attempt answers one request and is then refused by the provider.
    fleet.mode("half-throttle");
    fleet.tick();
    let first = fleet.wakes()[0].clone();
    let retry = next_wake(&mut fleet, |report| report["retry_of"].is_string());
    assert_eq!(retry["retry_of"], first["id"].as_str().unwrap(), "{retry}");
    assert_eq!(wake_model(&mut fleet), "spent-model");

    // The retry failed too, so the wake after it is not a retry of it: it is
    // an ordinary one, which its own failed route being held off may move to
    // any clear route in the captain's list.
    let (retry_prompt, _) = prompt_with(&fleet, "one retry of wake");
    let next = next_wake(&mut fleet, |report| report["retry_of"].is_null());
    assert_ne!(next["wake"], retry["wake"]);
    let prompt = next_prompt(&fleet, retry_prompt + 1);
    assert!(!prompt.contains("one retry of wake"), "{prompt}");
    // Each attempt answered one request, so nothing is asked twice. The next
    // wake runs in its own process and answers after it is reported running.
    for id in ["one", "two", "three"] {
        let answer = answered_within(&mut fleet, id);
        assert_eq!(answer["state"], "answered", "{id}: {answer}");
    }
}

/// A failed wake whose job record was pruned cannot read its log, and is
/// treated as not retryable rather than failing the tick.
#[test]
fn a_failed_wake_whose_job_was_pruned_is_not_retryable() {
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600,"max_wakes_per_hour":1}),
    );
    native_routes(&fleet, false);
    fleet.send(json!({"id":"req-1","from":"apps","to":"infra","kind":"request","body":"work"}));
    fleet.mode("half-throttle");
    fleet.tick();
    let failed = fleet.wakes()[0].clone();
    let job_id = failed["job_id"].as_str().unwrap();

    // Prune the job record from the jobs table.
    fleet
        .store
        .connection()
        .execute("DELETE FROM jobs WHERE id=?1", [job_id])
        .unwrap();

    // The next tick must not error; it simply treats the wake as not retryable.
    let reports = fleet.tick();
    let retried = reports["ships"]
        .as_array()
        .unwrap()
        .iter()
        .any(|ship| ship["retry_of"].is_string());
    assert!(!retried, "pruned job must not be retried: {reports:?}");
}

/// A retry skips the route that FAILED, not just the ship's own configured route.
#[test]
fn a_retry_skips_the_route_that_failed_not_just_the_ships_own_route() {
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600,"max_wakes_per_hour":1}),
    );
    native_routes(&fleet, false);
    // Back off the ship's own route beforehand, so the wake starts on the fallback ("spent").
    back_off(&fleet, "own/-/own-model");
    fleet.send(
        json!({"id":"req-2","from":"apps","to":"infra","kind":"request","body":"work on fallback"}),
    );
    // A second request the failed attempt does not answer, so there is mail to retry:
    fleet.send(
        json!({"id":"req-3","from":"apps","to":"infra","kind":"request","body":"still open"}),
    );
    // "spent" fails with half-throttle:
    fleet.mode("half-throttle");
    fleet.tick();
    assert_eq!(wake_model(&mut fleet), "spent-model");

    // The retry must skip "spent" (the route that failed) and pick the next fallback ("fallback-model"):
    let retry = next_wake(&mut fleet, |report| report["retry_of"].is_string());
    assert!(retry["retry_of"].is_string());
    assert_eq!(wake_model(&mut fleet), "fallback-model");
}

#[test]
fn a_cleared_first_mate_route_is_used_again_for_the_next_wake() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    back_off(&fleet, "own/-/own-model");
    fleet.send(json!({"id":"fallback-first","from":"apps","to":"infra","kind":"request","body":"Run while own route is backed off."}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(wake_model(&mut fleet), "spent-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
    rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite"))
        .unwrap()
        .execute(
            "UPDATE model_routes SET until_ms=0 WHERE route='own/-/own-model'",
            [],
        )
        .unwrap();
    fleet.send(json!({"id":"own-route","from":"apps","to":"infra","kind":"request","body":"Use the configured route again."}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
}

#[test]
fn a_provider_pinned_charter_waits_instead_of_changing_first_mate_route() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","objective":"Answer mail","repo":fleet.root.join("repo"),
            "min_free_bytes":0,"job_timeout_secs":60,
            "provider_plugin":{"manifest":fleet.root.join("own/manifest.json")}}),
        None,
    );
    back_off(&fleet, "own/-/own-model");
    fleet.send(json!({"id":"pinned","from":"apps","to":"infra","kind":"request","body":"Keep the pinned provider."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(
        report["skipped"], "model route backing off or past its headroom",
        "{report}"
    );
    assert!(fleet.wakes().is_empty());
}

#[test]
fn a_hosted_first_mate_on_a_backed_off_route_falls_back_headlessly_without_relaunch() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, true);
    back_off(&fleet, "own/-/own-model");
    fleet.send(json!({"id":"host-fallback","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert!(report.get("hosted").is_none(), "{report}");
    assert_eq!(wake_model(&mut fleet), "spent-model");
    let launches: i64 = fleet
        .store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM host_launches WHERE ship='infra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        launches, 0,
        "hosted route must not be relaunched while backed off"
    );
    assert_eq!(fleet.settle()["status"], "succeeded");
    let launches_after_settle: i64 = fleet
        .store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM host_launches WHERE ship='infra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        launches_after_settle, 0,
        "hosting stays off while backed off"
    );
    rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite"))
        .unwrap()
        .execute(
            "UPDATE model_routes SET until_ms=0 WHERE route='own/-/own-model'",
            [],
        )
        .unwrap();
    fleet.send(json!({"id":"host-again","from":"apps","to":"infra","kind":"request","body":"Host again on the own route."}));
    // This fixture's adapter cannot host, so the relaunch is refused, but it
    // is attempted: the own route is clear again and the wake uses it.
    let report = Fleet::infra(&fleet.tick());
    assert!(report["host_error"].is_string(), "{report}");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
}

/// The ship's first-mate profile with no model of its own, as `init` leaves
/// it: the own manifest and the fixture's harness settings.
fn modelless_profile(fleet: &Fleet) {
    let mut profile = fleet.ok("ship.show", json!({"name":"infra"}), None)["first_mate"].clone();
    profile["options"]
        .as_object_mut()
        .unwrap()
        .remove("model")
        .unwrap();
    fleet.ok(
        "ship.put",
        json!({"name":"infra","first_mate":profile}),
        None,
    );
}

/// The model argument the latest wake's job runs with, and the id of the
/// manifest it launched, as the wake row records its harness.
fn wake_job(fleet: &mut Fleet) -> (Option<String>, String) {
    let wake = fleet.wakes()[0].clone();
    let job = dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
    let model = job["command"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|arg| {
            [
                "own-model",
                "spent-model",
                "fallback-model",
                "missing-model",
            ]
            .contains(arg)
        })
        .map(str::to_owned);
    let harness: String = fleet
        .store
        .connection()
        .query_row(
            "SELECT harness FROM wakes WHERE id=?1",
            [wake["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    (model, harness)
}

/// The route the latest wake row and its `wake.started` record name.
fn recorded_route(fleet: &mut Fleet) -> (Value, Value) {
    let raw: Option<String> = fleet
        .store
        .connection()
        .query_row(
            "SELECT route FROM wakes ORDER BY created_ms DESC, rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let row = raw.map_or(Value::Null, |raw| serde_json::from_str(&raw).unwrap());
    let started = wakes_started(fleet).last().unwrap()["route"].clone();
    (row, started)
}

/// Captain finding 22: a first mate whose profile names no model, as `init`
/// leaves one, wakes on the first route of the fleet's first_mate list, with
/// that route's manifest and options, and the wake records which and why. A
/// profile model set later wins again, unchanged.
#[test]
fn a_first_mate_with_no_model_wakes_on_the_first_listed_route_and_records_it() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    modelless_profile(&fleet);
    fleet.send(json!({"id":"listed","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert_eq!(report["route"]["name"], "spent", "{report}");
    assert_eq!(
        report["route"]["why"],
        "first available on infra's first_mate list: spent (the profile names no model)"
    );
    assert_eq!(report["route"]["identity"], "fallback/-/spent-model");
    let (model, harness) = wake_job(&mut fleet);
    assert_eq!(model.as_deref(), Some("spent-model"));
    assert_eq!(harness, "fallback");
    assert_eq!(fleet.settle()["status"], "succeeded");
    let (row, started) = recorded_route(&mut fleet);
    assert_eq!(row["name"], "spent", "{row}");
    assert_eq!(started, row);
    // The profile is never rewritten: the route is chosen at each wake.
    let profile = fleet.ok("ship.show", json!({"name":"infra"}), None)["first_mate"].clone();
    assert!(profile["options"].get("model").is_none(), "{profile}");
    // Bearings say which route the next launch takes.
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("First mate route: spent (fallback/-/spent-model); first available"),
        "{bearings}"
    );

    // An explicit model in the profile is the ship's own route again.
    let mut profile = profile;
    profile["options"]["model"] = json!("own-model");
    fleet.ok(
        "ship.put",
        json!({"name":"infra","first_mate":profile}),
        None,
    );
    fleet.send(json!({"id":"explicit","from":"apps","to":"infra","kind":"request","body":"Use your own model."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert!(report["route"].is_null(), "{report}");
    assert_eq!(wake_model(&mut fleet), "own-model");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(recorded_route(&mut fleet), (Value::Null, Value::Null));
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        !bearings["text"]
            .as_str()
            .unwrap()
            .contains("First mate route:"),
        "{bearings}"
    );
}

/// With no first_mate list, a profile with no model runs as it always did:
/// on its own manifest, with no model chosen for it.
#[test]
fn a_first_mate_with_no_model_and_no_first_mate_list_runs_as_before() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    modelless_profile(&fleet);
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "spent": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(&fleet, "spent-model"),"cost":"plan"}
            },
            "crew": ["spent"]
        }),
        None,
    );
    fleet.send(json!({"id":"unlisted","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert!(report["route"].is_null(), "{report}");
    let (model, harness) = wake_job(&mut fleet);
    assert_eq!(model, None);
    assert_eq!(harness, "own");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(recorded_route(&mut fleet), (Value::Null, Value::Null));
}

/// A listed route backing off, or whose harness the last readiness check
/// found missing, is passed over for the next one, and the why says so; with
/// every route unavailable the first mate waits rather than run on its
/// harness's default.
#[test]
fn a_first_mate_with_no_model_passes_over_unavailable_listed_routes() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    modelless_profile(&fleet);
    let absent = fleet.root.join("absent");
    fs::create_dir_all(&absent).unwrap();
    fs::write(
        absent.join("manifest.json"),
        json!({"schema_version":1,"id":"absent","requires":["absent-tool"],
            "executable":[support::python().display().to_string(),fleet.root.join("adapter.py")]})
        .to_string(),
    )
    .unwrap();
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "missing": {"manifest":absent.join("manifest.json"),
                    "options":mate_options(&fleet, "missing-model"),"cost":"plan"},
                "spent": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(&fleet, "spent-model"),"cost":"plan"},
                "fallback": {"manifest":fleet.root.join("fallback/manifest.json"),
                    "options":mate_options(&fleet, "fallback-model"),"cost":"plan"}
            },
            "first_mate": ["missing", "spent", "fallback"]
        }),
        None,
    );
    fleet
        .store
        .connection()
        .execute(
            "INSERT OR REPLACE INTO machine_readiness(tool, level, ok, version, path, why,
               install, needed_by, fingerprint, checked_ms)
             VALUES('absent-tool', 'warn', 0, NULL, NULL, 'absent-tool is not on the PATH',
               'install absent-tool', '[]', 'test', ?1)",
            [privateer::store::Store::now_ms()],
        )
        .unwrap();
    back_off(&fleet, "fallback/-/spent-model");
    fleet.send(json!({"id":"passed-over","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert_eq!(report["route"]["name"], "fallback", "{report}");
    let why = report["route"]["why"].as_str().unwrap();
    assert!(
        why.starts_with("first available on infra's first_mate list: fallback"),
        "{why}"
    );
    assert!(
        why.contains("passed over missing: unavailable: absent-tool not installed"),
        "{why}"
    );
    assert!(
        why.contains("spent: model route fallback/-/spent-model is backing off"),
        "{why}"
    );
    assert_eq!(wake_job(&mut fleet).0.as_deref(), Some("fallback-model"));
    assert_eq!(fleet.settle()["status"], "succeeded");

    back_off(&fleet, "fallback/-/fallback-model");
    fleet.send(
        json!({"id":"none-clear","from":"apps","to":"infra","kind":"request","body":"Anyone?"}),
    );
    let wakes = fleet.wakes().len();
    let report = Fleet::infra(&fleet.tick());
    let skipped = report["skipped"].as_str().unwrap_or_default();
    assert!(
        skipped.starts_with("every first_mate route for infra is unavailable"),
        "{report}"
    );
    assert_eq!(fleet.wakes().len(), wakes);
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("First mate route: none; every first_mate route for infra is unavailable"),
        "{bearings}"
    );
}

/// A hosted first mate takes the same listed route: its hosted launch is
/// tried on it, and the headless wake that follows runs on it too.
#[test]
fn a_hosted_first_mate_with_no_model_takes_the_listed_route_too() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, true);
    modelless_profile(&fleet);
    fleet.send(json!({"id":"hosted-listed","from":"apps","to":"infra","kind":"request","body":"Please handle this."}));
    let report = Fleet::infra(&fleet.tick());
    // This fixture's adapter cannot host, so the launch is refused and the
    // wake runs headless, on the listed route.
    assert!(report["host_error"].is_string(), "{report}");
    assert_eq!(report["route"]["name"], "spent", "{report}");
    assert_eq!(wake_job(&mut fleet).0.as_deref(), Some("spent-model"));
    assert_eq!(fleet.settle()["status"], "succeeded");
}

/// Two first_mate routes on the fixture's own (resumable) harness, listed in
/// `order`, for a profile that names no model.
fn same_harness_routes(fleet: &Fleet, order: &[&str]) {
    // Routes apply to the home's own ship.
    fs::write(
        fleet.home.join("config.json"),
        json!({"resource_home":fleet.root.join("ledger"),"ship":"infra"}).to_string(),
    )
    .unwrap();
    let manifest = fleet.root.join("manifest.json");
    fleet.ok(
        "routes.set",
        json!({
            "routes": {
                "a": {"manifest": manifest, "options": {"model": "model-a"}, "cost": "plan"},
                "b": {"manifest": manifest, "options": {"model": "model-b"}, "cost": "plan"}
            },
            "first_mate": order
        }),
        None,
    );
}

/// A warm session belongs to the model route it ran on. When the first_mate
/// list's choice moves to another route on the same harness, the next wake
/// starts fresh rather than resume the old conversation on the new model, in
/// both directions; on its own route a session resumes as it always did.
#[test]
fn a_warm_session_resumes_only_on_the_route_it_ran_on() {
    let mut fleet = Fleet::new(now_settings());
    let wake = |fleet: &mut Fleet, body: &str| {
        fleet.send(json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":body}));
        let woke = Fleet::infra(&fleet.tick());
        assert_eq!(woke["status"], "running", "{woke}");
        let settled = fleet.settle();
        assert_eq!(settled["status"], "succeeded", "{settled}");
        (woke, settled["session_id"].as_str().unwrap().to_owned())
    };
    same_harness_routes(&fleet, &["a", "b"]);
    let (woke, on_a) = wake(&mut fleet, "first on a");
    assert_eq!(woke["route"]["name"], "a", "{woke}");
    assert_eq!(woke["mode"], "fresh", "{woke}");
    let (woke, again) = wake(&mut fleet, "again on a");
    assert_eq!(woke["mode"], "resume", "{woke}");
    assert_eq!(again, on_a);

    // Route a gives way to b: a fresh session, not a's on model-b.
    same_harness_routes(&fleet, &["b", "a"]);
    let (woke, on_b) = wake(&mut fleet, "first on b");
    assert_eq!(woke["route"]["name"], "b", "{woke}");
    assert_eq!(woke["mode"], "fresh", "{woke}");
    assert_ne!(on_b, on_a);
    assert_eq!(wake_model_arg(&mut fleet).as_deref(), Some("model-b"));
    let (woke, again) = wake(&mut fleet, "again on b");
    assert_eq!(woke["mode"], "resume", "{woke}");
    assert_eq!(again, on_b);

    // And back: b's session never resumes on model-a.
    same_harness_routes(&fleet, &["a", "b"]);
    let (woke, back) = wake(&mut fleet, "back on a");
    assert_eq!(woke["route"]["name"], "a", "{woke}");
    assert_eq!(woke["mode"], "fresh", "{woke}");
    assert_ne!(back, on_b);
}

/// The route model the latest wake's job ran with.
fn wake_model_arg(fleet: &mut Fleet) -> Option<String> {
    let wake = fleet.wakes()[0].clone();
    let job = dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
    job["command"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|arg| ["model-a", "model-b"].contains(arg))
        .map(str::to_owned)
}

/// An errand's session is bound to its route the same way, and with no
/// errand list of its own an errand runs on the first mate's listed route.
#[test]
fn an_errand_session_resumes_only_on_the_route_it_ran_on() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let errand = |fleet: &mut Fleet, brief: &str| {
        let started = fleet.ok(
            "errand.start",
            json!({"focus":"task:infra-repair","brief":brief}),
            Some("infra"),
        );
        let id = started["errand"].as_str().unwrap().to_owned();
        assert_eq!(errand_settled(fleet, &id)["status"], "succeeded");
        started
    };
    same_harness_routes(&fleet, &["a", "b"]);
    let started = errand(&mut fleet, "Review it on a.");
    assert_eq!(started["route"]["name"], "a", "{started}");
    assert_eq!(started["mode"], "fresh", "{started}");
    assert_eq!(errand(&mut fleet, "Again on a.")["mode"], "resume");
    same_harness_routes(&fleet, &["b", "a"]);
    let started = errand(&mut fleet, "Now on b.");
    assert_eq!(started["route"]["name"], "b", "{started}");
    assert_eq!(started["mode"], "fresh", "{started}");
    assert_eq!(errand(&mut fleet, "Again on b.")["mode"], "resume");
}

/// The open runtime.needs items saying a first mate has no route to take.
fn no_route_needs(fleet: &Fleet) -> Vec<Value> {
    fleet.ok("runtime.needs", json!({"limit":100}), None)["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["evidence"] == "first_mate_no_route")
        .cloned()
        .collect()
}

/// With every first_mate route unavailable, wake-worthy mail that has waited
/// past max_delay_secs is the captain's to know of: runtime.needs names the
/// ship and each route's reason, and the item clears once a route does.
#[test]
fn a_first_mate_with_no_route_and_overdue_mail_is_a_captain_need_until_a_route_clears() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    modelless_profile(&fleet);
    let max_delay = |fleet: &Fleet, secs: u64| {
        let mut profile =
            fleet.ok("ship.show", json!({"name":"infra"}), None)["first_mate"].clone();
        profile["max_delay_secs"] = json!(secs);
        fleet.ok(
            "ship.put",
            json!({"name":"infra","first_mate":profile}),
            None,
        );
    };
    back_off(&fleet, "fallback/-/spent-model");
    back_off(&fleet, "fallback/-/fallback-model");
    max_delay(&fleet, 3600);
    fleet.send(json!({"id":"stuck","from":"apps","to":"infra","kind":"request","body":"Anyone?"}));
    let report = Fleet::infra(&fleet.tick());
    assert!(
        report["skipped"]
            .as_str()
            .unwrap_or_default()
            .starts_with("every first_mate route for infra is unavailable"),
        "{report}"
    );
    // Not overdue yet: the first mate waits quietly.
    assert_eq!(no_route_needs(&fleet), Vec::<Value>::new());

    max_delay(&fleet, 0);
    fleet.tick();
    let needs = no_route_needs(&fleet);
    assert_eq!(needs.len(), 1, "{needs:#?}");
    let need = &needs[0];
    assert_eq!(need["ship"], "infra", "{need}");
    assert_eq!(need["kind"], "blocker", "{need}");
    let summary = need["summary"].as_str().unwrap();
    for reason in [
        "spent: model route fallback/-/spent-model is backing off",
        "fallback: model route fallback/-/fallback-model is backing off",
    ] {
        assert!(summary.contains(reason), "{summary}");
    }
    // Another pass with nothing changed keeps the same item.
    fleet.tick();
    let again = no_route_needs(&fleet);
    assert_eq!(again.len(), 1, "{again:#?}");
    assert_eq!(again[0]["version"], need["version"]);

    // A route clears: the first mate wakes and the need is gone.
    let ledger = rusqlite::Connection::open(fleet.root.join("ledger/resources.sqlite")).unwrap();
    ledger
        .execute(
            "DELETE FROM model_routes WHERE route='fallback/-/fallback-model'",
            [],
        )
        .unwrap();
    let report = Fleet::infra(&fleet.tick());
    assert_eq!(report["status"], "running", "{report}");
    assert_eq!(report["route"]["name"], "fallback", "{report}");
    assert_eq!(no_route_needs(&fleet), Vec::<Value>::new());
    assert_eq!(fleet.settle()["status"], "succeeded");
}

/// A ship the dispatcher pass no longer checks, because it lost its first
/// mate or moved to another machine, keeps no first_mate_no_route need: the
/// pass that maintains the waits drops them.
#[test]
fn a_ship_that_loses_its_first_mate_or_moves_away_keeps_no_route_wait() {
    let mut fleet = Fleet::new(now_settings());
    native_routes(&fleet, false);
    modelless_profile(&fleet);
    let mut profile = fleet.ok("ship.show", json!({"name":"infra"}), None)["first_mate"].clone();
    profile["max_delay_secs"] = json!(0);
    let put = |fleet: &Fleet, ship: Value| fleet.ok("ship.put", ship, None);
    put(&fleet, json!({"name":"infra","first_mate":profile}));
    back_off(&fleet, "fallback/-/spent-model");
    back_off(&fleet, "fallback/-/fallback-model");
    fleet.send(json!({"id":"stuck","from":"apps","to":"infra","kind":"request","body":"Anyone?"}));
    fleet.tick();
    assert_eq!(no_route_needs(&fleet).len(), 1);

    // No first mate left to wake: the wait goes with it.
    put(&fleet, json!({"name":"infra"}));
    fleet.tick();
    assert_eq!(no_route_needs(&fleet), Vec::<Value>::new());

    put(&fleet, json!({"name":"infra","first_mate":profile}));
    fleet.tick();
    assert_eq!(no_route_needs(&fleet).len(), 1);

    // Moved to another machine: this one no longer checks it.
    fleet.ok(
        "peer.put",
        json!({"name":"elsewhere","transport":"local",
            "executable":"/nonexistent/privateer","home":fleet.root.join("elsewhere")}),
        None,
    );
    put(
        &fleet,
        json!({"name":"infra","location":{"peer":"elsewhere"}}),
    );
    fleet.tick();
    assert_eq!(no_route_needs(&fleet), Vec::<Value>::new());
}

#[test]
fn mail_wakes_the_first_mate_which_answers_and_its_usage_is_recorded() {
    let mut fleet = Fleet::new(now_settings());
    let request = fleet.send(
        json!({"id":"staging","from":"apps","to":"infra","kind":"request","body":"Is staging healthy?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    assert_eq!(woke["through_seq"], request["seq"]);
    let wake = fleet.settle();
    assert_eq!(wake["status"], "succeeded", "{wake}");
    assert_eq!(wake["mode"], "fresh");
    assert_eq!(wake["usage"]["cached_input_tokens"], 1000);
    assert_eq!(wake["usage"]["output_tokens"], 80);
    // The first mate answered as its ship and acknowledged what it saw.
    let answered = dispatch(&mut fleet.store, "mail.show", json!({"id":"staging"})).unwrap();
    assert_eq!(answered["state"], "answered");
    let reply = dispatch(&mut fleet.store, "mail.inbox", json!({"address":"apps"})).unwrap();
    assert_eq!(reply["messages"][0]["body"], "handled by infra");
    let status = dispatch(&mut fleet.store, "status", json!({})).unwrap();
    assert_eq!(status["open_requests"], 0);
    // The wake text follows the stable brief, so the brief stays a cacheable prefix.
    let prompt = &fleet.prompts()[0];
    assert!(prompt.starts_with("Task infra-duty:"), "{prompt}");
    assert!(prompt.contains("New mail through seq"));
    assert!(prompt.contains("id staging from apps kind request"));
    // Crew guidance appears only when crew mail is in the wake.
    assert!(!prompt.contains("Mail from crew"));
    // Nothing new: no further wake.
    let idle = Fleet::infra(&fleet.tick());
    assert_eq!(idle["skipped"], "nothing wake-worthy");
}

#[test]
fn an_awake_first_mate_is_not_woken_twice_and_the_next_wake_covers_only_new_mail() {
    let mut fleet = Fleet::new(now_settings());
    fleet.mode("sleep 1.5");
    fleet.send(json!({"id":"first","from":"apps","to":"infra","kind":"request","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.send(json!({"id":"second","from":"apps","to":"infra","kind":"request","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "first mate already awake"
    );
    // The tick that records the first wake as finished wakes the first mate
    // again for the mail that arrived meanwhile; settle waits for both.
    fleet.settle();
    let wakes = fleet.wakes();
    assert_eq!(wakes.len(), 2);
    assert!(wakes.iter().all(|w| w["status"] == "succeeded"));
    let prompts = fleet.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("id second"));
    assert!(!prompts[1].contains("id first"));
}

#[test]
fn routine_notices_wait_for_the_next_real_wake() {
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","urgency":"low","body":"FYI: docs updated"}),
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    fleet.send(
        json!({"id":"real","from":"apps","to":"infra","kind":"notice","body":"Deploy window moved"}),
    );
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    let prompt = &fleet.prompts()[0];
    assert!(prompt.contains("id fyi") && prompt.contains("id real"));
}

#[test]
fn a_correction_in_the_thread_of_an_open_follow_up_wakes_the_ship_that_raised_it() {
    // The stow incident, replayed: the machine is held for an update, a ship
    // asks it something, this ship answers and raises a follow-up on the
    // answer, and the asking ship corrects itself later in that thread. The
    // correction is a low notice, and it must still reach a ship with the
    // follow-up open: nothing else in its mailbox is wake-worthy.
    let mut fleet = Fleet::new(now_settings());
    // A stow is one machine's hold in the fleet registry, which the fixture
    // does not write: name this home's ship there so the hold has someone to
    // ask, exactly as `ship.create` would have.
    rusqlite::Connection::open(fleet.root.join("fleet.sqlite"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS ships(name TEXT PRIMARY KEY, home TEXT NOT NULL,
               created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, retired_ms INTEGER);",
        )
        .unwrap();
    rusqlite::Connection::open(fleet.root.join("fleet.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO ships(name,home,created_by,created_ms) VALUES('infra',?1,'privateer',0)",
            [fleet.home.to_str().unwrap()],
        )
        .unwrap();
    fleet.send(json!({"id":"baf32096","from":"apps","to":"infra",
        "kind":"request","body":"Which machine should I deploy on?"}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(fleet.settle()["status"], "succeeded");
    let answered = fleet.ok("mail.show", json!({"id":"baf32096"}), None);
    assert_eq!(answered["state"], "answered");
    let stow = fleet.ok("stow.start", json!({}), None)["stow"].clone();
    // The hold asks every ship to stow in its own mailbox, which is a wake of
    // its own; settle that so the correction below is the only reason to wake.
    let asked = fleet.wakes().len();
    let deadline = Instant::now() + Duration::from_secs(20);
    while fleet.wakes().len() == asked {
        fleet.tick();
        assert!(
            Instant::now() < deadline,
            "the stow request never woke the first mate"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(fleet.settle()["status"], "succeeded");
    let follow_up = fleet.send(
        json!({"id":"e376ceec","from":"infra","to":"captain","kind":"request",
            "because_of":"baf32096","body":"Can you release the laptop for the deploy?",
            "payload":{"depends_on":{"stow":stow}}}),
    );
    assert_eq!(follow_up["because_of"], "baf32096");
    // The follow-up waits on the captain, who is not this ship: quiet.
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    // apps corrects itself in the thread it opened, as a low notice.
    fleet.send(
        json!({"id":"correction","from":"apps","to":"infra","kind":"notice","urgency":"low",
            "correlation":"baf32096","body":"Correction: the desktop is free after all"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running", "{woke}");
    assert_eq!(fleet.settle()["status"], "succeeded");
    let prompt = fleet.prompts().last().unwrap().clone();
    assert!(prompt.contains("id correction"), "{prompt}");
    // The wake says which open request the correction concerns.
    assert!(prompt.contains("e376ceec"), "{prompt}");
    assert!(prompt.contains("mail.withdraw"), "{prompt}");
    // The ship withdraws the follow-up it no longer needs.
    fleet.ok(
        "mail.withdraw",
        json!({"id":"e376ceec","reason":"the desktop was free"}),
        Some("infra"),
    );
    assert_eq!(
        fleet.ok("mail.show", json!({"id":"e376ceec"}), None)["state"],
        "withdrawn"
    );
    // A later low notice in the same thread is background again.
    fleet.send(
        json!({"id":"later","from":"apps","to":"infra","kind":"notice","urgency":"low",
            "correlation":"baf32096","body":"And the desktop is busy now"}),
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
}

#[test]
fn a_correction_still_waits_for_the_batch_window_and_the_wake_budget() {
    let mut fleet = Fleet::new(now_settings());
    fleet.send(json!({"id":"parent","from":"apps","to":"infra",
        "kind":"request","body":"Which machine should I deploy on?"}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    fleet.send(
        json!({"id":"follow-up","from":"infra","to":"captain","kind":"request",
        "because_of":"parent","body":"Can you release the laptop?"}),
    );
    fleet.configure(json!({"batch_secs":3600,"max_delay_secs":3600}));
    fleet.send(
        json!({"id":"correction","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"parent","body":"Correction: the desktop is free"}),
    );
    // Wake-worthy, but it is ordinary mail for the batch window.
    assert_eq!(Fleet::infra(&fleet.tick())["skipped"], "batching");
    // The budget still counts it: one wake has been spent on the parent.
    fleet.configure(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":2}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    fleet.send(
        json!({"id":"later","from":"apps","to":"infra","kind":"notice","urgency":"low",
        "correlation":"parent","body":"And the desktop is busy now"}),
    );
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "hourly wake budget spent"
    );
}

#[test]
fn a_request_behind_a_flood_of_routine_notices_is_reached_in_one_wake() {
    let mut fleet = Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":1}));
    for n in 0..205 {
        fleet.send(
            json!({"id":format!("routine-{n}"),"from":"apps","to":"infra",
            "kind":"notice","urgency":"low","body":"Routine progress"}),
        );
    }
    let request = fleet.send(json!({"id":"after-the-page","from":"apps","to":"infra",
        "kind":"request","urgency":"high","body":"Please decide"}));
    // One wake, with the budget for one: it starts at the request and covers
    // the routine notices before it, rather than spending the wake on them.
    let first = Fleet::infra(&fleet.tick());
    assert_eq!(first["status"], "running", "{first}");
    assert_eq!(first["through_seq"], request["seq"]);
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(
        fleet.ok("mail.show", json!({"id":request["id"]}), None)["state"],
        "answered"
    );
    let inbox = fleet.ok("mail.inbox", json!({"address":"infra"}), None);
    assert_eq!(inbox["acknowledged_through"], request["seq"]);
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "nothing wake-worthy"
    );
    let history = fleet.ok(
        "mail.inbox",
        json!({"address":"infra","all":true,"limit":1000}),
        None,
    );
    assert_eq!(history["messages"].as_array().unwrap().len(), 206);
    let prompts = fleet.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].len() < 32_768);
    assert!(prompts[0].contains("id after-the-page"), "{}", prompts[0]);
    // The covered notices are counted, not listed.
    assert!(prompts[0].contains("205 routine notices"), "{}", prompts[0]);
    assert!(!prompts[0].contains("id routine-0"));
}

#[test]
fn concurrent_legacy_activation_upgrades_preserve_history() {
    use privateer::store::Store;
    use std::sync::{Arc, Barrier};
    let fleet = Fleet::new(now_settings());
    fleet
        .store
        .connection()
        .execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms)
         VALUES('legacy', 'old-ship', 'ship', 17, 'fresh', 'succeeded', 1)",
            [],
        )
        .unwrap();
    for _ in 0..3 {
        fleet
            .store
            .connection()
            .execute_batch(
                "ALTER TABLE wakes DROP COLUMN focuses;
             ALTER TABLE wakes DROP COLUMN harness;
             ALTER TABLE wakes DROP COLUMN session_id;
             ALTER TABLE wakes DROP COLUMN dispatcher_generation;",
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(16));
        let openers: Vec<_> = (0..16)
            .map(|_| {
                let barrier = barrier.clone();
                let home = fleet.home.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&home)
                        .map(|store| {
                            store
                                .connection()
                                .query_row(
                                    "SELECT through_seq FROM wakes WHERE id='legacy'",
                                    [],
                                    |r| r.get::<_, i64>(0),
                                )
                                .unwrap()
                        })
                        .map_err(|e| e.to_string())
                })
            })
            .collect();
        for opener in openers {
            assert_eq!(opener.join().unwrap().unwrap(), 17);
        }
        let columns: i64 = fleet.store.connection().query_row(
            "SELECT COUNT(*) FROM pragma_table_info('wakes') WHERE name IN ('focuses','harness','session_id','dispatcher_generation')",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(columns, 4);
    }
}

#[test]
fn a_slow_peer_has_a_bounded_pass_and_local_work_and_other_peers_progress() {
    use privateer::store::Store;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    let mut fleet = Fleet::new(now_settings());
    let remote_home = fleet.root.join("remote");
    let mut remote = Store::open(&remote_home).unwrap();
    dispatch(
        &mut remote,
        "peer.put",
        json!({"name":"origin","transport":"local",
        "executable":support::BIN,"home":fleet.home}),
    )
    .unwrap();
    dispatch(
        &mut remote,
        "ship.put",
        json!({"name":"apps","location":{"peer":"origin"}}),
    )
    .unwrap();
    dispatch(&mut remote, "ship.put", json!({"name":"healthy"})).unwrap();
    for (peer, ship) in [("slow", "slow-ship"), ("healthy", "healthy")] {
        dispatch(
            &mut fleet.store,
            "peer.put",
            json!({"name":peer,"transport":"local",
            "executable":"/nonexistent/privateer","home":remote_home,"timeout_ms":5000}),
        )
        .unwrap();
        dispatch(
            &mut fleet.store,
            "ship.put",
            json!({"name":ship,"location":{"peer":peer}}),
        )
        .unwrap();
        fleet.send(json!({"id":format!("out-{peer}"),"from":"apps","to":ship,
            "kind":"notice","body":"queued while offline"}));
    }
    let marker = fleet.root.join("slow-started.json");
    let script = fleet.root.join("slow-peer.py");
    fs::write(&script, format!(
        "#!{}\nimport json, os, pathlib, sqlite3, time\ntime.sleep(0.25)  # exercise cold interpreter startup\nwith sqlite3.connect({:?}) as db:\n    wakes = db.execute(\"SELECT COUNT(*) FROM wakes\").fetchone()[0]\npathlib.Path({:?}).write_text(json.dumps({{\"pid\":os.getpid(),\"wakes\":wakes}}))\ntime.sleep(5)\n",
        support::python().display(), fleet.home.join("privateer.sqlite").to_string_lossy(), marker.to_string_lossy(),
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    for (peer, executable) in [
        ("slow", script.as_path()),
        ("healthy", std::path::Path::new(support::BIN)),
    ] {
        dispatch(
            &mut fleet.store,
            "peer.put",
            json!({"name":peer,"transport":"local",
            "executable":executable,"home":remote_home,"timeout_ms":5000}),
        )
        .unwrap();
    }
    fleet
        .store
        .connection()
        .execute_batch(
            "UPDATE mail_delivery_retry SET next_attempt_ms=0,
           last_attempt_ms=CASE peer WHEN 'slow' THEN 1 ELSE 2 END;",
        )
        .unwrap();
    fleet.send(json!({"id":"local-request","from":"apps","to":"infra",
        "kind":"request","body":"local work must start before remote delivery"}));
    let tick = |budget| {
        let output = fleet
            .command()
            .args(["serve", "--json"])
            .arg(json!({"once":true,"delivery_budget_ms":budget}).to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    // Budget for cold Python startup (including the injected delay), while
    // remaining well below the fixture's five-second blocked call and the
    // configured peer timeout. Production timeouts are unchanged.
    let first = tick(3000);
    assert_eq!(Fleet::infra(&first)["status"], "running", "{first}");
    assert_eq!(first["delivered"]["attempted"], 1);
    // The pass cut the slow call at what was left of its delivery budget, not
    // at the peer's own five seconds. Read the cap the call ran under rather
    // than timing the whole pass, whose process start-up and local work a
    // loaded runner stretches by seconds.
    let slow = fleet.ok("mail.show", json!({"id":"out-slow"}), None);
    let error = slow["delivery_error"].as_str().unwrap_or_default();
    let capped_ms: u64 = error
        .split_once("timed out after ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("slow call was not cut at a deadline: {slow}"));
    assert!(capped_ms <= 3000, "{slow}");
    // The fake peer writes the marker after its cold start; wait for it
    // rather than reading once, since a loaded runner can start it late.
    let until = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            Instant::now() < until,
            "slow peer never wrote {marker:?}: {first}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let observed: serde_json::Value = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
    assert_eq!(
        observed["wakes"], 1,
        "local wake must precede the slow call"
    );
    // The capped peer call reaped its transport; no timed-out payload remains.
    assert_eq!(
        unsafe { libc::kill(observed["pid"].as_i64().unwrap() as i32, 0) },
        -1
    );
    // The healthy call is a whole privateer subprocess; on a loaded runner its
    // start-up alone can pass one second, so give it the same budget.
    let second = tick(3000);
    assert_eq!(second["delivered"]["delivered"], 1, "{second}");
    assert_eq!(second["delivered"]["pending"], 1);
    let inbox = dispatch(&mut remote, "mail.inbox", json!({"address":"healthy"})).unwrap();
    assert_eq!(inbox["messages"][0]["id"], "out-healthy");
    fleet.settle();
    assert_eq!(
        fleet.ok("mail.show", json!({"id":"local-request"}), None)["state"],
        "answered"
    );
}

#[test]
fn batching_and_the_hourly_budget_defer_wakes() {
    let mut fleet = Fleet::new(json!({"batch_secs":3600,"max_delay_secs":3600}));
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"one"}));
    assert_eq!(Fleet::infra(&fleet.tick())["skipped"], "batching");
    fleet.configure(json!({"batch_secs":0,"max_delay_secs":0,"max_wakes_per_hour":1}));
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    fleet.settle();
    fleet.send(json!({"from":"apps","to":"infra","kind":"notice","body":"two"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "hourly wake budget spent"
    );
}

#[test]
fn a_failed_wake_backs_off_then_retries_the_same_mail() {
    let mut fleet =
        Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600}));
    fleet.mode("fail");
    fleet
        .send(json!({"id":"retry-me","from":"apps","to":"infra","kind":"request","body":"please"}));
    fleet.tick();
    let failed = fleet.settle();
    assert_eq!(failed["status"], "failed");
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "backing off after a failed wake"
    );
    // A newer backlog alarm record (a hosted wake's budget row) is not a
    // wake attempt and does not hide the failure.
    fleet
        .store
        .connection()
        .execute(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms, finished_ms, reason)
             VALUES('backlog-infra-1', 'infra', 'thread:backlog-infra', 0, 'hosted', 'succeeded',
               ?1, ?1, 'backlog')",
            [privateer::store::Store::now_ms() + 1],
        )
        .unwrap();
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "backing off after a failed wake"
    );
    fleet.configure(now_settings());
    fleet.mode("normal");
    assert_eq!(Fleet::infra(&fleet.tick())["status"], "running");
    assert_eq!(fleet.settle()["status"], "succeeded");
    assert_eq!(
        dispatch(&mut fleet.store, "mail.show", json!({"id":"retry-me"})).unwrap()["state"],
        "answered"
    );
}

#[test]
fn only_one_operator_dispatcher_runs_per_home() {
    let fleet = Fleet::new(now_settings());
    let agent = fleet
        .command()
        .args(["serve", "--json", "{\"once\":true}"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "infra")
        .output()
        .unwrap();
    assert!(!agent.status.success());
    let mut background = support::Resident::spawn(
        {
            let mut command = fleet.command();
            command
                .args(["serve", "--json", "{\"interval_secs\":1}"])
                .stdout(std::process::Stdio::null());
            command
        },
        &fleet.root,
    );
    std::thread::sleep(Duration::from_millis(500));
    let second = fleet
        .command()
        .args(["serve", "--json", "{\"once\":true}"])
        .output()
        .unwrap();
    background.stop();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
}

/// A dropped dispatcher fixture must not leave the dispatcher, or a payload
/// that detached into its own session, behind: an orphaned `serve` observed
/// under the user's service manager outlived its test run and the deletion
/// of the test home it ran on. The resident dispatcher is stopped with its
/// group and then swept by `/proc`, which is scanned for this fixture's own
/// temporary home path, on Linux only: a process's command line or working
/// directory referring to that unique path can only be one this fixture
/// started.
#[test]
#[cfg(target_os = "linux")]
fn a_dropped_dispatcher_leaves_no_process_referring_to_its_home() {
    let mut fleet = Fleet::new(now_settings());
    // A wake whose payload lingers: it runs detached, in its own session, so
    // killing the dispatcher alone would leave it running.
    fleet.mode("sleep 60");
    fleet.send(json!({
        "id":"linger", "from":"apps", "to":"infra",
        "kind":"request", "body":"take your time"
    }));
    let mut dispatcher = support::Resident::spawn(
        {
            let mut command = fleet.command();
            command
                .args(["serve", "--json", "{\"interval_secs\":1}"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            command
        },
        &fleet.root,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let payload = loop {
        let latest = fleet.wakes().first().cloned();
        if let Some(payload) = latest
            .filter(|wake| wake["status"] == "running")
            .and_then(|wake| wake["job_id"].as_str().map(str::to_owned))
            .and_then(|job| {
                dispatch(&mut fleet.store, "job.status", json!({"id":job}))
                    .ok()
                    .and_then(|job| job["payload"]["pid"].as_i64())
            })
            .filter(|pid| support::processes_referring_to(&fleet.root).contains(&(*pid as u32)))
        {
            break payload;
        }
        assert!(
            Instant::now() < deadline,
            "the detached payload never started: {:?}",
            fleet.wakes()
        );
        thread::sleep(Duration::from_millis(100));
    };
    let running = support::processes_referring_to(&fleet.root);
    assert!(
        running.contains(&(payload as u32)),
        "the running payload {payload} should be visible in /proc: {running:?}"
    );
    dispatcher.stop();
    let leftover = support::processes_referring_to(&fleet.root);
    assert!(
        leftover.is_empty(),
        "processes outlived the dropped dispatcher: {leftover:?}"
    );
}

#[test]
fn first_mate_profiles_are_validated() {
    let mut fleet = Fleet::new(now_settings());
    let manifest = fleet.root.join("manifest.json");
    for bad in [
        json!({"manifest": manifest, "duty_task": "apps-duty"}),
        json!({"manifest": "relative/manifest.json", "duty_task": "infra-duty"}),
        json!({"duty_task": "infra-duty"}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "batch_secs": -1}),
        json!({"manifest": manifest, "duty_task": "infra-duty", "surprise": true}),
    ] {
        assert!(
            dispatch(
                &mut fleet.store,
                "ship.put",
                json!({"name":"infra","first_mate":bad})
            )
            .is_err(),
            "{bad}"
        );
    }
    let ship = dispatch(&mut fleet.store, "ship.show", json!({"name":"infra"})).unwrap();
    assert_eq!(ship["first_mate"]["max_wakes_per_hour"], 12);
}

#[test]
fn wakes_group_mail_by_focus_carry_handoff_notes_and_run_under_the_focus_task() {
    let mut fleet = Fleet::new(now_settings());
    let first = fleet.send(
        json!({"id":"repair-status","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"How is the repair going?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["focus"], "task:infra-repair", "{woke}");
    assert_eq!(woke["task_id"], "infra-repair");
    fleet.settle();
    // The first mate left a note for the focus it worked on.
    let note = dispatch(
        &mut fleet.store,
        "note.get",
        json!({"ship":"infra","focus":"task:infra-repair"}),
    )
    .unwrap();
    assert_eq!(note["notes"][0]["author"], "infra");
    assert_eq!(
        note["notes"][0]["body"],
        format!("Seen through seq {}", first["seq"])
    );
    // Pins ride along with the note; a crew assignment stays out of wakes.
    dispatch(
        &mut fleet.store,
        "note.pin",
        json!({"ship":"infra","focus":"task:infra-repair","key":"head","value":"ad536c8"}),
    )
    .unwrap();
    dispatch(
        &mut fleet.store,
        "note.put",
        json!({"ship":"infra","focus":"assignment:infra-repair","body":"Rebuild the cache index and report the timings."}),
    )
    .unwrap();
    // Later mail on the same task brings that note back; an unrelated
    // question is its own thread, run under the duty task.
    fleet.send(
        json!({"id":"repair-more","from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"Also check the cache"}),
    );
    fleet.send(
        json!({"id":"unrelated","from":"apps","to":"infra","kind":"request","body":"Any disk alerts?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["task_id"], "infra-repair");
    assert_eq!(
        woke["focuses"],
        json!(["task:infra-repair", "thread:unrelated"])
    );
    fleet.settle();
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.contains("## task:infra-repair"), "{prompt}");
    assert!(prompt.contains("Your handoff note (v1)"));
    assert!(prompt.contains(&format!("Seen through seq {}", first["seq"])));
    assert!(prompt.contains("Pinned: head=ad536c8"), "{prompt}");
    assert!(!prompt.contains("Rebuild the cache index"));
    assert!(prompt.contains("## thread:unrelated"));
    assert!(prompt.contains("Bearings for infra"));
    // Mail with no owned task runs under the duty task.
    fleet.send(
        json!({"id":"just-a-question","from":"apps","to":"infra","kind":"request","body":"Which region?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["focus"], "thread:just-a-question");
    assert_eq!(woke["task_id"], "infra-duty");
    fleet.settle();
}

#[test]
fn a_warm_single_focus_session_is_resumed_with_only_the_new_mail() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    fleet.send(
        json!({"id":"q1","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"Start the repair?"}),
    );
    fleet.tick();
    let first = fleet.settle();
    assert_eq!(first["mode"], "fresh");
    let session = first["session_id"].as_str().unwrap().to_owned();
    fleet.send(
        json!({"id":"q2","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"And the cache?"}),
    );
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["mode"], "resume", "{woke}");
    assert_eq!(woke["task_id"], "infra-repair");
    let second = fleet.settle();
    assert_eq!(second["session_id"], session.as_str());
    // A resumed wake sends only the wake text: no brief, bearings or notes.
    let prompt = fleet.prompts().pop().unwrap();
    assert!(prompt.starts_with("Wake wake-"), "{prompt}");
    assert!(!prompt.contains("Bearings for"));
    assert!(!prompt.contains("Your handoff note"));
    assert!(prompt.contains("id q2"));
}

#[test]
fn a_no_action_wake_leaves_a_stale_mail_warning_in_fresh_and_resumed_prompts() {
    for cache_window in [0, 300] {
        let mut fleet = Fleet::new(json!({
            "batch_secs":0, "max_delay_secs":0, "retry_backoff_secs":0,
            "cache_window_secs":cache_window
        }));
        fleet.mode("no-action");
        let handoff = fleet.send(json!({
            "id":"handoff", "from":"apps", "to":"infra", "kind":"notice",
            "task_id":"infra-repair", "body":"No action needed for this handoff message."
        }));
        fleet.tick();
        let first = fleet.settle();
        assert_eq!(first["status"], "succeeded", "{first}");
        assert!(!fleet.prompts()[0].contains("Stale unacknowledged mail:"));
        // Covered but unacked mail does not create a repeated wake by itself.
        assert_eq!(
            Fleet::infra(&fleet.tick())["skipped"],
            "nothing wake-worthy"
        );
        let before = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
        assert_eq!(before["stale_unacknowledged_mail"]["count"], 1);
        assert_eq!(
            before["stale_unacknowledged_mail"]["oldest_seq"],
            handoff["seq"]
        );

        fleet.mode("normal");
        fleet.send(json!({
            "id":"correction", "from":"apps", "to":"infra", "kind":"request",
            "task_id":"infra-repair", "body":"Handle this new correction independently."
        }));
        let woke = Fleet::infra(&fleet.tick());
        assert_eq!(
            woke["mode"],
            if cache_window == 0 { "fresh" } else { "resume" }
        );
        fleet.settle();
        let prompt = fleet.prompts().pop().unwrap();
        assert_eq!(
            prompt.matches("Stale unacknowledged mail:").count(),
            1,
            "{prompt}"
        );
        assert!(
            prompt.contains(&format!("oldest seq {}", handoff["seq"])),
            "{prompt}"
        );
        assert!(
            prompt.contains("still needing handling or an ack"),
            "{prompt}"
        );
        assert!(
            prompt.contains("not to other mail in this or later wakes"),
            "{prompt}"
        );
        assert!(prompt.contains("id correction"), "{prompt}");
        assert!(!prompt.contains("id handoff"), "{prompt}");
        let after = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
        assert_eq!(after["stale_unacknowledged_mail"]["count"], 0);
        let history = fleet.ok(
            "mail.inbox",
            json!({"address":"infra", "all":true}),
            Some("infra"),
        );
        let correction = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "correction")
            .unwrap();
        assert_eq!(correction["state"], "answered");
    }
}

#[test]
fn cold_mixed_rotated_or_failed_sessions_start_fresh() {
    // Outside the cache window.
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0,"cache_window_secs":0}),
    );
    for id in ["a1", "a2"] {
        fleet.send(json!({"id":id,"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":id}));
        fleet.tick();
        assert_eq!(fleet.settle()["mode"], "fresh");
    }
    // Past the rotation limit.
    let mut fleet = Fleet::new(
        json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":0,"session_max_wakes":1}),
    );
    for id in ["b1", "b2"] {
        fleet.send(json!({"id":id,"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":id}));
        fleet.tick();
        assert_eq!(fleet.settle()["mode"], "fresh");
    }
    // Mail spanning two foci never resumes one focus's conversation.
    let mut fleet = Fleet::new(now_settings());
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"c1"}),
    );
    fleet.tick();
    fleet.settle();
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"c2"}),
    );
    fleet.send(json!({"id":"other","from":"apps","to":"infra","kind":"notice","body":"elsewhere"}));
    fleet.tick();
    assert_eq!(fleet.settle()["mode"], "fresh");
    // A failed resumed wake drops the session; the retry starts fresh.
    let mut fleet =
        Fleet::new(json!({"batch_secs":0,"max_delay_secs":0,"retry_backoff_secs":3600}));
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"d1"}),
    );
    fleet.tick();
    fleet.settle();
    fleet.mode("fail");
    fleet.send(
        json!({"from":"apps","to":"infra","kind":"notice","task_id":"infra-repair","body":"d2"}),
    );
    assert_eq!(Fleet::infra(&fleet.tick())["mode"], "resume");
    assert_eq!(fleet.settle()["status"], "failed");
    fleet.mode("normal");
    fleet.configure(now_settings());
    assert_eq!(Fleet::infra(&fleet.tick())["mode"], "fresh");
    fleet.settle();
}

#[test]
fn a_read_message_delegate_first_mate_wakes_without_implementation_authority() {
    let mut fleet = Fleet::new(now_settings());
    dispatch(
        &mut fleet.store,
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
    )
    .unwrap();
    fleet.send(json!({"id":"liaison-question","from":"apps","to":"infra","kind":"request","task_id":"infra-repair","body":"Read and relay"}));
    let woke = Fleet::infra(&fleet.tick());
    assert_eq!(woke["status"], "running");
    assert_eq!(woke["task_id"], "infra-repair");
    let wake = fleet.settle();
    assert_eq!(wake["status"], "succeeded", "{wake}");
    assert_eq!(
        dispatch(
            &mut fleet.store,
            "mail.show",
            json!({"id":"liaison-question"})
        )
        .unwrap()["state"],
        "answered"
    );
    let job = dispatch(&mut fleet.store, "job.status", json!({"id":wake["job_id"]})).unwrap();
    assert_eq!(job["action"], "spawn");
    assert_eq!(job["task_id"], "infra-repair");
    for action in ["implement", "spawn"] {
        let denied = dispatch(
            &mut fleet.store,
            "job.start",
            json!({"task_id":"infra-duty","command":["sh","-c","exit 0"],"action":action}),
        );
        assert!(
            denied.is_err(),
            "ordinary jobs may not claim {action} under a liaison charter"
        );
    }
}

#[test]
fn task_linked_liaison_wakes_stop_the_harness_and_child_when_authority_ends() {
    fn active(pid: i64) -> bool {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let stat = String::from_utf8(output.stdout).unwrap();
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    for revoke in [false, true] {
        let mut fleet = Fleet::new(now_settings());
        fleet.ok(
            "charter.put",
            json!({"id":"fleet","actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok(
            "charter.put",
            json!({"id":"liaison-focus","objective":"Coordinate a peer request",
            "repo":fleet.root.join("repo"),"min_free_bytes":0,"job_timeout_secs":60,
            "actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok(
            "task.create",
            json!({"id":"liaison-request","charter_id":"liaison-focus",
            "owner":"infra","title":"Relay peer request"}),
            None,
        );
        fs::write(fleet.root.join("mate.py"), r#"import json, os, pathlib, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
pathlib.Path(sys.argv[5]).write_text(json.dumps({'pid': os.getpid(), 'child': child.pid, 'owner': sys.argv[2]}))
child.wait()
"#).unwrap();
        if revoke {
            fleet.send(
                json!({"id":"linked-request","from":"infra","to":"apps","kind":"request",
                "task_id":"liaison-request","body":"Which peer can help?"}),
            );
            fleet.send(
                json!({"id":"linked-reply","from":"apps","to":"infra","kind":"reply",
                "correlation":"linked-request","body":"Please coordinate with us"}),
            );
        } else {
            fleet.send(
                json!({"id":"linked-request","from":"apps","to":"infra","kind":"request",
                "task_id":"liaison-request","body":"Please coordinate with us"}),
            );
        }
        let woke = Fleet::infra(&fleet.tick());
        assert_eq!(woke["status"], "running", "{woke}");
        let job_id = &woke["wake"];
        let until = Instant::now() + Duration::from_secs(8);
        let (running, processes) = loop {
            let job = fleet.ok("job.status", json!({"id":job_id}), None);
            let processes = fs::read(fleet.root.join("prompts.log"))
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
            if let Some(processes) = processes.filter(|_| job["status"] == "running") {
                break (job, processes);
            }
            if Instant::now() >= until {
                fleet.ok("job.cancel", json!({"id":job_id}), None);
                panic!("harness did not start: {job}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let pid = processes["pid"].as_i64().unwrap();
        let child = processes["child"].as_i64().unwrap();
        let was_alive = active(pid) && active(child);
        if revoke {
            fleet.ok("charter.revoke", json!({"id":"liaison-focus"}), None);
        } else {
            fleet.ok(
                "task.update",
                json!({"id":"liaison-request","actor":"infra","status":"cancelled"}),
                None,
            );
        }
        let until = Instant::now() + Duration::from_secs(8);
        let stopped = loop {
            let job = fleet.ok("job.status", json!({"id":job_id}), None);
            if job["status"] == "cancelled" || Instant::now() >= until {
                break job;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let payload_alive = active(pid);
        let child_alive = active(child);
        fleet.ok("job.cancel", json!({"id":job_id}), None);
        assert_eq!(woke["focus"], "task:liaison-request");
        assert_eq!(woke["task_id"], "liaison-request");
        assert_eq!(running["task_id"], "liaison-request");
        assert_eq!(
            fleet.ok("task.show", json!({"id":"liaison-request"}), None)["charter_id"],
            "liaison-focus"
        );
        assert_eq!(running["action"], "spawn");
        assert_eq!(running["agent_actor"], "infra");
        assert_eq!(running["payload"]["pid"], processes["pid"]);
        assert_eq!(processes["owner"], "infra");
        assert!(
            was_alive,
            "harness and child must be running before authority ends"
        );
        assert_eq!(stopped["status"], "cancelled", "{stopped}");
        let reason = stopped["reason"].as_str().unwrap();
        assert!(
            reason.contains(if revoke {
                "charter was revoked"
            } else {
                "task is terminal"
            }),
            "{reason}"
        );
        assert!(
            !payload_alive,
            "harness survived the end of its task authority"
        );
        assert!(
            !child_alive,
            "harness child survived the end of its task authority"
        );
        assert!(fleet.store.authorize("infra-duty", "spawn").is_ok());
    }
}

#[test]
fn liaison_wakes_keep_duty_fallback_for_ineligible_or_foreign_tasks() {
    for ineligible in ["foreign", "no-spawn", "cancelled", "done", "revoked"] {
        let mut fleet = Fleet::new(now_settings());
        fleet.ok(
            "charter.put",
            json!({"id":"fleet","actions":["inspect","spawn","message"]}),
            None,
        );
        fleet.ok("charter.put", json!({"id":"focus","objective":"Coordinate request",
            "repo":fleet.root.join("repo"),"min_free_bytes":0,"actions":["inspect","spawn","message"]}), None);
        let owner = if ineligible == "foreign" {
            "apps"
        } else {
            "infra"
        };
        fleet.ok(
            "task.create",
            json!({"id":"focus","charter_id":"focus","owner":owner,"title":"Peer request"}),
            None,
        );
        match ineligible {
            "no-spawn" => {
                fleet.ok(
                    "charter.put",
                    json!({"id":"focus","actions":["inspect","message"]}),
                    None,
                );
            }
            "cancelled" | "done" => {
                fleet.ok(
                    "task.update",
                    json!({"id":"focus","actor":"infra","status":ineligible}),
                    None,
                );
            }
            "revoked" => {
                fleet.ok("charter.revoke", json!({"id":"focus"}), None);
            }
            _ => {}
        }
        // Terminal tasks cannot acquire new open questions. A notice still
        // carries historical task context and must take the same duty fallback.
        let kind = if matches!(ineligible, "cancelled" | "done") {
            "notice"
        } else {
            "request"
        };
        fleet.send(json!({"id":"question","from":"apps","to":"infra","kind":kind,"task_id":"focus","body":"Can you relay this?"}));
        let woke = Fleet::infra(&fleet.tick());
        let wake = fleet.settle();
        assert_eq!(woke["task_id"], "infra-duty", "{ineligible}: {woke}");
        assert_eq!(wake["status"], "succeeded", "{ineligible}: {wake}");
        let job = fleet.ok("job.status", json!({"id":wake["job_id"]}), None);
        assert_eq!(job["task_id"], "infra-duty");
        assert_eq!(job["action"], "spawn");
        assert_eq!(job["agent_actor"], "infra");
    }
}

/// Tick until the errand has settled, and return its record.
fn errand_settled(fleet: &mut Fleet, id: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        fleet.tick();
        let list = fleet.ok("errand.list", json!({}), Some("infra"));
        let errand = list["errands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .cloned()
            .unwrap();
        if errand["status"] != "running" {
            return errand;
        }
        assert!(std::time::Instant::now() < until, "errand never settled");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Tick until the infra mailbox holds a message whose body contains `needle`,
/// and return the whole history. The first mate's wake acknowledges mail in
/// the background, so an unread-only read can miss a message that has arrived.
fn mail_history_containing(fleet: &mut Fleet, needle: &str) -> Value {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        fleet.tick();
        let history = fleet.ok(
            "mail.inbox",
            json!({"address":"infra","all":true}),
            Some("infra"),
        );
        let found = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"].as_str().unwrap_or_default().contains(needle));
        if found {
            return history;
        }
        assert!(std::time::Instant::now() < until, "{history}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn an_errand_thinks_in_its_own_session_and_mails_only_its_verdict() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    fleet.ok(
        "note.put",
        json!({"ship":"infra","focus":"task:infra-repair","body":"Crew rebuilt the cache index; review it before merging."}),
        Some("infra"),
    );
    // Only the ship that owns the focus sends errands on it.
    let refused = fleet.refused(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review it"}),
        Some("apps"),
    );
    assert!(refused.contains("not owned by apps"), "{refused}");

    let started = fleet.ok(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review the cache rebuild and say whether it can merge."}),
        Some("infra"),
    );
    assert_eq!(started["mode"], "fresh");
    let id = started["errand"].as_str().unwrap().to_owned();
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains(&format!("- {id} on task:infra-repair"))
    );
    let errand = errand_settled(&mut fleet, &id);
    assert_eq!(errand["status"], "succeeded", "{errand}");
    // It started from the focus's records, not the first mate's thread. The
    // verdict mail may already have woken the first mate, so find the errand's.
    let errand_prompt = |fleet: &Fleet, id: &str| {
        let mark = format!("Errand {id}");
        fleet
            .prompts()
            .into_iter()
            .rfind(|p| p.contains(&mark))
            .unwrap()
    };
    let prompt = errand_prompt(&fleet, &id);
    assert!(
        prompt.contains(&format!("Errand {id} for the ship infra")),
        "{prompt}"
    );
    assert!(prompt.contains("Review the cache rebuild"));
    assert!(prompt.contains("review it before merging"));
    // The first mate receives the verdict and no crew outcome for the errand.
    let inbox = mail_history_containing(&mut fleet, &format!("Reviewed: {id}"));
    let mail: Vec<&Value> = inbox["messages"].as_array().unwrap().iter().collect();
    assert_eq!(mail.len(), 1, "{inbox}");
    assert_eq!(mail[0]["from"], "crew");
    assert_eq!(
        mail[0]["body"],
        format!("Errand on task:infra-repair, verdict:\nReviewed: {id}. Brief seen: True")
    );

    // A follow-up on the same focus continues the errand's session.
    let again = fleet.ok(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Check the fix to the cache test."}),
        Some("infra"),
    );
    assert_eq!(again["mode"], "resume");
    let next = again["errand"].as_str().unwrap().to_owned();
    assert_eq!(errand_settled(&mut fleet, &next)["status"], "succeeded");
    let prompt = errand_prompt(&fleet, &next);
    assert!(
        prompt.starts_with(&format!("Errand {next}, continuing on task:infra-repair")),
        "{prompt}"
    );

    // An errand that dies without a reply says so.
    fleet.mode("fail");
    let failing = fleet.ok(
        "errand.start",
        json!({"focus":"thread:outage","brief":"Catch up on the outage thread."}),
        Some("infra"),
    );
    let failed = failing["errand"].as_str().unwrap().to_owned();
    assert_eq!(errand_settled(&mut fleet, &failed)["status"], "failed");
    let inbox = mail_history_containing(&mut fleet, "without a verdict");
    let last = inbox["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert!(
        last["body"].as_str().unwrap().contains("without a verdict"),
        "{last}"
    );
    assert_eq!(last["urgency"], "high");
}

#[test]
fn an_errand_whose_turn_failed_after_a_full_reply_delivers_it_as_unconfirmed() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let errand = |fleet: &mut Fleet, mode: &str, focus: &str| {
        fleet.mode(mode);
        let started = fleet.ok(
            "errand.start",
            json!({"focus":focus,"brief":"Review the cache rebuild."}),
            Some("infra"),
        );
        let id = started["errand"].as_str().unwrap().to_owned();
        let settled = errand_settled(fleet, &id);
        let history = mail_history_containing(fleet, &format!("Errand on {focus}"));
        let mail = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == format!("{id}-verdict"))
            .cloned()
            .unwrap();
        (id, settled, mail)
    };

    // The reply was whole, but the run could not confirm it completed: the
    // verdict is delivered, labelled with how the run ended, and the errand
    // keeps the status its job had.
    let (id, settled, mail) = errand(&mut fleet, "errand-unconfirmed", "thread:unconfirmed");
    assert_eq!(settled["status"], "failed", "{settled}");
    assert_eq!(
        mail["body"],
        format!(
            "Errand on thread:unconfirmed, verdict (unconfirmed; the run ended: native completion not confirmed):\nReviewed: {id}. Brief seen: True"
        ),
        "{mail}"
    );
    assert_eq!(mail["urgency"], "normal", "{mail}");
    assert_eq!(mail["payload"]["status"], "failed", "{mail}");
    assert_eq!(mail["payload"]["verdict"], "unconfirmed", "{mail}");
    assert_eq!(
        mail["payload"]["ended"], "native completion not confirmed",
        "{mail}"
    );
    assert!(
        settled["verdict"].as_str().unwrap().contains("Reviewed:"),
        "{settled}"
    );

    // Claude's whole result, then a denied tool: only the log holds it.
    let (id, settled, mail) = errand(&mut fleet, "errand-blocked", "thread:blocked");
    assert_eq!(settled["status"], "failed", "{settled}");
    let body = mail["body"].as_str().unwrap();
    assert!(
        body.starts_with(&format!(
            "Errand on thread:blocked, verdict (unconfirmed; the run ended: permission blocked):\nReviewed: {id}. Brief seen: True\n"
        )),
        "{mail}"
    );
    // The denial still opened the captain's ask; the verdict is not held for it.
    assert!(
        body.contains("Claude was denied Bash(cargo test *)"),
        "{mail}"
    );
    assert!(mail["payload"]["about"].is_string(), "{mail}");
    assert_eq!(mail["urgency"], "normal", "{mail}");
    assert_eq!(mail["payload"]["ended"], "permission blocked", "{mail}");

    // An API error in Claude's result line is not a verdict.
    let (_, settled, mail) = errand(&mut fleet, "errand-error-result", "thread:errored");
    assert_eq!(settled["status"], "failed", "{settled}");
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert!(!body.contains("API Error"), "{mail}");
    assert_eq!(mail["urgency"], "high", "{mail}");

    // Nor is a Codex message said mid-turn, before the turn failed.
    let (_, _, mail) = errand(&mut fleet, "errand-midturn", "thread:midturn");
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert_eq!(mail["urgency"], "high", "{mail}");

    // A Claude run ends more than one turn: the first turn's progress
    // result, followed by the second's work, is not the verdict.
    let (_, settled, mail) = errand(&mut fleet, "errand-multiturn", "thread:multiturn");
    assert_eq!(settled["status"], "failed", "{settled}");
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert!(!body.contains("Started the check"), "{mail}");
    assert_eq!(mail["urgency"], "high", "{mail}");

    // Nor is a mid-turn Codex message the reply file kept once the log's
    // middle, where it was said, is dropped.
    let (_, _, mail) = errand(&mut fleet, "errand-midturn-cut", "thread:midturn-cut");
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert!(!body.contains("Let me run the tests"), "{mail}");
    assert_eq!(mail["urgency"], "high", "{mail}");

    // A whole Claude result the log cut is still delivered from the reply file.
    let (id, settled, mail) = errand(&mut fleet, "errand-blocked-cut", "thread:blocked-cut");
    assert_eq!(settled["status"], "failed", "{settled}");
    assert!(
        mail["body"].as_str().unwrap().starts_with(&format!(
            "Errand on thread:blocked-cut, verdict (unconfirmed; the run ended: permission blocked):\nReviewed: {id}. Brief seen: True\n"
        )),
        "{}",
        &mail["body"].as_str().unwrap()[..300]
    );
    assert_eq!(mail["urgency"], "normal", "{}", mail["urgency"]);

    // A run with no reply at all keeps saying so.
    let (_, _, mail) = errand(&mut fleet, "fail", "thread:silent");
    let body = mail["body"].as_str().unwrap();
    assert!(body.contains("failed without a verdict"), "{mail}");
    assert_eq!(mail["urgency"], "high", "{mail}");
}

/// Tick until no wake of the infra ship is running.
fn quiet_infra(fleet: &mut Fleet) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        fleet.tick();
        if fleet.wakes().iter().all(|w| w["status"] != "running") {
            return;
        }
        assert!(Instant::now() < until, "an infra wake never ended");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_throttled_model_route_backs_off_and_a_success_clears_it() {
    let mut fleet = Fleet::new(now_settings());
    // Crew memory and the free-memory floor are admission's concern, tested
    // apart: these crews only need to start.
    fleet.ok(
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20,"crew_memory_bytes":{"codex":1u64 << 20,"claude":1u64 << 20}}),
        None,
    );
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let ledger_path = fleet.root.join("ledger/resources.sqlite");
    let ledger = || rusqlite::Connection::open(&ledger_path).unwrap();
    let routes =
        |fleet: &Fleet| fleet.ok("resource.status", json!({}), None)["model_routes"].clone();
    let crew = |fleet: &Fleet, id: &str| {
        fleet.run(
            "agent.start",
            json!({"id":id,"task_id":"infra-repair"}),
            Some("infra"),
        )
    };
    let wait_job = |fleet: &Fleet, id: &str| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !matches!(
            fleet.ok("job.status", json!({"id":id}), None)["status"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "timed_out")
        ) {
            assert!(std::time::Instant::now() < until, "job {id} never ended");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    // A run that fails for another reason says nothing about the provider.
    fleet.tick();
    fleet.mode("fail");
    assert!(crew(&fleet, "broken").status.success());
    wait_job(&fleet, "broken");
    fleet.tick();
    assert_eq!(routes(&fleet), json!([]));

    // A run the provider throttled backs its route off.
    fleet.mode("throttle");
    assert!(crew(&fleet, "first").status.success());
    wait_job(&fleet, "first");
    fleet.tick();
    let backed = routes(&fleet);
    // A wake on the same route may also have been throttled meanwhile.
    let first_failures = backed[0]["failures"].as_i64().unwrap();
    assert!(first_failures >= 1, "{backed}");
    assert!(backed[0]["last_error"].as_str().unwrap().starts_with("429"));
    let until = backed[0]["until_ms"].as_i64().unwrap();

    // New crew on that route are refused, and headless wakes wait. An infra
    // wake the run before started in fail mode may still be running, and may
    // have been throttled by now: let every infra wake end before reading why
    // the next one waits.
    let refused = crew(&fleet, "second");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("backing off"));
    quiet_infra(&mut fleet);
    fleet.send(json!({"id":"q","from":"apps","to":"infra","kind":"request","body":"Ready?"}));
    assert_eq!(
        Fleet::infra(&fleet.tick())["skipped"],
        "model route backing off or past its headroom"
    );
    let bearings = fleet.ok("ship.bearings", json!({"ship":"infra"}), Some("infra"));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("is backing off")
    );

    // Throttled again once it is allowed back: the backoff doubles.
    ledger()
        .execute("UPDATE model_routes SET until_ms=0", [])
        .unwrap();
    assert!(crew(&fleet, "third").status.success());
    wait_job(&fleet, "third");
    fleet.tick();
    let backed = routes(&fleet);
    assert!(
        backed[0]["failures"].as_i64().unwrap() > first_failures,
        "{backed}"
    );
    let doubled = backed[0]["until_ms"].as_i64().unwrap() - until;
    assert!(
        doubled >= 25_000,
        "the second backoff was not longer: {doubled} ms"
    );

    // A run that succeeds on the route clears it.
    ledger()
        .execute("UPDATE model_routes SET until_ms=0", [])
        .unwrap();
    fleet.mode("normal");
    fleet.tick();
    fleet.settle();
    fleet.tick();
    assert_eq!(routes(&fleet), json!([]));
    let left: i64 = ledger()
        .query_row("SELECT COUNT(*) FROM model_routes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn a_throttled_wake_with_no_fallback_waits_then_wakes_on_its_own_route() {
    // No routes file: the first mate has no other route to retry on.
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20,"crew_memory_bytes":{"codex":1u64 << 20}}),
        None,
    );
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let ledger_path = fleet.root.join("ledger/resources.sqlite");
    let ledger = || rusqlite::Connection::open(&ledger_path).unwrap();

    fleet.mode("throttle");
    fleet.send(json!({"id":"q","from":"apps","to":"infra","kind":"request","body":"Ready?"}));
    let throttled = Fleet::infra(&fleet.tick());
    assert!(throttled["wake"].is_string(), "{throttled}");
    let failed = fleet.settle();
    assert_eq!(failed["status"], "failed", "{failed}");

    // The retry finds no route, so the wake waits as it does without a
    // failure, on the route that throttled it.
    let waiting = Fleet::infra(&fleet.tick());
    assert_eq!(
        waiting["skipped"], "model route backing off or past its headroom",
        "{waiting}"
    );

    // Once that route's backoff ends, the wake goes out on it, not as a retry.
    ledger()
        .execute("UPDATE model_routes SET until_ms=0", [])
        .unwrap();
    fleet.mode("normal");
    let woken = Fleet::infra(&fleet.tick());
    assert!(woken["wake"].is_string(), "{woken}");
    assert!(woken["retry_of"].is_null(), "{woken}");
    let done = fleet.settle();
    assert_eq!(done["id"], woken["wake"], "{done}");
    assert_eq!(done["status"], "succeeded", "{done}");
}

#[test]
fn a_stated_reset_holds_a_throttled_route_past_the_short_window() {
    let fleet = Fleet::new(now_settings());
    fleet.ok(
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20,"crew_memory_bytes":{"codex":1u64 << 20,"claude":1u64 << 20}}),
        None,
    );
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let crew = |fleet: &Fleet, id: &str| {
        fleet.run(
            "agent.start",
            json!({"id":id,"task_id":"infra-repair"}),
            Some("infra"),
        )
    };
    let wait_job = |fleet: &Fleet, id: &str| {
        let until = Instant::now() + Duration::from_secs(20);
        while !matches!(
            fleet.ok("job.status", json!({"id":id}), None)["status"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "timed_out")
        ) {
            assert!(Instant::now() < until, "job {id} never ended");
            thread::sleep(Duration::from_millis(100));
        }
    };

    // The first pass has nothing to read yet and leaves the scan cursor behind.
    fleet.tick();
    // A throttle that states when the window reopens holds its route until then,
    // rather than for the fifteen minutes an unstated one doubles up to.
    fleet.mode("window");
    assert!(crew(&fleet, "windowed").status.success());
    wait_job(&fleet, "windowed");
    fleet.tick();
    let backed = fleet.ok("resource.status", json!({}), None)["model_routes"].clone();
    assert!(!backed.as_array().unwrap().is_empty(), "{backed}");
    let until = backed[0]["until_ms"].as_i64().unwrap();
    assert!(
        until - privateer::store::Store::now_ms() > 4 * 3_600_000,
        "the stated five-hour reset was not held: {backed}"
    );
    assert!(
        backed[0]["last_error"]
            .as_str()
            .unwrap()
            .contains("resets in 5 hours"),
        "{backed}"
    );

    // So new crew on that route are refused while it waits.
    let refused = crew(&fleet, "later");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("backing off"), "{stderr}");
}

#[test]
fn codex_crew_sandbox_defaults_and_flags_read_only() {
    let fleet = Fleet::new(now_settings());
    fleet.ok(
        "resource.configure",
        json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,"crew_default_memory_bytes":1u64 << 20,"crew_memory_bytes":{"codex":1u64 << 20,"claude":1u64 << 20}}),
        None,
    );
    // An isolated bundled:codex manifest so the test never runs real Codex.
    let plugins = fleet.root.join("plugins");
    fs::create_dir_all(plugins.join("codex")).unwrap();
    fs::write(
        plugins.join("codex/manifest.json"),
        json!({
            "schema_version":1,"id":"codex",
            "executable":[support::python(), fleet.root.join("codex_adapter.py")],
            "timeout_ms":2000,"max_output_bytes":65536
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        fleet.root.join("codex_adapter.py"),
        r#"import json,sys
r=json.loads(sys.stdin.readline())
sandbox=r.get("options",{}).get("sandbox","-")
print(json.dumps({"id":r["id"],"version":1,"ok":True,"result":{"argv":[sys.executable,"-c","import sys;sys.exit(0)","--sandbox",sandbox],"cwd":r["context"]["repo"],"env_names":[]}}))
"#,
    )
    .unwrap();
    let start = |args: Value| {
        let mut cmd = fleet.command();
        cmd.env("PRIVATEER_PLUGINS", &plugins)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "infra")
            .args(["agent.start", "--json", &args.to_string()]);
        cmd.output().unwrap()
    };
    let repo = fleet.root.join("repo");

    let default = start(json!({"id":"codex-default","task_id":"infra-repair",
        "manifest":"bundled:codex","options":{},"cwd":repo}));
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    let job: Value = serde_json::from_slice(&default.stdout).unwrap();
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|p| p == ["--sandbox", "danger-full-access"]),
        "{job}"
    );
    assert_ne!(job["codex_read_only_crew"], json!(true), "{job}");

    let read_only = start(json!({"id":"codex-read-only","task_id":"infra-repair",
        "manifest":"bundled:codex","options":{"sandbox":"read-only"},"cwd":repo}));
    assert!(
        read_only.status.success(),
        "{}",
        String::from_utf8_lossy(&read_only.stderr)
    );
    let job: Value = serde_json::from_slice(&read_only.stdout).unwrap();
    assert!(
        job["command"]
            .as_array()
            .unwrap()
            .windows(2)
            .any(|p| p == ["--sandbox", "read-only"]),
        "{job}"
    );
    assert_eq!(job["codex_read_only_crew"], json!(true), "{job}");

    // Malformed options must reach the clean typed error, never a panic.
    let malformed = start(json!({"id":"codex-malformed","task_id":"infra-repair",
        "manifest":"bundled:codex","options":"nope","cwd":repo}));
    assert!(!malformed.status.success());
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("plugin options must be an object"),
        "{}",
        String::from_utf8_lossy(&malformed.stderr)
    );
}

#[test]
fn wakes_errands_and_crew_record_their_usage_and_route_and_the_summary_sums_them() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "charter.put",
        json!({"id":"fleet","actions":["inspect","spawn","message"]}),
        None,
    );
    let job = |fleet: &Fleet, id: &Value| fleet.ok("job.status", json!({"id": id}), None);
    let wait_job = |fleet: &Fleet, id: &str| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let record = fleet.ok("job.status", json!({"id": id}), None);
            if matches!(
                record["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled" | "timed_out")
            ) {
                return record;
            }
            assert!(std::time::Instant::now() < until, "job {id} never ended");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    // A wake: Codex counts its cached tokens inside its input.
    fleet.send(json!({"id":"q","from":"apps","to":"infra","kind":"request","body":"Healthy?"}));
    fleet.tick();
    let wake = fleet.settle();
    let woke = job(&fleet, &wake["job_id"]);
    assert_eq!(
        woke["usage"],
        json!({"input":200,"output":80,"cacheRead":1000,"cacheWrite":0,"totalTokens":1280}),
        "{woke}"
    );
    let route = woke["model_route"].as_str().unwrap().to_owned();
    let harness = route.split('/').next().unwrap().to_owned();

    // An errand.
    let started = fleet.ok(
        "errand.start",
        json!({"focus":"task:infra-repair","brief":"Review the cache rebuild."}),
        Some("infra"),
    );
    let errand = started["errand"].as_str().unwrap().to_owned();
    assert_eq!(errand_settled(&mut fleet, &errand)["status"], "succeeded");
    let reviewed = job(&fleet, &json!(errand));
    assert_eq!(
        reviewed["usage"],
        json!({"input":900,"output":40,"cacheRead":0,"cacheWrite":0,"totalTokens":940}),
        "{reviewed}"
    );
    assert_eq!(reviewed["model_route"], route.as_str());
    // Its verdict mail may have woken the first mate; let that wake end first.
    fleet.settle();

    // Crew, with no dispatcher pass between them, so no wake joins them: one
    // reports usage, one reports nothing and records none, not zero.
    let crews_from = privateer::store::Store::now_ms();
    fleet.mode("crew");
    let start = |fleet: &Fleet, id: &str| {
        fleet.ok(
            "agent.start",
            json!({"id":id,"task_id":"infra-repair"}),
            Some("infra"),
        )
    };
    start(&fleet, "counted");
    let counted = wait_job(&fleet, "counted");
    assert_eq!(counted["status"], "succeeded", "{counted}");
    assert_eq!(
        counted["usage"],
        json!({"input":200,"output":30,"cacheRead":100,"cacheWrite":0,"totalTokens":330}),
        "{counted}"
    );
    assert_eq!(counted["model_route"], route.as_str());
    fleet.mode("fail");
    start(&fleet, "silent");
    let silent = wait_job(&fleet, "silent");
    assert_eq!(silent["status"], "failed", "{silent}");
    assert!(silent["usage"].is_null(), "{silent}");
    assert_eq!(silent["model_route"], route.as_str());
    let crews_until = privateer::store::Store::now_ms() + 1;

    // The summary reads the records: grouped any way, the two crew are one
    // group here, with the silent one's harness named as a gap.
    let summary = |group_by: &str, since: i64, until: Value| {
        fleet.ok(
            "usage.summary",
            json!({"since_ms": since, "until_ms": until, "group_by": group_by}),
            None,
        )
    };
    let crew_usage =
        json!({"input":200,"output":30,"cacheRead":100,"cacheWrite":0,"totalTokens":330});
    for (group_by, key) in [("route", json!(route)), ("ship", json!("infra"))] {
        let read = summary(group_by, crews_from, json!(crews_until));
        assert_eq!(read["group_by"], group_by);
        assert_eq!(
            read["groups"],
            json!([{"key": key, "jobs": 2, "usage": crew_usage, "gaps": [harness]}]),
            "{read}"
        );
    }
    let by_day = summary("day", crews_from, json!(crews_until));
    let day = by_day["groups"][0]["key"].as_str().unwrap();
    assert_eq!(day.len(), 10, "{by_day}");
    assert_eq!(by_day["groups"][0]["jobs"], 2, "{by_day}");
    let by_machine = summary("machine", crews_from, json!(crews_until));
    assert_eq!(by_machine["groups"].as_array().unwrap().len(), 1);
    assert_eq!(by_machine["machines"][0]["reached"], true, "{by_machine}");

    // The whole period counts the wake and the errand too, summed.
    let all = summary("route", 0, Value::Null);
    assert!(all["until_ms"].is_null());
    let group = &all["groups"][0];
    assert!(group["jobs"].as_u64().unwrap() >= 4, "{all}");
    assert!(
        group["usage"]["totalTokens"].as_u64().unwrap() >= 1280 + 940 + 330,
        "{all}"
    );
    // Bounds: a period that ends before the crew began holds nothing of theirs.
    let before = summary("route", crews_from - 1, json!(crews_from));
    assert_eq!(before["groups"], json!([]), "{before}");

    // The operator's alone.
    let refused = fleet.refused(
        "usage.summary",
        json!({"since_ms": 0, "group_by": "route"}),
        Some("infra"),
    );
    assert!(refused.contains("operator command"), "{refused}");
}

/// Every `wake.started` this fleet's home recorded, oldest first.
fn wakes_started(fleet: &mut Fleet) -> Vec<Value> {
    let mut found = vec![];
    let mut after = 0;
    loop {
        let page = dispatch(
            &mut fleet.store,
            "audit.list",
            json!({"after_seq": after, "limit": 1000}),
        )
        .unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        let Some(last) = page.last() else {
            return found;
        };
        after = last["seq"].as_i64().unwrap();
        found.extend(
            page.iter()
                .filter(|e| e["kind"] == "wake.started")
                .map(|e| e["payload"].clone()),
        );
    }
}

/// Each headless wake records why it woke, by the oldest wake-worthy mail's
/// kind, beside the wake row and in the audit `activity.list` reads.
#[test]
fn a_headless_wake_records_why_it_woke() {
    let mut fleet = Fleet::new(now_settings());
    // Another ship's mail, with a routine notice before it that is no cause.
    fleet.send(
        json!({"id":"fyi","from":"apps","to":"infra","kind":"notice","urgency":"low","body":"FYI"}),
    );
    let asked = fleet
        .send(json!({"id":"ship-ask","from":"apps","to":"infra","kind":"request","body":"Ready?"}));
    let woke = Fleet::infra(&fleet.tick());
    fleet.settle();
    let started = wakes_started(&mut fleet);
    let record = started.last().unwrap();
    assert_eq!(record["wake"], woke["wake"], "{record}");
    assert_eq!(record["ship"], "infra");
    assert_eq!(record["mode"], "fresh");
    assert_eq!(record["reason"], "mail");
    assert_eq!(record["status"], "running");
    assert_eq!(record["through_seq"], asked["seq"]);
    assert_eq!(
        record["causes"],
        json!([{"kind":"mail","from":"apps","mail_id":"ship-ask","seq":asked["seq"],
            "task_id":null,"job_id":null}])
    );
    assert_eq!(record["more"], 0);
    let listed = fleet.wakes()[0].clone();
    assert_eq!(listed["reason"], "mail", "{listed}");
    assert_eq!(listed["cause"]["causes"], record["causes"]);

    // The captain's mail, and the oldest cause names the reason.
    fleet.send(json!({"id":"from-captain","from":"captain","to":"infra","kind":"request","body":"Status?"}));
    fleet.send(
        json!({"id":"after-captain","from":"apps","to":"infra","kind":"request","body":"And?"}),
    );
    fleet.tick();
    fleet.settle();
    let record = wakes_started(&mut fleet).last().unwrap().clone();
    assert_eq!(record["reason"], "captain", "{record}");
    assert_eq!(record["causes"][0]["mail_id"], "from-captain");
    assert_eq!(record["causes"][1]["kind"], "mail");

    // A crew's outcome, as its job's end mails it.
    fleet.mode("crew");
    fleet.ok(
        "agent.start",
        json!({"id":"crew-reason","task_id":"infra-repair"}),
        Some("infra"),
    );
    let until = Instant::now() + Duration::from_secs(30);
    let record = loop {
        fleet.tick();
        if let Some(record) = wakes_started(&mut fleet)
            .into_iter()
            .find(|r| r["reason"] == "crew_outcome")
        {
            break record;
        }
        assert!(Instant::now() < until, "no crew outcome wake");
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(record["causes"][0]["from"], "crew", "{record}");
    assert_eq!(record["causes"][0]["job_id"], "crew-reason");
    assert_eq!(record["causes"][0]["task_id"], "infra-repair");
    fleet.mode("normal");
    fleet.settle();
}

/// The cause column reaches a wakes table made before it, and a wake from
/// before it lists with no cause.
#[test]
fn the_wake_cause_column_is_added_to_an_existing_home() {
    use privateer::store::Store;
    let mut fleet = Fleet::new(now_settings());
    fleet
        .store
        .connection()
        .execute_batch(
            "INSERT INTO wakes(id, ship, focus, through_seq, mode, status, created_ms)
               VALUES('legacy', 'infra', 'ship', 0, 'fresh', 'succeeded', 1);
             ALTER TABLE wakes DROP COLUMN cause;",
        )
        .unwrap();
    let present = |store: &Store| -> bool {
        store
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('wakes') WHERE name='cause')",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(!present(&fleet.store));
    fleet.store = Store::open(&fleet.home).unwrap();
    assert!(present(&fleet.store));
    let legacy = fleet.wakes()[0].clone();
    assert_eq!(legacy["id"], "legacy");
    assert!(
        legacy["cause"].is_null() && legacy["reason"].is_null(),
        "{legacy}"
    );
    fleet.send(
        json!({"id":"after-upgrade","from":"apps","to":"infra","kind":"request","body":"Hi"}),
    );
    fleet.tick();
    assert_eq!(fleet.settle()["reason"], "mail");
}

/// A hosted pane's turn records why it began, with no wake row: the hourly
/// wake budget, work offers and headless wakes never count it. Only the
/// ship's own hosted session, holding the lease, may record it.
#[test]
fn a_hosted_pane_records_why_it_woke_without_spending_the_wake_budget() {
    let mut fleet = Fleet::new(now_settings());
    fleet.ok(
        "ship.attend",
        json!({"ship":"infra","holder":"host:pane","ttl_secs":90}),
        None,
    );
    let first =
        fleet.send(json!({"id":"pane-1","from":"apps","to":"infra","kind":"request","body":"One"}));
    fleet.send(json!({"id":"pane-low","from":"apps","to":"infra","kind":"notice","urgency":"low","body":"FYI"}));
    let last = fleet
        .send(json!({"id":"pane-2","from":"captain","to":"infra","kind":"request","body":"Two"}));
    let through = last["seq"].as_i64().unwrap();
    let args = |holder: &str, trigger: &str, after: Value, through: Value| json!({"ship":"infra","holder":holder,"trigger":trigger,"after_seq":after,"through_seq":through});
    let before = wakes_started(&mut fleet).len();
    for (refused, why, actor) in [
        (
            args("host:other", "mail", json!(0), json!(through)),
            "current hosted presence lease",
            Some("infra"),
        ),
        (
            args("start:pane", "mail", json!(0), json!(through)),
            "current hosted presence lease",
            Some("infra"),
        ),
        (
            args("host:pane", "mail", json!(0), json!(through)),
            "cannot act as",
            Some("apps"),
        ),
        (
            args("host:pane", "mail", json!(0), json!(through)),
            "hosted first mate's own",
            None,
        ),
        (
            args("host:pane", "mail", json!(through), json!(0)),
            "must not exceed",
            Some("infra"),
        ),
        (
            args("host:pane", "mail", json!(-1), json!(through)),
            "non-negative",
            Some("infra"),
        ),
        (
            args("host:pane", "mail", json!(0), Value::Null),
            "or neither",
            Some("infra"),
        ),
        (
            args("host:pane", "mail", Value::Null, Value::Null),
            "names after_seq",
            Some("infra"),
        ),
        (
            args("host:pane", "nap", json!(0), json!(through)),
            "trigger must be",
            Some("infra"),
        ),
    ] {
        let error = fleet.refused("wake.hosted", refused.clone(), actor);
        assert!(error.contains(why), "{refused}: {error}");
    }
    assert_eq!(wakes_started(&mut fleet).len(), before);

    let recorded = fleet.ok(
        "wake.hosted",
        args("host:pane", "mail", json!(0), json!(through)),
        Some("infra"),
    );
    assert_eq!(recorded, json!({"recorded": true, "reason": "mail"}));
    let started = wakes_started(&mut fleet);
    assert_eq!(started.len(), before + 1);
    let record = &started[before];
    assert_eq!(record["mode"], "hosted", "{record}");
    assert!(record["wake"].is_null());
    assert_eq!(record["trigger"], "mail");
    assert_eq!(record["after_seq"], 0);
    assert_eq!(record["through_seq"], through);
    let causes: Vec<(&str, &str)> = record["causes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["mail_id"].as_str().unwrap(), c["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(causes, [("pane-1", "mail"), ("pane-2", "captain")]);
    // Only after the first: the captain's.
    let later = fleet.ok(
        "wake.hosted",
        args("host:pane", "mail", first["seq"].clone(), json!(through)),
        Some("infra"),
    );
    assert_eq!(later["reason"], "captain");
    // The captain's prompt, with or without mail beside it, and a follow-up.
    for (trigger, after, upto, reason) in [
        ("captain", json!(0), json!(through), "captain_pane"),
        ("captain", Value::Null, Value::Null, "captain_pane"),
        ("follow", Value::Null, Value::Null, "follow_up"),
    ] {
        let reply = fleet.ok(
            "wake.hosted",
            args("host:pane", trigger, after, upto),
            Some("infra"),
        );
        assert_eq!(reply["reason"], reason);
    }
    let started = wakes_started(&mut fleet);
    assert_eq!(started.len(), before + 5);
    assert_eq!(started[before + 2]["causes"].as_array().unwrap().len(), 2);
    assert_eq!(started[before + 3]["causes"], json!([]));
    // No wake row, so the hourly budget is untouched.
    assert!(fleet.wakes().is_empty());
}
