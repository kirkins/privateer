//! Cache identity and transport custody for linked observations.
use super::*;

/// Include the immutable route as well as the durable generation. This also
/// fails closed if a pinned executable/core or route is edited directly.
pub(crate) fn link_incarnation(conn: &Connection, name: &str) -> Result<String> {
    let (generation, role, peer, known_as): (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT incarnation, role, peer, known_as FROM machines WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let peer: Value = serde_json::from_str(&peer)?;
    Ok(crate::core::hash(&json!([
        generation, role, peer, known_as
    ])))
}

pub(crate) fn same_incarnation(conn: &Connection, name: &str, expected: &str) -> bool {
    link_incarnation(conn, name).is_ok_and(|current| current == expected)
}

pub(super) fn call(name: &str, expected: &str, command: &str, args: Value) -> Result<Value> {
    let conn = crate::fleet::registry()?;
    // Read the token and actual transport configuration from ONE SQLite view.
    // Do not retain a database lock while waiting on a subprocess.
    let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Deferred)?;
    ensure!(
        same_incarnation(&tx, name, expected),
        "machine {name}: link changed"
    );
    known_as(&tx, name)?;
    let mut machine = machine(&tx, name)?;
    ensure!(machine.role == "collects", "{name} reaches this machine");
    tx.commit()?;
    // As call_reached_bound does: a link's shorter custom timeout must not
    // cut a snapshot-sized call short.
    let limit = machine.peer["timeout_ms"]
        .as_u64()
        .unwrap_or(LINK_TIMEOUT_MS);
    machine.peer["timeout_ms"] = json!(limit.max(SNAPSHOT_TIMEOUT.as_millis() as u64));
    let answer = super::call(&machine, command, args, Some(SNAPSHOT_TIMEOUT));
    ensure!(
        same_incarnation(&conn, name, expected),
        "machine {name}: link changed"
    );
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keeps_an_unchanged_link_but_replacement_and_remove_readd_rotate() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let mut link = Machine {
            name: "beta".into(),
            role: "collects".into(),
            peer: json!({"transport":"local", "executable":"alpha"}),
            known_as: Some("alpha".into()),
            refreshed_ms: None,
            next_contact_ms: None,
            failures: 0,
        };
        record(&conn, &link, &[]).unwrap();
        let first = link_incarnation(&conn, "beta").unwrap();
        record(&conn, &link, &[]).unwrap();
        assert_eq!(link_incarnation(&conn, "beta").unwrap(), first);
        record_link(&conn, &link, &[], true).unwrap();
        assert_ne!(
            link_incarnation(&conn, "beta").unwrap(),
            first,
            "an explicit re-link cannot trust the peer still at the same endpoint"
        );
        link.peer["executable"] = json!("gamma");
        record(&conn, &link, &[]).unwrap();
        let second = link_incarnation(&conn, "beta").unwrap();
        assert_ne!(second, first);
        link.peer["executable"] = json!("alpha");
        record(&conn, &link, &[]).unwrap();
        let third = link_incarnation(&conn, "beta").unwrap();
        assert_ne!(third, first, "A-B-A must not revive A's cache");
        conn.execute("DELETE FROM machines WHERE name='beta'", [])
            .unwrap();
        record(&conn, &link, &[]).unwrap();
        assert_ne!(link_incarnation(&conn, "beta").unwrap(), third);
        // Direct pin edits are also part of the effective binding.
        conn.execute(
            "UPDATE machines SET peer=json_set(peer,'$.executable','gamma') WHERE name='beta'",
            [],
        )
        .unwrap();
        assert_ne!(link_incarnation(&conn, "beta").unwrap(), third);
    }
}
