//! Versioned coverage contract (`coverage.v1`) for scoped door results.
//!
//! Every scoped result that reports mail or waiting coverage carries one
//! `coverage` object of this schema, so a reader never has to guess what a
//! result covered. Scopes keep their meanings apart:
//! - `selected-mailbox`: one mailbox, here or on a reachable peer;
//! - `local`: this machine's visible homes;
//! - `operation-only`: an operation's own outcome, not its inputs;
//! - `desk-domain`: an operator-configured coverage domain.
//!
//! `complete:true` is always relative to its scope and is never fleet-wide.
//! A desk-domain result is complete only under the rule in `domain_complete`;
//! nothing in this phase ingests sources, so no served result reaches it.
//!
//! Domain manifests and source identity/alias records are core-owned and
//! filtered by the admitted grant before they leave the core: hidden ships,
//! their sources and their counts never appear. Linked machines share operator
//! trust: a linked peer can assert any origin, alias or coverage, so these
//! records are bookkeeping, not authentication of the machine that wrote them.
// The manifest and alias records are the contract later intake, placement and
// reader work builds on; this phase defines and tests them without serving them.
#![cfg_attr(not(test), allow(dead_code))]
use super::scoped::DelegatedContext;
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

pub(crate) const SCHEMA: &str = "coverage.v1";
/// Push-only sources count as fresh this long after their last completed
/// contact unless the operator sets another bound for the domain.
pub(crate) const DEFAULT_MAX_SOURCE_AGE_MS: i64 = 15 * 60 * 1000;
const MAX_SOURCE_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const SOURCE_STATES: [&str; 11] = [
    "covered",
    "cached",
    "unavailable",
    "reverse_only",
    "unsupported",
    "history_gap",
    "unreadable",
    "backpressure",
    "handoff_pending",
    "stale_generation",
    "ambiguous_owner",
];

/// One selected mailbox; other machines were not queried.
pub(super) fn selected_mailbox(machine: &str, state: &str, retry: Option<&str>) -> Value {
    let complete = state == "complete";
    checked(
        json!({"schema":SCHEMA,"scope":"selected-mailbox","complete":complete,
        "machines":[{"machine":machine,"state":state,"complete":complete,"retry":retry}],
        "other_machines":"not_queried"}),
    )
}

/// Coverage of visible homes on this machine; no peer was queried.
pub(crate) fn local(complete: bool) -> Value {
    checked(json!({"schema":SCHEMA,"scope":"local","complete":complete,
        "other_machines":"not_queried"}))
}

/// An operation completed; it says nothing about mailbox coverage.
pub(super) fn operation_only(operation: &str) -> Value {
    checked(
        json!({"schema":SCHEMA,"scope":"operation-only","complete":true,
        "operation":operation,"mailboxes":"not_queried"}),
    )
}

/// A desk operation's own outcome, whatever coverage an older desk reported.
pub(super) fn stamp_operation(result: &mut Value, operation: &str) {
    result["coverage"] = operation_only(operation);
}

/// The same answer for a domain that does not exist and for one whose every
/// source is hidden from this grant.
pub(crate) fn unknown_domain() -> Value {
    checked(
        json!({"schema":SCHEMA,"scope":"desk-domain","complete":false,
        "state":"unknown_domain","domain":null,"manifest_revision":null,"owner":null,
        "snapshot":null,"sources":[],"other_machines":"not_queried"}),
    )
}

pub(crate) struct Owner {
    pub id: String,
    pub generation: u64,
    pub incarnation: String,
    pub state: &'static str,
}

pub(crate) struct Barrier {
    pub mode: &'static str,
    pub received_at: i64,
    pub age_ms: i64,
    pub max_source_age_ms: i64,
}

pub(crate) struct Source {
    pub id: String,
    pub state: &'static str,
    pub through: Option<String>,
    pub barrier: Option<Barrier>,
}

pub(crate) struct Domain {
    pub domain: String,
    pub max_source_age_ms: i64,
    pub manifest_revision: Option<i64>,
    pub owner: Option<Owner>,
    pub snapshot: Option<String>,
    pub sources: Vec<Source>,
    pub retry: Option<(&'static str, i64)>,
}

/// Desk-domain coverage; completeness is derived, never supplied.
pub(crate) fn desk_domain(d: &Domain) -> Value {
    let sources: Vec<Value> = d
        .sources
        .iter()
        .map(|s| {
            let mut source = json!({"id":s.id,"state":s.state,"through":s.through});
            if let Some(b) = &s.barrier {
                source["barrier_mode"] = json!(b.mode);
                source["received_at"] = json!(b.received_at);
                source["age_ms"] = json!(b.age_ms);
                source["max_source_age_ms"] = json!(b.max_source_age_ms);
            }
            source
        })
        .collect();
    let mut coverage = json!({"schema":SCHEMA,"scope":"desk-domain","complete":false,
        "domain":d.domain,"manifest_revision":d.manifest_revision,
        "max_source_age_ms":d.max_source_age_ms,
        "owner":d.owner.as_ref().map(|o| json!({"id":o.id,"generation":o.generation,
            "incarnation":o.incarnation,"state":o.state})),
        "snapshot":d.snapshot,"sources":sources,"other_machines":"not_queried"});
    if let Some((action, after_ms)) = d.retry {
        coverage["retry"] = json!({"action":action,"after_ms":after_ms});
    }
    coverage["complete"] = json!(domain_complete(&coverage));
    checked(coverage)
}

/// Runtime coverage reuses source identity/state and freshness fields without
/// inventing a desk owner or inheriting mail's push freshness policy.
#[derive(Clone, Copy)]
pub(crate) enum RuntimeFacet {
    Inventory,
    Workers,
    Sessions,
    Jobs,
    Work,
    Capacity,
    Needs,
    Mail,
}
impl RuntimeFacet {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Workers => "workers",
            Self::Sessions => "sessions",
            Self::Jobs => "jobs",
            Self::Work => "work",
            Self::Capacity => "capacity",
            Self::Needs => "needs",
            Self::Mail => "mail",
        }
    }
}
pub(crate) struct RuntimeSource {
    pub source: Source,
    pub machine: String,
    pub ship: Option<String>,
    pub facet: RuntimeFacet,
    pub mode: &'static str,
    pub observed_at: Option<i64>,
    pub received_at: Option<i64>,
    pub age_ms: Option<i64>,
    pub reason: &'static str,
}

