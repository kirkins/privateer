//! `privateer upgrade adopt`: a fake install (a published release, real-file
//! launchers in a fake HOME's ~/.cargo/bin and ~/.local/bin, generated
//! dispatcher and door units and a launchd agent naming them) with fake
//! `systemctl` and `launchctl` that only log. Adopt always runs from a copy
//! inside the fixture, never the test binary: it would adopt the directory
//! it runs from.
mod support;

use privateer::releases::fetch::SUMS;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use support::BIN;

const C: &str = "3333333333333333333333333333333333333333";
const OLD: &str = "1111111111111111111111111111111111111111";
const BROKER: &str = env!("CARGO_BIN_EXE_privateer-mcp-broker");

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_string_lossy().into());
        }
    }
}

/// A complete, checked release at `releases/<commit>`, as fetch publishes.
fn release(data: &Path, commit: &str) {
    let root = data.join("releases").join(commit);
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::copy(BIN, root.join("bin/privateer")).unwrap();
    fs::copy(BROKER, root.join("bin/privateer-mcp-broker")).unwrap();
    executable(&root.join("bin/privateer-dashboard"), "#!/bin/sh\nexit 0\n");
    let plugins = root.join("share/privateer/plugins/pi");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("manifest.json"),
        json!({"schema_version":1,"id":"pi","executable":["python3","adapter.py"]}).to_string(),
    )
    .unwrap();
    let mut names = Vec::new();
    walk(&root, &root, &mut names);
    names.sort();
    let sums: String = names
        .iter()
        .map(|n| {
            format!(
                "{:x}  {n}\n",
                Sha256::digest(fs::read(root.join(n)).unwrap())
            )
        })
        .collect();
    fs::write(root.join(SUMS), sums).unwrap();
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    /// A machine installed the old way: privateer copied into ~/.cargo/bin
    /// and ~/.local/bin, a dispatcher and a door unit and a dispatcher agent
    /// naming them, and release C fetched but not selected.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let world = Self { _dir: dir, root };
        release(&world.data(), C);
        for launcher in [world.cargo(), world.local()] {
            fs::create_dir_all(launcher.parent().unwrap()).unwrap();
            fs::copy(BIN, &launcher).unwrap();
        }
        fs::create_dir_all(world.units()).unwrap();
        let unit = |description: &str, program: &Path, args: &str, kill: &str| {
            format!(
                "[Unit]\nDescription={description}\n\n[Service]\nExecStart=\"{}\" {args}\nEnvironment=\"PATH=/usr/bin:/bin\"\nKillMode={kill}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
                program.display()
            )
        };
        let home = world.root.join("ship");
        fs::write(
            world.dispatcher_unit(),
            unit(
                "Privateer dispatcher for ship infra",
                &world.cargo(),
                &format!("--home \"{}\" serve", home.display()),
                "control-group",
            ),
        )
        .unwrap();
        fs::write(
            world.door_unit(),
            unit(
                "Privateer front door tg",
                &world.cargo(),
                "door run tg",
                "control-group",
            ),
        )
        .unwrap();
        fs::write(
            world.units().join("other.service"),
            "[Service]\nExecStart=/x\n",
        )
        .unwrap();
        fs::create_dir_all(world.agent().parent().unwrap()).unwrap();
        fs::write(
            world.agent(),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>privateer-apps-0000cccc</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--home</string><string>{}</string><string>serve</string></array>\n<key>AbandonProcessGroup</key><true/>\n</dict></plist>\n",
                world.local().display(),
                home.display()
            ),
        )
        .unwrap();
        fs::create_dir_all(world.root.join("fake")).unwrap();
        for name in ["systemctl", "launchctl"] {
            executable(
                &world.root.join("fake").join(name),
                &format!(
                    "#!/bin/sh\necho \"{name} $*\" >> '{}'\n",
                    world.calls_log().display()
                ),
            );
        }
        world
    }

    fn user(&self) -> PathBuf {
        self.root.join("user")
    }
    fn data(&self) -> PathBuf {
        self.root.join("data/privateer")
    }
    fn cargo(&self) -> PathBuf {
        self.user().join(".cargo/bin/privateer")
    }
    fn local(&self) -> PathBuf {
        self.user().join(".local/bin/privateer")
    }
    fn units(&self) -> PathBuf {
        self.root.join("config/systemd/user")
    }
    fn dispatcher_unit(&self) -> PathBuf {
        self.units().join("privateer-infra-0000aaaa.service")
    }
    fn door_unit(&self) -> PathBuf {
        self.units().join("privateer-door-tg-0000bbbb.service")
    }
    fn agent(&self) -> PathBuf {
        self.user()
            .join("Library/LaunchAgents/privateer-apps-0000cccc.plist")
    }
    fn calls_log(&self) -> PathBuf {
        self.root.join("fake/calls.log")
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.calls_log()).unwrap_or_default()
    }
    fn entry(&self) -> PathBuf {
        self.data().join("current/bin/privateer")
    }

    /// Run `program` (a launcher in this fixture, never the test binary)
    /// with every user directory and service manager inside the fixture.
    fn run(&self, program: &Path, args: &[&str]) -> Output {
        assert!(
            program.starts_with(&self.root)
                && self
                    .root
                    .starts_with(std::env::temp_dir().canonicalize().unwrap()),
            "adopt runs only from a copy inside a temporary fixture, never against the real HOME"
        );
        let mut command = support::command(program);
        command
            .current_dir(&self.root)
            .env("HOME", self.user())
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("PRIVATEER_FLEET", self.root.join("fleet.sqlite"))
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "systemd")
            .env("PRIVATEER_SYSTEMCTL", self.root.join("fake/systemctl"))
            .env("PRIVATEER_LAUNCHCTL", self.root.join("fake/launchctl"))
            .env_remove("CARGO_HOME")
            .args(args);
        command.output().unwrap()
    }

    fn adopt(&self, program: &Path, args: &[&str]) -> Output {
        let mut all = vec!["upgrade", "adopt"];
        all.extend(args);
        self.run(program, &all)
    }

    fn status(&self) -> Value {
        json_of(&self.run(&self.cargo(), &["upgrade", "status"]))
    }

    /// The files adoption may change, to show what it left alone.
    fn snapshot(&self) -> Vec<(PathBuf, Option<PathBuf>, Vec<u8>)> {
        let mut files = vec![
            self.cargo(),
            self.local(),
            self.user().join(".cargo/bin/privateer-dashboard"),
            self.user().join(".local/bin/privateer-dashboard"),
            self.dispatcher_unit(),
            self.door_unit(),
            self.units().join("other.service"),
            self.agent(),
            self.data().join("current"),
        ];
        files.sort();
        files
            .into_iter()
            .map(|p| {
                let link = fs::read_link(&p).ok();
                let bytes = if link.is_none() {
                    fs::read(&p).unwrap_or_default()
                } else {
                    Vec::new()
                };
                (p, link, bytes)
            })
            .collect()
    }
}

