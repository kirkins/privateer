mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn the_captains_slash_commands_run_the_fleet_cli_as_the_captain() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("codex"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    let folder = root.join("ships/fixture");
    fs::create_dir_all(&folder).unwrap();
    let env = |command: &mut std::process::Command| {
        command
            .env("HOME", &root)
            .env("PRIVATEER_FLEET", root.join("fleet.sqlite"))
            .env("PRIVATEER_SERVICE", "none")
            .env("PRIVATEER_RESOURCE_HOME", root.join("ledger"));
    };
    let mut init = support::command(env!("CARGO_BIN_EXE_privateer"));
    // Only init needs the fake harness; Node keeps the inherited PATH.
    init.arg("init")
        .current_dir(&folder)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    env(&mut init);
    let made = init.output().unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );

    let mut node = support::command("node");
    node.arg("tests/pi-captain.mjs")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("PRIVATEER_ROLE", "agent")
        .env("PRIVATEER_ACTOR", "fixture")
        .env("PRIVATEER_HOME", folder.join(".privateer"))
        .env("PRIVATEER_CONTROL_CLI", env!("CARGO_BIN_EXE_privateer"))
        .env("PRIVATEER_ATTENDED_HARNESS", "pi");
    env(&mut node);
    let output = node
        .output()
        .expect("Node is required to check the captain commands");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
