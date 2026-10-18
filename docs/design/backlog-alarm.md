# Design: a backlog alarm for an idle first mate

Status: proposal for review; nothing here is implemented. Task `privateer-backlog-alarm`, from the captain's design finding 401ade1f (gap 2). Read with [automatic prepared work](../scheduling.md) and [a first mate's attention](first-mate-attention.md).

## Problem

A sleeping first mate is good: it frees memory. A ship asleep with work it could do and free resources is not. The captain's night is about 8 hours and should move queued and roadmap work without the captain.

What already wakes a sleeping hosted first mate is `sleep::alarm` (`src/host/sleep.rs`): actionable mail that arrived after it slept, or ready scheduled work (`scheduling::has_ready`). Gap 1, `claim()` counting successful offers against the retry budget, is fixed (commit cf5a442, PR 228).

Gap 2: an open task that was never enqueued is invisible to both. `work.status` and bearings label it `unscheduled` ("Legacy or draft task; prepare and explicitly enqueue before automatic execution", `scheduling::snapshot` in `src/scheduling/observation.rs`), `ready_ids` never selects it, and nothing mails the ship about it. A first mate that finishes its crew and goes to sleep without enqueueing the next item leaves the ship idle until morning.

The same is true of a task that is `running` with nothing running under it (a crew ended and the first mate never closed or re-opened the task), and of a schedule in `fault`, whose one high-urgency notice was read and left.

## Model

A **backlog alarm** is a bounded, durable decision made by the dispatcher: this ship is quiet, nothing will start any of these tasks, so send the ship one notice that lists them. The notice is ordinary mail, so the existing paths wake the first mate: `sleep::alarm` for a sleeping hosted pane, the next turn for an idle hosted session, and `activation::consider` for a headless ship. No new wake path, no new process.

The alarm never starts a task. It wakes the owner to decide, so the rule in `docs/scheduling.md` that upgrading never launches legacy tasks still holds.

### Where the check runs

In the dispatcher tick, as a new step `scheduling::backlog::tick(store)` called from `activation::tick_with_delivery_budget` right after `crate::scheduling::tick(store)` and before the `consider` loop over `ships(store)`. Reasons:

- `serve` holds `serve.lock`, so the decision has one writer per home, and every input is in SQLite, so a restart re-reads it rather than losing in-memory state.
- It covers hosted and headless ships with one predicate. The pane's `sleep::alarm` runs every second in the pane's process; adding SQL there would repeat the work for each pane and still miss headless ships.
- Running before `consider` lets a headless ship's wake pick up the notice in the same tick.

`sleep::alarm` changes only in its reason: when the mail that would wake the pane is a backlog notice (sender `crew`, id prefix `backlog-`), it returns `"backlog"` instead of `"mail"`, so `host.woke` records why.

### Eligible tasks

A task is a **backlog candidate** for ship S when all of these hold:

