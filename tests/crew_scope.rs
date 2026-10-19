//! The crew role at the runtime boundary: a crew launched through
//! `agent.start` is found through its supervisor and held to crew commands,
//! whatever its environment says. Real supervised jobs, the real CLI, and a
//! fake `claude` that runs a script as the crew and logs each result.

mod support;

use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const REFUSED: &str = "is the first mate's, not crew's. Report instead: crew.report";

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
        // The fake claude runs crew.sh, logging each call as `NAME EXIT
        // stderr`, then ends its turn.
        let script = format!(
            r#"#!/bin/sh
ROOT='{root}'
export BIN='{bin}' H='{root}/engineering/.privateer' LOG='{root}/calls.log'
call() {{ name=$1; shift; err=$("$@" 2>&1 >/dev/null); printf '%s %s %s\n' "$name" "$?" "$(printf '%s' "$err" | tr '\n' ' ')" >> "$LOG"; }}
. "$ROOT/crew.sh"
printf '{{"type":"result","subtype":"success","is_error":false}}\n'
"#,
            root = fleet.root.display(),
            bin = BIN,
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
            .env_remove("HERDR_SOCKET_PATH");
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
    fn wait(&self, id: &str, accepted: &[&str]) -> Value {
        let until = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let out = self
                .command(
                    None,
                    &["job.status", "--json", &json!({"id": id}).to_string()],
                )
                .output()
                .unwrap();
            let job: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            if out.status.success() && accepted.contains(&job["status"].as_str().unwrap_or("")) {
                return job;
            }
            assert!(
                Instant::now() < until,
                "job {id} never reached {accepted:?}: {job}\ncalls: {:?}",
                self.calls()
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    /// Launch a crew whose payload runs `script` (sh, with `call NAME CMD...`).
    fn crew(&self, id: &str, script: &str) -> Value {
        fs::write(self.root.join("crew.sh"), script).unwrap();
        self.json(
            Some("engineering"),
            "agent.start",
            json!({"task_id":"fix","id":id,"manifest":"bundled:claude"}),
        );
        self.wait(id, &["succeeded", "failed"])
    }
    /// Each logged call: (name, exit code, stderr).
    fn calls(&self) -> Vec<(String, String, String)> {
        fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let mut parts = l.splitn(3, ' ');
                (
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").to_owned(),
                )
            })
            .collect()
    }
    fn call(&self, name: &str) -> (String, String) {
        self.calls()
            .into_iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, code, err)| (code, err))
            .unwrap_or_else(|| panic!("no call {name}: {:?}", self.calls()))
    }
    fn refusal_notices(&self) -> Vec<Value> {
        let inbox = self.json(
            None,
            "mail.inbox",
            json!({"address": "engineering", "all": true, "limit": 1000}),
        );
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["from"] == "crew" && m["id"].as_str().unwrap().ends_with("-refused"))
            .cloned()
            .collect()
    }
    fn audits(&self, kind: &str) -> usize {
        let conn =
            rusqlite::Connection::open(self.root.join("engineering/.privateer/privateer.sqlite"))
                .unwrap();
        conn.query_row("SELECT COUNT(*) FROM audit WHERE kind=?1", [kind], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap() as usize
    }
}

