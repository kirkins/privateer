//! A harness's own restriction, which no Privateer permission changes.
//!
//! Privateer controls one harness's approvals: Claude's, whose settings and
//! auto mode its allow rules and standing authorizations reach. A tool use
//! any other harness refused, or a command that ran but whose sandbox blocked
//! network or filesystem access, is no such denial: an Allow or Authorize
//! card for it changes nothing, and a policy change that withdrew the card
//! only brought the same failure back as a fresh one. So a ship's report of
//! one becomes one notice to the captain that names the harness, its sandbox
//! and the supported fix, and the ship is told plainly that no policy denied
//! it. Nothing waits on the notice.
//!
//! The notice's id is its cause: the ship, the harness, its sandbox mode and
//! network setting, and whether a sandbox or the harness's own settings
//! stopped it. While that cause holds, another report of it, from any task
//! and across any policy change, adds no mail. A changed cause (the captain
//! gave the ship network, say) is a new notice. No earlier mail is rewritten.
use crate::mail::CAPTAIN;
use crate::store::Store;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The harness whose approvals Privateer's allow rules and authorizations
/// reach.
const CLAUDE: &str = "claude";
/// What a payload names a harness it could not tell as.
pub(crate) const UNKNOWN: &str = "unknown";
/// What a notice shows of a sandbox mode or harness id: a short plain word.
const WORD_CHARS: usize = 64;

/// The harness a report came from, read from the ship's own record, never
/// from the report: a manifest's `id` (`bundled:codex` is `codex`), or none
/// when the record names no readable manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Harness {
    id: Option<String>,
}

impl Harness {
    /// Claude, as the source of a report says: a pane's auto-mode screen, or
    /// the denial line `claude_turn.py` prints in a job's log.
    pub(crate) fn claude() -> Self {
        Self {
            id: Some(CLAUDE.to_owned()),
        }
    }

    /// A harness id as a launch records it; a blank or odd one is unknown.
    pub(crate) fn of_id(id: &str) -> Self {
        let plain = !id.is_empty()
            && id.len() <= WORD_CHARS
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        Self {
            id: plain.then(|| id.to_ascii_lowercase()),
        }
    }

    /// The harness `ship`'s first mate runs, from its recorded manifest.
    pub(crate) fn of_first_mate(store: &Store, ship: &str) -> Result<Self> {
        let record = crate::mail::ship(store, ship)?.unwrap_or(Value::Null);
        let first_mate = &record["first_mate"];
        // A bundled manifest's id is its name; a local one's is read from it.
        let id = match first_mate["manifest"].as_str() {
            Some(reference) if reference.starts_with("bundled:") => {
                crate::bundled::harness(reference).unwrap_or_default()
            }
            _ => crate::activation::harness_id(first_mate),
        };
        Ok(Self::of_id(&id))
    }

    /// The lower-case id a payload carries: `unknown` when none.
    pub(crate) fn id(&self) -> &str {
        self.id.as_deref().unwrap_or(UNKNOWN)
    }

    /// How a card names it: `Codex`, or `The agent` when it is unknown.
    pub(crate) fn name(&self) -> String {
        match self.id.as_deref() {
            None => "The agent".to_owned(),
            Some("claude") => "Claude".to_owned(),
            Some("codex") => "Codex".to_owned(),
            Some(id) => {
                let mut chars = id.chars();
                chars
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            }
        }
    }

    /// Whether Privateer's allow rules and authorizations reach its
    /// approvals.
    pub(crate) fn controlled(&self) -> bool {
        self.id.as_deref() == Some(CLAUDE)
    }
}

/// Why a reported tool use did not run as asked, as `permissions.blocked`'s
/// optional `cause` says: refused by the harness's approval policy, or run
/// but restricted by its sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Policy,
    Sandbox,
}

impl Cause {
    pub(crate) fn parse(args: &Value) -> Result<Self> {
        Ok(match args.get("cause") {
            None | Some(Value::Null) => Self::Policy,
            Some(Value::String(cause)) if cause == "policy" => Self::Policy,
            Some(Value::String(cause)) if cause == "sandbox" => Self::Sandbox,
            Some(_) => bail!("cause must be \"policy\" or \"sandbox\""),
        })
    }
}

/// Whether a report goes to the captain as an Allow or Authorize decision:
/// only a policy refusal by the harness whose approvals Privateer controls.
pub(crate) fn decidable(harness: &Harness, cause: Cause) -> bool {
    harness.controlled() && cause == Cause::Policy
}

fn hex(text: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(text))
}

/// A sandbox mode as the first mate's options name it, or `default`.
fn sandbox_mode(options: &Value) -> String {
    options["sandbox"]
        .as_str()
        .filter(|mode| {
            !mode.is_empty()
                && mode.len() <= WORD_CHARS
                && mode
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        })
        .unwrap_or("default")
        .to_owned()
}

