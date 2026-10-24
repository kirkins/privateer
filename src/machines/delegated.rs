//! Delegated reverse-link outbox. The wire kind is deliberately outside the
//! ordinary mail KINDS: pre-v2.3d collectors refuse it before writing captain mail.
use super::*;
use crate::door::scoped::{DelegatedContext, Denial};

pub(super) const KIND: &str = "delegated.send.v1";

fn schema(store: &Store) -> Result<()> {
    store.connection().execute_batch(
        "CREATE TABLE IF NOT EXISTS delegated_outbox(
           id TEXT PRIMARY KEY, machine TEXT NOT NULL, envelope TEXT NOT NULL,
           created_ms INTEGER NOT NULL, outcome TEXT, completed_ms INTEGER);",
    )?;
    Ok(())
}

fn send(store: &Store, ship: &str, mut args: Value, context: &DelegatedContext) -> Result<Value> {
    crate::door::scoped::gateway_send(&args, context)?;
    ensure!(args["to"] == ship, "delegated routing mismatch");
    let conn = crate::fleet::registry()?;
    let holder: String = conn.query_row(
        "SELECT machine FROM machine_ships WHERE name=?1 LIMIT 1",
        [ship],
        |r| r.get(0),
    )?;
    let target = machine(&conn, &holder)?;
    if target.role == "collects" {
        return super::delegated_call(ship, "send", args, context);
    }
    ensure!(target.role == "collected", "invalid machine role");
    schema(store)?;
    let id = args["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    identifier(&id)?;
    args["id"] = json!(id);
    // Routing fields let old collectors locate the home, but the unknown kind
    // makes mail.deliver reject this even if it ignores all other new fields.
    let envelope = json!({"id":id,"from":crate::mail::CAPTAIN,"to":ship,
        "kind":KIND,"context":context.gateway(),"args":args});
    ensure!(
        envelope.to_string().len() <= crate::mail::MAX_MESSAGE_BYTES,
        "message exceeds 64 KiB"
    );
    let tx = store.write_transaction()?;
    tx.execute(
        "DELETE FROM delegated_outbox WHERE completed_ms < ?1",
        [Store::now_ms() - 30 * 86_400_000],
    )?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT envelope FROM delegated_outbox WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    let mut admitted_provenance = context.provenance();
    if let Some(existing) = existing {
        let mut existing: Value = serde_json::from_str(&existing)?;
        admitted_provenance = DelegatedContext::from_gateway(&existing["context"])?.provenance();
        let mut retry = envelope.clone();
        existing["context"]["result_id"] = Value::Null;
        retry["context"]["result_id"] = Value::Null;
        ensure!(
            existing == retry,
            "message id already holds a different message"
        );
    } else {
        ensure!(
            crate::mail::find(store, &id)?.is_none(),
            "message id already holds ordinary mail"
        );
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM delegated_outbox", [], |r| r.get(0))?;
        ensure!(count < 1000, "delegated outbox is full");
        tx.execute(
            "INSERT INTO delegated_outbox(id,machine,envelope,created_ms) VALUES(?1,?2,?3,?4)",
            params![id, holder, envelope.to_string(), Store::now_ms()],
        )?;
        store.audit(
            "mail.delegated.queued",
            &json!({"id":id,"to":ship,"provenance":context.provenance()}),
        )?;
    }
    let outcome: Option<String> = tx.query_row(
        "SELECT outcome FROM delegated_outbox WHERE id=?1",
        [&id],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(
        json!({"ship":ship,"queued":outcome.is_none(),"delivered":outcome.as_deref()==Some("delivered"),
        "outcome":outcome,"mail":{"id":id,"payload":{"provenance":admitted_provenance}},"provenance":context.provenance(),"returned":null}),
    )
}

pub(super) fn awaiting(
    store: &Store,
    machine: &str,
    limit: usize,
    room: usize,
) -> Result<Vec<Value>> {
    schema(store)?;
    let mut stmt = store.connection().prepare(
        "SELECT envelope FROM delegated_outbox WHERE machine=?1 AND outcome IS NULL ORDER BY created_ms,id LIMIT ?2")?;
    let rows = stmt.query_map(params![machine, limit as i64], |r| r.get::<_, String>(0))?;
    let mut found = vec![];
    let mut bytes = 0;
    for row in rows {
        let row = row?;
        if bytes + row.len() > room {
            break;
        }
        bytes += row.len();
        found.push(serde_json::from_str(&row)?);
    }
    Ok(found)
}

pub(super) fn receive(store: &Store, envelope: &Value) -> Result<Value> {
    ensure!(
        envelope.as_object().is_some_and(|o| o.len() == 6)
            && envelope["kind"] == KIND
            && envelope["from"] == crate::mail::CAPTAIN,
        "invalid delegated collection envelope"
    );
    let context = DelegatedContext::from_gateway(&envelope["context"])?;
    let args = &envelope["args"];
    ensure!(
        args["id"] == envelope["id"] && args["to"] == envelope["to"],
        "delegated routing mismatch"
    );
    crate::door::scoped::gateway_send(args, &context)?;
    let mut message = args.clone();
    message["from"] = json!(crate::mail::CAPTAIN);
    message["kind"] = json!(args["kind"].as_str().unwrap_or("request"));
    crate::mail::send_delegated(store, &message, &context)
}

pub(super) fn collected(
    store: &Store,
    machine: &str,
    delivered: &[(String, Option<String>)],
    refused: &[(String, String)],
) -> Result<usize> {
    schema(store)?;
    let mut recorded = 0;
    for (id, error) in delivered
        .iter()
        .map(|(id, _)| (id, None))
        .chain(refused.iter().map(|(id, e)| (id, Some(e))))
    {
        if error.is_some_and(|e| crate::waits::temporary(e)) {
            continue;
        }
        let raw: Option<String> = store.connection().query_row(
            "SELECT envelope FROM delegated_outbox WHERE id=?1 AND machine=?2 AND outcome IS NULL",
            params![id,machine], |r| r.get(0)).optional()?;
        let Some(raw) = raw else {
            continue;
        };
        let envelope: Value = serde_json::from_str(&raw)?;
        let outcome = error
            .map(|e| e.chars().take(4096).collect::<String>())
            .unwrap_or_else(|| "delivered".to_owned());
        // Deterministic notice id makes crash/replayed acknowledgments harmless.
        // The durable outbox result is not itself an approval or operator reply.
        let ship = crate::presence::folder_ship(store).context("outbox has no local ship")?;
        crate::mail::dispatch(
            &mut Store::open(store.home())?,
            "mail.send",
            json!({
            "id":format!("delegated-result-{}", crate::core::hash(&json!(id))),"from":ship,"to":crate::mail::CAPTAIN,
            "kind":"notice","body":format!("Delegated queued mail {id} to {}: {outcome}", envelope["to"].as_str().unwrap_or_default()),
            "payload":{"delegated_delivery":{"id":id,"outcome":outcome,"provenance":
                DelegatedContext::from_gateway(&envelope["context"])?.provenance()}}}),
        )?;
        store.connection().execute(
            "UPDATE delegated_outbox SET outcome=?3,completed_ms=?4 WHERE id=?1 AND machine=?2 AND outcome IS NULL",
            params![id,machine,outcome,Store::now_ms()])?;
        recorded += 1;
    }
    Ok(recorded)
}

// Keep transport failures typed at the door; queueing itself has no remote
// side effects and must not be confused with confirmed delivery.
pub(crate) fn send_checked(
    store: &Store,
    ship: &str,
    args: Value,
    context: &DelegatedContext,
) -> Result<Value> {
    send(store, ship, args, context).map_err(|e| {
        e.downcast_ref::<Denial>()
            .copied()
            .unwrap_or(Denial::ExecutionFailed)
            .into()
    })
}
