use super::*;
use crate::herdr::Client;
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

// Like the dashboard fake: one correlated reply per real Unix connection.
struct Herdr {
    spaces: Arc<Mutex<Value>>,
    sent: Arc<Mutex<Vec<Value>>>,
    fail: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Herdr {
    fn start(path: &Path) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let spaces = Arc::new(Mutex::new(json!([
            {"workspace_id":"w1","tokens":{}},
            {"workspace_id":"w2","tokens":{}}
        ])));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (state, calls, refused, done) =
            (spaces.clone(), sent.clone(), fail.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let params = &request["params"];
                let reply = match request["method"].as_str().unwrap() {
                    "workspace.list" => json!({"result":{"workspaces":*state.lock().unwrap()}}),
                    "workspace.report_metadata" => {
                        calls.lock().unwrap().push(params.clone());
                        if refused.load(Ordering::SeqCst) {
                            json!({"error":"fixture refusal"})
                        } else {
                            for space in state.lock().unwrap().as_array_mut().unwrap() {
                                if space["workspace_id"] == params["workspace_id"] {
                                    space["tokens"] = params["tokens"].clone();
                                }
                            }
                            json!({"result":{"type":"ok"}})
                        }
                    }
                    other => panic!("unexpected method {other}"),
                };
                let mut reply = reply;
                reply["id"] = request["id"].clone();
                writeln!(reader.get_mut(), "{reply}").unwrap();
            }
        });
        Self {
            spaces,
            sent,
            fail,
            stop,
            thread: Some(thread),
        }
    }
    fn tokens(&self, workspace: usize, tokens: Value) {
        self.spaces.lock().unwrap()[workspace]["tokens"] = tokens;
    }
    fn reports(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
    }
}
impl Drop for Herdr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn tokens(ship: &str) -> Value {
    json!({"privateer":"bridge","ship":ship})
}

#[test]
fn unchanged_ticks_and_recreated_clients_send_no_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let herdr = Herdr::start(&path);
    let now = Instant::now();
    Client::connect(&path)
        .unwrap()
        .tag_tokens_at("w1", tokens("ship-a"), now)
        .unwrap();
    // Ignore other sources' volatile tokens and JSON object ordering.
    herdr.tokens(
        0,
        json!({"ship":"ship-a","privateer":"bridge","updated_ms":123}),
    );
    for tick in 1..100 {
        Client::connect(&path)
            .unwrap()
            .tag_tokens_at("w1", tokens("ship-a"), now + Duration::from_secs(tick))
            .unwrap();
    }
    assert_eq!(herdr.reports().len(), 1);
}

#[test]
fn a_real_change_sends_once_and_a_burst_sends_only_its_latest_value_when_due() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let herdr = Herdr::start(&path);
    let mut client = Client::connect(&path).unwrap();
    let now = Instant::now();
    client.tag_tokens_at("w1", tokens("initial"), now).unwrap();
    client
        .tag_tokens_at("w1", tokens("changed"), now + MIN_INTERVAL)
        .unwrap();
    client
        .tag_tokens_at("w1", tokens("changed"), now + MIN_INTERVAL)
        .unwrap();
    assert_eq!(herdr.reports().len(), 2);
    for (ms, ship) in [(2100, "burst-a"), (2300, "burst-b"), (3999, "latest")] {
        client
            .tag_tokens_at("w1", tokens(ship), now + Duration::from_millis(ms))
            .unwrap();
    }
    assert_eq!(herdr.reports().len(), 2);
    client
        .tag_tokens_at("w1", tokens("latest"), now + MIN_INTERVAL * 2)
        .unwrap();
    client
        .tag_tokens_at("w1", tokens("latest"), now + MIN_INTERVAL * 3)
        .unwrap();
    let sent = herdr.reports();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2]["tokens"], tokens("latest"));
    let key = (path, client.identity(), "w1".to_owned());
    assert_eq!(
        reports().lock().unwrap().0[&key].sent,
        Some(tokens("latest"))
    );
}

#[test]
fn a_reconnect_resends_lost_metadata_without_waiting_for_the_old_socket_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let now = Instant::now();
    let herdr = Herdr::start(&path);
    Client::connect(&path)
        .unwrap()
        .tag_tokens_at("w1", tokens("ship-a"), now)
        .unwrap();
    // Keep the old inode allocated, as in the dashboard socket replacement test.
    std::fs::rename(&path, dir.path().join("old.sock")).unwrap();
    let replacement = Herdr::start(&path);
    Client::connect(&path)
        .unwrap()
        .tag_tokens_at("w1", tokens("ship-a"), now + Duration::from_millis(1))
        .unwrap();
    assert_eq!(herdr.reports().len(), 1);
    assert_eq!(replacement.reports().len(), 1);
}

#[test]
fn lost_tokens_and_new_workspaces_are_repaired_but_absent_workspaces_are_not_tagged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let herdr = Herdr::start(&path);
    let mut client = Client::connect(&path).unwrap();
    let now = Instant::now();
    client.tag_tokens_at("w1", tokens("ship-a"), now).unwrap();
    herdr.tokens(0, json!({}));
    client
        .tag_tokens_at("w1", tokens("ship-a"), now + MIN_INTERVAL)
        .unwrap();
    // Per-workspace limit: w2 does not wait for w1's interval.
    client
        .tag_tokens_at("w2", tokens("ship-b"), now + MIN_INTERVAL)
        .unwrap();
    client
        .tag_tokens_at("missing", tokens("ship-c"), now + MIN_INTERVAL)
        .unwrap();
    assert_eq!(herdr.reports().len(), 3);
}

#[test]
fn failures_are_not_successes_and_retries_are_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.sock");
    let herdr = Herdr::start(&path);
    let mut client = Client::connect(&path).unwrap();
    let now = Instant::now();
    herdr.fail.store(true, Ordering::SeqCst);
    assert!(client.tag_tokens_at("w1", tokens("ship-a"), now).is_err());
    herdr.fail.store(false, Ordering::SeqCst);
    client
        .tag_tokens_at("w1", tokens("ship-a"), now + Duration::from_millis(1))
        .unwrap();
    assert_eq!(herdr.reports().len(), 1);
    client
        .tag_tokens_at("w1", tokens("ship-a"), now + MIN_INTERVAL)
        .unwrap();
    assert_eq!(herdr.reports().len(), 2);
    assert_eq!(herdr.spaces.lock().unwrap()[0]["tokens"], tokens("ship-a"));
}

#[test]
fn reservations_and_cache_capacity_do_not_bypass_the_interval() {
    let mut reports = Reports::default();
    let now = Instant::now();
    let key = |id: usize| {
        (
            PathBuf::from("/isolated/herdr.sock"),
            (1, 2),
            id.to_string(),
        )
    };
    for id in 0..MAX_WORKSPACES {
        assert!(reports.reserve(key(id), &tokens("ship-a"), &json!({}), now));
    }
    assert!(!reports.reserve(key(0), &tokens("ship-b"), &json!({}), now));
    assert!(!reports.reserve(key(MAX_WORKSPACES), &tokens("ship-a"), &json!({}), now));
    assert!(reports.reserve(
        key(MAX_WORKSPACES),
        &tokens("ship-a"),
        &json!({}),
        now + MIN_INTERVAL
    ));
    assert_eq!(reports.0.len(), MAX_WORKSPACES);
}
