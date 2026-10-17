//! Optional captain-approved Pi and Claude resources. Selection is
//! deliberately NOT a harness option: first-mate resources never become crew
//! defaults.
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const KEY: &str = "pi_tool_bundle_registry_v1";

pub(crate) fn selection(value: &Value) -> Result<()> {
    let list = value.as_array().context("tool_bundles must be a list")?;
    ensure!(list.len() <= 16, "at most 16 tool bundles");
    let mut seen = std::collections::HashSet::new();
    for item in list {
        let item = item.as_str().context("tool bundle refs must be strings")?;
        ensure!(
            !item.is_empty()
                && item.len() <= 130
                && item
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-@._".contains(&b))
                && item.contains('@')
                && seen.insert(item),
            "invalid or duplicate tool bundle reference"
        );
    }
    Ok(())
}

fn load(store: &Store) -> Result<Value> {
    let raw: Option<String> = store
        .connection()
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    serde_json::from_str(
        &raw.context("bounded resources need an operator-approved bundle registry")?,
    )
    .context("invalid tool bundle registry")
}

/// Use the same bounded, redacting adapter protocol as harness planning. This
/// does not run a harness or read any native credential store.
fn qualify(registry: &Value, selected: Option<(&Value, &str, &str)>) -> Result<Value> {
    let manifest = crate::bundled::resolve("bundled:pi")?;
    let directory = manifest
        .parent()
        .context("Pi plugin directory")?
        .parent()
        .context("plugin directory")?;
    crate::plugins::bundle_request(directory, registry, selected)
}

pub(crate) fn resolve(store: &Store, selected: &Value, role: &str, harness: &str) -> Result<Value> {
    selection(selected)?;
    let result = qualify(&load(store)?, Some((selected, role, harness)))?;
    result
        .get("plan")
        .cloned()
        .context("bundle adapter omitted plan")
}

/// A bounded launch plan's identity: selected refs, pins and, for Claude, its
/// version and carried personal settings.
fn digest(plan: &Value) -> String {
    format!("{:x}", Sha256::digest(plan.to_string()))
}

/// The digest a bounded Claude first mate would launch under, to pick only a
/// session started under it. None when unbounded, for Pi (whose bounded runs
/// never resume), or when planning fails: the launch then refuses and says why.
pub(crate) fn session_digest(store: &Store, profile: &Value) -> Option<String> {
    let selected = profile.get("tool_bundles")?;
    (crate::activation::harness_id(profile) == "claude").then_some(())?;
    resolve(store, selected, "first_mate", "claude")
        .ok()
        .map(|plan| digest(&plan))
}

/// Record a bounded Claude launch's plan digest with its wake or errand, so
/// the session it starts carries it. A bounded resume continues only a
/// recorded first-mate session started under this same digest.
pub(crate) fn record_plan(
    store: &Store,
    id: &str,
    role: &str,
    plan: &Value,
    resume: Option<&Value>,
) -> Result<()> {
    let digest = digest(plan);
    if let Some(session) = resume {
        let started: bool = store.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1 AND plan_digest=?2)",
            params![session.as_str(), digest],
            |r| r.get(0),
        )?;
        ensure!(
            role == "first_mate" && started,
            "a bounded session resumes only under the plan it started with"
        );
    }
    store.connection().execute(
        "UPDATE wakes SET plan_digest=?2 WHERE job_id=?1 AND status='running'",
        params![id, digest],
    )?;
    store.connection().execute(
        "UPDATE errands SET plan_digest=?2 WHERE id=?1 AND status='running'",
        params![id, digest],
    )?;
    Ok(())
}

pub(crate) fn configure(store: &mut Store, args: Value) -> Result<Value> {
    store.caller().require_operator("bundle.configure")?;
    ensure!(
        args.as_object()
            .is_some_and(|v| v.len() == 1 && v.contains_key("registry")),
        "bundle.configure needs only registry"
    );
    let registry = &args["registry"];
    ensure!(
        serde_json::to_vec(registry)?.len() <= 48 * 1024,
        "bundle registry too large"
    );
    qualify(registry, None)?;
    // A name/version cannot be retargeted. Operators must publish a new ref
    // when content, path or roles change, even if an old ref was deselected.
    let tx = store.connection().unchecked_transaction()?;
    let previous: Option<String> = tx
        .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |r| {
            r.get(0)
        })
        .optional()?;
    let mut history: Value = match previous {
        Some(raw) => serde_json::from_str(&raw)?,
        None => json!({"bundles":[]}),
    };
    for old in history["bundles"]
        .as_array()
        .context("invalid bundle history")?
    {
        if let Some(new) = registry["bundles"]
            .as_array()
            .and_then(|list| list.iter().find(|b| b["ref"] == old["ref"]))
        {
            ensure!(
                old == new,
                "bundle refs are immutable; approve a new version"
            );
        }
    }
    // Retain tombstones separately: removing a ref cannot free its identity.
    let historical: Option<String> = tx
        .query_row(
            "SELECT value FROM metadata WHERE key='pi_tool_bundle_history_v1'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = historical {
        history = serde_json::from_str(&raw)?;
    }
    let old = history["bundles"]
        .as_array_mut()
        .context("invalid bundle history")?;
    for new in registry["bundles"].as_array().context("invalid bundles")? {
        if let Some(prior) = old.iter().find(|b| b["ref"] == new["ref"]) {
            ensure!(
                prior == new,
                "bundle refs are immutable; approve a new version"
            );
        } else {
            old.push(new.clone());
        }
    }
    ensure!(
        old.len() <= 256,
        "bundle history is full; use a new operational home"
    );
    tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [KEY, &registry.to_string()])?;
    tx.execute("INSERT INTO metadata(key,value) VALUES('pi_tool_bundle_history_v1',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [history.to_string()])?;
    tx.commit()?;
    // Audit identity/count only, never definitions, script content or private paths.
    let refs: Vec<_> = registry["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["ref"].clone())
        .collect();
    let result = json!({"configured":true,"pi_version":"1.2.0","refs":refs});
    store.audit("bundle.configure", &result)?;
    Ok(result)
}
