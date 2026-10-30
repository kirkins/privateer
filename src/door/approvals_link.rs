//! Linked approval admission, atomic origin commit, then host release.
//! Revoke after dispatch is not rollback: report outcome_unknown.
//! A prepared digest pins custody to one link incarnation and admitted grant.
//! Never hold the originating registry writer while waiting on a gateway.
use super::scoped::{DelegatedContext, Denial};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const PREPARE_LIFETIME_MS: i64 = 600_000;
const RECEIVE_BUDGET_MS: u64 = 5_000;
const PINS: i64 = 4096;
type PinRow = (String, String, String, String, String, String, i64);

pub(super) struct Route {
    machine: String,
    incarnation: String,
    ship: String,
    binding: Option<Value>,
}

fn schema(conn: &Connection) -> Result<()> {
    super::schema::write(conn, || {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS door_linked_approval_routes(
             grant_id TEXT NOT NULL, resource TEXT NOT NULL, digest TEXT NOT NULL,
             ship TEXT NOT NULL, ask_id TEXT NOT NULL, machine TEXT NOT NULL,
             incarnation TEXT NOT NULL, binding TEXT NOT NULL, authority TEXT NOT NULL,
             challenge_expires_ms INTEGER NOT NULL, retain_until_ms INTEGER NOT NULL,
             PRIMARY KEY(grant_id,resource,digest));",
        )?;
        Ok(())
    })
}

fn authority(c: &DelegatedContext) -> String {
    json!([c.capabilities(), c.visibility_revision()]).to_string()
}

/// No selected ship means local-only discovery. A remote prepare must never
/// fan out, creating/replacing challenges on machines the captain did not name.
pub(super) fn route(args: &Value, c: &DelegatedContext) -> Result<Option<Route>, Denial> {
    if args["ship"].is_null() && args["mode"] == "prepare" {
        return Ok(None);
    }
    if let Some(ship) = args["ship"].as_str() {
        if !c.allows(ship) {
            return Err(Denial::Ship);
        }
    }
    let registry = super::runtime::registry_readonly().map_err(|_| Denial::AuthUnavailable)?;
    let view =
        rusqlite::Transaction::new_unchecked(&registry, rusqlite::TransactionBehavior::Deferred)
            .map_err(|_| Denial::AuthUnavailable)?;
    let conn = &view;
    // A linked digest must not fall through into a colliding local ask when a
    // caller omits/changes ship. This reads only the admitted grant's own pins.
    if args["mode"] == "answer" {
        let installed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='door_linked_approval_routes')",
            [], |r| r.get(0),
        ).map_err(|_| Denial::AuthUnavailable)?;
        if installed {
            let pin: Option<PinRow> = conn
                .query_row(
                    "SELECT machine,incarnation,ship,ask_id,binding,authority,retain_until_ms
                 FROM door_linked_approval_routes WHERE grant_id=?1 AND resource=?2 AND digest=?3",
                    params![c.client(), c.resource(), args["digest"].as_str()],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| Denial::AuthUnavailable)?;
            if let Some((machine, incarnation, ship, id, binding, admitted, retain)) = pin {
                if args["id"] != id
                    || args["ship"].as_str().is_some_and(|s| s != ship)
                    || admitted != authority(c)
                    || retain <= crate::store::Store::now_ms()
                    || !c.allows(&ship)
                {
                    return Err(Denial::ApprovalUnavailable);
                }
                let route = Route {
                    machine,
                    incarnation,
                    ship,
                    binding: Some(
                        serde_json::from_str(&binding).map_err(|_| Denial::ApprovalUnavailable)?,
                    ),
                };
                return if current(conn, &route) {
                    Ok(Some(route))
                } else {
                    Err(Denial::ApprovalUnavailable)
                };
            }
        }
    }
    let Some(ship) = args["ship"].as_str() else {
        return Ok(None);
    };
    if crate::fleet::members(conn)
        .map_err(|_| Denial::AuthUnavailable)?
        .iter()
        .any(|m| m.name == ship && m.home.is_dir())
    {
        return Ok(None);
    }
    // Retain all candidates: ORDER BY ... LIMIT 1 is not proof of ownership.
    let mut stmt = conn
        .prepare("SELECT machine FROM machine_ships WHERE name=?1 ORDER BY machine")
        .map_err(|_| Denial::ApprovalUnavailable)?;
    let machines = stmt
        .query_map([ship], |r| r.get::<_, String>(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(|_| Denial::ApprovalUnavailable)?;
    let [machine] = machines.as_slice() else {
        return Err(Denial::ApprovalUnavailable);
    };
    // No remote answer without the grant-bound durable prepare pin.
    if args["mode"] != "prepare" {
        return Err(Denial::ApprovalUnavailable);
    }
    let callable: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM machines WHERE name=?1 AND role='collects')",
            [machine],
            |r| r.get(0),
        )
        .map_err(|_| Denial::ApprovalUnavailable)?;
    if !callable {
        return Err(Denial::ApprovalUnavailable);
    }
    let incarnation = crate::machines::link_incarnation(conn, machine)
        .map_err(|_| Denial::ApprovalUnavailable)?;
    Ok(Some(Route {
        machine: machine.clone(),
        incarnation,
        ship: ship.into(),
        binding: None,
    }))
}