/// V1 has no configured runtime manifest and cannot claim domain completeness.
/// Cached/directory facts never contribute to current activity totals.
pub(crate) fn runtime_domain(sources: &[RuntimeSource], inventory_complete: bool) -> Value {
    checked(
        json!({"schema":SCHEMA,"scope":"runtime-domain","complete":false,
        "domain":null,"state":"unknown_domain","inventory_complete":inventory_complete,
        "sources":sources.iter().map(|s| json!({"id":s.source.id,"state":s.source.state,
            "machine":s.machine,"ship":s.ship,"facet":s.facet.as_str(),"mode":s.mode,
            "observed_at":s.observed_at,"received_at":s.received_at,
            "age_ms":s.age_ms,
            "barrier_mode":if matches!(s.mode,"remote_live"|"reused") { Some("on_demand") } else { None },
            "max_source_age_ms":crate::snapshot::REUSE_MS,
            "reason":s.reason})).collect::<Vec<_>>(),"other_machines":"not_queried"}),
    )
}

fn validate_runtime(map: &Map<String, Value>, complete: bool) -> Result<()> {
    ensure!(
        !complete
            && map.get("domain") == Some(&Value::Null)
            && map.get("state") == Some(&json!("unknown_domain")),
        "runtime domain not established"
    );
    ensure!(
        map.get("inventory_complete").is_some_and(Value::is_boolean),
        "inventory completeness missing"
    );
    ensure!(
        map.get("other_machines") == Some(&json!("not_queried")),
        "other machines must be not_queried"
    );
    for source in map
        .get("sources")
        .and_then(Value::as_array)
        .context("runtime sources missing")?
    {
        let s = source
            .as_object()
            .context("runtime source must be an object")?;
        ensure!(s.len() == 12, "unexpected runtime source fields");
        ensure!(
            s.get("max_source_age_ms") == Some(&json!(crate::snapshot::REUSE_MS)),
            "invalid runtime freshness bound"
        );
        ensure!(
            s.get("barrier_mode")
                .is_some_and(|v| v.is_null() || v == "on_demand"),
            "invalid runtime barrier"
        );
        for field in ["id", "machine", "reason"] {
            ensure!(
                s.get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "runtime source field missing"
            );
        }
        ensure!(
            s.get("ship").is_some_and(|v| v.is_null() || v.is_string()),
            "invalid runtime ship"
        );
        ensure!(
            s.get("state")
                .and_then(Value::as_str)
                .is_some_and(|v| SOURCE_STATES.contains(&v)),
            "invalid runtime state"
        );
        ensure!(
            s.get("facet").and_then(Value::as_str).is_some_and(|v| [
                "inventory",
                "workers",
                "sessions",
                "jobs",
                "work",
                "capacity",
                "needs",
                "mail"
            ]
            .contains(&v)),
            "invalid runtime facet"
        );
        ensure!(
            s.get("mode").and_then(Value::as_str).is_some_and(|v| [
                "local_live",
                "remote_live",
                "reused",
                "cache",
                "carried",
                "directory"
            ]
            .contains(&v)),
            "invalid runtime mode"
        );
        for field in ["observed_at", "received_at", "age_ms"] {
            ensure!(
                s.get(field)
                    .is_some_and(|v| v.is_null() || v.as_i64().is_some_and(|n| n >= 0)),
                "invalid runtime time"
            );
        }
        ensure!(
            s["mode"] != "local_live" || s.get("observed_at") == s.get("received_at"),
            "local receipt disagrees"
        );
        ensure!(
            !s["received_at"].is_null() || s["age_ms"].is_null(),
            "undated runtime age"
        );
        ensure!(
            s["mode"] != "directory" || (s["observed_at"].is_null() && s["state"] != "covered"),
            "directory is not live evidence"
        );
    }
    Ok(())
}

/// Complete only with an active owner, a snapshot and every visible source
/// covered through a barrier: taken on demand, or the last completed push
/// contact within its bound. The vector is complete only as of that snapshot.
fn domain_complete(coverage: &Value) -> bool {
    let fresh = |s: &Value| {
        s["state"] == "covered"
            && s["through"].is_string()
            && match s["barrier_mode"].as_str() {
                Some("on_demand") => true,
                Some("last_received_push") => matches!(
                    (s["age_ms"].as_i64(), coverage["max_source_age_ms"].as_i64()),
                    (Some(age), Some(max)) if (0..=max).contains(&age)
                ),
                _ => false,
            }
    };
    coverage["owner"]["state"] == "active"
        && coverage["snapshot"].is_string()
        && coverage["sources"]
            .as_array()
            .is_some_and(|s| !s.is_empty() && s.iter().all(fresh))
}

fn checked(coverage: Value) -> Value {
    debug_assert!(
        validate(&coverage, &[]).is_ok(),
        "invalid coverage {coverage}"
    );
    coverage
}

