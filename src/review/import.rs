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

/// Bind explicit PR wording in the saved evidence, not bare #N references
/// that may name an issue (for example, "PR 774 (#772)").
fn exact_pr(brief: &str, reply: &str, number: u64) -> Result<()> {
    let check = |named: &str| -> Result<()> {
        ensure!(
            named.parse::<u64>().ok() == Some(number),
            "the errand names PR #{named}, not requested PR #{number}"
        );
        Ok(())
    };
    for text in [brief, reply] {
        let words: Vec<_> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        for (i, word) in words.iter().enumerate() {
            let named = if word.eq_ignore_ascii_case("pr") {
                words.get(i + 1)
            } else if word.eq_ignore_ascii_case("pull")
                && words
                    .get(i + 1)
                    .is_some_and(|w| w.eq_ignore_ascii_case("request"))
            {
                words.get(i + 2)
            } else {
                None
            };
            if let Some(named) = named.filter(|w| w.bytes().all(|b| b.is_ascii_digit())) {
                check(named)?;
            }
        }
    }
    Ok(())
}

/// Only unambiguous repository forms bind: forge URLs, explicit repo or
/// repository wording, and owner/name#N. Bare two-segment paths are not evidence.
fn exact_repo(brief: &str, reply: &str, repo: &str) -> Result<()> {
    let check = |named: &str| -> Result<()> {
        ensure!(
            named.eq_ignore_ascii_case(repo),
            "the errand names repository {named}, not selected repository {repo}"
        );
        Ok(())
    };
    let is_repo = |token: &str| {
        let parts: Vec<_> = token.split('/').collect();
        parts.len() == 2
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
            })
    };
    for text in [brief, reply] {
        let tokens: Vec<_> = text
            .split(|c: char| !c.is_ascii_alphanumeric() && !"-._/#:".contains(c))
            .filter(|s| !s.is_empty())
            .collect();
        for (i, token) in tokens.iter().enumerate() {
            let token = token.trim_end_matches('.');
            if let Some((scheme, rest)) = token.split_once("://")
                && (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
            {
                let parts: Vec<_> = rest.split('/').collect();
                if parts.len() >= 3 && !parts[0].is_empty() {
                    let named = format!("{}/{}", parts[1], parts[2]);
                    if is_repo(&named) {
                        check(&named)?;
                    }
                }
            } else if let Some((named, number)) = token.split_once('#') {
                if is_repo(named)
                    && !number.is_empty()
                    && number.bytes().all(|b| b.is_ascii_digit())
                {
                    check(named)?;
                }
            } else if is_repo(token)
                && i > 0
                && ["repo", "repository"].iter().any(|word| {
                    tokens[i - 1]
                        .trim_end_matches(':')
                        .eq_ignore_ascii_case(word)
                })
            {
                check(token)?;
            }
        }
    }
    Ok(())
}

fn saved_errand(conn: &rusqlite::Connection, errand: &str, ship: &str) -> Result<Value> {
    let mut saved: Value = conn.query_row(
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
    // Only the runtime's durable verdict notice confirms settlement.
    saved["confirmed"] = json!(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM mail WHERE id=?1 AND sender=?2 AND recipient=?3
           AND kind='notice' AND json_extract(payload,'$.errand')=?4
           AND json_extract(payload,'$.status')='succeeded')",
        params![format!("{errand}-verdict"), crate::mail::CREW, ship, errand],
        |r| r.get::<_, bool>(0),
    )?);
    Ok(saved)
}

fn existing_record(
    conn: &rusqlite::Connection,
    errand: &str,
    number: u64,
    head: &str,
    repo: Option<&str>,
) -> Result<Option<Value>> {
    let existing = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM reviews WHERE errand_id=?1"),
            [errand],
            row,
        )
        .optional()?;
    if let Some(existing) = &existing {
        ensure!(
            existing["pr"].as_u64() == Some(number) && existing["head_sha"] == head,
            "errand {errand} already has a review for PR #{} at head {}",
            existing["pr"],
            existing["head_sha"].as_str().unwrap_or_default()
        );
        if let Some(repo) = repo {
            ensure!(
                existing["repo"]
                    .as_str()
                    .is_some_and(|r| r.eq_ignore_ascii_case(repo)),
                "errand {errand} already has a review in repository {} for PR #{} at head {}",
                existing["repo"],
                existing["pr"],
                existing["head_sha"]
            );
        }
    }
    Ok(existing)
}