#[test]
fn a_crew_is_refused_first_mate_commands_and_told_to_report() {
    let f = Fleet::new();
    // The first mate's own gate job on the same task, started outside any job.
    f.json(
        Some("engineering"),
        "job.start",
        json!({"id":"gate","task_id":"fix","command":["sleep","30"],"heavy":false}),
    );
    f.crew(
        "crew-1",
        r#"
call mail.ack "$BIN" --home "$H" mail.ack --json '{"address":"engineering","through_seq":0}'
call task.update "$BIN" --home "$H" task.update --json '{"actor":"engineering","id":"fix","status":"done"}'
call mail.send "$BIN" --home "$H" mail.send --json '{"from":"engineering","to":"engineering","kind":"notice","body":"hi"}'
call errand.start "$BIN" --home "$H" errand.start --json '{"focus":"task:fix","brief":"go"}'
call agent.start "$BIN" --home "$H" agent.start --json '{"task_id":"fix","id":"crew-2","manifest":"bundled:claude"}'
call job.retry "$BIN" --home "$H" job.retry --json '{"id":"crew-1"}'
call job.cancel "$BIN" --home "$H" job.cancel --json '{"id":"gate"}'
call crew.report "$BIN" --home "$H" crew.report --json '{"kind":"progress","body":"working"}'
call note.get "$BIN" --home "$H" note.get --json '{"ship":"engineering","focus":"task:fix"}'
call note.other "$BIN" --home "$H" note.get --json '{"ship":"engineering","focus":"ship"}'
call task.show "$BIN" --home "$H" task.show --json '{"id":"fix"}'
call job.start "$BIN" --home "$H" job.start --json '{"id":"inner","task_id":"fix","command":["true"],"heavy":false}'
call job.status "$BIN" --home "$H" job.status --json '{"id":"inner"}'
call workspace.status "$BIN" --home "$H" workspace.status --json '{"task_id":"fix"}'
"#,
    );
    for name in [
        "mail.ack",
        "task.update",
        "mail.send",
        "errand.start",
        "agent.start",
    ] {
        let (code, err) = f.call(name);
        assert_eq!(code, "1", "{name}: {err}");
        assert!(err.contains(&format!("{name} {REFUSED}")), "{name}: {err}");
    }
    let (code, err) = f.call("job.retry");
    assert!(code == "1" && err.contains("never a launch"), "{err}");
    let (code, err) = f.call("job.cancel");
    assert!(
        code == "1" && err.contains("only the jobs it started"),
        "{err}"
    );
    let (code, err) = f.call("note.other");
    assert!(code == "1" && err.contains("reads only task:fix"), "{err}");
    for name in [
        "crew.report",
        "note.get",
        "task.show",
        "job.start",
        "job.status",
        "workspace.status",
    ] {
        let (code, err) = f.call(name);
        assert_eq!(code, "0", "{name}: {err}");
    }
    // Nothing was written: the task is open, the gate still runs, no second crew.
    assert_eq!(
        f.json(None, "task.show", json!({"id":"fix"}))["status"],
        "open"
    );
    assert_eq!(
        f.json(None, "job.status", json!({"id":"gate"}))["status"],
        "running"
    );
    assert!(
        !f.command(None, &["job.status", "--json", r#"{"id":"crew-2"}"#])
            .output()
            .unwrap()
            .status
            .success()
    );
    // Every refusal is audited; the first mate hears once.
    assert_eq!(f.audits("crew.refused"), 8);
    let notices = f.refusal_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["urgency"], "low");
    assert_eq!(notices[0]["task_id"], "fix");
    f.json(None, "job.cancel", json!({"id":"gate"}));
}

#[test]
fn a_crew_that_drops_its_role_or_job_is_still_crew() {
    let f = Fleet::new();
    f.crew(
        "crew-1",
        r#"
call no-role env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR "$BIN" --home "$H" task.update --json '{"actor":"engineering","id":"fix","status":"done"}'
call no-role-no-job env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR -u PRIVATEER_JOB "$BIN" --home "$H" mail.ack --json '{"address":"engineering","through_seq":0}'
call new-home env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR HOME=/tmp PRIVATEER_RESOURCE_HOME=/tmp/x "$BIN" --home "$H" mail.ack --json '{"address":"engineering","through_seq":0}'
call routes env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR "$BIN" --home "$H" routes show
call no-job env -u PRIVATEER_JOB "$BIN" --home "$H" mail.ack --json '{"address":"engineering","through_seq":0}'
call borrowed env PRIVATEER_JOB=gate "$BIN" --home "$H" crew.report --json '{"kind":"progress","body":"x"}'
call heavy "$BIN" --home "$H" job.start --json '{"id":"inner","task_id":"fix","heavy":false,"command":["sh","-c","env -u PRIVATEER_JOB -u PRIVATEER_ROLE -u PRIVATEER_ACTOR \"$BIN\" --home \"$H\" task.update --json \"{\\\"actor\\\":\\\"engineering\\\",\\\"id\\\":\\\"fix\\\",\\\"status\\\":\\\"done\\\"}\""]}'
"#,
    );
    for name in ["no-role", "no-role-no-job", "new-home", "routes"] {
        let (code, err) = f.call(name);
        assert!(
            code == "1"
                && err.contains("job crew-1 runs as agent engineering; a job cannot drop its role"),
            "{name}: {err}"
        );
    }
    let (code, err) = f.call("no-job");
    assert!(
        code == "1" && err.contains(&format!("mail.ack {REFUSED}")),
        "{err}"
    );
    let (code, err) = f.call("borrowed");
    assert!(
        code == "1" && err.contains("PRIVATEER_JOB cannot name job gate"),
        "{err}"
    );
    // The crew's own job, even with its job variable unset, is crew too.
    let (code, err) = f.call("heavy");
    assert_eq!(code, "0", "{err}");
    let inner = f.wait("inner", &["succeeded", "failed"]);
    assert_eq!(inner["started_by"], "crew-1", "{inner}");
    assert_eq!(inner["exit_code"], 1, "{inner}");
    assert_eq!(
        f.json(None, "task.show", json!({"id":"fix"}))["status"],
        "open"
    );
    assert!(f.audits("caller.role_drop_refused") >= 6);
    assert_eq!(f.refusal_notices().len(), 1);
    // The same commands from outside any job run as the captain.
    f.json(None, "mail.inbox", json!({"address":"engineering"}));
    f.ok(None, &["routes", "show"]);
}

#[test]
fn outside_a_job_a_variable_naming_a_running_crew_narrows_to_it() {
    let f = Fleet::new();
    fs::write(f.root.join("crew.sh"), "sleep 20\n").unwrap();
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-1","manifest":"bundled:claude"}),
    );
    f.wait("crew-1", &["running"]);
    let narrowed = |agent: Option<&str>| {
        let out = f
            .command(
                agent,
                &[
                    "mail.ack",
                    "--json",
                    r#"{"address":"engineering","through_seq":0}"#,
                ],
            )
            .env("PRIVATEER_JOB", "crew-1")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    for agent in [None, Some("engineering")] {
        let (ok, err) = narrowed(agent);
        assert!(
            !ok && err.contains(&format!("mail.ack {REFUSED}")),
            "{agent:?}: {err}"
        );
    }
    f.json(None, "job.cancel", json!({"id":"crew-1"}));
}

#[test]
fn a_refused_crew_call_on_a_scratch_home_is_recorded_nowhere() {
    // The scratch home is outside the job's world, but the default registry
    // (this account's own) reaches it. The refusal stands and writes nothing.
    let f = Fleet::new();
    fs::create_dir(f.root.join("scratch")).unwrap();
    f.crew(
        "crew-1",
        r#"
REAL=$(getent passwd "$(id -u)" | cut -d: -f6)
call leak env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR -u PRIVATEER_JOB -u PRIVATEER_FLEET -u XDG_STATE_HOME HOME="$REAL" "$BIN" --home "$ROOT/scratch" mail.ack --json '{"address":"engineering","through_seq":0}'
"#,
    );
    let (code, err) = f.call("leak");
    assert!(
        code == "1"
            && err.contains("job crew-1 runs as agent engineering; a job cannot drop its role"),
        "{err}"
    );
    assert_eq!(f.audits("caller.role_drop_refused"), 0);
    assert!(f.refusal_notices().is_empty());
}

#[test]
fn a_crew_may_park_and_read_its_own_assignment() {
    let f = Fleet::new();
    f.crew(
        "crew-1",
        r#"
call crew.park "$BIN" --home "$H" crew.park --json '{}'
call assignment "$BIN" --home "$H" note.get --json '{"ship":"engineering","focus":"assignment:fix"}'
"#,
    );
    let (code, err) = f.call("crew.park");
    assert!(code == "1" && !err.contains(REFUSED), "{err}");
    let (code, err) = f.call("assignment");
    assert_eq!(code, "0", "{err}");
}

#[test]
fn an_errand_keeps_the_first_mates_powers() {
    let f = Fleet::new();
    fs::write(
        f.root.join("crew.sh"),
        r#"
call mail.send "$BIN" --home "$H" mail.send --json '{"from":"engineering","to":"captain","kind":"notice","body":"hi"}'
call task.update "$BIN" --home "$H" task.update --json '{"actor":"engineering","id":"fix","status":"running"}'
"#,
    )
    .unwrap();
    // The ship's first mate runs its errands on the same fake harness.
    fs::copy(f.root.join("bin/claude"), f.root.join("bin/codex")).unwrap();
    f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"go"}),
    );
    let until = Instant::now() + std::time::Duration::from_secs(60);
    while f.calls().len() < 2 && Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for name in ["mail.send", "task.update"] {
        let (code, err) = f.call(name);
        assert_eq!(code, "0", "{name}: {err}");
    }
}
