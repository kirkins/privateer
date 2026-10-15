use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

struct Place {
    _temp: tempfile::TempDir,
    root: PathBuf,
    folder: PathBuf,
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
            probe,
        }
    }
    fn run(&self, dir: &PathBuf, args: &[&str], service: &str) -> Output {
        Command::new(BIN)
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
            .env_remove("HERDR_SOCKET_PATH")
            .output()
            .unwrap()
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
    let fake = place.root.join("bin/fake-systemctl");
    fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .current_dir(&place.folder)
            .env("PATH", &place.path)
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
            "ExecStart=\"{BIN}\" --home \"{}\" serve",
            place.folder.join(".privateer").display()
        )),
        "{unit}"
    );
    assert!(unit.contains(&format!("Environment=\"PATH={}\"", place.path)));
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
fn herdr_dashboard_entry_uses_installed_sibling_and_preserves_direct_start_and_fallback() {
    let place = Place::new();
    let binary = place.root.join("bin/privateer");
    fs::copy(BIN, &binary).unwrap();
    let dashboard = place.root.join("bin/privateer-dashboard");
    let probe = place.root.join("dashboard-argv");
    fs::write(
        &dashboard,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", probe.display()),
    )
    .unwrap();
    fs::set_permissions(&dashboard, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str], herdr: bool| {
        let mut command = Command::new(&binary);
        command
            .args(args)
            .current_dir(&place.folder)
            .env("PATH", &place.path)
            .env("HOME", &place.root)
            .env("PRIVATEER_FLEET", place.root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", place.root.join("ledger"))
            .env_remove("PRIVATEER_HOME")
            .env_remove("PRIVATEER_ROLE")
            .env_remove("PRIVATEER_ACTOR")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        if herdr {
            command.env("HERDR_ENV", "1");
        }
        command.output().unwrap()
    };
    let fresh = run(&[], true);
    assert!(fresh.status.success(), "{}", text(&fresh));
    assert!(!place.folder.join(".privateer").exists());
    assert!(place.probes().is_empty());
    assert_eq!(fs::read_to_string(&probe).unwrap(), "\n");
    assert!(run(&["init", "--harness", "codex"], false).status.success());
    assert!(run(&[], true).status.success());
    let args = fs::read_to_string(&probe).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["--home", place.folder.join(".privateer").to_str().unwrap()]
    );
    assert!(run(&["start"], true).status.success());
    assert_eq!(place.probes().len(), 1);
    assert!(run(&[], false).status.success());
    assert_eq!(place.probes().len(), 2);
    fs::remove_file(&dashboard).unwrap();
    assert!(run(&[], true).status.success());
    assert_eq!(place.probes().len(), 3);
}