fn current(conn: &Connection, route: &Route) -> bool {
    crate::machines::same_incarnation(conn, &route.machine, &route.incarnation)
        && conn
            .query_row(
                "SELECT count(*)=1 AND min(machine)=?2 FROM machine_ships WHERE name=?1",
                params![route.ship, route.machine],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
        && !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND retired_ms IS NULL)",
                [&route.ship],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(true)
}

/// Grant-bound durable preparation receipt. No caller-selected route fields
/// reach this constructor, and no token or transport configuration goes on wire.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u8,
    origin: String,
    link: String,
    authority: String,
}
impl Binding {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && uuid::Uuid::parse_str(&self.origin).is_ok(),
            Denial::Arguments
        );
        ensure!(
            [&self.link, &self.authority]
                .iter()
                .all(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            Denial::Arguments
        );
        Ok(())
    }
}

pub(super) fn prepare(
    args: &Value,
    c: &DelegatedContext,
    route: &Route,
    recheck: &dyn Fn() -> Result<(), Denial>,
) -> Result<Value> {
    ensure!(
        args["mode"] == "prepare" && c.has("approval:answer"),
        Denial::Scope
    );
    let conn = crate::fleet::registry()?;
    schema(&conn)?;
    // Initialize the origin identity before transport, not under a publication
    // guard that could hold the registry while the subprocess waits.
    let binding = serde_json::to_value(Binding {
        version: 1,
        origin: super::reader_link::machine_id_in(&conn)?,
        link: route.incarnation.clone(),
        authority: crate::core::hash(&json!([c.client(), c.resource(), authority(c)])),
    })?;
    recheck()?;
    ensure!(current(&conn, route), Denial::ApprovalUnavailable);
    let result = crate::machines::approval_of(&route.machine, &route.incarnation,
        &json!({"version":1,"context":c.gateway(),"binding":binding,"receive_budget_ms":RECEIVE_BUDGET_MS,"args":args}))
        .map_err(|e| e.downcast_ref::<Denial>().copied().unwrap_or(Denial::ApprovalUnavailable));
    // Recheck even a remote refusal: a late response cannot retain authority
    // merely because the peer happened to deny the request too.
    recheck()?;
    ensure!(current(&conn, route), Denial::ApprovalUnavailable);
    let wire = result.map_err(|d| match d {
        Denial::ConfirmationRequired
        | Denial::ApprovalStale
        | Denial::Scope
        | Denial::Ship
        | Denial::RetryCapacity => d,
        _ => Denial::ApprovalUnavailable,
    })?;
    let mut result = validate_reply(&wire, &binding, args)?;
    pin(&conn, route, c, &binding, &result)?;
    recheck()?;
    ensure!(current(&conn, route), Denial::ApprovalUnavailable);
    result["origin_machine"] = json!(route.machine);
    result["answer_available"] = json!(true);
    Ok(result)
}

