//! Crew park and resume: a crew ends its turn on a heavy job it started, and
//! its outcome resumes the same native session (or a fresh crew with a
//! handoff). Real supervised jobs, the real CLI, and a fake `claude` that
//! records its launches.

mod support;

use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const SESSION: &str = "7d1b1f0e-1111-4222-8333-444455556666";
/// See `Fleet::launch_budget`.
const LAUNCH_BUDGET_SECS: u64 = 20;

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("engineering")).unwrap();
        let fleet = Self { _tmp: tmp, root };
        // The fake claude. Its mode file picks what the crew does: park on a
        // child (session, nosession, long, gated), start a child and run out
        // its budget (deadline), or run out with nothing outstanding (held). A
        // resumed run is recognised by its job id, and completes its turn. A
        // gated child runs until the test releases it (`release_child`).
        let script = format!(
            r#"#!/bin/sh
ROOT='{root}'
BIN='{bin}'
gate="while [ ! -e $ROOT/release-child ]; do [ -d $ROOT ] || exit 1; sleep 0.1; done; echo built"
printf 'LAUNCH %s\n' "$(printf '%s ' "$*" | tr '\n' ' ')" >> "$ROOT/launches.log"
MODE=$(cat "$ROOT/mode" 2>/dev/null)
if [ "$MODE" = chain ]; then
  # Every launch parks on its own child, up to six launches; the seventh (the
  # sixth resume) finishes its turn.
  N=$(wc -l < "$ROOT/launches.log" | tr -d ' ')
  printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
  if [ "$N" -gt 6 ]; then
    printf '{{"type":"result","subtype":"success","is_error":false,"session_id":"{session}"}}\n'
    exit 0
  fi
  "$BIN" job.start --json "{{\"id\":\"child-$N\",\"task_id\":\"fix\",\"command\":[\"sh\",\"-c\",\"sleep 3; echo built\"],\"heavy\":true}}" >>"$ROOT/steps.log" 2>&1 || exit 3
  "$BIN" crew.park --json "{{\"job_id\":\"child-$N\",\"next\":\"check the build output\"}}" >>"$ROOT/steps.log" 2>&1 || exit 4
  sleep 60
  exit 0
fi
case "$PRIVATEER_JOB" in
  crew-resume-*)
    printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
    printf '{{"type":"result","subtype":"success","is_error":false,"session_id":"{session}"}}\n'
    if [ "$MODE" = aged_hold ]; then sleep 60; fi
    exit 0 ;;
esac
if [ "$MODE" != nosession ]; then
  printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
fi
case "$MODE" in
  held) sleep 60 ;;
  late)
    sleep 3
    "$BIN" job.start --json '{{"id":"child-1","task_id":"fix","command":["sh","-c","sleep 6; echo built"],"heavy":true}}' >>"$ROOT/steps.log" 2>&1 || exit 3
    sleep 60 ;;
  deadline)
    "$BIN" job.start --json "{{\"id\":\"child-1\",\"task_id\":\"fix\",\"command\":[\"sh\",\"-c\",\"$gate\"],\"heavy\":true}}" >>"$ROOT/steps.log" 2>&1 || exit 3
    sleep 60 ;;
  *)
    child='sleep 2; echo built'
    if [ "$MODE" = slow ] || [ "$MODE" = aged ] || [ "$MODE" = aged_hold ]; then child='sleep 6; echo built'; fi
    if [ "$MODE" = gated ]; then child=$gate; fi
    if [ "$MODE" = aged ] || [ "$MODE" = aged_hold ]; then sleep 5; fi
    if [ "$MODE" = long ]; then child='sleep 60'; fi
    "$BIN" job.start --json "{{\"id\":\"child-1\",\"task_id\":\"fix\",\"command\":[\"sh\",\"-c\",\"$child\"],\"heavy\":true}}" >>"$ROOT/steps.log" 2>&1 || exit 3
    "$BIN" crew.park --json '{{"job_id":"child-1","next":"check the build output"}}' >>"$ROOT/steps.log" 2>&1 || exit 4
    sleep 60 ;;
