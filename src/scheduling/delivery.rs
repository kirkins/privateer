use super::*;

pub(super) fn profile(store: &Store, ship: &str) -> Result<Value> {
    let raw: String =
        store
            .connection()
            .query_row("SELECT record FROM ships WHERE name=?1", [ship], |r| {
                r.get(0)
            })?;
    let record: Value = serde_json::from_str(&raw)?;
    ensure!(
        record["location"] == "here",
        "work must be scheduled in its owning home"
    );
    ensure!(
        record["first_mate"].is_object(),
        "ship has no first mate profile"
    );
    Ok(record["first_mate"].clone())
}

/// Why ready work cannot be offered now. A headless launch may use the
/// first-mate fallback in place of a backed-off own route; a hosted session
/// keeps its own route, so its work waits as before.
fn blocked(
    store: &Store,
    ship: &str,
    task: &str,
    profile: &Value,
    headless: bool,
    now: i64,
) -> Result<Option<String>> {
    if crate::permissions::paused(store, ship)? {
        return Ok(Some("Waiting for tool permission resolution".into()));
    }
    let ledger = crate::jobs::host_ledger(store)?;
    if crate::jobs::stowed(&ledger)?.is_some() {
        return Ok(Some("Waiting for fleet resume".into()));
    }
    if let Some(route) = profile["manifest"]
        .as_str()
        .and_then(|m| crate::routes::route_of(m, &profile["options"]))
    {
        if let Some((until, _)) = crate::routes::blocked(&ledger, &route)? {
            // A hand-edited routes file must not stop the dispatcher: no fallback.
            let fallback = headless
                && matches!(
                    crate::crew_routes::first_mate_fallback(store, ship, task, profile),
                    Ok(Some(_))
                );
            if !fallback {
                return Ok(Some(format!("Waiting for model route until {until}")));
            }
        }
    }
    let wakes: i64 = store.connection().query_row(
        "SELECT count(*) FROM wakes WHERE ship=?1 AND created_ms>?2",
        params![ship, now - 3_600_000],
        |r| r.get(0),
    )?;
    if wakes >= profile["max_wakes_per_hour"].as_i64().unwrap_or(12) {
        return Ok(Some(
            "Waiting for hourly wake budget; rechecked by dispatcher".into(),
        ));
    }
    let awake: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM wakes WHERE ship=?1 AND status IN ('offered','running'))",
        [ship],
        |r| r.get(0),
    )?;
    Ok(awake.then(|| "Waiting for the current coordinator".into()))
}

