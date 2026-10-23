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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_permissions(
        project TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, device TEXT NOT NULL,
        inode TEXT NOT NULL, proposal TEXT NOT NULL, accepted TEXT NOT NULL DEFAULT '[]',
        state TEXT NOT NULL DEFAULT 'pending', ask TEXT UNIQUE, home TEXT NOT NULL);",
    )?;
    Ok(())
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
        "INSERT INTO project_permissions(project,root,device,inode,proposal,ask,home)
        VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(project) DO UPDATE SET
        root=excluded.root,device=excluded.device,inode=excluded.inode,proposal=excluded.proposal,
        accepted='[]',state='pending',ask=excluded.ask,home=excluded.home",
        params![
            project,
            root.to_str(),
            meta.dev().to_string(),
            meta.ino().to_string(),
            json!(rules).to_string(),
            ask,
            home.to_str()
        ],
    )?;
    if let Some(id) = &ask {
        let body = format!(
            "Allow build and test tools for {project}?\n{}\nThese commands run the project's own code as your OS user. Only Claude launches inside this project receive them; reviewers never do. Other harnesses keep their own settings. Declining changes nothing.\nTo select fewer rules: privateer permissions project {project} accept 'RULE' ...",
            rules.join("\n")
        );
        let from = crate::fleet::caller_ship(store).context("project consent needs a sender")?;
        crate::mail::dispatch(
            store,
            "mail.send",
            json!({"id":id,"from":from,"to":"captain",
            "kind":"decision","body":body,"payload":{"project_consent":true,"decision":{
                "question":format!("Allow build and test tools for {project}?"),
                "why":"These commands execute project code as your OS user, so only you can approve them.",
                "options":[
                    {"id":"accept","label":"Allow proposed tools","what_happens":"Grant exactly the listed rules for this project, on the next Claude launch."},
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
    let (root, proposal, accepted, state): (String, String, String, String) = conn.query_row(
        "SELECT root,proposal,accepted,state FROM project_permissions WHERE project=?1",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(
        json!({"project":project,"root":root,"proposed":serde_json::from_str::<Value>(&proposal)?,
        "accepted":serde_json::from_str::<Value>(&accepted)?,"state":state,
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
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let project = select(&tx, selector)?;
    let result = if action == "show" {
        show(&tx, &project)?
    } else {
        update(&tx, &project, action, selected)?
    };
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

pub(crate) fn for_launch(
    store: &Store,
    cwd: &Path,
    harness: &str,
    role: &str,
) -> Result<Vec<String>> {
    if harness != "claude"
        || !matches!(role, "crew" | "first_mate")
        || crate::presence::folder_ship(store).is_none()
    {
        return Ok(vec![]);
    }
    recover(store)?;
    let cwd = cwd.canonicalize()?;
    let conn = crate::fleet::registry()?;
    let mut stmt = conn.prepare(
        "SELECT root,device,inode,accepted FROM project_permissions WHERE state='accepted'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut rules = Vec::new();
    for row in rows {
        let (root, device, inode, accepted) = row?;
        let root = Path::new(&root);
        if !cwd.starts_with(root) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if !meta.is_dir() || meta.dev().to_string() != device || meta.ino().to_string() != inode {
            continue;
        }
        let accepted: Vec<String> = serde_json::from_str(&accepted)?;
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
