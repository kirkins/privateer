//! Dormant, home-local deployment policy. No dispatched path authorizes or starts
//! an upload. Roles/terminal consent are same-account workflow controls, not an OS
//! security boundary. See docs/design/standing-deploy-grants.md.
use crate::store::{Caller, Store, identifier};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

pub(crate) fn init(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS standing_deploy_grants(
            id TEXT PRIMARY KEY, grantee TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0),
            spec TEXT NOT NULL, created_by TEXT NOT NULL, created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL, paused_ms INTEGER, revoked_ms INTEGER);
         CREATE INDEX IF NOT EXISTS standing_deploy_grants_grantee
            ON standing_deploy_grants(grantee,id);",
    )?;
    ledger::init(store)?;
    Ok(())
}

// A claim checks an existing immutable use, never debits a second use. Only
// the ledger can construct Recorded; a caller cannot opt out of budget checks.
fn deployment_authority(
    store: &Store,
    grant: &Value,
    task_id: &str,
    used: Option<&ledger::Recorded>,
) -> Result<Value> {
    visible(store, grant)?;
    ensure!(
        grant["state"] == "active" && grant["production"] == false,
        "only active nonproduction grants can be consumed"
    );
    let grantee = crate::store::required(grant, "grantee")?;
    hosted_grantee(store, grantee)?;
    identifier(task_id)?;
    let task = store.task(task_id)?;
    ensure!(
        task["owner"] == grantee,
        "deployment task must be owned by grantee"
    );
    let charter = store.authorize(task_id, "deploy_nonproduction")?;
    if let Some(used) = used {
        used.budget(store, grant, task_id)?;
    } else {
        ledger::budget(store, grant)?;
    }
    Ok(json!({"task_digest":crate::core::hash(&task),"charter_digest":crate::core::hash(&charter)}))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    id: String,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactRule {
    kind: String,
    repository_id: String,
    branch_ref: String,
    required_checks: Vec<String>,
    app_id: String,
    workflow_path: String,
    event: String,
    provenance_mode: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    id: String,
    expected_revision: i64,
    grantee: String,
    environment: String,
    production: bool,
    artifact_rule: ArtifactRule,
    verifier: Binding,
    adapter: Binding,
    notify: bool,
    note: Option<String>,
    expires_ms: Option<i64>,
    max_uses: Option<i64>,
    wait_window_ms: Option<i64>,
}

fn text(value: &str, name: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "{name} must be nonempty, at most {max} bytes, without control characters"
    );
    Ok(())
}

impl Put {
    fn validate(&self, created_ms: i64) -> Result<()> {
        for id in [&self.id, &self.grantee, &self.environment] {
            identifier(id)?;
            text(id, "identifier", 160)?;
        }
        ensure!(
            self.expected_revision >= 0,
            "expected_revision must be nonnegative"
        );
        let rule = &self.artifact_rule;
        ensure!(
            rule.kind == "green_branch_commit",
            "unsupported artifact rule kind"
        );
        text(&rule.repository_id, "repository_id", 512)?;
        text(&rule.app_id, "app_id", 160)?;
        text(&rule.branch_ref, "branch_ref", 256)?;
        ensure!(
            rule.branch_ref.starts_with("refs/heads/")
                && rule.branch_ref.len() > "refs/heads/".len()
                && !rule
                    .branch_ref
                    .contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
                && !rule.branch_ref.contains("..")
                && !rule.branch_ref.contains("@{")
                && rule.branch_ref.split('/').all(|p| !p.is_empty()
                    && !p.starts_with('.')
                    && !p.ends_with('.')
                    && !p.ends_with(".lock")),
            "branch_ref must be a full branch ref"
        );
        text(&rule.workflow_path, "workflow_path", 256)?;
        ensure!(
            !rule.workflow_path.starts_with('/')
                && !rule.workflow_path.contains('\\')
                && rule
                    .workflow_path
                    .split('/')
                    .all(|p| !matches!(p, "" | "." | "..")),
            "workflow_path must be repository-relative"
        );
        ensure!(rule.event == "push", "only push check runs are eligible");
        ensure!(
            matches!(
                rule.provenance_mode.as_str(),
                "ci_attestation" | "runtime_build"
            ),
            "unsupported provenance_mode"
        );
        ensure!(
            !rule.required_checks.is_empty() && rule.required_checks.len() <= 32,
            "required_checks holds 1..32 names"
        );
        let mut seen = std::collections::HashSet::new();
        for check in &rule.required_checks {
            text(check, "check name", 128)?;
            ensure!(seen.insert(check), "duplicate required check");
        }
        for binding in [&self.verifier, &self.adapter] {
            identifier(&binding.id)?;
            text(&binding.id, "binding id", 160)?;
            ensure!(
                binding.sha256.len() == 64
                    && binding
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "binding sha256 must be 64 lowercase hex digits"
            );
        }
        if let Some(note) = &self.note {
            text(note, "note", 2048)?;
        }
        if let Some(expiry) = self.expires_ms {
            ensure!(
                expiry > Store::now_ms(),
                "the grant would already be expired"
            );
            let days = if self.production { 90 } else { 365 };
            ensure!(
                expiry <= created_ms.saturating_add(days * 86_400_000),
                "expires_ms exceeds the {days}-day lifetime from creation"
            );
        }
        for (name, value) in [
            ("max_uses", self.max_uses),
            ("wait_window_ms", self.wait_window_ms),
        ] {
            if let Some(value) = value {
                ensure!(value > 0, "{name} must be positive");
            }
        }
        if self.production {
            ensure!(
                self.notify
                    && self.expires_ms.is_some()
                    && self.max_uses.is_some()
                    && self.wait_window_ms.is_some(),
                "production requires notify, expires_ms, max_uses and wait_window_ms"
            );
            ensure!(
                self.max_uses <= Some(100),
                "production max_uses exceeds 100"
            );
            ensure!(
                (300_000..=86_400_000).contains(&self.wait_window_ms.unwrap()),
                "production wait_window_ms must be 5 minutes to 24 hours"
            );
        }
        Ok(())
    }
}

