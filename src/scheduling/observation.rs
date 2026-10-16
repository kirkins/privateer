use super::*;

pub(crate) fn snapshot(store: &Store, ship: &str) -> Result<Vec<Value>> {
    if !available(store)? {
        return Ok(vec![]);
    }
    let mut statement=store.connection().prepare("SELECT t.id,s.record FROM tasks t LEFT JOIN task_schedule s ON s.task_id=t.id
        WHERE t.owner=?1 AND t.status NOT IN ('done','cancelled') AND (s.task_id IS NOT NULL OR t.id<>?1||'-duty') ORDER BY t.rank IS NULL,t.rank,t.rowid LIMIT 100")?;
    let rows = statement
        .query_map([ship], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|(id,raw)|match raw {Some(raw)=>{ensure!(raw.len()<=65536,"schedule exceeds byte bound");Ok(view(&serde_json::from_str::<Scheduled>(&raw)?))},None=>Ok(json!({"task_id":id,"state":"unscheduled","reason":"Legacy or draft task; prepare and explicitly enqueue before automatic execution"}))}).collect()
}

pub(crate) fn fault_item(store: &Store, id: &str) -> Result<Value> {
    let work = load(store, id)?.context("fault schedule missing")?;
    ensure!(work.state == "fault", "schedule is not a fault");
    Ok(
        json!({"ship":work.owner,"task_id":work.task_id,"status":"fault","summary":work.reason,
        "created_ms":work.created_ms,"version":crate::core::hash(&json!([work.revision,work.offer_id,work.state,work.reason]))}),
    )
}
