//! Bounded linked runtime observations. Memory cache is link/grant/selector bound;
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
    binding: Option<String>,
    pub section: Option<RuntimeSection>,
    pub mode: &'static str,
    pub state: &'static str,
    pub reason: &'static str,
    pub received: Option<i64>,
    pub age: Option<i64>,
    pub current: bool,
    attempted: bool,
    successful: bool,
}
impl Observation {
    fn unknown(state: &'static str, reason: &'static str) -> Self {
        Self {
            binding: None,
            section: None,
            mode: "directory",
            state,
            reason,
            received: None,
            age: None,
            current: false,
            attempted: false,
            successful: false,
        }
    }

    /// Contact outcome is independent of any fallback facts. Never expose raw
    /// transport errors, endpoint identities, cache keys or link incarnations.
    pub(super) fn contact(&self, machine: &str) -> Value {
        json!({"machine":machine,"attempted":self.attempted,
            "successful":self.successful,"failed":self.attempted && !self.successful,
            "reused":self.mode == "reused","reverse_only":self.reason == "reverse_only",
            "reason":self.reason,"provenance":match self.mode {
                "remote_live" => "fresh", "reused" | "cache" => "cached",
                "carried" => "carried", _ => "directory_only",
            },"contact_age_ms":self.age,"received_at":self.received})
    }

    pub(super) fn recheck(&mut self, conn: &Connection, name: &str) {
        if self
            .binding
            .as_deref()
            .is_some_and(|binding| !crate::machines::same_incarnation(conn, name, binding))
        {
            let attempted = self.attempted;
            *self = Self::unknown("unavailable", "link_changed");
            self.attempted = attempted;
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
            && args.get("census").is_none_or(|v| v == 1 || v == 2),
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
        args.get("census").and_then(Value::as_u64),
    )?;
    Ok(serde_json::to_value(section)?)
}

fn key(name: &str, incarnation: &str, args: &Value, c: &DelegatedContext) -> String {
    let mut authority = c.gateway();
    authority.as_object_mut().unwrap().remove("result_id");
    crate::core::hash(&json!([
        crate::fleet::registry_path().ok(),
        name,
        incarnation,
        args,
        authority
    ]))
}

pub(super) fn observe(name: &str, args: &Value, c: &DelegatedContext) -> Observation {
    let Ok(conn) = super::runtime::registry_readonly() else {
        return Observation::unknown("unreadable", "registry_unreadable");
    };
    let Ok(incarnation) = crate::machines::link_incarnation(&conn, name) else {
        return Observation::unknown("unavailable", "link_changed");
    };
    let mut observation = observe_bound(&conn, name, &incarnation, args, c);
    observation.binding = Some(incarnation);
    observation.recheck(&conn, name);
    observation
}

fn observe_bound(
    conn: &Connection,
    name: &str,
    incarnation: &str,
    args: &Value,
    c: &DelegatedContext,
) -> Observation {
    let role: Option<String> = conn
        .query_row("SELECT role FROM machines WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten();
    // A grant-visible directory entry alone does not establish a callable link.
    if role.as_deref() != Some("collects") {
        return fallback(
            conn,
            name,
            incarnation,
            None,
            "reverse_only",
            "reverse_only",
        );
    }
    let key = key(name, incarnation, args, c);
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
            binding: None,
            section: Some(p.section.clone()),
            mode: "reused",
            state: "covered",
            reason: "reused_contact",
            received: Some(p.received),
            age: Some(p.contact.elapsed().as_millis() as i64),
            current: true,
            attempted: false,
            successful: false,
        };
    }
    let answer = crate::machines::runtime_of(name, incarnation, args, c).and_then(|v| {
        let section: RuntimeSection = serde_json::from_value(v)?;
        section.validate()?;
        Ok(section)
    });
    match answer {
        Ok(section) => {
            if !crate::machines::same_incarnation(conn, name, incarnation) {
                let mut lost = Observation::unknown("unavailable", "link_changed");
                lost.attempted = true;
                return lost;
            }
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
                binding: None,
                section: Some(section),
                mode: "remote_live",
                state: "covered",
                reason: "recorded_state_only",
                received: Some(received),
                age: Some(0),
                current: true,
                attempted: true,
                successful: true,
            }
        }
        Err(error) => {
            let unsupported = error.to_string()
                == format!("machine {name}: unknown gateway command fleet.runtime.v1");
            let mut failed = fallback(
                conn,
                name,
                incarnation,
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
            );
            failed.attempted = true;
            failed
        }
    }
}

