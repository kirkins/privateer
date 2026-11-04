//! Local operator retention decisions. A release is a dated loss boundary,
//! not a repair of source loss or an assertion that missing mail was empty.
use super::*;
use crate::store::{Caller, required};
use std::path::Path;

pub(super) fn ready(c: &Connection) -> Result<bool> {
    Ok(c.query_row("SELECT COUNT(*)=2 FROM sqlite_master WHERE type='trigger' AND name IN ('desk_loss_insert_epoch','desk_loss_update_epoch')", [], |r| r.get(0))?)
}

pub(super) fn init(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS desk_intake_loss_epoch(
           domain TEXT NOT NULL, source TEXT NOT NULL, epoch INTEGER NOT NULL,
           PRIMARY KEY(domain,source));
         CREATE TRIGGER IF NOT EXISTS desk_loss_insert_epoch BEFORE INSERT ON desk_intake_loss BEGIN
           INSERT INTO desk_intake_loss_epoch VALUES(NEW.domain,NEW.source,1)
           ON CONFLICT(domain,source) DO UPDATE SET epoch=epoch+1;
         END;
         CREATE TRIGGER IF NOT EXISTS desk_loss_update_epoch AFTER UPDATE ON desk_intake_loss BEGIN
           INSERT INTO desk_intake_loss_epoch VALUES(NEW.domain,NEW.source,1)
           ON CONFLICT(domain,source) DO UPDATE SET epoch=epoch+1;
         END;
         CREATE TABLE IF NOT EXISTS desk_home_missing(home TEXT PRIMARY KEY, first_seen_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS desk_home_release(
           home TEXT PRIMARY KEY, ship TEXT NOT NULL, actor TEXT NOT NULL,
           released_ms INTEGER NOT NULL, first_seen_missing_ms INTEGER NOT NULL,
           unread INTEGER, open INTEGER, reason TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS desk_home_loss_boundary(
           seq INTEGER PRIMARY KEY AUTOINCREMENT, home TEXT NOT NULL, ship TEXT NOT NULL,
           actor TEXT NOT NULL, released_ms INTEGER NOT NULL, first_seen_missing_ms INTEGER NOT NULL,
           unread INTEGER, open INTEGER, reason TEXT NOT NULL, plan_digest TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(super) fn state(
    c: &Connection,
    path: &Path,
    deadline: Option<std::time::Instant>,
) -> Result<&'static str> {
    let state = home::census_state(path, deadline)?;
    let released: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM desk_home_release WHERE home=?1)",
        [path.to_string_lossy().as_ref()],
        |r| r.get(0),
    )?;
    Ok(if released {
        if state == "missing" {
            "released"
        } else {
            "reappeared"
        }
    } else {
        state
    })
}

fn revision(c: &Connection) -> Result<String> {
    let mut parts = Vec::new();
    for sql in [
        "SELECT json_array(name,home,retired_ms) FROM ships ORDER BY name LIMIT 10001",
        "SELECT json_array(home) FROM desk_home_inventory ORDER BY home LIMIT 10001",
        "SELECT json_array(home,ship,actor,released_ms,first_seen_missing_ms,unread,open,reason) FROM desk_home_release ORDER BY home LIMIT 10001",
        "SELECT json_array(domain,source,epoch) FROM desk_intake_loss_epoch ORDER BY domain,source LIMIT 10001",
        "SELECT json_array(home,first_seen_ms) FROM desk_home_missing ORDER BY home LIMIT 10001",
        // Bind cached counts as well as registry membership to the plan.
        "SELECT json_array(domain,source,proof) FROM desk_source_reconciliation ORDER BY domain,source LIMIT 10001",
        "SELECT json_array(COALESCE(MAX(seq),0)) FROM desk_home_loss_boundary",
    ] {
        let rows = c
            .prepare(sql)?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(rows.len() <= 10000, "release plan inventory backpressure");
        parts.push(rows);
    }
    Ok(crate::core::hash(&json!(parts)))
}

/// Pure read: does not initialize a registry, record a sighting, or drive peers.
pub(super) fn plan(c: &Connection, args: &Value) -> Result<Value> {
    let home = required(args, "home")?;
    let ship = required(args, "ship")?;
    let operation = required(args, "operation")?;
    ensure!(
        matches!(operation, "release" | "readmit"),
        "invalid retention operation"
    );
    ensure!(Path::new(home).is_absolute(), "home must be absolute");
    let registration: Option<(String, bool)> = c
        .query_row(
            "SELECT home,retired_ms IS NOT NULL FROM ships WHERE name=?1",
            [ship],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let retired = registration
        .as_ref()
        .is_some_and(|(path, retired)| path == home && *retired);
    let active: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM ships WHERE home=?1 AND retired_ms IS NULL)",
        [home],
        |r| r.get(0),
    )?;
    let released: Option<String> = c
        .query_row(
            "SELECT ship FROM desk_home_release WHERE home=?1",
            [home],
            |r| r.get(0),
        )
        .optional()?;
    let state = state(c, Path::new(home), None)?;
    let allowed = if operation == "release" {
        retired && !active && released.is_none() && state == "missing"
    } else {
        released.as_deref() == Some(ship)
            && state == "reappeared"
            && home::census_state(Path::new(home), None)? == "retained"
    };
    let body = json!({"home":home,"ship":ship,"operation":operation,"census_state":state,
        "retired":retired,"active":active,"allowed":allowed,"registry_revision":revision(c)?});
    let digest = crate::core::hash(&body);
    Ok(json!({"plan":body,"digest":digest}))
}