/// What stopped it and the supported fix, for the captain.
fn fix(harness: &Harness, sandboxed: bool, mode: &str, network: bool) -> String {
    let name = harness.name();
    match harness.id.as_deref() {
        None => "Which harness this ship runs could not be read from its first_mate manifest, so no allow rule or authorization is offered: they reach only Claude's approvals. Check the ship's first_mate manifest and options.".to_owned(),
        Some("codex") if !network && mode != "danger-full-access" => format!(
            "Codex {mode} has no network; the supported per-ship option is first_mate options network:true, see #724. Writes outside its writable folders are its sandbox's too: first_mate options sandbox chooses it."
        ),
        Some("codex") => format!(
            "Codex runs its commands in its own sandbox ({mode}), which first_mate options sandbox chooses; Privateer's allow rules and authorizations do not change it."
        ),
        Some(CLAUDE) => "Claude's sandbox is chosen by its own settings, not by Privateer's allow rules or authorizations; change it in the ship's Claude settings or first_mate options.".to_owned(),
        Some(_) if sandboxed => format!(
            "{name}'s sandbox ({mode}) is chosen by the ship's first_mate options; Privateer's allow rules and authorizations reach only Claude."
        ),
        Some(_) => format!(
            "{name}'s own approval settings refused it; Privateer's allow rules and authorizations reach only Claude. The ship's first_mate options choose {name}'s settings."
        ),
    }
}

