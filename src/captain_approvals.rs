//! Captain approvals: one step a ship asked the captain to OK, declared on
//! its request as `payload.approves`, recorded from the captain's own reply,
//! and told to Claude's auto-mode classifier at the ship's next first-mate
//! launch.
//!
//! The action is frozen in the request the captain read, and the stored body
//! says what a yes grants. A row is bound to its ship, its task and its
//! expiry, reaches `autoMode.allow` only, and dies with its task. Nothing a
//! ship sends after the request can create, widen, extend or move one.
//!
//! Every ship runs as the same OS user, so this is a workflow control that
//! keeps a confused agent from approving itself, not isolation: an agent that
//! writes the database directly is not stopped here.
use crate::store::{Caller, Store, required};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const DEFAULT_TTL_SECS: u64 = 86_400;
pub(crate) const MAX_TTL_SECS: u64 = 604_800;
/// Live approvals one ship may hold, as `tool_permissions.policy` accepts.
pub(crate) const MAX_LIVE: usize = 8;
const MAX_ACTION_CHARS: usize = 300;
const MAX_PREFIX_CHARS: usize = 120;

const POLICY_WORDS: &[&str] = &[
    "permission",
    "setting",
    ".claude",
    "charter",
    "automode",
    "allowedtools",
];
const CREDENTIAL_WORDS: &[&str] = &[
    ".ssh", ".aws", ".netrc", "id_rsa", ".env", "keychain", "token", "password",
];
const BROAD_WORDS: &[&str] = &[
    "anything",
    "everything",
    "all commands",
    "any command",
    "whatever",
    "always",
    "from now on",
];
/// Commands that run whatever follows them.
const WRAPPERS: &[&str] = &["env", "sudo", "xargs", "eval"];
/// Shells and interpreters, refused alone or running inline code.
const INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "node"];
/// Words that make a free-text reply something other than a plain yes.
const REFUSALS: &[&str] = &[
    "no", "not", "don't", "dont", "do not", "never", "stop", "wait", "hold", "later", "cancel",
    "deny", "decline", "reject", "instead", "unless", "except", "but", "only", "first", "if",
    "nope", "nah", "cannot", "can't", "won't", "until", "pause", "abort", "refuse", "maybe",
];

/// One line of text within `max` characters, trimmed, with no control
/// characters. Errors name the field, never the value.
fn line(value: &Value, field: &str, max: usize) -> Result<String> {
    let text = value
        .as_str()
        .with_context(|| format!("{field} must be a string"))?
        .trim();
    let len = text.chars().count();
    ensure!(
        (1..=max).contains(&len) && !text.chars().any(char::is_control),
        "{field} must be one line of 1 to {max} characters"
    );
    Ok(text.to_owned())
}