1. `tasks.owner = S`, `tasks.handoff_to IS NULL` (an offered handoff is someone else's next move), and the task is not S's duty task (`S-duty`, excluded as `context::backlog` and `snapshot` do).
2. Its scheduling record is one of:
   - absent (unscheduled), with `tasks.status = 'open'`;
   - absent, with `tasks.status = 'running'` and no job in its subtree in an active status (`scheduling::live_jobs`, statuses `starting, queued, launching, running, orphaned`); a `running` task with live work is being worked;
   - `state = 'fault'` with `tasks.status` not `done` or `cancelled`.
3. `tasks.status` is not `blocked`. A blocked task is excluded whether or not it has a schedule: blocked with a typed wait is `waiting` (the scheduler resumes it), blocked without one is the first mate's recorded decision to leave it.
4. No ancestor has a schedule in `stopped` (`scheduling::stopped_ancestor`) or an active claim (`offered`, `claimed`, `executing`): a coordinator holding the parent owns its children.

Not candidates, by the same rules: schedules in `ready` (the existing alarm fires), `waiting` (deferred with a real wait), `offered`/`claimed`/`executing` (live), `stopped` (manual defer), `done`; tasks `done` or `cancelled`.

Rank does not change eligibility. It orders candidates (`context::BACKLOG_ORDER`, `rank IS NULL, rank, rowid`), and the notice lists the first **10**; the rest are a count.

### Ship gates

The alarm fires for S only when every gate passes. Each gate that fails is reported in the tick output as `{"ship": S, "backlog": "<reason>"}`, never mailed.

| Gate | Check (existing code) |
|---|---|
| Local ship with a first mate | `location = "here"` and `first_mate` is an object (as `activation::ships`) |
| Alarm enabled | `first_mate.backlog_alarm_secs` > 0 (new `SETTINGS` entry in `src/activation.rs`, default 3600, 0 off, range 0..86400) |
| Not stowed | `jobs::stowed(&jobs::host_ledger(store)?)` is none |
| No permission question | `!permissions::paused(store, S)` |
| Own route clear | `activation::own_route_blocked` is false; a hosted session keeps its own route, so a backing-off route waits |
| Hourly wake budget | `wakes` rows for S in the last hour `< max_wakes_per_hour`, the same count `delivery::blocked` and `consider` use |
| Not awake | no `wakes` row for S in `offered` or `running` |
| Nothing else will wake it | `!scheduling::has_ready(store, S)`; for a headless ship, no wake-worthy mail after `consider`'s own mark (the later of `mail_cursor` and the last succeeded wake's `through_seq`), so a first mate that forgets to ack cannot hold the alarm off. A sleeping pane with such mail is already awake, so hosted ships need no mail check |
| Nothing live | no job in an active status on any task S owns; with PR 224, no crew park in `parked`, `resuming` or `held` |
| Quiet | see below |
| Spacing and backoff | `now >= backlog_alarms.next_ms` (below) |
| Candidates | at least one backlog candidate |

**Quiet** means one of:

- hosted and asleep: presence is attended, its holder starts with `presence::HOSTED`, and there is no `work_sessions` row for that holder checked within `sleep::FRESH_MS` (`sleep::sleep` deletes the row; a pane started asleep never writes one);
- hosted, awake and idle for `sleep::DEFAULT_AFTER_SECS` (`sleep::idle_for`), which covers a ship with `sleep_after_secs = 0`;
- unattended (`!presence::attended`) with a first-mate profile: `consider` makes the headless wake.

A person-started session aboard (attended, not hosted) is never quiet: the person is there.

### Firing

In one write transaction:

1. Record the alarm in `backlog_alarms` (below): `fired_ms = now`, `fires += 1`, `considered` = the listed ids, `mail_seq`.
2. Insert the notice with `mail::record_crew`: id `backlog-<S>-<fires>` (so a replay after a crash is `ON CONFLICT DO NOTHING`), sender `crew`, to S, kind `notice`, urgency `normal`, no `task_id`, payload `{"backlog_alarm": {"fires": n, "considered": [...], "total": N}}`. `record_crew` today requires `task_id`; it takes it as optional.
3. For a hosted ship only, insert a `wakes` row: reason `backlog`, focus `backlog`, mode `hosted`, status `succeeded`, `finished_ms = now`, `through_seq = 0`. It is a record, never `offered` or `running`, so it blocks nothing, and it counts against the hourly budget as offers do. A headless ship gets no extra row: the wake `consider` starts for the notice is the one counted.
4. Audit `backlog.alarm` with `{ship, fires, considered, total, backoff_secs, attention}`.

### Focus and brief

The notice belongs to a new fixed focus, `backlog`. `context::check_focus` accepts it beside `captain` and `ship`; `context::focus_of` returns it for a message from `crew` whose id starts with `backlog-`. A headless wake then runs under the duty task (as for any non-task focus in `consider`), keeps its own warm session per focus, and the first mate can leave its judgement in the `backlog` note between wakes.

The body, kept under 2 KB (titles clipped to 80 characters):

```
Backlog check: you are idle and nothing will start these tasks.
- <id> (rank 2): <title> [unscheduled]
- <id>: <title> [running, nothing live]
- <id>: <title> [fault: <reason, 120 chars>]
... and 7 more (work.status lists them).
For each: write assignment:<id> and task.enqueue it; or enqueue and task.defer with a real wait; or task.update status blocked with the reason in its task note; or task.handoff; or close it with evidence. This check does not need the captain; do not mail them about it.
```

When the ship's last attention reading (`attention::summary_for`) is not `normal`, one line replaces the first item of the "For each" sentence: `Attention is <pressure>: enqueue, defer, hand off or close only; do not start work in this conversation. Enqueued work goes to a fresh coordinator.` This is the captain's "high attention may still enqueue but not start": `work.poll` already hands ready work to a fresh coordinator (`focused_handoff`) when pressure is not normal.

