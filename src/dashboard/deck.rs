//! The bridge's fleet-wide deck: every machine's ships, crews, needs, routes
//! and recent activity, read from the runtime's own fleet commands on a slow
//! cadence, away from the input loop. Presentation only: nothing here
//! answers, focuses or changes state, and a missing reading stays missing.
use super::command;
use crate::store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

/// How far back the activity strips and run counts look.
pub const WINDOW_MS: i64 = 12 * 3_600_000;
/// One strip cell per hour of the window.
pub const BUCKETS: usize = 12;
const EVENT_LIMIT: usize = 6000;
const PAGE: i64 = 200;
/// Pages read per pass; a longer backlog is caught up over later passes.
const PAGES_PER_PASS: usize = 12;
const REFRESH: Duration = Duration::from_secs(30);
/// Token usage changes slowly and costs a fleet-wide scan: every tenth pass.
const USAGE_EVERY: u32 = 10;
/// The activity kinds the deck shows; the rest is directory sync noise.
pub const FEED_KINDS: [&str; 8] = [
    "job.started",
    "job.finished",
    "task.create",
    "task.update",
    "captain.reply",
    "captain.dismiss",
    "ship.created",
    "privileged.",
];

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FleetView {
    /// The machine that answered: this one.
    pub machine: String,
    pub generated_ms: i64,
    pub machines: Vec<Machine>,
    pub ships: Vec<DeckShip>,
    pub needs: Vec<DeckNeed>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Machine {
    pub name: String,
    /// "here" for this machine.
    pub link: String,
    pub reachable: bool,
    pub as_of_ms: Option<i64>,
    pub error: Option<String>,
    pub stowed: bool,
    pub routes: Vec<Route>,
    /// What the machine could not read, e.g. "needs:privateer".
    pub unknown: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Route {
    pub name: String,
    pub state: String,
    pub cost: String,
    pub until_ms: Option<i64>,
    /// A shared plan's recorded use, e.g. 49% of a weekly window.
    pub used: Option<Used>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Used {
    pub share: f64,
    pub window: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeckShip {
    pub machine: String,
    pub name: String,
    pub harness: String,
    pub presence: String,
    pub presence_since_ms: Option<i64>,
    pub hosted: bool,
    pub liaison: bool,
    pub jobs: Vec<DeckJob>,
    pub jobs_total: usize,
    pub open_tasks_total: usize,
    pub open_requests: usize,
    pub unread: usize,
}

impl DeckShip {
    pub fn running(&self) -> impl Iterator<Item = &DeckJob> {
        self.jobs.iter().filter(|j| j.active())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeckJob {
    pub task_id: String,
    pub task_title: Option<String>,
    pub state: String,
    pub heavy: bool,
    pub started_ms: Option<i64>,
    /// "crew", "command" (a test, build or other job), "errand" or "first_mate".
    pub kind: String,
}

impl DeckJob {
    /// What the deck calls it: crew, job or errand; a first mate's own wake
    /// is the first mate, not work under it.
    pub fn noun(&self) -> Option<&'static str> {
        match self.kind.as_str() {
            "first_mate" => None,
            "command" => Some("job"),
            "errand" => Some("errand"),
            _ => Some("crew"),
        }
    }
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "running" | "launching" | "starting")
    }
}

/// One runtime need as the fleet view lists it, kept whole so the focus
/// view can show its decision and answer it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct DeckNeed(pub Value);

impl DeckNeed {
    pub fn id(&self) -> &str {
        self.0["id"].as_str().unwrap_or("")
    }
    pub fn machine(&self) -> &str {
        self.0["machine"].as_str().unwrap_or("")
    }
    pub fn needs_hands(&self) -> bool {
        self.0["needs_hands"] == true
    }
    pub fn since_ms(&self) -> Option<i64> {
        self.0["asked_ms"]
            .as_i64()
            .or(self.0["created_ms"].as_i64())
    }
    pub fn text(&self) -> &str {
        self.0["question"]
            .as_str()
            .or(self.0["summary"].as_str())
            .unwrap_or("")
    }
}

/// The whole of an ask waiting on the captain, from `captain.waiting`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ask {
    pub body: String,
    pub step: Option<String>,
    pub report: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub groups: Vec<UsageGroup>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct UsageGroup {
    pub key: String,
    pub jobs: u64,
    pub usage: Option<Tokens>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Tokens {
    #[serde(rename = "totalTokens")]
    pub total: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Resources {
    pub host: Host,
    pub disk_trend: Option<DiskTrend>,
    /// Jobs waiting for capacity.
    pub queue: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Host {
    pub cpus: f64,
    pub load: f64,
    pub disk_available_bytes: u64,
    pub memory_available_bytes: u64,
    pub memory_total_bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DiskTrend {
    pub bytes_per_hour: f64,
}

/// One recorded runtime event, trimmed to what the deck shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub at_ms: i64,
    pub machine: String,
    pub ship: String,
    pub kind: String,
    pub task: Option<String>,
    pub status: Option<String>,
    pub exit_code: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Deck {
    pub fleet: Option<FleetView>,
    /// When the fleet view was last read.
    pub fleet_ms: Option<i64>,
    pub usage: Option<Usage>,
    pub resources: Option<Resources>,
    /// Oldest first, within the window.
    pub events: VecDeque<Event>,
    /// Where the next activity read starts.
    pub cursor: Option<Value>,
    /// The activity read has caught up with the present.
    pub caught_up: bool,
    /// Asks waiting on the captain, by mail id.
    pub asks: std::collections::BTreeMap<String, Ask>,
    pub errors: Vec<String>,
}

impl Deck {
    /// Adds one `activity.list` page and says whether more is waiting.
    pub fn absorb_activity(&mut self, page: &Value, now_ms: i64) -> bool {
        let entries = page["entries"].as_array().map_or(&[][..], Vec::as_slice);
        for entry in entries {
            let detail = &entry["detail"];
            let text = |key: &str| {
                detail[key]
                    .as_str()
                    .filter(|s| !s.is_empty() && !s.contains('•'))
                    .map(str::to_owned)
            };
            let kind = entry["kind"].as_str().unwrap_or_default().to_owned();
            self.events.push_back(Event {
                at_ms: entry["at_ms"].as_i64().unwrap_or_default(),
                machine: entry["machine"].as_str().unwrap_or_default().to_owned(),
                ship: entry["ship"].as_str().unwrap_or_default().to_owned(),
                task: text("task_id").or_else(|| kind.starts_with("task.").then(|| text("id"))?),
                status: text("status"),
                exit_code: detail["exit_code"].as_i64(),
                kind,
            });
            if let Some(cursor) = entry.get("cursor").filter(|c| c.is_string()) {
                self.cursor = Some(cursor.clone());
            }
        }
        let more = page["more"].as_bool().unwrap_or(false);
        if more && let Some(next) = page.get("next").filter(|n| n.is_string()) {
            self.cursor = Some(next.clone());
        }
        self.prune(now_ms);
        more
    }

    /// Replaces the asks with a `captain.waiting` reading.
    pub fn absorb_waiting(&mut self, waiting: &Value) {
        self.asks.clear();
        for entry in waiting["waiting"].as_array().into_iter().flatten() {
            let mail = &entry["mail"];
            let Some(id) = mail["id"].as_str() else {
                continue;
            };
            let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
            self.asks.insert(
                id.to_owned(),
                Ask {
                    body: mail["body"].as_str().unwrap_or("").to_owned(),
                    step: text(&entry["step"])
                        .or_else(|| text(&mail["payload"]["needs_hands"]["step"])),
                    report: text(&mail["payload"]["report"]["content"]),
                },
            );
        }
    }

    fn prune(&mut self, now_ms: i64) {
        while self
            .events
            .front()
            .is_some_and(|e| now_ms - e.at_ms > WINDOW_MS)
            || self.events.len() > EVENT_LIMIT
        {
            self.events.pop_front();
        }
    }

    /// Crew launches per hour over the window, oldest first, among the
    /// events `keep` accepts.
    pub fn launches(&self, now_ms: i64, keep: impl Fn(&Event) -> bool) -> [u32; BUCKETS] {
        let mut buckets = [0; BUCKETS];
        for event in &self.events {
            let age = now_ms - event.at_ms;
            if event.kind != "job.started" || !(0..WINDOW_MS).contains(&age) {
                continue;
            }
            if !keep(event) {
                continue;
            }
            let hour = (age * BUCKETS as i64 / WINDOW_MS) as usize;
            buckets[BUCKETS - 1 - hour] += 1;
        }
        buckets
    }

    /// Crew runs that ended in the window: (succeeded, did not succeed).
    pub fn outcomes(&self, now_ms: i64) -> (usize, usize) {
        self.events
            .iter()
            .filter(|e| e.kind == "job.finished" && now_ms - e.at_ms < WINDOW_MS)
            .fold((0, 0), |(ok, bad), e| {
                if e.status.as_deref() == Some("succeeded") {
                    (ok + 1, bad)
                } else {
                    (ok, bad + 1)
                }
            })
    }
}

/// Reads the deck from `home` every pass and hands each reading over as soon
/// as a part of it lands. Any home works: the commands read the whole fleet.
pub fn worker(home: PathBuf) -> Receiver<Deck> {
    let (send, updates) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let binary = command::binary().unwrap_or_else(|_| "privateer".into());
        let call = |verb: &str, args: Value| command::json(&binary, &home, verb, &args);
        let mut deck = Deck::default();
        for pass in 0u32.. {
            deck.errors.clear();
            match call("fleet.snapshot", json!({})).and_then(|v| Ok(serde_json::from_value(v)?)) {
                Ok(fleet) => {
                    deck.fleet = Some(fleet);
                    deck.fleet_ms = Some(Store::now_ms());
                }
                Err(e) => deck.errors.push(format!("fleet: {e}")),
            }
            match call("captain.waiting", json!({})) {
                Ok(waiting) => deck.absorb_waiting(&waiting),
                Err(e) => deck.errors.push(format!("asks: {e}")),
            }
            match call("resource.status", json!({})).and_then(|v| Ok(serde_json::from_value(v)?)) {
                Ok(resources) => deck.resources = Some(resources),
                Err(e) => deck.errors.push(format!("resources: {e}")),
            }
            if send.send(deck.clone()).is_err() {
                return;
            }
            for _ in 0..PAGES_PER_PASS {
                let now = Store::now_ms();
                let after = deck.cursor.clone().unwrap_or(json!(now - WINDOW_MS));
                let page = call(
                    "activity.list",
                    json!({"after": after, "limit": PAGE, "kinds": FEED_KINDS}),
                );
                match page {
                    Ok(page) => {
                        let more = deck.absorb_activity(&page, now);
                        deck.caught_up = !more;
                        if !more {
                            break;
                        }
                    }
                    Err(e) => {
                        deck.errors.push(format!("activity: {e}"));
                        break;
                    }
                }
            }
            if pass % USAGE_EVERY == 0 {
                let since = Store::now_ms() - 24 * 3_600_000;
                match call("usage.summary", json!({"since_ms": since}))
                    .and_then(|v| Ok(serde_json::from_value(v)?))
                {
                    Ok(usage) => deck.usage = Some(usage),
                    Err(e) => deck.errors.push(format!("usage: {e}")),
                }
            }
            if send.send(deck.clone()).is_err() {
                return;
            }
            thread::sleep(REFRESH);
        }
    });
    updates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at_ms: i64, ship: &str, kind: &str, detail: Value) -> Value {
        json!({"at_ms": at_ms, "machine": "mac", "ship": ship, "kind": kind,
            "detail": detail, "cursor": format!("a1.{at_ms}.1.{ship}")})
    }

    #[test]
    fn activity_keeps_the_window_buckets_launches_and_resumes_from_its_cursor() {
        let now = 100 * WINDOW_MS;
        let mut deck = Deck::default();
        let more = deck.absorb_activity(
            &json!({"more": false, "entries": [
                entry(now - WINDOW_MS - 1, "old", "job.started", json!({})),
                entry(now - 30 * 60_000, "app", "job.started", json!({"task_id": "build"})),
                entry(now - 90 * 60_000, "app", "job.started", json!({"task_id": "•••"})),
                entry(now - 60_000, "app", "job.finished", json!({"status": "succeeded"})),
                entry(now - 60_000, "web", "job.finished", json!({"status": "failed", "exit_code": 1})),
            ]}),
            now,
        );
        assert!(!more);
        // The event older than the window is dropped; masked ids stay unknown.
        assert_eq!(deck.events.len(), 4);
        assert_eq!(deck.events[0].task.as_deref(), Some("build"));
        assert_eq!(deck.events[1].task, None);
        let ship = |name: &'static str| move |e: &Event| e.machine == "mac" && e.ship == name;
        let strip = deck.launches(now, ship("app"));
        assert_eq!(strip[BUCKETS - 1], 1);
        assert_eq!(strip[BUCKETS - 2], 1);
        assert_eq!(deck.launches(now, ship("web")), [0; BUCKETS]);
        assert_eq!(deck.outcomes(now), (1, 1));
        // The next read starts after the last entry seen.
        assert_eq!(
            deck.cursor,
            Some(json!(format!("a1.{}.1.web", now - 60_000)))
        );
        // A page that says more is waiting moves the cursor to its `next`.
        assert!(deck.absorb_activity(
            &json!({"more": true, "next": "a1.5.2.x", "entries": []}),
            now
        ));
        assert_eq!(deck.cursor, Some(json!("a1.5.2.x")));
    }

    #[test]
    fn fleet_view_reads_a_partial_snapshot() {
        let fleet: FleetView = serde_json::from_value(json!({
            "machine": "laptop",
            "machines": [{"name": "laptop", "link": "here", "reachable": true,
                "routes": [{"name": "opus", "state": "available",
                    "used": {"share": 0.49, "window": "weekly window"}}]}],
            "ships": [{"machine": "laptop", "name": "app", "presence": "hosted",
                "jobs": [{"task_id": "t", "state": "running", "heavy": true}],
                "future_field": 1}],
            "needs": [{"id": "n", "kind": "decision", "ship": "app", "machine": "laptop",
                "asked_ms": 5, "question": "Ship it?"}]
        }))
        .unwrap();
        assert_eq!(fleet.ships[0].running().count(), 1);
        assert_eq!(
            fleet.machines[0].routes[0].used.as_ref().unwrap().share,
            0.49
        );
        assert_eq!(fleet.needs[0].since_ms(), Some(5));
        assert_eq!(fleet.needs[0].text(), "Ship it?");
    }
}
