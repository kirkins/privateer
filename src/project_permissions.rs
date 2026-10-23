//! Manifest-only proposals and captain-owned, directory-scoped Claude grants.
use crate::store::{Caller, Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[cfg(test)]
mod tests;

const CARGO: &[&str] = &[
    "Bash(cargo build *)",
    "Bash(cargo check *)",
    "Bash(cargo test *)",
    "Bash(cargo fmt *)",
    "Bash(cargo clippy *)",
];
const OPTIONAL: &[&str] = &[
    "Bash(npm run build *)",
    "Bash(npm test *)",
    "Bash(make build *)",
    "Bash(make test *)",
];

pub(crate) fn init(conn: &rusqlite::Connection) -> Result<()> {
    if has_identity_time(conn)? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // Another opener may have completed the migration while we waited.
    if has_identity_time(&tx)? {
        tx.commit()?;
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_permissions(
        project TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, device TEXT NOT NULL,
        inode TEXT NOT NULL, proposal TEXT NOT NULL, accepted TEXT NOT NULL DEFAULT '[]',
        state TEXT NOT NULL DEFAULT 'pending', ask TEXT UNIQUE, home TEXT NOT NULL,
        identity_time TEXT);",
    )?;
    if !has_identity_time(&tx)? {
        // Existing grants lack replacement protection. Leave them fail-closed
        // until the captain explicitly accepts the project again.
        tx.execute(
            "ALTER TABLE project_permissions ADD COLUMN identity_time TEXT",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn has_identity_time(conn: &rusqlite::Connection) -> Result<bool> {
    Ok(conn
        .prepare("PRAGMA table_info(project_permissions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "identity_time"))
}

fn identity_note(time: Option<&str>) -> Option<&'static str> {
    time.filter(|time| time.starts_with("change:")).map(|_| {
        "Birth time is unavailable; consent uses directory change time (ctime). Creating, removing or renaming entries directly in the project root, or changing its metadata, clears consent on the next launch. This fails closed; explicitly accept again to restore consent."
    })
}

fn identity_time(meta: &fs::Metadata) -> String {
    match meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        Some(t) => format!("birth:{}:{}", t.as_secs(), t.subsec_nanos()),
        // Some filesystems have no birth time. ctime is conservative: a root
        // metadata change invalidates consent too, rather than risking reuse.
        None => format!("change:{}:{}", meta.ctime(), meta.ctime_nsec()),
    }
}

// No subprocesses, includes, interpolation, package hooks or symlink traversal.
// Large/unreadable files (or malformed package JSON) are ignored rather than
// blocking project.add. Cargo detection needs only the manifest's presence.
fn manifest(root: &Path, name: &str) -> Option<String> {
    let path = root.join(name);
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > 1_048_576 {
        return None;
    }
    let mut text = String::new();
    file.take(1_048_577).read_to_string(&mut text).ok()?;
    (text.len() <= 1_048_576).then_some(text)
}

pub fn detect(root: &Path) -> Vec<String> {
    let mut rules = Vec::new();
    if manifest(root, "Cargo.toml").is_some() {
        rules.extend(CARGO.iter().map(|r| (*r).to_owned()));
    }
    if let Some(package) =
        manifest(root, "package.json").and_then(|text| serde_json::from_str::<Value>(&text).ok())
    {
        for (script, rule) in [("build", OPTIONAL[0]), ("test", OPTIONAL[1])] {
            if package["scripts"][script]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty())
            {
                rules.push(rule.to_owned());
            }
        }
    }
    if let Some(make) = manifest(root, "Makefile") {
        for (target, rule) in [("build", OPTIONAL[2]), ("test", OPTIONAL[3])] {
            if make.lines().any(|line| {
                // Only literal explicit targets. Never evaluate make syntax.
                !line.starts_with(char::is_whitespace)
                    && line.split_once(':').is_some_and(|(names, rest)| {
                        !names.contains(['$', '%', '=', '#'])
                            && !rest.starts_with('=')
                            && names.split_whitespace().any(|name| name == target)
                    })
            }) {
                rules.push(rule.to_owned());
            }
        }
    }
    rules
}

/// Called only by project.add after publishing its clone, not an agent API.
pub(crate) fn propose(store: &mut Store, ship: &str, name: &str, root: &Path) -> Result<Value> {
    let root = root.canonicalize()?;
    let meta = root.metadata()?;
    let rules = detect(&root);
    let project = format!("{ship}/{name}");
    let home = store.home().canonicalize()?;
    let ask = (!rules.is_empty() && matches!(store.caller(), Caller::Agent { .. }))
        .then(|| format!("project-consent-{}", uuid::Uuid::new_v4()));
    let conn = crate::fleet::registry()?;
    conn.execute(
        "INSERT INTO project_permissions(project,root,device,inode,proposal,ask,home,identity_time)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(project) DO UPDATE SET
        root=excluded.root,device=excluded.device,inode=excluded.inode,identity_time=excluded.identity_time,proposal=excluded.proposal,
        accepted='[]',state='pending',ask=excluded.ask,home=excluded.home",
        params![
            project,
            root.to_str(),
            meta.dev().to_string(),
            meta.ino().to_string(),
            json!(rules).to_string(),
            ask,
            home.to_str(),
            identity_time(&meta)
        ],
    )?;
    if let Some(id) = &ask {
        let body = format!(
            "Allow build and test tools for {project}?\n{}\nCrews can edit scripts or pass flags to run arbitrary commands as your OS user; accepting trusts them with code execution. Only Claude crews inside this project or its leased worktrees receive these rules; first mates, reviewers and errands never do. Other harnesses keep their own settings. Declining changes nothing.\nTo select fewer rules: privateer permissions project {project} accept 'RULE' ...",
            rules.join("\n")
        );
        let from = crate::fleet::caller_ship(store).context("project consent needs a sender")?;
        crate::mail::dispatch(
            store,
            "mail.send",
            json!({"id":id,"from":from,"to":"captain",
            "kind":"decision","body":body,"payload":{"project_consent":true,"decision":{
                "question":format!("Allow build and test tools for {project}?"),
                "why":"Crews can edit scripts or pass flags to run arbitrary commands as your OS user; accepting trusts them with code execution. Claude crews only, including this project's leased worktrees; never first mates, reviewers or errands.",
                "options":[
                    {"id":"accept","label":"Allow proposed tools","what_happens":"Grant exactly the listed rules for this project, on the next Claude crew launch."},
                    {"id":"decline","label":"Not now","what_happens":"Keep current permissions; grant nothing."}
                ]}}}),
        )?;
    }
    show(&conn, &project)
}

fn select(conn: &rusqlite::Connection, selector: &str) -> Result<String> {
    let mut stmt = conn.prepare("SELECT project FROM project_permissions WHERE project=?1 OR root=?1 OR substr(project,instr(project,'/')+1)=?1")?;
    let rows = stmt
        .query_map([selector], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() == 1,
        "project unknown or ambiguous; use SHIP/NAME or the absolute project path"
    );
    Ok(rows[0].clone())
}

fn show(conn: &rusqlite::Connection, project: &str) -> Result<Value> {
    let row = ProjectGrant::get(conn, project)?.context("project missing")?;
    Ok(
        json!({"project":project,"root":row.root,"proposed":serde_json::from_str::<Value>(&row.proposal)?,
        "accepted":serde_json::from_str::<Value>(&row.accepted)?,"state":row.state,
        "identity_note":identity_note(row.time.as_deref()),
        "accept_command":format!("privateer permissions project {project} accept")}),
    )
}

fn update(
    conn: &rusqlite::Connection,
    project: &str,
    action: &str,
    selected: &[String],
) -> Result<Value> {
    let row = show(conn, project)?;
    let proposed: Vec<String> = serde_json::from_value(row["proposed"].clone())?;
    let rules = match action {
        "accept" if selected.is_empty() => proposed.clone(),
        "accept" => selected.to_vec(),
        "decline" | "clear" => vec![],
        _ => bail!("use accept [RULE ...], show, decline or clear"),
    };
    ensure!(
        rules.iter().all(|r| proposed.contains(r)),
        "accept may only drop proposed rules, never add them"
    );
    let state = if action == "accept" {
        "accepted"
    } else {
        action
    };
    // Invalidate the original card: a stale phone answer cannot restore a
    // cleared or edited grant. Updates are serialized in the registry.
    conn.execute(
        "UPDATE project_permissions SET accepted=?2,state=?3,ask=NULL WHERE project=?1",
        params![project, json!(rules).to_string(), state],
    )?;
    show(conn, project)
}

pub fn cli(args: &[String], caller: &Caller) -> Result<Value> {
    let [selector, action, selected @ ..] = args else {
        bail!(
            "usage: privateer permissions project SHIP/NAME accept [RULE ...]|show|decline|clear"
        );
    };
    if action != "show" {
        caller.require_operator("permissions.project")?;
    }
    ensure!(
        action == "accept" || selected.is_empty(),
        "only accept takes a rule selection"
    );
    let mut conn = crate::fleet::registry()?;
    if action == "show" {
        return show(&conn, &select(&conn, selector)?);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let project = select(&tx, selector)?;
    if action == "accept" {
        // Explicit operator consent upgrades old rows to the current
        // directory identity. Phone replies never refresh that identity.
        let row = show(&tx, &project)?;
        let meta = fs::symlink_metadata(row["root"].as_str().context("project root missing")?)?;
        ensure!(meta.is_dir(), "project root is not a directory");
        tx.execute(
            "UPDATE project_permissions SET device=?2,inode=?3,identity_time=?4 WHERE project=?1",
            params![
                project,
                meta.dev().to_string(),
                meta.ino().to_string(),
                identity_time(&meta)
            ],
        )?;
    }
    let result = update(&tx, &project, action, selected)?;
    tx.commit()?;
    Ok(result)
}

pub(crate) fn is_ask(store: &Store, id: &str) -> bool {
    // The desk may hold only a delivered copy, even on another machine.
    // This flag requests an operator provenance marker, not a grant: applying
    // it still requires the original home's registry row and exact ask id.
    crate::mail::find(store, id)
        .ok()
        .flatten()
        .is_some_and(|ask| ask["kind"] == "decision" && ask["payload"]["project_consent"] == true)
}

/// Trust the runtime's operator marker, never an agent's decision payload.
/// Matching the locally recorded id also prevents forged cards granting rules.
pub(crate) fn answered(store: &Store, id: &str, reply: &Value) -> Result<()> {
    if crate::presence::folder_ship(store).is_none()
        || reply["from"] != "captain"
        || reply["kind"] != "reply"
        || reply["payload"]["operator"] != true
    {
        return Ok(());
    }
    let mut conn = crate::fleet::registry()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let home = store.home().canonicalize()?;
    let project: Option<String> = tx
        .query_row(
            "SELECT project FROM project_permissions WHERE ask=?1 AND home=?2",
            params![id, home.to_str()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(project) = project {
        match reply["payload"]["answer"]["option_id"].as_str() {
            Some("accept") => {
                update(&tx, &project, "accept", &[])?;
            }
            Some("decline") => {
                update(&tx, &project, "decline", &[])?;
            }
            _ => return Ok(()),
        }
    }
    tx.commit()?;
    Ok(())
}

/// Reconcile durable captain replies before reading grants. This also recovers
/// a crash between recording a reply and applying its policy change.
fn recover(store: &Store) -> Result<()> {
    let conn = crate::fleet::registry()?;
    let home = store.home().canonicalize()?;
    let mut stmt =
        conn.prepare("SELECT ask FROM project_permissions WHERE home=?1 AND ask IS NOT NULL")?;
    let ids = stmt
        .query_map([home.to_str()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    drop(conn);
    for id in ids {
        if let Some(ask) = crate::mail::find(store, &id)?
            && let Some(reply) = ask["answered_by"].as_str()
            && let Some(reply) = crate::mail::find(store, reply)?
        {
            answered(store, &id, &reply)?;
        }
    }
    Ok(())
}

pub(crate) fn for_launch(store: &Store, cwd: &Path, harness: &str, role: &str) -> Vec<String> {
    if harness != "claude" || role != "crew" || crate::presence::folder_ship(store).is_none() {
        return vec![];
    }
    match launch_rules(store, cwd) {
        Ok(rules) => rules,
        Err(error) => {
            eprintln!(
                "privateer: project consent unavailable; launching without project rules: {error:#}"
            );
            vec![]
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectGrant {
    project: String,
    root: String,
    device: String,
    inode: String,
    time: Option<String>,
    accepted: String,
    state: String,
    proposal: String,
    ask: Option<String>,
    home: String,
}

impl ProjectGrant {
    const SELECT: &'static str = "SELECT project,root,device,inode,identity_time,accepted,state,proposal,ask,home FROM project_permissions";

    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            project: row.get(0)?,
            root: row.get(1)?,
            device: row.get(2)?,
            inode: row.get(3)?,
            time: row.get(4)?,
            accepted: row.get(5)?,
            state: row.get(6)?,
            proposal: row.get(7)?,
            ask: row.get(8)?,
            home: row.get(9)?,
        })
    }

    fn get(conn: &rusqlite::Connection, project: &str) -> Result<Option<Self>> {
        Ok(conn
            .query_row(
                &format!("{} WHERE project=?1", Self::SELECT),
                [project],
                Self::read,
            )
            .optional()?)
    }

    fn invalidate(&self, conn: &mut rusqlite::Connection, removed: bool) -> Result<()> {
        if !removed && self.accepted == "[]" && self.state == "clear" && self.ask.is_none() {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Metadata was checked without a transaction. Never overwrite a newer
        // accept, proposal or clear (or delete its replacement row).
        if Self::get(&tx, &self.project)?.as_ref() == Some(self) {
            if removed {
                tx.execute(
                    "DELETE FROM project_permissions WHERE project=?1",
                    [&self.project],
                )?;
            } else {
                tx.execute("UPDATE project_permissions SET accepted='[]',state='clear',ask=NULL WHERE project=?1", [&self.project])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn launch_rules(store: &Store, cwd: &Path) -> Result<Vec<String>> {
    recover(store)?;
    let cwd = cwd.canonicalize()?;
    let mut conn = crate::fleet::registry()?;
    registry_rules(&mut conn, &cwd, |root| fs::symlink_metadata(root))
}

fn registry_rules(
    conn: &mut rusqlite::Connection,
    cwd: &Path,
    mut metadata: impl FnMut(&Path) -> std::io::Result<fs::Metadata>,
) -> Result<Vec<String>> {
    let rows = conn
        .prepare(ProjectGrant::SELECT)?
        .query_map([], ProjectGrant::read)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The statement and its implicit read transaction are gone before any
    // filesystem operation, which may block indefinitely on a remote mount.
    let mut rules = Vec::new();
    for row in rows {
        let root = Path::new(&row.root);
        let meta = match metadata(root) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Observing removal retires consent and its card, unless a
                // concurrent operator action has since replaced this snapshot.
                row.invalidate(conn, true)?;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if !meta.is_dir()
            || meta.dev().to_string() != row.device
            || meta.ino().to_string() != row.inode
            || row.time.as_deref() != Some(identity_time(&meta).as_str())
        {
            row.invalidate(conn, false)?;
            continue;
        }
        if row.state != "accepted" || !cwd.starts_with(root) {
            continue;
        }
        let accepted: Vec<String> = serde_json::from_str(&row.accepted)?;
        for rule in accepted {
            ensure!(
                CARGO.contains(&rule.as_str()) || OPTIONAL.contains(&rule.as_str()),
                "invalid project rule"
            );
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    Ok(rules)
}
