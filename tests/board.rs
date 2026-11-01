mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

// Concurrent forks can briefly inherit a fixture copy's writable descriptor
// until exec closes it. Match the executable-replacement fixtures: retry only
// ETXTBSY, before any product process has started, for at most two seconds.
fn fixture_output(command: &mut std::process::Command) -> Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match command.output() {
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result.unwrap(),
        }
    }
}

struct Place {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
    binary: PathBuf,
    path: String,
    probe: PathBuf,
}

impl Place {
    /// An empty folder and a fake codex, the only harness on PATH, that
    /// records where and as whom it ran.
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let folder = root.join("infra");
        fs::create_dir(&folder).unwrap();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        // The legacy entry tests must have no installed sibling/PATH dashboard.
        let binary = bin.join("privateer");
        fs::copy(BIN, &binary).unwrap();
        let probe = root.join("probe.txt");
        fs::write(
            bin.join("codex"),
            format!(
                "#!/bin/sh\necho \"$(pwd) $PRIVATEER_ROLE $PRIVATEER_ACTOR\" >> '{}'\n",
                probe.display()
            ),
        )
        .unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            path: format!("{}:/usr/bin:/bin", bin.display()),
            root,
            folder,
            binary,
            probe,
        }
    }
    fn run(&self, dir: &PathBuf, args: &[&str], service: &str) -> Output {
        let mut command = support::command(&self.binary);
        command
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_SERVICE", service)
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        fixture_output(&mut command)
    }
    fn probes(&self) -> Vec<String> {
        fs::read_to_string(&self.probe)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn bare_privateer_without_a_terminal_explains_and_changes_nothing() {
    let place = Place::new();
    let output = place.run(&place.folder, &[], "none");
    assert!(output.status.success());
    assert!(
        text(&output).contains("privateer --yes"),
        "{}",
        text(&output)
    );
    assert!(!place.folder.join(".privateer").exists());
    assert!(place.probes().is_empty());
}

#[test]
fn bare_privateer_makes_a_ship_and_boards_it_then_goes_straight_aboard() {
    let place = Place::new();
    let first = place.run(&place.folder, &["--yes"], "none");
    assert!(first.status.success(), "{}", text(&first));
    let said = text(&first);
    assert!(
        said.contains("is now the ship infra, crewed by codex"),
        "{said}"
    );
    assert!(said.contains("Dispatcher: not started"));
    assert!(place.folder.join(".privateer/privateer.sqlite").is_file());
    assert!(place.folder.join("AGENTS.md").is_file());
    // From anywhere inside the ship, bare privateer boards without asking.
    let nested = place.folder.join("projects/app");
    fs::create_dir_all(&nested).unwrap();
    let again = place.run(&nested, &[], "none");
    assert!(again.status.success(), "{}", text(&again));
    assert!(!text(&again).contains("is now the ship"));
    let ran = format!("{} agent infra", place.folder.display());
    assert_eq!(place.probes(), [ran.clone(), ran]);
}

#[test]
fn bare_privateer_refuses_to_make_the_home_directory_a_ship() {
    let place = Place::new();
    let output = place.run(&place.root, &["--yes"], "none");
    assert!(!output.status.success());
    assert!(
        text(&output).contains("dedicated folder"),
        "{}",
        text(&output)
    );
    assert!(!place.root.join(".privateer").exists());
}

#[test]
fn a_detached_dispatcher_starts_once_and_stops() {
    let place = Place::new();
    assert!(place.run(&place.folder, &["init"], "none").status.success());
    let started = place.run(&place.folder, &["service", "start"], "detached");
    assert!(started.status.success(), "{}", text(&started));
    assert!(text(&started).contains("started in the background"));
    let mut running = false;
    for _ in 0..50 {
        let status = text(&place.run(&place.folder, &["service", "status"], "detached"));
        if status.contains(": running") {
            running = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(running, "dispatcher never took its lock");
    let home = place.folder.join(".privateer");
    let record = home.join("serve.python.json");
    let python: PathBuf = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    assert!(python.is_absolute() && python.is_file());
    assert_eq!(
        fs::metadata(&record).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let status = text(&place.run(&place.folder, &["service", "status"], "detached"));
    assert!(
        status.contains(&format!("Python: {:?}", python.to_str().unwrap())),
        "{status}"
    );
    #[cfg(target_os = "linux")]
    {
        let pid = fs::read_to_string(home.join("serve.pid")).unwrap();
        let environment = fs::read(format!("/proc/{}/environ", pid.trim())).unwrap();
        let expected = format!("PRIVATEER_PYTHON={}", python.display());
        assert!(
            environment
                .split(|b| *b == 0)
                .any(|entry| entry == expected.as_bytes())
        );
    }
    let boarded = place.run(&place.folder, &[], "detached");
    assert!(
        text(&boarded).contains("Dispatcher: already running"),
        "{}",
        text(&boarded)
    );
    let stopped = place.run(&place.folder, &["service", "stop"], "detached");
    assert!(
        text(&stopped).contains("Stopped the dispatcher"),
        "{}",
        text(&stopped)
    );
    assert!(
        text(&place.run(&place.folder, &["service", "status"], "detached")).contains("not running")
    );
}

#[test]
fn a_systemd_user_service_records_path_and_is_enabled_and_removed() {
    let place = Place::new();
    assert!(place.run(&place.folder, &["init"], "none").status.success());
    let calls = place.root.join("systemctl.log");
    let shims = place.root.join("alpha/shims");
    fs::create_dir_all(&shims).unwrap();
    fs::write(shims.join("python3"), "#!/bin/sh\nexit 88\n").unwrap();
    fs::set_permissions(shims.join("python3"), fs::Permissions::from_mode(0o700)).unwrap();
    let install_path = format!("{}:{}", shims.display(), place.path);
    let fake = place.root.join("bin/fake-systemctl");
    fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        support::command(&place.binary)
            .args(args)
            .current_dir(&place.folder)
            .env("PATH", &install_path)
            .env("PRIVATEER_PYTHON", "/alpha/stale-python3")
            .env("HOME", &place.root)
            .env("XDG_CONFIG_HOME", place.root.join("config"))
            .env("PRIVATEER_SERVICE", "systemd")
            .env("PRIVATEER_SYSTEMCTL", &fake)
            .env("PRIVATEER_RESOURCE_HOME", place.root.join("ledger"))
            .env("PRIVATEER_FLEET", place.root.join("fleet.sqlite"))
            .env_remove("PRIVATEER_HOME")
            .output()
            .unwrap()
    };
    let started = run(&["service", "start"]);
    assert!(started.status.success(), "{}", text(&started));
    let units: Vec<_> = fs::read_dir(place.root.join("config/systemd/user"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(units.len(), 1);
    let name = units[0].file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.starts_with("privateer-infra-") && name.ends_with(".service"));
    let unit = fs::read_to_string(&units[0]).unwrap();
    assert!(
        unit.contains(&format!(
            "ExecStart=\"{}\" --home \"{}\" serve",
            place.binary.display(),
            place.folder.join(".privateer").display()
        )),
        "{unit}"
    );
    assert!(unit.contains(&format!("Environment=\"PATH={install_path}\"")));
    let python = PathBuf::from("/usr/bin/python3").canonicalize().unwrap();
    assert!(
        unit.contains(&format!(
            "Environment=\"PRIVATEER_PYTHON={}\"",
            python.display()
        )),
        "{unit}"
    );
    let status = text(&run(&["service", "status"]));
    assert!(
        status.contains(&format!("Python: {:?}", python.to_str().unwrap())),
        "{status}"
    );
    assert!(unit.contains(&format!(
        "Environment=\"PRIVATEER_FLEET={}\"",
        place.root.join("fleet.sqlite").display()
    )));
    assert!(text(&run(&["service", "status"])).contains(&format!("user service {name}")));
    let stopped = run(&["service", "stop"]);
    assert!(stopped.status.success(), "{}", text(&stopped));
    assert!(!units[0].exists());
    let log = fs::read_to_string(&calls).unwrap();
    assert!(log.contains("--user daemon-reload"));
    assert!(log.contains(&format!("--user enable --now {name}")));
    assert!(log.contains(&format!("--user disable --now {name}")));
}

#[test]
fn a_launchd_agent_runs_the_dispatcher_until_bootout_stops_it() {
    let place = Place::new();
    assert!(place.run(&place.folder, &["init"], "none").status.success());
    // A stand-in for launchd: bootstrap runs the agent's program with its
    // recorded environment, bootout sends it SIGTERM.
    let calls = place.root.join("launchctl.log");
    let fake = place.root.join("bin/fake-launchctl");
    fs::write(
        &fake,
        format!(
            r#"#!{python}
import os, pathlib, plistlib, signal, subprocess, sys
pathlib.Path({calls:?}).open('a').write(' '.join(sys.argv[1:]) + '\n')
pid = pathlib.Path({calls:?} + '.pid')
if sys.argv[1] == 'bootstrap':
    agent = plistlib.load(open(sys.argv[3], 'rb'))
    log = open(agent['StandardOutPath'], 'a')
    env = dict(agent['EnvironmentVariables'], HOME=os.environ['HOME'])
    child = subprocess.Popen(agent['ProgramArguments'], env=env, stdin=subprocess.DEVNULL,
                             stdout=log, stderr=log, start_new_session=True)
    pid.write_text(str(child.pid))
elif sys.argv[1] == 'bootout':
    if not pid.exists():
        sys.exit(3)
    os.kill(int(pid.read_text()), signal.SIGTERM)
    pid.unlink()
"#,
            python = support::python().display(),
            calls = calls.to_str().unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        let mut command = support::command(&place.binary);
        command
            .args(args)
            .current_dir(&place.folder)
            .env("PATH", &place.path)
            .env("HOME", &place.root)
            .env("PRIVATEER_SERVICE", "launchd")
            .env("PRIVATEER_LAUNCHCTL", &fake)
            .env("PRIVATEER_RESOURCE_HOME", place.root.join("ledger"))
            .env("PRIVATEER_FLEET", place.root.join("fleet.sqlite"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("PRIVATEER_HOME");
        fixture_output(&mut command)
    };
    let started = run(&["service", "start"]);
    assert!(started.status.success(), "{}", text(&started));
    let agents: Vec<_> = fs::read_dir(place.root.join("Library/LaunchAgents"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(agents.len(), 1);
    let label = agents[0].file_stem().unwrap().to_str().unwrap().to_owned();
    assert!(label.starts_with("privateer-infra-"), "{label}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        let status = text(&run(&["service", "status"]));
        if status.contains(": running") || std::time::Instant::now() > deadline {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(
        status.contains(&format!("running (launchd agent {label})")),
        "{status}"
    );
    // Stopping the dispatcher must leave job supervisors in its group alone.
    let agent = fs::read_to_string(&agents[0]).unwrap();
    assert!(
        agent.contains("<key>AbandonProcessGroup</key><true/>"),
        "{agent}"
    );
    let again = run(&["service", "start"]);
    assert!(text(&again).contains("already running"), "{}", text(&again));

    let stopped = run(&["service", "stop"]);
    assert!(stopped.status.success(), "{}", text(&stopped));
    assert!(!agents[0].exists());
    let status = text(&run(&["service", "status"]));
    assert!(status.contains("not running (no user service)"), "{status}");
    let log = fs::read_to_string(&calls).unwrap();
    let uid = unsafe { libc::getuid() };
    assert!(log.contains(&format!("bootstrap gui/{uid} {}", agents[0].display())));
    assert!(log.contains(&format!("bootout gui/{uid}/{label}")));
}

#[test]
fn dashboard_entry_uses_installed_sibling_or_path_in_both_environments() {
    let place = Place::new();
    let path_binary = place.root.join("runtime/privateer");
    fs::create_dir(path_binary.parent().unwrap()).unwrap();
    fs::copy(BIN, &path_binary).unwrap();
    let dashboard = place.root.join("bin/privateer-dashboard");
    let probe = place.root.join("dashboard-argv");
    fs::write(
        &dashboard,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", probe.display()),
    )
    .unwrap();
    fs::set_permissions(&dashboard, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |binary: &PathBuf, args: &[&str], herdr: bool| {
        let mut command = support::command(binary);
        command
            .args(args)
            .current_dir(&place.folder)
            .env("PATH", &place.path)
            .env("HOME", &place.root)
            .env("XDG_CONFIG_HOME", place.root.join("config"))
            .env("PRIVATEER_FLEET", place.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", place.root.join("ledger"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(key);
            }
        }
        if herdr {
            command.env("HERDR_ENV", "1");
        }
        fixture_output(&mut command)
    };
    // The first runtime finds a sibling; the second finds the same probe on PATH.
    for binary in [&place.binary, &path_binary] {
        for herdr in [false, true] {
            let fresh = run(binary, &[], herdr);
            assert!(fresh.status.success(), "{}", text(&fresh));
            assert!(!place.folder.join(".privateer").exists());
            assert!(!place.root.join("fleet.sqlite").exists());
            assert!(place.probes().is_empty());
            // The bridge only: nothing boards, so no ship becomes attended.
            assert_eq!(fs::read_to_string(&probe).unwrap().trim(), "");
        }
    }
    assert!(
        run(&place.binary, &["init", "--harness", "codex"], false)
            .status
            .success()
    );
    let mut launches = 0;
    for binary in [&place.binary, &path_binary] {
        for herdr in [false, true] {
            assert!(run(binary, &[], herdr).status.success());
            let args = fs::read_to_string(&probe).unwrap();
            assert_eq!(
                args.lines().collect::<Vec<_>>(),
                ["--home", place.folder.join(".privateer").to_str().unwrap()]
            );
            assert_eq!(place.probes().len(), launches);
            assert!(run(binary, &["start"], herdr).status.success());
            launches += 1;
            assert_eq!(place.probes().len(), launches);
        }
    }
    fs::remove_file(&dashboard).unwrap();
    for binary in [&place.binary, &path_binary] {
        for herdr in [false, true] {
            assert!(run(binary, &[], herdr).status.success());
            launches += 1;
            assert_eq!(place.probes().len(), launches);
        }
    }
}
