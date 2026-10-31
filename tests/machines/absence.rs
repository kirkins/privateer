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
    assert_eq!(flushed["pending"], 1, "{flushed}");
    assert_eq!(flushed["undeliverable"], 0, "{flushed}");
    assert_pending(&alpha);
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    assert_delivered(&alpha, &beta, &ssh);
}

#[test]
fn pending_mail_survives_remaining_link_refreshes_after_remove_and_relink() {
    remove_and_relink_with_another_link(false);
}

#[test]
fn remove_also_protects_names_missing_from_the_last_partial_directory() {
    remove_and_relink_with_another_link(true);
}

fn remove_and_relink_with_another_link(partial: bool) {
    let (_temp, alpha, beta, beta_ssh) = pair();
    let gamma = Machine {
        root: beta.root.parent().unwrap().join("gamma-machine"),
    };
    beta.ship("beta");
    gamma.ship("gamma");
    let gamma_ssh = remote_ssh(&gamma);
    connect(&alpha, "alpha", "beta", &beta, &beta_ssh);
    connect(&alpha, "alpha", "gamma", &gamma, &gamma_ssh);
    queue(&alpha);
    if partial {
        let hidden = beta.root.join("beta-unavailable");
        fs::rename(beta.folder("beta"), &hidden).unwrap();
        refresh(&alpha, "alpha", "beta", &beta_ssh);
        assert_pending(&alpha);
        fs::rename(hidden, beta.folder("beta")).unwrap();
    }
    alpha.ok("alpha", "machine.remove", json!({"name":"beta"}));
    // Each command is a fresh process: protection must survive restarts and
    // more than the absence threshold of complete exchanges on the other link.
    for _ in 0..5 {
        refresh(&alpha, "alpha", "gamma", &gamma_ssh);
        assert_pending(&alpha);
    }
    connect(&alpha, "alpha", "beta", &beta, &beta_ssh);
    assert_delivered(&alpha, &beta, &beta_ssh);
}

#[test]
fn machine_list_shows_removed_machines_protected_names_and_pending_mail() {
    let (_temp, alpha, beta, ssh) = pair();
    beta.ship("beta");
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    queue(&alpha);
    alpha.ok("alpha", "machine.remove", json!({"name":"beta"}));
    let listed = alpha.ok("alpha", "machine.list", json!({}));
    let removed = listed["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{listed}");
    assert_eq!(removed[0]["machine"], "beta", "{listed}");
    let ships = removed[0]["ships"].as_array().unwrap();
    assert!(ships.contains(&json!("beta")), "{listed}");
    assert!(ships.contains(&json!("beta-home")), "{listed}");
    assert_eq!(removed[0]["pending_mail"], 1, "{listed}");
    assert_eq!(removed[0]["unread_homes"], 0, "{listed}");
    assert_eq!(removed[0]["still_linked"], false, "{listed}");
    assert_pending(&alpha);

    // Dropping the protection makes it disappear from the listing, and the
    // mail itself is neither refused nor deleted by that command.
    let forgotten = alpha.ok("alpha", "machine.forget_removed", json!({"name":"beta"}));
    assert_eq!(forgotten["cleared_protection"], 2, "{forgotten}");
    let listed = alpha.ok("alpha", "machine.list", json!({}));
    assert_eq!(listed["removed"], json!([]), "{listed}");
    assert_pending(&alpha);

    // An unknown machine is a clear error, on the plain CLI form too.
    let unknown = alpha.run("alpha", &["machine", "forget-removed", "mac"], None);
    assert!(!unknown.status.success(), "{}", text(&unknown));
    assert!(
        text(&unknown).contains("no removed machine named mac"),
        "{}",
        text(&unknown)
    );
}

