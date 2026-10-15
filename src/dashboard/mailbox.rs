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
        let mut homes: BTreeSet<PathBuf> = snapshot
            .ships
            .iter()
            .filter_map(|s| {
                s.live
                    .as_ref()
                    .filter(|l| l.error.is_none())
                    .map(|l| l.home.clone())
            })
            .collect();
        if homes.is_empty() {
            if let Some(home) = fallback {
                homes.insert(home.to_path_buf());
            }
        }
        let mut messages = Vec::new();
        let mut errors = Vec::new();
        for home in homes {
            let result = (|| -> Result<_> {
                let inbox = command::json(
                    binary,
                    &home,
                    "mail.inbox",
                    &json!({"address":"captain","limit":200}),
                )?;
                let requests = command::json(
                    binary,
                    &home,
                    "mail.requests",
                    &json!({"address":"captain","state":"open","limit":200}),
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
        messages.truncate(1000);
        self.messages = messages;
        self.errors = errors;
        self.read.retain(|(home, seq)| {
            self.messages
                .iter()
                .any(|m| m.home.as_ref() == Some(home) && m.seq == *seq && !m.acknowledged)
        });
    }
    pub fn mark_read(&mut self, binary: &Path, mail: &Mail) -> Result<()> {
        let home = mail.home.as_ref().context("sample message has no home")?;
        self.read.insert((home.clone(), mail.seq));
        let mut through = None;
        for seq in self.unread.get(home).into_iter().flatten() {
            if !self.read.contains(&(home.clone(), *seq)) {
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
