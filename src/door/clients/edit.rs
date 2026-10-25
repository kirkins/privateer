//! Owner-only in-place authority edits. Never mint or rotate credentials.
use super::*;
use crate::cli::Flags;

fn list(flags: &Flags, key: &str) -> Result<Option<Vec<String>>> {
    flags
        .values
        .get(key)
        .map(|s| {
            // Empty CSV deliberately means no scopes / no named ships.
            if s.is_empty() {
                Ok(vec![])
            } else {
                csv(Some(s))
            }
        })
        .transpose()
}

fn change(
    old: &[String],
    set: Option<Vec<String>>,
    add: Option<Vec<String>>,
    remove: Option<Vec<String>>,
) -> Result<Vec<String>> {
    ensure!(
        set.is_none() || (add.is_none() && remove.is_none()),
        "cannot combine set with add/remove"
    );
    let mut result = set.unwrap_or_else(|| old.to_vec());
    if let Some(add) = add {
        for value in add {
            ensure!(
                !remove.as_ref().is_some_and(|r| r.contains(&value)),
                "cannot add and remove the same value"
            );
            if !result.contains(&value) {
                result.push(value);
            }
        }
    }
    if let Some(remove) = remove {
        result.retain(|v| !remove.contains(v));
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn validate_ships(conn: &Connection, ships: &[String]) -> Result<()> {
    for ship in ships {
        identifier(ship)?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ships WHERE name=?1 AND retired_ms IS NULL UNION ALL SELECT 1 FROM machine_ships WHERE name=?1)",
            [ship], |r| r.get(0))?;
        ensure!(
            exists,
            "unknown fleet ship: {ship}; use --all-ships for current and future ships"
        );
    }
    Ok(())
}

fn proposed(conn: &Connection, before: &Value, flags: &Flags) -> Result<Value> {
    ensure!(
        flags.values.keys().all(|k| matches!(
            k.as_str(),
            "scopes" | "add-scopes" | "remove-scopes" | "ships" | "add-ships" | "remove-ships"
        )),
        USAGE
    );
    ensure!(
        flags
            .switches
            .iter()
            .all(|s| matches!(s.as_str(), "all-ships" | "named-ships")),
        USAGE
    );
    ensure!(
        !flags.values.is_empty() || !flags.switches.is_empty(),
        "edit needs an authority change"
    );
    let all = flags.switches.iter().any(|s| s == "all-ships");
    let named = flags.switches.iter().any(|s| s == "named-ships");
    let ships = list(flags, "ships")?;
    let add_ships = list(flags, "add-ships")?;
    let remove_ships = list(flags, "remove-ships")?;
    ensure!(
        !all || (!named && ships.is_none() && add_ships.is_none() && remove_ships.is_none()),
        "--all-ships conflicts with named ship edits"
    );
    ensure!(!named || ships.is_some(), "--named-ships requires --ships");
    ensure!(
        !before["ships"].is_null()
            || ships.is_some()
            || (add_ships.is_none() && remove_ships.is_none()),
        "all-ships has no explicit list; use --named-ships --ships CSV"
    );
    for values in [&ships, &add_ships, &remove_ships].into_iter().flatten() {
        validate_ships(conn, values)?;
    }
    let scopes = list(flags, "scopes")?;
    let add_scopes = list(flags, "add-scopes")?;
    let remove_scopes = list(flags, "remove-scopes")?;
    for values in [&scopes, &add_scopes, &remove_scopes].into_iter().flatten() {
        ensure!(
            values.iter().all(|s| SCOPES.contains(&s.as_str())),
            "unknown client scope"
        );
    }
    let old_scopes: Vec<String> = serde_json::from_value(before["scopes"].clone())?;
    let new_scopes = change(&old_scopes, scopes, add_scopes, remove_scopes)?;
    let old_ships: Option<Vec<String>> = serde_json::from_value(before["ships"].clone())?;
    let new_ships = if all {
        None
    } else if ships.is_some() || add_ships.is_some() || remove_ships.is_some() {
        Some(change(
            old_ships.as_deref().unwrap_or_default(),
            ships,
            add_ships,
            remove_ships,
        )?)
    } else {
        old_ships
    };
    // Scope-only edits can preserve a retired named ship; explicit ship edits
    // must result in a currently known fleet selection.
    if flags.values.keys().any(|k| k.ends_with("ships")) {
        if let Some(ships) = &new_ships {
            validate_ships(conn, ships)?;
        }
    }
    Ok(json!({"scopes":new_scopes,"ships":new_ships}))
}

fn snapshot(conn: &Connection, id: &str) -> Result<Value> {
    let mut grant = show(conn, Some(id), None)?["clients"][0].clone();
    ensure!(grant["revoked_ms"].is_null(), "client is revoked");
    // Requests can continue while the owner reads the prompt. Last-use is not
    // authority and must not cause a spurious concurrent-edit refusal.
    grant
        .as_object_mut()
        .context("invalid grant")?
        .remove("last_use_ms");
    let protocol: Option<u32> = conn
        .query_row(
            "SELECT protocol FROM doors WHERE name=?1",
            [grant["door"].as_str().context("invalid door")?],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        protocol == Some(2),
        "client door was removed or is not scoped v2"
    );
    Ok(grant)
}

pub(super) fn run(conn: &mut Connection, id: &str, flags: &Flags) -> Result<()> {
    let before = snapshot(conn, id)?;
    let after = proposed(conn, &before, flags)?;
    let consent = json!({"before":before,"after":after,
        "notice":"Paused and expired grants may be edited; editing does not resume, extend expiry, or restore revoked grants. Applies on the next request; already admitted effects are not recalled."});
    let _tty = terminal(&consent, "edit")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        snapshot(&tx, id)? == before,
        "grant changed during consent; inspect and retry edit"
    );
    ensure!(
        proposed(&tx, &before, flags)? == after,
        "fleet changed during consent; retry edit"
    );
    let scopes = serde_json::to_string(&after["scopes"])?;
    let ships = if after["ships"].is_null() {
        None
    } else {
        Some(serde_json::to_string(&after["ships"])?)
    };
    tx.execute(
        "UPDATE door_clients SET scopes=?2,ships=?3,updated_ms=?4 WHERE id=?1",
        params![id, scopes, ships, Store::now_ms()],
    )?;
    // Preserve retry records: their authority hash must now refuse stale keys,
    // rather than forgetting protection and executing the same key anew.
    let detail = json!({"owner_consent":true,
        "before":{"scopes":before["scopes"],"ships":before["ships"]},"after":after});
    audit(
        &tx,
        before["door"].as_str().context("invalid door")?,
        Some(id),
        "edit",
        &detail.to_string(),
    )?;
    tx.commit()?;
    println!("{}", show(conn, Some(id), None)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_set_add_remove_and_reject_conflicts() {
        let old = vec!["a".into(), "b".into()];
        assert_eq!(
            change(&old, Some(vec![]), None, None).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            change(
                &old,
                None,
                Some(vec!["c".into(), "a".into()]),
                Some(vec!["b".into()])
            )
            .unwrap(),
            vec!["a", "c"]
        );
        assert!(change(&old, Some(vec![]), Some(vec![]), None).is_err());
        assert!(change(&old, None, Some(vec!["a".into()]), Some(vec!["a".into()])).is_err());
    }
}
