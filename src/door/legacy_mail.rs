//! Mailbox selection for trusted v1 adapters. Scoped v2 routing is separate.

use super::{Session, members};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::path::PathBuf;

impl Session<'_> {
    pub(super) fn send_home(&mut self, to: &str, mail: &Value) -> Result<PathBuf> {
        let default = self.home(to)?;
        let Some(correlation) = mail["correlation"].as_str() else {
            return Ok(default);
        };
        let members = members()?;
        // A local ship's own home is authoritative. For a remote ship, its
        // captain conversation may have arrived in any local home, rather
        // than the current liaison's mailbox.
        if members.iter().any(|(ship, _)| ship == to) {
            return Ok(default);
        }
        let mut found = None;
        for (_, home) in members {
            let Some(original) = crate::mail::find(self.store(&home)?, correlation)? else {
                continue;
            };
            let captain = crate::mail::CAPTAIN;
            if !((original["from"] == to && original["to"] == captain)
                || (original["from"] == captain && original["to"] == to))
            {
                continue;
            }
            ensure!(
                found.as_ref().is_none_or(|previous| previous == &home),
                "correlated message {correlation} with {to} is held in multiple mailboxes"
            );
            found = Some(home);
        }
        // Preserve existing gateway behavior when no local copy is retained;
        // mail.send still validates the correlation and the answer itself.
        Ok(found.unwrap_or(default))
    }
}
