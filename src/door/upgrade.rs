//! Durable scoped upgrade confirmations, deliberately separate from the v1
//! session slot. A consumed plan is never restored after a failed handoff.
use super::scoped::{DelegatedContext, Denial};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const LIFETIME_MS: i64 = 300_000;

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS door_upgrade_plans(
            id TEXT PRIMARY KEY, client TEXT NOT NULL, resource TEXT NOT NULL,
            plan TEXT NOT NULL, created_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL,
            consumed_ms INTEGER);",
    )?;
    Ok(())
}

fn save(conn: &mut Connection, context: &DelegatedContext, plan: &Value) -> Result<String> {
    init(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let now = crate::store::Store::now_ms();
    tx.execute("DELETE FROM door_upgrade_plans WHERE expires_ms<=?1", [now])?;
    // One outstanding plan per grant/resource, without touching anyone else's
    // pending confirmation. Also bound fleet-wide storage (fail, don't evict).
    tx.execute(
        "DELETE FROM door_upgrade_plans WHERE client=?1 AND resource=?2",
        params![context.client(), context.gateway()["resource"].as_str()],
    )?;
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM door_upgrade_plans", [], |r| r.get(0))?;
    ensure!(count < 4096, "too many pending upgrade plans");
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO door_upgrade_plans(id,client,resource,plan,created_ms,expires_ms)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            id,
            context.client(),
            context.gateway()["resource"].as_str(),
            plan.to_string(),
            now,
            now + LIFETIME_MS
        ],
    )?;
    tx.commit()?;
    Ok(id)
}

fn consume(conn: &mut Connection, context: &DelegatedContext, id: &str) -> Result<Value> {
    init(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let now = crate::store::Store::now_ms();
    let plan: Option<String> = tx
        .query_row(
            "SELECT plan FROM door_upgrade_plans WHERE id=?1 AND client=?2 AND resource=?3
         AND consumed_ms IS NULL AND expires_ms>?4",
            params![
                id,
                context.client(),
                context.gateway()["resource"].as_str(),
                now
            ],
            |r| r.get(0),
        )
        .optional()?;
    let plan = plan.ok_or(Denial::UpgradeUnavailable)?;
    tx.execute(
        "UPDATE door_upgrade_plans SET consumed_ms=?2 WHERE id=?1",
        params![id, now],
    )?;
    tx.commit()?;
    // Corrupt storage cannot make a consumed plan reusable.
    Ok(serde_json::from_str(&plan)?)
}

pub(super) fn request(
    store: &crate::store::Store,
    args: &Value,
    context: &DelegatedContext,
) -> Result<Value> {
    if !context.has("fleet:admin") {
        return Err(Denial::Scope.into());
    }
    // Upgrades affect all ships on the target machines, not only the selected
    // mailbox. Refuse a restricted grant that excludes an affected ship.
    for ship in crate::fleet::members(&crate::fleet::registry()?)? {
        if !context.allows(&ship.name) {
            return Err(Denial::Ship.into());
        }
    }
    for (ship, _, _) in crate::machines::remote_ships()? {
        if !context.allows(&ship) {
            return Err(Denial::Ship.into());
        }
    }
    let mut conn = crate::fleet::registry()?;
    if args["action"].as_str().unwrap_or("plan") == "plan" {
        let mut plan = crate::releases::linked::plan(args["commit"].as_str().context("commit")?)?;
        let id = save(&mut conn, context, &plan)?;
        store.with_delegated_audit(context, || {
            store.audit("upgrade.planned", &json!({
            "confirmation":id,"plan":plan,"expires_ms":crate::store::Store::now_ms()+LIFETIME_MS
        }))
        })?;
        plan["confirmation"] = json!(id);
        plan["dry_run"] = json!(true);
        return Ok(plan);
    }
    let plan = consume(
        &mut conn,
        context,
        args["confirmation"].as_str().context("confirmation")?,
    )?;
    // The typed runtime entry re-plans, checks exact equality, and retains all
    // existing upgrade gates and durable custody. No production approval is
    // created or consumed by this captain-scoped operation.
    crate::releases::requests::start_delegated(store, &plan, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(client: &str) -> DelegatedContext {
        DelegatedContext::resolved(
            client.into(),
            "door:test".into(),
            vec!["fleet:admin".into()],
            None,
        )
    }

    #[test]
    fn plans_bind_client_resource_expiry_and_are_consumed_once_across_connections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plans.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        let owner = context("owner");
        let plan =
            json!({"commit":"b".repeat(40),"machines":[{"name":"local"}],"environment":"exact"});
        let id = save(&mut conn, &owner, &plan).unwrap();
        assert!(consume(&mut conn, &context("other"), &id).is_err());
        let other_resource =
            DelegatedContext::resolved("owner".into(), "door:other".into(), vec![], None);
        assert!(consume(&mut conn, &other_resource, &id).is_err());
        conn.execute(
            "UPDATE door_upgrade_plans SET expires_ms=0 WHERE id=?1",
            [&id],
        )
        .unwrap();
        assert!(consume(&mut conn, &owner, &id).is_err());
        let id = save(&mut conn, &owner, &plan).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (path, id, barrier) = (path.clone(), id.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let mut conn = Connection::open(path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    barrier.wait();
                    consume(&mut conn, &context("owner"), &id).is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|t| t.join().unwrap().then_some(()))
                .count(),
            1
        );
        assert!(consume(&mut conn, &owner, &id).is_err());
    }
}