fn fallback(
    conn: &Connection,
    name: &str,
    incarnation: &str,
    previous: Option<Cached>,
    state: &'static str,
    reason: &'static str,
) -> Observation {
    if !crate::machines::same_incarnation(conn, name, incarnation) {
        return Observation::unknown("unavailable", "link_changed");
    }
    if let Some(previous) = previous {
        let age = (previous.received <= crate::store::Store::now_ms())
            .then(|| previous.contact.elapsed().as_millis() as i64);
        return Observation {
            binding: None,
            section: Some(previous.section),
            mode: "cache",
            state: "cached",
            reason,
            received: Some(previous.received),
            age,
            current: false,
            attempted: false,
            successful: false,
        };
    }
    match crate::snapshot::stored_for(conn, name, incarnation)
        .ok()
        .flatten()
    {
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
                binding: None,
                section: Some(section),
                mode: if stored.via == "collect" {
                    "carried"
                } else {
                    "cache"
                },
                state: "cached",
                reason,
                received: stored.received_ms,
                // A persisted receipt has no monotonic clock continuity after
                // restart. Preserve its date, but never invent a contact age.
                age: None,
                current: false,
                attempted: false,
                successful: false,
            }
        }
        None => Observation::unknown(state, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_partition_and_fallback_custody_follow_the_link_not_its_label() {
        let conn = Connection::open_in_memory().unwrap();
        crate::machines::init(&conn).unwrap();
        conn.execute("INSERT INTO machines(name,role,peer,added_ms,incarnation) VALUES('beta','collects','{}',0,'alpha')", []).unwrap();
        let alpha = crate::machines::link_incarnation(&conn, "beta").unwrap();
        let c = DelegatedContext::resolved(
            "alpha".into(),
            "door:alpha".into(),
            vec!["fleet:read".into()],
            None,
        );
        let args = json!({"schema":"runtime.v1", "ship":"survey"});
        let first = key("beta", &alpha, &args, &c);
        assert_eq!(first, key("beta", &alpha, &args, &c));
        let section = RuntimeSection {
            schema: "runtime.v1".into(),
            observed_at: 10,
            ships: vec![],
            truncated: false,
        };
        let cached = Cached {
            section: section.clone(),
            received: 10,
            contact: Instant::now(),
        };
        let same = fallback(
            &conn,
            "beta",
            &alpha,
            Some(cached.clone()),
            "unavailable",
            "contact_failed",
        );
        assert!(same.section.is_some());
        let mut admitted = same;
        admitted.binding = Some(alpha.clone());
        admitted.attempted = true;
        admitted.successful = true;
        conn.execute(
            "UPDATE machines SET incarnation='gamma' WHERE name='beta'",
            [],
        )
        .unwrap();
        let gamma = crate::machines::link_incarnation(&conn, "beta").unwrap();
        assert_ne!(first, key("beta", &gamma, &args, &c));
        let late = fallback(
            &conn,
            "beta",
            &alpha,
            Some(cached),
            "unavailable",
            "contact_failed",
        );
        assert!(late.section.is_none());
        admitted.recheck(&conn, "beta");
        let contact = admitted.contact("beta");
        assert_eq!(contact["attempted"], true);
        assert_eq!(contact["successful"], false);
        assert_eq!(contact["failed"], true);
        assert_eq!(contact["reason"], "link_changed");
        assert!(contact["contact_age_ms"].is_null());
        assert!(
            admitted.section.is_none(),
            "replacement before publication discards admitted facts"
        );
    }
}
