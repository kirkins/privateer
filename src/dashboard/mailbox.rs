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
}
impl Mailbox {
    pub fn refresh(&mut self, binary: &Path, snapshot: &Snapshot, fallback: Option<&Path>) {
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
        self.unread.retain(|home, _| homes.contains_key(home));
        self.read.retain(|(home, _)| homes.contains_key(home));
        for (home, error) in homes {
            let result = (|| -> Result<_> {
                anyhow::ensure!(error.is_none(), "{}", error.unwrap_or_default());
                let inbox = command::json(
                    binary,
                    &home,
                    "mail.inbox",
                    &json!({"address":"captain","limit":command::MAIL_PAGE_LIMIT}),
                )?;
                let requests = command::json(
                    binary,
                    &home,
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
                    rows.insert(
                        seq,
                        Mail {
                            id: id.into(),
                            kind: m["kind"].as_str().unwrap_or("").into(),
                            from: m["from"].as_str().unwrap_or("").into(),
                            subject: m["body"].as_str().unwrap_or("").chars().take(100).collect(),
                            body: m["body"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .take(2048)
                                .collect(),
                            status: m["state"].as_str().unwrap_or("unread").into(),
                            home: Some(home.clone()),
                            seq,
                            acknowledged: seq <= cursor,
                        },
                    );
                }
                self.read
                    .retain(|(read_home, seq)| read_home != &home || *seq > cursor);
                self.ack_read(binary, &home)?;
                Ok(rows.into_values().collect::<Vec<_>>())
            })();
            match result {
                Ok(rows) => messages.extend(rows),
                Err(e) => {
                    errors.push(format!("{}: {e}", home.display()));
                    messages.extend(
                        self.messages
                            .iter()
                            .filter(|m| m.home.as_ref() == Some(&home))
                            .cloned(),
                    );
                }
            }
        }
        messages.sort_by_key(|m| {
            (
                m.acknowledged
                    || m.home
                        .as_ref()
                        .is_some_and(|home| self.read.contains(&(home.clone(), m.seq))),
                m.home.clone(),
                m.seq,
            )
        });
        messages.truncate(1000);
        messages.sort_by_key(|m| {
            (
                match m.kind.as_str() {
                    "decision" => 0,
                    "request" => 1,
                    "reply" => 2,
                    _ => 3,
                },
                m.home.clone(),
                m.seq,
            )
        });
        self.messages = messages;
        self.errors = errors;
    }
    pub fn mark_read(&mut self, binary: &Path, mail: &Mail) -> Result<()> {
        let home = mail.home.as_ref().context("sample message has no home")?;
        self.read.insert((home.clone(), mail.seq));
        self.ack_read(binary, home)
    }
    fn ack_read(&self, binary: &Path, home: &Path) -> Result<()> {
        let mut through = None;
        for seq in self.unread.get(home).into_iter().flatten() {
            if !self.read.contains(&(home.to_path_buf(), *seq)) {
                break;
            }
            through = Some(*seq);
        }
        if let Some(seq) = through {
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
