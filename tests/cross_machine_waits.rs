//! Cross-ship waits over a real machine link. Two machines, each with its own
//! registry and ship homes in separate directories, linked one way through a
//! fake `ssh` that runs the other machine's real gateway subprocess, as the
//! tests in `machines.rs` do. The laptop reaches the Mac: what the laptop
//! sends is delivered at once through the Mac's gateway, and what the Mac
//! sends waits there until the laptop collects it. A wait's subscription and
//! the owner's answer each cross that link, one way or the other.
mod support;

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

struct Machine {
    root: PathBuf,
}

impl Machine {
    fn registry(&self) -> PathBuf {
        self.root.join("fleet.sqlite")
    }

    fn folder(&self, ship: &str) -> PathBuf {
        self.root.join(ship)
    }

    fn command(&self, ship: &str, args: &[&str], ssh: Option<&Path>) -> std::process::Command {
        let mut command = support::command(support::BIN);
        command
            .args(args)
            .current_dir(self.folder(ship))
            .env("HOME", &self.root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("PRIVATEER_FLEET", self.registry())
            .env("PRIVATEER_RESOURCE_HOME", self.root.join("ledger"))
            .env("PRIVATEER_SERVICE", "none")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH");
        if let Some(ssh) = ssh {
            command.env("PRIVATEER_SSH_EXECUTABLE", ssh);
        }
        command
    }

    fn run(&self, ship: &str, args: &[&str], ssh: Option<&Path>) -> Output {
        self.command(ship, args, ssh).output().unwrap()
    }

    fn json(&self, ship: &str, command: &str, args: Value, ssh: Option<&Path>) -> Output {
        self.run(ship, &[command, "--json", &args.to_string()], ssh)
    }

    /// A command in a ship's folder; `ssh` reaches the other machine, for a
    /// command that may deliver there.
    fn ok_over(&self, ship: &str, command: &str, args: Value, ssh: Option<&Path>) -> Value {
        let output = self.json(ship, command, args, ssh);
        assert!(output.status.success(), "{command}: {}", text(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn ok(&self, ship: &str, command: &str, args: Value) -> Value {
        self.ok_over(ship, command, args, None)
    }

    /// One request to this machine's gateway, as a linking machine sends it.
    fn gateway(&self, command: &str, args: Value) -> Value {
        use std::io::{BufRead, Write};
        let mut child = self
            .command("fitforge", &["gateway"], None)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let request = json!({"id": "test", "command": command, "args": args});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(child.wait().unwrap().success());
        serde_json::from_str(&line).unwrap()
    }

    fn ship(&self, name: &str) {
        fs::create_dir_all(self.folder(name)).unwrap();
        let made = self.run(name, &["init", "--ship", name, "--harness", "codex"], None);
        assert!(made.status.success(), "{}", text(&made));
        // The free-disk floor is admission's concern, tested apart: this
        // fixture starts real jobs on whatever volume and space the shared
        // test machine has, so its own private ledger and charter must not
        // refuse them.
        let made = self.run(
            name,
            &[
                "resource.configure",
                "--json",
                "{\"min_free_disk_bytes\":0}",
            ],
            None,
        );
        assert!(made.status.success(), "{}", text(&made));
        let made = self.run(
            name,
            &[
                "charter.put",
                "--json",
                &format!("{{\"id\":\"{name}\",\"min_free_bytes\":0}}"),
            ],
            None,
        );
        assert!(made.status.success(), "{}", text(&made));
    }

    /// One dispatcher pass in a ship's home.
    fn pass(&self, ship: &str, ssh: &Path) -> Value {
        let output = self.run(ship, &["serve", "--json", r#"{"once":true}"#], Some(ssh));
        assert!(output.status.success(), "{}", text(&output));
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .next_back()
            .unwrap_or(Value::Null)
    }

    /// A ship's database, read and changed directly as a test fixture.
    fn database(&self, ship: &str) -> rusqlite::Connection {
        rusqlite::Connection::open(self.folder(ship).join(".privateer/privateer.sqlite")).unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The laptop with `liaison`, the Mac with `fitforge`, each with the extra
/// ships named, linked from the laptop. The ssh keeps the Mac's own home,
/// ledger and PATH, as a real remote host does. Both machines have the
/// ships' harness installed, so a ship may move between them.
struct Fleet {
    _temp: Sweep,
    laptop: Machine,
    mac: Machine,
    ssh: PathBuf,
}

/// The shared temporary root both machines live in. Dropping it ends any
/// process a test left referring to that root — a dispatcher a move started
/// — before the root is deleted, so nothing outlives the test run orphaned
/// under the user's service manager. Only processes whose command line or
/// working directory refers to this fixture's unique root are signalled,
/// never by name.
struct Sweep {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Drop for Sweep {
    fn drop(&mut self) {
        support::end_processes_referring_to(&self.root);
    }
}

fn fleet(laptop_ships: &[&str], mac_ships: &[&str]) -> Fleet {
    let temp = support::tempdir();
    let root = fs::canonicalize(temp.path()).unwrap();
    let laptop = Machine {
        root: root.join("laptop"),
    };
    let mac = Machine {
        root: root.join("mac"),
    };
    for machine in [&laptop, &mac] {
        let bin = machine.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    for ship in ["liaison"].iter().chain(laptop_ships) {
        laptop.ship(ship);
    }
    for ship in ["fitforge"].iter().chain(mac_ships) {
        mac.ship(ship);
    }
    let ssh = mac.root.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nfor argument do remote=$argument; done\nenv HOME={root} PATH={root}/bin:/usr/bin:/bin PRIVATEER_FLEET={root}/fleet.sqlite PRIVATEER_RESOURCE_HOME={root}/ledger /bin/sh -c \"$remote\"\n",
            root = mac.root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let registry = mac.registry();
    let linked = laptop.run(
        "liaison",
        &[
            "machine",
            "add",
            "mac",
            "test-host",
            "--executable",
            support::BIN,
            "--remote-fleet",
            registry.to_str().unwrap(),
            "--as",
            "laptop",
        ],
        Some(&ssh),
    );
    assert!(linked.status.success(), "{}", text(&linked));
    Fleet {
        _temp: Sweep {
            _temp: temp,
            root: root.clone(),
        },
        laptop,
        mac,
        ssh,
    }
}

/// A fake ssh that cannot reach the Mac: the link is down.
fn down_ssh(fleet: &Fleet) -> PathBuf {
    let ssh = fleet.laptop.root.join("ssh-down");
    fs::write(
        &ssh,
        "#!/bin/sh\necho 'ssh: connect to host test-host: Network is unreachable' >&2\nexit 255\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    ssh
}

fn task(machine: &Machine, ship: &str, id: &str) {
    machine.ok(
        ship,
        "task.create",
        json!({"id": id, "charter_id": ship, "owner": ship, "title": "The dependency"}),
    );
}

fn share(machine: &Machine, ship: &str, id: &str, with: &[&str]) {
    machine.ok(
        ship,
        "task.update",
        json!({"id": id, "actor": ship, "shared_with": with}),
    );
}

fn finish(machine: &Machine, ship: &str, id: &str, status: &str) {
    machine.ok(
        ship,
        "task.update",
        json!({"id": id, "actor": ship, "status": status}),
    );
}

/// A scheduled task of `ship`, waiting on `reference`.
fn waiter(machine: &Machine, ship: &str, id: &str, reference: Value) {
    machine.ok(
        ship,
        "task.create",
        json!({"id": id, "charter_id": ship, "owner": ship, "title": "Waits on another ship"}),
    );
    machine.ok(
        ship,
        "task.enqueue",
        json!({"id": id, "brief": "Continue once the dependency is done."}),
    );
    let deferred = defer(machine, ship, id, reference);
    assert_eq!(deferred["state"], "waiting", "{deferred}");
}

fn defer(machine: &Machine, ship: &str, id: &str, reference: Value) -> Value {
    machine.ok(
        ship,
        "task.defer",
        json!({"id": id, "wait": {"kind": "remote", "reason": "needs the far work", "ref": reference}}),
    )
}

/// One task's schedule in its home.
fn work(machine: &Machine, ship: &str, id: &str) -> Value {
    machine.ok(ship, "work.status", json!({"ship": ship}))["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

/// A scheduler pass for `ship`, as its attended session polls while idle:
/// every live schedule is checked.
fn poll(machine: &Machine, ship: &str) {
    machine.ok(
        ship,
        "ship.attend",
        json!({"ship": ship, "holder": "host:wait-test", "ttl_secs": 90}),
    );
    machine.ok(
        ship,
        "work.poll",
        json!({"ship": ship, "holder": "host:wait-test", "idle": true,
            "attention": {"tokens": 1000, "window": 100000}}),
    );
}

fn flush(machine: &Machine, ship: &str, ssh: Option<&Path>) -> Value {
    machine.ok_over(ship, "mail.flush", json!({}), ssh)
}

/// The laptop collects what waits for it on the Mac, now.
fn collect(fleet: &Fleet) -> Value {
    fleet
        .laptop
        .ok_over("liaison", "machine.collect", json!({}), Some(&fleet.ssh))
}

/// The laptop's contact with the Mac is due now, not on its schedule.
fn due(fleet: &Fleet) {
    rusqlite::Connection::open(fleet.laptop.registry())
        .unwrap()
        .execute("UPDATE machines SET next_contact_ms=NULL", [])
        .unwrap();
}

fn rows(machine: &Machine, ship: &str, task: &str) -> i64 {
    machine
        .database(ship)
        .query_row(
            "SELECT COUNT(*) FROM task_subscriptions WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .unwrap()
}

/// The mail a ship's home holds for `to`, oldest first.
fn outbox(machine: &Machine, ship: &str, to: &str) -> Vec<Value> {
    let conn = machine.database(ship);
    let mut statement = conn
        .prepare(
            "SELECT id, sender, payload, delivery, delivery_refusals, delivery_attempts,
               delivery_error FROM mail WHERE recipient=?1 ORDER BY seq",
        )
        .unwrap();
    statement
        .query_map([to], |r| {
            Ok(
                json!({"id": r.get::<_, String>(0)?, "from": r.get::<_, String>(1)?,
                "payload": serde_json::from_str::<Value>(&r.get::<_, String>(2)?).unwrap(),
                "delivery": r.get::<_, Option<String>>(3)?,
                "refusals": r.get::<_, i64>(4)?, "attempts": r.get::<_, i64>(5)?,
                "error": r.get::<_, Option<String>>(6)?}),
            )
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// The wait notices of one kind a ship's home holds for `to`.
fn notices(machine: &Machine, ship: &str, to: &str, key: &str) -> Vec<Value> {
    outbox(machine, ship, to)
        .into_iter()
        .filter(|m| m["payload"].get(key).is_some())
        .collect()
}

fn nonce_of(machine: &Machine, ship: &str, task: &str) -> String {
    machine
        .database(ship)
        .query_row(
            "SELECT json_extract(record,'$.subscription.nonce') FROM task_schedule WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )
        .unwrap()
}

fn audited(machine: &Machine, ship: &str, kind: &str) -> Vec<Value> {
    machine.ok(ship, "audit.list", json!({}))["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == kind)
        .map(|event| event["payload"].clone())
        .collect()
}

/// Nothing a wait notice carries reaches a ship's inbox.
fn no_wait_mail(machine: &Machine, ship: &str) {
    let inbox = machine.ok(ship, "mail.inbox", json!({"address": ship}));
    assert!(
        inbox["messages"].as_array().unwrap().iter().all(|m| {
            [
                "task_end",
                "wait_closed",
                "wait_subscribe",
                "wait_unsubscribe",
            ]
            .iter()
            .all(|key| m["payload"].get(*key).is_none())
        }),
        "{inbox}"
    );
}

/// The laptop's `liaison` waits on `far-work` of `fitforge` on the Mac, and
/// its subscription has crossed the link to the Mac's gateway.
fn laptop_waits() -> Fleet {
    let fleet = fleet(&[], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    task(mac, "fitforge", "far-work");
    share(mac, "fitforge", "far-work", &["liaison"]);
    waiter(
        laptop,
        "liaison",
        "waiting-1",
        json!({"ship": "fitforge", "task": "far-work", "machine": "mac"}),
    );
    flush(laptop, "liaison", Some(&fleet.ssh));
    let sent = notices(laptop, "liaison", "fitforge", "wait_subscribe");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["delivery"], "delivered", "{sent:?}");
    assert_eq!(rows(mac, "fitforge", "far-work"), 1);
    fleet
}

/// The Mac's `fitforge` waits on `far-work` of `liaison` on the laptop; its
/// subscription waited on the Mac until the laptop collected it.
fn mac_waits(laptop_ships: &[&str]) -> Fleet {
    let fleet = fleet(laptop_ships, &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    task(laptop, "liaison", "far-work");
    share(laptop, "liaison", "far-work", &["fitforge"]);
    waiter(
        mac,
        "fitforge",
        "waiting-1",
        json!({"ship": "liaison", "task": "far-work", "machine": "laptop"}),
    );
    assert_eq!(
        notices(mac, "fitforge", "liaison", "wait_subscribe")[0]["delivery"],
        "pending"
    );
    collect(&fleet);
    assert_eq!(
        notices(mac, "fitforge", "liaison", "wait_subscribe")[0]["delivery"],
        "delivered"
    );
    assert_eq!(rows(laptop, "liaison", "far-work"), 1);
    fleet
}

#[test]
fn a_far_done_collected_over_the_link_resumes_the_wait() {
    let fleet = laptop_waits();
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let nonce = nonce_of(laptop, "liaison", "waiting-1");
    finish(mac, "fitforge", "far-work", "done");
    // The owner's report waits on the Mac for the laptop to collect it.
    let ends = notices(mac, "fitforge", "liaison", "task_end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["delivery"], "pending");
    assert_eq!(
        ends[0]["payload"]["task_end"],
        json!({"task": "far-work", "status": "done", "reason": "done", "nonce": nonce})
    );
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    assert_eq!(work(laptop, "liaison", "waiting-1")["state"], "waiting");
    let collected = collect(&fleet);
    assert_eq!(collected["collected"][0]["delivered"], 1, "{collected}");
    let resumed = work(laptop, "liaison", "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
    let reason = resumed["resumed_by"].as_str().unwrap();
    assert!(
        reason.contains("far-work")
            && reason.contains("fitforge")
            && reason.contains("machine mac")
            && reason.contains("done"),
        "{reason}"
    );
    assert_eq!(
        notices(mac, "fitforge", "liaison", "task_end")[0]["delivery"],
        "delivered"
    );
    // Applied, never mailed, and nothing is sent back.
    no_wait_mail(laptop, "liaison");
    assert!(notices(laptop, "liaison", "fitforge", "wait_unsubscribe").is_empty());
    let resolved = audited(laptop, "liaison", "wait.resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["via"], "mac");
}

#[test]
fn a_far_cancel_delivered_over_the_link_faults_the_wait() {
    let fleet = mac_waits(&[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    finish(laptop, "liaison", "far-work", "cancelled");
    // The laptop delivers its report at once through the Mac's gateway.
    flush(laptop, "liaison", Some(&fleet.ssh));
    let ends = notices(laptop, "liaison", "fitforge", "task_end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["delivery"], "delivered", "{ends:?}");
    let faulted = work(mac, "fitforge", "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    let reason = faulted["reason"].as_str().unwrap();
    assert!(
        reason.contains("cancelled") && reason.contains("machine laptop"),
        "{reason}"
    );
    // A later pass never resumes it as if the dependency succeeded.
    poll(mac, "fitforge");
    assert_eq!(work(mac, "fitforge", "waiting-1")["state"], "fault");
    let resolved = audited(mac, "fitforge", "wait.resolved");
    assert_eq!(resolved[0]["via"], "laptop", "{resolved:?}");
    no_wait_mail(mac, "fitforge");
}

#[test]
fn an_unshared_or_missing_far_task_is_refused_over_the_link_alike_and_the_waits_fault() {
    let fleet = fleet(&[], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    // Not shared with liaison, and a task that does not exist at all.
    task(mac, "fitforge", "far-work");
    share(mac, "fitforge", "far-work", &["someone-else"]);
    waiter(
        laptop,
        "liaison",
        "waiting-1",
        json!({"ship": "fitforge", "task": "far-work", "machine": "mac"}),
    );
    waiter(
        laptop,
        "liaison",
        "waiting-2",
        json!({"ship": "fitforge", "task": "no-such-work", "machine": "mac"}),
    );
    flush(laptop, "liaison", Some(&fleet.ssh));
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    assert_eq!(rows(mac, "fitforge", "no-such-work"), 0);
    // The same refusal for both, and it says nothing else about either.
    let closed: Vec<Value> = notices(mac, "fitforge", "liaison", "wait_closed")
        .into_iter()
        .map(|m| {
            let mut body = m["payload"]["wait_closed"].clone();
            assert!(body["nonce"].is_string(), "{body}");
            body.as_object_mut().unwrap().remove("nonce");
            body.as_object_mut().unwrap().remove("task");
            body
        })
        .collect();
    assert_eq!(
        closed,
        vec![json!({"reason": "refused"}), json!({"reason": "refused"})]
    );
    collect(&fleet);
    for id in ["waiting-1", "waiting-2"] {
        let faulted = work(laptop, "liaison", id);
        assert_eq!(faulted["state"], "fault", "{faulted}");
        assert!(
            faulted["reason"]
                .as_str()
                .unwrap()
                .contains("was closed: refused"),
            "{faulted}"
        );
    }
    // The owner holds no row, so nothing is sent back to it.
    assert!(notices(laptop, "liaison", "fitforge", "wait_unsubscribe").is_empty());
    assert_eq!(audited(mac, "fitforge", "wait.refused").len(), 2);
    no_wait_mail(laptop, "liaison");
}

#[test]
fn a_far_end_lost_during_an_outage_is_answered_on_the_resync_and_a_replay_is_harmless() {
    let fleet = laptop_waits();
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let nonce = nonce_of(laptop, "liaison", "waiting-1");
    // The link goes down: the laptop's contact with the Mac fails.
    due(&fleet);
    laptop.pass("liaison", &down_ssh(&fleet));
    let failures: i64 = rusqlite::Connection::open(laptop.registry())
        .unwrap()
        .query_row("SELECT failures FROM machines WHERE name='mac'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(failures > 0);
    // The far task ends while the link is down, and its report is lost (a
    // report refused three times is dropped, say).
    finish(mac, "fitforge", "far-work", "done");
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    mac.database("fitforge")
        .execute(
            "UPDATE mail SET delivery='dropped' WHERE json_extract(payload,'$.task_end') IS NOT NULL",
            [],
        )
        .unwrap();
    // The link comes back: the laptop's next contact succeeds after failing,
    // and the waiting home sends its live subscription again.
    due(&fleet);
    laptop.pass("liaison", &fleet.ssh);
    let resyncs = audited(laptop, "liaison", "wait.resync");
    assert_eq!(resyncs.len(), 1, "{resyncs:?}");
    assert_eq!(resyncs[0]["peer"], "machine-mac");
    assert_eq!(resyncs[0]["resubscribed"], 1);
    flush(laptop, "liaison", Some(&fleet.ssh));
    let subscribes = notices(laptop, "liaison", "fitforge", "wait_subscribe");
    assert_eq!(subscribes.len(), 2, "{subscribes:?}");
    assert!(
        subscribes.iter().all(|m| m["delivery"] == "delivered"
            && m["payload"]["wait_subscribe"]["nonce"] == json!(nonce))
    );
    // The owner answers the subscription for the task that ended with its
    // final status at once, and stores no row.
    let answered: Vec<Value> = notices(mac, "fitforge", "liaison", "task_end")
        .into_iter()
        .filter(|m| m["delivery"] == "pending")
        .collect();
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(
        answered[0]["payload"]["task_end"],
        json!({"task": "far-work", "status": "done", "reason": "done", "nonce": nonce})
    );
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    assert_eq!(work(laptop, "liaison", "waiting-1")["state"], "waiting");
    collect(&fleet);
    let resumed = work(laptop, "liaison", "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
    assert!(resumed["resumed_by"].as_str().unwrap().contains("done"));
    // The same report collected again finds no wait and changes nothing.
    mac.database("fitforge")
        .execute(
            "UPDATE mail SET delivery='pending' WHERE id=?1",
            [answered[0]["id"].as_str().unwrap()],
        )
        .unwrap();
    let again = collect(&fleet);
    assert_eq!(again["collected"][0]["delivered"], 1, "{again}");
    let dropped = audited(laptop, "liaison", "wait.dropped");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["why"], "no_wait");
    assert_eq!(work(laptop, "liaison", "waiting-1")["state"], "ready");
    assert_eq!(audited(laptop, "liaison", "wait.resolved").len(), 1);
    no_wait_mail(laptop, "liaison");
}

#[test]
fn forged_reports_through_the_gateway_are_dropped_and_the_real_one_resumes() {
    // `decoy` is a real ship on the laptop, which the Mac knows behind the
    // same link as the owner.
    let fleet = mac_waits(&["decoy"]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let nonce = nonce_of(mac, "fitforge", "waiting-1");
    let decoy = mac.ok("fitforge", "ship.show", json!({"name": "decoy"}));
    assert_eq!(decoy["location"], json!({"peer": "machine-laptop"}));
    let deliver = |id: &str, from: &str, status: &str, via: Option<&str>| -> Value {
        let mut envelope = json!({"id": id, "from": from, "to": "fitforge", "kind": "notice",
            "body": "", "payload": {"task_end": {"task": "far-work", "status": status,
                "reason": status, "nonce": nonce}}});
        if let Some(via) = via {
            envelope["via"] = json!(via);
        }
        let answer = mac.gateway("mail.deliver", envelope);
        assert_eq!(answer["ok"], true, "{answer}");
        answer["result"]["wait"].clone()
    };
    // Another ship on the linked machine is not the ship the ref names.
    let other = deliver("forged-1", "decoy", "done", Some("laptop"));
    assert_eq!(other["why"], "no_wait", "{other}");
    // The right ship, carried by a machine the ref does not name: the gateway
    // keeps only a via naming a machine linked to it, so an unknown one, like
    // none at all, is not the ref's machine.
    for (id, via) in [("forged-2", Some("elsewhere")), ("forged-3", None)] {
        let carried = deliver(id, "liaison", "done", via);
        assert_eq!(carried["why"], "via", "{id}: {carried}");
    }
    // The right ship and machine, with a status that is not final.
    let open = deliver("forged-4", "liaison", "open", Some("laptop"));
    assert_eq!(open["why"], "status", "{open}");
    assert_eq!(work(mac, "fitforge", "waiting-1")["state"], "waiting");
    let whys: Vec<Value> = audited(mac, "fitforge", "wait.dropped")
        .iter()
        .map(|d| d["why"].clone())
        .collect();
    assert_eq!(whys.len(), 4, "{whys:?}");
    for why in ["no_wait", "via", "status"] {
        assert!(whys.contains(&json!(why)), "{whys:?}");
    }
    no_wait_mail(mac, "fitforge");
    // The owner's own report, by its own link, resumes the wait.
    finish(laptop, "liaison", "far-work", "done");
    flush(laptop, "liaison", Some(&fleet.ssh));
    let resumed = work(mac, "fitforge", "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
}

/// Retire a ship on the laptop through the real CLI, as the captain, with
/// the link to carry its last notices.
fn retire(fleet: &Fleet, ship: &str) -> Value {
    fleet
        .laptop
        .ok_over(ship, "ship.retire", json!({"name": ship}), Some(&fleet.ssh))
}

#[test]
fn retiring_the_owner_closes_the_wait_across_the_link_as_retired() {
    let fleet = mac_waits(&[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let nonce = nonce_of(mac, "fitforge", "waiting-1");
    let retired = retire(&fleet, "liaison");
    let waits = &retired["waits"];
    assert_eq!(waits["closed"], 1, "{retired}");
    // The retire delivered its notice through the gateway before it stopped.
    assert_eq!(
        waits["drain"],
        json!({"delivered": 1, "pending": 0}),
        "{retired}"
    );
    assert_eq!(rows(laptop, "liaison", "far-work"), 0);
    let closed = notices(laptop, "liaison", "fitforge", "wait_closed");
    assert_eq!(
        closed[0]["payload"]["wait_closed"],
        json!({"task": "far-work", "reason": "retired", "nonce": nonce})
    );
    let faulted = work(mac, "fitforge", "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    assert!(
        faulted["reason"]
            .as_str()
            .unwrap()
            .contains("was closed: retired"),
        "{faulted}"
    );
    poll(mac, "fitforge");
    assert_eq!(work(mac, "fitforge", "waiting-1")["state"], "fault");
    assert!(notices(mac, "fitforge", "liaison", "wait_unsubscribe").is_empty());
}

#[test]
fn retiring_the_waiting_ship_unsubscribes_across_the_link_and_the_owner_row_goes() {
    let fleet = laptop_waits();
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let nonce = nonce_of(laptop, "liaison", "waiting-1");
    let retired = retire(&fleet, "liaison");
    let waits = &retired["waits"];
    assert_eq!(waits["unsubscribed"], 1, "{retired}");
    assert_eq!(waits["closed"], 0, "{retired}");
    assert_eq!(
        waits["drain"],
        json!({"delivered": 1, "pending": 0}),
        "{retired}"
    );
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    let unsubscribes = notices(laptop, "liaison", "fitforge", "wait_unsubscribe");
    assert_eq!(unsubscribes.len(), 1, "{unsubscribes:?}");
    assert_eq!(
        unsubscribes[0]["payload"]["wait_unsubscribe"]["nonce"],
        json!(nonce)
    );
    assert_eq!(audited(mac, "fitforge", "wait.unsubscribe").len(), 1);
    // Its waiting task is not cancelled; its schedule is left as it was.
    assert_eq!(work(laptop, "liaison", "waiting-1")["state"], "waiting");
}

#[test]
fn a_wait_notice_the_gateway_refuses_is_dropped_on_its_third_refusal_and_later_mail_flows() {
    // A ship whose name holds "busy": its refusal is still a refusal.
    let fleet = fleet(&[], &["busy-worker"]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    // Retired on the Mac, though the laptop's directory still names it, so
    // the Mac's gateway refuses anything addressed to it, every time.
    rusqlite::Connection::open(mac.registry())
        .unwrap()
        .execute("UPDATE ships SET retired_ms=1 WHERE name='busy-worker'", [])
        .unwrap();
    waiter(
        laptop,
        "liaison",
        "waiting-1",
        json!({"ship": "busy-worker", "task": "dock-work", "machine": "mac"}),
    );
    // Mail behind the refused subscription waits while it is retried.
    let ping = laptop.ok_over(
        "liaison",
        "mail.send",
        json!({"from": "liaison", "to": "fitforge", "kind": "notice", "body": "ping"}),
        Some(&fleet.ssh),
    );
    let subscribe = || notices(laptop, "liaison", "busy-worker", "wait_subscribe")[0].clone();
    assert_eq!(subscribe()["refusals"], 1, "{}", subscribe());
    assert_eq!(subscribe()["delivery"], "pending");
    assert!(
        subscribe()["error"]
            .as_str()
            .unwrap()
            .contains("no ship named busy-worker"),
        "{}",
        subscribe()
    );
    assert_eq!(
        outbox(laptop, "liaison", "fitforge")[0]["delivery"],
        "pending"
    );
    flush(laptop, "liaison", Some(&fleet.ssh));
    assert_eq!(subscribe()["refusals"], 2);
    assert!(audited(laptop, "liaison", "wait.notice_dropped").is_empty());
    // The third refusal drops it, and the mail behind it goes in that pass.
    let flushed = flush(laptop, "liaison", Some(&fleet.ssh));
    assert_eq!(subscribe()["delivery"], "dropped", "{}", subscribe());
    assert_eq!(subscribe()["refusals"], 3);
    assert_eq!(
        outbox(laptop, "liaison", "fitforge")[0]["delivery"],
        "delivered"
    );
    let arrived = mac.ok("fitforge", "mail.inbox", json!({"address": "fitforge"}));
    assert!(
        arrived["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == ping["id"]),
        "{arrived}"
    );
    let dropped = audited(laptop, "liaison", "wait.notice_dropped");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["notice"], "wait_subscribe");
    assert_eq!(dropped[0]["refusals"], 3);
    // Dropped, not quarantined: nothing for the captain to chase, and it is
    // never retried again.
    assert_eq!(flushed["quarantined"], 0, "{flushed}");
    flush(laptop, "liaison", Some(&fleet.ssh));
    assert_eq!(audited(laptop, "liaison", "wait.notice_dropped").len(), 1);
    let later = laptop.ok_over(
        "liaison",
        "mail.send",
        json!({"from": "liaison", "to": "fitforge", "kind": "notice", "body": "later"}),
        Some(&fleet.ssh),
    );
    assert_eq!(later["delivery"], "delivered", "{later}");
}

#[test]
fn a_wait_notice_a_locked_home_behind_the_gateway_refuses_is_retried_and_never_dropped() {
    let fleet = fleet(&[], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    task(mac, "fitforge", "far-work");
    share(mac, "fitforge", "far-work", &["liaison"]);
    // The owner's home answers every subscription with the error a write
    // past its busy timeout gives, through its gateway.
    mac.database("fitforge")
        .execute_batch(
            "CREATE TRIGGER locked BEFORE INSERT ON task_subscriptions
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();
    waiter(
        laptop,
        "liaison",
        "waiting-1",
        json!({"ship": "fitforge", "task": "far-work", "machine": "mac"}),
    );
    for _ in 0..4 {
        flush(laptop, "liaison", Some(&fleet.ssh));
    }
    let subscribe = notices(laptop, "liaison", "fitforge", "wait_subscribe")[0].clone();
    assert_eq!(subscribe["delivery"], "pending", "{subscribe}");
    assert_eq!(subscribe["refusals"], 0, "{subscribe}");
    assert_eq!(subscribe["attempts"], 4, "{subscribe}");
    assert!(
        subscribe["error"]
            .as_str()
            .unwrap()
            .contains("database is locked"),
        "{subscribe}"
    );
    assert!(audited(laptop, "liaison", "wait.notice_dropped").is_empty());
    // Once the owner's home can write again, the subscription arrives.
    mac.database("fitforge")
        .execute_batch("DROP TRIGGER locked;")
        .unwrap();
    flush(laptop, "liaison", Some(&fleet.ssh));
    assert_eq!(
        notices(laptop, "liaison", "fitforge", "wait_subscribe")[0]["delivery"],
        "delivered"
    );
    assert_eq!(rows(mac, "fitforge", "far-work"), 1);
}

#[test]
fn collected_mail_a_locked_home_refuses_stays_pending_and_is_delivered_later() {
    let fleet = fleet(&[], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    let sent = mac.ok(
        "fitforge",
        "mail.send",
        json!({"from": "fitforge", "to": "liaison", "kind": "notice", "body": "Hull patched"}),
    );
    assert_eq!(sent["delivery"], "pending", "{sent}");
    // The laptop's home for liaison cannot write just then.
    laptop
        .database("liaison")
        .execute_batch(
            "CREATE TRIGGER locked BEFORE INSERT ON mail
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();
    let refused = collect(&fleet);
    assert_eq!(refused["collected"][0]["refused"], 1, "{refused}");
    // Not quarantined: it waits on the Mac to be collected again.
    let held = &outbox(mac, "fitforge", "liaison")[0];
    assert_eq!(held["delivery"], "pending", "{held}");
    assert_eq!(held["attempts"], 1, "{held}");
    assert!(
        held["error"]
            .as_str()
            .unwrap()
            .contains("database is locked"),
        "{held}"
    );
    let status = mac.ok("fitforge", "status", json!({}));
    assert_eq!(status["quarantined_mail"], 0, "{status}");
    assert!(audited(mac, "fitforge", "mail.quarantined").is_empty());
    laptop
        .database("liaison")
        .execute_batch("DROP TRIGGER locked;")
        .unwrap();
    let delivered = collect(&fleet);
    assert_eq!(delivered["collected"][0]["delivered"], 1, "{delivered}");
    assert_eq!(
        outbox(mac, "fitforge", "liaison")[0]["delivery"],
        "delivered"
    );
    let inbox = laptop.ok("liaison", "mail.inbox", json!({"address": "liaison"}));
    assert!(
        inbox["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "Hull patched"),
        "{inbox}"
    );
}

/// Move a laptop ship to the Mac, as the captain does from the laptop, and
/// what it said.
fn move_to_mac(fleet: &Fleet, ship: &str) -> String {
    let moved = fleet.laptop.run(
        "liaison",
        &["ship", "move", ship, "--to", "mac"],
        Some(&fleet.ssh),
    );
    assert!(moved.status.success(), "{}", text(&moved));
    text(&moved)
}

#[test]
fn when_the_owner_moves_the_wait_faults_and_waits_again_by_the_new_route() {
    let fleet = fleet(&["scout"], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    task(laptop, "scout", "far-work");
    share(laptop, "scout", "far-work", &["fitforge"]);
    waiter(
        mac,
        "fitforge",
        "waiting-1",
        json!({"ship": "scout", "task": "far-work", "machine": "laptop"}),
    );
    collect(&fleet);
    assert_eq!(rows(laptop, "scout", "far-work"), 1);
    move_to_mac(&fleet, "scout");
    // The owner's task and its subscription moved with it; the Mac's ships
    // now reach it on this machine.
    assert_eq!(rows(mac, "scout", "far-work"), 1);
    let scout = mac.ok("fitforge", "ship.show", json!({"name": "scout"}));
    assert_eq!(scout["location"], json!({"peer": "fleet-scout"}), "{scout}");
    // The ref names the laptop, so a report from here would be dropped as
    // carried by the wrong route: the wait faults, saying how to wait again.
    poll(mac, "fitforge");
    let faulted = work(mac, "fitforge", "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    let reason = faulted["reason"].as_str().unwrap();
    assert!(
        reason.contains("no longer reaches it")
            && reason.contains("scout is now on this machine; re-wait without ref.machine"),
        "{reason}"
    );
    // Faulting withdrew the old subscription by the new route.
    flush(mac, "fitforge", None);
    assert_eq!(rows(mac, "scout", "far-work"), 0);
    // Its first mate waits again as this machine reaches the owner, and the
    // owner's report resumes it.
    let rewaited = defer(
        mac,
        "fitforge",
        "waiting-1",
        json!({"ship": "scout", "task": "far-work"}),
    );
    assert_eq!(rewaited["state"], "waiting", "{rewaited}");
    flush(mac, "fitforge", None);
    assert_eq!(rows(mac, "scout", "far-work"), 1);
    finish(mac, "scout", "far-work", "done");
    flush(mac, "scout", None);
    let resumed = work(mac, "fitforge", "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
}

#[test]
fn when_the_waiting_ship_moves_its_wait_faults_on_arrival_and_waits_again_from_there() {
    let fleet = fleet(&["scout"], &[]);
    let (laptop, mac) = (&fleet.laptop, &fleet.mac);
    task(mac, "fitforge", "far-work");
    share(mac, "fitforge", "far-work", &["scout"]);
    waiter(
        laptop,
        "scout",
        "waiting-1",
        json!({"ship": "fitforge", "task": "far-work", "machine": "mac"}),
    );
    flush(laptop, "scout", Some(&fleet.ssh));
    assert_eq!(rows(mac, "fitforge", "far-work"), 1);
    let moved = move_to_mac(&fleet, "scout");
    assert!(
        moved.contains("1 remote waits faulted: their refs named the route from"),
        "{moved}"
    );
    // On arrival its ref names the machine it is now on: it faults at once.
    let faulted = work(mac, "scout", "waiting-1");
    assert_eq!(faulted["state"], "fault", "{faulted}");
    let reason = faulted["reason"].as_str().unwrap();
    assert!(
        reason.contains("scout moved here from")
            && reason.contains("fitforge is now on this machine; re-wait without ref.machine"),
        "{reason}"
    );
    assert_eq!(audited(mac, "scout", "wait.arrived")[0]["faulted"], 1);
    // The old subscription is withdrawn by the route the owner has now.
    flush(mac, "scout", None);
    assert_eq!(rows(mac, "fitforge", "far-work"), 0);
    let rewaited = defer(
        mac,
        "scout",
        "waiting-1",
        json!({"ship": "fitforge", "task": "far-work"}),
    );
    assert_eq!(rewaited["state"], "waiting", "{rewaited}");
    flush(mac, "scout", None);
    assert_eq!(rows(mac, "fitforge", "far-work"), 1);
    finish(mac, "fitforge", "far-work", "done");
    flush(mac, "fitforge", None);
    let resumed = work(mac, "scout", "waiting-1");
    assert_eq!(resumed["state"], "ready", "{resumed}");
}
