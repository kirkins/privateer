mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

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
        vec!["project", "add", "unused"],
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
            assert_eq!(
                call("charter.show", json!({"id":name}))["actions"],
                json!(["inspect", "spawn", "message"])
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
            let jobs = call("job.list", json!({}));
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
                for job in jobs.as_array().into_iter().flatten() {
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
    assert!(agents.contains(&format!("{BIN} --home ")));
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
    assert!(
        ship["first_mate"]["manifest"]
            .as_str()
            .unwrap()
            .ends_with("plugins/codex/manifest.json")
    );
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
    assert!(
        ship["first_mate"]["manifest"]
            .as_str()
            .unwrap()
            .ends_with("plugins/claude/manifest.json")
    );
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
fn liaison_preset_is_read_message_delegate_and_keeps_captain_edits() {
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
    assert_eq!(charter["actions"], json!(["inspect", "spawn", "message"]));
    let emitted = fs::read_to_string(folder.ship.join("AGENTS.md")).unwrap();
    // AGENTS.md is the generated public first-mate interface, not a proxy
    // for successful model onboarding; the PTY separately proves creation.
    assert!(emitted.contains("Onboarding the captain"));
    assert!(emitted.contains("never widen your own charter"));
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