#[test]
fn machine_list_reads_homes_without_creating_or_locking_them() {
    let (_temp, alpha, beta, ssh) = pair();
    beta.ship("beta");
    alpha.ship("gamma");
    alpha.ship("delta");
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    queue(&alpha);
    alpha.ok("alpha", "machine.remove", json!({"name":"beta"}));

    // A member folder with no database holds no mail: the listing must not
    // create, migrate or write-lock a home to count what waits there.
    let bare = alpha.folder("gamma").join(".privateer/privateer.sqlite");
    fs::remove_file(&bare).unwrap();
    let listed = alpha.ok("alpha", "machine.list", json!({}));
    let removed = listed["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{listed}");
    assert_eq!(removed[0]["pending_mail"], 1, "{listed}");
    assert_eq!(removed[0]["unread_homes"], 0, "{listed}");
    assert!(
        !bare.exists(),
        "machine.list must not create a home database"
    );

    // A home that cannot be read hides its mail instead of counting zero:
    // the count is unknown, and the unread homes are named by count.
    let sealed = alpha.folder("delta").join(".privateer/privateer.sqlite");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o0)).unwrap();
    let listed = alpha.ok("alpha", "machine.list", json!({}));
    let removed = &listed["removed"][0];
    assert!(removed["pending_mail"].is_null(), "{listed}");
    assert_eq!(removed["unread_homes"], 1, "{listed}");
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn forget_removed_resumes_absence_refusal_after_three_complete_exchanges() {
    let (_temp, alpha, beta, beta_ssh) = pair();
    let gamma = Machine {
        root: beta.root.parent().unwrap().join("gamma-machine"),
    };
    beta.ship("beta");
    gamma.ship("gamma");
    let gamma_ssh = remote_ssh(&gamma);
    connect(&alpha, "alpha", "beta", &beta, &beta_ssh);
    connect(&alpha, "alpha", "gamma", &gamma, &gamma_ssh);
    queue(&alpha);
    alpha.ok("alpha", "machine.remove", json!({"name":"beta"}));
    // Protection holds across processes and complete exchanges on the
    // remaining link, exactly as a never-removed machine's listing would.
    for _ in 0..5 {
        refresh(&alpha, "alpha", "gamma", &gamma_ssh);
        assert_pending(&alpha);
    }
    // Agents cannot drop a machine's protection.
    let agent = alpha.run(
        "alpha",
        &[
            "machine.forget_removed",
            "--json",
            "{\"name\":\"beta\"}",
            "--role",
            "agent",
            "--actor",
            "alpha",
        ],
        None,
    );
    assert!(!agent.status.success(), "{}", text(&agent));
    assert!(
        text(&agent).contains("operator command"),
        "{}",
        text(&agent)
    );
    alpha.ok("alpha", "machine.forget_removed", json!({"name":"beta"}));
    // Ordinary three-exchange refusal resumes on the remaining link.
    for _ in 0..2 {
        refresh(&alpha, "alpha", "gamma", &gamma_ssh);
        assert_pending(&alpha);
    }
    refresh(&alpha, "alpha", "gamma", &gamma_ssh);
    let refused = alpha.ok("alpha", "mail.show", json!({"id":"survey"}));
    assert_eq!(refused["delivery"], "undeliverable", "{refused}");
    assert!(
        refused["delivery_error"]
            .as_str()
            .unwrap()
            .contains("recipient_unlisted"),
        "{refused}"
    );
}

#[test]
fn a_temporarily_missing_folder_never_counts_as_a_complete_directory() {
    let (_temp, alpha, beta, ssh) = pair();
    beta.ship("beta");
    connect(&alpha, "alpha", "beta", &beta, &ssh);
    queue(&alpha);
    let hidden = beta.root.join("beta-unavailable");
    fs::rename(beta.folder("beta"), &hidden).unwrap();
    let status = beta.ok("beta-home", "machine.list", json!({}));
    assert_eq!(status["directory_incomplete"], json!(["beta"]), "{status}");
    for _ in 0..5 {
        refresh(&alpha, "alpha", "beta", &ssh);
        assert_pending(&alpha);
    }
    fs::rename(hidden, beta.folder("beta")).unwrap();
    let status = beta.ok("beta-home", "machine.list", json!({}));
    assert_eq!(status["directory_incomplete"], json!([]), "{status}");
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
