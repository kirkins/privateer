use super::delivery::{native_event, poll, profile};
use super::*;

fn enqueue(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let tx = store.write_transaction()?;
    let task = owner(store, id)?;
    profile(store, task["owner"].as_str().context("task owner missing")?)?;
    ensure!(
        stopped_ancestor(store, id)?.is_none(),
        "a scheduled ancestor is explicitly stopped"
    );
    ensure!(
        !live_jobs(store, id)?,
        "task still has live or uncertain jobs"
    );
    let old = load(store, id)?;
    ensure!(
        old.as_ref()
            .is_none_or(|w| !matches!(w.state.as_str(), "offered" | "claimed" | "executing")),
        "task already has an execution claim"
    );
    let brief = match args.get("brief") {
        Some(value) => value.as_str().context("brief must be text")?.to_owned(),
        None => crate::context::current_note(
            store,
            task["owner"].as_str().unwrap(),
            &format!("assignment:{id}"),
        )?
        .and_then(|n| n["body"].as_str().map(str::to_owned))
        .context("prepare an assignment or supply brief before enqueueing")?,
    };
    ensure!(
        !brief.trim().is_empty() && brief.len() <= BRIEF_BYTES,
        "brief must contain 1..16384 bytes"
    );
    let now = Store::now_ms();
    let work = Scheduled {
        task_id: id.into(),
        owner: task["owner"].as_str().unwrap().into(),
        revision: old.map_or(1, |w| w.revision.saturating_add(1)),
        brief,
        state: "ready".into(),
        wait: None,
        not_before_ms: now,
        offer_id: None,
        holder: None,
        job_id: None,
        profile: None,
        charter_hash: None,
        attempts: 0,
        created_ms: now,
        last_checked_ms: now,
        lease_until_ms: 0,
        reason: None,
    };
    save(store, &work)?;
    store
        .connection()
        .execute("UPDATE tasks SET status='open' WHERE id=?1", [id])?;
    store.audit("task.enqueue", &json!({"id":id,"revision":work.revision}))?;
    tx.commit()?;
    Ok(view(&work))
}

fn defer(store: &Store, args: &Value) -> Result<Value> {
    let id = required(args, "id")?;
    let tx = store.write_transaction()?;
    owner(store, id)?;
    let mut work = load(store, id)?.context("task is not scheduled")?;
    if matches!(work.state.as_str(), "offered" | "claimed") {
        ensure!(
            args["offer_id"].as_str() == work.offer_id.as_deref(),
            "current offer_id is required"
        );
    }
    let wait = args.get("wait").context("wait is required")?;
    let kind = required(wait, "kind")?;
    let reason = required(wait, "reason")?;
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 1000,
        "wait reason must contain 1..1000 bytes"
    );
    match kind {
        "time" | "attention" => {
            let at = wait["until_ms"]
                .as_i64()
                .context("wait.until_ms is required")?;
            ensure!(at > Store::now_ms(), "wait deadline must be in the future");
            if kind == "attention" {
                let reading = crate::attention::summary_for(store, &work.owner)?;
                ensure!(
                    !(reading["fresh"] == true && reading["pressure"] == "normal"),
                    "attention wait refused: current reading is {}",
                    crate::attention::line(&reading).unwrap_or_else(|| "Attention: unknown".into())
                );
            }
        }
        "task" => {
            let dependency = required(wait, "ref")?;
            ensure!(dependency != id, "a task cannot wait on itself");
            store.task(dependency)?;
            let mut seen = std::collections::HashSet::from([id.to_owned()]);
            let mut next = dependency.to_owned();
            for _ in 0..128 {
                ensure!(seen.insert(next.clone()), "cyclic scheduling dependency");
                let Some(w) = load(store, &next)? else {
                    break;
                };
                let Some(wait) = w.wait.filter(|w| w["kind"] == "task") else {
                    break;
                };
                next = required(&wait, "ref")?.to_owned();
                ensure!(seen.len() < 128, "scheduling dependency limit exceeded");
            }
        }
        "request" => {
            let request =
                crate::mail::find(store, required(wait, "ref")?)?.context("unknown request")?;
            ensure!(
                request["from"] == work.owner
                    && request["task_id"] == id
                    && request["state"] == "open",
                "wait must reference this task's own open request"
            );
        }
        "manual" => {}
        _ => bail!("wait.kind must be time, attention, task, request or manual"),
    }
    let hint = time_wait_hint(store, kind, reason)?;
    // Deferring an offer that was taken up is handled work, not a retry.
    if matches!(work.state.as_str(), "offered" | "claimed" | "executing") {
        work.attempts = 0;
    }
    work.state = if kind == "manual" {
        "stopped"
    } else {
        "waiting"
    }
    .into();
    work.reason = Some(reason.into());
    work.wait = Some(wait.clone());
    work.not_before_ms = wait["until_ms"].as_i64().unwrap_or(0);
    work.last_checked_ms = Store::now_ms();
    finish_wake(store, &work, "succeeded", work.last_checked_ms)?;
    save(store, &work)?;
    store
        .connection()
        .execute("UPDATE tasks SET status='blocked' WHERE id=?1", [id])?;
    store.audit("task.defer", &json!({"id":id,"wait":wait,"hint":hint}))?;
    tx.commit()?;
    let mut out = view(&work);
    out["hint"] = json!(hint);
    Ok(out)
}

/// A time wait whose reason names a task, a request or an "after <event>" is
/// a dependency written as a clock. The defer still applies; the hint tells
/// the caller to wait on the event instead, so it resumes when the event
/// happens and not at a guessed time.
fn time_wait_hint(store: &Store, kind: &str, reason: &str) -> Result<Option<String>> {
    if kind != "time" {
        return Ok(None);
    }
    let words: Vec<&str> = reason
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '"' | '\''
                )
            })
        })
        .collect();
    let mut hints = Vec::new();
    for word in &words {
        if !is_id_shape(word) {
            continue;
        }
        // A task is checked first; a request id is a mail id, so it is looked up too.
        if store.task(word).is_ok() {
            hints.push(format!(
                "task {word}: use wait kind task with ref \"{word}\""
            ));
        } else if crate::mail::find(store, word)
            .ok()
            .flatten()
            .is_some_and(|m| m["kind"] == "request")
        {
            hints.push(format!(
                "request {word}: use wait kind request with ref \"{word}\""
            ));
        }
    }
    if words
        .windows(2)
        .any(|pair| pair[0].eq_ignore_ascii_case("after"))
    {
        hints.push(
            "reason says \"after <event>\": use wait kind task (local) or request (mail)".into(),
        );
    }
    Ok((!hints.is_empty()).then(|| {
        format!(
            "a time wait names a dependency, so it will resume at a guessed time: {}",
            hints.join("; ")
        )
    }))
}

/// Task ids and mail ids in this home: lowercase letters, digits and hyphens.
fn is_id_shape(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn dispatch(store: &Store, command: &str, args: Value) -> Result<Value> {
    match command {
        "task.enqueue" => enqueue(store, &args),
        "task.defer" => defer(store, &args),
        "work.poll" => poll(store, &args),
        "work.started" | "work.release" | "work.settled" => native_event(store, command, &args),
        "work.status" => {
            let ship = required(&args, "ship")?;
            Ok(
                json!({"ship":ship,"tasks":snapshot(store,ship)?,"backlog_alarm":super::backlog::view(store,ship)?}),
            )
        }
        _ => bail!("unknown scheduling command {command}"),
    }
}
