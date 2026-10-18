# Design: a backlog alarm for an idle first mate

Status: approved by the Opus design review with eight changes, all folded in below (round 2), and implemented in `src/scheduling/backlog.rs`. Task `privateer-backlog-alarm`, from the captain's design finding 401ade1f (gap 2). Read with [automatic prepared work](../scheduling.md) and [a first mate's attention](first-mate-attention.md).

## Problem

A sleeping first mate is good: it frees memory. A ship asleep with work it could do and free resources is not. The captain's night is about 8 hours and should move queued and roadmap work without the captain.

What already wakes a sleeping hosted first mate is `sleep::alarm` (`src/host/sleep.rs`): actionable mail that arrived after it slept, or ready scheduled work (`scheduling::has_ready`). Gap 1, `claim()` counting successful offers against the retry budget, is fixed (commit cf5a442, PR 228).

Gap 2: an open task that was never enqueued is invisible to both. `work.status` and bearings label it `unscheduled` ("Legacy or draft task; prepare and explicitly enqueue before automatic execution", `scheduling::snapshot` in `src/scheduling/observation.rs`), `ready_ids` never selects it, and nothing mails the ship about it. A first mate that finishes its crew and goes to sleep without enqueueing the next item leaves the ship idle until morning.

The same is true of a task that is `running` with nothing moving it (a crew ended and the first mate never closed or re-opened the task), and of a schedule in `fault`, whose one high-urgency notice was read and left.

## Model

A **backlog alarm** is a bounded, durable decision made by the dispatcher: this ship is quiet, nothing will start any of these tasks, so send the ship one notice that lists them. The notice is ordinary mail, so the existing paths wake the first mate: `sleep::alarm` for a sleeping hosted pane, the adapter's next turn for an idle hosted session, and `activation::consider` for a headless ship. No new wake path, no new process.

The alarm never starts a task. It wakes the owner to decide, so the rule in `docs/scheduling.md` that upgrading never launches legacy tasks still holds.

### Where the check runs

In the dispatcher tick, as a step `scheduling::backlog_tick(store)` called from `activation::tick_with_delivery_budget` right after `crate::scheduling::tick(store)` and before the `consider` loop over `ships(store)`. Its reports go under the tick's `backlog` key. Reasons:

- `serve` holds `serve.lock`, so the decision has one writer per home, and every input is in SQLite, so a restart re-reads it rather than losing in-memory state.
- It covers hosted and headless ships with one predicate. The pane's `sleep::alarm` runs every second in the pane's process; adding SQL there would repeat the work for each pane and still miss headless ships.
- Running before `consider` lets a headless ship's wake pick up the notice in the same tick.

`sleep::alarm` changes only in its reason: when every wake-worthy message since the pane slept is a backlog notice (sender `crew`, correlation `backlog-<S>`), it returns `"backlog"` instead of `"mail"`, so `host.woke` records why.

### Eligible tasks

A task is a **backlog candidate** for ship S when all of these hold:

