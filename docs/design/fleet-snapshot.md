# Design: `fleet.snapshot`, one JSON of the fleet across machines

Status: revised after the design review (round 2) and implemented in full (round 3 added per-machine `routes` through the shared `crew_routes::states`, the `jobs_live` and `tasks_owner` indexes, and `park` text on a parked job's `wait`; round 4, from the code review, returns needs in full under `since`, reads answered needs once per home, orders stored sections by receipt rather than by either clock, and keeps a carried section best effort). Task `privateer-fleet-snapshot` (data side of issue 94, for quarterdeck's fleet map, its PR 7). Read with [cross-machine fleet](cross-machine-fleet.md) (links, the gateway), [runtime needs](../runtime-needs.md) and [architecture](../architecture.md#linked-machines). Names marked **new** were added by this work; every other name was checked in the tree at b95363b.

## Problem

The captain's portal wants one live picture: every machine, every ship on it, what each ship is doing now, and what waits on the captain. Today it has to stitch that from per-home commands, and some of it cannot be had at all:

- `fleet.list` and `machine.list` are directories. `ship.bearings` and `job.list` are per ship, per home, in text or raw records.
- The dashboard (`src/dashboard/fleet.rs`, `attach_linked`) shows ships on linked machines from the cached directory only: "nothing is asked of the other machine and no state of it is read".
- No "doing now" line exists anywhere. A crew's last `crew.report` progress is mail to its first mate; nothing joins it to the job.
- There is no change cursor, so a portal that polls cannot tell whether anything moved.

## Model

`fleet.snapshot {since?}` (**new**), operator-only, read-only. It is built from what the runtime already reads for bearings, `runtime.needs` and `ship.profile`; it adds one derived field per job (`doing`), one per ship (`presence`), and one cache table for change times and last-known state.

The answering machine reads its own homes (`crate::captain::fleet`, as `captain.feed` and `ship.profile` do), then asks each machine it reaches for that machine's own section over the gateway, and merges.

### JSON shape

```json
{
  "schema_version": 1,
  "generated_ms": 1792330000000,
  "machine": "laptop",
  "machines": [{
    "name": "laptop", "link": "here", "reachable": true, "error": null, "via": "direct",
    "as_of_ms": 1792330000000, "changed_ms": 1792329950000, "unchanged": false,
    "desk": true, "stowed": false, "truncated": false, "unknown": [],
    "routes": [{
      "name": "opus", "route": "claude/anthropic/claude-opus-5-5", "cost": "paid", "tier": 1,
      "state": "backing_off", "until_ms": 1792330300000, "error": "429 rate limited",
      "allowance": {"shape": "5h window, weekly Mon 00:00 UTC", "window_ends_ms": null,
                    "weekly_ends_ms": null, "window_used": 0.42, "week_used": 0.18,
                    "last_used_ms": 1792320000000}
    }]
  }],
  "ships": [{
    "name": "privateer", "machine": "laptop",
    "liaison": false, "harness": "claude", "hosted": true,
    "presence": "asleep", "presence_since_ms": 1792328000000,
    "unread": 2, "open_requests": 1,
    "open_tasks": [{"id": "privateer-fleet-snapshot", "title": "fleet.snapshot (operator)…",
                    "status": "running", "handoff_to": null}],
    "open_tasks_total": 51, "stale_tasks": 3,
    "jobs": [{
      "id": "crew-…", "task_id": "privateer-fleet-snapshot", "task_title": "fleet.snapshot …",
      "kind": "crew", "state": "running", "heavy": false,
      "created_ms": 1792323864223, "spawned_ms": 1792323864900, "started_ms": 1792323864900,
      "model_route": "claude/anthropic/claude-opus-5-5",
      "doing": {"text": "design note drafted; mapping tests", "at_ms": 1792329000000,
                "report_id": "crew-…-…"},
      "wait": null
    }],
    "jobs_total": 1
  }],
  "needs": [{
    "machine": "laptop", "ship": "quarterdeck", "id": "<origin>:mail:95173931…",
    "source": "mail", "source_id": "95173931…", "mail_id": "95173931…", "mail_kind": "request",
    "kind": "decision", "task_id": "…", "urgency": "normal", "deadline_ms": null,
    "asked_ms": 1792240000000, "question": "…", "choices": [], "version": "…",
    "as_of_ms": null, "actions": ["mail.send"]
  }]
}
```

Arrays are in name order (machines, ships, routes, with unnamed ledger holds after the named routes in key order), jobs newest first, needs as `runtime.needs` orders them. Every time is an absolute `_ms`; the snapshot carries no durations (no `age_ms`, `waited_ms`, `observed_ms`), so it is identical while nothing changes and the portal derives ages from `generated_ms`.

**Rule: no value inside a machine's section is computed from the current time.** The only way the passage of time changes a section is a state crossing an instant already stored in a record: a presence lease expiring, a route backoff or cap ending, an allowance window renewing. Those are real changes. Durations, "for N minutes" text and read times stay out of the section (`behind[].unhealthy` is a boolean, ships carry no `as_of_ms`; the machine row's `as_of_ms` is outside the hashed part). A test moves time forward across a quiet interval and checks the digest and `changed_ms` do not move.

