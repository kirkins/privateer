//! Crew close their own loop: a first mate's checks, run by the runtime at
//! the crew's commit, reported in the crew's outcome. Real supervised jobs,
//! the real CLI, a real Git workspace, and a fake `claude` crew.

mod support;

use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const SESSION: &str = "7d1b1f0e-1111-4222-8333-444455556666";

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

impl Fleet {
    /// A ship whose charter repository is a Git checkout, one task, and a
    /// crew harness whose behaviour the test's `mode` picks.
    fn new(mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        for dir in ["bin", "engineering", "repo"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        let repo = root.join("repo");
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(repo.join("state"), "bad\n").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let fleet = Self { _tmp: tmp, root };
        fs::write(fleet.root.join("mode"), mode).unwrap();
        let treehouse = fleet.root.join("bin/treehouse");
        fs::write(&treehouse, include_str!("fixtures/treehouse.py")).unwrap();
        fs::set_permissions(&treehouse, fs::Permissions::from_mode(0o700)).unwrap();
        // The crew: `loop` fails its check, parks, fixes it, parks again and
        // finishes; `lazy` never verifies; `capped` fails until it is refused.
        let script = format!(
            r#"#!/bin/sh
ROOT='{root}'
BIN='{bin}'
printf 'LAUNCH %s\n' "$(printf '%s ' "$*" | tr '\n' ' ')" >> "$ROOT/launches.log"
P() {{ "$BIN" --home "$ROOT/engineering/.privateer" "$@"; }}
say() {{ printf '$ %s\n' "$*" >>"$ROOT/steps.log"; P "$@" >>"$ROOT/steps.log" 2>&1; }}
commit() {{
  printf '%s\n' "$1" > state
  git add state && git -c user.name=Crew -c user.email=crew@example.invalid commit -q -m "$1"
}}
finish() {{
  printf '{{"type":"result","subtype":"success","is_error":false,"session_id":"{session}"}}\n'
  exit 0
}}
printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
case "$(cat "$ROOT/mode")" in
  loop)
    case "$PRIVATEER_JOB" in
      crew-1)
        commit worse
        say verify.run --json '{{"task_id":"fix"}}'
        say crew.park --json '{{"job_id":"verify-crew-1-1","next":"fix the failing check"}}'
        sleep 60 ;;
      crew-resume-park-crew-1-1-1)
        commit good
        say verify.run --json '{{"task_id":"fix"}}'
        say crew.park --json '{{"job_id":"verify-crew-resume-park-crew-1-1-1-1","next":"report"}}'
        sleep 60 ;;
    esac
    finish ;;
  lazy)
    say verify.set --json '{{"task_id":"fix","none":"the crew decided"}}'
    echo good > state
    say verify.run --json '{{"task_id":"fix"}}'
    finish ;;
  capped)
    commit worse
    for i in 1 2 3; do
      say verify.run --json '{{"task_id":"fix"}}'
      until P job.status --json "{{\"id\":\"verify-crew-1-$i\"}}" | grep -Eq '"status": ?"failed"'; do sleep 0.1; done
    done
    say verify.run --json '{{"task_id":"fix"}}'
    finish ;;
