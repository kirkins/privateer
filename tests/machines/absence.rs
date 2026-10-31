use super::*;

fn pair() -> (tempfile::TempDir, Machine, Machine, PathBuf) {
    let temp = support::tempdir();
    let alpha = Machine {
        root: temp.path().join("alpha-machine"),
    };
    let beta = Machine {
        root: temp.path().join("beta-machine"),
    };
    alpha.ship("alpha");
    beta.ship("beta-home");
    let ssh = remote_ssh(&beta);
    (temp, alpha, beta, ssh)
}

fn connect(from: &Machine, home: &str, name: &str, to: &Machine, ssh: &Path) {
    let registry = to.registry();
    let result = from.run(
        home,
        &[
            "machine",
            "add",
            name,
            "test-host",
            "--executable",
            support::BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            home,
        ],
        Some(ssh),
    );
    assert!(result.status.success(), "{}", text(&result));
}

fn refresh(machine: &Machine, home: &str, name: &str, ssh: &Path) {
    let result = machine.run(home, &["machine", "refresh", name], Some(ssh));
    assert!(result.status.success(), "{}", text(&result));
}

fn queue(alpha: &Machine) {
    let down = alpha.root.join("ssh-down");
    fs::write(&down, "#!/bin/sh\nexit 255\n").unwrap();
    fs::set_permissions(&down, fs::Permissions::from_mode(0o755)).unwrap();
    let sent = alpha.ok_over(
        "alpha",
        "mail.send",
        json!({
            "id":"survey","from":"alpha","to":"beta","kind":"request","body":"Survey results?"
        }),
        &down,
    );
    assert_eq!(sent["delivery"], "pending", "{sent}");
}

fn assert_pending(alpha: &Machine) {
    let mail = alpha.ok("alpha", "mail.show", json!({"id":"survey"}));
    assert_eq!(mail["delivery"], "pending", "{mail}");
    assert_eq!(mail["state"], "open", "{mail}");
}

fn assert_delivered(alpha: &Machine, beta: &Machine, ssh: &Path) {
    alpha.ok_over("alpha", "mail.flush", json!({}), ssh);
    let sent = alpha.ok("alpha", "mail.show", json!({"id":"survey"}));
    assert_eq!(sent["delivery"], "delivered", "{sent}");
    let inbox = beta.ok("beta", "mail.inbox", json!({"address":"beta"}));
    assert!(
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "survey"),
        "{inbox}"
    );
}

#[test]
fn pending_mail_recovers_after_machine_remove_and_relink() {
    let (_temp, alpha, beta, ssh) = pair();
    beta.ship("beta");
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    queue(&alpha);
    alpha.ok("alpha", "machine.remove", json!({"name":"beta"}));
    let flushed = alpha.ok("alpha", "mail.flush", json!({}));
    assert_eq!(flushed["refused"], 0, "{flushed}");
    assert_pending(&alpha);
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    assert_delivered(&alpha, &beta, &ssh);
}

#[test]
fn a_temporarily_missing_folder_never_counts_as_a_complete_directory() {
    let (_temp, alpha, beta, ssh) = pair();
    beta.ship("beta");
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    queue(&alpha);
    let hidden = beta.root.join("beta-unavailable");
    fs::rename(beta.folder("beta"), &hidden).unwrap();
    for _ in 0..5 {
        refresh(&alpha, "alpha", "beta", &ssh);
        assert_pending(&alpha);
    }
    fs::rename(hidden, beta.folder("beta")).unwrap();
    refresh(&alpha, "alpha", "beta", &ssh);
    assert_delivered(&alpha, &beta, &ssh);
}

#[test]
fn mail_survives_a_move_from_beta_to_gamma_before_gamma_lists_the_ship() {
    let (_temp, alpha, beta, beta_ssh) = pair();
    let gamma = Machine {
        root: beta.root.parent().unwrap().join("gamma-machine"),
    };
    gamma.ship("gamma");
    ship_with_project(&beta, &gamma, "beta");
    let gamma_ssh = remote_ssh(&gamma);
    connect(&beta, "beta-home", "gamma", &gamma, &gamma_ssh);
    connect(&alpha, "alpha", "beta", &beta, &beta_ssh);
    connect(&alpha, "alpha", "gamma", &gamma, &gamma_ssh);
    queue(&alpha);
    let moved = beta.run(
        "beta-home",
        &["ship", "move", "beta", "--to", "gamma"],
        Some(&gamma_ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));
    // A sees the departure first. Even repeated B refreshes cannot stand in
    // for C's directory, and old mail must remain recoverable in the gap.
    for _ in 0..4 {
        refresh(&alpha, "alpha", "beta", &beta_ssh);
        assert_pending(&alpha);
    }
    refresh(&alpha, "alpha", "gamma", &gamma_ssh);
    let address = alpha.ok("alpha", "ship.show", json!({"name":"beta"}));
    assert_eq!(address["location"], json!({"peer":"machine-gamma"}));
    assert_delivered(&alpha, &gamma, &gamma_ssh);
}