### machines[]

- `link`: `here` (the answering machine), `reaches` (this machine linked it and can call its gateway, registry role `collects`), `reached_by` (it linked this one; role `collected`; a link is one-way, so nothing here can ask it).
- `reachable`: the section shown was read live by this call or within the reuse window below. `as_of_ms` is when the section shown was read, always set: `generated_ms` for this machine, the remote's own `generated_ms` for a reached one (live, reused or cached), on that machine's clock. Whether a stored section is young enough to reuse or fresh enough to show reachable is judged by `received_ms` (**new** column), when this registry recorded it on its own clock, never by the remote's `as_of_ms`: a remote clock an hour ahead would otherwise look fresh for an hour after it stopped answering.
- `via`: where the section came from: `direct` (read here, or asked over the gateway in this call), `reused` (a gateway read younger than `REUSE_MS`, 5 s, not asked again), `collect` (carried on the reaching machine's `fleet.collect`, see below), `cache` (the last section stored, the machine not answering), or `directory` (never read; ships from the directory only).
- `error`: why a machine this one reaches did not answer (timeout, refusal, a gateway too old to serve `fleet.snapshot`), bounded to 500 chars.
- `desk`: whether the captain's desk is on that machine (`crate::machines` `desk`, `holds_desk`).
- `stowed`: from `crate::stow::state` (`active` non-empty); null when the ledger cannot be read, never false.
- `unknown`: parts that could not be read, such as `"needs:apps"` when one home's `runtime.needs` came back unavailable, `"home:apps"`, or `"answered:apps"` on this machine's row when that home could not be checked for answers to other machines' needs (below). A part listed here is not an all-clear, the same rule `runtime.needs` keeps.
- `truncated`: a cap below was hit.

### routes (per machine)

Routes are a machine's: the captain's route file (`crate::crew_routes::load`) and the backoff and usage rows in that machine's host ledger (`model_routes`, `route_usage`, read through `crate::jobs::host_ledger`). One entry per route in the policy's `routes` map, plus any route the ledger holds backed off that the policy does not name (`name` null). `state`:

- `capped`: blocked (`crate::routes::blocked`) with `route_usage.capped_until_ms` in the future; `until_ms` is the block's end.
- `backing_off`: blocked without a cap; `until_ms` is `model_routes.until_ms`; `error` is its `last_error`, bounded.
- `idle`: not blocked, and its declared allowance has gone unused since it last renewed (the rule bearings print as "Idle allowances", `idle_line`).
- `available`: otherwise.

`allowance` is null for a route with no declared allowance; otherwise `shape` (as bearings describe it), the usage row's `window_ends_ms`, `weekly_ends_ms`, `window_used`, `week_used` (shares, 0 to 1) and `last_used_ms`. `tier` is the effective tier (1 when the route states none, as ranking reads it); an unnamed hold has null `cost`, `tier` and `allowance`, and its `route` is the ledger key held, which may be an account key (`claude/anthropic/*`) that also holds named routes. `error` is the hold's `model_routes.last_error`, the classified failure `routes::refusal` shows, never raw output, at most 500 chars.

The classification is `crate::crew_routes::states(policy, ledger, now)`, which bearings' `summary` also calls, so the two never disagree. It reads only stored rows and compares them with `now`: a hold's `until_ms`, a cap's `capped_until_ms`, a window's `window_ends_ms` and a daily allowance's renewal minute. So a route's entry changes with time only when `now` crosses one of those instants, which the rule above counts as a real change; routes are inside the hashed section. A lib test reads the states at two times with no instant between them and gets the same answer, then one past several instants and gets the new states. The machine's routes file is the registry's (`crew_routes::load`, the same on every home here), and the ledger is read once per machine, read-only with a 100 ms busy timeout; a routes file or ledger that cannot be read leaves `routes` empty and names `"routes"` in `unknown`. No route is changed or probed.

### ships[]

One per active fleet member on its machine (`crate::fleet::members`), in its own home.

- `presence` (answer to question 2), one of (none of them is the `attended` boolean `crate::presence::presence` returns, which is true for hosted sessions too):
  - `hosted`: a Herdr-hosted pane holds the presence lease (holder starts with `crate::presence::HOSTED`) and its harness runs.
  - `asleep`: that pane holds the lease with its harness stopped (`crate::host::sleep::sleep` or `asleep_at_start`).
  - `session`: a live session that is not hosted holds the lease (the captain's own terminal, an attended adapter).
  - `absent`: no live lease; mail starts a headless turn (`wakes`).
  - `unavailable`: the home could not be read within its budget; the ship has no other live fields.
  - `unknown`: the machine was not reached and nothing was ever cached for it (ships listed from the directory only).

  Asleep is not readable today: `sleep` moves the lease to a fresh `host:` holder, the same shape as an awake hosted session, and records `host.slept` only as an audit event. Two columns (**new**) tie it to the holder: `presence.asleep_holder` and `presence.asleep_since_ms`, written with the holder change in `sleep` and by `asleep_at_start` for the holder it starts asleep. A ship is asleep only while `asleep_holder` equals the live `holder`, so any holder change (`attend` by a new session, `adopt`, `leave`, expiry) ends it without anyone clearing it; the pane clears both when it takes the harness back (where `woke` is called), since that keeps the same holder. `presence_since_ms` is `asleep_since_ms` when asleep, otherwise the lease's `started_ms`.
- `hosted`: configured for a hosted pane (`crate::host::hosted` on the first-mate record), independent of presence, as `ship.profile` reports it.
- `unread`, `open_requests`: the counts the dashboard reads (`ShipState.unread`, `ShipState.requests`).
- `open_tasks`: tasks owned by the ship with status `open`, `running` or `blocked`, newest rowid first, at most 20; `open_tasks_total` is the full count. `stale_tasks` counts the open tasks with no update for the ship's `first_mate.stale_task_days` (see [stale open tasks](../scheduling.md#stale-open-tasks)), null when the home cannot say.
- `jobs` (answer to the jobs-state question): **live jobs only**, each with its `state`. A job is live when its status is not terminal (`succeeded`, `failed`, `cancelled`, `timed_out`, `resumed`; the set `terminal` in `src/jobs.rs` keeps). `state` is the stored status verbatim: `starting`, `queued`, `launching`, `running`, `parked` or `orphaned`. There is no separate `waiting_capacity`: `queued` *is* the host capacity queue (`wait_in_queue`), and `wait` says what it waits for. At most 20 per ship, newest first; `jobs_total` is the full count of live jobs.

### jobs[]

- `kind`: `first_mate` (a wake's job, `wakes.job_id`), `errand` (the job of an `errands` row), `crew` (`action` spawn with `agent_actor` set), or `command`.
- `heavy`, `created_ms`, `model_route` from the job record. No argv, cwd, environment or log is returned.
- `spawned_ms` (**new** job field, question 3): set by the supervisor in the same `alter` that records the payload and moves the job to `running`. Null while queued or starting, and for jobs recorded before the upgrade.
- `started_ms`: when the job began holding host capacity, the reservation's `created_ms` (`Holder.started_ms`); null while queued, and for light work, which takes no reservation.
- `doing` (the "doing now" line): the job's most recent `crew.report` of kind `progress`. Reports are mail from `crew` with id `crew-<job>-<incarnation>-<suffix>` and `payload.job_id`, so the read is a range on the unique `mail.id` index (`id >= 'crew-<job>-' AND id < 'crew-<job>.'`) filtered on `payload.job_id` and `payload.report = 'progress'`, newest `seq` first, `LIMIT 1`. `text` is the report body after the runtime's lead line ("Crew job … reports progress:"), first line, at most 200 chars. A parked crew's `doing` is its park's `next` line (`crew_parks.next`) with `at_ms` the park's `updated_ms`. Null when the crew never reported.
- `wait`: null unless the job waits:
  - `queued`: `{"for": "capacity" | "disk", "since_ms": created_ms, "behind": [{"job_id", "ship", "unhealthy": bool}]}`. The holders are read **once per machine** (not once per ship as `capacity_waits` does), through a `capacity_holders` variant with a 100 ms busy timeout; a ledger busy past that leaves `behind` empty and names `"ledger"` in `unknown`. `unhealthy` is a boolean: the holder's text says "for N minutes" and would move the digest. `disk` by the same evidence rule `runtime.needs` uses (`reason` names "disk available").
  - `parked`: `{"for": "job", "job_id": <child>, "park": "park N of M"}` from `crew_parks`: N the parks the crew's lineage has made, M its charter's `crew_max_parks` (6 by default), the same text `crew::park_count` gives bearings.

### needs[]

Each home's typed `runtime.needs` items (`crate::needs::query`, operator, its own budget), with `machine` added and `observed_ms`, `waited_ms` dropped.

**Transactions do not nest.** `needs::query` opens its own deferred transaction and installs its own progress handler and busy timeout on the home's connection. The snapshot therefore reads each home in two steps on the same connection: `needs::query` first, which finishes and restores the connection, then the snapshot's own deferred read transaction under its own budget. The two reads are separate views of the home; a change between them shows in the next call.

- **Question 1, each need its machine:** `machine` is the machine of the asking ship, from the asking ship's directory entry (`sender`: a ship hosted here is this machine; a ship in `machine_ships` is that machine), not the machine of the home holding the copy. A desk copy of a request from a machine never read or cached is still pinned to the right machine. A sender in no directory keeps the holding machine. A request is held twice when the captain's desk collected it: the original in the ship's home there, a desk copy in the captain's home here. The merge keeps one item per `mail_id`, preferring a live read of the asking machine.
- **Answered needs are not shown from elsewhere.** A remote, reused, carried or cached need whose `mail_id` has a copy in any home here that is no longer `open` is dropped: the captain answered it here and the other machine has not heard yet. The check is one read per home for every remote `mail_id` at once (`SELECT id FROM mail WHERE id IN (…) AND state IS NOT 'open'`, by the `mail.id` key, in chunks of 500), under that home's observation budget and the 3 s local budget for the whole pass. A home not read in it is named `answered:<ship>` in this machine's `unknown`, and its copies count as open: the need stays shown.
- **Cached needs** (from a `cache` or `collect` section) carry that section's `as_of_ms` and `actions: []`: the portal shows them, but must not offer to answer from a read that may be stale. Live needs carry `as_of_ms: null` and `runtime.needs`' own actions.
- **Question 5, each need its mail_id:** every need sourced from mail (`source: "mail"`, kinds `decision` and `approval`) carries `mail_id`, `mail_kind`, `urgency`, `deadline_ms` and `asked_ms` (the merged fields). Blockers (`source` `job`, `schedule`, `workspace`) are not mail and carry `mail_id: null`; they are named by `source` and `source_id` (a job id or task id).

### changed_ms (question 3)

`changed_ms` for a machine is **the `generated_ms` of the first snapshot that machine computed whose section had its current content**. The section is the machine's own row without `reachable`, `error`, `as_of_ms`, `changed_ms`, `unchanged`, plus its ships and its needs. Each machine computes it about itself, so every caller reads the same value:

- The section is serialized in its stable order and hashed (`crate::core::hash`). A registry table `fleet_snapshots` (**new**) keeps, per machine, `digest`, `changed_ms`, `as_of_ms`, `via` and `body`.
- **Monotone, ordered by nothing on a clock.** The row is read and written in one write transaction (`BEGIN IMMEDIATE`), and the read just made is both stored and returned. If the digest differs, `changed_ms = max(generated_ms, stored changed_ms + 1)`, so it never goes back or repeats even when the clock steps back; if it is equal, `changed_ms` is kept. Comparing `generated_ms` with the stored `as_of_ms` would freeze the section for as long as a clock step back lasted, so no read is refused by time. Two calls racing may store the older of their two reads; it is a change with its own `changed_ms`, so a `since` cursor still matches the content it names, and the next read replaces it.
- A route's backoff ending changes its `state`, so time alone can change the digest; that is a real change.
- Changes are seen only when someone calls: two changes between calls read as one, and a change undone before the next call reads as none. `changed_ms <= as_of_ms` always. The first call after upgrade sets `changed_ms` to its own time.

### since

A per-machine token, not a time: `since` is an object `{"<machine>": <changed_ms as last returned>}`. A machine whose `changed_ms` **equals** its entry is returned as its row with `"unchanged": true` and none of its routes or ships; the portal keeps what it had. **Needs are always returned in full**, whatever `since` says: a need answered here, or a machine going from live to cached, changes which needs show and how (`as_of_ms`, `actions`) without moving that machine's `changed_ms`, and a need's `machine` is the asking ship's, not the section's, so the portal could not tell which to keep. Any other value, a machine missing from `since`, or a machine new to the portal returns the full section. Equality, never `<=`: each machine's `changed_ms` is on that machine's own clock, and a remote clock behind the portal's, or a step back, would otherwise hide a real change until the next one. Between machines, the asking machine passes the `changed_ms` of the section it has stored for the other as `since: n`; an equal one comes back without its section and only re-dates the stored one, so an unchanged remote section does not cross the link. No deltas within a section.

### Linked machines over the gateway

- `serve_request` (`src/machines.rs`) gains a `"fleet.snapshot"` arm. Every gateway command but delivery already requires the operator (`caller.require_operator`), and `call` already sends as `Caller::Operator`: the linking operator's SSH access is the authority, as for `fleet.link` and `stow.*`. No new write power: the arm reads homes and writes only the answering machine's own `fleet_snapshots` row about itself.
- A gateway answers **only its own section** (`machines` holding one `here` entry, its ships, its needs). It never asks its own links, so there is no recursion and no A→B→A loop.
- The answering machine calls each `reaches` machine with a per-machine timeout `SNAPSHOT_TIMEOUT` (**new**, 10 s, as `STOW_TIMEOUT`), one thread per machine (`std::thread::scope`), so the whole call waits at most one timeout beyond the local read, however many machines are down.
- **Polling cost.** Each gateway call is one SSH connection. A stored gateway read younger than `REUSE_MS` (**new**, 5 s) is reused instead of asked again, labelled `via: "reused"` with its own `as_of_ms`; a portal polling every second opens at most one connection per machine per 5 s. Quarterdeck should state its poll interval in its PR.
- A reply is stored in this registry's `fleet_snapshots` row for that machine (`body`, `as_of_ms`, the remote's `changed_ms`) and merged with `reachable: true`. The remote's own `link: "here"` is rewritten to `reaches`.

### Unreachable machines (question 4)

- A `reaches` machine that does not answer keeps its last-known section from `fleet_snapshots`: its ships and needs as last read, `reachable: false`, `error`, `as_of_ms` the time of that read, and its stored `changed_ms`. The machine row's `as_of_ms` dates every ship and need in it; ships carry none of their own.
- Never reached since upgrade: its ships come from the cached directory (`crate::machines::remote_ships`, the dashboard's source) with `presence: "unknown"` and no live fields; `as_of_ms` is the directory's `refreshed_ms`.
- A `reached_by` machine is never asked (links are one-way). It carries its own section to this machine on the `fleet.collect` call it already makes each contact (`CONTACT_MS`, 30 s), as stows ride along today: the collecting machine reads its local section (the same reader, its own `fleet_snapshots` row updated), and sends it as `snapshot` in the `fleet.collect` arguments when it is at most `CARRY_BYTES` (**new**, 256 KiB, inside the call's request bound); larger, it sends `{"truncated": true, "as_of_ms", "changed_ms"}` without ships. The reached machine stores it in its `fleet_snapshots` row for that machine with `via: "collect"` and the sender's `as_of_ms`, rewriting `link` to `reached_by`. It is shown `reachable: true` while younger than `2 * CONTACT_MS`, otherwise `reachable: false` with its `as_of_ms`; its needs are cached needs (above). A machine that has never sent one is listed with `reachable: false`, `error: "links one way; nothing carried from <name> yet"` and its directory ships. A peer too old to carry it sends no field, read as nothing new.
- **Last received wins.** A stored row for another machine is replaced by each section received for it, direct or carried, and the section just received is what the call shows. A link is one way, so one machine's row has one source; comparing the remote's `as_of_ms` would hold an old section in place after that machine's clock stepped back. An unchanged reply (`since` matched) only re-dates the stored row, and only while it still holds that `changed_ms`.
- **Best effort.** The `fleet.collect` arm keeps a carried section if it can; an error storing it never fails the collect or its mail. A carried section names the reaching machine by the `machine` the collect gives, which must be linked to this one (`reaching_here`); any other name is refused with the collect, and nothing is kept.
- The reaching machine reads its own section once per contact pass (the dispatcher's `tick`, or `machine.collect`), on the first collect that carries it, and carries the same one to every machine it reaches in that pass.
- Bounds: one row per linked machine, `body` at most 768 KiB (under the machine peer's 1 MiB `max_response_bytes` default), deleted with the link in `machine.remove`. A row whose body fails to parse is dropped and the machine reads as never reached.

### Cost per call

Bounded per home, per machine and in wall time; no unbounded scan:

- **Homes:** every active member on the machine, each read in one deferred read transaction under an observation budget like `runtime.needs`' (`ObservationBudget`, made `pub(crate)`): 25 ms busy wait, 250 ms of statement work, after that home's `runtime.needs` read under its own budget (so up to about 500 ms per home in the worst case). A home over budget is `presence: "unavailable"` and is named in `unknown`; it does not fail the call. The machine's whole local read stops at 3 s (**new** `LOCAL_BUDGET_MS`); homes not reached by then are listed `unavailable`. **Known limit:** homes are read in name order, so on a slow machine the same last homes are the ones always cut off; a later round may rotate the start. They are named in `unknown`, never shown as empty.
- **Jobs:** live jobs are found by status. The `jobs` table has no status column or index, and `job.list` reads every record; a partial expression index `jobs_live` on `json_extract(record,'$.created_ms')` over non-terminal statuses (`jobs::TERMINAL_SQL`, which the query repeats word for word so SQLite can use it) reads only live jobs, newest first. A lib test checks both job reads and the task read use their index (`EXPLAIN QUERY PLAN`). Indexes are created when a home is next opened for writing; a home not yet upgraded is read by scanning, as before. At most 20 jobs per ship are expanded; each `doing` read is one index range with `LIMIT 1`.
- **Tasks:** `SELECT … FROM tasks WHERE owner=? AND status IN ('open','running','blocked') LIMIT 21` and a `COUNT`. The index `tasks_owner` on `(owner, status)` serves both.
- **Needs:** `runtime.needs` with `limit` 40 per home; more is `truncated`. Its blocker read opens the host ledger through `jobs::stale_reservations` with that function's own 5 s busy timeout, once per home that has a queued job, outside both observation budgets: a ledger write lock held that long can hold one home's needs read up to 5 s past `LOCAL_BUDGET`. Not changed here; `runtime.needs` owns that wait.
- **Answered needs:** one read per home here for all remote `mail_id`s together, under the local budget (above).
- **Job kinds:** a wake's job by the `wake_job` index (**new**, `wakes(job_id)`), an errand's by the `errands` key.
- **Ledger:** one read-only open of the host ledger per machine with a 100 ms busy timeout, for the holders and the route rows, read once and shared by every home; process identity checks are bounded by the number of reservations. Never `capacity_waits` per ship (it reopens the ledger with a 5 s busy timeout each time).
- **Totals:** at most 200 ships per machine (`ROW_LIMIT`, as the dashboard caps), 768 KiB per machine section; past either, `truncated` and open tasks are dropped first, then jobs past 5 per ship.

### Secrets

No value from configuration, environment, argv, credentials or logs is read into the snapshot. Fields come from typed records only: names, statuses, times, counts, task titles, route identities (harness/provider/model, as bearings show them), bounded error text the ledger already records, need text `runtime.needs` already returns, and crew progress text the captain already reads in mail and `captain.feed`. No first-mate options are returned at all, so no masking is needed; if a later round adds them, they go through `ship.profile`'s `masked` (`crate::block_ask::sensitive_name`), never a second rule. A test checks a home whose first-mate options and a job's argv hold a canary secret, and that the canary appears nowhere in the output.

## Changes by file

- `src/fleet.rs` (or a new `src/snapshot.rs`): `fleet.snapshot` command, the local section reader, the merge, the digest and `fleet_snapshots` upsert; added to `OPERATOR_COMMANDS` in `src/lib.rs`.
- `src/machines.rs`: the `"fleet.snapshot"` gateway arm; parallel calls with `SNAPSHOT_TIMEOUT`; `fleet_snapshots` created in `init` and its row deleted in `remove`.
- `src/crew_routes.rs`: `states` factored out of `summary`; `summary` calls it.
- `src/presence.rs`, `src/host/sleep.rs`: `asleep_holder` and `asleep_since_ms` columns, set in `sleep` and `asleep_at_start`, cleared on wake; a `presence::state` (**new**) returning the presence value.
- `src/jobs.rs`: `spawned_ms` on the job record, set with `running`; a `capacity_holders` variant taking a busy timeout.
- `src/machines.rs` `collect` and the `fleet.collect` arm: the carried `snapshot`.
- `src/needs.rs`: `ObservationBudget` `pub(crate)`; no change to `runtime.needs` output.
- `src/jobs.rs`: `jobs_live` index; a `live_jobs(store, ship, limit)` (**new**) reader returning the job fields above.
- `src/store.rs`: `tasks_owner` index.
- `docs/architecture.md`: one paragraph after `ship.profile`, with the shape.

## Test plan

Isolated homes and registries (HOME, PRIVATEER_FLEET, PRIVATEER_RESOURCE_HOME), real subprocess transport through the fake ssh.

`tests/fleet.rs` (one machine, its `Machine` helper):

1. Two ships, one with a running crew that sent two `crew.report` progress reports: the job is listed `running` with the second report as `doing`; a finished job is not listed.
2. A heavy job queued behind another: `state: "queued"`, `wait.for: "capacity"` naming the holder; a parked crew shows its `next` and child.
3. Presence: a hosted lease reads `hosted`; after `sleep` it reads `asleep` with `presence_since_ms`; after waking `hosted` again; no lease reads `absent`.
4. A captain request is a need with `mail_id`, `machine` and `asked_ms`; a capacity blocker has `mail_id: null` and `source_id`.
5. `changed_ms` holds across two calls with nothing changed, even as time passes (a lib test builds the section at `now` and `now + 1 h`; the integration test waits across a second), moves on a new task, and survives a restart of the reading process; `since: {"<machine>": changed_ms}` returns the machine `unchanged` with no ships, and any other value returns it full. A stored `changed_ms` ahead of the clock still moves forward (`+1`) on a change.
6. A route backed off in the ledger reads `backing_off` with `until_ms`, a capped one `capped`; bearings and the snapshot agree on both.
7. The canary secret appears nowhere; an agent caller is refused.
8. A home whose database is locked past the budget reads `unavailable` and is in `unknown`, and the others are complete.

`tests/machines.rs` (two machines: `fixture`, `link`, `remote_ssh`, `remote_ssh_with`, `connections`):

9. Laptop links mac; the snapshot on laptop lists both machines, mac's ships with live jobs and needs, `link: "reaches"`, `reachable: true`; one gateway connection per call.
10. A request from a mac ship collected to the laptop desk is one need with `machine: "mac"`.
11. A second call within `REUSE_MS` opens no new connection and shows mac `via: "reused"` with the first call's `as_of_ms`.
12. Mac unreachable (`remote_ssh_with` an answer that fails, or one that sleeps past the timeout): mac keeps its last-known ships with `reachable: false`, `via: "cache"`, `as_of_ms` of the earlier call, the same `changed_ms`, its needs with `as_of_ms` and no actions, a need the desk here already answered is gone, and the call returns within the timeout plus the local budget.
13. On mac, before any collect, the snapshot lists the laptop as `reached_by`, `reachable: false`, directory ships `unknown`, and opens no connection. After the laptop's dispatcher pass (`fleet.collect`), mac lists the laptop's ships and live jobs with `via: "collect"`.
14. A gateway that does not know `fleet.snapshot` (an older binary, faked by `remote_ssh_with`) reads as unreachable with that error, not as an empty machine.
15. `machine remove` drops the cached row.

Tests prove behaviour through commands and durable state, not snapshots of layout.

## Decisions (round 2)

1. **Machines that reach this one, and which machine answers for the portal** (questions 1 and 5, a first-mate decision): the captain's desk is on the server, which the laptop reaches, so the server's portal must see the laptop's live state now. Each reaching machine carries its own section on the `fleet.collect` exchange it already makes (bounded, with its `as_of_ms`), and the server's `fleet.snapshot` shows the freshest of that and any direct read. No new connection, no captain decision.
2. **`asleep_since_ms`:** the column, tied to the holder (`asleep_holder`), as above.
3. **Job start time:** `spawned_ms` is recorded on the job when the payload spawns.
4. **`changed_ms` resolution:** a push or long-poll command (`fleet.watch`) is out of scope.
