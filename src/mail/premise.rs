//! Explicit dependencies of asks, not dependencies inferred from their prose.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Premise {
    Task { r#ref: String },
    Pr { repo: String, number: u64 },
    Issue { repo: String, number: u64 },
}

pub(super) fn parse(args: &Value, kind: &str) -> Result<Option<String>> {
    let Some(value) = args.get("premise").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    ensure!(
        matches!(kind, "request" | "decision"),
        "premise applies only to requests and decisions"
    );
    let premise: Premise = serde_json::from_value(value.clone())
        .context("premise must be {kind:task,ref:task-id} or {kind:pr|issue,repo:owner/name,number:positive-integer}")?;
    match &premise {
        Premise::Task { r#ref } => identifier(r#ref).context("invalid premise task ref")?,
        Premise::Pr { repo, number } | Premise::Issue { repo, number } => {
            let parts: Vec<_> = repo.split('/').collect();
            ensure!(
                parts.len() == 2
                    && parts.iter().all(|p| !p.is_empty()
                        && *p != "."
                        && *p != ".."
                        && p.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
                "premise repo must be owner/name"
            );
            ensure!(
                *number > 0 && *number <= i64::MAX as u64,
                "premise number must be a positive integer"
            );
        }
    }
    Ok(Some(serde_json::to_string(&premise)?))
}

/// Premise metadata is optional on receive: future kinds must not block a
/// peer's delivery queue. Locally authored mail still uses strict `parse`.
pub(super) fn received(args: &Value, kind: &str) -> Option<String> {
    parse(args, kind).ok().flatten()
}

/// Rechecked under the send's writer lock. Remote task references are only
/// shape-checked: the owning home, never a receiving home, observes them.
pub(super) fn owned(store: &Store, premise: Option<&str>, sender: &str) -> Result<()> {
    if let Some(premise) = premise {
        if let Premise::Task { r#ref } = serde_json::from_str(premise)? {
            let task = store.task(&r#ref).context("premise task must exist")?;
            ensure!(
                task["owner"] == sender,
                "premise task must belong to the sender"
            );
        }
    }
    Ok(())
}

/// Called inside the task update or send transaction, after linked asks end.
/// The outcome marker and both notices commit with the terminal task fact.
/// It outlives mail acknowledgement, pruning, restart and task reopening.
pub(super) fn resolve_task(store: &Store, task: &str, status: &str) -> Result<()> {
    if !matches!(status, "done" | "cancelled") {
        return Ok(());
    }
    let mut stmt = store.connection().prepare(&format!(
        "SELECT {COLUMNS} FROM mail WHERE state='open' AND kind IN ('request','decision')
         AND premise IS NOT NULL AND premise_outcome IS NULL AND json_extract(premise,'$.kind')='task'
         AND json_extract(premise,'$.ref')=?1 AND COALESCE(delivery,'') <> 'received'"
    ))?;
    let asks = stmt
        .query_map([task], message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for ask in asks {
        let id = ask["id"].as_str().context("ask id")?;
        let body = format!(
            "Ask {id}: premise task {task} is {status}. This ask may be stale; reassess, withdraw or update it. No answer or approval was changed."
        );
        let change = json!({"ask":id,"premise":ask["premise"],"outcome":status});
        record_crew(
            store.connection(),
            &json!({"id":outcome_id(store,"premise-wake")?,
            "to":ask["from"],"kind":"notice","urgency":"normal","correlation":id,
            "body":body,"payload":{"premise_changed":change}}),
        )?;
        // Keep the recipient's card informed through the ordinary outbox too.
        record_runtime(
            store,
            &json!({"id":outcome_id(store,"premise-card")?,
            "from":ask["from"],"to":ask["to"],"kind":"notice","urgency":"low",
            "correlation":id,"body":format!(
                "Ask {id}: premise task {task} is {status}. This ask may be stale; the requester may reassess, withdraw or update it. No answer or approval was changed."
            ),
            "payload":{"premise_changed":change,"attached_to":id}}),
        )?;
        store.connection().execute(
            "UPDATE mail SET premise_outcome=?2 WHERE id=?1",
            params![id, status],
        )?;
    }
    Ok(())
}

pub(super) fn resolve_sent(store: &Store, premise: Option<&str>) -> Result<()> {
    if let Some(premise) = premise {
        if let Premise::Task { r#ref } = serde_json::from_str(premise)? {
            let task = store.task(&r#ref)?;
            resolve_task(store, &r#ref, task["status"].as_str().unwrap_or(""))?;
        }
    }
    Ok(())
}

/// A peer's notice can annotate only its own ask with the same premise.
/// This is display metadata, never an answer, closure or authority.
pub(super) fn delivered(
    store: &Store,
    from: &str,
    to: &str,
    kind: &str,
    correlation: Option<&str>,
    payload: &Value,
) -> Result<bool> {
    let Some(change) = payload.get("premise_changed") else {
        return Ok(false);
    };
    let Some(ask_id) = correlation.filter(|id| Some(*id) == change["ask"].as_str()) else {
        return Ok(false);
    };
    let outcome = change["outcome"].as_str();
    if kind != "notice" || !matches!(outcome, Some("done" | "cancelled")) {
        return Ok(false);
    }
    if let Some(ask) = by_id(store, ask_id)?
        && ask["from"] == from
        && ask["to"] == to
        && matches!(ask["kind"].as_str(), Some("request" | "decision"))
        && ask["premise"] == change["premise"]
        && !ask["premise"].is_null()
    {
        store.connection().execute(
            "UPDATE mail SET premise_outcome=COALESCE(premise_outcome,?2) WHERE id=?1",
            params![ask_id, outcome],
        )?;
        return Ok(true);
    }
    // An older receiver may have discarded the premise, or a newer sender
    // may know an outcome we do not. Keep the notice as ordinary mail.
    Ok(false)
}

pub(crate) fn display(premise: &Value, outcome: Option<&str>) -> String {
    if premise.is_null() {
        return String::new();
    }
    let label = if premise["kind"] == "task" {
        format!("task {}", premise["ref"].as_str().unwrap_or("?"))
    } else {
        format!(
            "{} {}#{} (not observed)",
            premise["kind"].as_str().unwrap_or("?"),
            premise["repo"].as_str().unwrap_or("?"),
            premise["number"]
        )
    };
    match outcome {
        Some(outcome) => format!(" [premise: {label}; {outcome}; ask may be stale]"),
        None => format!(" [premise: {label}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_copies_ignore_unknown_premises_without_losing_mail() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("home")).unwrap();
        for (id, premise) in [
            ("future", json!({"kind":"future","ref":"work"})),
            ("malformed", json!({"kind":"task","ref":12})),
            ("known", json!({"kind":"task","ref":"work"})),
        ] {
            let mail = json!({"id":id,"from":"apps","to":"infra","kind":"request","body":"Keep this fact","premise":premise});
            assert_eq!(
                record_copy(store.connection(), &mail, 1, source::Provenance::Unresolved).unwrap(),
                1
            );
            let stored = by_id(&store, id).unwrap().unwrap();
            assert_eq!(stored["body"], "Keep this fact");
            assert_eq!(
                stored["premise"],
                if id == "known" { premise } else { Value::Null }
            );
        }
    }

    #[test]
    fn strict_typed_premises() {
        for p in [
            json!({"kind":"task","ref":"work"}),
            json!({"kind":"pr","repo":"acme/project","number":7}),
            json!({"kind":"issue","repo":"acme/project","number":8}),
        ] {
            assert!(parse(&json!({"premise":p}), "request").unwrap().is_some());
            assert!(parse(&json!({"premise":p}), "notice").is_err());
        }
        for p in [
            json!({}),
            json!("task"),
            json!({"kind":"task","ref":""}),
            json!({"kind":"task","ref":"work","repo":"acme/project"}),
            json!({"kind":"pr","repo":"acme/project","number":0}),
            json!({"kind":"issue","repo":"https://example.invalid/p","number":1}),
        ] {
            assert!(parse(&json!({"premise":p}), "decision").is_err());
        }
    }
}