### What "changed nothing" means

A wake is **productive** when at least one task it listed is no longer a backlog candidate: enqueued (now `ready`, or `waiting` after a defer), blocked, handed off, closed, owner changed, or its subtree now has a live job. It is measured, not inferred from audit rows, by re-running the candidate predicate over `considered` and comparing with the stored list. A rank change, a note or a status rewrite that leaves the task a candidate does not count: the wake exists to move tasks out of the backlog.

The comparison runs lazily, at the first tick where every gate except spacing passes and `now >= next_ms`, which is after the woken session has finished (gate "not awake") and gone quiet again.

### Bounds and backoff

`backoff_secs` starts at `backlog_alarm_secs` (one hour).

- **Spacing:** `next_ms = fired_ms + backoff_secs`. Nothing fires before it.
- **Unproductive:** at `next_ms` the evaluation finds nothing left the list. Then `backoff_secs = min(2 × backoff_secs, 8 h)`, `next_ms = fired_ms + backoff_secs`, `quiet += 1`, audit `backlog.unproductive`, no fire this tick.
- **Productive:** `backoff_secs` resets to `backlog_alarm_secs`, `quiet = 0`, and the alarm fires again at once if candidates remain.
- **Backlog changed:** a candidate that is not in `considered` (a new task, a reopened one, a new fault) resets `backoff_secs` and `next_ms = max(now, fired_ms + backlog_alarm_secs)`: still never more than once an hour.
- **No candidates:** the row is kept with `considered = []` and backoff reset, so the next candidate fires after the spacing.

An unproductive ship is therefore woken at 0, 2 h, 6 h and 14 h: three wakes in an 8-hour night, then one every 8 hours.

### Durable state

New table, created in `scheduling::init` next to `task_schedule`:

```sql
CREATE TABLE IF NOT EXISTS backlog_alarms(ship TEXT PRIMARY KEY, fired_ms INTEGER NOT NULL,
  next_ms INTEGER NOT NULL, backoff_secs INTEGER NOT NULL, fires INTEGER NOT NULL,
  quiet INTEGER NOT NULL, considered TEXT NOT NULL, mail_seq INTEGER);
```

`considered` holds at most 10 ids. Every decision reads this row in the tick's write transaction, so a dispatcher restart (or `serve --once`) neither re-fires early nor forgets a backoff; `serve`'s generation boundary deletes `sessions`, not this table. A home that never had a backlog has no row.

`work.status` gains `"backlog_alarm": {fired_ms, next_ms, backoff_secs, fires, quiet, considered}` for the ship, and bearings one line (`Backlog alarm: last 02:10, 3 listed; next not before 06:10 (backing off)`), so a morning check reads what happened without the audit log. The audit rows carry the full history.

## Interactions

- **Captain and the away digest.** The notice goes to the ship, never to the captain. The away digest (`away::held`) reads only mail in the captain's mailbox, so a backlog wake adds nothing to it unless the woken first mate writes to the captain, which the brief asks it not to do for this check. The alarm itself never mails the captain, a liaison or a ship owner, and a faulting backlog step reports in the tick output, not by mail.
- **Stow.** A stowed fleet fires nothing (gate). A stow leaves spacing and backoff as they were, so after resume the alarm fires at the next tick only if its spacing has passed.
- **Hourly wake budget.** Counted for hosted ships through the `wakes` record row, and for headless ships through `consider`'s own wake. A spent budget skips the alarm without touching backoff.
- **Route outages.** A blocked own route skips the alarm without touching backoff; the next tick after the route clears may fire. Fallback routes are not used: a headless fallback is for work already decided, and the backlog wake is cheap to delay.
- **Ready work and mail.** Both win: the alarm only fires when neither existing wake would. A backlog wake that enqueues a task leads, after the session goes idle, to `work.poll` offering it in the same session, or `scheduling::tick` launching it headless.
- **Batching.** A headless notice waits for `batch_secs` like any mail, and is grouped with other mail that arrives meanwhile.

## Non-hosted and remote ships