esac
"#,
            root = fleet.root.display(),
            bin = BIN,
            session = SESSION,
        );
        let claude = fleet.root.join("bin/claude");
        fs::write(&claude, script).unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        let codex = fleet.root.join("bin/codex");
        fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(codex, fs::Permissions::from_mode(0o700)).unwrap();
        fleet.ok(
            None,
            &["init", "--ship", "engineering", "--harness", "codex"],
        );
        fleet.json(
            None,
            "charter.put",
            json!({"id": "engineering", "repo": repo, "job_timeout_secs": 120}),
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
        // The first mate says how the crew proves it is done.
        fleet.json(
            Some("engineering"),
            "verify.set",
            json!({"task_id": "fix", "checks": [
                {"name": "state", "command": ["sh", "-c", "grep -qx good state"], "heavy": false}
            ]}),
        );
        fleet
    }
    fn command(&self, agent: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(self.root.join("engineering"))
            .args(args)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("PATH", support::path_with(&self.root.join("bin")))
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
    fn ok(&self, agent: Option<&str>, args: &[&str]) -> String {
        let o = self.command(agent, args).output().unwrap();
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
    fn start_crew(&self) {
        self.json(
            Some("engineering"),
            "agent.start",
            json!({"task_id": "fix", "id": "crew-1", "manifest": "bundled:claude"}),
        );
    }
    fn steps(&self) -> String {
        fs::read_to_string(self.root.join("steps.log")).unwrap_or_default()
    }
    /// The first mate's outcome for a crew job, once it is written.
    fn outcome(&self, job: &str) -> Value {
        let until = Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let inbox = self.json(
                None,
                "mail.inbox",
                json!({"address": "engineering", "all": true, "limit": 1000}),
            );
            let found = inbox["messages"].as_array().unwrap().iter().find(|m| {
                m["from"] == "crew"
                    && m["payload"]["job_id"] == job
                    && m["payload"]["status"].is_string()
            });
            if let Some(found) = found {
                return found.clone();
            }
            assert!(
                Instant::now() < until,
                "no outcome for {job}\nsteps: {}\ninbox: {inbox}",
                self.steps()
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

/// The crew's check fails, its resume reads which one, it fixes the work and
/// its final outcome reports the check passed at the commit it ended on.
#[test]
fn a_crew_fixes_a_failing_check_and_its_outcome_is_verified() {
    let f = Fleet::new("loop");
    f.start_crew();
    let outcome = f.outcome("crew-resume-park-crew-1-2-1");
    assert_eq!(outcome["payload"]["status"], "succeeded", "{outcome}");
    assert_eq!(outcome["urgency"], "normal", "{outcome}");
    let verification = &outcome["payload"]["verification"];
    assert_eq!(verification["verified"], true, "{outcome}");
    assert_eq!(verification["checks"][0]["state"], "passed", "{outcome}");
    // The commit it verified is the crew's last one.
    let launches = fs::read_to_string(f.root.join("launches.log")).unwrap();
    let first_resume = launches.lines().nth(1).unwrap_or_default();
    assert!(
        first_resume.contains("state failed exit 1"),
        "the resume reads which check failed: {first_resume}"
    );
}

/// A crew that never runs its checks is reported unverified and urgent, and
/// it can neither drop its checks nor verify uncommitted work.
#[test]
fn an_unverified_crew_is_reported_urgently_and_cannot_waive_its_checks() {
    let f = Fleet::new("lazy");
    f.start_crew();
    let outcome = f.outcome("crew-1");
    assert_eq!(outcome["payload"]["status"], "succeeded", "{outcome}");
    assert_eq!(outcome["urgency"], "high", "{outcome}");
    let verification = &outcome["payload"]["verification"];
    assert_eq!(verification["verified"], false, "{outcome}");
    assert_eq!(verification["checks"][0]["state"], "not run", "{outcome}");
    let steps = f.steps();
    assert!(steps.contains("is the first mate's"), "{steps}");
    assert!(steps.contains("commit first"), "{steps}");
}

/// After three failures of a check the crew must ask instead, and nobody
/// but the run's own job can run the runtime's runner.
#[test]
fn a_check_that_keeps_failing_sends_the_crew_to_its_first_mate() {
    let f = Fleet::new("capped");
    f.start_crew();
    let outcome = f.outcome("crew-1");
    assert_eq!(outcome["urgency"], "high", "{outcome}");
    assert_eq!(
        outcome["payload"]["verification"]["checks"][0]["state"], "failed",
        "{outcome}"
    );
    let steps = f.steps();
    assert!(steps.contains("failed 3 times"), "{steps}");
    assert!(steps.contains("crew.report kind decision"), "{steps}");
    let by_hand = f
        .command(Some("engineering"), &["_verify-run", "verify-crew-1-1"])
        .output()
        .unwrap();
    assert!(!by_hand.status.success());
    assert!(
        String::from_utf8_lossy(&by_hand.stderr).contains("runs only as its own job's payload"),
        "{by_hand:?}"
    );
}

/// Checks named from the repository's manifest are read at its HEAD, so an
/// uncommitted (or a branch's) edit cannot change what a check runs.
#[test]
fn manifest_checks_are_read_at_the_repository_head() {
    let f = Fleet::new("lazy");
    let repo = f.root.join("repo");
    let manifest = |test: &str| {
        json!({"checks": {"test": {"command": ["sh", "-c", test]}}, "default": ["test"]})
            .to_string()
    };
    fs::write(repo.join(".privateer-verify.json"), manifest("make test")).unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "manifest",
        ],
    );
    fs::write(repo.join(".privateer-verify.json"), manifest("true")).unwrap();
    let set = f.json(
        Some("engineering"),
        "verify.set",
        json!({"task_id": "fix", "checks": "default"}),
    );
    assert_eq!(set["revision"], 2, "{set}");
    assert_eq!(
        set["checks"][0]["command"],
        json!(["sh", "-c", "make test"]),
        "{set}"
    );
}
