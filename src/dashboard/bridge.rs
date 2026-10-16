//! Bounded bridge observations. Attention is exclusively runtime.needs, never mail prose.
use super::{command, model::Snapshot};
use crate::store::{Caller, Store};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const PAGE_LIMIT: usize = 100;
pub const MAX_PAGES: usize = 4;
pub const MAX_ITEMS: usize = 400;
pub const FRESH_MS: i64 = 10_000;
pub const RECENT_MS: i64 = 3 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Identity {
    pub normalized_home: PathBuf,
    pub directory_device: u64,
    pub directory_inode: u64,
    pub device: u64,
    pub inode: u64,
}
impl Identity {
    pub fn read(home: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        command::existing_home(home)?;
        crate::store::reject_symlinks(home)?;
        let normalized_home = std::fs::canonicalize(home)?;
        let directory = std::fs::metadata(&normalized_home)?;
        let meta = std::fs::metadata(home.join("privateer.sqlite"))?;
        Ok(Self {
            normalized_home,
            directory_device: directory.dev(),
            directory_inode: directory.ino(),
            device: meta.dev(),
            inode: meta.ino(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Need {
    pub home: PathBuf,
    pub identity: Identity,
    pub item: Value,
    pub stale: bool,
}
impl Need {
    pub fn id(&self) -> &str {
        self.item["id"].as_str().unwrap_or("")
    }
    pub fn same_content(&self, other: &Need) -> bool {
        let mut expected = self.item.clone();
        let mut current = other.item.clone();
        for item in [&mut expected, &mut current] {
            if let Some(object) = item.as_object_mut() {
                object.remove("observed_ms");
            }
        }
        expected == current
    }
    pub fn binding(&self) -> Binding {
        Binding {
            home: self.home.clone(),
            identity: self.identity.clone(),
            origin: self.item["origin"].as_str().unwrap_or("").into(),
            id: self.id().into(),
        }
    }
}

/// Operator selection follows a source only within its observed physical generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub home: PathBuf,
    pub identity: Identity,
    pub origin: String,
    pub id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Source {
    pub home: PathBuf,
    pub origin: Option<String>,
    pub available: bool,
    pub checked_ms: i64,
    pub unknown: Vec<String>,
    pub error: Option<String>,
    pub items: Vec<Need>,
    #[serde(skip)]
    next_due_ms: i64,
    #[serde(skip)]
    failures: u32,
    #[serde(skip)]
    pending: Vec<Need>,
    #[serde(skip)]
    scan: Option<(Identity, String)>,
    #[serde(skip)]
    after: Option<String>,
    #[serde(skip)]
    pages: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Attention {
    pub sources: Vec<Source>,
    pub error: Option<String>,
    #[serde(skip)]
    next: usize,
}

pub fn homes(snapshot: &Snapshot, fallback: Option<&Path>) -> BTreeMap<PathBuf, Option<String>> {
    let mut homes = BTreeMap::new();
    for live in snapshot.ships.iter().filter_map(|s| s.live.as_ref()) {
        let error = homes.entry(live.home.clone()).or_insert(None);
        if live.error.is_some() {
            *error = live.error.clone();
        }
    }
    if homes.is_empty()
        && let Some(home) = fallback
    {
        homes.insert(home.to_path_buf(), None);
    }
    homes
}

impl Attention {
    /// One home/page per call, no remote adapters. Partial scans never replace retained facts.
    pub fn step(&mut self, snapshot: &Snapshot, fallback: Option<&Path>) {
        match Caller::from_env(None, None) {
            Ok(caller) => self.step_as(snapshot, fallback, &caller),
            Err(e) => self.error = Some(e.to_string()),
        }
    }
    fn step_as(&mut self, snapshot: &Snapshot, fallback: Option<&Path>, caller: &Caller) {
        let homes = homes(snapshot, fallback);
        self.error = snapshot.presentation_error.clone();
        if !snapshot.scope_complete() {
            self.error =
                Some("Fleet discovery incomplete/unknown; unobserved homes remain unknown".into());
        }
        for source in &mut self.sources {
            if !homes.contains_key(&source.home) {
                source.available = false;
                source.pending.clear();
                source.scan = None;
                source.after = None;
                source.pages = 0;
                source.error =
                    Some("Home no longer configured; retained history is not resolution".into());
                for item in &mut source.items {
                    item.stale = true;
                }
            }
        }
        for home in homes.keys().take(200) {
            if self.sources.len() >= 200 && !self.sources.iter().any(|s| &s.home == home) {
                self.error =
                    Some("Source retention capped at 200 homes; additional homes unknown".into());
                break;
            }
            if !self.sources.iter().any(|s| &s.home == home) {
                self.sources.push(Source {
                    home: home.clone(),
                    error: Some("Not observed yet".into()),
                    ..Source::default()
                });
            }
        }
        if self.sources.is_empty() {
            return;
        }
        self.next %= self.sources.len();
        let source = &mut self.sources[self.next];
        self.next += 1;
        if Store::now_ms() < source.next_due_ms {
            return;
        }
        let result = (|| -> Result<()> {
            ensure!(self.error.is_none(), "presentation/home scope unavailable");
            ensure!(
                homes.get(&source.home).is_some_and(Option::is_none),
                "home unavailable/retired"
            );
            let identity = Identity::read(&source.home)?;
            if let Some((pinned, _)) = &source.scan {
                ensure!(pinned == &identity, "home replaced during pagination");
            }
            let store = Store::open_readonly(&source.home, caller.clone())?;
            ensure!(
                Identity::read(&source.home)? == identity,
                "home replaced while opening observation"
            );
            let page = crate::needs::query(
                &store,
                &json!({"address":"captain","limit":PAGE_LIMIT,"after":source.after}),
            )?;
            ensure!(
                Identity::read(&source.home)? == identity,
                "home replaced during observation"
            );
            ensure!(
                page["schema_version"] == 1 && page["available"] == true,
                "{}",
                page["reason"]
                    .as_str()
                    .unwrap_or("runtime.needs unavailable")
            );
            let origin = page["origin"].as_str().context("missing durable origin")?;
            if let Some((_, pinned_origin)) = &source.scan {
                ensure!(pinned_origin == origin, "origin changed during pagination");
            } else {
                source.scan = Some((identity.clone(), origin.into()));
            }
            source.origin = Some(origin.into());
            source.unknown = serde_json::from_value(page["unknown"].clone())?;
            for item in page["items"].as_array().context("missing runtime items")? {
                ensure!(
                    item["origin"] == origin
                        && item["id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with(&format!("{origin}:"))),
                    "invalid runtime identity"
                );
                source.pending.push(Need {
                    home: source.home.clone(),
                    identity: identity.clone(),
                    item: item.clone(),
                    stale: false,
                });
            }
            source.pages += 1;
            source.after = page["next_after"].as_str().map(str::to_owned);
            ensure!(
                source.after.is_none() || source.pages < MAX_PAGES,
                "Attention exceeds bounded four-page scan; retained history is not an all-clear"
            );
            if source.after.is_none() {
                source.items = std::mem::take(&mut source.pending);
                source.pages = 0;
                source.scan = None;
                source.available = true;
                source.checked_ms = Store::now_ms();
                source.next_due_ms = source.checked_ms + 2000;
                source.failures = 0;
                source.error = None;
            }
            Ok(())
        })();
        if let Err(error) = result {
            source.available = false;
            source.error = Some(error.to_string());
            source.failures = source.failures.saturating_add(1);
            source.next_due_ms = Store::now_ms() + (1000_i64 << source.failures.min(4));
            source.pending.clear();
            source.scan = None;
            source.after = None;
            source.pages = 0;
            for item in &mut source.items {
                item.stale = true;
            }
        }
        let mut origins = BTreeMap::<String, usize>::new();
        for source in self.sources.iter().filter(|s| homes.contains_key(&s.home)) {
            if let Some(origin) = &source.origin {
                *origins.entry(origin.clone()).or_default() += 1;
            }
        }
        let mut remaining = MAX_ITEMS;
        for source in &mut self.sources {
            if source
                .origin
                .as_ref()
                .is_some_and(|o| origins.get(o).copied().unwrap_or(0) > 1)
            {
                source.available = false;
                source.error = Some("Copied/ambiguous runtime origin; actions refused".into());
                for item in &mut source.items {
                    item.stale = true;
                }
            }
            if source.items.len() > remaining {
                source.items.truncate(remaining);
                source.available = false;
                source.error = Some(
                    "Attention retention bounded to 400 items; additional facts unknown".into(),
                );
                for item in &mut source.items {
                    item.stale = true;
                }
            }
            remaining = remaining.saturating_sub(source.items.len());
        }
    }
    pub fn refresh_home(&mut self, home: &Path) {
        if let Some(index) = self.sources.iter().position(|s| s.home == home) {
            self.sources[index].next_due_ms = 0;
            self.next = index;
        }
    }
    pub fn items(&self) -> Vec<&Need> {
        self.sources.iter().flat_map(|s| &s.items).collect()
    }
    pub fn actionable(&self, need: &Need) -> bool {
        !need.stale
            && self.error.is_none()
            && self.sources.iter().any(|s| {
                s.home == need.home
                    && s.available
                    && s.error.is_none()
                    && Store::now_ms() - s.checked_ms < FRESH_MS
                    && s.items
                        .iter()
                        .any(|n| n.binding() == need.binding() && n.same_content(need))
            })
    }
}

/// Explicit local action only. The same connection reobserves and mutates; no
/// CLI reopen/migration. This guards ordinary replacement races, not a hostile
/// same-user filesystem sandbox. Core/mail retain their atomic authority locks.
pub fn execute(
    snapshot: &Snapshot,
    fallback: Option<&Path>,
    attention: &Attention,
    answer: &Answer,
) -> Result<Value> {
    execute_as(
        snapshot,
        fallback,
        attention,
        answer,
        Caller::from_env(None, None)?,
    )
}
fn execute_as(
    snapshot: &Snapshot,
    fallback: Option<&Path>,
    attention: &Attention,
    answer: &Answer,
    caller: Caller,
) -> Result<Value> {
    caller.require_operator("bridge answer")?;
    let need = &answer.need;
    ensure!(
        snapshot.scope_complete() && snapshot.presentation_error.is_none(),
        "discovery/presentation scope incomplete/unknown/unavailable"
    );
    ensure!(
        attention.actionable(need),
        "attention unavailable/stale; refresh and reselect"
    );
    let scope = homes(snapshot, fallback);
    ensure!(
        scope.get(&need.home).is_some_and(Option::is_none),
        "home retired/replaced/unavailable"
    );
    ensure!(
        Identity::read(&need.home)? == need.identity,
        "home replaced; refresh and reselect"
    );
    ensure!(scope.len() <= 200, "action home scope exceeds bound");
    for home in scope.keys().filter(|h| *h != &need.home) {
        let other = Store::open_readonly(home, caller.clone())?;
        let origin: String = other.connection().query_row(
            "SELECT value FROM metadata WHERE key='runtime_origin'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            need.item["origin"] != origin,
            "copied/ambiguous runtime origin; action refused"
        );
    }
    let mut store = Store::open_existing_action(&need.home, caller)?;
    ensure!(
        Identity::read(&need.home)? == need.identity,
        "home replaced during open"
    );
    let origin: String = store.connection().query_row(
        "SELECT value FROM metadata WHERE key='runtime_origin'",
        [],
        |r| r.get(0),
    )?;
    ensure!(need.item["origin"] == origin, "home origin changed");
    // Bound the action recheck just like refresh (empty continuation pages count).
    let mut after = None::<String>;
    let mut matched = false;
    for _ in 0..MAX_PAGES {
        let page = crate::needs::query(&store, &json!({"limit":PAGE_LIMIT,"after":after}))?;
        ensure!(
            page["available"] == true && page["origin"] == origin,
            "runtime.needs unavailable"
        );
        for item in page["items"].as_array().context("missing runtime items")? {
            if item["id"] == need.item["id"] {
                // observed_ms changes; all semantic identity/authority/content must not.
                let mut expected = need.item.clone();
                let mut current = item.clone();
                expected
                    .as_object_mut()
                    .context("invalid item")?
                    .remove("observed_ms");
                current
                    .as_object_mut()
                    .context("invalid item")?
                    .remove("observed_ms");
                ensure!(expected == current, "request/version/binding changed");
                matched = true;
                break;
            }
        }
        after = page["next_after"].as_str().map(str::to_owned);
        if matched || after.is_none() {
            break;
        }
    }
    let (verb, args) = answer.command()?;
    if !matched {
        // Only a proved same committed outcome may reach the runtime's idempotent
        // retry path. A missing/closed/replaced item is never new authorization.
        let original = crate::mail::find(
            &store,
            need.item["source_id"].as_str().context("source id")?,
        )?
        .context("request missing")?;
        let committed = if verb == "mail.send" {
            let prior = crate::mail::find(&store, &answer.id)?;
            prior.is_some_and(|m| {
                m["from"] == "captain"
                    && m["to"] == args["to"]
                    && m["kind"] == "reply"
                    && m["correlation"] == args["correlation"]
                    && m["body"] == args["body"]
                    && original["answered_by"] == answer.id
            })
        } else if verb == "approval.grant" {
            original["state"] == "approved" && store.connection().query_row("SELECT EXISTS(SELECT 1 FROM approvals WHERE id=?1 AND task_id=?2 AND artifact=?3 AND environment=?4 AND charter_hash=?5 AND approver='captain')", rusqlite::params![answer.id, args["task_id"].as_str(), args["artifact"].as_str(), args["environment"].as_str(), args["expected_version"].as_str()], |r|r.get::<_,bool>(0))?
        } else {
            original["state"] == "denied"
        };
        ensure!(
            committed,
            "request closed, replaced or outside bounded observation; refresh"
        );
    }
    ensure!(
        matches!(verb, "mail.send" | "approval.grant" | "approval.deny"),
        "unsupported bridge action"
    );
    ensure!(
        Identity::read(&need.home)? == need.identity,
        "home replaced before dispatch"
    );
    crate::dispatch(&mut store, verb, args)
}

/// A draft owns immutable selection, origin and stable outcome IDs across retries.
#[derive(Clone, Debug)]
pub struct Answer {
    pub need: Need,
    pub id: String,
    pub body: String,
    pub choice: usize,
    pub deny: bool,
}
impl Answer {
    pub fn new(need: Need) -> Self {
        Self {
            need,
            id: uuid::Uuid::new_v4().to_string(),
            body: String::new(),
            choice: 0,
            deny: false,
        }
    }
    pub fn command(&self) -> Result<(&'static str, Value)> {
        let item = &self.need.item;
        let actions = item["actions"]
            .as_array()
            .context("missing typed actions")?;
        if item["kind"] == "approval" {
            let verb = if self.deny {
                "approval.deny"
            } else {
                "approval.grant"
            };
            ensure!(
                actions.contains(&json!(verb)),
                "unsupported approval action"
            );
            Ok((
                verb,
                json!({"id":self.id,"request_id":item["source_id"],"expected_version":item["version"],"task_id":item["task_id"],"artifact":item["artifact"],"environment":item["environment"],"approver":"captain"}),
            ))
        } else {
            ensure!(
                item["kind"] == "decision" && actions.contains(&json!("mail.send")),
                "no inline action for recorded blockers"
            );
            let choices = item["choices"].as_array().context("missing choices")?;
            let body = if choices.is_empty() {
                self.body.as_str()
            } else {
                choices
                    .get(self.choice)
                    .and_then(Value::as_str)
                    .context("select a provided choice")?
            };
            ensure!(!body.trim().is_empty(), "answer is empty");
            Ok((
                "mail.send",
                json!({"id":self.id,"from":"captain","to":item["ship"],"kind":"reply","correlation":item["source_id"],"body":body}),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{model::Snapshot, ui::App};
    use super::*;
    use crossterm::event::KeyCode;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fixture() -> (tempfile::TempDir, Store, App) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let mut store = Store::open(&root.path().join("home")).unwrap();
        crate::dispatch(
            &mut store,
            "charter.put",
            json!({"id":"project","objective":"Deliver","repo":repo,"min_free_bytes":0}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"main","charter_id":"project","owner":"apps","title":"Recorded work"}),
        )
        .unwrap();
        crate::dispatch(
            &mut store,
            "ship.put",
            json!({"name":"apps","scope":"Apps"}),
        )
        .unwrap();
        let app = App {
            snapshot: super::super::fleet::read(
                &root.path().join("fleet.sqlite"),
                Some(store.home()),
            )
            .unwrap(),
            ..App::default()
        };
        (root, store, app)
    }
    fn observe(app: &mut App, home: &Path) {
        let mut attention = app.attention.clone();
        for source in &mut attention.sources {
            source.next_due_ms = 0;
        }
        attention.step_as(&app.snapshot, Some(home), &Caller::Operator);
        app.update_attention(attention);
    }
    fn intent(app: &mut App, text: &str) -> Answer {
        if !app.needs_focus {
            app.bridge_key(KeyCode::Tab);
        }
        app.bridge_key(KeyCode::Enter);
        assert!(app.answer.is_some(), "{}", app.notice);
        for c in text.chars() {
            app.bridge_key(KeyCode::Char(c));
        }
        app.bridge_key(KeyCode::Enter);
        app.answer_action.take().unwrap()
    }
    fn act(app: &App, home: &Path, answer: &Answer) -> Result<Value> {
        execute_as(
            &app.snapshot,
            Some(home),
            &app.attention,
            answer,
            Caller::Operator,
        )
    }

    #[test]
    fn clipped_recorded_jobs_do_not_hide_needs_or_block_reply_without_ack() {
        let (root, mut store, mut app) = fixture();
        for index in 0..201 {
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        format!("history-{index}"),
                        json!({"task_id":"main","status":"succeeded"}).to_string()
                    ],
                )
                .unwrap();
        }
        crate::dispatch(&mut store, "mail.send", json!({"id":"question","from":"apps","to":"captain","kind":"request","body":"Please answer"})).unwrap();
        app.snapshot =
            super::super::fleet::read(&root.path().join("fleet.sqlite"), Some(store.home()))
                .unwrap();
        assert!(app.snapshot.truncated && app.snapshot.scope_complete());
        assert_eq!(app.snapshot.jobs.len(), 200);
        observe(&mut app, store.home());
        assert!(app.attention.error.is_none());
        assert_eq!(app.attention.items().len(), 1);
        let answer = intent(&mut app, "explicit reply");
        // Neither unknown defaults nor older serialized snapshots supply discovery evidence.
        let mut unknown = app.snapshot.clone();
        unknown.discovery = super::super::model::Discovery::Unknown;
        let mut retained = app.attention.clone();
        retained.step_as(&unknown, Some(store.home()), &Caller::Operator);
        assert!(retained.error.is_some());
        assert_eq!(retained.items().len(), 1);
        assert!(!retained.actionable(&answer.need));
        assert!(
            execute_as(
                &unknown,
                Some(store.home()),
                &app.attention,
                &answer,
                Caller::Operator
            )
            .is_err()
        );
        let mut serialized = serde_json::to_value(&app.snapshot).unwrap();
        serialized.as_object_mut().unwrap().remove("discovery");
        assert!(
            !serde_json::from_value::<Snapshot>(serialized)
                .unwrap()
                .scope_complete()
        );
        assert!(!Snapshot::default().scope_complete());
        assert_eq!(
            act(&app, store.home(), &answer).unwrap()["body"],
            "explicit reply"
        );
        assert_eq!(
            crate::dispatch(&mut store, "mail.inbox", json!({"address":"captain"})).unwrap()["acknowledged_through"],
            0
        );
        observe(&mut app, store.home());
        assert!(app.attention.items().is_empty());
    }

    #[test]
    fn input_to_correlated_choice_and_text_reply_is_idempotent_and_closure_is_not_ack() {
        let (_root, mut store, mut app) = fixture();
        crate::dispatch(&mut store,"mail.send",json!({"id":"choose","from":"apps","to":"captain","kind":"decision","task_id":"main","body":"Which path?","payload":{"choices":["inspect","wait"]}})).unwrap();
        observe(&mut app, store.home());
        assert_eq!(app.attention.items().len(), 1);
        let seq = crate::mail::find(&store, "choose").unwrap().unwrap()["seq"].clone();
        crate::dispatch(
            &mut store,
            "mail.ack",
            json!({"address":"captain","through_seq":seq}),
        )
        .unwrap();
        observe(&mut app, store.home());
        assert_eq!(app.attention.items().len(), 1);
        app.bridge_key(KeyCode::Tab);
        app.bridge_key(KeyCode::Enter);
        app.bridge_key(KeyCode::Right);
        app.bridge_key(KeyCode::Enter);
        let answer = app.answer_action.take().unwrap();
        let first = act(&app, store.home(), &answer).unwrap();
        assert_eq!(first["correlation"], "choose");
        assert_eq!(first["body"], "wait");
        assert_eq!(act(&app, store.home(), &answer).unwrap()["id"], first["id"]);
        observe(&mut app, store.home());
        assert!(app.attention.items().is_empty());
        app.answer = None;
        app.select_ship(None);
        crate::dispatch(&mut store,"mail.send",json!({"id":"text","from":"apps","to":"captain","kind":"request","task_id":"main","body":"Question?"})).unwrap();
        observe(&mut app, store.home());
        let answer = intent(&mut app, "free text");
        assert_eq!(
            act(&app, store.home(), &answer).unwrap()["body"],
            "free text"
        );
        app.answer = None;
        app.select_ship(None);
        for id in ["withdraw", "terminal"] {
            crate::dispatch(&mut store,"mail.send",json!({"id":id,"from":"apps","to":"captain","kind":"request","task_id":"main","body":"Question?"})).unwrap();
        }
        observe(&mut app, store.home());
        let old = app
            .attention
            .items()
            .iter()
            .find(|n| n.item["source_id"] == "withdraw")
            .unwrap()
            .to_owned()
            .clone();
        let mut stale = Answer::new(old);
        stale.body = "too late".into();
        crate::dispatch(
            &mut store,
            "mail.withdraw",
            json!({"id":"withdraw","actor":"apps","reason":"obsolete"}),
        )
        .unwrap();
        assert!(act(&app, store.home(), &stale).is_err());
        observe(&mut app, store.home());
        assert_eq!(app.attention.items().len(), 1);
        crate::dispatch(
            &mut store,
            "task.update",
            json!({"id":"main","actor":"apps","status":"done"}),
        )
        .unwrap();
        observe(&mut app, store.home());
        assert!(app.attention.items().is_empty());
        assert!(!app.attention.sources[0].unknown.is_empty());
        assert_eq!(
            crate::mail::find(&store, "withdraw").unwrap().unwrap()["state"],
            "withdrawn"
        );
    }

    #[test]
    fn exact_approval_intent_grants_or_denies_and_stale_authority_never_grants() {
        let (_root, mut store, mut app) = fixture();
        for (id, deny) in [("approve", false), ("deny", true), ("stale", false)] {
            crate::dispatch(&mut store,"mail.send",json!({"id":id,"from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Ship exact artifact?","payload":{"artifact":"sha256:exact","environment":"isolated-production"}})).unwrap();
            app.select_ship(None);
            app.answer = None;
            observe(&mut app, store.home());
            app.bridge_key(KeyCode::Tab);
            app.bridge_key(KeyCode::Enter);
            if deny {
                app.bridge_key(KeyCode::Char('d'));
            }
            app.bridge_key(KeyCode::Enter);
            let answer = app.answer_action.take().unwrap();
            let (_, args) = answer.command().unwrap();
            assert_eq!(args["request_id"], id);
            assert_eq!(args["artifact"], "sha256:exact");
            assert_eq!(args["environment"], "isolated-production");
            assert!(args["expected_version"].as_str().is_some());
            if id == "stale" {
                crate::dispatch(&mut store, "charter.revoke", json!({"id":"project"})).unwrap();
                assert!(act(&app, store.home(), &answer).is_err());
            } else {
                act(&app, store.home(), &answer).unwrap();
                act(&app, store.home(), &answer).unwrap();
                assert_eq!(
                    crate::mail::find(&store, id).unwrap().unwrap()["state"],
                    if deny { "denied" } else { "approved" }
                );
            }
            observe(&mut app, store.home());
            assert!(app.attention.items().is_empty());
        }
        assert_eq!(
            store
                .connection()
                .query_row("SELECT count(*) FROM approvals", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn replaced_copied_missing_old_symlink_and_agent_homes_refuse_without_repair() {
        let (root, mut store, mut app) = fixture();
        crate::dispatch(
            &mut store,
            "mail.send",
            json!({"id":"request","from":"apps","to":"captain","kind":"request","body":"Answer?"}),
        )
        .unwrap();
        observe(&mut app, store.home());
        let answer = intent(&mut app, "yes");
        // A normal refresh changes observed_ms, not selection or draft authority.
        std::thread::sleep(std::time::Duration::from_millis(2));
        observe(&mut app, store.home());
        assert!(app.attention.actionable(&answer.need));
        assert_eq!(app.answer.as_ref().unwrap().id, answer.id);
        let mut truncated = app.snapshot.clone();
        truncated.discovery = super::super::model::Discovery::Incomplete;
        assert!(
            execute_as(
                &truncated,
                Some(store.home()),
                &app.attention,
                &answer,
                Caller::Operator
            )
            .is_err()
        );
        assert!(
            execute_as(
                &app.snapshot,
                Some(store.home()),
                &app.attention,
                &answer,
                Caller::Agent {
                    actor: "apps".into()
                }
            )
            .is_err()
        );
        store
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let home = store.home().to_path_buf();
        let saved = root.path().join("saved");
        fs::rename(&home, &saved).unwrap();
        fs::create_dir(&home).unwrap();
        fs::copy(
            saved.join("privateer.sqlite"),
            home.join("privateer.sqlite"),
        )
        .unwrap();
        assert!(
            act(&app, &home, &answer)
                .unwrap_err()
                .to_string()
                .contains("replaced")
        );
        observe(&mut app, &home);
        assert_eq!(app.attention.items()[0].id(), answer.need.id());
        assert_ne!(app.attention.items()[0].identity, answer.need.identity);
        assert!(app.need_id.is_none());
        // An immutable open draft never rebinds, even after a complete fresh scan.
        app.bridge_key(KeyCode::Enter);
        let old_intent = app.answer_action.take().unwrap();
        assert_eq!(old_intent.need.identity, answer.need.identity);
        assert!(act(&app, &home, &old_intent).is_err());
        app.bridge_key(KeyCode::Esc);
        app.bridge_key(KeyCode::Enter);
        assert!(app.answer.is_none() && app.answer_action.is_none());
        app.bridge_key(KeyCode::Down); // explicit reselection of the copied generation
        let new_intent = intent(&mut app, "fresh answer");
        assert_ne!(new_intent.need.identity, answer.need.identity);
        assert_eq!(
            act(&app, &home, &new_intent).unwrap()["body"],
            "fresh answer"
        );
        app.answer = None;
        fs::remove_dir_all(&home).unwrap();
        observe(&mut app, &home);
        assert_eq!(app.attention.items().len(), 1);
        assert!(app.attention.items()[0].stale);
        assert!(act(&app, &home, &answer).is_err());
        assert!(!home.exists());
        std::os::unix::fs::symlink(&saved, &home).unwrap();
        assert!(act(&app, &home, &answer).is_err());
        fs::remove_file(&home).unwrap();
        fs::rename(&saved, &home).unwrap();
        let copy = root.path().join("copy");
        fs::create_dir(&copy).unwrap();
        fs::copy(home.join("privateer.sqlite"), copy.join("privateer.sqlite")).unwrap();
        app.snapshot.ships.push(super::super::model::Ship {
            id: "copy".into(),
            name: "copy".into(),
            live: Some(super::super::fleet::ShipState {
                home: copy,
                ..Default::default()
            }),
            ..Default::default()
        });
        app.snapshot.ships[0].live = Some(super::super::fleet::ShipState {
            home: home.clone(),
            ..Default::default()
        });
        observe(&mut app, &home);
        observe(&mut app, &home);
        assert!(app.attention.sources.iter().all(|s| !s.available));
        assert!(act(&app, &home, &answer).is_err());
        let old = root.path().join("old");
        fs::create_dir(&old).unwrap();
        let db = old.join("privateer.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE metadata(key TEXT,value TEXT); INSERT INTO metadata VALUES('schema','1');").unwrap();
        drop(conn);
        fs::set_permissions(&db, fs::Permissions::from_mode(0o640)).unwrap();
        let bytes = fs::read(&db).unwrap();
        assert!(Store::open_existing_action(&old, Caller::Operator).is_err());
        assert!(
            Store::open_existing_action(
                &old,
                Caller::Agent {
                    actor: "apps".into()
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(&db).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(&old).unwrap().count(), 1);

        // Real runtime.needs source-key pagination can start with EMPTY items
        // and a nonnull cursor: exact approvals whose authority is now obsolete.
        let (root, mut store, mut app) = fixture();
        for i in 0..PAGE_LIMIT {
            crate::dispatch(&mut store, "mail.send", json!({"id":format!("stale-{i:03}"),"from":"apps","to":"captain","kind":"request","request_kind":"production_approval","task_id":"main","body":"Exact?","payload":{"artifact":"old","environment":"fixture"}})).unwrap();
        }
        let mut charter = store.charter("project").unwrap();
        charter["objective"] = json!("Changed authority");
        crate::dispatch(&mut store, "charter.put", charter).unwrap();
        crate::dispatch(&mut store,"mail.send",json!({"id":"zz-tail","from":"apps","to":"captain","kind":"request","body":"Retained fact"})).unwrap();
        let page = crate::needs::query(&store, &json!({"limit":PAGE_LIMIT})).unwrap();
        assert_eq!(page["available"], true);
        assert!(page["items"].as_array().unwrap().is_empty());
        assert!(page["next_after"].as_str().is_some());
        observe(&mut app, store.home());
        assert!(app.attention.sources[0].pending.is_empty());
        assert!(app.attention.sources[0].scan.is_some());
        assert!(!app.attention.sources[0].available);
        observe(&mut app, store.home());
        assert_eq!(app.attention.items()[0].item["source_id"], "zz-tail");
        // Begin another scan, again with an EMPTY first continuation page.
        observe(&mut app, store.home());
        assert!(app.attention.sources[0].pending.is_empty());
        assert!(app.attention.sources[0].after.is_some());
        store
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let home = store.home().to_owned();
        let saved = root.path().join("saved");
        fs::rename(&home, &saved).unwrap();
        fs::create_dir(&home).unwrap();
        fs::copy(
            saved.join("privateer.sqlite"),
            home.join("privateer.sqlite"),
        )
        .unwrap();
        let mut replacement = Store::open_existing_action(&home, Caller::Operator).unwrap();
        crate::dispatch(&mut replacement,"mail.send",json!({"id":"000-fresh","from":"apps","to":"captain","kind":"request","body":"Earlier key in copied home"})).unwrap();
        observe(&mut app, &home);
        let source = &app.attention.sources[0];
        assert!(!source.available);
        assert!(source.error.as_ref().unwrap().contains("replaced"));
        assert!(source.scan.is_none() && source.after.is_none() && source.pending.is_empty());
        assert_eq!(source.items.len(), 1);
        assert!(source.items[0].stale);
        // Restart from the beginning, never publish a partial all-clear.
        observe(&mut app, &home);
        assert!(!app.attention.sources[0].available);
        assert_eq!(app.attention.items().len(), 1);
        observe(&mut app, &home);
        assert!(app.attention.sources[0].available);
        assert_eq!(
            app.attention
                .items()
                .iter()
                .map(|n| n.item["source_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["000-fresh", "zz-tail"]
        );
        assert!(
            app.attention
                .items()
                .iter()
                .all(|n| !n.stale && n.identity == Identity::read(&home).unwrap())
        );
    }

    #[test]
    fn recently_done_requires_audited_completion_time_and_expires_without_fabricated_merges() {
        let (_root, mut store, _app) = fixture();
        crate::dispatch(&mut store,"event.emit",json!({"id":"prose","task_id":"main","issue_key":"done","kind":"outcome","payload":{"summary":"merged everything"}})).unwrap();
        assert!(Snapshot::read(store.home()).unwrap().recent.is_empty());
        crate::dispatch(
            &mut store,
            "task.update",
            json!({"id":"main","actor":"apps","status":"done"}),
        )
        .unwrap();
        let snapshot = Snapshot::read(store.home()).unwrap();
        assert_eq!(snapshot.recent.len(), 1);
        let recent = &snapshot.recent[0];
        assert_eq!(recent.owner, "apps");
        assert_eq!(recent.home, store.home());
        store
            .connection()
            .execute(
                "UPDATE audit SET time_ms=?1 WHERE kind='task.update'",
                [Store::now_ms() - RECENT_MS - 1],
            )
            .unwrap();
        assert!(Snapshot::read(store.home()).unwrap().recent.is_empty());
        store
            .connection()
            .execute("DELETE FROM audit WHERE kind='task.update'", [])
            .unwrap();
        assert!(Snapshot::read(store.home()).unwrap().recent.is_empty());
    }
}