/// Validate a coverage object against `coverage.v1`, and that it names none
/// of `hidden` (ships outside the grant's allowed set) anywhere.
pub(crate) fn validate(coverage: &Value, hidden: &[&str]) -> Result<()> {
    let map = coverage.as_object().context("coverage is not an object")?;
    ensure!(
        map.get("schema").and_then(Value::as_str) == Some(SCHEMA),
        "unsupported coverage schema"
    );
    let scope = map
        .get("scope")
        .and_then(Value::as_str)
        .context("coverage scope missing")?;
    let complete = map
        .get("complete")
        .and_then(Value::as_bool)
        .context("coverage completeness missing")?;
    let fields: &[&str] = match scope {
        "selected-mailbox" => &["machines", "other_machines"],
        "local" => &["other_machines", "remote", "reasons"],
        "operation-only" => &["operation", "mailboxes"],
        "runtime-domain" => &[
            "domain",
            "state",
            "inventory_complete",
            "sources",
            "other_machines",
        ],
        "desk-domain" => &[
            "state",
            "domain",
            "manifest_revision",
            "max_source_age_ms",
            "owner",
            "snapshot",
            "sources",
            "other_machines",
            "retry",
        ],
        _ => bail!("unknown coverage scope"),
    };
    if let Some(key) = map.keys().find(|k| {
        !["schema", "scope", "complete"].contains(&k.as_str()) && !fields.contains(&k.as_str())
    }) {
        bail!("unexpected coverage field {key}");
    }
    let text = |key: &str| map.get(key).and_then(Value::as_str);
    match scope {
        "selected-mailbox" => {
            ensure!(
                text("other_machines") == Some("not_queried"),
                "other machines must be not_queried"
            );
            let machines = map
                .get("machines")
                .and_then(Value::as_array)
                .context("machines missing")?;
            ensure!(machines.len() == 1, "a selected mailbox has one machine");
            let m = machines[0]
                .as_object()
                .context("machine is not an object")?;
            ensure!(m.len() == 4, "unexpected machine fields");
            ensure!(
                m.get("machine").is_some_and(Value::is_string),
                "machine missing"
            );
            let state = m
                .get("state")
                .and_then(Value::as_str)
                .context("state missing")?;
            ensure!(
                ["complete", "unavailable", "unsupported"].contains(&state),
                "unknown mailbox state"
            );
            ensure!(
                m.get("complete") == Some(&json!(state == "complete")),
                "machine completeness disagrees"
            );
            ensure!(
                m.get("retry").is_some_and(|r| r.is_null() || r.is_string()),
                "retry must be a string or null"
            );
            ensure!(
                complete == (state == "complete"),
                "completeness disagrees with the mailbox"
            );
        }
        "local" => {
            ensure!(
                text("other_machines") == Some("not_queried"),
                "other machines must be not_queried"
            );
            ensure!(
                map.get("remote").is_none_or(|r| r == "unavailable"),
                "remote must be unavailable"
            );
            if let Some(reasons) = map.get("reasons") {
                let reasons = reasons.as_array().context("invalid local reasons")?;
                ensure!(
                    !complete
                        && !reasons.is_empty()
                        && reasons.iter().all(|r| {
                            r.as_str().is_some_and(|r| {
                                ["missing_home", "request_scan_limit", "waiting_limit"].contains(&r)
                            })
                        }),
                    "invalid local incompleteness reasons"
                );
            }
        }
        "operation-only" => {
            ensure!(
                text("operation").is_some_and(|o| !o.is_empty()),
                "operation missing"
            );
            ensure!(
                text("mailboxes") == Some("not_queried"),
                "mailboxes must be not_queried"
            );
        }
        "runtime-domain" => validate_runtime(map, complete)?,
        _ => validate_domain(map, complete)?,
    }
    ensure!(
        hidden.is_empty() || !mentions(coverage, hidden),
        "coverage names a hidden ship"
    );
    Ok(())
}

fn validate_domain(map: &Map<String, Value>, complete: bool) -> Result<()> {
    ensure!(
        map.get("other_machines") == Some(&json!("not_queried")),
        "other machines must be not_queried"
    );
    let sources = map
        .get("sources")
        .and_then(Value::as_array)
        .context("sources missing")?;
    let optional = |key: &str, check: fn(&Value) -> bool| {
        map.get(key).is_some_and(|v| v.is_null() || check(v))
    };
    ensure!(
        optional("manifest_revision", |v| v.as_i64().is_some_and(|r| r > 0)),
        "invalid manifest revision"
    );
    ensure!(optional("snapshot", Value::is_string), "invalid snapshot");
    if map.get("domain").is_some_and(Value::is_null) {
        ensure!(
            map.get("state") == Some(&json!("unknown_domain")),
            "a missing domain must be unknown_domain"
        );
        ensure!(
            !complete && sources.is_empty(),
            "an unknown domain covers nothing"
        );
        ensure!(
            ["owner", "manifest_revision", "snapshot"]
                .iter()
                .all(|k| map.get(*k).is_some_and(Value::is_null)),
            "an unknown domain discloses nothing"
        );
    } else {
        ensure!(
            map.get("domain").is_some_and(Value::is_string),
            "domain missing"
        );
        ensure!(
            map.get("max_source_age_ms")
                .and_then(Value::as_i64)
                .is_some_and(|max| (1..=MAX_SOURCE_AGE_MS).contains(&max)),
            "invalid domain freshness bound"
        );
        ensure!(
            map.get("state").is_none(),
            "a known domain has no domain state"
        );
    }
    if let Some(owner) = map.get("owner").filter(|o| !o.is_null()) {
        let o = owner.as_object().context("owner is not an object")?;
        ensure!(o.len() == 4, "unexpected owner fields");
        ensure!(
            o.get("id").is_some_and(Value::is_string),
            "owner id missing"
        );
        ensure!(
            o.get("generation")
                .and_then(Value::as_u64)
                .is_some_and(|g| g > 0),
            "owner generation missing"
        );
        ensure!(
            o.get("incarnation").is_some_and(Value::is_string),
            "owner incarnation missing"
        );
        ensure!(
            o.get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| ["active", "staging", "fenced"].contains(&s)),
            "unknown owner state"
        );
    } else {
        ensure!(map.contains_key("owner"), "owner missing");
    }
    for source in sources {
        let s = source.as_object().context("source is not an object")?;
        for key in s.keys() {
            ensure!(
                [
                    "id",
                    "state",
                    "through",
                    "barrier_mode",
                    "received_at",
                    "age_ms",
                    "max_source_age_ms"
                ]
                .contains(&key.as_str()),
                "unexpected source field {key}"
            );
        }
        ensure!(
            s.get("id").is_some_and(Value::is_string),
            "source id missing"
        );
        ensure!(
            s.get("state")
                .and_then(Value::as_str)
                .is_some_and(|st| SOURCE_STATES.contains(&st)),
            "unknown source state"
        );
        ensure!(
            s.get("through")
                .is_some_and(|t| t.is_null() || t.is_string()),
            "invalid source checkpoint"
        );
        if let Some(mode) = s.get("barrier_mode") {
            ensure!(
                ["on_demand", "last_received_push"]
                    .iter()
                    .any(|m| mode == m),
                "unknown barrier mode"
            );
            for key in ["received_at", "age_ms", "max_source_age_ms"] {
                ensure!(
                    s.get(key).and_then(Value::as_i64).is_some(),
                    "barrier {key} missing"
                );
            }
        } else {
            ensure!(s.len() == 3, "barrier fields need a barrier mode");
        }
    }
    if let Some(retry) = map.get("retry") {
        let r = retry.as_object().context("retry is not an object")?;
        ensure!(
            r.len() == 2
                && r.get("action").is_some_and(Value::is_string)
                && r.get("after_ms").and_then(Value::as_i64).is_some(),
            "invalid retry"
        );
    }
    ensure!(
        !complete || domain_complete(&Value::Object(map.clone())),
        "completeness is not proved"
    );
    Ok(())
}