/// Record a harness's restriction: one notice to the captain per cause, in
/// the caller's transaction. Returns what `permissions.blocked` answers,
/// with `restriction` the notice and `message` what the ship is told.
#[allow(clippy::too_many_arguments)]
pub(crate) fn report(
    store: &Store,
    ship: &str,
    task_id: &str,
    task: &Value,
    job_id: Option<&str>,
    harness: &Harness,
    cause: Cause,
    options: &Value,
    records: Vec<Value>,
    more: usize,
) -> Result<Value> {
    // Codex runs headless with no approval prompt, so what stops its command
    // is its sandbox, whatever the report says.
    let sandboxed = cause == Cause::Sandbox || harness.id() == "codex";
    let mode = sandbox_mode(options);
    let network = options["network"] == true;
    let kind = if sandboxed { "sandbox" } else { "settings" };
    let name = harness.name();
    let fix = fix(harness, sandboxed, &mode, network);
    // Only what it was: no rule or authorization is offered for it.
    let denials: Vec<Value> = records
        .iter()
        .map(|d| {
            let mut kept = json!({"tool": d["tool"], "preview": d["preview"]});
            for key in ["reason", "tool_use_id", "unidentified"] {
                if let Some(value) = d.get(key) {
                    kept[key] = value.clone();
                }
            }
            kept
        })
        .collect();
    let what = denials
        .iter()
        .map(|d| match d["tool"].as_str().filter(|t| !t.is_empty()) {
            Some(tool) => tool.to_owned(),
            None => "a tool".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let what = match more {
        0 => what,
        more => format!("{what} and {more} more"),
    };
    let previews = denials
        .iter()
        .map(|d| {
            let preview = d["preview"].as_str().unwrap_or("");
            match d["tool"].as_str().filter(|t| !t.is_empty()) {
                Some(tool) => format!("{tool}: {preview}"),
                None => preview.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    let net = if network { "network on" } else { "no network" };
    let headline = if sandboxed {
        format!("{ship}: {name}'s sandbox ({mode}, {net}) restricted {what}")
    } else {
        format!("{ship}: {name}'s own settings refused {what}")
    };
    let headline = crate::block_ask::clip(&headline, 200);
    let title = task["title"].as_str().unwrap_or(task_id);
    let job_line = job_id.map(|j| format!(" Job: {j}.")).unwrap_or_default();
    let body = format!(
        "{headline}. No Privateer permission denied it, so no allow rule or authorization is offered. {fix}\nWorking on {}.{job_line}\nRestricted: {previews}.\nThe ship carries on without it; nothing waits on an answer.",
        crate::block_ask::clip(&format!("{task_id}: {title}"), 200),
    );
    let key = hex(format!(
        "{ship}\0{}\0{mode}\0{network}\0{kind}",
        harness.id()
    ));
    let id = format!("restriction-{key}");
    let payload = json!({"type": "harness_restriction", "restriction": {
        "key": key, "ship": ship, "task_id": task_id, "job_id": job_id,
        "harness": harness.id(), "cause": kind, "sandbox": mode, "network": network,
        "denials": denials, "more": more, "fix": fix}});
    let fresh = crate::mail::record_runtime(
        store,
        &json!({"id": id, "from": ship, "to": CAPTAIN, "kind": "notice", "task_id": task_id,
            "body": body, "payload": payload, "urgency": "normal"}),
    )?;
    store.audit(
        "permissions.restriction",
        &json!({"id": id, "ship": ship, "task_id": task_id, "job_id": job_id,
            "harness": harness.id(), "cause": kind, "duplicate": !fresh}),
    )?;
    let stopped = if sandboxed {
        format!("{name}'s sandbox ({mode}, {net}) restricted it")
    } else {
        format!("{name}'s own settings refused it")
    };
    let told = if fresh { "has" } else { "already has" };
    let message = format!(
        "Your tool was not denied by policy: {stopped}, and no Privateer permission changes that. The captain {told} notice {id}; nothing waits on an answer, so do not report it again. Carry on without it, or ask the captain for the fix: {fix}"
    );
    Ok(
        json!({"request_id": null, "restriction": id, "duplicate": !fresh,
        "declined": false, "rules": what, "harness": harness.id(), "message": message}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_is_named_from_its_id_and_never_as_claude_by_default() {
        assert_eq!(Harness::of_id("codex").name(), "Codex");
        assert_eq!(Harness::of_id("codex").id(), "codex");
        assert_eq!(Harness::of_id("gemini").name(), "Gemini");
        assert_eq!(Harness::of_id("").name(), "The agent");
        assert_eq!(Harness::of_id("").id(), UNKNOWN);
        assert_eq!(Harness::of_id("a b").id(), UNKNOWN);
        assert!(Harness::of_id("claude").controlled());
        assert!(!Harness::of_id("codex").controlled());
        assert!(!Harness::of_id("").controlled());
        assert!(decidable(&Harness::claude(), Cause::Policy));
        assert!(!decidable(&Harness::claude(), Cause::Sandbox));
        assert!(!decidable(&Harness::of_id("codex"), Cause::Policy));
    }

    #[test]
    fn a_cause_is_policy_or_sandbox_only() {
        assert_eq!(Cause::parse(&json!({})).unwrap(), Cause::Policy);
        assert_eq!(
            Cause::parse(&json!({"cause": "sandbox"})).unwrap(),
            Cause::Sandbox
        );
        assert!(Cause::parse(&json!({"cause": "network"})).is_err());
        assert!(Cause::parse(&json!({"cause": true})).is_err());
    }

    /// A ship whose record names no manifest is reported as "The agent",
    /// with no rule offered, and its repeat adds nothing.
    #[test]
    fn an_unknown_harness_is_the_agent_and_its_repeat_is_one_notice() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        store
            .connection()
            .execute_batch(
                "INSERT INTO charters(id,data) VALUES('alpha','{}');
                 INSERT INTO tasks(id,charter_id,owner,title,status,parent,depth)
                 VALUES('alpha-duty','alpha','alpha','Duty','open',NULL,0);
                 INSERT INTO ships(name,record) VALUES('alpha','{}');",
            )
            .unwrap();
        let task = store.task("alpha-duty").unwrap();
        let harness = Harness::of_first_mate(&store, "alpha").unwrap();
        assert_eq!(harness.id(), UNKNOWN);
        let report = || {
            let (records, more) = crate::block_ask::records(
                &[json!({"tool_name": "Bash", "tool_input": {"command": "cargo publish"}})],
                None,
            );
            report(
                &store,
                "alpha",
                "alpha-duty",
                &task,
                None,
                &harness,
                Cause::Policy,
                &json!({}),
                records,
                more,
            )
            .unwrap()
        };
        let first = report();
        assert!(first["request_id"].is_null(), "{first}");
        assert_eq!(first["duplicate"], false);
        let id = first["restriction"].as_str().unwrap().to_owned();
        let notice = crate::mail::find(&store, &id).unwrap().unwrap();
        let body = notice["body"].as_str().unwrap();
        assert!(
            body.starts_with("alpha: The agent's own settings refused Bash"),
            "{body}"
        );
        assert!(!body.contains("Claude was denied"), "{body}");
        assert!(!body.contains("Bash(cargo publish"), "{body}");
        assert_eq!(notice["kind"], "notice");
        assert_eq!(notice["payload"]["type"], "harness_restriction");
        assert_eq!(notice["payload"]["restriction"]["harness"], UNKNOWN);
        assert!(notice["payload"].get("decision").is_none(), "{notice}");
        let again = report();
        assert_eq!(again["duplicate"], true, "{again}");
        assert_eq!(again["restriction"], id.as_str());
        let count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM mail WHERE recipient='captain'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn codex_without_network_is_told_the_supported_option() {
        let codex = Harness::of_id("codex");
        let text = fix(&codex, true, "workspace-write", false);
        assert!(
            text.contains("Codex workspace-write has no network")
                && text.contains("first_mate options network:true"),
            "{text}"
        );
        assert!(!fix(&codex, true, "workspace-write", true).contains("network:true"));
    }
}
