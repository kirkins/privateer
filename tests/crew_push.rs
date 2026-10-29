//! A crew's launch environment holds no push credential (crew-role.md, layer
//! D); a first mate's wake and errand keep theirs. Real supervised launches
//! through `agent.start`, `errand.start` and a wake, with one fake harness
//! that records its environment and probes git, ssh and gh. The git remote,
//! ssh and gh are fakes on PATH; the probes never reach a network.

mod support;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command, time::Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
/// Set in the test's environment, so every launch that inherits it shows
/// whether the runtime removed it.
const SSH_AGENT: &str = "/tmp/privateer-test-agent.sentinel";
const GH_TOKEN: &str = "gh_sentinel_from_environment";
const SESSION: &str = "7d1b1f0e-1111-4222-8333-444455556666";
const REMOTE: &str = "ssh://git.example.test:2222/owner/repo.git";

struct Fleet {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fleet {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        for dir in ["bin", "engineering", "probe", "env", "home/.config/gh"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let fleet = Self { _tmp: tmp, root };
        fleet.write_fakes();
        fleet.write_git_config();
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
        fleet.make_repo();
        fleet
    }

    /// The fake harness, installed as both `claude` (crews) and `codex`
    /// (first mates and errands). It records its environment, then probes:
    /// a git push over ssh, a credential fill, and `gh auth status`.
    fn write_fakes(&self) {
        let harness = format!(
            r#"#!/bin/sh
ROOT='{root}'
env | sort > "$ROOT/env/$PRIVATEER_JOB.env"
printf '{{"type":"system","subtype":"init","session_id":"{session}"}}\n'
printf '{{"type":"result","subtype":"success","is_error":false,"session_id":"{session}"}}\n'
P="$ROOT/probe/$PRIVATEER_JOB"
git -C "$ROOT/repo" push '{remote}' HEAD:refs/heads/crew-branch >"$P.push" 2>&1
printf 'protocol=https\nhost=git.example.test\n\n' | git credential fill >"$P.cred" 2>&1
gh auth status >"$P.gh" 2>&1
exit 0
"#,
            root = self.root.display(),
            remote = REMOTE,
            session = SESSION,
        );
        for name in ["claude", "codex"] {
            self.script(name, &harness);
        }
        // ssh records its argv and whether it was handed an agent, then fails
        // the way a host that refuses the connection would.
        self.script(
            "ssh",
            &format!(
                "#!/bin/sh\necho \"job=${{PRIVATEER_JOB-none}} SSH_AUTH_SOCK=${{SSH_AUTH_SOCK-unset}} $*\" >> '{}'\nexit 255\n",
                self.root.join("ssh.log").display()
            ),
        );
        // gh logs in from GH_CONFIG_DIR when set, else from HOME, and takes a
        // token from GH_TOKEN as gh does.
        self.script(
            "gh",
            r#"#!/bin/sh
if [ -n "$GH_TOKEN" ]; then echo "logged in with GH_TOKEN"; exit 0; fi
if [ -e "${GH_CONFIG_DIR:-$HOME/.config/gh}/hosts.yml" ]; then echo "logged in from config"; exit 0; fi
echo "not logged in"; exit 1
"#,
        );
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The test user's own state: a global and a URL-scoped credential helper
    /// that would answer a push, a gh login and an ssh key. A launch that
    /// keeps its credentials shows them in its probes.
    fn write_git_config(&self) {
        fs::write(
            self.root.join("home/.gitconfig"),
            "[credential]\n\thelper = \"!echo password=GLOBAL_LEAK; echo username=user; echo\"\n\
             [credential \"https://git.example.test\"]\n\thelper = \"!echo password=URL_LEAK; echo username=user; echo\"\n",
        )
        .unwrap();
        fs::write(
            self.root.join("home/.config/gh/hosts.yml"),
            "git.example.test:\n    oauth_token: gho_FAKE_FOR_TESTS\n",
        )
        .unwrap();
    }

    /// The engineering workspace's clone, with one commit to push.
    fn make_repo(&self) {
        let repo = self.root.join("repo");
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
                .env("HOME", self.root.join("home"))
                .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        fs::create_dir(&repo).unwrap();
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "base",
        ]);
    }

