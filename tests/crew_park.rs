//! Crew park and resume: a crew ends its turn on a heavy job it started, and
//! its outcome resumes the same native session (or a fresh crew with a
//! handoff). Real supervised jobs, the real CLI, and a fake `claude` that
//! records its launches.

mod support;

use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const SESSION: &str = "7d1b1f0e-1111-4222-8333-444455556666";

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
        // The fake claude. Its mode file picks the child's length and whether
        // the crew announces a session. A resume run carries --resume.
        let script = format!(
            r#"#!/bin/sh
ROOT='{root}'
BIN='{bin}'
printf 'LAUNCH %s\n' "$(printf '%s ' "$*" | tr '\n' ' ')" >> "$ROOT/launches.log"
case "$*" in
  *--resume*|*"resuming crew work"*|*"heavy job has settled"*)
    # A resumed run completes its turn, as the harness reports it.
    printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
    printf '{{"type":"result","subtype":"success","is_error":false,"session_id":"{session}"}}\n'
    exit 0 ;;
esac
if [ "$(cat "$ROOT/mode" 2>/dev/null)" != nosession ]; then
  printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
fi
if [ "$(cat "$ROOT/mode" 2>/dev/null)" = long ]; then
  child='sleep 60'
else
  child='sleep 2; echo built'
fi
"$BIN" job.start --json "{{\"id\":\"child-1\",\"task_id\":\"fix\",\"command\":[\"sh\",\"-c\",\"$child\"],\"heavy\":true}}" >>"$ROOT/steps.log" 2>&1 || exit 3
"$BIN" crew.park --json '{{"job_id":"child-1","next":"check the build output"}}' >>"$ROOT/steps.log" 2>&1 || exit 4
sleep 60
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
    let parked = f.wait("crew-1", &["parked"]);
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
    f.wait("crew-2", &["parked"]);
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