fn json_of(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn refused(output: &Output, says: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(says),
        "{stderr}"
    );
}

/// Each action's kind, path and state, in order.
fn actions(report: &Value) -> Vec<(String, String, String)> {
    report["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["kind"].as_str().unwrap().into(),
                a["path"].as_str().unwrap_or_default().into(),
                a["state"].as_str().unwrap().into(),
            )
        })
        .collect()
}

fn path(p: PathBuf) -> String {
    p.to_string_lossy().into()
}

#[test]
fn adopt_moves_launchers_and_units_through_current_restarting_nothing() {
    let w = World::new();
    let before = w.snapshot();
    let plan = |state: &str| {
        let s = || state.to_string();
        vec![
            ("current".into(), path(w.data().join("current")), s()),
            ("launcher".into(), path(w.cargo()), s()),
            ("launcher".into(), path(w.local()), s()),
            ("unit".into(), path(w.door_unit()), s()),
            ("unit".into(), path(w.dispatcher_unit()), s()),
            ("unit".into(), path(w.agent()), s()),
            ("reload".into(), String::new(), s()),
        ]
    };

    // A dry run lists the plan and changes nothing.
    let dry = json_of(&w.adopt(&w.cargo(), &["--dry-run", "--commit", C]));
    assert_eq!(actions(&dry), plan("would"), "{dry:#}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["noop"], false);
    assert_eq!(dry["actions"][1]["proof"], "the running image");
    assert_eq!(dry["actions"][2]["proof"], "privateer answered as itself");
    assert_eq!(w.snapshot(), before);
    assert_eq!(w.calls(), "");
    assert!(!w.root.join("fleet.sqlite").exists());

    // Adopting does the same plan.
    let done = json_of(&w.adopt(&w.cargo(), &["--commit", C]));
    assert_eq!(actions(&done), plan("done"), "{done:#}");
    assert_eq!(
        fs::read_link(w.data().join("current")).unwrap(),
        Path::new("releases").join(C)
    );
    for launcher in [w.cargo(), w.local()] {
        assert_eq!(fs::read_link(&launcher).unwrap(), w.entry());
    }
    let exec = format!("ExecStart=\"{}\" ", w.entry().display());
    let dispatcher = fs::read_to_string(w.dispatcher_unit()).unwrap();
    assert!(
        dispatcher.contains(&format!(
            "{exec}--home \"{}\" serve\n",
            w.root.join("ship").display()
        )) && dispatcher.contains("\nKillMode=process\n")
            && !dispatcher.contains("control-group"),
        "{dispatcher}"
    );
    // A door keeps its own stop policy: its adapter must stop with it.
    let door = fs::read_to_string(w.door_unit()).unwrap();
    assert!(
        door.contains(&format!("{exec}door run tg\n")) && door.contains("KillMode=control-group"),
        "{door}"
    );
    let agent = fs::read_to_string(w.agent()).unwrap();
    assert!(
        agent.contains(&format!(
            "<array><string>{}</string><string>--home</string>",
            w.entry().display()
        )),
        "{agent}"
    );
    assert_eq!(
        fs::read(w.units().join("other.service")).unwrap(),
        b"[Service]\nExecStart=/x\n"
    );
    // One reload; nothing started, stopped or restarted, launchd untouched.
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n");
    let status = w.status();
    assert_eq!(status["current_commit"], C);
    assert_eq!(status["launcher_unversioned"], json!([]));
    assert_eq!(status["history"].as_array().unwrap().len(), 1, "{status:#}");
    assert_eq!(status["history"][0]["kind"], "adopt");
    assert_eq!(status["history"][0]["to_commit"], C);

    // Again, from the launcher that now runs the release: nothing to do.
    let after = w.snapshot();
    let again = json_of(&w.adopt(&w.cargo(), &[]));
    assert_eq!(again["noop"], true, "{again:#}");
    let mut already = plan("already");
    already.pop();
    assert_eq!(actions(&again), already);
    assert_eq!(w.snapshot(), after);
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n");
    assert_eq!(w.status()["history"].as_array().unwrap().len(), 1);

    // A later `cargo install` makes a launcher a real file again.
    fs::remove_file(w.local()).unwrap();
    fs::copy(BIN, w.local()).unwrap();
    assert_eq!(w.status()["launcher_unversioned"], json!([w.local()]));
}

#[test]
fn adopt_refuses_what_it_cannot_prove_and_leaves_it_alone() {
    let w = World::new();
    let before = w.snapshot();
    refused(&w.adopt(&w.cargo(), &[]), "pass --commit");
    refused(
        &w.adopt(&w.cargo(), &["--commit", OLD]),
        &format!("run `privateer upgrade fetch {OLD}` first"),
    );
    // Switching an installation that already selects a release is not
    // adoption's.
    fs::create_dir_all(w.data().join("releases").join(OLD)).unwrap();
    symlink(Path::new("releases").join(OLD), w.data().join("current")).unwrap();
    refused(
        &w.adopt(&w.cargo(), &["--commit", C]),
        &format!("current already selects {OLD}"),
    );
    fs::remove_file(w.data().join("current")).unwrap();
    assert_eq!(w.snapshot(), before);
    assert_eq!(w.calls(), "");

    // An installed privateer that is another build than the release's may be
    // newer: adopt cannot tell, so a real run needs the operator's
    // --allow-different-build. A dry run still lists the whole plan and says
    // it would refuse.
    let mut other = fs::read(BIN).unwrap();
    other.extend_from_slice(b"another build");
    fs::write(w.local(), &other).unwrap();
    let different = w.snapshot();
    let dry = json_of(&w.adopt(&w.cargo(), &["--dry-run", "--commit", C]));
    assert_eq!(dry["would_refuse"], json!("different build"), "{dry:#}");
    assert!(
        dry["refusal"]
            .as_str()
            .unwrap()
            .contains(&format!("different build from releases/{C}")),
        "{dry:#}"
    );
    assert_eq!(
        dry["installed"],
        json!([{"path": w.cargo(), "same_build": true}, {"path": w.local(), "same_build": false}]),
        "{dry:#}"
    );
    let allowed = json_of(&w.adopt(
        &w.cargo(),
        &["--dry-run", "--allow-different-build", "--commit", C],
    ));
    assert_eq!(allowed.get("would_refuse"), None, "{allowed:#}");
    assert_eq!(actions(&dry), actions(&allowed));
    assert!(
        actions(&dry)
            .iter()
            .any(|(kind, _, state)| kind == "unit" && state == "would"),
        "{dry:#}"
    );
    refused(
        &w.adopt(&w.cargo(), &["--commit", C]),
        "pass --allow-different-build if releases/",
    );
    assert_eq!(w.snapshot(), different);
    assert_eq!(w.calls(), "");
    assert_eq!(w.status()["history"], json!([]));
    fs::copy(BIN, w.local()).unwrap();
    assert_eq!(w.snapshot(), before);

    // A launcher linked elsewhere, a native program that does not answer as
    // the dashboard, and a script wrapper are reported and left alone.
    let elsewhere = w.root.join("elsewhere/privateer");
    fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    fs::copy(BIN, &elsewhere).unwrap();
    fs::remove_file(w.local()).unwrap();
    symlink(&elsewhere, w.local()).unwrap();
    let foreign = w.user().join(".cargo/bin/privateer-dashboard");
    fs::copy(BROKER, &foreign).unwrap();
    let wrapper = w.user().join(".local/bin/privateer-dashboard");
    executable(
        &wrapper,
        "#!/bin/sh\nexec /opt/elsewhere/privateer-dashboard \"$@\"\n",
    );
    let left = w.snapshot();
    // A reload that fails stops the run after the files were replaced: the
    // report says which were done, and the command fails.
    executable(
        &w.root.join("fake/systemctl"),
        &format!(
            "#!/bin/sh\necho \"systemctl $*\" >> '{}'\nexit 1\n",
            w.calls_log().display()
        ),
    );
    let output = w.adopt(&w.cargo(), &["--commit", C]);
    refused(&output, "upgrade adopt stopped");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let states: Vec<_> = actions(&report)
        .into_iter()
        .filter(|(_, _, state)| state != "left")
        .map(|(kind, _, state)| (kind, state))
        .collect();
    let done = |k: &str| (k.to_string(), "done".to_string());
    assert_eq!(
        states,
        [
            done("current"),
            done("launcher"),
            done("unit"),
            done("unit"),
            ("reload".into(), "failed".into())
        ],
        "{report:#}"
    );
    assert_eq!(fs::read_link(w.cargo()).unwrap(), w.entry());
    assert!(
        fs::read_to_string(w.dispatcher_unit())
            .unwrap()
            .contains(&w.entry().display().to_string())
    );
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("rerun `privateer upgrade adopt`"),
        "{report:#}"
    );
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n");
    let find = |p: &Path| {
        report["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["path"] == json!(p))
            .unwrap_or_else(|| panic!("{} is not reported: {report:#}", p.display()))
            .clone()
    };
    assert_eq!(find(&w.cargo())["state"], "done");
    for (file, reason) in [
        (w.local(), "a link to"),
        (
            foreign.clone(),
            "did not answer as Privateer's privateer-dashboard",
        ),
        (wrapper.clone(), "not a native executable"),
        // The agent runs that linked-elsewhere launcher, which is not moved.
        (w.agent(), "neither a launcher this adoption moves"),
    ] {
        let action = find(&file);
        assert_eq!(action["state"], "left", "{action}");
        assert!(
            action["reason"].as_str().unwrap().contains(reason),
            "{action}"
        );
    }
    let now = w.snapshot();
    for (file, _, _) in &left {
        if [w.local(), foreign.clone(), wrapper.clone()].contains(file) {
            assert_eq!(
                now.iter().find(|(p, _, _)| p == file),
                left.iter().find(|(p, _, _)| p == file)
            );
        }
    }
    // Status flags the dashboard file, which is not a release's.
    let status = w.status();
    // A stopped adoption is not recorded as one.
    assert_eq!(status["history"], json!([]), "{status:#}");
    assert_eq!(
        status["launcher_unversioned"],
        json!([foreign]),
        "{status:#}"
    );

    // Once systemctl works, a rerun finds everything in place but nothing
    // recorded: it reloads and records the adoption, and then is a no-op.
    executable(
        &w.root.join("fake/systemctl"),
        &format!(
            "#!/bin/sh\necho \"systemctl $*\" >> '{}'\n",
            w.calls_log().display()
        ),
    );
    let rerun = json_of(&w.adopt(&w.cargo(), &[]));
    assert_eq!(rerun["noop"], false, "{rerun:#}");
    let changed: Vec<_> = actions(&rerun)
        .into_iter()
        .filter(|(_, _, state)| !["left", "already"].contains(&state.as_str()))
        .collect();
    assert_eq!(
        changed,
        [("reload".into(), String::new(), "done".into())],
        "{rerun:#}"
    );
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n".repeat(2));
    let history = &w.status()["history"];
    assert_eq!(history.as_array().unwrap().len(), 1, "{history:#}");
    assert_eq!(history[0]["kind"], "adopt");
    let again = json_of(&w.adopt(&w.cargo(), &[]));
    assert_eq!(again["noop"], true, "{again:#}");
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n".repeat(2));
}

/// Restores a directory's permissions however the test ends, so the
/// fixture can be removed.
struct Writable(PathBuf);
impl Drop for Writable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn adopt_repoints_only_units_running_what_it_adopts_past_an_unwritable_launcher() {
    let w = World::new();
    // Adopt runs from a privateer in a directory this user cannot write,
    // like /usr/local/bin: it is proven but left, and the run goes on.
    let system = w.root.join("system/bin/privateer");
    fs::create_dir_all(system.parent().unwrap()).unwrap();
    fs::copy(BIN, &system).unwrap();
    let dev = w.root.join("dev/target/release/privateer");
    fs::create_dir_all(dev.parent().unwrap()).unwrap();
    fs::copy(BIN, &dev).unwrap();
    // Another name for the cargo launcher's file is the same launcher.
    let alias = w.root.join("alias/privateer");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    fs::hard_link(w.cargo(), &alias).unwrap();
    let pinned = |name: &str, program: &Path| {
        let unit = w.units().join(format!("privateer-{name}-0000eeee.service"));
        fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=Privateer dispatcher for ship {name}\n\n[Service]\nExecStart=\"{}\" --home \"/ships/{name}\" serve\nKillMode=process\n",
                program.display()
            ),
        )
        .unwrap();
        unit
    };
    let left = [
        pinned("dev", &dev),
        pinned("system", &system),
        pinned(
            "other",
            &w.root.join("other/privateer/current/bin/privateer"),
        ),
    ];
    let moved = [
        pinned("alias", &alias),
        pinned(
            "release",
            &w.data().join("releases").join(C).join("bin/privateer"),
        ),
    ];
    // An unquoted program after blanks is found and replaced in place; one
    // with a systemd prefix is not what `start` writes, and is left.
    let door = |name: &str, exec: &str| {
        let unit = w
            .units()
            .join(format!("privateer-door-{name}-0000ffff.service"));
        fs::write(
            &unit,
            format!("[Unit]\nDescription=Privateer front door {name}\n\n[Service]\n{exec} door run {name}\nKillMode=control-group\n"),
        )
        .unwrap();
        unit
    };
    let blanks = door("blanks", &format!("ExecStart=  {}", w.cargo().display()));
    let prefixed = door("prefixed", &format!("ExecStart=-{}", w.cargo().display()));
    // A door named `serve` is a door: its agent keeps its process group.
    let serve = w
        .agent()
        .with_file_name("privateer-door-serve-0000ffff.plist");
    fs::write(
        &serve,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>privateer-door-serve-0000ffff</string>\n<key>ProgramArguments</key><array><string>{}</string><string>door</string><string>run</string><string>serve</string></array>\n<key>AbandonProcessGroup</key><false/>\n</dict></plist>\n",
            w.cargo().display()
        ),
    )
    .unwrap();
    let kept: Vec<_> = left.iter().map(|u| fs::read(u).unwrap()).collect();
    let prefixed_text = fs::read(&prefixed).unwrap();
    fs::set_permissions(system.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = Writable(system.parent().unwrap().to_path_buf());

    let report = json_of(&w.adopt(&system, &["--commit", C]));
    let find = |p: &Path| {
        report["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["path"] == json!(p))
            .unwrap_or_else(|| panic!("{} is not reported: {report:#}", p.display()))
            .clone()
    };
    let launcher = find(&system);
    assert_eq!(launcher["state"], "left", "{launcher}");
    assert_eq!(launcher["proof"], "the running image");
    assert!(
        launcher["reason"]
            .as_str()
            .unwrap()
            .contains("not writable"),
        "{launcher}"
    );
    assert!(fs::symlink_metadata(&system).unwrap().is_file());
    for launcher in [w.cargo(), w.local()] {
        assert_eq!(fs::read_link(&launcher).unwrap(), w.entry());
    }
    for (unit, text) in left.iter().zip(&kept) {
        let action = find(unit);
        assert_eq!(action["state"], "left", "{action}");
        assert!(
            action["reason"]
                .as_str()
                .unwrap()
                .contains("neither a launcher this adoption moves"),
            "{action}"
        );
        assert_eq!(&fs::read(unit).unwrap(), text);
    }
    for unit in moved {
        assert_eq!(find(&unit)["state"], "done", "{report:#}");
        assert!(
            fs::read_to_string(&unit)
                .unwrap()
                .contains(&format!("ExecStart=\"{}\" ", w.entry().display()))
        );
    }
    assert_eq!(find(&w.dispatcher_unit())["state"], "done");
    assert_eq!(find(&blanks)["state"], "done");
    let text = fs::read_to_string(&blanks).unwrap();
    assert!(
        text.contains(&format!(
            "\nExecStart=  \"{}\" door run blanks\n",
            w.entry().display()
        )),
        "{text}"
    );
    let action = find(&prefixed);
    assert_eq!(action["state"], "left", "{action}");
    assert!(
        action["reason"]
            .as_str()
            .unwrap()
            .contains("neither quoted nor an absolute path"),
        "{action}"
    );
    assert_eq!(fs::read(&prefixed).unwrap(), prefixed_text);
    let action = find(&serve);
    assert_eq!(
        (&action["service"], &action["state"]),
        (&json!("door"), &json!("done")),
        "{action}"
    );
    let text = fs::read_to_string(&serve).unwrap();
    assert!(
        text.contains(&format!("<array><string>{}</string>", w.entry().display()))
            && text.contains("<key>AbandonProcessGroup</key><false/>"),
        "{text}"
    );
    assert_eq!(w.calls(), "systemctl --user daemon-reload\n");
    assert!(report.get("error").is_none(), "{report:#}");
}