/// Whether `name` stands alone in `text`, not inside a longer word.
fn names(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let (text, name) = (text.to_lowercase(), name.to_lowercase());
    text.match_indices(&name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// The ships this machine knows besides `ship`: the fleet registry's, when
/// one exists, and this home's directory.
fn other_ships(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut ships: Vec<String> = {
        let mut statement = store.connection().prepare("SELECT name FROM ships")?;
        statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    // Read an existing registry only: a send must not create one.
    if crate::fleet::registry_path().is_ok_and(|p| p.is_file())
        && let Ok(conn) = crate::fleet::registry()
    {
        ships.extend(crate::fleet::members(&conn)?.into_iter().map(|m| m.name));
    }
    ships.retain(|name| name != ship);
    ships.sort();
    ships.dedup();
    Ok(ships)
}

/// The coarse send-time lint. The captain's reading is the real check.
fn lint(store: &Store, ship: &str, field: &str, text: &str) -> Result<()> {
    let lower = text.to_lowercase();
    if let Some(other) = other_ships(store, ship)?
        .into_iter()
        .find(|other| names(text, other))
    {
        bail!("{field} names ship {other}; an approval covers the asking ship's own work only");
    }
    ensure!(
        !POLICY_WORDS.iter().any(|w| lower.contains(w)),
        "{field} mentions policy or settings; ask the captain for those with permissions, not an approval"
    );
    ensure!(
        !CREDENTIAL_WORDS.iter().any(|w| lower.contains(w)),
        "{field} mentions credentials; an approval never covers them"
    );
    ensure!(
        !BROAD_WORDS.iter().any(|w| lower.contains(w)),
        "{field} is too broad; name the one step the captain is approving"
    );
    Ok(())
}

fn check_prefix(prefix: &str) -> Result<()> {
    let mut words = prefix.split_whitespace();
    let first = words.next().unwrap_or_default();
    let program = first.rsplit('/').next().unwrap_or(first);
    ensure!(
        !WRAPPERS.contains(&program),
        "approves.command_prefix cannot start with {program}, which runs any command"
    );
    let interpreter = INTERPRETERS.contains(&program) || program.starts_with("python");
    if interpreter {
        let rest: Vec<&str> = words.collect();
        ensure!(
            rest.iter().any(|w| !w.starts_with('-'))
                && !rest
                    .iter()
                    .any(|w| matches!(*w, "-c" | "-e" | "--eval" | "-m")),
            "approves.command_prefix cannot be just a shell or interpreter; name the script it runs"
        );
    }
    Ok(())
}

fn hours(ttl_secs: u64) -> String {
    match ttl_secs {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// Check and normalize `payload.approves` on a message a ship sends to the
/// captain. Returns the line the stored body gains, so every surface shows
/// what a yes grants; None when the message declares nothing.
pub(crate) fn declare(
    store: &Store,
    from: &str,
    to: &str,
    kind: &str,
    request_kind: Option<&str>,
    task_id: Option<&str>,
    payload: &mut Value,
) -> Result<Option<String>> {
    let Some(approves) = payload.get("approves").cloned() else {
        return Ok(None);
    };
    ensure!(
        matches!(kind, "request" | "decision"),
        "approves rides a request or a decision"
    );
    ensure!(
        to == crate::mail::CAPTAIN && from != crate::mail::CAPTAIN,
        "approves is a ship's ask to the captain"
    );
    ensure!(
        request_kind != Some("production_approval"),
        "a production approval is granted with approval.grant, never through approves"
    );
    ensure!(
        payload["type"] != "tool_permission"
            && ["block", "placement", "needs_hands"]
                .iter()
                .all(|k| payload.get(*k).is_none()),
        "approves cannot ride an ask the runtime raises"
    );
    let fields = approves.as_object().context("approves must be an object")?;
    ensure!(
        fields
            .keys()
            .all(|k| matches!(k.as_str(), "action" | "command_prefix" | "ttl_secs")),
        "approves takes only action, command_prefix and ttl_secs"
    );
    let action = line(
        fields
            .get("action")
            .context("approves.action is required")?,
        "approves.action",
        MAX_ACTION_CHARS,
    )?;
    lint(store, from, "approves.action", &action)?;
    let prefix = match fields.get("command_prefix").filter(|v| !v.is_null()) {
        Some(value) => {
            let prefix = line(value, "approves.command_prefix", MAX_PREFIX_CHARS)?;
            check_prefix(&prefix)?;
            lint(store, from, "approves.command_prefix", &prefix)?;
            Some(prefix)
        }
        None => None,
    };
    let ttl_secs = match fields.get("ttl_secs").filter(|v| !v.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|s| (1..=MAX_TTL_SECS).contains(s))
            .with_context(|| format!("approves.ttl_secs must be 1..{MAX_TTL_SECS}"))?,
        None => DEFAULT_TTL_SECS,
    };
    let task = task_id.context("approves needs the task_id of the asking ship's open task")?;
    let task_record = store.task(task)?;
    let owner = task_record["owner"].as_str().unwrap_or_default();
    ensure!(
        owner == from,
        "approves needs a task {from} owns; {task} is {owner}'s"
    );
    store
        .active_charter(task)
        .with_context(|| format!("approves needs an open task; {task} is not"))?;
    ensure!(
        live(store, from, Store::now_ms())?.len() < MAX_LIVE,
        "{from} already holds {MAX_LIVE} live captain approvals; finish or let one expire first"
    );
    let mut normalized = json!({"action": action, "ttl_secs": ttl_secs});
    if let Some(prefix) = prefix {
        normalized["command_prefix"] = json!(prefix);
    }
    payload["approves"] = normalized;
    Ok(Some(format!(
        "Approving lets {from} do this for {}: {action}",
        hours(ttl_secs)
    )))
}

/// Check a decision's `approves` option flags: booleans only, only when the
/// decision declares approves, and then on at least one option.
pub(crate) fn check_options(decision: &Value, declared: bool) -> Result<()> {
    let mut marked = 0;
    for option in decision["options"].as_array().into_iter().flatten() {
        if let Some(flag) = option.get("approves") {
            ensure!(
                declared,
                "decision.options approves applies only with payload.approves"
            );
            marked += usize::from(
                flag.as_bool()
                    .context("decision.options approves must be true or false")?,
            );
        }
    }
    ensure!(
        !declared || marked > 0,
        "a decision with payload.approves marks at least one option approves: true"
    );
    Ok(())
}

/// Whether the reply text holds a word that makes it something other than a
/// plain yes.
fn hedged(text: &str) -> Option<String> {
    let lower = text.to_lowercase().replace('’', "'");
    if lower.contains('?') {
        return Some("?".into());
    }
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect();
    REFUSALS
        .iter()
        .find(|refusal| {
            let parts: Vec<&str> = refusal.split(' ').collect();
            words.windows(parts.len()).any(|w| w == parts.as_slice())
        })
        .map(|w| (*w).to_owned())
}

/// What the captain's reply to `request` approves, recorded in the caller's
/// write transaction at `now`. Only the home holding the original request
/// (not a desk copy) records; elsewhere, and for a request that declares
/// nothing, this returns None. Otherwise the result is the reply's
/// `payload.approval`.
pub(crate) fn record(
    store: &Store,
    request: &Value,
    reply_id: &str,
    body: &str,
    payload: &Value,
    now: i64,
) -> Result<Option<Value>> {
    let Some(approves) = request["payload"].get("approves") else {
        return Ok(None);
    };
    if request["delivery"] == "received" {
        return Ok(None);
    }
    let ship = request["from"].as_str().unwrap_or_default();
    let skip = |why: &str| -> Result<Option<Value>> {
        store.audit(
            "permissions.approval_skipped",
            &json!({"ship": ship, "request": request["id"], "reply": reply_id, "why": why}),
        )?;
        Ok(Some(json!({"recorded": false, "why": why})))
    };
    if request["state"] != "open" {
        return skip("the request was closed before this reply, so nothing was approved");
    }
    if payload.get("fanned_from").is_some() || payload["operator"] != true {
        return skip(
            "only the captain's own reply approves; a relay, a bridge or a copy approves nothing",
        );
    }
    let text = [body, payload["answer"]["text"].as_str().unwrap_or("")]
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let source = if request["kind"] == "decision" {
        let chosen = payload["answer"]["option_id"].as_str();
        let option = request["payload"]["decision"]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| chosen.is_some() && o["id"].as_str() == chosen);
        let Some(option) = option.filter(|o| o["approves"] == true) else {
            return skip(
                "the captain did not choose an option that approves; free text on a decision approves nothing",
            );
        };
        // The door may send the chosen label back as the answer's text.
        if text != option["label"].as_str().unwrap_or_default()
            && let Some(word) = hedged(&text)
        {
            return skip(&format!(
                "the captain's words carry \"{word}\", so nothing was approved; ask again"
            ));
        }
        "option"
    } else {
        if text.is_empty() {
            return skip("the captain's reply is empty, so nothing was approved; ask again");
        }
        if let Some(word) = hedged(&text) {
            return skip(&format!(
                "the captain's reply carries \"{word}\", so it is not a plain yes and nothing was approved; ask again with a decision"
            ));
        }
        "reply"
    };
    let task = request["task_id"].as_str().unwrap_or_default();
    let open = store
        .task(task)
        .is_ok_and(|t| t["owner"] == ship && store.active_charter(task).is_ok());
    if !open {
        return skip("the request's task is no longer open, so nothing was approved");
    }
    if live(store, ship, now)?.len() >= MAX_LIVE {
        return skip(&format!(
            "{ship} already holds {MAX_LIVE} live captain approvals, so nothing was approved"
        ));
    }
    let action = approves["action"].as_str().unwrap_or_default();
    let ttl = approves["ttl_secs"]
        .as_u64()
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS);
    if action.is_empty() {
        return skip("the request names no action, so nothing was approved");
    }
    let id = format!("approval-{}", uuid::Uuid::new_v4().simple());
    let expires_ms = now + ttl as i64 * 1000;
    store.connection().execute(
        "INSERT INTO captain_approvals(id,ship,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms,source)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            id,
            ship,
            task,
            request["id"].as_str().unwrap_or_default(),
            reply_id,
            action,
            approves["command_prefix"].as_str(),
            now,
            expires_ms,
            source
        ],
    )?;
    store.audit(
        "permissions.approval_recorded",
        &json!({"id": id, "ship": ship, "task": task, "request": request["id"],
            "reply": reply_id, "action": action, "expires_ms": expires_ms, "source": source}),
    )?;
    Ok(Some(
        json!({"recorded": true, "id": id, "action": action, "expires_ms": expires_ms}),
    ))
}

struct Row {
    id: String,
    task: String,
    request: String,
    reply: String,
    action: String,
    expires_ms: i64,
}

/// A ship's live approvals: not revoked, not expired, and on a task that is
/// still open under an active charter.
fn live(store: &Store, ship: &str, now: i64) -> Result<Vec<Row>> {
    let mut statement = store.connection().prepare(
        "SELECT id,task_id,request_id,reply_id,action,expires_ms FROM captain_approvals
         WHERE ship=?1 AND revoked_ms IS NULL AND expires_ms>?2 ORDER BY created_ms, id",
    )?;
    let rows = statement
        .query_map(params![ship, now], |r| {
            Ok(Row {
                id: r.get(0)?,
                task: r.get(1)?,
                request: r.get(2)?,
                reply: r.get(3)?,
                action: r.get(4)?,
                expires_ms: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|row| store.active_charter(&row.task).is_ok())
        .collect())
}

/// What the auto-mode classifier is told for a ship's first-mate launch: one
/// sentence per live approval.
pub(crate) fn sentences(store: &Store, ship: &str, now: i64) -> Result<Vec<String>> {
    Ok(live(store, ship, now)?
        .into_iter()
        .take(MAX_LIVE)
        .map(|row| {
            format!(
                "The captain approved this in reply {} to request {}, for ship {ship} only, until {}: {}",
                row.reply,
                row.request,
                crate::routes::utc_time(row.expires_ms),
                row.action
            )
        })
        .collect())
}

fn left(ms: i64) -> String {
    let minutes = (ms / 60_000).max(1);
    if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// Bearings lines for a ship's live approvals, and how to ask for one.
pub(crate) fn bearings_lines(store: &Store, ship: &str) -> Result<Vec<String>> {
    let now = Store::now_ms();
    let mut lines: Vec<String> = live(store, ship, now)?
        .into_iter()
        .map(|row| {
            format!(
                "Captain approval {} ({} left): {}",
                row.id,
                left(row.expires_ms - now),
                row.action
            )
        })
        .collect();
    lines.push(
        "Free-text replies count only to requests that carry approves; ask with approves when you need a step approved."
            .into(),
    );
    Ok(lines)
}

fn list(store: &Store, args: &Value) -> Result<Value> {
    let ship = required(args, "ship")?;
    crate::store::identifier(ship)?;
    let now = Store::now_ms();
    let live: Vec<String> = live(store, ship, now)?.into_iter().map(|r| r.id).collect();
    let mut statement = store.connection().prepare(
        "SELECT id,task_id,request_id,reply_id,action,command_prefix,created_ms,expires_ms,revoked_ms,source
         FROM captain_approvals WHERE ship=?1 ORDER BY created_ms DESC, id LIMIT 100",
    )?;
    let approvals = statement
        .query_map([ship], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "ship": ship, "task_id": r.get::<_, String>(1)?,
                "request_id": r.get::<_, String>(2)?, "reply_id": r.get::<_, String>(3)?,
                "action": r.get::<_, String>(4)?, "command_prefix": r.get::<_, Option<String>>(5)?,
                "created_ms": r.get::<_, i64>(6)?, "expires_ms": r.get::<_, i64>(7)?,
                "revoked_ms": r.get::<_, Option<i64>>(8)?, "source": r.get::<_, String>(9)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|mut row| {
            row["live"] = json!(live.iter().any(|id| row["id"] == id.as_str()));
            row
        })
        .collect::<Vec<_>>();
    Ok(json!({"ship": ship, "approvals": approvals}))
}

fn revoke(store: &Store, args: &Value) -> Result<Value> {
    store.caller().require_operator("captain_approval.revoke")?;
    let id = required(args, "id")?;
    let revoked: Option<Option<i64>> = store
        .connection()
        .query_row(
            "SELECT revoked_ms FROM captain_approvals WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(revoked) = revoked else {
        bail!("no captain approval {id}");
    };
    if let Some(revoked_ms) = revoked {
        return Ok(json!({"id": id, "revoked_ms": revoked_ms, "already": true}));
    }
    let now = Store::now_ms();
    store.connection().execute(
        "UPDATE captain_approvals SET revoked_ms=?2 WHERE id=?1 AND revoked_ms IS NULL",
        params![id, now],
    )?;
    store.audit("permissions.approval_revoked", &json!({"id": id}))?;
    Ok(json!({"id": id, "revoked_ms": now, "already": false}))
}

pub fn dispatch(store: &mut Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "captain_approval.list" => list(store, &args),
        "captain_approval.revoke" => {
            ensure!(
                *store.caller() == Caller::Operator,
                "captain_approval.revoke is an operator command; agents cannot run it"
            );
            revoke(store, &args)
        }
        _ => bail!("unknown command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_yes_passes_and_a_hedge_names_its_word() {
        for yes in ["Submit build 55", "yes", "Go ahead.", "ok", "Approved"] {
            assert_eq!(hedged(yes), None, "{yes}");
        }
        for (reply, word) in [
            ("no", "no"),
            ("not yet", "not"),
            ("wait for CI", "wait"),
            ("ok?", "?"),
            ("Don’t", "don't"),
            ("yes but only staging", "but"),
            ("do it if CI is green", "if"),
        ] {
            assert_eq!(hedged(reply).as_deref(), Some(word), "{reply}");
        }
        // A word inside a longer one is not a refusal.
        assert_eq!(hedged("notify the butler"), None);
    }

    #[test]
    fn a_prefix_must_name_more_than_a_shell_or_wrapper() {
        for bad in [
            "bash",
            "sh -c",
            "python3",
            "python3 -c",
            "/usr/bin/env make",
            "sudo make deploy",
            "xargs rm",
            "node -e",
            "python -m http.server",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
        for good in [
            "make deploy",
            "python3 scripts/release.py",
            "bash ./ship.sh",
        ] {
            check_prefix(good).unwrap();
        }
    }

    #[test]
    fn a_ship_name_counts_only_as_a_whole_word() {
        assert!(names("Deploy infra's build", "infra"));
        assert!(names("restart INFRA.", "infra"));
        assert!(!names("Deploy infrastructure", "infra"));
        assert!(!names("Deploy my-infra-box", "infra"));
    }

    #[test]
    fn a_received_copy_records_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        let request = json!({"id": "r1", "from": "apps", "kind": "request", "delivery": "received",
            "task_id": "t", "payload": {"approves": {"action": "Submit build 55", "ttl_secs": 60}}});
        let outcome = record(
            &store,
            &request,
            "reply-1",
            "yes",
            &json!({"operator": true}),
            1,
        );
        assert_eq!(outcome.unwrap(), None);
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM captain_approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
