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
            let presence = crate::presence::presence(store, &ship)?;
            let hosted = presence["lease"]["holder"]
                .as_str()
                .is_some_and(|holder| holder.starts_with(crate::presence::HOSTED));
            let healthy:bool=store.connection().query_row("SELECT EXISTS(SELECT 1 FROM work_sessions WHERE ship=?1 AND holder=?2 AND checked_ms>?3)",params![ship,presence["lease"]["holder"].as_str(),Store::now_ms()-ADAPTER_MS],|r|r.get(0))?;
            for id in ready_ids(store, &ship)? {
                let Some(mut work) = load(store, &id)? else {
                    continue;
                };
                if !hosted {
                    work.reason = Some("Waiting for the person-started session to leave; automatic native delivery is hosted only".into());
                } else if !healthy && Store::now_ms() - work.created_ms > CLAIM_MS {
                    fault(
                        store,
                        &mut work,
                        "Ready work has no responsive hosted work adapter; no competing session was launched",
                        Store::now_ms(),
                    )?;
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