/// Whether any key or string value contains one of `names` as a whole
/// identifier token (identifiers use letters, digits and `-_.:@`).
fn mentions(value: &Value, names: &[&str]) -> bool {
    let hit = |text: &str| {
        text.split(|c: char| !(c.is_ascii_alphanumeric() || "-_.:@".contains(c)))
            .any(|token| names.contains(&token))
    };
    match value {
        Value::String(s) => hit(s),
        Value::Array(items) => items.iter().any(|v| mentions(v, names)),
        Value::Object(map) => map.iter().any(|(k, v)| hit(k) || mentions(v, names)),
        _ => false,
    }
}

fn schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_coverage_domain(
           id TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 1,
           max_source_age_ms INTEGER NOT NULL, created_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS door_coverage_source(
           domain TEXT NOT NULL REFERENCES door_coverage_domain(id),
           core TEXT NOT NULL, handle TEXT NOT NULL UNIQUE,
           PRIMARY KEY(domain,core));
         CREATE TABLE IF NOT EXISTS door_coverage_member(
           domain TEXT NOT NULL, ship TEXT NOT NULL, core TEXT NOT NULL,
           PRIMARY KEY(domain,ship),
           FOREIGN KEY(domain,core) REFERENCES door_coverage_source(domain,core));
         CREATE TABLE IF NOT EXISTS door_source_identity(
           handle TEXT PRIMARY KEY, core TEXT NOT NULL, mailbox TEXT NOT NULL,
           UNIQUE(core,mailbox));
         CREATE TABLE IF NOT EXISTS door_source_alias(
           core TEXT NOT NULL, mailbox TEXT NOT NULL, id TEXT NOT NULL,
           origin_core TEXT NOT NULL, origin_mailbox TEXT NOT NULL, origin_id TEXT NOT NULL,
           recorded_ms INTEGER NOT NULL, PRIMARY KEY(core,mailbox,id));
         CREATE INDEX IF NOT EXISTS door_source_alias_origin
           ON door_source_alias(origin_core,origin_mailbox,origin_id);",
    )?;
    Ok(())
}

fn uuid(value: &str) -> Result<String> {
    Ok(uuid::Uuid::parse_str(value)?.hyphenated().to_string())
}

/// Define an empty coverage domain with an opaque ID. The bound for push-only
/// freshness is the operator's: no door, grant or adapter can widen it.
pub(crate) fn define_domain(conn: &Connection, max_source_age_ms: Option<i64>) -> Result<String> {
    let max = max_source_age_ms.unwrap_or(DEFAULT_MAX_SOURCE_AGE_MS);
    ensure!(
        (1..=MAX_SOURCE_AGE_MS).contains(&max),
        "max_source_age_ms must be 1–{MAX_SOURCE_AGE_MS}"
    );
    schema(conn)?;
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO door_coverage_domain(id,max_source_age_ms,created_ms) VALUES(?1,?2,?3)",
        params![id, max, Store::now_ms()],
    )?;
    Ok(id)
}

/// Enroll a source core and the ships whose homes it holds. A ship belongs
/// to one source per domain; any change bumps the manifest revision.
pub(crate) fn enroll(conn: &Connection, domain: &str, core: &str, ships: &[&str]) -> Result<()> {
    schema(conn)?;
    let core = uuid(core)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM door_coverage_domain WHERE id=?1)",
        [domain],
        |r| r.get(0),
    )?;
    ensure!(known, "unknown coverage domain");
    let mut changed = tx.execute(
        "INSERT OR IGNORE INTO door_coverage_source(domain,core,handle) VALUES(?1,?2,?3)",
        params![domain, core, uuid::Uuid::new_v4().to_string()],
    )?;
    for ship in ships {
        crate::store::identifier(ship)?;
        let holder: Option<String> = tx
            .query_row(
                "SELECT core FROM door_coverage_member WHERE domain=?1 AND ship=?2",
                params![domain, ship],
                |r| r.get(0),
            )
            .optional()?;
        match holder {
            Some(holder) => ensure!(
                holder == core,
                "ship {ship} already has a source in this domain"
            ),
            None => {
                changed += tx.execute(
                    "INSERT INTO door_coverage_member(domain,ship,core) VALUES(?1,?2,?3)",
                    params![domain, ship, core],
                )?;
            }
        }
    }
    if changed > 0 {
        tx.execute(
            "UPDATE door_coverage_domain SET revision=revision+1 WHERE id=?1",
            [domain],
        )?;
    }
    tx.commit()?;
    Ok(())
}