1. `tasks.owner = S`, `tasks.handoff_to IS NULL` (an offered handoff is someone else's next move), and the task is not S's duty task (the profile's `duty_task`, and `S-duty` as `context::backlog` and `snapshot` exclude it).
2. Its scheduling record is one of:
   - absent (unscheduled), with `tasks.status = 'open'`;
   - absent, with `tasks.status = 'running'` and the task listed by `context::idle_tasks`: no live job in its subtree, no open request the ship sent about it or a task beneath it, and no `waiting` pin on its focus. `idle_tasks` counts a job `parked` on a heavy job, and a crew park in `parked`, `held` or `resuming`, as live (PR 224);
   - `state = 'fault'` with `tasks.status` `open` or `running` (a `running` one must also pass `idle_tasks`).
3. `tasks.status` is not `blocked`. A blocked task is excluded whether or not it has a schedule: blocked with a typed wait is `waiting` (the scheduler resumes it), blocked without one is the first mate's recorded decision to leave it.
4. No open request the ship sent about it or a task beneath it (the same exclusion `idle_tasks` applies, extended to `open` and faulted tasks): it is waiting on the captain or a peer.
5. No ancestor has a schedule in `stopped` (`scheduling::stopped_ancestor`) or an active claim (`offered`, `claimed`, `executing`): a coordinator holding the parent owns its children.

Not candidates, by the same rules: schedules in `ready` (the existing alarm fires), `waiting` (deferred with a real wait), `offered`/`claimed`/`executing` (live), `stopped` (manual defer), `done`; tasks `done` or `cancelled`.

Rank does not change eligibility. It orders candidates (`rank IS NULL, rank, rowid`, as `context::BACKLOG_ORDER`), the query is capped at **100**, and the notice lists the first **10**; the rest are a count.

Each candidate carries its schedule **revision** (0 when it has none). Only `task.enqueue` bumps it, so a listed task that was enqueued, ran and faulted again has a new revision even though it is a candidate again.

### Ship gates

The alarm fires for S only when every gate passes. Each gate that fails is reported in the tick output as `{"ship": S, "skipped": "<reason>"}`, never mailed. A gate that fails leaves the row's spacing and backoff as they were.

| Gate | Check (existing code) |
|---|---|
| Local ship with a first mate | `location = "here"` and `first_mate` is an object (as `activation::ships`) |
| Alarm enabled | `first_mate.backlog_alarm_secs` > 0 (new `SETTINGS` entry in `src/activation.rs`, default 3600, 0 off, range 0..86400) |
| Not stowed | `jobs::stowed(&jobs::host_ledger(store)?)` is none |
| No permission question | `!permissions::paused(store, S)` |
| Not awake | no `wakes` row for S in `offered` or `running` |
| Quiet | see below |
| Own route clear | `activation::own_route_blocked` is false. No fallback route: a headless fallback is for work already decided, and a backlog wake is cheap to delay |
| Nothing else will wake it | `!scheduling::has_ready(store, S)`; for a headless ship, no wake-worthy mail after `consider`'s own mark (the later of `mail_cursor` and the last succeeded wake's `through_seq`), so a first mate that forgets to ack cannot hold the alarm off, and an earlier backlog notice still waiting for its wake holds off the next. A sleeping pane with such mail is already waking, so hosted ships need no mail check |
| Nothing live | no job on any task S owns in an active status (`starting, queued, launching, running, orphaned`) or `parked`, and no crew park of S in `parked`, `held` or `resuming` |
| Candidates | at least one backlog candidate |
| Spacing and backoff | `now >= backlog_alarms.next_ms` (below) |
| Hourly wake budget | `wakes` rows for S in the last hour `< max_wakes_per_hour`, the same count `delivery::blocked` and `consider` use; checked last, at the fire |

**Quiet** means one of:

- hosted and asleep: presence is attended, its holder starts with `presence::HOSTED`, and there is no `work_sessions` row for that holder checked within `sleep::FRESH_MS` (`sleep::sleep` deletes the row; a pane started asleep never writes one);
- hosted, awake and idle for `sleep::DEFAULT_AFTER_SECS` (`sleep::idle_for`), which covers a ship with `sleep_after_secs = 0`;
- unattended (`!presence::attended`) with a **headless** profile (no `host`): `consider` makes the headless wake.

Not quiet:

- a person-started session aboard (attended, not hosted): the person is there;
- a **hosted** profile with no pane (review item 6). `consider` relaunches the pane first, and the new pane can start asleep with its mark at the latest mail (`asleep_at_start` falls back to `MAX(seq)`), which would already include the notice, so the notice would wake nothing. The alarm waits for the tick after the pane is back, reported as `hosted pane not running`.

### Firing

In one write transaction:

1. Update the ship's `backlog_alarms` row (below): `fired_ms = now`, `fires += 1`, `considered` = the listed ids with their revisions, `candidates` = the hash of the full candidate set, `evaluated = 0`, `next_ms = now + backoff_secs`, `mail_id`.
2. Insert the notice with `mail::record_crew`: id `backlog-<S>-<fires>` (so a replay after a crash is `ON CONFLICT DO NOTHING`), sender `crew`, to S, kind `notice`, urgency `normal`, **correlation `backlog-<S>`**, no `task_id`, payload `{"backlog_alarm": {"fires": n, "considered": [...], "total": N}}`. `record_crew` takes `task_id` and `correlation` as optional.
3. For a hosted ship only, insert a `wakes` row: id = the mail id, reason `backlog`, focus `thread:backlog-<S>`, mode `hosted`, status `succeeded`, `finished_ms = now`, `through_seq = 0`. It is a record, never `offered` or `running`, so it blocks nothing, and it counts against the hourly budget (review: fine). `consider`'s last-wake lookup for retry backoff filters on `reason <> 'backlog'`, so a backlog record never hides a recent failed wake (review item 8). A headless ship gets no extra row: the wake `consider` starts for the notice is the one counted.
4. Audit `backlog.alarm` with `{ship, fires, considered, total, backoff_secs, attention}`.

### Focus and brief

Every notice carries `correlation: "backlog-<S>"`, a root that is not a message id. `context::focus_of` finds no task behind it and returns `thread:backlog-<S>`, a stable focus per ship, so the first mate's note for it carries over between wakes and `check_focus` needs no change. A headless wake on that focus runs under the duty task (as for any non-task focus in `consider`) and keeps its own warm session.

A correlation on a notice is not a reply. Only `kind = "reply"` closes a request (`mail::send` marks the correlated request `answered`), and `record_crew` writes only notices and requests, so a backlog notice never answers anything; `focus.digest` on `thread:backlog-<S>` lists every backlog notice (`id = root OR correlation = root`).

The body, kept under 2 KB (titles clipped to 80 characters):

```
Backlog check: you are idle and nothing will start these tasks.
- <id> (rank 2): <title> [unscheduled]
- <id>: <title> [running, nothing moving it]
- <id>: <title> [fault: <reason, 120 chars>]
... and 7 more (work.status lists them).
For each: write assignment:<id> and task.enqueue it; or enqueue and task.defer with a real wait; or task.update status blocked with the reason in its task note; or task.handoff; or close it with evidence. This check does not need the captain; do not mail them about it.
```

Only when the ship's last attention reading (`attention::summary_for`) has pressure `high` does one line replace the first item of the "For each" sentence: `Attention is high: enqueue, defer, hand off or close only; do not start work in this conversation. Enqueued work goes to a fresh coordinator.` `unknown` (no context sample, normal for a sleeping or headless ship) and `elevated` keep the ordinary brief (review item 7). `work.poll` already hands ready work to a fresh coordinator (state `focused_handoff`) when pressure is not normal.

### What "changed nothing" means

A wake is **productive** when at least one task it listed is no longer a backlog candidate (enqueued, now `ready` or `waiting`; blocked; handed off; closed; owner changed; a live job beneath it), **or** is a candidate again under a new schedule revision (it was enqueued and has since faulted, which is work done; review item 3). It is measured by re-running the candidate query and comparing with the stored `considered` ids and revisions, not inferred from audit rows. A rank change, a note, a written assignment note (review Q2) or a status rewrite that leaves the task a candidate at the same revision does not count: the wake exists to move tasks out of the backlog.

**Backlog changed** is separate: the hash of the full candidate set (ids and revisions, at most 100) differs from the hash stored at the fire. A new task, a reopened one, a new fault, or a task past the first 10 being handled all change it. Comparing a hash rather than `considered` means a ship with 50 drafts does not see "40 new candidates" on every tick (review item 2).

### Bounds and backoff

`backoff_secs` starts at `backlog_alarm_secs` (one hour); the cap is `max(8 h, backlog_alarm_secs)`. Each fire is **evaluated once** (`evaluated` flag, review item 1), at the first tick where the gates pass and either `now >= next_ms` or the backlog changed:

- **Unproductive and unchanged:** `backoff_secs = min(2 × backoff_secs, cap)`, `next_ms = fired_ms + backoff_secs`, `quiet += 1`, audit `backlog.unproductive`.
- **Productive:** `backoff_secs = backlog_alarm_secs`, `quiet = 0`, `next_ms = fired_ms + backlog_alarm_secs`, audit `backlog.productive`.
- **Changed but not productive:** `backoff_secs = backlog_alarm_secs`, `next_ms = fired_ms + backlog_alarm_secs`; `quiet` is kept.

Then, in the same tick, the alarm fires if `now >= next_ms` (spacing never drops below one interval from the last fire). After evaluation, a later change of the backlog resets `backoff_secs` and `next_ms` the same way, so a new task is listed as soon as the hour since the last fire has passed. With no candidates the row's backoff is reset and its stored set emptied, so the next candidate fires after the spacing.

An unproductive ship is therefore woken at 0, 2 h, 6 h and 14 h: fire at 0 (next 1 h); evaluate at 1 h (backoff 2 h, next 2 h); fire at 2 h (next 4 h); evaluate at 4 h (backoff 4 h, next 6 h); fire at 6 h (next 10 h); evaluate at 10 h (backoff 8 h, next 14 h); fire at 14 h. Three wakes in an 8-hour night, then one every 8 hours.

**Upgrade (review Q5).** The alarm defaults on, but a ship with no row is treated as if it had just fired: its first tick writes a row with `fired_ms = now`, `evaluated = 1` and an empty set, and fires nothing. The first notice therefore comes one interval after the upgrade (or after the ship is first seen), never on the first tick.

### Durable state

New table, created in `scheduling::init` next to `task_schedule`:

```sql
CREATE TABLE IF NOT EXISTS backlog_alarms(ship TEXT PRIMARY KEY, fired_ms INTEGER NOT NULL,
  next_ms INTEGER NOT NULL, backoff_secs INTEGER NOT NULL, fires INTEGER NOT NULL,
  quiet INTEGER NOT NULL, evaluated INTEGER NOT NULL CHECK(evaluated IN (0,1)),
  considered TEXT NOT NULL, candidates TEXT NOT NULL, mail_id TEXT);
```

`considered` holds at most 10 `{id, revision}` pairs; `candidates` is a hash. Every decision reads this row in the tick's write transaction, so a dispatcher restart (or `serve --once`) neither re-fires early nor forgets a backoff; `serve`'s generation boundary deletes `sessions`, not this table.

`work.status` gains `"backlog_alarm": {fired_ms, next_ms, backoff_secs, fires, quiet, evaluated, considered}` for the ship (null without a row), and bearings one line (`Backlog alarm: 3 fired, last listed 4; next not before <ms> (backing off, 2 unproductive)`), so a morning check reads what happened without the audit log. The audit rows carry the full history.

## Interactions

- **Captain and the away digest.** The notice goes to the ship, never to the captain. The away digest (`away::held`) reads only mail in the captain's mailbox, so a backlog wake adds nothing to it unless the woken first mate writes to the captain, which the brief asks it not to do for this check. The alarm itself never mails the captain, a liaison or a ship owner, and a faulting backlog step reports in the tick output, not by mail.
- **Stow.** A stowed fleet fires nothing (gate). A stow leaves spacing and backoff as they were, so after resume the alarm fires at the next tick only if its spacing has passed.
- **Hourly wake budget.** Counted for hosted ships through the `wakes` record row, and for headless ships through `consider`'s own wake. A spent budget skips the fire without touching backoff.
- **Route outages.** A blocked own route skips the alarm without touching backoff; the next tick after the route clears may fire.
- **Ready work and mail.** Both win: the alarm only fires when neither existing wake would. A backlog wake that enqueues a task leads, after the session goes idle, to `work.poll` offering it in the same session, or `scheduling::tick` launching it headless.
- **Batching.** A headless notice waits for `batch_secs` like any mail, and is grouped with other mail that arrives meanwhile.

## Non-hosted and remote ships

- **Headless ships** (no `host` in the profile): the gates use "unattended" for quiet, and `consider` wakes a headless first mate for the notice under the duty task, focus `thread:backlog-<S>`.
- **Hosted ships without a pane** get no alarm until `consider` has relaunched the pane (review item 6).
- **Remote ships** are skipped: `location = "here"` is a gate, and scheduling already requires work to be scheduled in its owning home (`delivery::profile`). The alarm for a ship on another machine runs in that machine's dispatcher. A machine on an older release simply has no alarm; nothing is sent across machines.

## Failure modes and how the bounds contain them

- **A first mate that wakes and does nothing.** Each unproductive evaluation doubles the spacing to 8 hours, and an 8-hour night costs it three wakes. `quiet` counts them, so bearings and `work.status` show "backing off, 3 unproductive", and the audit names the tasks. No escalation to the captain: the morning check shows it.
- **A ship with 50 open drafts.** The notice lists the first 10 by rank and gives the count; only those 10 are measured for "productive", and the hash covers the rest. One handled draft is productive and resets the spacing to an hour, so a diligent first mate works through the backlog at up to 10 tasks an hour, and the hourly wake budget still applies. An untouched backlog backs off as above. The notice stays under 2 KB whatever the count, and the candidate query is bounded (`LIMIT 100` like `snapshot`).
- **A faulted task that keeps faulting.** Faults stay candidates (review Q1). A re-enqueue bumps the revision, which counts as productive once; a fault left alone is unproductive and backs off.
- **A wake that fails to start** (headless launch error, route refusal at launch): `consider` records a failed wake and backs off as today. The alarm's own row was written, so it does not fire again before its spacing, and the notice still pending holds off the next one.
- **A notice that is never read** (the pane cannot start its harness): `host::failed` already backs off the launch, and the notice stays in the mailbox. The alarm adds at most one notice per spacing, and an unread one is unproductive, so notices back off to one every 8 hours.
- **Crash mid-fire.** The fire is one transaction; a replay reuses the mail id.
- **A task that flaps** between candidate and not (enqueued, faulted, reopened): each change counts as the backlog changing, which resets the backoff but never the hourly spacing.
- **A lingering `orphaned` job** anywhere on the ship holds the alarm off indefinitely (it is live by the scheduler's own definition). The review flagged it as something to watch, not a blocker; the tick reports `something live` for it.
- **Clock skew or a long dispatcher outage.** All comparisons are `now >= next_ms`; an outage only delays the next alarm.

## Deferred: `auto_enqueue_ranked`

A ship-level setting under which the dispatcher enqueues ranked, unscheduled, open tasks without waking the first mate. Deferred (the review agrees).

`task.enqueue` needs a written assignment: `brief`, or the `assignment:<id>` note (`scheduling::commands::enqueue` refuses "prepare an assignment or supply brief before enqueueing"). So the setting could only apply to ranked tasks that already have an assignment note. The backlog wake reaches the same result with the owner's judgement at a cost of one wake an hour, and an assignment note drafted for later is not a decision to run it now. `docs/scheduling.md` keeps enqueueing explicit precisely so that writing an assignment and starting a worker do not race. If wanted later: a profile flag `auto_enqueue_ranked` (default false), the dispatcher enqueuing as the operator with `auto: true` in the `task.enqueue` audit row, at most one task per ship per tick, and only tasks whose assignment note is older than `backlog_alarm_secs`.

## Changes by file

- `src/scheduling/backlog.rs` (new): candidate predicate, gates, fire, evaluation, `backlog_alarms` reads and writes, the notice text, the `work.status` and bearings views.
- `src/scheduling/mod.rs`: `mod backlog;`, `backlog_alarms` in `init`, `pub(crate) use backlog::tick as backlog_tick`.
- `src/activation.rs`: call the backlog tick in `tick_with_delivery_budget`; `backlog_alarm_secs` in `SETTINGS`; `own_route_blocked` made `pub(crate)`; `consider`'s last-wake lookup skips `reason = 'backlog'`.
- `src/host/sleep.rs`: `alarm` returns `"backlog"` for backlog notices; the pane's asleep line says a backlog check may wake it.
- `src/mail.rs`: `record_crew` takes optional `task_id` and `correlation`.
- `src/context.rs`: `idle_tasks` counts parked jobs and crew parks as live; the bearings line.
- `work.status` in `src/scheduling/commands.rs`: the `backlog_alarm` block.
- `docs/scheduling.md`: a short section on the alarm and how a first mate leaves a draft on purpose (`task.update` blocked with a reason).

## Tests

All in isolated homes (`HOME`, `XDG_CONFIG_HOME`, `PRIVATEER_FLEET`) with the existing helpers; time is moved by backdating the `backlog_alarms` row, as `rank_and_shared_wake_budget_apply_to_automatic_work` backdates `wakes`.

- **(a) Repeated defer and resume stays offered.** Already covered by `a_taken_up_offer_that_defers_on_a_child_is_never_counted_as_an_attempt` in `tests/scheduling.rs` (PR 228); the backlog tick does not change it (the parent is `waiting` or `offered`, never a candidate).
- **(b) Woken once, not again within the hour.** `tests/hosted.rs`: a sleeping hosted pane with an open, unscheduled task gets one backlog notice (correlation `backlog-infra`) once its row's hour has passed; the harness is woken with `--continue`, `host.woke` records reason `backlog`, one `wakes` row with reason `backlog`. Asleep again with the task untouched: no second notice within the hour. The headless twin in `tests/scheduling.rs` asserts `consider` starts one wake on focus `thread:backlog-infra` and the next tick starts none.
- **(c) The exact fire/evaluate sequence.** `tests/scheduling.rs`, headless, a wake that changes nothing: fires at 0, 2, 6 and 14 h and at no tick between, with `backoff_secs` 3600, 7200, 14400, 28800 and `quiet` counting, across a `Store` reopen.
- **(d) Waits and blocks do not trigger.** A deferred task with a time wait, an unscheduled `blocked` task, a handed-off task, the duty task, a `running` task with an open request it sent, and a `running` task with a parked crew: no notice, report `no candidates`. Then a new open task: one notice listing only it.
- **(e) A stowed ship is not woken.** Report `stowed`, row untouched; after resume, one notice.
- **15 drafts.** One handled listed task resets the backoff; an untouched backlog keeps backing off; the notice lists 10 and counts 15.
- **Upgrade.** A ship with no row is not fired on its first tick, and is one interval later.
