//! Workspace presentation reconciliation. Clients are short-lived (one per
//! host pass), so the bounded rate limit is shared across clients, not streams.
//! Herdr's observed tokens are the durable source of truth: this also recovers
//! lost metadata after a restart without periodic unchanged reports.
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_WORKSPACES: usize = 1024;

type Key = (PathBuf, (u64, u64), String);

struct Report {
    attempted: Instant,
    sent: Option<Value>,
}

#[derive(Default)]
struct Reports(HashMap<Key, Report>);

impl Reports {
    fn reserve(&mut self, key: Key, tokens: &Value, observed: &Value, now: Instant) -> bool {
        // Other metadata sources may contribute tokens too. Compare only the
        // meaningful fields we own, not their timestamps or presentation data.
        if tokens.as_object().is_some_and(|wanted| {
            wanted
                .iter()
                .all(|(key, value)| observed.get(key) == Some(value))
        }) {
            return false;
        }
        if self
            .0
            .get(&key)
            .is_some_and(|last| now.saturating_duration_since(last.attempted) < MIN_INTERVAL)
        {
            return false;
        }
        if !self.0.contains_key(&key) && self.0.len() >= MAX_WORKSPACES {
            // Never evict an active reservation: concurrent callers must not
            // bypass the interval. Old socket incarnations age out this way.
            let oldest = self
                .0
                .iter()
                .filter(|(_, report)| {
                    now.saturating_duration_since(report.attempted) >= MIN_INTERVAL
                })
                .min_by_key(|(_, report)| report.attempted)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { return false };
            self.0.remove(&oldest);
        }
        self.0
            .entry(key)
            .and_modify(|last| last.attempted = now)
            .or_insert(Report {
                attempted: now,
                sent: None,
            });
        true
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;

fn reports() -> &'static Mutex<Reports> {
    static REPORTS: OnceLock<Mutex<Reports>> = OnceLock::new();
    REPORTS.get_or_init(Mutex::default)
}

impl super::Client {
    pub(super) fn tag_tokens_at(
        &mut self,
        workspace: &str,
        tokens: Value,
        now: Instant,
    ) -> anyhow::Result<()> {
        let listed = self.request("workspace.list", serde_json::json!({}))?;
        let spaces = listed["workspaces"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("workspace list missing"))?;
        // A missing workspace is not authorization to recreate or tag it.
        let Some(space) = spaces
            .iter()
            .find(|space| space["workspace_id"] == workspace)
        else {
            return Ok(());
        };
        let key = (self.path.clone(), self.identity, workspace.to_owned());
        if !reports().lock().unwrap_or_else(|p| p.into_inner()).reserve(
            key.clone(),
            &tokens,
            &space["tokens"],
            now,
        ) {
            return Ok(());
        }
        // Reserve before I/O, but never hold the shared lock over a socket
        // request. Failed/unknown outcomes are rate-limited too, not cached as
        // successes. The next host pass supplies the latest desired value.
        self.request(
            "workspace.report_metadata",
            serde_json::json!({
                "workspace_id":workspace,"source":"privateer","tokens":tokens
            }),
        )?;
        if let Some(report) = reports()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .get_mut(&key)
        {
            report.sent = Some(tokens);
        }
        Ok(())
    }
}