/// No writer is held over either call. The read-only probe distinguishes an
/// already unavailable link from a lost commit acknowledgment; it is not a
/// distributed authorization lease or a promise that a later send will work.
pub(super) fn answer(
    args: &Value,
    c: &DelegatedContext,
    route: &Route,
    door: &str,
    recheck: &dyn Fn() -> Result<(), Denial>,
) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    let binding = route.binding.as_ref().ok_or(Denial::ApprovalUnavailable)?;
    let mut args = args.clone();
    args["ship"] = json!(route.ship);
    let mut body = json!({"version":1,"context":c.gateway(),"binding":binding,
        "receive_budget_ms":RECEIVE_BUDGET_MS,"args":args,"probe":true});
    recheck()?;
    ensure!(current(&conn, route), Denial::ApprovalUnavailable);
    let ready = crate::machines::approval_of(&route.machine, &route.incarnation, &body)
        .map_err(|_| Denial::ApprovalUnavailable)?;
    ensure!(
        ready == json!({"version":1,"binding":binding,"ready":true}),
        Denial::ApprovalUnavailable
    );
    recheck()?;
    ensure!(current(&conn, route), Denial::ApprovalUnavailable);
    super::clients::audit(
        &conn,
        door,
        Some(c.client()),
        "approval.dispatch",
        "linked_answer",
    )?;
    body.as_object_mut().unwrap().remove("probe");
    let result = crate::machines::approval_of(&route.machine, &route.incarnation, &body)
        .map_err(|e| {
            e.downcast_ref::<Denial>()
                .copied()
                .unwrap_or(Denial::OutcomeUnknown)
        })
        .and_then(|wire| {
            validate_answer(&wire, binding, &args).map_err(|_| Denial::OutcomeUnknown)
        });
    // Even an explicit remote refusal cannot be published after losing release
    // authority: revoke after admission may have raced the origin's commit.
    if recheck().is_err() {
        super::clients::audit(
            &conn,
            door,
            Some(c.client()),
            "approval.revoked_after_dispatch",
            "outcome_unknown",
        )
        .map_err(|_| Denial::OutcomeUnknown)?;
        super::clients::audit(
            &conn,
            door,
            Some(c.client()),
            "approval.release",
            "outcome_unknown",
        )
        .map_err(|_| Denial::OutcomeUnknown)?;
        return Err(Denial::OutcomeUnknown.into());
    }
    let result = if current(&conn, route) {
        result
    } else {
        Err(Denial::OutcomeUnknown)
    };
    super::clients::audit(
        &conn,
        door,
        Some(c.client()),
        "approval.release",
        result.as_ref().err().map_or("released", |d| d.reason()),
    )
    .map_err(|_| Denial::OutcomeUnknown)?;
    result.map_err(Into::into)
}

fn validate_answer(wire: &Value, binding: &Value, args: &Value) -> Result<Value> {
    ensure!(
        fields(wire, &["version", "binding", "answered"])
            && wire["version"] == 1
            && wire["binding"] == *binding,
        Denial::OutcomeUnknown
    );
    let a = &wire["answered"];
    let r = &a["receipt"];
    ensure!(
        fields(a, &["id", "ship", "answered_via", "receipt"])
            && a["id"] == args["id"]
            && a["ship"] == args["ship"]
            && a["answered_via"] == "linked-chat-door"
            && fields(
                r,
                &[
                    "reply_id",
                    "digest",
                    "version",
                    "choice",
                    "approved",
                    "approval_id"
                ]
            )
            && r["digest"] == args["digest"]
            && r["version"] == args["version"]
            && r["choice"] == json!({"action":args["action"],"option_id":args["option_id"]})
            && r["reply_id"]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
            && r["approved"].is_boolean()
            && (if r["approved"] == true {
                r["approval_id"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("approval-") && s.len() <= 64)
            } else {
                r["approval_id"].is_null()
            }),
        Denial::OutcomeUnknown
    );
    if let Some(action) = args["action"].as_str() {
        ensure!(
            r["approved"] == (action == "approve"),
            Denial::OutcomeUnknown
        );
    }
    Ok(a.clone())
}

fn pin(
    conn: &Connection,
    route: &Route,
    c: &DelegatedContext,
    binding: &Value,
    result: &Value,
) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let now = crate::store::Store::now_ms();
    tx.execute(
        "DELETE FROM door_linked_approval_routes WHERE retain_until_ms<=?1",
        [now],
    )?;
    // One current challenge per selected route/grant/ask, matching the origin's
    // replacement semantics. Retention uses only the host clock: the peer's
    // absolute challenge expiry belongs to the peer, not a clock-skew test.
    tx.execute("DELETE FROM door_linked_approval_routes WHERE grant_id=?1 AND resource=?2 AND ship=?3 AND ask_id=?4",
        params![c.client(), c.resource(), route.ship, result["id"].as_str()])?;
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM door_linked_approval_routes WHERE grant_id=?1 AND resource=?2",
        params![c.client(), c.resource()],
        |r| r.get(0),
    )?;
    ensure!(count < PINS, Denial::RetryCapacity);
    ensure!(current(&tx, route), Denial::ApprovalUnavailable);
    tx.execute(
        "INSERT INTO door_linked_approval_routes VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            c.client(),
            c.resource(),
            result["digest"].as_str(),
            route.ship,
            result["id"].as_str(),
            route.machine,
            route.incarnation,
            binding.to_string(),
            authority(c),
            result["expires_ms"].as_i64(),
            now + PREPARE_LIFETIME_MS
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn fields(value: &Value, names: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|o| o.len() == names.len() && o.keys().all(|k| names.contains(&k.as_str())))
}

