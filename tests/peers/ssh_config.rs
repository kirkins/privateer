use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn explicit_ssh_config_is_persisted_passed_and_revalidated() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("alpha");
    let config = root.path().join("alpha config");
    fs::write(&config, "Host *\n").unwrap();
    let endpoint = json!({"name":"beta", "host":"beta", "executable":binary(),
        "home":root.path().join("beta"), "ssh_config":config});
    ok(&home, "peer.put", endpoint, None);
    assert_eq!(
        ok(&home, "peer.list", json!({}), None)["peers"][0]["ssh_config"],
        json!(config)
    );
    let ssh = script(
        root.path(),
        "ssh",
        r#"
[ "$1" = '-F' ] || exit 2
[ "$2" = "$EXPECTED_CONFIG" ] || exit 3
shift 2
[ "$1" = '-T' ] || exit 4
shift
for option in StrictHostKeyChecking=yes BatchMode=yes ConnectTimeout=10; do
    [ "$1" = '-o' ] && [ "$2" = "$option" ] || exit 5
    shift 2
done
[ "$1" = '--' ] && [ "$2" = 'beta' ] || exit 6
read -r request
printf '%s\n' '{"id":"alpha","ok":true,"result":{}}'
"#,
    );
    let request = json!({"name":"beta","id":"alpha","command":"status"});
    let output = support::command(binary())
        .args([
            "--home",
            home.to_str().unwrap(),
            "peer.call",
            "--json",
            &request.to_string(),
        ])
        .env("PRIVATEER_SSH_EXECUTABLE", &ssh)
        .env("EXPECTED_CONFIG", &config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::set_permissions(&config, fs::Permissions::from_mode(0o622)).unwrap();
    let rejected = run(&home, "peer.call", request.clone(), Some(&ssh));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("group/other writable"));
    fs::remove_file(&config).unwrap();
    let rejected = run(&home, "peer.call", request, Some(&ssh));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("inspect SSH config"));
}

#[test]
fn config_diagnostic_survives_an_early_ssh_exit_and_does_not_classify_auth_failures() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("alpha");
    put(
        &home,
        &root.path().join("beta"),
        Some("beta"),
        10_000,
        1_048_576,
        Path::new(binary()),
    );
    for (name, message, classified) in [
        (
            "config",
            "Bad owner or permissions on /alpha/ssh_config.d/beta.conf",
            true,
        ),
        ("auth", "Permission denied (publickey).", false),
    ] {
        let ssh = script(
            root.path(),
            name,
            &format!("printf '%s\\n' '{message}' >&2\nexit 255"),
        );
        // Send through RPC to avoid the OS argv limit and exercise the
        // early-exit / broken-stdin boundary with a request larger than a pipe.
        let rejected = run_rpc(
            &home,
            "peer.call",
            json!({"name":"infrastructure","command":"status",
            "args":{"large":"x".repeat(200_000)}}),
            &ssh,
        );
        let response: Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(response["ok"], false, "{response}");
        let diagnostic = response["error"].as_str().unwrap();
        if classified {
            assert!(
                diagnostic.contains("/alpha/ssh_config.d/beta.conf"),
                "{diagnostic}"
            );
            assert!(
                diagnostic.contains("bad owner or permissions"),
                "{diagnostic}"
            );
        } else {
            assert!(diagnostic.contains(message), "{diagnostic}");
        }
        assert_eq!(diagnostic.contains("job.start"), classified, "{diagnostic}");
    }
}