    /// The test's own environment carries a gh token, an SSH agent and one
    /// git config entry of the starter's. Every launch inherits them unless
    /// the runtime removes them.
    fn command(&self, agent: Option<&str>, args: &[&str]) -> Command {
        let mut c = support::command(BIN);
        c.current_dir(self.root.join("engineering"))
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", support::path_with(&self.root.join("bin")))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env(
                "PRIVATEER_PLUGINS",
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins"),
            )
            .env("GH_TOKEN", GH_TOKEN)
            .env("GITHUB_TOKEN", GH_TOKEN)
            .env("SSH_AUTH_SOCK", SSH_AGENT)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "user.name")
            .env("GIT_CONFIG_VALUE_0", "starter")
            // A gate run as a crew job inherits the supervisor's scrubbed git
            // environment (a fetch key, a helper); the first-mate launches this
            // fleet reasons about must start from a clean baseline, so what the
            // launches record is what the fixture itself chose.
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_ASKPASS")
            .env_remove("GIT_TERMINAL_PROMPT")
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
            let out = self.run(
                None,
                &["job.status", "--json", &json!({"id": id}).to_string()],
            );
            let job: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            if out.status.success() && accepted.contains(&job["status"].as_str().unwrap_or("")) {
                return job;
            }
            if Instant::now() >= until {
                let log = fs::read_dir(self.root.join("engineering/.privateer/jobs").join(id))
                    .ok()
                    .and_then(|mut d| d.next())
                    .map(|e| {
                        format!(
                            "{:?} out={:?}",
                            fs::read_dir(e.as_ref().unwrap().path())
                                .unwrap()
                                .map(|x| x.unwrap().file_name())
                                .collect::<Vec<_>>(),
                            fs::read_to_string(e.unwrap().path().join("stdout.log"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                panic!("job {id} never reached {accepted:?}: {job}\nstderr: {log}");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    /// The environment one launch recorded, as `NAME=value` pairs.
    fn env_of(&self, job: &str) -> HashMap<String, String> {
        let text = fs::read_to_string(self.root.join("env").join(format!("{job}.env")))
            .unwrap_or_else(|_| panic!("no environment recorded for {job}"));
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }
    fn probe(&self, job: &str, probe: &str) -> String {
        fs::read_to_string(self.root.join("probe").join(format!("{job}.{probe}")))
            .unwrap_or_else(|_| panic!("no {probe} probe for {job}"))
    }
    /// Every launch the fake harness recorded, other than `except`.
    fn other_launches(&self, except: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root.join("env"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter_map(|n| n.strip_suffix(".env").map(str::to_owned))
            .filter(|n| n != except)
            .collect();
        names.sort();
        names
    }
    fn ssh_lines(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("ssh.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// A crew's payload: no push credential in its environment, its git, ssh or
/// gh probes, and its own starter's config entries kept.
#[test]
fn a_crew_launch_carries_no_push_credential() {
    let f = Fleet::new();
    f.json(
        Some("engineering"),
        "agent.start",
        json!({"task_id":"fix","id":"crew-1","manifest":"bundled:claude"}),
    );
    f.wait("crew-1", &["succeeded"]);

    let env = f.env_of("crew-1");
    assert_eq!(env.get("PRIVATEER_JOB").map(String::as_str), Some("crew-1"));
    for name in ["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"] {
        assert!(!env.contains_key(name), "crew kept {name}");
    }
    // The starter's own config entry comes first; the empty helper follows it.
    assert_eq!(env["GIT_CONFIG_COUNT"], "2");
    assert_eq!(env["GIT_CONFIG_KEY_0"], "user.name");
    assert_eq!(env["GIT_CONFIG_KEY_1"], "credential.helper");
    assert_eq!(env["GIT_CONFIG_VALUE_1"], "");
    let gh_dir = PathBuf::from(&env["GH_CONFIG_DIR"]);
    assert!(gh_dir.ends_with("gh"), "{gh_dir:?}");
    assert!(
        gh_dir.to_string_lossy().contains("/jobs/crew-1/"),
        "{gh_dir:?}"
    );
    assert_eq!(
        fs::read_dir(&gh_dir).unwrap().count(),
        0,
        "gh config not empty"
    );
    assert!(env["GIT_SSH_COMMAND"].contains("IdentitiesOnly=yes"));
    assert!(env["GIT_SSH_COMMAND"].contains("-F /dev/null"));

    // git: the push is refused (no credential), and no helper answers.
    let cred = f.probe("crew-1", "cred");
    assert!(!cred.contains("password="), "a helper answered: {cred}");
    assert!(!cred.contains("LEAK"), "{cred}");
    assert!(
        f.probe("crew-1", "push")
            .contains("Could not read from remote repository"),
        "{}",
        f.probe("crew-1", "push")
    );

    // ssh: the push offers no key and no agent.
    let ssh = f.ssh_lines();
    let crew_ssh: Vec<&String> = ssh
        .iter()
        .filter(|l| l.starts_with("job=crew-1 "))
        .collect();
    assert_eq!(crew_ssh.len(), 1, "{ssh:?}");
    assert!(crew_ssh[0].contains("SSH_AUTH_SOCK=unset"), "{crew_ssh:?}");
    assert!(crew_ssh[0].contains("IdentitiesOnly=yes"), "{crew_ssh:?}");
    assert!(
        crew_ssh[0].contains("IdentityFile=/dev/null"),
        "{crew_ssh:?}"
    );
    assert!(crew_ssh[0].contains("IdentityAgent=none"), "{crew_ssh:?}");

    // gh: the empty config directory holds no login, and GH_TOKEN is gone.
    assert!(f.probe("crew-1", "gh").contains("not logged in"));
}

/// A first mate's errand keeps its normal environment, and so do its probes,
/// which show the helper and the token it inherits.
#[test]
fn a_first_mates_errand_keeps_its_push_credentials() {
    let f = Fleet::new();
    let reply = f.json(
        Some("engineering"),
        "errand.start",
        json!({"focus":"task:fix","brief":"check the build"}),
    );
    let errand = reply["errand"].as_str().unwrap().to_owned();
    f.wait(&errand, &["succeeded"]);

    let env = f.env_of(&errand);
    assert_eq!(env["GH_TOKEN"], GH_TOKEN);
    assert_eq!(env["SSH_AUTH_SOCK"], SSH_AGENT);
    assert_eq!(env["GIT_CONFIG_COUNT"], "1");
    assert!(!env.contains_key("GIT_SSH_COMMAND"));
    assert!(!env.contains_key("GH_CONFIG_DIR"));

    assert!(
        f.probe(&errand, "cred").contains("GLOBAL_LEAK"),
        "{}",
        f.probe(&errand, "cred")
    );
    assert!(f.probe(&errand, "gh").contains("logged in"));
    let ssh = f.ssh_lines();
    assert!(
        ssh.iter().any(|l| l.starts_with(&format!("job={errand} "))
            && l.contains(&format!("SSH_AUTH_SOCK={SSH_AGENT}"))),
        "{ssh:?}"
    );
}

/// A first mate's headless wake keeps its push credentials, like an errand.
#[test]
fn a_first_mates_wake_keeps_its_push_credentials() {
    let f = Fleet::new();
    // A wake waits out the batch window; this test wants it at once.
    let shown = f.json(None, "ship.show", json!({"name":"engineering"}));
    let mut profile = shown["first_mate"].clone();
    for key in ["batch_secs", "max_delay_secs", "retry_backoff_secs"] {
        profile[key] = json!(0);
    }
    f.json(
        None,
        "ship.put",
        json!({"name":"engineering","scope":shown["scope"],"first_mate":profile}),
    );
    f.json(
        None,
        "mail.send",
        json!({"id":"ping-1","from":"captain","to":"engineering","kind":"request","body":"status?"}),
    );
    f.ok(
        None,
        &["serve", "--json", &json!({"once": true}).to_string()],
    );
    let until = Instant::now() + std::time::Duration::from_secs(60);
    let wake = loop {
        if let Some(job) = f.other_launches("").into_iter().next() {
            break job;
        }
        assert!(Instant::now() < until, "the wake never launched");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    f.wait(&wake, &["succeeded"]);

    let env = f.env_of(&wake);
    assert_eq!(env["GH_TOKEN"], GH_TOKEN);
    assert_eq!(env["SSH_AUTH_SOCK"], SSH_AGENT);
    assert_eq!(env["GIT_CONFIG_COUNT"], "1");
    assert!(!env.contains_key("GIT_SSH_COMMAND"));
    assert!(f.probe(&wake, "cred").contains("GLOBAL_LEAK"));
}