fn cached_counts(c: &Connection, home: &str) -> Result<(Option<i64>, Option<i64>)> {
    let key = crate::core::hash(&json!(home));
    // Counts are dated observations, never current emptiness. Preserve null
    // when this home has never been successfully read.
    Ok(c.query_row("SELECT json_extract(h.value,'$.unread'),json_extract(h.value,'$.open') FROM desk_source_reconciliation r,json_each(r.proof,'$.homes') h WHERE json_extract(h.value,'$.home')=?1 ORDER BY (json_extract(h.value,'$.unread') IS NULL AND json_extract(h.value,'$.open') IS NULL),r.domain,r.source LIMIT 1", [key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((None,None)))
}

pub(super) fn apply(
    c: &Connection,
    caller: &Caller,
    operation: &str,
    args: &Value,
) -> Result<Value> {
    caller.require_operator("desk home retention apply")?;
    let reason = required(args, "reason")?;
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 4096,
        "a bounded reason is required"
    );
    let mut request = args.clone();
    request["operation"] = json!(operation);
    let tx = rusqlite::Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    let current = plan(&tx, &request)?;
    ensure!(
        current["digest"].as_str() == Some(required(args, "digest")?),
        "retention plan digest mismatch; inspect a new plan"
    );
    ensure!(
        current["plan"]["allowed"] == true,
        "retention operation refused for this home/ship/census state"
    );
    let home = required(args, "home")?;
    let ship = required(args, "ship")?;
    let now = crate::store::Store::now_ms();
    let (unread, open) = cached_counts(&tx, home)?;
    if operation == "release" {
        let first: i64 = tx.query_row(
            "SELECT COALESCE((SELECT first_seen_ms FROM desk_home_missing WHERE home=?1),?2)",
            params![home, now],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO desk_home_release VALUES(?1,?2,'captain',?3,?4,?5,?6,?7)",
            params![home, ship, now, first, unread, open, reason],
        )?;
        tx.execute("INSERT INTO desk_home_loss_boundary(home,ship,actor,released_ms,first_seen_missing_ms,unread,open,reason,plan_digest) VALUES(?1,?2,'captain',?3,?4,?5,?6,?7,?8)", params![home,ship,now,first,unread,open,reason,required(args,"digest")?])?;
        home::retain_home(&tx, Path::new(home))?;
    } else {
        tx.execute("DELETE FROM desk_home_release WHERE home=?1", [home])?;
        tx.execute("DELETE FROM desk_home_missing WHERE home=?1", [home])?;
    }
    let audit = json!({"plan":current,"actor":"captain","reason":reason,"at_ms":now});
    tx.execute(
        "INSERT INTO desk_intake_audit(peer,stage,body,at_ms) VALUES('operator',?1,?2,?3)",
        params![format!("home.{operation}"), audit.to_string(), now],
    )?;
    // Filesystem changes cannot be locked by SQLite. Recheck immediately
    // before commit; all later proof use also revalidates absence.
    ensure!(
        state(&tx, Path::new(home), None)?
            == if operation == "release" {
                "released"
            } else {
                "retained"
            },
        "home changed during retention apply"
    );
    tx.commit()?;
    Ok(audit)
}

pub(crate) fn dispatch(caller: &Caller, command: &str, args: &Value) -> Result<Value> {
    let planning = command == "machine.desk.home.plan";
    if !planning {
        caller.require_operator(command)?;
    }
    let path = crate::fleet::registry_path()?;
    crate::store::reject_symlinks(&path)?;
    let flags = if planning {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let c = Connection::open_with_flags(path, flags)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    if planning {
        let read = rusqlite::Transaction::new_unchecked(&c, TransactionBehavior::Deferred)?;
        plan(&read, args)
    } else {
        match command {
            "machine.desk.home.release" => apply(&c, caller, "release", args),
            "machine.desk.home.readmit" => apply(&c, caller, "readmit", args),
            _ => anyhow::bail!("unknown home retention command"),
        }
    }
}

#[cfg(test)]
mod tests;