pub(super) fn ready_ids(store: &Store, ship: &str) -> Result<Vec<String>> {
    let mut statement = store.connection().prepare(
        "SELECT s.task_id FROM task_schedule s JOIN tasks t ON t.id=s.task_id
         WHERE t.owner=?1 AND json_extract(s.record,'$.owner')=?1 AND json_extract(s.record,'$.state')='ready'
           AND json_extract(s.record,'$.not_before_ms')<=?2
         ORDER BY t.rank IS NULL,t.rank,t.rowid LIMIT 100")?;
    Ok(statement
        .query_map(params![ship, Store::now_ms()], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn claim(store: &Store, ship: &str, holder: Option<&str>, mode: &str) -> Result<Option<Scheduled>> {
    let tx = store.write_transaction()?;
    let now = Store::now_ms();
    let presence = crate::presence::presence(store, ship)?;
    if let Some(holder) = holder {
        if presence["attended"] != true || presence["lease"]["holder"] != holder {
            return Ok(None);
        }
    } else if presence["attended"] == true {
        return Ok(None);
    }
    let profile = match profile(store, ship) {
        Ok(profile) => profile,
        Err(error) => {
            for id in ready_ids(store, ship)? {
                let mut work = load(store, &id)?.context("schedule vanished")?;
                fault(
                    store,
                    &mut work,
                    format!("Scheduling profile unavailable: {error}"),
                    now,
                )?;
                save(store, &work)?;
            }
            tx.commit()?;
            return Ok(None);
        }
    };
    // A profile that names no model is offered on the captain's first_mate
    // list, as a wake is; with none of its routes clear the work waits.
    let mut profile = profile;
    let waiting = match crate::crew_routes::listed_first_mate(store, ship, &profile) {
        Ok(crate::crew_routes::Listed::Route(choice)) => {
            crate::crew_routes::onto(&mut profile, &choice);
            None
        }
        Ok(crate::crew_routes::Listed::Unlisted) => None,
        Ok(crate::crew_routes::Listed::Unavailable(why)) => Some(format!("Waiting for {why}")),
        // A hand-edited routes file must not stop the dispatcher.
        Err(error) => Some(format!("Waiting for a readable routes file: {error:#}")),
    };
    for id in ready_ids(store, ship)? {
        let mut work = load(store, &id)?.context("schedule vanished")?;
        check(store, &mut work, now)?;
        if work.state != "ready" || now < work.not_before_ms || live_jobs(store, &id)? {
            save(store, &work)?;
            continue;
        }
        if work.attempts >= MAX_ATTEMPTS {
            fault(
                store,
                &mut work,
                "Scheduling delivery retry budget exhausted",
                now,
            )?;
            save(store, &work)?;
            continue;
        }
        let reason = match &waiting {
            Some(waiting) => Some(waiting.clone()),
            None => blocked(store, ship, &id, &profile, mode == "fresh", now)?,
        };
        if let Some(reason) = reason {
            work.reason = Some(reason);
            save(store, &work)?;
            continue;
        }
        let charter = store.authorize(&id, "spawn")?;
        let offer = format!("work-{}", uuid::Uuid::new_v4());
        work.state = "offered".into();
        work.offer_id = Some(offer.clone());
        work.holder = holder.map(str::to_owned);
        work.job_id = (mode == "fresh").then(|| offer.clone());
        work.profile = Some(profile.clone());
        work.charter_hash = Some(crate::core::hash(&charter));
        // An offer is not an attempt: only a failed or expired delivery counts.
        work.lease_until_ms = now + CLAIM_MS;
        work.reason = None;
        // What made it ready: this offer's wake reason, recorded once.
        let woke_by = work
            .woke_by
            .take()
            .unwrap_or_else(|| json!({"kind": "enqueued"}));
        let cause = json!({"reason": woke_by["kind"], "causes": [woke_by], "more": 0});
        store.connection().execute(
            "INSERT INTO wakes(id,ship,focus,through_seq,job_id,mode,status,created_ms,focuses,reason,cause,dispatcher_generation)
             VALUES(?1,?2,?3,0,?4,?5,?6,?7,?8,'work',?9,(SELECT value FROM metadata WHERE key='dispatcher_generation'))",
            params![offer,ship,format!("task:{id}"),work.job_id,mode,if mode=="fresh"{"running"}else{"offered"},now,json!([format!("task:{id}")]).to_string(),cause.to_string()])?;
        save(store, &work)?;
        store.audit(
            "work.offered",
            &json!({"task_id":id,"offer_id":offer,"revision":work.revision,"mode":mode}),
        )?;
        crate::wake_reason::record(
            store,
            ship,
            Some(&offer),
            "work",
            &cause,
            &json!({"task_id": id, "revision": work.revision, "delivery": mode}),
        )?;
        tx.commit()?;
        return Ok(Some(work));
    }
    tx.commit()?;
    Ok(None)
}

fn prompt(work: &Scheduled, attention: &Value) -> String {
    // Only state a reading we actually measured; a headless handoff has none.
    let mut attention = crate::attention::line(attention)
        .map(|line| format!("{line}\n"))
        .unwrap_or_default();
    if let Some(resumed_by) = &work.resumed_by {
        attention = format!("Resumed: {resumed_by}\n{attention}");
    }
    format!(
        "Scheduled work on task {} (revision {}). Offer {}.\n{}Prepared assignment:\n{}\n\nYou coordinate this task under its existing charter. Start a concrete step, delegate prepared child work, or complete the task with verified evidence. For a wait use task.defer with id, offer_id and wait {{kind: time|attention|task|request|remote|manual, reason, until_ms or ref}}. A prose note does not schedule resumption. Record your result in the task's note. Include work_offer: \"{}\" on agent.start, job.start, or errand.start for work under this task. Do not enqueue this task again while its claim or payload is live.\n",
        work.task_id,
        work.revision,
        work.offer_id.as_deref().unwrap_or(""),
        attention,
        work.brief,
        work.offer_id.as_deref().unwrap_or("")
    )
}

fn lease(store: &Store, args: &Value) -> Result<(String, String)> {
    let ship = required(args, "ship")?.to_owned();
    let holder = required(args, "holder")?.to_owned();
    let presence = crate::presence::presence(store, &ship)?;
    ensure!(
        holder.starts_with(crate::presence::HOSTED)
            && presence["attended"] == true
            && presence["lease"]["holder"] == holder,
        "work delivery requires the current hosted presence lease"
    );
    Ok((ship, holder))
}

/// The fault a silent hosted adapter leaves on ready work.
const SILENT: &str =
    "Ready work has no responsive hosted work adapter; no competing session was launched";

/// How recent a silent-adapter fault must be for the adapter's next poll to
/// take it back. A harness that starts again (a wake, an approvals refresh or
/// a relaunch) polls within a minute or two, so its faults are minutes old.
/// An older fault is the first mate's to handle from its notice: they may
/// have acted on it, and a scheduled window may have passed. This also keeps
/// faults stored before recovery existed (with no fault time) where they are.
const RECOVER_MS: i64 = 5 * 60_000;

/// An adapter reporting again takes back the work its recent silence
/// faulted, so the work is offered as its prepared assignment rather than
/// left behind a notice. Only work nobody has taken up since is recovered:
/// the task is still open and still this ship's, no job in its tree has run
/// since the fault, the fault is within `RECOVER_MS`, and the revision has
/// faulted fewer than `MAX_ATTEMPTS` times. The task's status is never
/// changed here.
fn recover(store: &Store, ship: &str) -> Result<()> {
    let now = Store::now_ms();
    let mut statement = store.connection().prepare(
        "SELECT s.task_id FROM task_schedule s JOIN tasks t ON t.id=s.task_id
         WHERE t.owner=?1 AND t.status='open'
           AND json_extract(s.record,'$.owner')=?1 AND json_extract(s.record,'$.state')='fault'
           AND json_extract(s.record,'$.reason')=?2 AND json_extract(s.record,'$.faulted_ms')>?3
         LIMIT 100",
    )?;
    let ids = statement
        .query_map(params![ship, SILENT, now - RECOVER_MS], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for id in ids {
        let Some(mut work) = load(store, &id)? else {
            continue;
        };
        if work.faults >= MAX_ATTEMPTS || jobs_since(store, &id, work.faulted_ms)? {
            continue;
        }
        ready_again(&mut work);
        work.woke_by = Some(json!({"kind": "retry"}));
        work.reason = Some("The hosted work adapter responds again; offered anew".into());
        save(store, &work)?;
        store.audit(
            "work.recovered",
            &json!({"task_id": id, "revision": work.revision}),
        )?;
    }
    Ok(())
}

pub(super) fn poll(store: &Store, args: &Value) -> Result<Value> {
    let tx = store.write_transaction()?;
    let (ship, holder) = lease(store, args)?;
    let idle = args["idle"].as_bool().context("idle must be boolean")?;
    let now = Store::now_ms();
    // An idle spell lasts while this session keeps reporting idle without a
    // gap; a busy report, a new session or a silent adapter starts it afresh.
    store.connection().execute("INSERT INTO work_sessions(ship,holder,idle,checked_ms,idle_since_ms) VALUES(?1,?2,?3,?4,CASE WHEN ?3 THEN ?4 END)
        ON CONFLICT(ship) DO UPDATE SET idle_since_ms=CASE WHEN NOT excluded.idle THEN NULL
          WHEN work_sessions.holder=excluded.holder AND work_sessions.idle AND excluded.checked_ms-work_sessions.checked_ms<=?5
          THEN COALESCE(work_sessions.idle_since_ms,excluded.checked_ms) ELSE excluded.checked_ms END,
          holder=excluded.holder,idle=excluded.idle,checked_ms=excluded.checked_ms", params![ship,holder,idle,now,IDLE_GAP_MS])?;
    recover(store, &ship)?;
    // Renew only this native session's active turn. Readiness is not progress.
    if !idle {
        let mut statement = store.connection().prepare("SELECT task_id FROM task_schedule WHERE json_extract(record,'$.holder')=?1 AND json_extract(record,'$.owner')=?2 AND json_extract(record,'$.state')='claimed' LIMIT 100")?;
        let ids = statement
            .query_map(params![holder, ship], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for id in ids {
            let Some(mut w) = load(store, &id)? else {
                continue;
            };
            w.lease_until_ms = now + CLAIM_MS;
            save(store, &w)?;
        }
        tx.commit()?;
        return Ok(json!({"offer":null,"state":"busy"}));
    }
    tx.commit()?;
    reconcile_for(store, Some(&ship))?;
    if ready_ids(store, &ship)?.is_empty() {
        return Ok(json!({"offer":null,"state":"idle"}));
    }
    let attention = if args["attention"].is_object() {
        crate::attention::dispatch(
            store,
            "attention.report",
            json!({"ship":ship,"holder":holder,"kind":"sample",
            "tokens":args["attention"]["tokens"],"window":args["attention"]["window"]}),
        )?
    } else {
        json!({"pressure":"unknown"})
    };
    let mode = if attention["pressure"] == "normal" {
        "hosted"
    } else {
        "fresh"
    };
    let offered = claim(store, &ship, Some(&holder), mode)?;
    Ok(match offered {
        Some(work) if mode == "hosted" => {
            json!({"offer":{"id":work.offer_id,"task_id":work.task_id,"revision":work.revision,"prompt":prompt(&work, &attention)},"state":"offered"})
        }
        Some(work) => {
            json!({"offer":null,"state":"focused_handoff","task_id":work.task_id,"offer_id":work.offer_id})
        }
        None => json!({"offer":null,"state":"waiting"}),
    })
}

pub(super) fn native_event(store: &Store, command: &str, args: &Value) -> Result<Value> {
    let tx = store.write_transaction()?;
    let (ship, holder) = lease(store, args)?;
    let offer = required(args, "offer_id")?;
    let id: String = store
        .connection()
        .query_row(
            "SELECT task_id FROM task_schedule WHERE json_extract(record,'$.offer_id')=?1",
            [offer],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown work offer")?;
    let mut work = load(store, &id)?.context("schedule vanished")?;
    ensure!(
        work.owner == ship && work.holder.as_deref() == Some(&holder) && work.job_id.is_none(),
        "offer is not owned by this native session"
    );
    if matches!(command, "work.started" | "work.release") {
        ensure!(
            owner(store, &id)?["owner"] == ship,
            "task ownership changed"
        );
    }
    let now = Store::now_ms();
    match command {
        "work.started" => {
            ensure!(
                matches!(work.state.as_str(), "offered" | "claimed"),
                "offer is not pending"
            );
            work.state = "claimed".into();
            work.lease_until_ms = now + CLAIM_MS;
            store
                .connection()
                .execute("UPDATE tasks SET status='running' WHERE id=?1", [&id])?;
            store.connection().execute(
                "UPDATE wakes SET status='running' WHERE id=?1 AND reason='work'",
                [offer],
            )?;
        }
        "work.release" => {
            ensure!(
                work.state == "offered",
                "only an unaccepted offer can be released"
            );
            finish_wake(store, &work, "failed", now)?;
            work.attempts += 1;
            reopen(store, &mut work)?;
            work.woke_by = Some(json!({"kind": "retry"}));
            work.not_before_ms = now + 60_000;
            work.reason = Some("Delivery failed; bounded retry after cooldown".into());
        }
        "work.settled" => {
            advance(store, &mut work, now)?;
            if matches!(work.state.as_str(), "claimed" | "executing") {
                if live_jobs(store, &id)? {
                    work.state = "executing".into();
                    finish_wake(store, &work, "succeeded", now)?;
                } else {
                    fault(
                        store,
                        &mut work,
                        "Native turn ended without completion, live child work or a typed wait",
                        now,
                    )?;
                }
            }
        }
        _ => unreachable!(),
    }
    save(store, &work)?;
    tx.commit()?;
    Ok(view(&work))
}

fn start_headless(store: &mut Store, work: &mut Scheduled) -> Result<()> {
    let id = work.offer_id.clone().context("offer id missing")?;
    let mut profile = work.profile.clone().context("offer profile missing")?;
    let existing: Option<String> = store
        .connection()
        .query_row("SELECT record FROM jobs WHERE id=?1", [&id], |r| r.get(0))
        .optional()?;
    if let Some(raw) = existing {
        let job: Value = serde_json::from_str(&raw)?;
        ensure!(
            job["task_id"] == work.task_id,
            "scheduled job id belongs to another task"
        );
    } else {
        let charter = store.authorize(&work.task_id, "spawn")?;
        ensure!(
            work.charter_hash.as_deref() == Some(&crate::core::hash(&charter)),
            "work authority changed after offer"
        );
        let presence = crate::presence::presence(store, &work.owner)?;
        if let Some(holder) = &work.holder {
            ensure!(
                presence["attended"] != true || presence["lease"]["holder"] == *holder,
                "hosted handoff lease was replaced"
            );
        } else {
            ensure!(
                presence["attended"] != true,
                "a hosted session acquired the ship before launch"
            );
        }
        if let Some(fallback) =
            crate::crew_routes::first_mate_fallback(store, &work.owner, &work.task_id, &profile)?
        {
            profile["manifest"] = fallback.manifest;
            profile["options"] = fallback.options;
            profile.as_object_mut().unwrap().remove("config");
            profile.as_object_mut().unwrap().remove("tool_bundles");
        }
        let mut args = json!({"id":id,"work_offer":id,"task_id":work.task_id,"manifest":profile["manifest"],"options":profile["options"],"wake":prompt(work, &Value::Null)});
        for key in ["tool_bundles", "config"] {
            if let Some(value) = profile.get(key) {
                args[key] = value.clone();
            }
        }
        crate::dispatch(store, "agent.start", args)?;
    }
    // A fast worker may already have deferred or completed the task. Never
    // overwrite its durable result with the launcher's stale snapshot.
    let tx = store.write_transaction()?;
    let mut current = load(store, &work.task_id)?.context("schedule vanished during launch")?;
    ensure!(
        current.offer_id == work.offer_id,
        "scheduled offer changed during launch"
    );
    if current.state == "offered" {
        current.state = "claimed".into();
        current.lease_until_ms = Store::now_ms() + CLAIM_MS;
        store.connection().execute(
            "UPDATE tasks SET status='running' WHERE id=?1 AND status='open'",
            [&work.task_id],
        )?;
        save(store, &current)?;
    }
    *work = current;
    tx.commit()?;
    Ok(())
}

fn launch_failed(store: &Store, work: &mut Scheduled, error: &anyhow::Error) -> Result<()> {
    let tx = store.write_transaction()?;
    let mut current =
        load(store, &work.task_id)?.context("schedule vanished during launch failure")?;
    if current.offer_id == work.offer_id && matches!(current.state.as_str(), "offered" | "claimed")
    {
        fault(
            store,
            &mut current,
            format!("Scheduled launch failed: {error}"),
            Store::now_ms(),
        )?;
        save(store, &current)?;
    }
    *work = current;
    tx.commit()?;
    Ok(())
}

pub(crate) fn tick(store: &mut Store) -> Result<Vec<Value>> {
    reconcile_for(store, None)?;
    // Recover recorded launch intents before selecting another task. Reusing
    // the same job id lets agent.start recover a crash without a second payload.
    let mut statement=store.connection().prepare("SELECT task_id FROM task_schedule WHERE json_extract(record,'$.state')='offered' AND json_extract(record,'$.job_id') IS NOT NULL LIMIT 100")?;
    let ids = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut launched = vec![];
    for id in ids {
        // The SELECT above ran outside this pass's transactions: a row it
        // listed may have been reconciled or released since.
        let Some(mut work) = load(store, &id)? else {
            continue;
        };
        if let Err(error) = start_headless(store, &mut work) {
            launch_failed(store, &mut work, &error)?;
        }
        launched.push(view(&work));
    }
    let mut statement=store.connection().prepare("SELECT DISTINCT json_extract(record,'$.owner') FROM task_schedule WHERE json_extract(record,'$.state')='ready' LIMIT 100")?;
    let ships = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for ship in ships {
        let tx = store.write_transaction()?;
        if crate::presence::attended(store, &ship)? {
            let session = crate::presence::session(store, &ship)?;
            let hosted = session
                .as_ref()
                .filter(|s| s.holder.starts_with(crate::presence::HOSTED));
            let now = Store::now_ms();
            let healthy:bool=store.connection().query_row("SELECT EXISTS(SELECT 1 FROM work_sessions WHERE ship=?1 AND holder=?2 AND checked_ms>?3)",params![ship,hosted.map(|s|s.holder.as_str()),now-ADAPTER_MS],|r|r.get(0))?;
            for id in ready_ids(store, &ship)? {
                let Some(mut work) = load(store, &id)? else {
                    continue;
                };
                let Some(session) = hosted else {
                    work.reason = Some("Waiting for the person-started session to leave; automatic native delivery is hosted only".into());
                    save(store, &work)?;
                    continue;
                };
                // The adapter's grace runs from the later of the work becoming
                // ready and the holder's session beginning: a new or woken
                // session has not reported yet.
                let since = work
                    .ready_ms
                    .max(work.created_ms)
                    .max(work.not_before_ms)
                    .max(session.since_ms);
                if live_jobs(store, &id)? {
                    // As claim does: a running crew is the task's work, not a fault.
                    work.reason = Some("Waiting for the task's live jobs to end".into());
                } else if session.asleep {
                    // Ready work is the sleeping pane's alarm: it wakes and polls.
                    // A pane asleep under a tool-permission pause does not wake
                    // (its alarm waits for the pause), and its work waits here
                    // without a fault: the captain already has that decision
                    // in front of them, and the work is offered once it lifts.
                    work.reason = Some(
                        "Waiting for the sleeping hosted session to wake for ready work".into(),
                    );
                } else if !healthy && now - since > CLAIM_MS {
                    fault(store, &mut work, SILENT, now)?;
                } else {
                    work.reason = Some(
                        "Waiting for the hosted session to accept ready work when idle".into(),
                    );
                }
                save(store, &work)?;
            }
            tx.commit()?;
            continue;
        }
        tx.commit()?;
        if let Some(mut work) = claim(store, &ship, None, "fresh")? {
            if let Err(error) = start_headless(store, &mut work) {
                launch_failed(store, &mut work, &error)?;
            }
            launched.push(view(&work));
        }
    }
    Ok(launched)
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    const SHIP: &str = "infra";
    const PANE: &str = "host:infra:pane";
    const DAY_MS: i64 = 86_400_000;

    /// The fixture's home with `infra` here and a hosted session aboard.
    fn hosted() -> (tempfile::TempDir, Store) {
        let (temp, store) = fixture::home();
        store
            .connection()
            .execute(
                "INSERT OR REPLACE INTO ships(name,record) VALUES(?1,?2)",
                params![SHIP, json!({"location":"here","first_mate":{}}).to_string()],
            )
            .unwrap();
        crate::presence::attend(&store, SHIP, PANE, 90, None).unwrap();
        (temp, store)
    }

    /// `work-1` enqueued days ago, and ready since then.
    fn ready_long_ago(store: &Store) {
        fixture::schedule(store, "ready", None);
        edit(store, |w| {
            w.created_ms -= 3 * DAY_MS;
            w.ready_ms -= 3 * DAY_MS;
            w.last_checked_ms -= 3 * DAY_MS;
        });
    }

    fn edit(store: &Store, change: impl FnOnce(&mut Scheduled)) {
        let mut work = load(store, "work-1").unwrap().unwrap();
        change(&mut work);
        save(store, &work).unwrap();
    }

    /// As if the current holder's session began `ms` earlier.
    fn age_session(store: &Store, ms: i64) {
        store
            .connection()
            .execute(
                "UPDATE presence SET holder_since_ms=holder_since_ms-?1, started_ms=started_ms-?1",
                [ms],
            )
            .unwrap();
    }

    fn work(store: &Store) -> Scheduled {
        load(store, "work-1").unwrap().unwrap()
    }

    fn holder(store: &Store) -> String {
        crate::presence::session(store, SHIP)
            .unwrap()
            .unwrap()
            .holder
    }

    fn idle_report(store: &Store, holder: &str) {
        let now = Store::now_ms();
        store
            .connection()
            .execute(
                "INSERT OR REPLACE INTO work_sessions(ship,holder,idle,checked_ms,idle_since_ms)
                 VALUES(?1,?2,1,?3,?4)",
                params![SHIP, holder, now, now - 10_000],
            )
            .unwrap();
    }

    fn assert_waiting(store: &Store, reason: &str) {
        let work = work(store);
        assert_eq!(work.state, "ready", "{:?}", work.reason);
        assert!(
            work.reason.as_deref().is_some_and(|r| r.contains(reason)),
            "{:?}",
            work.reason
        );
    }

    fn assert_silent_fault(store: &Store) {
        let work = work(store);
        assert_eq!(work.state, "fault", "{:?}", work.reason);
        assert_eq!(work.reason.as_deref(), Some(SILENT));
    }

    /// A sleeping pane is not a silent adapter: ready work waits for it and is
    /// its alarm. Woken, its harness has the full grace before its first poll,
    /// and is faulted only after it. Its first poll then takes the work back.
    #[test]
    fn ready_work_wakes_a_sleeping_session_and_is_never_faulted_against_it() {
        let (_temp, mut store) = hosted();
        idle_report(&store, PANE);
        let asleep = crate::host::sleep::sleep(&store, SHIP, PANE, 1)
            .unwrap()
            .expect("an idle session with nothing ready sleeps");
        ready_long_ago(&store);
        age_session(&store, DAY_MS);
        tick(&mut store).unwrap();
        assert_waiting(&store, "sleeping hosted session to wake");
        assert_eq!(
            crate::host::sleep::alarm(&store, SHIP, &asleep).unwrap(),
            Some("scheduled work")
        );

        crate::host::sleep::woke(&store, SHIP, "scheduled work", None);
        tick(&mut store).unwrap();
        assert_waiting(&store, "accept ready work when idle");

        age_session(&store, CLAIM_MS + 1_000);
        tick(&mut store).unwrap();
        assert_silent_fault(&store);

        let polled = crate::scheduling::dispatch(
            &store,
            "work.poll",
            json!({"ship":SHIP,"holder":asleep.holder,"idle":false}),
        )
        .unwrap();
        assert_eq!(polled["state"], "busy", "{polled}");
        let work = work(&store);
        assert_eq!(work.state, "ready", "{:?}", work.reason);
        assert_eq!(work.woke_by, Some(json!({"kind": "retry"})));
        let recovered: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM audit WHERE kind='work.recovered'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recovered, 1);
    }

    /// Each runtime hand-over of the lease, and a new pane taking it after a
    /// relaunch, starts a session that has not polled yet. It gets the grace
    /// from then, however long the work has been ready.
    #[test]
    fn a_new_holder_has_the_full_grace_before_its_first_poll() {
        type Swap = fn(&Store) -> String;
        let swaps: [(&str, Swap); 3] = [
            ("relaunch fence", |store| {
                store
                    .connection()
                    .execute(
                        "INSERT INTO hosted_relaunches(ship,holder,force,requested_ms,expires_ms)
                         VALUES(?1,?2,0,0,?3)",
                        params![SHIP, PANE, Store::now_ms() + 60_000],
                    )
                    .unwrap();
                let (_, fenced, _) = crate::host::upgrade::fence(store, SHIP, PANE, 0)
                    .unwrap()
                    .expect("a requested relaunch of an idle pane is fenced");
                fenced
            }),
            // approvals::fence moves the lease by this same hand-over.
            ("approvals refresh", |store| {
                assert!(crate::presence::hand_over(store, SHIP, PANE, "host:infra:fresh").unwrap());
                "host:infra:fresh".into()
            }),
            ("relaunched pane", |store| {
                crate::presence::leave(store, SHIP, PANE).unwrap();
                crate::presence::attend(store, SHIP, "host:infra:next", 90, None).unwrap();
                "host:infra:next".into()
            }),
        ];
        for (swap, run) in swaps {
            let (_temp, mut store) = hosted();
            idle_report(&store, PANE);
            age_session(&store, DAY_MS);
            let new = run(&store);
            assert_eq!(holder(&store), new, "{swap}");
            ready_long_ago(&store);
            tick(&mut store).unwrap();
            assert_waiting(&store, "accept ready work when idle");
            age_session(&store, CLAIM_MS + 1_000);
            tick(&mut store).unwrap();
            assert_silent_fault(&store);
        }
    }

    /// A crew started directly on the task is its work running, not a silent
    /// adapter, however long the adapter has been quiet; nor does it wake a
    /// sleeping pane.
    #[test]
    fn ready_work_with_a_live_job_is_not_faulted() {
        let (_temp, mut store) = hosted();
        ready_long_ago(&store);
        age_session(&store, DAY_MS);
        store
            .connection()
            .execute(
                "INSERT INTO jobs(id,record) VALUES('crew-1',?1)",
                [json!({"id":"crew-1","task_id":"work-1","status":"running"}).to_string()],
            )
            .unwrap();
        tick(&mut store).unwrap();
        assert_waiting(&store, "live jobs to end");
        assert!(!crate::scheduling::has_ready(&store, SHIP).unwrap());
        store
            .connection()
            .execute(
                "UPDATE jobs SET record=json_set(record,'$.status','succeeded')",
                [],
            )
            .unwrap();
        tick(&mut store).unwrap();
        assert_silent_fault(&store);
    }

    /// Work enqueued days ago that becomes ready now has the full grace from
    /// becoming ready, not from its enqueue.
    #[test]
    fn work_that_becomes_ready_now_has_its_grace_from_then() {
        let (_temp, mut store) = hosted();
        crate::dispatch(
            &mut store,
            "task.create",
            json!({"id":"dep","charter_id":"project","owner":"infra","title":"Dependency"}),
        )
        .unwrap();
        fixture::schedule(&store, "waiting", Some(json!({"kind":"task","ref":"dep"})));
        edit(&store, |w| {
            w.created_ms -= 3 * DAY_MS;
            w.ready_ms -= 3 * DAY_MS;
        });
        age_session(&store, DAY_MS);
        store
            .connection()
            .execute("UPDATE tasks SET status='done' WHERE id='dep'", [])
            .unwrap();
        tick(&mut store).unwrap();
        assert_waiting(&store, "accept ready work when idle");
        edit(&store, |w| w.ready_ms -= CLAIM_MS + 1_000);
        tick(&mut store).unwrap();
        assert_silent_fault(&store);
    }

    /// A schedule and a lease stored before these fields existed still load:
    /// the work's grace runs from its enqueue, the session's from its lease.
    #[test]
    fn records_from_before_ready_and_holder_times_still_load() {
        let (_temp, mut store) = hosted();
        fixture::schedule(&store, "ready", None);
        store
            .connection()
            .execute(
                "UPDATE task_schedule SET record=json_remove(record,'$.ready_ms')",
                [],
            )
            .unwrap();
        assert_eq!(work(&store).ready_ms, 0);
        store
            .connection()
            .execute_batch("ALTER TABLE presence DROP COLUMN holder_since_ms;")
            .unwrap();
        crate::presence::init(&store).unwrap();
        let session = crate::presence::session(&store, SHIP).unwrap().unwrap();
        let started: i64 = store
            .connection()
            .query_row("SELECT started_ms FROM presence", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session.since_ms, started);
        assert!(!session.asleep);
        age_session(&store, DAY_MS);
        tick(&mut store).unwrap();
        assert_waiting(&store, "accept ready work when idle");
        edit(&store, |w| w.created_ms -= CLAIM_MS + 1_000);
        tick(&mut store).unwrap();
        assert_silent_fault(&store);
    }

    /// Recovery takes back only work nobody has taken up since its recent
    /// fault, and never changes the task's status. Each case starts from a
    /// fresh silent-adapter fault on the open task.
    #[test]
    fn a_responding_adapter_recovers_only_untouched_recent_faults() {
        fn job(store: &Store, id: &str, status: &str, created_ms: i64, finished_ms: Option<i64>) {
            store
                .connection()
                .execute(
                    "INSERT INTO jobs(id,record) VALUES(?1,?2)",
                    params![
                        id,
                        json!({"id":id,"task_id":"work-1","status":status,
                               "created_ms":created_ms,"finished_ms":finished_ms})
                        .to_string()
                    ],
                )
                .unwrap();
        }
        fn task_set(store: &Store, column: &str, value: &str) {
            store
                .connection()
                .execute(
                    &format!("UPDATE tasks SET {column}=?1 WHERE id='work-1'"),
                    [value],
                )
                .unwrap();
        }
        type Case = fn(&Store);
        let cases: [(&str, Case, bool); 10] = [
            ("an untouched open task", |_| {}, true),
            // A crew that ended before the fault is not a taking up.
            (
                "a crew that ended before the fault",
                |store| {
                    let faulted = work(store).faulted_ms;
                    job(
                        store,
                        "crew-0",
                        "succeeded",
                        faulted - 60_000,
                        Some(faulted - 1_000),
                    );
                },
                true,
            ),
            (
                "a task handed to another owner",
                |store| task_set(store, "owner", "other"),
                false,
            ),
            (
                "a live crew",
                |store| {
                    job(
                        store,
                        "crew-1",
                        "running",
                        work(store).faulted_ms - 60_000,
                        None,
                    );
                },
                false,
            ),
            (
                "a crew started and finished since the fault",
                |store| {
                    let faulted = work(store).faulted_ms;
                    job(store, "crew-1", "succeeded", faulted, Some(faulted + 1));
                },
                false,
            ),
            (
                "a crew that ran on past the fault",
                |store| {
                    let faulted = work(store).faulted_ms;
                    job(
                        store,
                        "crew-1",
                        "failed",
                        faulted - 60_000,
                        Some(faulted + 1),
                    );
                },
                false,
            ),
            (
                "a running task",
                |store| task_set(store, "status", "running"),
                false,
            ),
            (
                "a finished task",
                |store| task_set(store, "status", "done"),
                false,
            ),
            (
                "a fault older than the horizon",
                |store| {
                    edit(store, |w| w.faulted_ms -= RECOVER_MS + 1_000);
                },
                false,
            ),
            (
                "a fault stored before fault times were kept",
                |store| {
                    store
                        .connection()
                        .execute(
                            "UPDATE task_schedule SET record=json_remove(record,'$.faulted_ms')",
                            [],
                        )
                        .unwrap();
                },
                false,
            ),
        ];
        for (case, set_up, recovered) in cases {
            let (_temp, store) = hosted();
            fixture::schedule(&store, "ready", None);
            let mut faulted = work(&store);
            fault(&store, &mut faulted, SILENT, Store::now_ms()).unwrap();
            save(&store, &faulted).unwrap();
            set_up(&store);
            let status = || -> String {
                store
                    .connection()
                    .query_row("SELECT status FROM tasks WHERE id='work-1'", [], |r| {
                        r.get(0)
                    })
                    .unwrap()
            };
            let before = status();
            recover(&store, SHIP).unwrap();
            let after = work(&store);
            assert_eq!(
                after.state == "ready",
                recovered,
                "{case}: {:?}",
                after.reason
            );
            assert_eq!(status(), before, "{case}: the task's status changed");
            if recovered {
                assert_eq!(after.woke_by, Some(json!({"kind": "retry"})), "{case}");
            } else {
                assert_eq!(after.reason.as_deref(), Some(SILENT), "{case}");
            }
        }

        // A revision its adapter has faulted MAX_ATTEMPTS times stays faulted.
        let (_temp, store) = hosted();
        fixture::schedule(&store, "fault", None);
        edit(&store, |w| {
            w.reason = Some(SILENT.into());
            w.faults = MAX_ATTEMPTS;
            w.faulted_ms = Store::now_ms();
        });
        recover(&store, SHIP).unwrap();
        assert_eq!(work(&store).state, "fault");
    }
}
