//! Fixed capacity/typed-needs projection over existing admission and needs
//! readers. Uses the receiving inspection allowlist, never operator dispatch.
use super::scoped::{Command, DelegatedContext, Denial};
use crate::{
    jobs::capacity::Capacity,
    snapshot::inspection as view,
    store::{Caller, Store},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) fn validate_args(command: Command, args: &Value) -> Result<(), Denial> {
    let fields = args.as_object().ok_or(Denial::Arguments)?;
    let allowed: &[&str] = match command {
        Command::MachineCapacity => &["machine", "ship", "task_id"],
        Command::RuntimeNeeds => &["ship", "limit", "continuation", "include_text"],
        _ => return Err(Denial::Arguments),
    };
    if fields.keys().any(|k| !allowed.contains(&k.as_str()))
        || ["machine", "ship", "task_id"].iter().any(|k| {
            args.get(*k)
                .is_some_and(|v| v.as_str().is_none_or(|s| s.len() > 128 || !view::handle(s)))
        })
        || (command == Command::MachineCapacity && !args["machine"].is_string())
        || (command == Command::RuntimeNeeds && !args["ship"].is_string())
        || (args.get("task_id").is_some() && args.get("ship").is_none())
        || args
            .get("limit")
            .is_some_and(|v| v.as_u64().is_none_or(|n| !(1..=50).contains(&n)))
        || args.get("include_text").is_some_and(|v| !v.is_boolean())
        || args
            .get("continuation")
            .is_some_and(|v| v.as_str().is_none_or(|s| uuid::Uuid::parse_str(s).is_err()))
    {
        return Err(Denial::Arguments);
    }
    Ok(())
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Coverage {
    schema: String,
    scope: String,
    complete: bool,
    machine: String,
    ship: Option<String>,
    facet: String,
    state: String,
    mode: String,
    observed_at: Option<i64>,
    other_machines: String,
    mail_complete: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Need {
    id: String,
    origin: String,
    source: String,
    source_id: String,
    ship: String,
    task_id: Option<String>,
    kind: String,
    evidence: Option<String>,
    mail_id: Option<String>,
    mail_kind: Option<String>,
    urgency: Option<String>,
    created_ms: Option<i64>,
    deadline_ms: Option<i64>,
    observed_ms: i64,
    version: Option<String>,
    question: Option<String>,
    choices: Option<Vec<String>>,
    text_withheld: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    schema: String,
    capacity: Option<Capacity>,
    needs: Vec<Need>,
    unknown: Vec<String>,
    more: bool,
    continuation: Option<String>,
    coverage: Coverage,
    writes: String,
    cache_writes: String,
    action_authority: String,
}
impl Reply {
    fn empty(command: Command, machine: &str, ship: Option<&str>, state: &str) -> Self {
        Self {
            schema: "capacity-needs.v1".into(),
            capacity: None,
            needs: vec![],
            unknown: vec!["runtime_observation".into()],
            more: false,
            continuation: None,
            coverage: Coverage {
                schema: "coverage.v1".into(),
                scope: if ship.is_some() {
                    "selected-ship"
                } else {
                    "selected-machine"
                }
                .into(),
                complete: false,
                machine: machine.into(),
                ship: ship.map(str::to_owned),
                facet: if command == Command::MachineCapacity {
                    "capacity"
                } else {
                    "needs"
                }
                .into(),
                state: state.into(),
                mode: "unavailable".into(),
                observed_at: None,
                other_machines: "not_requested".into(),
                mail_complete: false,
            },
            writes: "none".into(),
            cache_writes: "bounded_registry_continuations_only".into(),
            action_authority: "none".into(),
        }
    }
    fn validate(&mut self, command: Command, c: &DelegatedContext, args: &Value) -> Result<()> {
        let ship = args["ship"].as_str();
        ensure!(
            self.schema == "capacity-needs.v1"
                && self.coverage.schema == "coverage.v1"
                && self.coverage.ship.as_deref() == ship
                && !self.coverage.mail_complete
                && self.writes == "none"
                && self.action_authority == "none"
                && self.cache_writes == "bounded_registry_continuations_only",
            Denial::Arguments
        );
        ensure!(
            view::handle(&self.coverage.machine)
                && self.unknown.iter().all(|s| [
                    "runtime_observation",
                    "live_process_liveness",
                    "provider_health",
                    "aggregate_mail_coverage"
                ]
                .contains(&s.as_str())),
            Denial::Arguments
        );
        ensure!(
            self.needs.len() <= 50
                && serde_json::to_vec(self)?.len() <= 64 * 1024
                && self
                    .continuation
                    .as_deref()
                    .is_none_or(|s| uuid::Uuid::parse_str(s).is_ok()),
            Denial::Arguments
        );
        ensure!(
            ["covered", "unavailable", "unsupported", "reverse_only"]
                .contains(&self.coverage.state.as_str())
                && ["local_live", "remote_live", "unavailable"]
                    .contains(&self.coverage.mode.as_str())
                && self.coverage.other_machines == "not_requested",
            Denial::Arguments
        );
        ensure!(
            self.coverage.facet
                == if command == Command::MachineCapacity {
                    "capacity"
                } else {
                    "needs"
                }
                && self.coverage.scope
                    == if ship.is_some() {
                        "selected-ship"
                    } else {
                        "selected-machine"
                    },
            Denial::Arguments
        );
        if let Some(cap) = &self.capacity {
            ensure!(command == Command::MachineCapacity, Denial::Arguments);
            ensure!(
                cap.headroom_basis == "heavy_slots_only_not_agent_capacity"
                    && view::handle(&cap.ledger)
                    && ["visible", "withheld"].contains(&cap.shared_host.as_str())
                    && ["withheld", "known", "unknown", "not_requested"]
                        .contains(&cap.charter_policy_state.as_str())
                    && ["active_task_charter", "unknown"]
                        .contains(&cap.charter_policy_source.as_str())
                    && ["covered", "withheld", "unknown", "unavailable"]
                        .contains(&cap.route_state.as_str())
                    && cap.reservation_count == cap.holds.len() as u64
                    && cap.queue_depth == cap.queue.len() as u64
                    && cap.occupied_heavy_slots
                        == cap.holds.iter().filter(|h| h.heavy).count() as u64,
                Denial::Arguments
            );
            if let Some(p) = &cap.policy {
                ensure!(
                    p.crew_memory
                        .iter()
                        .all(|m| view::HARNESSES.contains(&m.harness.as_str())
                            && ["host", "manifest", "default"].contains(&m.source.as_str())),
                    Denial::Arguments
                );
                ensure!(
                    p.sources.iter().all(|(k, v)| [
                        "max_heavy_jobs",
                        "max_load_per_cpu",
                        "min_available_memory_bytes",
                        "min_free_disk_bytes",
                        "ramp_secs",
                        "heavy_estimate",
                        "crew_default_memory_bytes"
                    ]
                    .contains(&k.as_str())
                        && ["host_configuration", "runtime_default"].contains(&v.as_str())),
                    Denial::Arguments
                );
            }
            if let Some(r) = &cap.readings {
                ensure!(
                    r.disks.iter().all(|d| view::handle(&d.filesystem)
                        && ["covered", "unknown"].contains(&d.state.as_str())),
                    Denial::Arguments
                );
            }
            if c.ships().is_some() {
                ensure!(
                    ship.is_some()
                        && cap.shared_host == "withheld"
                        && cap.ledger == "selected_ship"
                        && cap.policy.is_none()
                        && cap.readings.is_none()
                        && cap.route_availability.is_none()
                        && cap.route_state == "withheld"
                        && cap.charter_heavy_cap.is_none()
                        && cap.charter_occupied_heavy_slots.is_none()
                        && cap.free_heavy_slots.is_none()
                        && cap.charter_policy_state == "withheld",
                    Denial::Ship
                );
                ensure!(
                    cap.holds.iter().all(|h| h.ship.as_deref() == ship)
                        && cap
                            .queue
                            .iter()
                            .all(|q| q.blocked.as_deref() == Some("capacity")
                                && q.blocked_at_ms.is_none()
                                && q.blocker_basis == "unattributed_capacity_wait"),
                    Denial::Ship
                );
            }
            ensure!(
                cap.holds.iter().all(|h| h
                    .ship
                    .as_deref()
                    .is_none_or(|s| view::handle(s) && c.allows(s))
                    && h.job_id.as_deref().is_none_or(view::handle)
                    && h.incarnation.as_deref().is_none_or(view::handle)
                    && h.task_id.as_deref().is_none_or(view::handle)
                    && h.liveness == "unknown_held_conservatively"),
                Denial::Arguments
            );
            ensure!(
                cap.queue
                    .iter()
                    .all(|q| q.job_id.as_deref().is_none_or(view::handle)
                        && q.blocked.as_deref().is_none_or(|s| [
                            "capacity",
                            "heavy_slot",
                            "cpu",
                            "disk"
                        ]
                        .contains(&s))
                        && [
                            "last_head_classification_not_current_diagnosis",
                            "unattributed_capacity_wait"
                        ]
                        .contains(&q.blocker_basis.as_str())),
                Denial::Arguments
            );
            if let Some(routes) = &cap.route_availability {
                ensure!(
                    routes
                        .keys()
                        .all(|k| ["available", "backing_off", "capped", "idle"]
                            .contains(&k.as_str())),
                    Denial::Arguments
                );
            }
        }
        let show = args["include_text"] == true && c.has("mail:read");
        for n in &mut self.needs {
            ensure!(
                command == Command::RuntimeNeeds
                    && Some(n.ship.as_str()) == ship
                    && c.allows(&n.ship)
                    && [&n.id, &n.origin, &n.source_id]
                        .iter()
                        .all(|s| view::handle(s))
                    && n.task_id.as_deref().is_none_or(view::handle)
                    && n.version.as_deref().is_none_or(view::handle)
                    && n.mail_id.as_deref().is_none_or(view::handle),
                Denial::Ship
            );
            ensure!(
                [
                    "mail",
                    "job",
                    "schedule",
                    "remote_wait",
                    "first_mate",
                    "workspace"
                ]
                .contains(&n.source.as_str())
                    && ["approval", "decision", "blocker"].contains(&n.kind.as_str())
                    && n.evidence.as_deref().is_none_or(|s| EVIDENCE.contains(&s)),
                Denial::Arguments
            );
            ensure!(
                n.mail_kind
                    .as_deref()
                    .is_none_or(|s| ["request", "decision"].contains(&s))
                    && n.urgency
                        .as_deref()
                        .is_none_or(|s| ["low", "normal", "high"].contains(&s)),
                Denial::Arguments
            );
            ensure!(
                (show && n.kind != "approval")
                    || (n.question.is_none() && n.choices.is_none() && n.text_withheld),
                Denial::Scope
            );
            if let Some(s) = &mut n.question {
                *s = view::safe_text(s);
            }
            if let Some(choices) = &mut n.choices {
                ensure!(choices.len() <= 20, Denial::Arguments);
                for s in choices {
                    *s = view::safe_text(s);
                }
            }
        }
        Ok(())
    }
}
const EVIDENCE: &[&str] = &[
    "capacity_wait",
    "orphaned_job",
    "scheduling_fault",
    "stale_remote_wait",
    "workspace_disk_wait",
    "first_mate_no_route",
];

fn ship_home(conn: &Connection, ship: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT home FROM ships WHERE name=?1 AND retired_ms IS NULL",
            [ship],
            |r| r.get(0),
        )
        .optional()?)
}
pub(super) fn request(command: Command, args: &Value, c: &DelegatedContext) -> Result<Value> {
    validate_args(command, args)?;
    ensure!(c.has("fleet:read"), Denial::Scope);
    let ship = args["ship"].as_str();
    ensure!(ship.is_none_or(|s| c.allows(s)), Denial::Ship);
    ensure!(c.ships().is_none() || ship.is_some(), Denial::Ship);
    let conn = super::runtime::registry_readonly()?;
    let machine = if command == Command::MachineCapacity {
        args["machine"].as_str().unwrap().to_owned()
    } else if ship_home(&conn, ship.unwrap())?.is_some() {
        "local".into()
    } else {
        conn.query_row(
            "SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine LIMIT 1",
            [ship],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .ok_or(Denial::Ship)?
    };
    let operation = if command == Command::MachineCapacity {
        "machine_capacity"
    } else {
        "runtime_needs"
    };
    let mut reply = if machine == "local" {
        if let Some(ship) = ship {
            ensure!(ship_home(&conn, ship)?.is_some(), Denial::Ship);
        }
        local_read(&crate::fleet::registry()?, command, args, c)?
    } else {
        let valid: bool = if let Some(ship) = ship {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM machine_ships WHERE machine=?1 AND name=?2)",
                rusqlite::params![machine, ship],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1)",
                [&machine],
                |r| r.get(0),
            )?
        };
        ensure!(valid, Denial::Ship);
        let callable: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
            [&machine],
            |r| r.get(0),
        )?;
        if !callable {
            Reply::empty(command, &machine, ship, "reverse_only")
        } else {
            match crate::machines::inspection_of(
                &machine,
                &json!({"context":c.gateway(),"operation":operation,"args":args}),
            ) {
                Ok(v) => {
                    let mut r: Reply = serde_json::from_value(v)?;
                    r.validate(command, c, args)?;
                    r.coverage.machine = machine.clone();
                    if r.coverage.state == "covered" {
                        r.coverage.mode = "remote_live".into();
                    }
                    r
                }
                Err(e) if e.downcast_ref::<Denial>().is_some() => return Err(e),
                Err(e) => Reply::empty(
                    command,
                    &machine,
                    ship,
                    if e.to_string().contains("unknown gateway command") {
                        "unsupported"
                    } else {
                        "unavailable"
                    },
                ),
            }
        }
    };
    reply.validate(command, c, args)?;
    Ok(serde_json::to_value(reply)?)
}
pub(super) fn serve_local(operation: &Value, args: &Value, c: &DelegatedContext) -> Result<Value> {
    let command = match operation.as_str() {
        Some("machine_capacity") => Command::MachineCapacity,
        Some("runtime_needs") => Command::RuntimeNeeds,
        _ => return Err(Denial::Arguments.into()),
    };
    validate_args(command, args)?;
    // Machine is a host-side directory handle, not an instruction to recurse.
    Ok(serde_json::to_value(local_read(
        &crate::fleet::registry()?,
        command,
        args,
        c,
    )?)?)
}
fn local_read(
    conn: &Connection,
    command: Command,
    args: &Value,
    c: &DelegatedContext,
) -> Result<Reply> {
    validate_args(command, args)?;
    let ship = args["ship"].as_str();
    ensure!(
        c.has("fleet:read")
            && ship.is_none_or(|s| c.allows(s))
            && (c.ships().is_none() || ship.is_some()),
        Denial::Ship
    );
    let home = if let Some(ship) = ship {
        Some(ship_home(conn, ship)?.ok_or(Denial::Ship)?)
    } else {
        None
    };
    let store = home
        .as_ref()
        .map(|h| Store::open_readonly(std::path::Path::new(h), Caller::Operator))
        .transpose();
    let mut reply = Reply::empty(command, "local", ship, "unavailable");
    let Ok(store) = store else { return Ok(reply) };
    if let (Some(store), Some(ship)) = (&store, ship) {
        ensure!(view::owned(store, ship)?, Denial::Ship);
    }
    let before_source = store.as_ref().map(view::origin).transpose()?;
    if command == Command::MachineCapacity {
        // Unknown ledger/settings/host readings never become finite free slots.
        match crate::jobs::capacity::read(
            store.as_ref(),
            ship,
            args["task_id"].as_str(),
            c.ships().is_none(),
            c,
        ) {
            Ok(mut cap) => {
                if c.ships().is_some() {
                    let s = store.as_ref().ok_or(Denial::Ship)?;
                    let source = before_source.as_deref().ok_or(Denial::Ship)?;
                    for h in &mut cap.holds {
                        h.recheck(s, ship.ok_or(Denial::Ship)?, source, c);
                    }
                }
                reply.capacity = Some(cap);
                reply.unknown.clear();
                reply.coverage.complete = c.ships().is_none();
            }
            Err(_) => return Ok(reply),
        }
    } else {
        let store = store.as_ref().ok_or(Denial::Ship)?;
        let ship = ship.ok_or(Denial::Ship)?;
        let source = view::origin(store)?;
        let after = super::inspection::needs_cursor(conn, args, c, &source, None)?;
        let native = match crate::needs::scoped_query(
            store,
            ship,
            c,
            args["limit"].as_u64().unwrap_or(25) as usize,
            after.as_deref().unwrap_or(""),
        ) {
            Ok(v) if v["available"] == true => v,
            _ => return Ok(reply),
        };
        let show = args["include_text"] == true && c.has("mail:read");
        for n in native["items"].as_array().into_iter().flatten() {
            if n["ship"].as_str() != Some(ship) {
                continue;
            }
            let Some(id) = n["source_id"].as_str().filter(|s| view::handle(s)) else {
                continue;
            };
            let src = n["source"].as_str().unwrap_or("");
            let kind = n["kind"].as_str().unwrap_or("");
            if !["approval", "decision", "blocker"].contains(&kind) {
                continue;
            }
            // Native privileged approval bodies contain the staged argv/cwd.
            // Never render approval text, even with mail scope; metadata and
            // existing authorized mail tools remain separate entitlements.
            let ask = src == "mail" && kind != "approval";
            if src == "job" {
                let reference = view::JobRef {
                    origin: source.clone(),
                    ship: ship.into(),
                    id: id.into(),
                    incarnation: n["incarnation"].as_str().unwrap_or("").into(),
                    task_id: n["task_id"].as_str().unwrap_or("").into(),
                };
                if view::check_reference(store, ship, &reference, c).is_err() {
                    // Unprovable worker/custody is not a source-wide failure:
                    // do not let a hidden actor turn an otherwise visible page
                    // into an unavailable-state oracle.
                    continue;
                }
            }
            reply.needs.push(Need {
                id: crate::core::hash(&json!([source, src, id])),
                origin: source.clone(),
                source: src.into(),
                source_id: id.into(),
                ship: ship.into(),
                task_id: n["task_id"]
                    .as_str()
                    .filter(|s| view::handle(s))
                    .map(str::to_owned),
                kind: kind.into(),
                evidence: n["evidence"]
                    .as_str()
                    .filter(|s| EVIDENCE.contains(s))
                    .map(str::to_owned),
                mail_id: n["mail_id"]
                    .as_str()
                    .filter(|s| view::handle(s))
                    .map(str::to_owned),
                mail_kind: n["mail_kind"]
                    .as_str()
                    .filter(|s| ["request", "decision"].contains(s))
                    .map(str::to_owned),
                urgency: n["urgency"]
                    .as_str()
                    .filter(|s| ["low", "normal", "high"].contains(s))
                    .map(str::to_owned),
                created_ms: n["created_ms"].as_i64(),
                deadline_ms: n["deadline_ms"].as_i64(),
                observed_ms: native["observed_ms"].as_i64().unwrap_or(Store::now_ms()),
                version: n["version"]
                    .as_str()
                    .filter(|s| view::handle(s))
                    .map(str::to_owned),
                question: if show && ask {
                    n["question"].as_str().map(view::safe_text)
                } else {
                    None
                },
                choices: if show && ask {
                    Some(
                        n["choices"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .take(20)
                            .map(view::safe_text)
                            .collect(),
                    )
                } else {
                    None
                },
                text_withheld: !show || !ask,
            });
        }
        if let Some(next) = native["next_after"].as_str() {
            reply.more = true;
            reply.continuation =
                super::inspection::needs_cursor(conn, args, c, &source, Some(next))?;
        }
        // A successful native needs observation is still explicitly not a
        // complete captain-mail subscription or an all-clear.
        reply.unknown = vec![
            "live_process_liveness".into(),
            "provider_health".into(),
            "aggregate_mail_coverage".into(),
        ];
        ensure!(
            view::origin(store)? == source && view::owned(store, ship)?,
            Denial::Ship
        );
        for n in &reply.needs {
            if let Some(task) = &n.task_id {
                ensure!(store.task(task)?["owner"] == ship, Denial::Ship);
            }
        }
    }
    if let (Some(s), Some(ship)) = (&store, ship) {
        ensure!(
            view::origin(s)? == before_source.ok_or(Denial::Ship)? && view::owned(s, ship)?,
            Denial::Ship
        );
    }
    reply.coverage.state = "covered".into();
    reply.coverage.mode = "local_live".into();
    reply.coverage.observed_at = Some(Store::now_ms());
    if serde_json::to_vec(&reply)?.len() > 64 * 1024 {
        return Ok(Reply::empty(command, "local", ship, "unavailable"));
    }
    reply.validate(command, c, args)?;
    Ok(reply)
}

#[cfg(all(test, target_os = "linux"))]
mod filesystem_tests;
#[cfg(test)]
mod tests;
