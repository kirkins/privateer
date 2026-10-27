//! Bounded linked runtime observations. Memory cache is grant/selector bound;
//! restart loses contact continuity. Persistent operator sections are dated
//! fallback facts only and always pass through the same closed projection.
use super::scoped::{DelegatedContext, Denial};
use crate::snapshot::scoped::RuntimeSection;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::Instant,
};

const CACHE_LIMIT: usize = 16;
pub(super) const FANOUT: usize = 8;
#[derive(Clone)]
struct Cached {
    section: RuntimeSection,
    received: i64,
    contact: Instant,
}
static CACHE: OnceLock<Mutex<BTreeMap<String, Cached>>> = OnceLock::new();

pub(super) struct Observation {
    pub section: Option<RuntimeSection>,
    pub mode: &'static str,
    pub state: &'static str,
    pub reason: &'static str,
    pub received: Option<i64>,
    pub age: Option<i64>,
    pub current: bool,
}
impl Observation {
    fn unknown(state: &'static str, reason: &'static str) -> Self {
        Self {
            section: None,
            mode: "directory",
            state,
            reason,
            received: None,
            age: None,
            current: false,
        }
    }
}

/// Dedicated gateway, not a command tunnel. No linked enumeration or calls.
pub(crate) fn serve(args: &Value) -> Result<Value> {
    ensure!(
        args.as_object().is_some_and(|o| o.contains_key("context")
            && o.contains_key("args")
            && o.keys()
                .all(|k| ["context", "args", "census"].contains(&k.as_str())))
            && args.get("census").is_none_or(|v| v == 1),
        Denial::Arguments
    );
    let c = DelegatedContext::from_gateway(&args["context"])?;
    ensure!(c.has("fleet:read"), Denial::Scope);
    super::runtime::validate_args(&args["args"]).map_err(|_| Denial::Arguments)?;
    ensure!(args["args"].get("machine").is_none(), Denial::Arguments);
    let conn = super::runtime::registry_readonly()?;
    let section = RuntimeSection::read_mode(
        &conn,
        &c,
        args["args"]["ship"].as_str(),
        args["census"] == 1,
    )?;
    Ok(serde_json::to_value(section)?)
}

fn key(name: &str, args: &Value, c: &DelegatedContext) -> String {
    let mut authority = c.gateway();
    authority.as_object_mut().unwrap().remove("result_id");
    crate::core::hash(&json!([
        crate::fleet::registry_path().ok(),
        name,
        args,
        authority
    ]))
}

pub(super) fn observe(name: &str, args: &Value, c: &DelegatedContext) -> Observation {
    let Ok(conn) = super::runtime::registry_readonly() else {
        return Observation::unknown("unreadable", "registry_unreadable");
    };
    let role: Option<String> = conn
        .query_row("SELECT role FROM machines WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten();
    // A grant-visible directory entry alone does not establish a callable link.
    if role.as_deref() != Some("collects") {
        return fallback(&conn, name, None, "reverse_only", "reverse_only");
    }
    let key = key(name, args, c);
    let previous = CACHE
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|m| m.get(&key).cloned());
    if let Some(p) = previous.as_ref().filter(|p| {
        p.contact.elapsed().as_millis() < crate::snapshot::REUSE_MS as u128
            && p.received <= crate::store::Store::now_ms()
    }) {
        return Observation {
            section: Some(p.section.clone()),
            mode: "reused",
            state: "covered",
            reason: "reused_contact",
            received: Some(p.received),
            age: Some(p.contact.elapsed().as_millis() as i64),
            current: true,
        };
    }
    let answer = crate::machines::runtime_of(name, args, c).and_then(|v| {
        let section: RuntimeSection = serde_json::from_value(v)?;
        section.validate()?;
        Ok(section)
    });
    match answer {
        Ok(section) => {
            let received = crate::store::Store::now_ms();
            let cached = Cached {
                section: section.clone(),
                received,
                contact: Instant::now(),
            };
            if let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() {
                if cache.len() == CACHE_LIMIT && !cache.contains_key(&key) {
                    if let Some(oldest) = cache
                        .iter()
                        .min_by_key(|(_, v)| v.contact)
                        .map(|(k, _)| k.clone())
                    {
                        cache.remove(&oldest);
                    }
                }
                cache.insert(key, cached);
            }
            Observation {
                section: Some(section),
                mode: "remote_live",
                state: "covered",
                reason: "recorded_state_only",
                received: Some(received),
                age: Some(0),
                current: true,
            }
        }
        Err(error) => {
            let unsupported = error.to_string()
                == format!("machine {name}: unknown gateway command fleet.runtime.v1");
            fallback(
                &conn,
                name,
                previous,
                if unsupported {
                    "unsupported"
                } else {
                    "unavailable"
                },
                if unsupported {
                    "upgrade_peer"
                } else {
                    "contact_failed"
                },
            )
        }
    }
}

fn fallback(
    conn: &Connection,
    name: &str,
    previous: Option<Cached>,
    state: &'static str,
    reason: &'static str,
) -> Observation {
    if let Some(previous) = previous {
        let age = (previous.received <= crate::store::Store::now_ms())
            .then(|| previous.contact.elapsed().as_millis() as i64);
        return Observation {
            section: Some(previous.section),
            mode: "cache",
            state: "cached",
            reason,
            received: Some(previous.received),
            age,
            current: false,
        };
    }
    match crate::snapshot::stored(conn, name).ok().flatten() {
        Some(stored) => {
            let section = RuntimeSection {
                schema: "runtime.v1".into(),
                observed_at: stored.as_of_ms,
                ships: stored.section["ships"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(200)
                    .filter_map(crate::snapshot::scoped::from_operator)
                    .collect(),
                truncated: stored.section["machine"]["truncated"] == true,
            };
            Observation {
                section: Some(section),
                mode: if stored.via == "collect" {
                    "carried"
                } else {
                    "cache"
                },
                state: "cached",
                reason,
                received: stored.received_ms,
                age: None,
                current: false,
            }
        }
        None => Observation::unknown(state, reason),
    }
}
