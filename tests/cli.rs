mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");
const DEFAULT_ACTIONS: [&str; 10] = [
    "inspect",
    "spawn",
    "implement",
    "test",
    "publish_pr",
    "publish_review",
    "merge_green",
    "message",
    "recovery",
    "cleanup",
];

struct Folder {
    _temp: tempfile::TempDir,
    root: PathBuf,
    ship: PathBuf,
    path: String,
}

impl Folder {
    /// A fresh folder, with a fake `codex` as the only harness on PATH.
    fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let ship = root.join(name);
        fs::create_dir(&ship).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());
        Self {
            _temp: temp,
            root,
            ship,
            path,
        }
    }
    fn command_in(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = support::command(BIN);
        command
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME");
        command
    }
    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command_in(dir, args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run_in(&self.ship, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn json(&self, command: &str, args: Value) -> Value {
        serde_json::from_str(&self.ok(&[command, "--json", &args.to_string()])).unwrap()
    }
}

#[test]
fn ergonomic_agent_delegation_retains_identity_and_child_action_limits() {
    let folder = Folder::new("liaison");
    folder.ok(&["init", "--liaison", "--harness", "codex"]);
    // A captain-narrowed liaison: children inherit only what it holds.
    folder.json(
        "charter.put",
        json!({"id":"liaison","actions":["inspect","spawn","message"]}),
    );
    let home = folder.ship.join(".privateer");
    let home = home.to_str().unwrap();
    for (i, (role, actor, flags)) in [
        (Some("agent"), Some("liaison"), vec![]),
        (None, None, vec!["--role", "agent", "--actor", "liaison"]),
        (Some("agent"), Some("liaison"), vec!["--role", "operator"]),
        (Some("operator"), Some("liaison"), vec!["--role", "agent"]),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("worker-{i}");
        let mut args = vec!["--home", home, "ship", "new", &name];
        args.extend_from_slice(&flags);
        let mut command = folder.command_in(&folder.root, &args);
        if let Some(role) = role {
            command.env("PRIVATEER_ROLE", role);
        }
        if let Some(actor) = actor {
            command.env("PRIVATEER_ACTOR", actor);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let child = folder.root.join(&name).join(".privateer");
        let output = folder.run_in(
            &folder.root,
            &[
                "--home",
                child.to_str().unwrap(),
                "charter.show",
                "--json",
                &json!({"id":name}).to_string(),
            ],
        );
        assert!(output.status.success());
        let charter: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(charter["actions"], json!(["inspect", "spawn", "message"]));
        let fleet = folder.json("fleet.list", json!({}));
        let member = fleet["ships"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap();
        assert_eq!(member["created_by"], "liaison");
        let denied = folder
            .command_in(
                &folder.root,
                &[
                    "--home",
                    child.to_str().unwrap(),
                    "job.start",
                    "--json",
                    &json!({"task_id":format!("{name}-duty"),"command":["true"]}).to_string(),
                ],
            )
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", &name)
            .output()
            .unwrap();
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("implement"));
        let denied = folder
            .command_in(
                &folder.root,
                &[
                    "--home",
                    child.to_str().unwrap(),
                    "--role",
                    "operator",
                    "charter.put",
                    "--json",
                    &json!({"id":name,"actions":["implement"]}).to_string(),
                ],
            )
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", &name)
            .output()
            .unwrap();
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("operator command"));
        let retired = folder
            .command_in(
                &folder.root,
                &[
                    "--home", home, "ship", "retire", &name, "--role", "operator",
                ],
            )
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(
            retired.status.success(),
            "{}",
            String::from_utf8_lossy(&retired.stderr)
        );
        assert!(child.is_dir());
    }
    let denied = folder
        .command_in(&folder.ship, &["ship", "retire", "liaison"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert_eq!(
        folder.json("charter.show", json!({"id":"liaison"}))["active"],
        true
    );
    let sent = folder
        .command_in(&folder.ship, &["tell", "captain", "As myself"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let mail = folder.json("mail.inbox", json!({"address":"captain"}));
    assert_eq!(mail["messages"][0]["from"], "liaison");
    folder.json("mail.send", json!({"id":"captain-notice","from":"captain","to":"liaison","kind":"notice","body":"For your mailbox"}));
    let inbox = folder
        .command_in(&folder.ship, &["inbox"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(
        inbox.status.success(),
        "{}",
        String::from_utf8_lossy(&inbox.stderr)
    );
    assert!(String::from_utf8_lossy(&inbox.stdout).contains("For your mailbox"));
    folder.json("mail.send", json!({"id":"captain-question","from":"captain","to":"liaison","kind":"request","body":"Who can help?"}));
    for args in [
        vec!["reply", "captain-question", "A peer can help"],
        vec!["ask", "captain", "Which project?"],
        vec!["status"],
    ] {
        let output = folder
            .command_in(&folder.ship, &args)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let captain = folder.json("mail.inbox", json!({"address":"captain"}));
    assert!(
        captain["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["from"] == "liaison")
    );
    assert_eq!(captain["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn agent_operator_entry_is_rejected_before_fresh_state_or_launches() {
    let folder = Folder::new("desk");
    let fresh = folder.ship.join(".privateer");
    for herdr in [false, true] {
        let mut command = folder.command_in(&folder.ship, &[]);
        if herdr {
            command.env("HERDR_ENV", "1");
        }
        let output = command
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
    }
    for args in [
        vec!["--yes"],
        vec!["init", "--liaison", "--harness", "codex"],
        vec!["init", "--json", "{}"],
        vec!["start"],
        vec!["service", "start"],
        vec!["service", "stop"],
        vec!["service", "status"],
        vec!["serve"],
    ] {
        for herdr in [false, true] {
            for via_flags in [false, true] {
                let mut argv = vec!["--home", fresh.to_str().unwrap()];
                argv.extend_from_slice(&args);
                let mut command = folder.command_in(&folder.ship, &argv);
                if herdr {
                    command.env("HERDR_ENV", "1");
                }
                if via_flags {
                    command.args(["--role", "agent", "--actor", "liaison"]);
                } else {
                    command
                        .env("PRIVATEER_ROLE", "agent")
                        .env("PRIVATEER_ACTOR", "liaison")
                        .args(["--role", "operator"]);
                }
                let output = command.output().unwrap();
                assert!(!output.status.success(), "{args:?}");
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("operator command"),
                    "{args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
                assert!(!folder.root.join("fleet.sqlite").exists());
                assert!(!folder.root.join("config").exists());
            }
        }
    }
    // Agents may add projects, but only to an existing ship they may manage.
    let output = folder
        .command_in(
            &folder.ship,
            &[
                "--home",
                fresh.to_str().unwrap(),
                "project",
                "add",
                "unused",
            ],
        )
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("existing ship home"));
    assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
    assert!(!folder.root.join("fleet.sqlite").exists());
    for flags in [
        vec![
            "--role", "agent", "--actor", "liaison", "--role", "operator",
        ],
        vec!["--role", "agent", "--actor", "liaison", "--actor", "other"],
        vec!["--role", "agent", "--actor", "other"],
        vec!["--role", "invalid"],
    ] {
        let output = folder
            .command_in(&folder.ship, &["init", "--harness", "codex"])
            .args(flags)
            .env("PRIVATEER_ROLE", "agent")
            .env("PRIVATEER_ACTOR", "liaison")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
    }
    let output = folder
        .command_in(&folder.ship, &["init", "--role", "agent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("actor identity"));
    assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
    let output = folder
        .command_in(&folder.ship, &["--home", fresh.to_str().unwrap(), "rpc"])
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fresh.exists());
}

#[cfg(feature = "dashboard")]
#[test]
fn agent_dashboard_entry_does_not_create_fleet_or_home() {
    let folder = Folder::new("desk");
    let output = support::command(env!("CARGO_BIN_EXE_privateer-dashboard"))
        .args([
            "--once",
            "--home",
            folder.ship.join(".privateer").to_str().unwrap(),
        ])
        .current_dir(&folder.ship)
        .env("HOME", &folder.root)
        .env("XDG_CONFIG_HOME", folder.root.join("config"))
        .env("PRIVATEER_FLEET", folder.root.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", folder.root.join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "liaison")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("operator command"));
    assert_eq!(fs::read_dir(&folder.ship).unwrap().count(), 0);
    assert!(!folder.root.join("fleet.sqlite").exists());
}

#[test]
fn detached_agent_delegation_runs_operator_dispatchers_and_bounded_agent_jobs() {
    use std::time::{Duration, Instant};

    fn until(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(
                Instant::now() < end,
                "private dispatcher did not complete the expected action"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn active(pid: i64) -> bool {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&output.stdout);
        !status.trim().is_empty() && !status.trim().starts_with('Z')
    }

    let folder = Folder::new("liaison");
    let plugins = folder.root.join("plugins");
    fs::create_dir_all(plugins.join("codex")).unwrap();
    let mate = folder.root.join("mate.py");
    let adapter = plugins.join("codex/adapter.py");
    fs::write(&adapter, r#"import json, pathlib, sys
r = json.loads(sys.stdin.readline())
c = r['context']
argv = [sys.executable, str(pathlib.Path(__file__).parents[2] / 'mate.py'), json.dumps(c['control']['cli_argv']), c['owner'], c['brief']]
print(json.dumps({'id':r['id'], 'version':1, 'ok':True, 'result':{'argv':argv, 'cwd':c['repo'], 'env_names':[]}}))
"#).unwrap();
    fs::write(
        plugins.join("codex/manifest.json"),
        json!({"schema_version":1,"id":"codex",
        "executable":["python3",adapter],"timeout_ms":5000,"max_output_bytes":1_048_576})
        .to_string(),
    )
    .unwrap();
    fs::write(&mate, r#"import json, os, pathlib, re, subprocess, sys, uuid
cli, ship, prompt = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
keys = ['HOME','XDG_CONFIG_HOME','PRIVATEER_FLEET','PRIVATEER_RESOURCE_HOME','PRIVATEER_PLUGINS','PRIVATEER_ROLE','PRIVATEER_ACTOR','PRIVATEER_JOB','PRIVATEER_HOME','PRIVATEER_PRESENCE_HOLDER','PRIVATEER_WAKE','PRIVATEER_CONTROL_CLI','PRIVATEER_ATTENDED_HARNESS','PRIVATEER_CLAUDE_MAIL_UNTIL']
pathlib.Path(os.environ['HOME'], ship + '-identity.json').write_text(json.dumps({'pid':os.getpid(), 'env':{key:os.environ.get(key) for key in keys}}))
print(json.dumps({'type':'thread.started','thread_id':str(uuid.uuid4())}), flush=True)
def call(verb, args):
    return json.loads(subprocess.run(cli + [verb, '--json', json.dumps(args)], capture_output=True, text=True, check=True).stdout)
for message in call('mail.inbox', {'address':ship})['messages']:
    if message['kind'] == 'request' and message['state'] == 'open':
        call('mail.send', {'from':ship, 'to':message['from'], 'kind':'reply', 'correlation':message['id'], 'body':'scripted reply from ' + ship})
call('mail.ack', {'address':ship, 'through_seq':int(re.search(r'through seq (\d+)', prompt).group(1))})
print(json.dumps({'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}}))
"#).unwrap();
    let init = folder
        .command_in(&folder.ship, &["init", "--liaison", "--harness", "codex"])
        .env("PRIVATEER_PLUGINS", &plugins)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    for typed in [false, true] {
        let name = if typed {
            "typed-worker"
        } else {
            "ergonomic-worker"
        };
        let home = folder.root.join(name).join(".privateer");
        let parent_home = folder.ship.join(".privateer");
        let mut dispatcher_pid = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let args = if typed {
                vec![
                    "ship.create".to_owned(),
                    "--json".to_owned(),
                    json!({"name":name,"harness":"codex"}).to_string(),
                ]
            } else {
                vec![
                    "ship".to_owned(),
                    "new".to_owned(),
                    name.to_owned(),
                    "--harness".to_owned(),
                    "codex".to_owned(),
                ]
            };
            let made = folder
                .command_in(&folder.ship, &[])
                .args(args)
                .env("PRIVATEER_PLUGINS", &plugins)
                .env("PRIVATEER_SERVICE", "detached")
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", "liaison")
                .env("PRIVATEER_HOME", &parent_home)
                .env("PRIVATEER_JOB", "stale-job")
                .env("PRIVATEER_PRESENCE_HOLDER", "stale-holder")
                .env("PRIVATEER_WAKE", "stale-wake")
                .env("PRIVATEER_CONTROL_CLI", "stale-control")
                .env("PRIVATEER_ATTENDED_HARNESS", "claude")
                .env("PRIVATEER_CLAUDE_MAIL_UNTIL", "9999999999999")
                .output()
                .unwrap();
            assert!(
                made.status.success(),
                "{}",
                String::from_utf8_lossy(&made.stderr)
            );
            let call = |verb: &str, args: Value| -> Value {
                let output = folder.run_in(
                    &folder.root,
                    &[
                        "--home",
                        home.to_str().unwrap(),
                        verb,
                        "--json",
                        &args.to_string(),
                    ],
                );
                assert!(
                    output.status.success(),
                    "{verb}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                serde_json::from_slice(&output.stdout).unwrap()
            };
            until(|| privateer::service::running(&home).unwrap_or(false));
            let pid: i64 = fs::read_to_string(home.join("serve.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            dispatcher_pid = Some(pid);
            let identity = Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "command="])
                .output()
                .unwrap();
            let identity = String::from_utf8_lossy(&identity.stdout);
            assert!(
                identity.contains(BIN)
                    && identity.contains(&format!("--home {} serve", home.display())),
                "{identity}"
            );
            // The liaison's ordinary authority passes to the ship it creates.
            assert_eq!(
                call("charter.show", json!({"id":name}))["actions"],
                json!(DEFAULT_ACTIONS)
            );
            let mut profile = call("ship.show", json!({"name":name}))["first_mate"].clone();
            profile["batch_secs"] = json!(0);
            profile["max_delay_secs"] = json!(0);
            call("ship.put", json!({"name":name,"first_mate":profile}));
            call(
                "mail.send",
                json!({"id":"question","from":"captain","to":name,"kind":"request","body":"Please relay this"}),
            );
            until(|| call("mail.show", json!({"id":"question"}))["state"] == "answered");
            until(|| {
                call("wake.list", json!({"ship":name}))["wakes"]
                    .as_array()
                    .is_some_and(|w| w.len() == 1 && w[0]["status"] == "succeeded")
            });
            let reply = call("mail.inbox", json!({"address":"captain"}));
            assert_eq!(
                reply["messages"][0]["body"],
                format!("scripted reply from {name}")
            );
            let record: Value = serde_json::from_slice(
                &fs::read(folder.root.join(format!("{name}-identity.json"))).unwrap(),
            )
            .unwrap();
            let env = &record["env"];
            assert_eq!(env["PRIVATEER_ROLE"], "agent");
            assert_eq!(env["PRIVATEER_ACTOR"], name);
            for key in [
                "PRIVATEER_HOME",
                "PRIVATEER_PRESENCE_HOLDER",
                "PRIVATEER_WAKE",
                "PRIVATEER_CONTROL_CLI",
                "PRIVATEER_ATTENDED_HARNESS",
                "PRIVATEER_CLAUDE_MAIL_UNTIL",
            ] {
                assert!(env[key].is_null(), "{key}: {}", env[key]);
            }
            for (key, path) in [
                ("HOME", folder.root.clone()),
                ("XDG_CONFIG_HOME", folder.root.join("config")),
                ("PRIVATEER_FLEET", folder.root.join("fleet.sqlite")),
                ("PRIVATEER_RESOURCE_HOME", folder.root.join("ledger")),
                ("PRIVATEER_PLUGINS", plugins.clone()),
            ] {
                assert_eq!(env[key], json!(path), "{key}");
            }
            let jobs = call("job.list", json!({}))["jobs"].clone();
            assert_eq!(jobs.as_array().unwrap().len(), 1);
            assert_eq!(jobs[0]["status"], "succeeded");
            assert_eq!(jobs[0]["action"], "spawn");
            assert_eq!(jobs[0]["agent_actor"], name);
            assert_eq!(jobs[0]["id"], env["PRIVATEER_JOB"]);
            assert_ne!(env["PRIVATEER_JOB"], "stale-job");
            assert_eq!(jobs[0]["payload"]["pid"], record["pid"]);
            until(|| {
                !active(record["pid"].as_i64().unwrap())
                    && !active(jobs[0]["supervisor"]["pid"].as_i64().unwrap())
            });
            let denied = folder
                .command_in(
                    &folder.root,
                    &["--home", home.to_str().unwrap(), "service", "stop"],
                )
                .env("PRIVATEER_ROLE", "agent")
                .env("PRIVATEER_ACTOR", name)
                .output()
                .unwrap();
            assert!(!denied.status.success());
            assert!(privateer::service::running(&home).unwrap());
        }));
        if home.join("privateer.sqlite").is_file() {
            let jobs = folder.run_in(
                &folder.root,
                &["--home", home.to_str().unwrap(), "job.list", "--json", "{}"],
            );
            if let Ok(jobs) = serde_json::from_slice::<Value>(&jobs.stdout) {
                for job in jobs["jobs"].as_array().into_iter().flatten() {
                    folder.run_in(
                        &folder.root,
                        &[
                            "--home",
                            home.to_str().unwrap(),
                            "job.cancel",
                            "--json",
                            &json!({"id":job["id"]}).to_string(),
                        ],
                    );
                }
            }
            let stopped = folder.run_in(
                &folder.root,
                &["--home", home.to_str().unwrap(), "service", "stop"],
            );
            assert!(
                stopped.status.success(),
                "{}",
                String::from_utf8_lossy(&stopped.stderr)
            );
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
        assert!(!privateer::service::running(&home).unwrap());
        if let Some(pid) = dispatcher_pid {
            until(|| !active(pid));
        }
    }
}

/// Captain finding 22: init says which route the first mate will take from
/// the fleet's first_mate list, and writes none into the profile, since each
/// wake chooses again. With no list it says the harness default runs.
#[test]
fn init_names_the_first_mate_route_from_the_fleet_list() {
    let folder = Folder::new("Routed");
    fs::write(
        folder.root.join("fleet.routes.json"),
        json!({"routes": {"flash": {"manifest": "bundled:codex", "cost": "plan",
            "options": {"model": "flash-model"}}}, "first_mate": ["flash"]})
        .to_string(),
    )
    .unwrap();
    let out = folder.ok(&["init", "--harness", "codex"]);
    assert!(
        out.contains("first mate route: flash (codex/-/flash-model), the first available on the fleet's first_mate list"),
        "{out}"
    );
    let ship = folder.json("ship.show", json!({"name":"routed"}));
    assert_eq!(
        ship["first_mate"]["options"],
        json!({"sandbox":"workspace-write"}),
        "{ship}"
    );

    let plain = Folder::new("Plain");
    let out = plain.ok(&["init", "--harness", "codex"]);
    assert!(
        out.contains("first mate model: codex's default (no first_mate routes listed)"),
        "{out}"
    );
}

#[test]
fn init_turns_a_folder_into_a_ship_any_harness_can_crew() {
    let folder = Folder::new("Infra Ship");
    let out = folder.ok(&["init", "--scope", "Deploys and build servers"]);
    assert!(out.contains("is now the ship infra-ship"), "{out}");
    assert!(out.contains("first mate harness: codex"));
    assert!(folder.ship.join(".privateer/privateer.sqlite").is_file());
    assert!(folder.ship.join("projects").is_dir());
    let agents = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("<!-- privateer:generated -->"));
    assert!(agents.contains("first mate of the Privateer ship **infra-ship**"));
    assert!(agents.contains("Its scope: Deploys and build servers."));
    // A captain's own hands are marked, with the exact step, never buried in a report.
    assert!(agents.contains(r#""needs_hands":{"step":"#), "{agents}");
    assert!(agents.contains("Never bury such an ask under a report"));
    // Anything about a request goes in its thread, and a follow-up names it.
    assert!(agents.contains("Anything about a request goes in that request's thread"));
    assert!(
        agents.contains(r#""because_of":"<parent request id>""#),
        "{agents}"
    );
    // A captain's choice between options is a typed decision, not a question in the body.
    assert!(agents.contains(
        "send it as `kind` `decision` with `payload.decision`, not as a question in the body"
    ));
    // A long document goes to the captain as a report to read and annotate.
    assert!(agents.contains("goes as a report they read and annotate"));
    assert!(
        agents.contains(r#"report.put {"from":"infra-ship""#),
        "{agents}"
    );
    assert!(agents.contains(&format!("{BIN} --home ")));
    // Issue 101: a ship that may publish owns its pull requests to merge.
    assert!(agents.contains("you own your pull requests end to end"));
    assert!(agents.contains("leave the merge to the captain"));
    assert!(!agents.contains("You delegate and relay"));
    assert_eq!(
        fs::read_to_string(folder.ship.join("CLAUDE.md")).unwrap(),
        "@AGENTS.md\n"
    );
    let settings: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["permissions"]["allow"][0],
        format!("Bash({BIN}:*)")
    );
    let ignore = fs::read_to_string(folder.ship.join(".gitignore")).unwrap();
    assert!(ignore.contains(".privateer/") && ignore.contains("projects/"));

    let ship = folder.json("ship.show", json!({"name":"infra-ship"}));
    assert_eq!(ship["scope"], "Deploys and build servers");
    assert_eq!(ship["first_mate"]["manifest"], "bundled:codex");
    assert_eq!(ship["first_mate"]["options"]["sandbox"], "workspace-write");
    assert_eq!(ship["first_mate"]["duty_task"], "infra-ship-duty");
    let charter = folder.json("charter.show", json!({"id":"infra-ship"}));
    assert_eq!(charter["repo"], folder.ship.display().to_string());
    let task = folder.json("task.show", json!({"id":"infra-ship-duty"}));
    assert_eq!(task["owner"], "infra-ship");
}

#[test]
fn init_again_keeps_the_captains_edits() {
    let folder = Folder::new("desk");
    folder.ok(&["init", "--ship", "infra", "--scope", "Deploys"]);
    fs::write(folder.ship.join("AGENTS.md"), "# My own instructions\n").unwrap();
    folder.json(
        "charter.put",
        json!({"id":"infra","objective":"Edited by the captain"}),
    );
    let out = folder.ok(&["init", "--harness", "claude"]);
    assert!(out.contains("was left alone"), "{out}");
    assert_eq!(
        fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap(),
        "# My own instructions\n"
    );
    let ship = folder.json("ship.show", json!({"name":"infra"}));
    assert_eq!(ship["scope"], "Deploys");
    assert_eq!(ship["first_mate"]["manifest"], "bundled:claude");
    assert_eq!(
        ship["first_mate"]["options"]["permission_mode"],
        "acceptEdits"
    );
    let charter = folder.json("charter.show", json!({"id":"infra"}));
    assert_eq!(charter["objective"], "Edited by the captain");
    assert_eq!(
        folder.json("fleet.list", json!({}))["ships"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let settings: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["permissions"]["allow"].as_array().unwrap().len(),
        1
    );
}

/// `args` under `umask`, with the ledger and fleet at their defaults beneath
/// the folder's HOME.
fn under_umask(folder: &Folder, umask: libc::mode_t, args: &[&str]) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = folder.command_in(&folder.ship, args);
    command
        .env_remove("PRIVATEER_RESOURCE_HOME")
        .env_remove("PRIVATEER_FLEET");
    unsafe {
        command.pre_exec(move || {
            libc::umask(umask);
            Ok(())
        })
    };
    command
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn init_under_a_permissive_umask_makes_its_own_directories_private() {
    use std::io::Write;
    use std::process::Stdio;
    // Server 21: umask 002 made the ledger directory 0775, then init refused it.
    let folder = Folder::new("desk");
    let output = under_umask(&folder, 0o002, &["init"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = folder.root.join(".local/state");
    for dir in [
        folder.root.join(".local"),
        state.clone(),
        state.join("privateer"),
        state.join("privateer-host"),
        folder.ship.join(".privateer"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    // A credential beside the fleet list accepts the folders init made.
    let args = json!({"ref":"brave-search"}).to_string();
    let mut child = under_umask(&folder, 0o002, &["credential.set", "--json", &args])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"secret\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(mode(&state.join("privateer/fleet.credentials")), 0o700);
    // Run again, init keeps them private.
    let output = under_umask(&folder, 0o002, &["init"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(mode(&state.join("privateer-host")), 0o700);
}

#[test]
fn init_still_refuses_a_ledger_directory_others_can_write() {
    let folder = Folder::new("desk");
    let ledger = folder.root.join(".local/state/privateer-host");
    fs::create_dir_all(&ledger).unwrap();
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o777)).unwrap();
    let output = under_umask(&folder, 0o002, &["init"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resource_home may not be writable by other users"),
        "{stderr}"
    );
    // Refused, not quietly tightened.
    assert_eq!(mode(&ledger), 0o777);
}

#[test]
fn a_moved_folder_points_privateers_claude_settings_at_this_binary() {
    // A ship moved from another machine still names that machine's binary.
    let folder = Folder::new("liaison");
    let stale = "/home/old/.cargo/bin/privateer";
    let foreign = "/usr/bin/foreign-tool hook claude SessionStart";
    let seed = |value: Value| {
        fs::create_dir_all(folder.ship.join(".claude")).unwrap();
        fs::write(folder.ship.join(".claude/settings.json"), value.to_string()).unwrap();
    };
    seed(json!({
        "model": "sonnet",
        "permissions": {"allow": [format!("Bash({stale}:*)"), "Bash(git:*)"]},
        "hooks": {
            "SessionStart": [
                {"hooks": [{"type": "command", "command": format!("{stale} hook claude SessionStart")}]},
                {"hooks": [{"type": "command", "command": foreign}]}
            ],
            "Stop": [{"hooks": [
                {"type": "command", "command": format!("{stale} hook claude Stop")},
                {"type": "command", "command": "notify-send done"}
            ]}],
            "SessionEnd": [{"hooks": [
                {"type": "command", "command": format!("{stale} hook claude SessionEnd")},
                {"type": "command", "command": format!("{stale} hook claude SessionEnd")}
            ]}]
        }
    }));
    folder.ok(&["init", "--ship", "liaison", "--scope", "Liaison"]);

    let read = |folder: &Folder| -> Value {
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap()
    };
    let settings = read(&folder);
    assert_eq!(settings["model"], "sonnet");
    assert_eq!(
        settings["permissions"]["allow"],
        json!(["Bash(git:*)", format!("Bash({BIN}:*)")])
    );
    fn commands(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap().to_owned())
            .collect()
    }
    let ours = |event: &str| format!("{BIN} hook claude {event}");
    assert_eq!(
        commands(&settings, "SessionStart"),
        [foreign.to_owned(), ours("SessionStart")]
    );
    assert_eq!(
        commands(&settings, "Stop"),
        ["notify-send done".to_owned(), ours("Stop")]
    );
    assert_eq!(commands(&settings, "SessionEnd"), [ours("SessionEnd")]);
    assert!(
        !String::from_utf8_lossy(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .contains(stale)
    );

    // Running again changes nothing, byte for byte.
    let first = fs::read(folder.ship.join(".claude/settings.json")).unwrap();
    folder.ok(&["init"]);
    assert_eq!(
        fs::read(folder.ship.join(".claude/settings.json")).unwrap(),
        first
    );

    // A dispatcher start, or a session start, brings a moved folder back to this binary.
    seed(json!({"permissions": {"allow": [format!("Bash({stale}:*)")]}}));
    folder.ok(&["service", "start"]);
    let settings = read(&folder);
    assert_eq!(
        settings["permissions"]["allow"],
        json!([format!("Bash({BIN}:*)")])
    );
    seed(
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": format!("{stale} hook claude Stop")}]}]}}),
    );
    folder.ok(&["start"]);
    let settings = read(&folder);
    assert_eq!(commands(&settings, "Stop"), [ours("Stop")]);
}

#[test]
fn claude_settings_leave_foreign_entries_exactly_as_they_were() {
    let folder = Folder::new("desk");
    folder.ok(&["init", "--ship", "desk", "--scope", "Desk"]);
    // Entries Privateer did not write: no hooks list, a non-list hooks, a bare
    // string, and a hook of another tool. None may gain a key or change.
    let foreign = json!([
        {"matcher": "Bash"},
        {"hooks": "not a list"},
        "a bare string entry",
        {"hooks": [{"type": "command", "command": "notify-send done"}]}
    ]);
    // Hand-written rules naming a bare or relative `privateer` are not ours.
    let rules = json!([
        "Bash(privateer:*)",
        "Bash(./target/debug/privateer:*)",
        format!("Bash({BIN}:*)")
    ]);
    fs::write(
        folder.ship.join(".claude/settings.json"),
        json!({"permissions": {"allow": rules}, "hooks": {"Stop": foreign}}).to_string(),
    )
    .unwrap();

    folder.ok(&["init"]);
    let settings: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["permissions"]["allow"],
        json!([
            "Bash(privateer:*)",
            "Bash(./target/debug/privateer:*)",
            format!("Bash({BIN}:*)")
        ])
    );
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop[..4], *foreign.as_array().unwrap());
    assert_eq!(
        stop[4]["hooks"][0]["command"],
        format!("{BIN} hook claude Stop")
    );
    assert_eq!(stop.len(), 5, "{settings}");
}

#[test]
fn a_malformed_claude_settings_file_does_not_stop_a_start() {
    let folder = Folder::new("desk");
    folder.ok(&["init", "--ship", "desk", "--scope", "Desk"]);
    let path = folder.ship.join(".claude/settings.json");
    for broken in ["{ not json", r#"{"permissions":{"allow":"Bash(git:*)"}}"#] {
        fs::write(&path, broken).unwrap();
        for args in [&["start"][..], &["service", "start"][..]] {
            let out = folder.run_in(&folder.ship, args);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{args:?}: {stderr}");
            assert!(
                stderr.contains("could not update the ship's Claude settings"),
                "{stderr}"
            );
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }
}

#[test]
fn a_dispatcher_start_brings_generated_instructions_up_to_this_build() {
    let folder = Folder::new("desk");
    folder.ok(&["init", "--ship", "infra", "--scope", "Deploys"]);
    let agents = folder.ship.join("AGENTS.md");
    let current = fs::read_to_string(&agents).unwrap();
    // As an older build wrote it, before the template learned decisions.
    fs::write(
        &agents,
        "<!-- privateer:generated -->\n# Old instructions\n",
    )
    .unwrap();

    folder.ok(&["serve", "--json", r#"{"once":true}"#]);
    assert_eq!(fs::read_to_string(&agents).unwrap(), current);

    // A file the captain took over is never rewritten.
    fs::write(&agents, "# My own instructions\n").unwrap();
    folder.ok(&["serve", "--json", r#"{"once":true}"#]);
    assert_eq!(
        fs::read_to_string(&agents).unwrap(),
        "# My own instructions\n"
    );
}

#[test]
fn a_first_mate_with_claude_in_chrome_learns_to_let_navigations_land() {
    let folder = Folder::new("desk");
    folder.ok(&["init", "--ship", "infra", "--harness", "claude"]);
    let agents = folder.ship.join("AGENTS.md");
    let refusal = "Permission denied for this action on this domain";
    assert!(!fs::read_to_string(&agents).unwrap().contains(refusal));

    let mut ship = folder.json("ship.show", json!({"name":"infra"}));
    ship["first_mate"]["options"]["chrome"] = json!(true);
    folder.json(
        "ship.put",
        json!({"name":"infra","first_mate":ship["first_mate"]}),
    );
    folder.ok(&["serve", "--json", r#"{"once":true}"#]);
    assert!(fs::read_to_string(&agents).unwrap().contains(refusal));
}

#[test]
fn commands_find_the_ship_from_any_folder_inside_it() {
    let folder = Folder::new("infra");
    let outside = folder.run_in(&folder.root, &["status"]);
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("privateer init"));
    folder.ok(&["init"]);
    let nested = folder.ship.join("projects/app/src");
    fs::create_dir_all(&nested).unwrap();
    let status = folder.run_in(&nested, &["status"]);
    assert!(status.status.success());
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("Bearings for infra"), "{text}");
    // JSON commands find the home the same way.
    let json = folder.run_in(&nested, &["status", "--json", "{}"]);
    let status: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(status["open_tasks"], 1);
    let ships = folder.ok(&["ships"]);
    assert!(ships.contains("infra (here, first mate: codex)"), "{ships}");
}

#[test]
fn the_captain_asks_reads_and_replies_in_plain_commands() {
    let folder = Folder::new("infra");
    folder.ok(&["init"]);
    let sent = folder.ok(&["ask", "infra", "Is", "staging", "healthy?"]);
    assert!(sent.contains("Sent request"), "{sent}");
    let mail = folder.json("mail.inbox", json!({"address":"infra"}));
    assert_eq!(mail["messages"][0]["body"], "Is staging healthy?");
    assert_eq!(mail["messages"][0]["from"], "captain");
    let request = mail["messages"][0]["id"].as_str().unwrap().to_owned();
    // The first mate answers, and asks something of its own.
    folder.json(
        "mail.send",
        json!({"from":"infra","to":"captain","kind":"reply","correlation":request,"body":"All green."}),
    );
    folder.json(
        "mail.send",
        json!({"id":"may-i","from":"infra","to":"captain","kind":"request","body":"May I restart the runner?"}),
    );
    let inbox = folder.ok(&["inbox"]);
    assert!(
        inbox.contains("All green.") && inbox.contains("May I restart the runner?"),
        "{inbox}"
    );
    assert!(folder.ok(&["inbox"]).contains("No new mail"));
    assert!(folder.ok(&["inbox", "--all"]).contains("All green."));
    folder.ok(&["reply", "may-i", "Yes,", "go", "ahead."]);
    let answered = folder.json("mail.show", json!({"id":"may-i"}));
    assert_eq!(answered["state"], "answered");
    folder.ok(&["tell", "infra", "Deploy window moved to Friday"]);
    let notices = folder.json("mail.inbox", json!({"address":"infra"}));
    let kinds: Vec<_> = notices["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds, ["request", "reply", "notice"]);
    assert!(folder.ok(&["wakes"]).contains("No wakes yet"));
}

#[test]
fn project_add_clones_a_repository_into_the_ship() {
    let folder = Folder::new("infra");
    folder.ok(&["init"]);
    let source = folder.root.join("source");
    fs::create_dir(&source).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&source)
                .env("PATH", &folder.path)
                .output()
                .unwrap()
                .status
                .success()
        )
    };
    git(&["init", "-q"]);
    fs::write(source.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "init",
    ]);
    let out = folder.ok(&["project", "add", source.to_str().unwrap(), "app"]);
    assert!(out.contains("projects/app"), "{out}");
    assert_eq!(
        fs::read_to_string(folder.ship.join("projects/app/README.md")).unwrap(),
        "hello"
    );
    let again = folder.run_in(
        &folder.ship,
        &["project", "add", source.to_str().unwrap(), "app"],
    );
    assert!(!again.status.success());
}

#[test]
fn init_needs_a_harness_or_says_how_to_choose_one() {
    let folder = Folder::new("infra");
    fs::remove_file(folder.root.join("bin/codex")).unwrap();
    let output = folder.run_in(&folder.ship, &["init"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--harness"));
    let output = folder.run_in(&folder.ship, &["init", "--harness", "no-such-harness"]);
    assert!(!output.status.success());
}

#[test]
fn liaison_preset_acts_for_the_captain_and_keeps_captain_edits() {
    let folder = Folder::new("desk");
    folder.ok(&[
        "init",
        "--ship",
        "contact",
        "--liaison",
        "--harness",
        "codex",
        "--scope",
        "Captain contact",
    ]);
    let charter = folder.json("charter.show", json!({"id":"contact"}));
    // Everything a captain-made ship holds; production stays with the captain.
    assert_eq!(charter["actions"], json!(DEFAULT_ACTIONS));
    let emitted = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    // AGENTS.md is the generated public first-mate interface, not a proxy
    // for successful model onboarding; the PTY separately proves creation.
    assert!(emitted.contains("Onboarding the captain"));
    assert!(emitted.contains("Production deployment stays with the captain"));
    assert!(emitted.contains("project.add"));
    assert!(!emitted.contains("ask the captain to run"));
    // A stow incident: the liaison restated a request body as current fact.
    // It must say when each request was asked, off the list's own ages.
    assert!(emitted.contains(r#"Report an outstanding item as "X asked at T, N min ago""#));
    assert!(emitted.contains("`asked_ms`"));
    assert!(emitted.contains("`age_ms`"));
    // ... and read runtime state back before restating what a request claims.
    assert!(emitted.contains("never a current fact"));
    assert!(emitted.contains("`stow.status`"));
    // Hands-on asks are relayed first and apart, each with its step.
    assert!(emitted.contains("`hands_on`"));
    assert!(emitted.contains(r#""needs_hands":{"step":"#));
    assert!(emitted.contains("Relay the `hands_on` asks first"));
    folder.ok(&["init", "--harness", "codex"]);
    assert!(
        fs::read_to_string(folder.ship.join("AGENTS.md"))
            .unwrap()
            .contains("Onboarding the captain")
    );
    folder.json(
        "charter.put",
        json!({"id":"contact","actions":["inspect","message"]}),
    );
    let charter = folder.json("charter.show", json!({"id":"contact"}));
    let ship = folder.json("ship.show", json!({"name":"contact"}));
    let task = folder.json("task.show", json!({"id":"contact-duty"}));
    for _ in 0..2 {
        folder.ok(&["init"]);
        assert_eq!(
            folder.json("charter.show", json!({"id":"contact"})),
            charter
        );
        assert_eq!(folder.json("ship.show", json!({"name":"contact"})), ship);
        assert_eq!(folder.json("task.show", json!({"id":"contact-duty"})), task);
        let fleet = folder.json("fleet.list", json!({}));
        let members = fleet["ships"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["name"], "contact");
        assert_eq!(members[0]["home"], json!(folder.ship.join(".privateer")));
        let local = folder.json("ship.list", json!({}));
        assert_eq!(local["ships"].as_array().unwrap().len(), 1);
        assert_eq!(local["ships"][0]["name"], "contact");
        assert!(
            fs::read_to_string(folder.ship.join("AGENTS.md"))
                .unwrap()
                .contains("first mate of the Privateer ship **contact**")
        );
    }
    let config: Value =
        serde_json::from_slice(&fs::read(folder.ship.join(".privateer/config.json")).unwrap())
            .unwrap();
    assert_eq!(config["preset"], "liaison");
    assert_eq!(config["ship"], "contact");
}

#[test]
fn generated_first_mate_commands_work_from_a_folder_with_shell_metacharacters() {
    let folder = Folder::new("contact's $(touch escaped)");
    folder.ok(&[
        "init",
        "--ship",
        "contact",
        "--liaison",
        "--harness",
        "codex",
    ]);
    let text = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    // Issue 101: liaison relays; it does not publish or review other ships' code.
    assert!(
        text.contains(
            "You delegate and relay: you do not publish, review or merge other ships' code"
        )
    );
    let prefix = text
        .split_once("Run Privateer as `")
        .unwrap()
        .1
        .split_once(" COMMAND")
        .unwrap()
        .0;
    // Execute the generated public shell interface, rather than merely asserting
    // that its source contains quoting tokens.
    let output = support::command("sh")
        .arg("-c")
        .arg(format!(
            "{prefix} ship.bearings --json '{{\"ship\":\"contact\"}}'"
        ))
        .current_dir(&folder.ship)
        .env("HOME", &folder.root)
        .env("XDG_CONFIG_HOME", folder.root.join("config"))
        .env("PRIVATEER_FLEET", folder.root.join("fleet.sqlite"))
        .env("PRIVATEER_RESOURCE_HOME", folder.root.join("ledger"))
        .env("PRIVATEER_SERVICE", "none")
        .env_remove("PRIVATEER_ROLE")
        .env_remove("PRIVATEER_ACTOR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["ship"],
        "contact"
    );
    assert!(!folder.ship.join("escaped").exists());
}

#[test]
fn runtime_needs_never_creates_or_migrates_a_home() {
    let folder = Folder::new("observation");
    let home = folder.root.join("absent-home");
    let args = [
        "--home",
        home.to_str().unwrap(),
        "runtime.needs",
        "--json",
        "{}",
    ];
    let output = folder.run_in(&folder.ship, &args);
    assert!(!output.status.success());
    assert!(!home.exists());
    fs::create_dir(&home).unwrap();
    let database = home.join("privateer.sqlite");
    let old = rusqlite::Connection::open(&database).unwrap();
    old.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO metadata VALUES('schema','1');").unwrap();
    drop(old);
    let before = fs::read(&database).unwrap();
    let output = folder.run_in(&folder.ship, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["available"], false);
    assert_eq!(result["unknown"], json!(["runtime_schema"]));
    assert!(result["origin"].is_null());
    assert_eq!(fs::read(&database).unwrap(), before);
    // An agent is refused before an existing or missing home is opened.
    let agent_home = folder.root.join("agent-absent");
    let output = folder.run_in(
        &folder.ship,
        &[
            "--role",
            "agent",
            "--actor",
            "apps",
            "--home",
            agent_home.to_str().unwrap(),
            "runtime.needs",
            "--json",
            "{}",
        ],
    );
    assert!(!output.status.success());
    assert!(!agent_home.exists());
    drop(privateer::store::Store::open(&home).unwrap());
    let mut store =
        privateer::store::Store::open_readonly(&home, privateer::store::Caller::Operator).unwrap();
    assert!(
        privateer::dispatch(&mut store, "runtime.needs", json!({})).unwrap()["available"]
            .as_bool()
            .unwrap()
    );
    assert!(
        store
            .connection()
            .execute("INSERT INTO metadata VALUES('unexpected','write')", [])
            .is_err()
    );
    drop(store);
    let locker = rusqlite::Connection::open(&database).unwrap();
    locker
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    // Every corrupt-ancestry phase uses this subprocess watchdog too: a
    // regression must be killed AND reaped, not hang the serial test suite.
    let observe_with_watchdog = || {
        let mut command = folder.command_in(&folder.ship, &args);
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        let started = std::time::Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("runtime observation exceeded coarse watchdog");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    };
    let output = observe_with_watchdog();
    if output.status.success() {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["available"], false, "locked store must not be quiet");
        assert!(!value["unknown"].as_array().unwrap().is_empty());
    }
    locker.execute_batch("ROLLBACK").unwrap();
    drop(locker);

    // Build a real typed approval before corrupting only this disposable DB.
    use privateer::{dispatch, store::Store};
    let mut store = Store::open(&home).unwrap();
    dispatch(
        &mut store,
        "ship.put",
        json!({"name":"apps","scope":"fixture"}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "charter.put",
        json!({"id":"project","objective":"Fixture","repo":folder.ship,"min_free_bytes":0}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"main","charter_id":"project","owner":"apps","title":"Fixture"}),
    )
    .unwrap();
    dispatch(&mut store, "mail.send", json!({"id":"release","from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Release?","payload":{"artifact":"abc","environment":"production"}})).unwrap();
    drop(store);
    let output = observe_with_watchdog();
    assert!(output.status.success());
    let baseline: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(baseline["available"], true);
    assert_eq!(baseline["items"].as_array().unwrap().len(), 1);

    for phase in ["self-cycle", "two-node-cycle", "long-short-queries"] {
        let conn = rusqlite::Connection::open(&database).unwrap();
        match phase {
            "self-cycle" => {
                conn.execute("UPDATE tasks SET parent='main' WHERE id='main'", [])
                    .unwrap();
            }
            "two-node-cycle" => {
                conn.execute_batch("INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth,handoff_to) VALUES('ancestor','project','apps','Fixture','open','main',1,NULL);
                UPDATE tasks SET parent='ancestor' WHERE id='main';").unwrap();
            }
            _ => {
                conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000)
                INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth,handoff_to) SELECT 'chain-'||x,'project','apps','Fixture','open',CASE WHEN x<5000 THEN 'chain-'||(x+1) END,1,NULL FROM n;
                UPDATE tasks SET parent='chain-1' WHERE id='main';").unwrap();
            }
        }
        drop(conn);
        let before = fs::read(&database).unwrap();
        let output = observe_with_watchdog();
        assert!(
            output.status.success(),
            "{phase}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["available"], false, "{phase}: {value}");
        assert!(value["items"].as_array().unwrap().is_empty());
        assert!(value["next_after"].is_null());
        assert_eq!(value["unknown"], json!(["runtime_observation"]));
        assert_eq!(
            fs::read(&database).unwrap(),
            before,
            "{phase}: read repaired/wrote source"
        );
        let store = Store::open_readonly(&home, privateer::store::Caller::Operator).unwrap();
        assert_eq!(store.task("main").unwrap()["owner"], "apps");
        assert_eq!(store.task("main").unwrap()["handoff_to"], Value::Null);
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT value FROM metadata WHERE key='runtime_origin'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            baseline["origin"].as_str().unwrap()
        );
        assert_eq!(
            store
                .connection()
                .query_row("SELECT state FROM mail WHERE id='release'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "open"
        );
    }
}