fn validate_reply(wire: &Value, binding: &Value, args: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        version: u8,
        binding: Value,
        prepared: Value,
    }
    let reply: Reply =
        serde_json::from_value(wire.clone()).map_err(|_| Denial::ApprovalUnavailable)?;
    ensure!(
        reply.version == 1 && reply.binding == *binding,
        Denial::ApprovalUnavailable
    );
    let p = &reply.prepared;
    ensure!(
        p.as_object().is_some_and(|o| o.len() == 11
            && o.keys().all(|k| [
                "digest",
                "version",
                "expires_ms",
                "ship",
                "id",
                "kind",
                "action_summary",
                "grant",
                "approve_unavailable",
                "question",
                "options"
            ]
            .contains(&k.as_str())))
            && p["id"] == args["id"]
            && p["ship"] == args["ship"]
            && matches!(p["kind"].as_str(), Some("request" | "decision"))
            && p["version"] == 4
            && p["expires_ms"].as_i64().is_some()
            && p["digest"]
                .as_str()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && p["action_summary"].as_str().is_some()
            && fields(
                &p["grant"],
                &[
                    "reply_id",
                    "command_prefix",
                    "ttl_secs",
                    "expires_ms",
                    "sentence"
                ]
            )
            && p["grant"]["reply_id"]
                .as_str()
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
            && p["grant"]["sentence"].as_str().is_some()
            && (p["grant"]["command_prefix"].is_null() || p["grant"]["command_prefix"].is_string())
            && p["grant"]["ttl_secs"].as_u64().is_some()
            && p["grant"]["expires_ms"].as_i64().is_some()
            && p["approve_unavailable"].is_boolean()
            && (p["question"].is_null() || p["question"].is_string())
            && p["options"]
                .as_array()
                .is_some_and(|options| options.len() <= 20
                    && options.iter().all(|o| fields(
                        o,
                        &["id", "label", "what_happens", "approves"]
                    ) && ["id", "label", "what_happens"]
                        .iter()
                        .all(|k| o[k].is_string())
                        && o["approves"].is_boolean()))
            && p.to_string().len() <= crate::mail::MAX_MESSAGE_BYTES,
        Denial::ApprovalUnavailable
    );
    Ok(reply.prepared)
}

/// Dedicated receiving gateway; operator authentication is enforced by the
/// machine gateway before entry. The context is admission, NOT captain proof.
pub(crate) fn serve(body: &Value) -> Result<Value> {
    let received = Instant::now();
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        version: u8,
        context: Value,
        binding: Binding,
        receive_budget_ms: u64,
        #[serde(default)]
        probe: bool,
        args: Value,
    }
    let wire: Wire = serde_json::from_value(body.clone()).map_err(|_| Denial::Arguments)?;
    ensure!(
        wire.version == 1 && (1..=RECEIVE_BUDGET_MS).contains(&wire.receive_budget_ms),
        Denial::Arguments
    );
    wire.binding.validate()?;
    super::approvals::validate_args(&wire.args)?;
    let c = DelegatedContext::from_gateway(&wire.context).map_err(|_| Denial::Arguments)?;
    ensure!(c.has("approval:answer"), Denial::Scope);
    let ship = wire.args["ship"].as_str().ok_or(Denial::Arguments)?;
    ensure!(c.allows(ship), Denial::Ship);
    ensure!(
        wire.binding.authority
            == crate::core::hash(&json!([c.client(), c.resource(), authority(&c)])),
        Denial::Arguments
    );
    let recheck = || check_budget(received.elapsed(), wire.receive_budget_ms);
    let binding = serde_json::to_value(wire.binding)?;
    if wire.probe {
        ensure!(wire.args["mode"] == "answer", Denial::Arguments);
        ensure!(
            super::members()?.iter().any(|(s, _)| s == ship),
            Denial::ApprovalUnavailable
        );
        recheck()?;
        return Ok(json!({"version":1,"binding":binding,"ready":true}));
    }
    if wire.args["mode"] == "prepare" {
        let prepared = super::approvals::prepare_linked(&wire.args, &c, &binding, &recheck)?;
        recheck()?;
        Ok(json!({"version":1,"binding":binding,"prepared":prepared}))
    } else {
        let receipt = super::approvals::answer_linked(&wire.args, &c, &binding, &recheck)?;
        // A post-commit budget failure must not claim a definite refusal.
        recheck().map_err(|_| Denial::OutcomeUnknown)?;
        Ok(json!({"version":1,"binding":binding,"answered":receipt}))
    }
}

#[cfg(test)]
mod tests;

fn check_budget(elapsed: Duration, budget_ms: u64) -> Result<(), Denial> {
    if elapsed < Duration::from_millis(budget_ms) {
        Ok(())
    } else {
        Err(Denial::ApprovalUnavailable)
    }
}