- **Headless ships** (no `hosted` in the profile, or hosted but unattended because the pane is gone): the gates use "unattended" for quiet, and `consider` wakes a headless first mate for the notice under the duty task, focus `backlog`. A hosted ship whose pane is gone is first relaunched by `consider` (`host::launch`), which delivers the notice in the new pane.
- **Remote ships** are skipped: `location = "here"` is a gate, and scheduling already requires work to be scheduled in its owning home (`delivery::profile`). The alarm for a ship on another machine runs in that machine's dispatcher. A machine on an older release simply has no alarm; nothing is sent across machines.

## Failure modes and how the bounds contain them

- **A first mate that wakes and does nothing.** Each unproductive wake doubles the spacing to 8 hours, and an 8-hour night costs it three wakes. `quiet` counts them, so bearings and `work.status` show "backing off, 3 unproductive", and the audit names the tasks. No escalation to the captain: the morning check shows it.
- **A ship with 50 open drafts.** The notice lists the first 10 by rank and gives the count; only those 10 are measured. One handled draft is productive and resets the spacing to an hour, so a diligent first mate works through the backlog at up to 10 tasks an hour, and the hourly wake budget still applies. The notice stays under 2 KB whatever the count, and the candidate query is bounded (`LIMIT 100` like `snapshot`).
- **A wake that fails to start** (headless launch error, route refusal at launch): `consider` records a failed wake and backs off as today. The alarm's own row was written, so it does not fire again before its spacing.
- **A notice that is never read** (the pane cannot start its harness): `host::failed` already backs off the launch, and the notice stays in the mailbox. The alarm adds at most one notice per spacing, and an unread one is unproductive, so notices back off to one every 8 hours.
- **Crash mid-fire.** The fire is one transaction; a replay reuses the mail id.
- **A task that flaps** between candidate and not (enqueued, faulted, reopened): each change counts as the backlog changing, which resets the backoff but never the hourly spacing.
- **Clock skew or a long dispatcher outage.** All comparisons are `now >= next_ms`; an outage only delays the next alarm.

## Optional: `auto_enqueue_ranked`

A ship-level setting under which the dispatcher enqueues ranked, unscheduled, open tasks without waking the first mate.

`task.enqueue` needs a written assignment: `brief`, or the `assignment:<id>` note (`scheduling::commands::enqueue` refuses "prepare an assignment or supply brief before enqueueing"). So the setting could only apply to ranked tasks that already have an assignment note; a ranked task without one would be left to the backlog alarm, which lists it as `[unscheduled, no assignment]`.

**Recommendation: do not build it in this round.** The backlog wake reaches the same result with the owner's judgement at a cost of one wake an hour, and an assignment note drafted for later is not a decision to run it now. `docs/scheduling.md` keeps enqueueing explicit precisely so that writing an assignment and starting a worker do not race. If wanted later: a profile flag `auto_enqueue_ranked` (default false), the dispatcher enqueuing as the operator with `auto: true` in the `task.enqueue` audit row, at most one task per ship per tick, and only tasks whose assignment note is older than `backlog_alarm_secs` (so a note still being written is not taken).

## Changes by file

- `src/scheduling/backlog.rs` (new): candidate predicate, gates, fire, evaluation, `backlog_alarms` reads and writes, the notice text.
- `src/scheduling/mod.rs`: `mod backlog;`, `backlog_alarms` in `init`, `pub(crate) use backlog::tick as backlog_tick`.
- `src/activation.rs`: call the backlog tick in `tick_with_delivery_budget`; `backlog_alarm_secs` in `SETTINGS`; `own_route_blocked` made `pub(crate)`.
- `src/host/sleep.rs`: `alarm` returns `"backlog"` for a backlog notice; the pane's asleep line says backlog checks may wake it.
- `src/mail.rs`: `record_crew` takes an optional `task_id`.
- `src/context.rs`: `backlog` focus in `check_focus` and `focus_of`; the bearings line.
- `src/scheduling/observation.rs` and `work.status` in `src/scheduling/commands.rs`: the `backlog_alarm` block.
- `docs/scheduling.md`: a short section on the alarm and how a first mate leaves a draft on purpose (`task.update` blocked with a reason).

## Tests

All in isolated homes with the existing helpers; time is moved by backdating rows, as `rank_and_shared_wake_budget_apply_to_automatic_work` does with `UPDATE wakes SET created_ms=created_ms-3600001`.

