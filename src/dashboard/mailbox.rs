//! Captain mail is always read and changed through the runtime's public commands.
use super::{captain::Mail, command, model::Snapshot};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Action {
    pub home: PathBuf,
    pub verb: String,
    pub args: Value,
}

#[derive(Default)]
pub struct Mailbox {
    pub messages: Vec<Mail>,
    pub errors: Vec<String>,
    // Reading a priority-sorted item must not acknowledge unseen earlier mail.
    read: BTreeSet<(PathBuf, i64)>,
    unread: BTreeMap<PathBuf, Vec<i64>>,
    generations: BTreeMap<PathBuf, (super::bridge::Identity, String)>,
    valid: BTreeMap<PathBuf, i64>,
}
impl Mailbox {
    pub fn refresh(&mut self, binary: &Path, snapshot: &Snapshot, fallback: Option<&Path>) {
        self.refresh_mode(binary, snapshot, fallback, None);
    }
    /// Bridge observation never uses normal CLI startup/migrations or ack writes.
    pub fn refresh_readonly(&mut self, snapshot: &Snapshot, fallback: Option<&Path>) {
        match crate::store::Caller::from_env(None, None) {
            Ok(caller) => self.refresh_mode(Path::new(""), snapshot, fallback, Some(caller)),
            Err(e) => {
                self.errors = vec![e.to_string()];
                self.valid.clear();
                self.read.clear();
                self.unread.clear();
            }
        }
    }
    fn refresh_mode(
        &mut self,
        binary: &Path,
        snapshot: &Snapshot,
        fallback: Option<&Path>,
        caller: Option<crate::store::Caller>,
    ) {
        let readonly = caller.is_some();
        if let Some(caller) = &caller
            && let Err(e) = caller.require_operator("captain history observation")
        {
            self.errors = vec![e.to_string()];
            self.messages.clear();
            self.read.clear();
            self.unread.clear();
            self.valid.clear();
            return;
        }
        if readonly {
            self.valid.clear();
        }
        let mut homes: BTreeMap<PathBuf, Option<&str>> = BTreeMap::new();
        for live in snapshot.ships.iter().filter_map(|s| s.live.as_ref()) {
            let error = homes.entry(live.home.clone()).or_default();
            if live.error.is_some() {
                *error = live.error.as_deref();
            }
        }
        if homes.is_empty() {
            if let Some(home) = fallback {
                homes.insert(home.to_path_buf(), None);
            }
        }
        let mut messages = Vec::new();
        let mut errors = Vec::new();
        let priority = |m: &Mail| match m.kind.as_str() {
            "decision" => 0,
            "request" => 1,
            "reply" => 2,
            _ => 3,
        };
        self.unread.retain(|home, _| homes.contains_key(home));
        self.read.retain(|(home, _)| homes.contains_key(home));
        self.generations.retain(|home, _| homes.contains_key(home));
        for (home, error) in homes {
            let result = (|| -> Result<_> {
                anyhow::ensure!(error.is_none(), "{}", error.unwrap_or_default());
                let identity = readonly
                    .then(|| super::bridge::Identity::read(&home))
                    .transpose()?;
                let mut store = readonly
                    .then(|| {
                        crate::store::Store::open_readonly(
                            &home,
                            caller.clone().context("missing observation caller")?,
                        )
                    })
                    .transpose()?;
                if let Some(store) = &store {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(250);
                    let mut checkpoints = 0;
                    store.connection().progress_handler(
                        1000,
                        Some(move || {
                            checkpoints += 1;
                            checkpoints >= 2000 || std::time::Instant::now() >= deadline
                        }),
                    );
                    let oversized: bool=store.connection().query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE length(CAST(body AS BLOB))>65536 OR length(CAST(payload AS BLOB))>65536)",[],|r|r.get(0))?;
                    anyhow::ensure!(
                        !oversized,
                        "history source exceeds bound; not read or repaired"
                    );
                }
                let origin = store
                    .as_ref()
                    .map(|s| {
                        s.connection().query_row(
                            "SELECT value FROM metadata WHERE key='runtime_origin'",
                            [],
                            |r| r.get::<_, String>(0),
                        )
                    })
                    .transpose()?;
                if let (Some(identity), Some(origin)) = (&identity, &origin) {
                    let generation = (identity.clone(), origin.clone());
                    if self.generations.get(&home) != Some(&generation) {
                        self.read.retain(|(h, _)| h != &home);
                        self.unread.remove(&home);
                    }
                    self.generations.insert(home.clone(), generation);
                }
                let mut query = |verb: &str, args: &Value| -> Result<Value> {
                    if let Some(store) = &mut store {
                        crate::mail::dispatch(store, verb, args.clone())
                    } else {
                        command::json(binary, &home, verb, args)
                    }
                };
                let inbox = query(
                    "mail.inbox",
                    &json!({"address":"captain","limit":command::MAIL_PAGE_LIMIT}),
                )?;
                let requests = query(
                    "mail.requests",
                    &json!({"address":"captain","state":"open","limit":command::MAIL_PAGE_LIMIT}),
                )?;
                let cursor = inbox["acknowledged_through"].as_i64().unwrap_or(0);
                self.unread.insert(
                    home.clone(),
                    inbox["messages"]
                        .as_array()
                        .context("missing inbox messages")?
                        .iter()
                        .filter_map(|m| m["seq"].as_i64())
                        .filter(|seq| *seq > cursor)
                        .collect(),
                );
                let mut rows = BTreeMap::new();
                for m in inbox["messages"]
                    .as_array()
                    .context("missing inbox messages")?
                    .iter()
                    .chain(
                        requests["requests"]
                            .as_array()
                            .context("missing requests")?,
                    )
                {
                    let id = m["id"].as_str().context("mail without id")?;
                    let seq = m["seq"].as_i64().context("mail without sequence")?;
                    let body = m["body"].as_str().unwrap_or("");
                    let preview = if body.is_empty() {
                        m["payload"].to_string()
                    } else {
                        body.into()
                    };
                    rows.insert(
                        seq,
                        Mail {
                            identity: identity.clone(),
                            origin: origin.clone(),
                            id: id.into(),
                            kind: m["kind"].as_str().unwrap_or("").into(),
                            from: m["from"].as_str().unwrap_or("").into(),
                            subject: subject_line(&preview),
                            body: body.into(),
                            payload: m["payload"].clone(),
                            status: m["state"].as_str().unwrap_or("unread").into(),
                            home: Some(home.clone()),
                            seq,
                            acknowledged: seq <= cursor,
                        },
                    );
                }
                self.read
                    .retain(|(read_home, seq)| read_home != &home || *seq > cursor);
                if readonly {
                    anyhow::ensure!(
                        identity == Some(super::bridge::Identity::read(&home)?),
                        "home replaced during mail observation"
                    );
                    self.valid
                        .insert(home.clone(), crate::store::Store::now_ms());
                } else {
                    self.ack_read(binary, &home)?;
                }
                Ok(rows.into_values().collect::<Vec<_>>())
            })();
            match result {
                Ok(rows) => messages.extend(rows),
                Err(e) => {
                    if readonly {
                        self.valid.remove(&home);
                        self.read.retain(|(h, _)| h != &home);
                        self.unread.remove(&home);
                    }
                    errors.push(format!("{}: {e}", home.display()));
                    messages.extend(
                        self.messages
                            .iter()
                            .filter(|m| m.home.as_ref() == Some(&home))
                            .cloned(),
                    );
                }
            }
            messages.sort_by_key(|m| {
                (
                    m.acknowledged
                        || m.home
                            .as_ref()
                            .is_some_and(|home| self.read.contains(&(home.clone(), m.seq))),
                    priority(m),
                    m.home.clone(),
                    m.seq,
                )
            });
            messages.truncate(1000);
        }
        if readonly {
            let mut origins = BTreeMap::<String, usize>::new();
            for (_, origin) in self.generations.values() {
                *origins.entry(origin.clone()).or_default() += 1;
            }
            for (home, (_, origin)) in &self.generations {
                if origins.get(origin).copied().unwrap_or(0) > 1 {
                    self.valid.remove(home);
                    self.read.retain(|(h, _)| h != home);
                    self.unread.remove(home);
                    errors.push(format!(
                        "{}: copied/ambiguous history origin; read intent refused",
                        home.display()
                    ));
                }
            }
        }
        messages.sort_by_key(|m| (priority(m), m.home.clone(), m.seq));
        self.messages = messages;
        self.errors = errors;
    }
    pub fn mark_read(&mut self, binary: &Path, mail: &Mail) -> Result<()> {
        let home = mail.home.as_ref().context("sample message has no home")?;
        self.read.insert((home.clone(), mail.seq));
        self.ack_read(binary, home)
    }
    pub fn mark_read_existing(
        &mut self,
        snapshot: &Snapshot,
        fallback: Option<&Path>,
        mail: &Mail,
    ) -> Result<()> {
        self.mark_read_existing_as(
            snapshot,
            fallback,
            mail,
            crate::store::Caller::from_env(None, None)?,
        )
    }
    fn mark_read_existing_as(
        &mut self,
        snapshot: &Snapshot,
        fallback: Option<&Path>,
        mail: &Mail,
        caller: crate::store::Caller,
    ) -> Result<()> {
        caller.require_operator("history read")?;
        let home = mail.home.as_ref().context("sample has no home")?;
        anyhow::ensure!(
            self.valid
                .get(home)
                .is_some_and(|at| crate::store::Store::now_ms() - at < super::bridge::FRESH_MS),
            "history observation failed/stale; refresh before reading"
        );
        anyhow::ensure!(
            self.generations
                .get(home)
                .is_some_and(
                    |(identity, origin)| mail.identity.as_ref() == Some(identity)
                        && mail.origin.as_ref() == Some(origin)
                ),
            "history generation changed; reselect"
        );
        let scope = super::bridge::homes(snapshot, fallback);
        anyhow::ensure!(scope.len() <= 200, "history scope exceeds bound");
        anyhow::ensure!(
            snapshot.presentation_error.is_none() && scope.get(home).is_some_and(Option::is_none),
            "history home retired/unavailable"
        );
        anyhow::ensure!(
            mail.identity.as_ref() == Some(&super::bridge::Identity::read(home)?),
            "history home replaced; reselect"
        );
        for other_home in scope.keys().filter(|h| *h != home) {
            let other = crate::store::Store::open_readonly(other_home, caller.clone())?;
            let origin: String = other.connection().query_row(
                "SELECT value FROM metadata WHERE key='runtime_origin'",
                [],
                |r| r.get(0),
            )?;
            anyhow::ensure!(
                mail.origin.as_ref() != Some(&origin),
                "copied history origin"
            );
        }
        let mut store = crate::store::Store::open_existing_action(home, caller)?;
        let origin: String = store.connection().query_row(
            "SELECT value FROM metadata WHERE key='runtime_origin'",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            mail.origin.as_ref() == Some(&origin),
            "history origin changed"
        );
        let oversized: bool=store.connection().query_row("SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1 AND (length(CAST(body AS BLOB))>65536 OR length(CAST(payload AS BLOB))>65536))",[&mail.id],|r|r.get(0))?;
        anyhow::ensure!(!oversized, "history selection exceeds source bound");
        let current = crate::mail::find(&store, &mail.id)?.context("history message missing")?;
        anyhow::ensure!(
            current["seq"] == mail.seq
                && current["from"] == mail.from
                && current["to"] == "captain",
            "history selection changed"
        );
        anyhow::ensure!(
            mail.identity.as_ref() == Some(&super::bridge::Identity::read(home)?),
            "history home replaced before acknowledgement"
        );
        let inbox = crate::dispatch(
            &mut store,
            "mail.inbox",
            json!({"address":"captain","limit":command::MAIL_PAGE_LIMIT}),
        )?;
        let cursor = inbox["acknowledged_through"]
            .as_i64()
            .context("missing cursor")?;
        if cursor >= mail.seq {
            return Ok(());
        }
        let current_unread: Vec<_> = inbox["messages"]
            .as_array()
            .context("missing inbox page")?
            .iter()
            .filter_map(|m| m["seq"].as_i64())
            .collect();
        anyhow::ensure!(
            self.unread.get(home) == Some(&current_unread),
            "mailbox prefix changed; refresh before acknowledgement"
        );
        self.read.insert((home.clone(), mail.seq));
        if let Some(seq) = self.read_prefix(home) {
            crate::dispatch(
                &mut store,
                "mail.ack",
                json!({"address":"captain","through_seq":seq}),
            )?;
        }
        Ok(())
    }
    fn read_prefix(&self, home: &Path) -> Option<i64> {
        let mut through = None;
        for seq in self.unread.get(home).into_iter().flatten() {
            if !self.read.contains(&(home.to_path_buf(), *seq)) {
                break;
            }
            through = Some(*seq);
        }
        through
    }
    fn ack_read(&self, binary: &Path, home: &Path) -> Result<()> {
        if let Some(seq) = self.read_prefix(home) {
            command::json(
                binary,
                home,
                "mail.ack",
                &json!({"address":"captain","through_seq":seq}),
            )?;
        }
        Ok(())
    }
}