struct Visible {
    revision: Option<i64>,
    max_source_age_ms: i64,
    sources: Vec<(String, Vec<String>)>,
}

/// The domain as this grant may see it: sources holding an allowed ship, and
/// only those ships. `None` when the domain is unknown or wholly hidden, so
/// the two are indistinguishable. A ship-restricted grant gets no revision: a
/// hidden enrollment would otherwise show as a change.
fn visible(
    conn: &Connection,
    domain: &str,
    c: Option<&DelegatedContext>,
) -> Result<Option<Visible>> {
    schema(conn)?;
    let Some((revision, max_source_age_ms)) = conn
        .query_row(
            "SELECT revision,max_source_age_ms FROM door_coverage_domain WHERE id=?1",
            [domain],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut statement = conn.prepare(
        "SELECT s.handle, m.ship FROM door_coverage_source s
         JOIN door_coverage_member m ON m.domain=s.domain AND m.core=s.core
         WHERE s.domain=?1 ORDER BY s.handle, m.ship",
    )?;
    let mut sources: Vec<(String, Vec<String>)> = vec![];
    for row in statement.query_map([domain], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (handle, ship) = row?;
        if c.is_some_and(|c| !c.allows(&ship)) {
            continue;
        }
        match sources.last_mut() {
            Some((last, ships)) if *last == handle => ships.push(ship),
            _ => sources.push((handle, vec![ship])),
        }
    }
    if sources.is_empty() {
        return Ok(None);
    }
    Ok(Some(Visible {
        revision: c.is_none_or(|c| c.ships().is_none()).then_some(revision),
        max_source_age_ms,
        sources,
    }))
}

/// The filtered domain manifest; inventory needs `fleet:read`.
pub(crate) fn manifest(
    conn: &Connection,
    domain: &str,
    c: Option<&DelegatedContext>,
) -> Result<Option<Value>> {
    if c.is_some_and(|c| !c.has("fleet:read")) {
        return Ok(None);
    }
    Ok(visible(conn, domain, c)?.map(|v| {
        json!({"domain":domain,"manifest_revision":v.revision,
            "max_source_age_ms":v.max_source_age_ms,
            "restricted":c.is_some_and(|c| c.ships().is_some()),
            "sources":v.sources.iter().map(|(id, ships)| json!({"id":id,"ships":ships}))
                .collect::<Vec<_>>()})
    }))
}

/// A domain's coverage for this grant (`mail:read`). Sources carry opaque
/// handles, never ships. Nothing ingests sources or installs an owner yet, so
/// every visible source is unavailable and the result is never complete.
pub(crate) fn domain_coverage(
    conn: &Connection,
    domain: &str,
    c: Option<&DelegatedContext>,
) -> Result<Value> {
    if c.is_some_and(|c| !c.has("mail:read")) {
        return Ok(unknown_domain());
    }
    let Some(v) = visible(conn, domain, c)? else {
        return Ok(unknown_domain());
    };
    Ok(desk_domain(&Domain {
        domain: domain.to_owned(),
        max_source_age_ms: v.max_source_age_ms,
        manifest_revision: v.revision,
        owner: None,
        snapshot: None,
        sources: v
            .sources
            .into_iter()
            .map(|(id, _)| Source {
                id,
                state: "unavailable",
                through: None,
                barrier: None,
            })
            .collect(),
        retry: None,
    }))
}

/// A stable mail identity: origin core, mailbox incarnation and mail ID. Never
/// a home path, ship name or copied sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MailIdentity {
    pub core: String,
    pub mailbox: String,
    pub id: String,
}

impl MailIdentity {
    pub(crate) fn new(core: &str, mailbox: &str, id: &str) -> Result<Self> {
        crate::store::identifier(id)?;
        Ok(Self {
            core: uuid(core)?,
            mailbox: uuid(mailbox)?,
            id: id.to_owned(),
        })
    }
}

