use super::*;

#[test]
fn machine_link_cli_and_json_keep_ssh_config_local_and_propagate_to_homes() {
    let temp = support::tempdir();
    let root = fs::canonicalize(temp.path()).unwrap();
    let _world = World {
        _temp: temp,
        root: root.clone(),
    };
    let alpha = Machine {
        root: root.join("alpha"),
    };
    let beta = Machine {
        root: root.join("beta"),
    };
    alpha.ship("alpha");
    beta.ship("beta");
    let ssh = remote_ssh(&beta);
    let config = root.join("alpha.conf");
    fs::write(&config, "Host *\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    for cli in [true, false] {
        let registry = beta.registry();
        let args = json!({"name":"beta", "target":"beta", "executable":support::BIN,
            "fleet":registry, "as":"alpha", "ssh_config":config});
        let output = if cli {
            alpha.run(
                "alpha",
                &[
                    "machine",
                    "add",
                    "beta",
                    "beta",
                    "--executable",
                    support::BIN,
                    "--remote-fleet",
                    registry.to_str().unwrap(),
                    "--as",
                    "alpha",
                    "--ssh-config",
                    config.to_str().unwrap(),
                ],
                Some(&ssh),
            )
        } else {
            alpha.run(
                "alpha",
                &["machine.add", "--json", &args.to_string()],
                Some(&ssh),
            )
        };
        assert!(output.status.success(), "{}", text(&output));
        let conn = rusqlite::Connection::open(alpha.registry()).unwrap();
        let peer: String = conn
            .query_row("SELECT peer FROM machines WHERE name='beta'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&peer).unwrap()["ssh_config"],
            json!(config)
        );
        let peers = alpha.ok("alpha", "peer.list", json!({}));
        assert_eq!(peers["peers"][0]["ssh_config"], json!(config));
        let reverse = beta.ok("beta", "peer.list", json!({}));
        assert!(reverse["peers"][0].get("ssh_config").is_none());
        let listed = alpha.ok("alpha", "machine.list", json!({}));
        assert_eq!(listed["machines"][0]["ssh_config"], json!(config));
    }
}