fn subject_line(preview: &str) -> String {
    preview
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_history_observes_without_ack_and_explicit_read_preserves_prefix_and_identity() {
        use crate::store::{Caller, Store};
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let mut store = Store::open(&home).unwrap();
        crate::dispatch(
            &mut store,
            "ship.put",
            json!({"name":"apps","scope":"Apps"}),
        )
        .unwrap();
        for (id, kind) in [("first", "notice"), ("second", "decision")] {
            crate::dispatch(
                &mut store,
                "mail.send",
                json!({"id":id,"from":"apps","to":"captain","kind":kind,"body":"History"}),
            )
            .unwrap();
        }
        let snapshot = Snapshot::default();
        let mut denied = Mailbox::default();
        denied.refresh_mode(
            Path::new(""),
            &snapshot,
            Some(&home),
            Some(Caller::Agent {
                actor: "apps".into(),
            }),
        );
        assert!(denied.messages.is_empty() && !denied.errors.is_empty());
        let mut mailbox = Mailbox::default();
        mailbox.refresh_mode(
            Path::new(""),
            &snapshot,
            Some(&home),
            Some(Caller::Operator),
        );
        assert!(mailbox.errors.is_empty(), "{:?}", mailbox.errors);
        let first = mailbox
            .messages
            .iter()
            .find(|m| m.id == "first")
            .unwrap()
            .clone();
        let second = mailbox
            .messages
            .iter()
            .find(|m| m.id == "second")
            .unwrap()
            .clone();
        mailbox
            .mark_read_existing_as(&snapshot, Some(&home), &second, Caller::Operator)
            .unwrap();
        assert_eq!(
            crate::dispatch(&mut store, "mail.inbox", json!({"address":"captain"})).unwrap()["acknowledged_through"],
            0
        );
        mailbox.refresh_mode(
            Path::new(""),
            &snapshot,
            Some(&home),
            Some(Caller::Operator),
        );
        assert_eq!(
            crate::dispatch(&mut store, "mail.inbox", json!({"address":"captain"})).unwrap()["acknowledged_through"],
            0
        );
        mailbox
            .mark_read_existing_as(&snapshot, Some(&home), &first, Caller::Operator)
            .unwrap();
        assert_eq!(
            crate::dispatch(&mut store, "mail.inbox", json!({"address":"captain"})).unwrap()["acknowledged_through"],
            second.seq
        );
        assert_eq!(
            crate::mail::find(&store, "second").unwrap().unwrap()["state"],
            "open"
        );
        assert!(
            mailbox
                .mark_read_existing_as(
                    &snapshot,
                    Some(&home),
                    &first,
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
        let saved = root.path().join("saved");
        std::fs::rename(&home, &saved).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::copy(
            saved.join("privateer.sqlite"),
            home.join("privateer.sqlite"),
        )
        .unwrap();
        assert!(
            mailbox
                .mark_read_existing_as(&snapshot, Some(&home), &first, Caller::Operator)
                .is_err()
        );
        let missing = root.path().join("missing");
        mailbox.refresh_mode(
            Path::new(""),
            &snapshot,
            Some(&missing),
            Some(Caller::Operator),
        );
        assert!(!missing.exists());
        assert!(!mailbox.errors.is_empty());
    }

    #[test]
    fn subject_collapses_line_breaks_and_whitespace() {
        assert_eq!(
            subject_line("ready.\nShould\tbe  one\n\nline \n"),
            "ready. Should be one line"
        );
        assert_eq!(subject_line(&"x".repeat(150)).chars().count(), 100);
    }
}
