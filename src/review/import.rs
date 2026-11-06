//! Local-only import of a completed, ordinary errand. No forge adapter,
//! fetch, publication or review launch belongs on this path.
use super::*;

/// Require unambiguous full-sha evidence in the saved brief or reply. Do not
/// accept a caller's sha merely because no other head was recorded. Multiple
/// different commits are ambiguous: the first mate must run an exact-head
/// review rather than guess which commit an old errand approved.
fn exact_head(brief: &str, reply: &str, head: &str) -> Result<()> {
    let mut seen = false;
    for sha in brief
        .split(|c: char| !c.is_ascii_alphanumeric())
        .chain(reply.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        ensure!(
            sha == head,
            "the errand's recorded head {sha} does not exactly match head_sha {head}"
        );
        seen = true;
    }
    ensure!(
        seen,
        "the errand's brief or verdict must name one full head sha"
    );
    Ok(())
}

pub(super) fn record(store: &Store, args: &Value) -> Result<Value> {
    let ship = first_mate(store, "review.record")?;
    let errand = required(args, "errand_id")?;
    let number = pr_number(args)?;
    let head = required(args, "head_sha")?;
    ensure!(
        head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "head_sha must be the full 40-character sha"
    );
    // The lock serializes retries and concurrent importers. In particular,
    // checking the owner precedes returning an existing record.
    let tx = store.write_transaction()?;
    let saved: Value = tx.query_row(
        "SELECT ship, task_id, brief, status, finished_ms, verdict, mode,
           route, model_route, route_why, route_info FROM errands WHERE id=?1",
        [errand],
        |r| Ok(json!({
            "ship": r.get::<_, String>(0)?, "task_id": r.get::<_, String>(1)?,
            "brief": r.get::<_, String>(2)?, "status": r.get::<_, String>(3)?,
            "finished_ms": r.get::<_, Option<i64>>(4)?, "verdict": r.get::<_, Option<String>>(5)?,
            "mode": r.get::<_, String>(6)?, "route": r.get::<_, Option<String>>(7)?,
            "model_route": r.get::<_, Option<String>>(8)?, "route_why": r.get::<_, Option<String>>(9)?,
            "route_info": r.get::<_, Option<String>>(10)?,
        })),
    ).optional()?.context("no such errand")?;
    ensure!(saved["ship"] == ship, "errand {errand} is not {ship}'s");
    let existing = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM reviews WHERE errand_id=?1"),
            [errand],
            row,
        )
        .optional()?;
    if let Some(existing) = existing {
        return Ok(existing);
    }
    ensure!(
        saved["status"] == "succeeded" && saved["finished_ms"].is_i64(),
        "errand {errand} has not finished successfully"
    );
    // Only the runtime's durable verdict notice confirms settlement. A
    // user-written mail or an unconfirmed reply from a failed run cannot do so.
    let confirmed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1 AND sender=?2 AND recipient=?3
           AND kind='notice' AND json_extract(payload,'$.errand')=?4
           AND json_extract(payload,'$.status')='succeeded')",
        params![format!("{errand}-verdict"), crate::mail::CREW, ship, errand],
        |r| r.get(0),
    )?;
    ensure!(confirmed, "errand {errand} has no confirmed verdict mail");
    let reply = saved["verdict"].as_str().context("errand has no verdict")?;
    ensure!(
        reply
            .lines()
            .filter(|l| l.trim().starts_with("VERDICT"))
            .count()
            == 1,
        "the errand must have a single VERDICT line"
    );
    let verdict = filter::verdict(reply);
    ensure!(verdict != "none", "the errand has no final VERDICT line");
    exact_head(saved["brief"].as_str().unwrap_or_default(), reply, head)?;
    let task = saved["task_id"].as_str().context("errand task")?;
    // Locating the clone reads only local git configuration; unlike
    // record_errand, this deliberately never reads a live PR or fetches it.
    let (_, _, clone) = task_clone(store, &ship, &json!({"task_id": task}), None)?;
    let findings =
        filter::findings_lenient(reply, &clone.dir, &crate::credentials::stored_values()?)
            .map_err(|why| anyhow::anyhow!("refused by the filter: {why}"))?;
    ensure!(
        verdict != "approve" || !findings.iter().any(|f| f.severity == "must-fix"),
        "the errand approved with a must-fix finding"
    );
    // New errands snapshot route strength at launch. Older errands retain
    // their recorded name and identity, with unknown strength, never the
    // caller's claims or a reconstruction from today's route configuration.
    let route: Value = saved["route_info"]
        .as_str()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(Value::Null);
    let identity = saved["model_route"].as_str().unwrap_or_default();
    let model = crate::routes::model_key(identity).unwrap_or_default();
    let id = format!("review-{}", uuid::Uuid::new_v4());
    let now = Store::now_ms();
    tx.execute(
        "INSERT INTO reviews(id, ship, task_id, forge, repo, pr, base_ref, base_sha, head_ref,
           head_sha, head_tree, errand_id, route_name, route_identity, route_model, route_tier,
           route_uses, route_why, fresh, authors, declared, folder, clone, verdict, findings,
           post_state, created_ms, finished_ms, kind, imported_ms)
         VALUES(?1,?2,?3,?4,?5,?6,'','','',?7,'',?8,?9,?10,?11,?12,?13,?14,?15,
           '{}','[]','',?16,?17,?18,'none',?19,?20,'imported',?19)",
        params![
            id,
            ship,
            task,
            clone.forge,
            clone.repo,
            number,
            head,
            errand,
            saved["route"].as_str().unwrap_or_default(),
            identity,
            model,
            route["tier"].as_i64(),
            route.get("uses").unwrap_or(&json!([])).to_string(),
            saved["route_why"].as_str().unwrap_or_default(),
            saved["mode"] == "fresh",
            clone.dir.to_string_lossy(),
            verdict,
            serde_json::to_string(&findings)?,
            now,
            saved["finished_ms"].as_i64()
        ],
    )?;
    tx.commit()?;
    super::record(store, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_unambiguous_exact_head_evidence_only() {
        let head = "a".repeat(40);
        assert!(exact_head(&format!("Review head `{head}`."), "", &head).is_ok());
        assert!(exact_head("", &format!("HEAD_SHA: {head}"), &head).is_ok());
        assert!(exact_head("Review aaaaaaa", "", &head).is_err());
        assert!(exact_head(&head, &"b".repeat(40), &head).is_err());
        assert!(exact_head(&head.to_uppercase(), "", &head).is_err());
        assert!(exact_head(&format!("x{head}x"), "", &head).is_err());
    }
}