pub(super) fn record(store: &Store, args: &Value) -> Result<Value> {
    let ship = first_mate(store, "review.record")?;
    let errand = required(args, "errand_id")?;
    let number = pr_number(args)?;
    let head = required(args, "head_sha")?;
    let requested_repo = args
        .get("repo")
        .filter(|r| !r.is_null())
        .map(|r| r.as_str().context("repo must be owner/name"))
        .transpose()?;
    ensure!(
        head.len() == 40
            && head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "head_sha must be the full 40-character sha in lowercase hex"
    );
    // Read without a write lock: git subprocesses and credential filtering
    // below can be slow. Recheck the evidence under the lock before insert.
    let saved = saved_errand(store.connection(), errand, &ship)?;
    if let Some(existing) =
        existing_record(store.connection(), errand, number, head, requested_repo)?
    {
        return Ok(existing);
    }
    ensure!(
        saved["mode"] == "fresh",
        "only fresh errands may be imported, not resumed sessions"
    );
    ensure!(
        saved["status"] == "succeeded" && saved["finished_ms"].is_i64(),
        "errand {errand} has not finished successfully"
    );
    ensure!(
        saved["confirmed"] == true,
        "errand {errand} has no confirmed verdict mail"
    );
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
    let brief = saved["brief"].as_str().unwrap_or_default();
    exact_head(brief, reply, head)?;
    exact_pr(brief, reply, number)?;
    let task = saved["task_id"].as_str().context("errand task")?;
    // Locating the clone reads only local git configuration; unlike
    // record_errand, this deliberately never reads a live PR or fetches it.
    let (_, _, clone) = task_clone(
        store,
        &ship,
        &json!({"task_id": task, "repo": requested_repo}),
        None,
    )?;
    exact_repo(brief, reply, &clone.repo)?;
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
    let tx = store.write_transaction()?;
    let current = saved_errand(&tx, errand, &ship)?;
    if let Some(existing) = existing_record(&tx, errand, number, head, requested_repo)? {
        return Ok(existing);
    }
    ensure!(
        current == saved,
        "errand evidence changed during import; retry"
    );
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
    fn repository_evidence_and_finding_paths() {
        assert!(exact_repo("Review acme/widget", "", "acme/widget").is_ok());
        // Bare paths, including ones that could also be repo names, never bind.
        assert!(
            exact_repo(
                "Review acme/other src/review.rs docs/x.md and/or",
                "See tests/review_gate.rs",
                "acme/widget"
            )
            .is_ok()
        );
        for text in ["repo acme/other", "RePo: acme/other", "acme/other#42"] {
            assert!(exact_repo(text, "", "acme/widget").is_err());
            assert!(exact_repo("", text, "acme/other").is_ok());
        }
        assert!(exact_repo("", "Repository: acme/other.", "acme/widget").is_err());
        assert!(exact_repo("https://github.com/acme/other/pull/42", "", "acme/widget").is_err());
        assert!(exact_repo("", "- FINDING nit src/lib.rs:1 spelling", "acme/widget").is_ok());
    }

    #[test]
    fn pr_references_must_all_agree() {
        for text in [
            "PR 42",
            "PR #42",
            "pr: 42",
            "pull request 42",
            "Pull Request #42",
        ] {
            assert!(exact_pr(text, "", 42).is_ok());
            assert!(exact_pr("", text, 41).is_err());
        }
        assert!(exact_pr("Review #42", "Also #41", 42).is_ok());
        assert!(exact_pr("PR 774 (#772)", "See issue #771", 774).is_ok());
        assert!(exact_pr("PR 42", "PR: 41", 42).is_err());
        assert!(exact_pr("Review it", "", 42).is_ok());
    }

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