fn find(store: &Store, id: &str) -> Result<Option<Value>> {
    store
        .connection()
        .query_row(
            "SELECT spec,revision,created_by,created_ms,updated_ms,paused_ms,revoked_ms
         FROM standing_deploy_grants WHERE id=?1",
            [id],
            |row| {
                let spec: String = row.get(0)?;
                Ok((
                    spec,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .optional()?
        .map(
            |(spec, revision, creator, created, updated, paused, revoked)| {
                let mut value: Value =
                    serde_json::from_str(&spec).context("corrupt deployment grant")?;
                value["revision"] = json!(revision);
                value["created_by"] = json!(creator);
                value["created_ms"] = json!(created);
                value["updated_ms"] = json!(updated);
                value["paused_ms"] = json!(paused);
                value["revoked_ms"] = json!(revoked);
                value["state"] = json!(if revoked.is_some() {
                    "revoked"
                } else if paused.is_some() {
                    "paused"
                } else if value["expires_ms"]
                    .as_i64()
                    .is_some_and(|e| e <= Store::now_ms())
                {
                    "expired"
                } else {
                    "active"
                });
                value["consumption_enabled"] = json!(false);
                Ok(value)
            },
        )
        .transpose()
}

fn known(store: &Store, id: &str) -> Result<Value> {
    identifier(id)?;
    find(store, id)?.with_context(|| format!("unknown deployment grant {id}"))
}

fn revision(value: &Value, expected: i64) -> Result<i64> {
    ensure!(
        value["revision"].as_i64() == Some(expected),
        "deployment grant revision changed"
    );
    expected
        .checked_add(1)
        .context("deployment grant revision exhausted")
}

fn captain(store: &Store, command: &str) -> Result<()> {
    store.caller().require_operator(command)?;
    ensure!(
        store.crew().is_none(),
        "deployment grant writes are captain-only"
    );
    Ok(())
}

/// Recheck this at consumption too: moving a home must not transfer authority.
pub(crate) fn hosted_grantee(store: &Store, grantee: &str) -> Result<()> {
    crate::context::local_ship(store, grantee).context("grantee must be a ship hosted by this home")
}

fn consent_summary(spec: &Value, previous: Option<&Value>) -> Value {
    match previous {
        None => json!({"create": spec}),
        Some(old) => {
            let changes: serde_json::Map<String, Value> = spec
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, value)| key.as_str() != "expected_revision" && old[*key] != **value)
                .map(|(key, value)| (key.clone(), json!({"from": old[key], "to": value})))
                .collect();
            json!({"id":spec["id"], "replaces_revision":old["revision"], "changes":changes})
        }
    }
}

fn replacement_revision(current: Option<&Value>, expected: i64) -> Result<i64> {
    if let Some(old) = current {
        ensure!(
            old["revoked_ms"].is_null(),
            "revoked deployment grant cannot be replaced"
        );
        ensure!(
            old["paused_ms"].is_null(),
            "resume the paused deployment grant first"
        );
        revision(old, expected)
    } else {
        ensure!(
            expected == 0,
            "new deployment grant requires expected_revision 0"
        );
        Ok(1)
    }
}

/// Consent is obtained before locking; all lifecycle/revision/expiry conditions
/// are rechecked under the writer lock afterwards. Only the CLI calls this in
/// production; tests supply a consent callback, not a protocol bypass flag.
fn put(store: &Store, args: Value, consent: impl FnOnce(&Value) -> Result<()>) -> Result<Value> {
    captain(store, "deploy_grant.put")?;
    ensure!(
        !store.delegated_audit(),
        "deployment grant creation cannot be delegated"
    );
    ensure!(
        serde_json::to_vec(&args)?.len() <= 16_384,
        "deployment grant is too large"
    );
    let request: Put = serde_json::from_value(args).context("invalid deployment grant")?;
    let previous = find(store, &request.id)?;
    replacement_revision(previous.as_ref(), request.expected_revision)?;
    let created_ms = previous
        .as_ref()
        .and_then(|old| old["created_ms"].as_i64())
        .unwrap_or_else(Store::now_ms);
    request.validate(created_ms)?;
    hosted_grantee(store, &request.grantee)?;
    let mut spec = serde_json::to_value(&request)?;
    consent(&consent_summary(&spec, previous.as_ref()))?;
    let tx = store.write_transaction()?;
    request.validate(created_ms)?;
    hosted_grantee(store, &request.grantee)?;
    let current = find(store, &request.id)?;
    ensure!(
        current == previous,
        "deployment grant changed during consent"
    );
    let now = Store::now_ms();
    let next = replacement_revision(current.as_ref(), request.expected_revision)?;
    spec.as_object_mut().unwrap().remove("expected_revision");
    tx.execute(
        "INSERT INTO standing_deploy_grants(id,grantee,revision,spec,created_by,created_ms,updated_ms)
         VALUES(?1,?2,?3,?4,'operator',?5,?5) ON CONFLICT(id) DO UPDATE SET
         grantee=excluded.grantee,revision=excluded.revision,spec=excluded.spec,updated_ms=excluded.updated_ms",
        params![request.id, request.grantee, next, spec.to_string(), now],
    )?;
    let result = known(store, &request.id)?;
    store.audit("deploy_grant.put", &json!({"grant":result}))?;
    tx.commit()?;
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    id: String,
    expected_revision: i64,
}

fn change(store: &Store, command: &str, args: Value) -> Result<Value> {
    captain(store, command)?;
    let change: Change = serde_json::from_value(args)?;
    let tx = store.write_transaction()?;
    let old = known(store, &change.id)?;
    ensure!(
        old["revoked_ms"].is_null(),
        "revoked deployment grant is irreversible"
    );
    let next = revision(&old, change.expected_revision)?;
    let now = Store::now_ms();
    let (paused, revoked) = match command {
        "deploy_grant.pause" => (Some(now), None),
        "deploy_grant.resume" => {
            ensure!(
                !old["paused_ms"].is_null(),
                "deployment grant is not paused"
            );
            (None, None)
        }
        "deploy_grant.revoke" => (old["paused_ms"].as_i64(), Some(now)),
        _ => bail!("unknown deployment grant change"),
    };
    tx.execute("UPDATE standing_deploy_grants SET revision=?2,updated_ms=?3,paused_ms=?4,revoked_ms=?5 WHERE id=?1",
        params![change.id, next, now, paused, revoked])?;
    let result = known(store, &change.id)?;
    store.audit(command, &json!({"grant":result}))?;
    tx.commit()?;
    Ok(result)
}

fn visible(store: &Store, grant: &Value) -> Result<()> {
    if let Caller::Agent { actor } = store.caller() {
        ensure!(
            grant["grantee"].as_str() == Some(actor),
            "unknown deployment grant"
        );
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    grantee: Option<String>,
    after_id: Option<String>,
    limit: Option<i64>,
}

fn list(store: &Store, args: Value) -> Result<Value> {
    let request: List = serde_json::from_value(args)?;
    let limit = request.limit.unwrap_or(50);
    ensure!((1..=100).contains(&limit), "limit must be 1..100");
    for name in [&request.grantee, &request.after_id].into_iter().flatten() {
        identifier(name)?;
    }
    let grantee = match store.caller() {
        Caller::Operator => request.grantee.as_deref(),
        Caller::Agent { actor } => {
            ensure!(
                request.grantee.as_deref().is_none_or(|g| g == actor),
                "agents list only their own deployment grants"
            );
            Some(actor.as_str())
        }
    };
    let mut query = store.connection().prepare(
        "SELECT id FROM standing_deploy_grants
        WHERE (?1 IS NULL OR grantee=?1) AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3",
    )?;
    let ids = query
        .query_map(params![grantee, request.after_id, limit + 1], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let grants = ids
        .iter()
        .take(limit as usize)
        .map(|id| known(store, id))
        .collect::<Result<Vec<_>>>()?;
    let next = if ids.len() > limit as usize {
        grants.last().map(|g| g["id"].clone())
    } else {
        None
    };
    Ok(json!({"grants":grants,"next_after_id":next,"consumption_enabled":false}))
}

pub(crate) const ACCEPTED: crate::Accepted = &[
    ("deploy_grant.list", &["grantee", "after_id", "limit"]),
    ("deploy_grant.pause", &["id", "expected_revision"]),
    (
        "deploy_grant.put",
        &[
            "id",
            "expected_revision",
            "grantee",
            "environment",
            "production",
            "artifact_rule",
            "verifier",
            "adapter",
            "notify",
            "note",
            "expires_ms",
            "max_uses",
            "wait_window_ms",
        ],
    ),
    ("deploy_grant.resume", &["id", "expected_revision"]),
    ("deploy_grant.revoke", &["id", "expected_revision"]),
    ("deploy_grant.show", &["id"]),
];

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    // Also validate direct module calls, not only the outer dispatcher.
    crate::check_dispatch(store, command, &args)?;
    match command {
        "deploy_grant.put" => {
            captain(store, command)?;
            bail!("deploy_grant.put requires captain terminal CLI consent; not RPC, door or relay")
        }
        "deploy_grant.pause" | "deploy_grant.resume" | "deploy_grant.revoke" => {
            change(store, command, args)
        }
        "deploy_grant.show" => {
            let id = crate::store::required(&args, "id")?;
            identifier(id)?;
            let grant = find(store, id)?.context("unknown deployment grant")?;
            visible(store, &grant)?;
            Ok(grant)
        }
        "deploy_grant.list" => list(store, args),
        _ => bail!("unknown deployment grant command {command}"),
    }
}

fn terminal(summary: &Value) -> Result<()> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("deployment grant creation requires captain controlling terminal consent")?;
    unsafe { libc::tcflush(std::os::fd::AsRawFd::as_raw_fd(&tty), libc::TCIFLUSH) };
    writeln!(
        tty,
        "Dormant deployment grant (no upload authority enabled):\n{}\nType deploy-grant to confirm:",
        serde_json::to_string_pretty(summary)?
    )?;
    tty.flush()?;
    let mut line = String::new();
    BufReader::new(tty.take(128)).read_line(&mut line)?;
    ensure!(
        line.trim() == "deploy-grant",
        "deployment grant consent not confirmed"
    );
    Ok(())
}

/// Separate CLI entry: no RPC/door/relay dispatch can create or widen a grant.
pub fn cli(home: &Path, args: Value, caller: Caller) -> Result<Value> {
    caller.require_operator("deploy_grant.put")?;
    for marker in ["PRIVATEER_ROLE", "PRIVATEER_JOB"] {
        ensure!(
            std::env::var_os(marker).is_none(),
            "deployment grant creation is outside agent/job sessions"
        );
    }
    let store = Store::open_as(home, caller)?;
    crate::check_dispatch(&store, "deploy_grant.put", &args)?;
    put(&store, args, terminal)
}

#[allow(
    dead_code,
    reason = "Dormant claim/recovery; no deployment dispatcher is registered"
)]
mod claims;
mod ledger;
pub mod verifier;

#[cfg(test)]
mod tests;
