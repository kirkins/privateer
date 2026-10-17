mod support;

use privateer::{dispatch, store::Store};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_privateer");

/// Never leave a watch running when an assertion fails.
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_watch_pane_lists_the_first_mate_in_herdr_and_mails_what_the_captain_types() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let mut store = Store::open(&home).unwrap();
    dispatch(&mut store, "ship.put", json!({"name":"mate"})).unwrap();
    drop(store);

    // Herdr answers one request per connection.
    let socket = temp.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let _ = writeln!(
                stream,
                "{}",
                json!({"id":request["id"],"result":{"type":"ok"}})
            );
            seen.lock().unwrap().push(request);
        }
    });
    let methods = || -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["method"].as_str().unwrap_or("").to_owned())
            .collect()
    };

    let mut watch = Child(
        support::command(BIN)
            .args(["--home", home.to_str().unwrap(), "watch", "mate"])
            .env("HERDR_ENV", "1")
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PANE_ID", "w1:p1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("the agent report", || {
        methods().iter().any(|m| m == "pane.report_metadata")
    });
    let report = requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "pane.report_agent")
        .cloned()
        .unwrap();
    assert_eq!(report["params"]["pane_id"], "w1:p1");
    assert_eq!(report["params"]["state"], "idle");
    let metadata = requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "pane.report_metadata")
        .cloned()
        .unwrap();
    assert_eq!(metadata["params"]["display_agent"], "mate");

    writeln!(watch.0.stdin.as_mut().unwrap(), "please check CI").unwrap();
    until("the captain's mail", || {
        let store = Store::open(&home).unwrap();
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE sender='captain' AND recipient='mate' AND body='please check CI'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    });

    unsafe { libc::kill(watch.0.id() as i32, libc::SIGINT) };
    let status = watch.0.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(methods().iter().any(|m| m == "pane.release_agent"));
}

#[test]
fn a_crew_view_lists_the_crew_in_herdr_and_sends_what_the_captain_types_to_its_first_mate() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let mut store = Store::open(&home).unwrap();
    dispatch(&mut store, "ship.put", json!({"name":"mate"})).unwrap();
    dispatch(
        &mut store,
        "charter.put",
        json!({"id":"c","repo":repo,"objective":"fixture"}),
    )
    .unwrap();
    dispatch(
        &mut store,
        "task.create",
        json!({"id":"mate-fix","owner":"mate","charter_id":"c","title":"fix"}),
    )
    .unwrap();
    // A finished headless Claude crew and its log.
    let job = json!({"id":"crew-9","task_id":"mate-fix","agent_actor":"mate","status":"succeeded",
        "incarnation":"i1","model_route":"claude/-/claude-haiku-5-5"});
    store
        .connection()
        .execute(
            "INSERT INTO jobs(id,record) VALUES('crew-9',?1)",
            [job.to_string()],
        )
        .unwrap();
    let log = home.join("jobs/crew-9/i1");
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        log.join("stdout.log"),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"fixed it"}]}})
            .to_string()
            + "\n",
    )
    .unwrap();
    drop(store);

    let socket = temp.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let _ = writeln!(
                stream,
                "{}",
                json!({"id":request["id"],"result":{"type":"ok"}})
            );
            seen.lock().unwrap().push(request);
        }
    });
    let find = |method: &str| {
        requests
            .lock()
            .unwrap()
            .iter()
            .find(|r| r["method"] == method)
            .cloned()
    };
    let mut watch = Child(
        support::command(BIN)
            .args(["--home", home.to_str().unwrap(), "watch", "--job", "crew-9"])
            .env("HERDR_ENV", "1")
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PANE_ID", "w4:p1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("the crew report", || find("pane.report_metadata").is_some());
    let report = find("pane.report_agent").unwrap();
    assert_eq!(report["params"]["agent"], "claude");
    assert_eq!(report["params"]["state"], "done");
    let metadata = find("pane.report_metadata").unwrap();
    assert_eq!(metadata["params"]["display_agent"], "mate-fix");
    assert_eq!(metadata["params"]["title"], "mate crew on mate-fix");

    // A headless crew takes no input: the line goes to its first mate.
    writeln!(watch.0.stdin.as_mut().unwrap(), "please rebase first").unwrap();
    until("the captain's note to the first mate", || {
        let store = Store::open(&home).unwrap();
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE sender='captain' AND recipient='mate'
                   AND task_id='mate-fix' AND body LIKE 'About crew job crew-9 on mate-fix: please rebase first'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    });
    unsafe { libc::kill(watch.0.id() as i32, libc::SIGINT) };
    assert!(watch.0.wait().unwrap().success());
}