esac
"#,
            root = fleet.root.display(),
            bin = BIN,
            session = SESSION,
        );
        let path = fleet.root.join("bin/claude");
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        let codex = fleet.root.join("bin/codex");
        fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(codex, fs::Permissions::from_mode(0o700)).unwrap();
        fleet.ok(
            None,
            &["init", "--ship", "engineering", "--harness", "codex"],
        );
        fleet.json(
            None,
            "task.create",
            json!({"id":"fix","charter_id":"engineering","owner":"engineering","title":"Fix it"}),
        );
        fleet.json(
            None,
            "resource.configure",
            json!({"min_available_memory_bytes":0,"min_free_disk_bytes":0,
                "max_load_per_cpu":1024,"ramp_secs":0,
                "crew_memory_bytes":{"claude":1u64 << 20,"codex":1u64 << 20},
                "heavy_estimate":{"disk_bytes":1u64 << 20}}),
        );
        fleet
    }
    fn set_mode(&self, mode: &str) {
        fs::write(self.root.join("mode"), mode).unwrap();
    }
    /// Put a crew's park into a state a crash would leave it in.
    fn set_park(&self, crew: &str, state: &str, attempt: i64, retries: i64) {
        let conn =
            rusqlite::Connection::open(self.root.join("engineering/.privateer/privateer.sqlite"))
                .unwrap();
        let changed = conn
            .execute(
                "UPDATE crew_parks SET state=?2, attempt=?3, retries=?4 WHERE crew_job_id=?1",
                rusqlite::params![crew, state, attempt, retries],
            )
            .unwrap();
        assert_eq!(changed, 1, "no park for {crew}");
    }
    /// Start or stop the dispatcher service, whose passes run the recovery.
    fn service(&self, verb: &str) {
        let out = self
            .command(None, &["service", verb])
            .env("PRIVATEER_SERVICE", "detached")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "service {verb}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    /// The task's routes, as the captain lists them.
    fn routes(&self, routes: Value) {
        let file = self.root.join("routes.json");
        fs::write(&file, routes.to_string()).unwrap();
        self.ok(None, &["routes", "set", file.to_str().unwrap()]);
    }
    /// Give the ship's charter an 8 second job budget, so a crew's budget runs
    /// out while its heavy job still runs.
    fn short_budget(&self) {
        self.budget(8);
    }
    /// A crew budget long enough that a crew launched on a loaded machine
    /// still starts its child well before its deadline.
    fn launch_budget(&self) {
        self.budget(LAUNCH_BUDGET_SECS);
    }
    fn budget(&self, secs: u64) {
        self.json(
            None,
            "charter.put",
            json!({"id": "engineering", "job_timeout_secs": secs}),
        );
    }
    fn command(&self, agent: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(self.root.join("engineering"))
            .args(args)
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env(
                "PRIVATEER_PLUGINS",
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        if let Some(agent) = agent {
            c.env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", agent);
        }
        c
    }
    fn run(&self, agent: Option<&str>, args: &[&str]) -> std::process::Output {
        self.command(agent, args).output().unwrap()
    }
    fn ok(&self, agent: Option<&str>, args: &[&str]) -> String {
        let o = self.run(agent, args);
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn json(&self, agent: Option<&str>, verb: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(agent, &[verb, "--json", &args.to_string()])).unwrap()
    }
    /// Wait until a job reaches one of the accepted statuses.
    fn wait(&self, id: &str, accepted: &[&str]) -> Value {
        let until = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            // A job that is not created yet is not an error to wait through.
            let out = self.run(
                None,
                &["job.status", "--json", &json!({"id": id}).to_string()],
            );
            let job: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            if out.status.success() && accepted.contains(&job["status"].as_str().unwrap_or("")) {
                return job;
            }
            assert!(
                Instant::now() < until,
                "job {id} never reached {accepted:?}: {job}\nsteps: {}\nnotices: {:?}",
                self.steps(),
                self.ship_notices()
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    /// Wait until a crew has opened its park. The park is open while the
    /// crew's payload ends its turn (the park grace), before its job reads
    /// `parked`, so this does not spend that grace.
    fn wait_park(&self, crew: &str) {
        let until = Instant::now() + std::time::Duration::from_secs(60);
        while !self.json(None, "job.status", json!({"id": crew}))["park"].is_string() {
            assert!(Instant::now() < until, "{crew} never parked");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    /// A column of a crew's park row.
    fn park_field(&self, crew: &str, column: &str) -> rusqlite::types::Value {
        let conn =
            rusqlite::Connection::open(self.root.join("engineering/.privateer/privateer.sqlite"))
                .unwrap();
        conn.query_row(
            &format!("SELECT {column} FROM crew_parks WHERE crew_job_id=?1"),
            [crew],
            |r| r.get(0),
        )
        .unwrap()
    }
    fn steps(&self) -> String {
        fs::read_to_string(self.root.join("steps.log")).unwrap_or_default()
    }
    fn launches(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("launches.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    /// Every notice the first mate has been sent, as text.
    fn ship_notices(&self) -> Vec<String> {
        let inbox = self.json(
            None,
            "mail.inbox",
            json!({"address": "engineering", "all": true, "limit": 1000}),
        );
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["body"].as_str().map(str::to_owned))
            .collect()
    }
}

fn start_crew(f: &Fleet, id: &str) {
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":id,"manifest":"bundled:claude"}),
    );
}

/// The crew parks, its child settles, and the crew resumes the same session
/// with the outcome as its prompt. The first mate is not told about the child.
#[test]
fn a_parked_crew_resumes_its_session_with_the_outcome() {
    let f = Fleet::new();
    f.set_mode("session");
    start_crew(&f, "crew-1");
    // The resume can follow at once, so the park may already be past.
    let parked = f.wait("crew-1", &["parked", "resumed"]);
    assert_eq!(parked["park"], "park-crew-1-1", "{parked}");
    f.wait("crew-resume-park-crew-1-1-1", &["succeeded"]);
    let resumed = f.wait("crew-1", &["resumed"]);
    assert_eq!(resumed["reason"], "resumed as crew-resume-park-crew-1-1-1");
    // The crew was told to park by the reply to its own job.start.
    assert!(
        f.steps().contains("Do not poll for this job"),
        "{}",
        f.steps()
    );
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
    assert!(
        launches[1].contains("Job child-1 succeeded"),
        "{launches:?}"
    );
    let notices = f.ship_notices();
    assert!(
        notices.iter().any(|b| b.contains("parked on job child-1")),
        "{notices:?}"
    );
    assert!(
        !notices
            .iter()
            .any(|b| b.contains("Crew job child-1 succeeded")),
        "the first mate must not hear the child's outcome: {notices:?}"
    );
}

/// With no session to resume, the crew starts fresh, and its brief carries the
/// park's next step and the outcome of the job it waited on.
#[test]
fn a_parked_crew_without_a_session_starts_fresh_with_a_handoff() {
    let f = Fleet::new();
    f.set_mode("nosession");
    start_crew(&f, "crew-2");
    // The resume can follow at once, so the park may already be past.
    f.wait("crew-2", &["parked", "resumed"]);
    f.wait("crew-resume-park-crew-2-1-1", &["succeeded"]);
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(!launches[1].contains("--resume"), "{launches:?}");
    assert!(
        launches[1].contains("check the build output"),
        "{launches:?}"
    );
    assert!(
        launches[1].contains("Job child-1 succeeded"),
        "{launches:?}"
    );
}

/// Cancelling a parked crew ends the crew and the heavy job it waits on, and
/// it does not resume.
#[test]
fn cancelling_a_parked_crew_cancels_its_job_and_does_not_resume() {
    let f = Fleet::new();
    f.set_mode("long");
    start_crew(&f, "crew-3");
    f.wait("crew-3", &["parked"]);
    let cancelled = f.json(None, "job.cancel", json!({"id": "crew-3"}));
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    f.wait("child-1", &["cancelled"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
}

/// The crew's budget runs out while its heavy job still runs: the crew parks
/// on that job instead of timing out, and the job's outcome (it times out at
/// its own deadline, after the crew's) resumes it. The first mate hears
/// nothing of the child.
#[test]
fn a_crew_that_runs_out_of_budget_parks_on_its_job_and_resumes() {
    let f = Fleet::new();
    f.set_mode("deadline");
    f.launch_budget();
    start_crew(&f, "crew-4");
    // The resume can follow at once, so the park may already be past.
    let parked = f.wait("crew-4", &["parked", "resumed"]);
    assert_eq!(parked["park"], "park-crew-4-1", "{parked}");
    f.wait("crew-resume-park-crew-4-1-1", &["succeeded"]);
    let resumed = f.wait("crew-4", &["resumed"]);
    assert_eq!(resumed["reason"], "resumed as crew-resume-park-crew-4-1-1");
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
    assert!(
        launches[1].contains("Job child-1 timed_out"),
        "{launches:?}"
    );
    let notices = f.ship_notices();
    assert!(
        !notices.iter().any(|b| b.contains("Crew job child-1")),
        "the first mate must not hear the child's outcome: {notices:?}"
    );
}

/// A crew at its budget with no heavy job outstanding is held. Its first mate
/// sees it in bearings, and resumes it with crew.resume; a second resume finds
/// nothing held.
#[test]
fn a_crew_with_no_job_outstanding_is_held_until_its_first_mate_resumes_it() {
    let f = Fleet::new();
    f.set_mode("held");
    f.short_budget();
    start_crew(&f, "crew-5");
    f.wait("crew-5", &["parked"]);
    let bearings = f.json(None, "ship.bearings", json!({"ship": "engineering"}));
    assert!(
        bearings["text"]
            .as_str()
            .unwrap()
            .contains("Crew crew-5 is held"),
        "{bearings}"
    );
    let resumed = f.json(None, "crew.resume", json!({"task_id": "fix"}));
    assert_eq!(resumed["resumed"][0]["park"], "park-crew-5-1", "{resumed}");
    f.wait("crew-resume-park-crew-5-1-1", &["succeeded"]);
    f.wait("crew-5", &["resumed"]);
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
    let again = f.run(
        None,
        &[
            "crew.resume",
            "--json",
            &json!({"task_id": "fix"}).to_string(),
        ],
    );
    assert!(!again.status.success(), "a second resume found a held crew");
}

/// A held crew is live work: its task is neither "running with nothing
/// moving it" in bearings nor a backlog the dispatcher wakes the first mate
/// about. Once the crew ends, the task is both.
#[test]
fn a_held_crew_keeps_its_task_off_the_backlog_until_it_ends() {
    let f = Fleet::new();
    f.set_mode("held");
    f.short_budget();
    start_crew(&f, "crew-6");
    f.wait("crew-6", &["parked"]);
    f.json(
        None,
        "task.update",
        json!({"id":"fix","actor":"engineering","status":"running"}),
    );
    let db =
        rusqlite::Connection::open(f.root.join("engineering/.privateer/privateer.sqlite")).unwrap();
    let unfinished = || -> Vec<(String, String)> {
        let mut statement = db
            .prepare(
                "SELECT json_extract(record,'$.id'), json_extract(record,'$.status') FROM jobs
                 WHERE json_extract(record,'$.status') NOT IN ('succeeded','failed','cancelled','timed_out','resumed')",
            )
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    // Nothing else runs: the held crew is the only unfinished job.
    assert_eq!(unfinished(), [("crew-6".to_owned(), "parked".to_owned())]);
    let idle = |f: &Fleet| {
        f.json(None, "ship.bearings", json!({"ship": "engineering"}))["idle_tasks"].clone()
    };
    assert_eq!(idle(&f), json!([]));
    // The first mate has read its mail, so no wake is due either.
    let read_all = |f: &Fleet| {
        let seq = f.json(
            None,
            "mail.inbox",
            json!({"address": "engineering", "all": true, "limit": 1000}),
        )["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["seq"].as_i64())
            .max()
            .unwrap_or(0);
        f.json(
            None,
            "mail.ack",
            json!({"address": "engineering", "through_seq": seq}),
        );
    };
    read_all(&f);
    let backlog = |f: &Fleet| -> Value {
        let report: Value =
            serde_json::from_str(&f.ok(None, &["serve", "--json", r#"{"once":true}"#])).unwrap();
        report["backlog"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["ship"] == "engineering")
            .cloned()
            .unwrap_or_else(|| panic!("{report}"))
    };
    assert_eq!(backlog(&f)["skipped"], "first backlog check");
    db.execute(
        "UPDATE backlog_alarms SET fired_ms=fired_ms-3600001, next_ms=next_ms-3600001",
        [],
    )
    .unwrap();
    let held = backlog(&f);
    assert_eq!(held["skipped"], "work is live", "{held}");
    // The crew ends; once its outcome is read, nothing moves the running task.
    f.json(None, "job.cancel", json!({"id": "crew-6"}));
    f.wait("crew-6", &["cancelled"]);
    read_all(&f);
    assert_eq!(idle(&f)[0]["id"], "fix");
    let fired = backlog(&f);
    assert_eq!(fired["fired"], "backlog-engineering-1", "{fired}");
    assert_eq!(fired["considered"], json!(["fix"]));
    let body: String = db
        .query_row(
            "SELECT body FROM mail WHERE id='backlog-engineering-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        body.contains("- fix: Fix it [running, nothing moving it]"),
        "{body}"
    );
}

/// A task with a crew parked on its heavy job takes no second launch unless
/// the launch replaces it, which cancels the crew and the job it waits on.
#[test]
fn a_launch_on_a_task_with_a_parked_crew_is_refused_unless_it_replaces_it() {
    let f = Fleet::new();
    f.set_mode("long");
    start_crew(&f, "crew-6");
    f.wait("crew-6", &["parked"]);
    let refused = f.run(
        Some("engineering"),
        &[
            "agent.start",
            "--json",
            &json!({"task_id": "fix", "id": "crew-7", "manifest": "bundled:claude"}).to_string(),
        ],
    );
    assert!(
        !refused.status.success(),
        "a second crew launched beside a parked one"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("parked crew"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-8", "manifest": "bundled:claude", "replace": true}),
    );
    f.wait("crew-6", &["cancelled"]);
    f.wait("child-1", &["cancelled"]);
}

/// Parked time is not charged to the crew's budget. The crew works for 5
/// seconds, parks on a 6 second job, and so spends 11 seconds in all against
/// an 8 second budget. The resume gets a fresh budget and completes.
#[test]
fn parked_time_is_not_charged_to_the_crew_budget() {
    let f = Fleet::new();
    f.set_mode("aged");
    f.short_budget();
    start_crew(&f, "crew-9");
    f.wait("crew-9", &["parked"]);
    f.wait("child-1", &["succeeded"]);
    f.wait("crew-resume-park-crew-9-1-1", &["succeeded"]);
    let resumed = f.wait("crew-9", &["resumed"]);
    assert_eq!(resumed["reason"], "resumed as crew-resume-park-crew-9-1-1");
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
}

/// The dispatcher is stopped while the crew's heavy job runs and is started
/// again after the job ends. The park survives the outage, and the crew
/// resumes exactly once.
#[test]
fn a_park_survives_a_dispatcher_restart_and_resumes_once() {
    let f = Fleet::new();
    f.set_mode("slow");
    let home = f.root.join("engineering/.privateer");
    fs::write(
        home.join("config.json"),
        json!({"ship": "engineering", "resource_home": f.root.join("ledger")}).to_string(),
    )
    .unwrap();
    let service = |verb: &str| {
        let out = f
            .command(None, &["service", verb])
            .env("PRIVATEER_SERVICE", "detached")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "service {verb}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    service("start");
    start_crew(&f, "crew-10");
    f.wait("crew-10", &["parked"]);
    service("stop");
    assert!(!privateer::service::running(&home).unwrap());
    // The heavy job ends while no dispatcher is running.
    f.wait("child-1", &["succeeded"]);
    service("start");
    f.wait("crew-10", &["resumed"]);
    f.wait("crew-resume-park-crew-10-1-1", &["succeeded"]);
    std::thread::sleep(std::time::Duration::from_millis(2500)); // Two more passes.
    service("stop");
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let resumes = f.json(None, "job.list", json!({}))["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| {
            j["id"]
                .as_str()
                .unwrap_or("")
                .starts_with("crew-resume-park-crew-10")
        })
        .count();
    assert_eq!(resumes, 1);
}

/// A stow holds a crew's resume like any other launch. The heavy job ends
/// while the machine is stowed, and the crew stays parked with no launch.
/// Once the stow is lifted, the next tick resumes it.
#[test]
fn a_resume_held_by_a_stow_goes_through_admission() {
    let f = Fleet::new();
    f.set_mode("slow");
    start_crew(&f, "crew-11");
    f.wait("crew-11", &["parked"]);
    f.ok(None, &["stow"]);
    f.wait("child-1", &["succeeded"]);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
    // The resume joins the queue behind the stow: it is recorded, not launched.
    f.wait("crew-resume-park-crew-11-1-1", &["queued"]);
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
    f.ok(None, &["stow", "resume"]);
    f.ok(
        None,
        &["serve", "--json", &json!({"once": true}).to_string()],
    );
    f.wait("crew-resume-park-crew-11-1-1", &["succeeded"]);
    f.wait("crew-11", &["resumed"]);
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
}

/// The incident replay: the heavy job ends after the crew's deadline. The
/// crew resumes, the first mate gets no failure, and a second start for the
/// task is refused while the crew is parked, so no duplicate start happens.
#[test]
fn a_heavy_job_that_ends_after_the_crew_deadline_resumes_without_a_duplicate() {
    let f = Fleet::new();
    f.set_mode("late");
    f.short_budget();
    start_crew(&f, "crew-12");
    // The crew starts a 6 second job at 3 seconds. Its 8 second budget runs
    // out while the job is still going, so it parks, and the job ends after
    // the crew's deadline.
    f.wait_park("crew-12");
    let refused = f.run(
        Some("engineering"),
        &[
            "agent.start",
            "--json",
            &json!({"task_id": "fix", "id": "crew-dup", "manifest": "bundled:claude"}).to_string(),
        ],
    );
    assert!(
        !refused.status.success(),
        "a duplicate start ran beside a parked crew"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("parked crew"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    f.wait("child-1", &["succeeded"]);
    f.wait("crew-resume-park-crew-12-1-1", &["succeeded"]);
    f.wait("crew-12", &["resumed"]);
    let launches = f.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(
        launches[1].contains(&format!("--resume {SESSION}")),
        "{launches:?}"
    );
    let notices = f.ship_notices();
    assert!(
        !notices
            .iter()
            .any(|b| b.contains("crew-12") && b.to_lowercase().contains("fail")),
        "the first mate got a failure: {notices:?}"
    );
    assert!(
        !notices.iter().any(|b| b.contains("crew-dup")),
        "a duplicate start reached the first mate: {notices:?}"
    );
}

/// Closing a task ends its parked crew as cancelled, and cancels the heavy job
/// it waits on. The first mate hears no crew failure.
#[test]
fn closing_the_task_ends_a_parked_crew_and_its_job_quietly() {
    let f = Fleet::new();
    f.set_mode("long");
    start_crew(&f, "crew-20");
    f.wait("crew-20", &["parked"]);
    f.json(
        None,
        "task.update",
        json!({"actor": "engineering", "id": "fix", "status": "cancelled"}),
    );
    let crew = f.wait("crew-20", &["cancelled"]);
    assert_eq!(crew["reason"], "task fix is cancelled", "{crew}");
    f.wait("child-1", &["cancelled"]);
    // The owner closed the task, so neither cancellation is mailed back to it.
    let notices = f.ship_notices();
    assert!(
        !notices.iter().any(|b| b.contains("crew-20 cancelled")
            || b.contains("child-1 cancelled")
            || b.to_lowercase().contains("fail")),
        "the owner that closed the task was sent a notice: {notices:?}"
    );
}

/// crew.resume refuses a held crew whose caps are spent, and says why; the
/// crew stays held and nothing launches.
#[test]
fn crew_resume_is_refused_once_the_crews_caps_are_spent() {
    let f = Fleet::new();
    f.set_mode("held");
    f.short_budget();
    start_crew(&f, "crew-29");
    f.wait("crew-29", &["parked"]);
    f.json(
        None,
        "charter.put",
        json!({"id": "engineering", "job_timeout_secs": 8, "crew_max_parks": 0}),
    );
    let refused = f.run(
        None,
        &[
            "crew.resume",
            "--json",
            &json!({"task_id": "fix"}).to_string(),
        ],
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "a spent crew was resumed");
    assert!(
        stderr.contains("crew.resume refused") && stderr.contains("the cap is 0"),
        "{stderr}"
    );
    assert_eq!(f.wait("crew-29", &["parked"])["status"], "parked");
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
}

/// A held crew past its elapsed cap is ended by the dispatcher's pass, like a
/// parked one, and its first mate is told why.
#[test]
fn a_held_crew_past_its_elapsed_cap_ends() {
    let f = Fleet::new();
    f.set_mode("held");
    f.short_budget();
    let home = f.root.join("engineering/.privateer");
    fs::write(
        home.join("config.json"),
        json!({"ship": "engineering", "resource_home": f.root.join("ledger")}).to_string(),
    )
    .unwrap();
    start_crew(&f, "crew-30");
    f.wait("crew-30", &["parked"]);
    // The crew has run for at least its 8 second budget already.
    f.json(
        None,
        "charter.put",
        json!({"id": "engineering", "job_timeout_secs": 8, "crew_max_elapsed_secs": 5}),
    );
    f.service("start");
    let crew = f.wait("crew-30", &["cancelled"]);
    f.service("stop");
    assert!(
        crew["reason"]
            .as_str()
            .unwrap_or("")
            .contains("more than 5 seconds in total; the park ends"),
        "{crew}"
    );
    let notices = f.ship_notices();
    assert!(
        notices.iter().any(|b| b.contains("crew-30 cancelled")),
        "the first mate was not told the crew ended: {notices:?}"
    );
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
}

/// A spent park cap at the deadline ends the crew with the reason, and a
/// crew with a heavy job outstanding has that job cancelled too.
#[test]
fn a_spent_park_cap_at_the_deadline_ends_the_crew_and_its_job() {
    let f = Fleet::new();
    f.set_mode("held");
    f.json(
        None,
        "charter.put",
        json!({"id": "engineering", "job_timeout_secs": 8, "crew_max_parks": 0}),
    );
    start_crew(&f, "crew-21");
    let crew = f.wait("crew-21", &["timed_out"]);
    assert!(
        crew["reason"]
            .as_str()
            .unwrap_or("")
            .contains("the cap is 0"),
        "{crew}"
    );
}

#[test]
fn a_spent_park_cap_cancels_the_job_a_crew_would_have_waited_on() {
    let f = Fleet::new();
    f.set_mode("deadline");
    // The child must be running by the crew's deadline for it to be cancelled.
    f.json(
        None,
        "charter.put",
        json!({"id": "engineering", "job_timeout_secs": LAUNCH_BUDGET_SECS, "crew_max_parks": 0}),
    );
    start_crew(&f, "crew-26");
    let crew = f.wait("crew-26", &["timed_out"]);
    assert!(
        crew["reason"]
            .as_str()
            .unwrap_or("")
            .contains("the cap is 0"),
        "{crew}"
    );
    f.wait("child-1", &["cancelled"]);
}

/// An identical repeat of a parked crew's start returns the job it already
/// started; only a new launch on the task is refused.
#[test]
fn an_identical_repeat_of_a_parked_crews_start_returns_its_job() {
    let f = Fleet::new();
    f.set_mode("long");
    start_crew(&f, "crew-22");
    f.wait("crew-22", &["parked"]);
    let again = f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id": "fix", "id": "crew-22", "manifest": "bundled:claude"}),
    );
    assert_eq!(again["id"], "crew-22", "{again}");
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
}

/// A resume that died mid-launch is recovered by the dispatcher: its one
/// retry runs under the same resume id, and the crew resumes.
#[test]
fn a_resume_stuck_in_resuming_is_retried_once_under_its_own_id() {
    let f = Fleet::new();
    f.set_mode("slow");
    let home = f.root.join("engineering/.privateer");
    fs::write(
        home.join("config.json"),
        json!({"ship": "engineering", "resource_home": f.root.join("ledger")}).to_string(),
    )
    .unwrap();
    start_crew(&f, "crew-23");
    f.wait_park("crew-23");
    // The launch of attempt 1 died before its job was recorded. This is set
    // while the child still runs, so its settlement (which resumes only a
    // `parked` park) launches nothing, and no job exists under that id.
    f.set_park("crew-23", "resuming", 1, 0);
    f.wait("child-1", &["succeeded"]);
    assert!(
        f.run(
            None,
            &[
                "job.status",
                "--json",
                r#"{"id":"crew-resume-park-crew-23-1-1"}"#
            ]
        )
        .status
        .code()
            != Some(0),
        "the resume was launched before recovery"
    );
    f.service("start");
    f.wait("crew-resume-park-crew-23-1-1", &["succeeded"]);
    f.wait("crew-23", &["resumed"]);
    f.service("stop");
    // Recovery retried it once, under the same id.
    assert_eq!(
        f.park_field("crew-23", "retries"),
        rusqlite::types::Value::Integer(1)
    );
    assert_eq!(f.launches().len(), 2, "{:?}", f.launches());
}

/// A resume whose launch is still in flight (its claimer, the child's
/// supervisor, waits on the workspace lock inside agent.start, before the
/// resume job is recorded) is left alone by recovery: no retry and no failure,
/// and the crew resumes once when the launch lands.
#[test]
fn recovery_leaves_a_resume_whose_launch_is_still_in_flight() {
    let f = Fleet::new();
    f.set_mode("slow");
    let home = f.root.join("engineering/.privateer");
    fs::write(
        home.join("config.json"),
        json!({"ship": "engineering", "resource_home": f.root.join("ledger")}).to_string(),
    )
    .unwrap();
    start_crew(&f, "crew-28");
    f.wait_park("crew-28");
    // Hold the workspace lock, so the resume's launch blocks inside agent.start.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("workspace.lock"))
        .unwrap();
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    f.service("start");
    f.wait("child-1", &["succeeded"]);
    let until = Instant::now() + std::time::Duration::from_secs(30);
    while f.park_field("crew-28", "state") != rusqlite::types::Value::Text("resuming".into()) {
        assert!(Instant::now() < until, "the resume was never claimed");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Several dispatcher passes (one every 2 s) see it resuming with no job.
    std::thread::sleep(std::time::Duration::from_secs(9));
    assert_eq!(
        f.park_field("crew-28", "retries"),
        rusqlite::types::Value::Integer(0),
        "recovery retried a launch that was still in flight"
    );
    drop(lock);
    f.wait("crew-resume-park-crew-28-1-1", &["succeeded"]);
    let crew = f.wait("crew-28", &["resumed"]);
    f.service("stop");
    assert_eq!(crew["reason"], "resumed as crew-resume-park-crew-28-1-1");
    assert_eq!(
        f.park_field("crew-28", "retries"),
        rusqlite::types::Value::Integer(0)
    );
    assert_eq!(f.launches().len(), 2, "{:?}", f.launches());
    let notices = f.ship_notices();
    assert!(
        !notices
            .iter()
            .any(|b| b.contains("crew-28") && b.to_lowercase().contains("fail")),
        "the first mate was told of a failure: {notices:?}"
    );
}

/// A resume that does not start even after its retry fails the crew, with a
/// reason the first mate can act on.
#[test]
fn a_resume_that_does_not_start_after_its_retry_fails_the_crew() {
    let f = Fleet::new();
    f.set_mode("slow");
    let home = f.root.join("engineering/.privateer");
    fs::write(
        home.join("config.json"),
        json!({"ship": "engineering", "resource_home": f.root.join("ledger")}).to_string(),
    )
    .unwrap();
    start_crew(&f, "crew-24");
    f.wait_park("crew-24");
    // Mark the park `resuming` while the child still runs. Once the child
    // settles, its settlement hook resumes only a `parked` park, so the resume
    // is not launched behind the test's back.
    f.set_park("crew-24", "resuming", 1, 1);
    f.wait("child-1", &["succeeded"]);
    f.service("start");
    let crew = f.wait("crew-24", &["failed"]);
    f.service("stop");
    assert!(
        crew["reason"]
            .as_str()
            .unwrap_or("")
            .contains("did not start, even after one retry"),
        "{crew}"
    );
}

/// A parked crew whose route the captain has since removed resumes as a fresh
/// crew on the first route still listed, not on the removed one.
#[test]
fn a_parked_crew_whose_route_was_removed_resumes_as_a_fresh_crew() {
    let f = Fleet::new();
    f.set_mode("slow");
    let codex = |model: &str| json!({"manifest": "bundled:codex", "options": {"model": model}, "cost": "credits"});
    f.routes(json!({
        "routes": {"claude-route": {"manifest": "bundled:claude", "cost": "credits"}, "codex-route": codex("m")},
        "crew": ["claude-route", "codex-route"]
    }));
    start_crew(&f, "crew-25");
    f.wait("crew-25", &["parked"]);
    f.routes(json!({"routes": {"codex-route": codex("m")}, "crew": ["codex-route"]}));
    f.wait("child-1", &["succeeded"]);
    let resume = f.wait("crew-resume-park-crew-25-1-1", &["succeeded", "failed"]);
    assert!(
        resume["command"].to_string().contains("codex"),
        "the resume did not take the route left: {resume}"
    );
    assert_eq!(f.launches().len(), 1, "{:?}", f.launches());
}

/// A resumed crew gets a fresh budget for its run, not what its predecessor
/// left: the first incarnation ran about five seconds and parked, and the
/// resumed run lasts its full eight-second budget before it parks again at
/// its deadline (the park cap, not the budget, bounds the total).
#[test]
fn a_resumed_crew_gets_a_fresh_budget_for_its_run() {
    let f = Fleet::new();
    f.set_mode("aged_hold");
    f.short_budget();
    start_crew(&f, "crew-27");
    f.wait("crew-27", &["parked"]);
    f.wait("child-1", &["succeeded"]);
    let resumed = Instant::now();
    f.wait("crew-resume-park-crew-27-1-1", &["parked", "timed_out"]);
    assert!(
        resumed.elapsed() >= std::time::Duration::from_secs(7),
        "the resumed crew ended before a fresh budget: {:?}",
        resumed.elapsed()
    );
}

/// A crew that parks and resumes six times, the cap, keeps every job id and
/// park id short and unique across its chain: a resume is named for the root
/// crew, not for the resume before it. The sixth resume launches and finishes.
#[test]
fn a_crew_that_parks_six_times_keeps_its_ids_short_and_unique() {
    let f = Fleet::new();
    f.set_mode("chain");
    start_crew(&f, "crew-chain");
    let until = Instant::now() + std::time::Duration::from_secs(120);
    while f.launches().len() < 7 {
        assert!(
            Instant::now() < until,
            "only {} launches: {:?}\nsteps: {}",
            f.launches().len(),
            f.launches(),
            f.steps()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let conn =
        rusqlite::Connection::open(f.root.join("engineering/.privateer/privateer.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, crew_job_id, resumed_job_id FROM crew_parks
             WHERE root='crew-chain' ORDER BY created_ms",
        )
        .unwrap();
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 6, "{rows:?}");
    let last = rows.last().unwrap().2.clone().expect("the sixth resume");
    assert_eq!(f.wait(&last, &["succeeded"])["status"], "succeeded");
    // Each resume is both one park's `resumed_job_id` and the next park's
    // crew, so the jobs are the root crew and every resume, each once.
    let parks: Vec<String> = rows.iter().map(|(park, _, _)| park.clone()).collect();
    let mut jobs = vec![rows[0].1.clone()];
    jobs.extend(rows.iter().filter_map(|(_, _, resumed)| resumed.clone()));
    assert_eq!(jobs.len(), 7, "{jobs:?}");
    for id in parks.iter().chain(&jobs) {
        assert!(id.len() <= 120, "job or park id over the limit: {id}");
    }
    let unique: std::collections::HashSet<&String> = parks.iter().chain(&jobs).collect();
    assert_eq!(
        unique.len(),
        parks.len() + jobs.len(),
        "ids repeat: {jobs:?} {parks:?}"
    );
    assert_eq!(rows[0].0, "park-crew-chain-1", "{rows:?}");
    assert_eq!(last, "crew-resume-park-crew-chain-6-1", "{rows:?}");
}