/// The opaque handle naming one source mailbox `(core, incarnation)`.
pub(crate) fn source_handle(conn: &Connection, core: &str, mailbox: &str) -> Result<String> {
    schema(conn)?;
    let (core, mailbox) = (uuid(core)?, uuid(mailbox)?);
    conn.execute(
        "INSERT OR IGNORE INTO door_source_identity(handle,core,mailbox) VALUES(?1,?2,?3)",
        params![uuid::Uuid::new_v4().to_string(), core, mailbox],
    )?;
    Ok(conn.query_row(
        "SELECT handle FROM door_source_identity WHERE core=?1 AND mailbox=?2",
        params![core, mailbox],
        |r| r.get(0),
    )?)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Alias {
    Recorded,
    Known,
    /// The copy already names another origin, or is itself an origin: an
    /// integrity gap to report, never a reason to suppress either.
    Conflict,
}

fn root(conn: &Connection, mail: &MailIdentity) -> Result<Option<MailIdentity>> {
    Ok(conn
        .query_row(
            "SELECT origin_core,origin_mailbox,origin_id FROM door_source_alias
             WHERE core=?1 AND mailbox=?2 AND id=?3",
            params![mail.core, mail.mailbox, mail.id],
            |r| {
                Ok(MailIdentity {
                    core: r.get(0)?,
                    mailbox: r.get(1)?,
                    id: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Record that `copy` is a copy of `origin`, as ingestion carried it. Aliases
/// point at the root origin, so a copy of a copy needs no chain.
pub(crate) fn record_alias(
    conn: &Connection,
    copy: &MailIdentity,
    origin: &MailIdentity,
) -> Result<Alias> {
    schema(conn)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // A literal self-record is harmless only for a root. Resolving a
    // descendant back to `copy` instead proposes a cycle, not a replay.
    let self_record = copy == origin;
    let origin = root(&tx, origin)?.unwrap_or_else(|| origin.clone());
    if origin == *copy {
        return Ok(if !self_record || root(&tx, copy)?.is_some() {
            Alias::Conflict
        } else {
            Alias::Known
        });
    }
    if let Some(existing) = root(&tx, copy)? {
        return Ok(if existing == origin {
            Alias::Known
        } else {
            Alias::Conflict
        });
    }
    let is_origin: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM door_source_alias WHERE origin_core=?1 AND origin_mailbox=?2 AND origin_id=?3)",
        params![copy.core, copy.mailbox, copy.id],
        |r| r.get(0),
    )?;
    if is_origin {
        return Ok(Alias::Conflict);
    }
    for mail in [copy, &origin] {
        source_handle(&tx, &mail.core, &mail.mailbox)?;
    }
    tx.execute(
        "INSERT INTO door_source_alias(core,mailbox,id,origin_core,origin_mailbox,origin_id,recorded_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![copy.core, copy.mailbox, copy.id, origin.core, origin.mailbox, origin.id, Store::now_ms()],
    )?;
    tx.commit()?;
    Ok(Alias::Recorded)
}

/// The origin a mail identity is a recorded copy of, else itself.
pub(crate) fn origin_of(conn: &Connection, mail: &MailIdentity) -> Result<MailIdentity> {
    schema(conn)?;
    Ok(root(conn, mail)?.unwrap_or_else(|| mail.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHA: &str = "00000000-0000-4000-8000-00000000000a";
    const BETA: &str = "00000000-0000-4000-8000-00000000000b";
    const BOX_A: &str = "00000000-0000-4000-8000-0000000000a1";
    const BOX_B: &str = "00000000-0000-4000-8000-0000000000b1";

    fn context(scopes: &[&str], ships: Option<&[&str]>) -> DelegatedContext {
        DelegatedContext::resolved(
            uuid::Uuid::new_v4().to_string(),
            "door:example".into(),
            scopes.iter().map(|s| s.to_string()).collect(),
            ships.map(|s| s.iter().map(|s| s.to_string()).collect()),
        )
    }

    fn registry() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn
    }

    #[test]
    fn every_builder_produces_valid_v1_coverage() {
        for coverage in [
            selected_mailbox("here", "complete", None),
            selected_mailbox("beta", "unavailable", Some("reverse_only")),
            selected_mailbox("beta", "unsupported", Some("upgrade_peer")),
            local(true),
            local(false),
            operation_only("digest_ack"),
            unknown_domain(),
        ] {
            validate(&coverage, &[]).unwrap();
            assert_eq!(coverage["schema"], SCHEMA);
        }
        assert_eq!(selected_mailbox("here", "complete", None)["complete"], true);
        assert_eq!(
            selected_mailbox("beta", "unavailable", Some("reverse_only"))["complete"],
            false
        );
    }

    #[test]
    fn validation_rejects_other_versions_scopes_fields_and_unproved_completeness() {
        let invalid = |edit: &dyn Fn(&mut Value), base: Value| {
            let mut coverage = base;
            edit(&mut coverage);
            assert!(validate(&coverage, &[]).is_err(), "{coverage}");
        };
        invalid(&|c| c["schema"] = json!("coverage.v2"), local(true));
        invalid(
            &|c| {
                c.as_object_mut().unwrap().remove("schema");
            },
            local(true),
        );
        invalid(&|c| c["scope"] = json!("fleet"), local(true));
        invalid(
            &|c| {
                c.as_object_mut().unwrap().remove("complete");
            },
            local(true),
        );
        invalid(&|c| c["complete"] = json!("yes"), local(true));
        invalid(&|c| c["fleet_wide"] = json!(true), local(true));
        invalid(&|c| c["other_machines"] = json!("covered"), local(true));
        invalid(
            &|c| c["complete"] = json!(true),
            selected_mailbox("beta", "unavailable", None),
        );
        invalid(
            &|c| c["machines"][0]["state"] = json!("partial"),
            selected_mailbox("here", "complete", None),
        );
        invalid(
            &|c| c["mailboxes"] = json!("covered"),
            operation_only("digest_ack"),
        );
        // Nothing can mark a domain complete without proof, including unknown ones.
        invalid(&|c| c["complete"] = json!(true), unknown_domain());
        invalid(&|c| c["state"] = json!("covered"), unknown_domain());
        invalid(
            &|c| c["owner"] = json!({"id":"x","generation":1,"incarnation":"i","state":"active"}),
            unknown_domain(),
        );
    }

    fn source(id: &str, state: &'static str, barrier: Option<Barrier>) -> Source {
        Source {
            id: id.into(),
            state,
            through: Some("checkpoint".into()),
            barrier,
        }
    }

    fn push(age_ms: i64) -> Option<Barrier> {
        Some(Barrier {
            mode: "last_received_push",
            received_at: 1_700_000_000_000,
            age_ms,
            max_source_age_ms: DEFAULT_MAX_SOURCE_AGE_MS,
        })
    }

    fn domain(owner: &'static str, sources: Vec<Source>) -> Domain {
        Domain {
            domain: "domain".into(),
            max_source_age_ms: DEFAULT_MAX_SOURCE_AGE_MS,
            manifest_revision: Some(3),
            owner: Some(Owner {
                id: "owner".into(),
                generation: 1,
                incarnation: "incarnation".into(),
                state: owner,
            }),
            snapshot: Some("snapshot".into()),
            sources,
            retry: None,
        }
    }

    #[test]
    fn desk_domain_completeness_is_derived_from_owner_and_source_barriers() {
        let on_demand = || {
            Some(Barrier {
                mode: "on_demand",
                received_at: 1,
                age_ms: 0,
                max_source_age_ms: DEFAULT_MAX_SOURCE_AGE_MS,
            })
        };
        let complete = desk_domain(&domain(
            "active",
            vec![
                source("a", "covered", on_demand()),
                source("b", "covered", push(1200)),
            ],
        ));
        validate(&complete, &[]).unwrap();
        assert_eq!(complete["complete"], true, "{complete}");
        for partial in [
            domain("staging", vec![source("a", "covered", on_demand())]),
            domain("active", vec![]),
            domain(
                "active",
                vec![source("a", "covered", push(DEFAULT_MAX_SOURCE_AGE_MS + 1))],
            ),
            domain("active", vec![source("a", "covered", push(-1))]),
            domain("active", vec![source("a", "covered", None)]),
            domain("active", vec![source("a", "cached", on_demand())]),
            domain(
                "active",
                vec![
                    source("a", "covered", on_demand()),
                    source("b", "reverse_only", None),
                ],
            ),
        ] {
            let coverage = desk_domain(&partial);
            validate(&coverage, &[]).unwrap();
            assert_eq!(coverage["complete"], false, "{coverage}");
            let mut forged = coverage.clone();
            forged["complete"] = json!(true);
            assert!(validate(&forged, &[]).is_err(), "{forged}");
        }
        let mut without = domain("active", vec![source("a", "covered", on_demand())]);
        without.snapshot = None;
        assert_eq!(desk_domain(&without)["complete"], false);
    }

    #[test]
    fn source_freshness_cannot_widen_the_operator_domain_bound() {
        let mut d = domain("active", vec![source("a", "covered", push(60_001))]);
        d.max_source_age_ms = 60_000;
        let coverage = desk_domain(&d);
        assert_eq!(coverage["max_source_age_ms"], 60_000);
        assert_eq!(coverage["complete"], false);
        validate(&coverage, &[]).unwrap();
        let mut forged = coverage;
        forged["complete"] = json!(true);
        assert!(validate(&forged, &[]).is_err());
        d.sources[0].barrier.as_mut().unwrap().age_ms = 60_000;
        assert_eq!(desk_domain(&d)["complete"], true);
    }

    #[test]
    fn operator_can_reuse_the_domain_contract_without_a_delegated_grant() {
        let conn = registry();
        let id = define_domain(&conn, Some(60_000)).unwrap();
        enroll(&conn, &id, ALPHA, &["survey"]).unwrap();
        let full = context(&["fleet:read", "mail:read"], None);
        assert_eq!(
            manifest(&conn, &id, None).unwrap(),
            manifest(&conn, &id, Some(&full)).unwrap()
        );
        let coverage = domain_coverage(&conn, &id, None).unwrap();
        assert_eq!(coverage, domain_coverage(&conn, &id, Some(&full)).unwrap());
        assert_eq!(coverage["max_source_age_ms"], 60_000);
        validate(&coverage, &[]).unwrap();
    }

    #[test]
    fn unknown_and_wholly_hidden_domains_are_indistinguishable() {
        let conn = registry();
        let reader = context(&["fleet:read", "mail:read"], Some(&["survey"]));
        assert_eq!(
            domain_coverage(&conn, "no-such-domain", Some(&reader)).unwrap(),
            unknown_domain()
        );
        assert!(
            manifest(&conn, "no-such-domain", Some(&reader))
                .unwrap()
                .is_none()
        );
        let id = define_domain(&conn, None).unwrap();
        enroll(&conn, &id, BETA, &["relay"]).unwrap();
        assert_eq!(
            domain_coverage(&conn, &id, Some(&reader)).unwrap(),
            unknown_domain()
        );
        assert!(manifest(&conn, &id, Some(&reader)).unwrap().is_none());
        assert!(enroll(&conn, "no-such-domain", ALPHA, &["survey"]).is_err());
    }

    #[test]
    fn manifests_and_coverage_name_only_allowed_ships() {
        let conn = registry();
        let id = define_domain(&conn, Some(60_000)).unwrap();
        enroll(&conn, &id, ALPHA, &["survey", "relay"]).unwrap();
        enroll(&conn, &id, BETA, &["relay-two"]).unwrap();
        let full = context(&["fleet:read", "mail:read"], None);
        let all = manifest(&conn, &id, Some(&full)).unwrap().unwrap();
        assert_eq!(all["manifest_revision"], 3, "{all}");
        assert_eq!(all["restricted"], false);
        assert_eq!(all["max_source_age_ms"], 60_000);
        assert_eq!(all["sources"].as_array().unwrap().len(), 2);

        let narrow = context(&["fleet:read", "mail:read"], Some(&["survey"]));
        let seen = manifest(&conn, &id, Some(&narrow)).unwrap().unwrap();
        let text = seen.to_string();
        for hidden in ["relay", "relay-two", ALPHA, BETA] {
            assert!(!text.contains(hidden), "{hidden} leaked: {text}");
        }
        assert_eq!(
            seen["sources"].as_array().unwrap().len(),
            1,
            "no hidden source or count"
        );
        assert_eq!(seen["sources"][0]["ships"], json!(["survey"]));
        assert_eq!(seen["restricted"], true);
        assert!(
            seen["manifest_revision"].is_null(),
            "hidden enrollments move the revision"
        );

        let coverage = domain_coverage(&conn, &id, Some(&narrow)).unwrap();
        validate(&coverage, &["relay", "relay-two"]).unwrap();
        assert!(
            !coverage.to_string().contains("survey"),
            "coverage uses opaque handles: {coverage}"
        );
        assert_eq!(coverage["complete"], false, "no intake yet: {coverage}");
        assert_eq!(coverage["sources"][0]["state"], "unavailable");
        assert_eq!(coverage["sources"][0]["id"], seen["sources"][0]["id"]);
        assert_eq!(
            domain_coverage(&conn, &id, Some(&full)).unwrap()["sources"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Inventory needs fleet:read; coverage needs mail:read.
        let mail_only = context(&["mail:read"], Some(&["survey"]));
        assert!(manifest(&conn, &id, Some(&mail_only)).unwrap().is_none());
        assert_eq!(
            domain_coverage(&conn, &id, Some(&context(&["fleet:read"], None))).unwrap(),
            unknown_domain()
        );

        // The privacy check refuses any hidden ship name, however it appears.
        let mut leaky = coverage.clone();
        leaky["sources"][0]["id"] = json!("relay");
        assert!(validate(&leaky, &["relay"]).is_err());
        leaky["sources"][0]["id"] = json!("source of relay-two");
        assert!(validate(&leaky, &["relay-two"]).is_err());
        assert!(validate(&leaky, &["relay"]).is_ok(), "whole tokens only");
    }

    #[test]
    fn enrollment_refuses_ambiguous_membership_and_bad_bounds() {
        let conn = registry();
        assert!(define_domain(&conn, Some(0)).is_err());
        assert!(define_domain(&conn, Some(MAX_SOURCE_AGE_MS + 1)).is_err());
        let id = define_domain(&conn, None).unwrap();
        enroll(&conn, &id, ALPHA, &["survey"]).unwrap();
        assert!(
            enroll(&conn, &id, BETA, &["survey"]).is_err(),
            "one source per ship"
        );
        assert!(enroll(&conn, &id, "not-a-core", &["relay"]).is_err());
        assert!(enroll(&conn, &id, BETA, &["bad/ship"]).is_err());
        let revision = || {
            conn.query_row(
                "SELECT revision FROM door_coverage_domain WHERE id=?1",
                [&id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        let before = revision();
        enroll(&conn, &id, ALPHA, &["survey"]).unwrap();
        assert_eq!(revision(), before, "re-enrolling is not a change");
        let max: i64 = conn
            .query_row(
                "SELECT max_source_age_ms FROM door_coverage_domain WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(max, DEFAULT_MAX_SOURCE_AGE_MS);
    }

    #[test]
    fn source_identities_are_stable_opaque_handles() {
        let conn = registry();
        let handle = source_handle(&conn, ALPHA, BOX_A).unwrap();
        assert_eq!(
            source_handle(&conn, &ALPHA.to_uppercase(), BOX_A).unwrap(),
            handle
        );
        assert_ne!(source_handle(&conn, ALPHA, BOX_B).unwrap(), handle);
        assert!(!handle.contains(ALPHA) && !handle.contains(BOX_A));
        assert!(
            source_handle(&conn, "home-a", BOX_A).is_err(),
            "never a path or name"
        );
        assert!(MailIdentity::new(ALPHA, BOX_A, "").is_err());
        assert!(MailIdentity::new(ALPHA, "/home/example", "mail-1").is_err());
    }

    #[test]
    fn aliases_resolve_to_one_origin_and_conflicts_never_suppress() {
        let conn = registry();
        let origin = MailIdentity::new(ALPHA, BOX_A, "mail-1").unwrap();
        let desk = MailIdentity::new(BETA, BOX_B, "mail-1").unwrap();
        let collected = MailIdentity::new(BETA, BOX_B, "mail-1-copy").unwrap();
        assert_eq!(
            origin_of(&conn, &desk).unwrap(),
            desk,
            "unrecorded mail is its own origin"
        );
        assert_eq!(
            record_alias(&conn, &desk, &origin).unwrap(),
            Alias::Recorded
        );
        assert_eq!(
            record_alias(&conn, &desk, &origin).unwrap(),
            Alias::Known,
            "replay is idempotent"
        );
        assert_eq!(record_alias(&conn, &origin, &origin).unwrap(), Alias::Known);
        // A copy of a copy points at the root origin.
        assert_eq!(
            record_alias(&conn, &collected, &desk).unwrap(),
            Alias::Recorded
        );
        assert_eq!(origin_of(&conn, &collected).unwrap(), origin);
        // Equal mail IDs with another origin are an integrity gap, not a merge.
        let other = MailIdentity::new(ALPHA, BOX_B, "mail-1").unwrap();
        assert_eq!(record_alias(&conn, &desk, &other).unwrap(), Alias::Conflict);
        assert_eq!(
            origin_of(&conn, &desk).unwrap(),
            origin,
            "a conflict changes nothing"
        );
        // A known origin cannot become someone's copy, and no cycle forms.
        assert_eq!(
            record_alias(&conn, &origin, &other).unwrap(),
            Alias::Conflict
        );
        assert_eq!(
            record_alias(&conn, &origin, &desk).unwrap(),
            Alias::Conflict
        );
        assert_eq!(origin_of(&conn, &origin).unwrap(), origin);
        let handles: i64 = conn
            .query_row("SELECT COUNT(*) FROM door_source_identity", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(handles, 2, "one handle per source mailbox");
    }

    #[test]
    fn legacy_digest_ack_coverage_is_replaced_at_the_origin() {
        let mut legacy = json!({"acknowledged":1,
            "coverage":{"scope":"desk-digest","complete":true,"mailboxes":"not_queried"}});
        assert!(
            validate(&legacy["coverage"], &[]).is_err(),
            "old peers are unversioned"
        );
        stamp_operation(&mut legacy, "digest_ack");
        validate(&legacy["coverage"], &[]).unwrap();
        assert_eq!(legacy["coverage"]["scope"], "operation-only");
        assert_eq!(legacy["acknowledged"], 1);
    }
}