- **(a) Repeated defer and resume stays offered.** Already covered by `a_taken_up_offer_that_defers_on_a_child_is_never_counted_as_an_attempt` in `tests/scheduling.rs` (commit cf5a442, PR 228): a hosted coordinator takes an offer, defers on a child and is offered again four times without a fault or an attempt. Kept as the regression; the backlog tick must not change it (the parent is `waiting` or `offered`, never a candidate).
- **(b) Woken once, not again within the hour.** `tests/hosted.rs`, with `hosted_sleeper` and `sleep_hosted`: create `infra-draft` (open, unscheduled), sleep the pane, `place.tick()`. Assert the mail `backlog-infra-1` from `crew` listing `infra-draft`, the harness woken (`pi-env` exists, `--continue` in argv), `host.woke` audited with reason `backlog`, one `wakes` row with reason `backlog`. Sleep it again without touching the task and tick: no second notice, the tick reports `backlog: spaced until ...`. A headless twin in `tests/scheduling.rs` with `Fleet::new` asserts `consider` starts one wake with focus `backlog` and the second tick starts none.
- **(c) Unproductive backoff.** `tests/scheduling.rs`, unattended ship: fire, mark the wake finished and ack the notice, backdate `backlog_alarms.fired_ms` and `next_ms` by 3600001 and tick: no fire, `backoff_secs` 7200, `quiet` 1, `backlog.unproductive` audited. Backdate past 7200 s: the second notice fires. Then `task.update` the listed task to `blocked`, add a new open task and tick past the spacing: `backoff_secs` back to 3600 and the new notice lists only the new task. A restart variant reopens the `Store` between ticks and asserts the same backoff.
- **(d) Waits and blocks do not trigger.** `tests/scheduling.rs`: one task enqueued then `task.defer`red with a time wait, one unscheduled task set `blocked`, one handed off with `task.handoff`, one `running` with a live job, the duty task. Tick: no notice, report `backlog: no candidates`. Then open a sixth unscheduled task: one notice listing only it.
- **(e) A stowed ship is not woken.** `tests/scheduling.rs`: an open unscheduled task, `stow.start`, tick: no notice, report `backlog: stowed`. `stow.resume` and tick: one notice. The hosted variant in `tests/hosted.rs` asserts the sleeping pane stays asleep while stowed.

Also: a hosted ship idle past `DEFAULT_AFTER_SECS` with `sleep_after_secs = 0` gets the notice as a turn; a person-started session aboard gets none; a spent hourly budget and a blocked route skip without changing `backoff_secs`; 50 open drafts produce one notice of 10 lines and `total: 50`.

## Open questions for the reviewer

1. **Eligible states beyond `open`.** This note includes `running` with nothing live and `fault`. Including faults means a faulted schedule nags hourly after its one high-urgency notice; excluding them leaves a faulted ship idle all night. Is "running, nothing live" safe for tasks whose work runs outside Privateer jobs (a PR waiting on CI, a captain errand)? Those should be `blocked` or deferred, but today often are not.
2. **"Productive" is strict.** Only a listed task leaving the candidate set counts. Should a written `assignment:<id>` note also count, since it is the first half of an enqueue?
3. **Hosted wakes and the budget.** Hosted mail wakes write no `wakes` row today and so cost nothing against `max_wakes_per_hour`; this note makes backlog wakes the first hosted wake that does. Acceptable, or should the alarm keep its own count in `backlog_alarms` instead?
4. **The `backlog` focus.** A fixed focus keeps one triage note per ship but needs `check_focus` and `focus_of` to learn a fourth name, matched on the mail id prefix. The alternative is a `thread:backlog-<S>-<n>` focus per fire, with no code in `context.rs` but no note carried between wakes.
5. **Default on.** With `backlog_alarm_secs` defaulting to 3600, the first tick after an upgrade wakes every quiet ship that has legacy open tasks (once, then hourly at most). Should the default be 0 for ships recorded before the setting, as `sleep_after_secs` defaults on for them?
6. **Route outage with a fallback.** Should a headless ship with a clear `first_mate` fallback route still get the alarm while its own route backs off, as mail wakes do?
7. **PR 224 crew parks.** "Nothing live" should include parked and held crews once `crew_parks` lands; until then a parked crew's task looks like "running, nothing live". Should this land after PR 224, or check for the table?
