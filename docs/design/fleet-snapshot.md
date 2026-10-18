# Design: `fleet.snapshot`, one JSON of the fleet across machines

Status: design only, for review. Task `privateer-fleet-snapshot` (data side of issue 94, for quarterdeck's fleet map, its PR 7). Read with [cross-machine fleet](cross-machine-fleet.md) (links, the gateway), [runtime needs](../runtime-needs.md) and [architecture](../architecture.md#linked-machines). Names marked **new** do not exist yet; every other name was checked in the tree at b95363b.

## Problem

The captain's portal wants one live picture: every machine, every ship on it, what each ship is doing now, and what waits on the captain. Today it has to stitch that from per-home commands, and some of it cannot be had at all:

- `fleet.list` and `machine.list` are directories. `ship.bearings` and `job.list` are per ship, per home, in text or raw records.
- The dashboard (`src/dashboard/fleet.rs`, `attach_linked`) shows ships on linked machines from the cached directory only: "nothing is asked of the other machine and no state of it is read".
- No "doing now" line exists anywhere. A crew's last `crew.report` progress is mail to its first mate; nothing joins it to the job.
- There is no change cursor, so a portal that polls cannot tell whether anything moved.

## Model

`fleet.snapshot {since_ms?}` (**new**), operator-only, read-only. It is built from what the runtime already reads for bearings, `runtime.needs` and `ship.profile`; it adds one derived field per job (`doing`), one per ship (`presence`), and one cache table for change times and last-known state.

The answering machine reads its own homes (`crate::captain::fleet`, as `captain.feed` and `ship.profile` do), then asks each machine it reaches for that machine's own section over the gateway, and merges.

### JSON shape

```json
{
  "schema_version": 1,
  "generated_ms": 1792330000000,
  "machine": "laptop",
  "machines": [{
    "name": "laptop", "link": "here", "reachable": true, "error": null,
    "as_of_ms": 1792330000000, "changed_ms": 1792329950000, "unchanged": false,
    "desk": true, "stowed": false, "truncated": false, "unknown": [],
    "routes": [{
      "name": "opus", "route": "claude/anthropic/claude-opus-5-5", "cost": "paid", "tier": 1,
      "state": "backing_off", "until_ms": 1792330300000, "error": "429 rate limited",
      "allowance": {"shape": "5h window, weekly Mon 00:00 UTC", "window_ends_ms": null,
                    "window_used": 0.42, "week_used": 0.18, "last_used_ms": 1792320000000}
    }]
  }],
  "ships": [{
    "name": "privateer", "machine": "laptop", "as_of_ms": 1792330000000,
    "liaison": false, "harness": "claude", "hosted": true,
    "presence": "asleep", "presence_since_ms": 1792328000000,
    "unread": 2, "open_requests": 1,
    "open_tasks": [{"id": "privateer-fleet-snapshot", "title": "fleet.snapshot (operator)…",
                    "status": "running", "handoff_to": null}],
    "open_tasks_total": 51,
    "jobs": [{
      "id": "crew-…", "task_id": "privateer-fleet-snapshot", "task_title": "fleet.snapshot …",
      "kind": "crew", "state": "running", "heavy": false,
      "created_ms": 1792323864223, "started_ms": 1792323864900,
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
    "actions": ["mail.send"]
  }]
}
```

Arrays are in name order (machines, ships, routes), jobs newest first, needs as `runtime.needs` orders them. Every time is an absolute `_ms`; the snapshot carries no durations (no `age_ms`, `waited_ms`, `observed_ms`), so it is identical while nothing changes and the portal derives ages from `generated_ms`.

### machines[]

- `link`: `here` (the answering machine), `reaches` (this machine linked it and can call its gateway, registry role `collects`), `reached_by` (it linked this one; role `collected`; a link is one-way, so nothing here can ask it).
- `reachable`: this call read the section live, from this machine or its gateway. `as_of_ms` is when the section shown was read: `generated_ms` when live, the cached read's time when not.
- `error`: why a machine this one reaches did not answer (timeout, refusal, a gateway too old to serve `fleet.snapshot`), bounded to 500 chars.
- `desk`: whether the captain's desk is on that machine (`crate::machines` `desk`, `holds_desk`).
- `stowed`: from `crate::stow::state` (`active` non-empty); null when the ledger cannot be read, never false.
- `unknown`: parts that could not be read, such as `"needs:apps"` when one home's `runtime.needs` came back unavailable, or `"home:apps"`. A part listed here is not an all-clear, the same rule `runtime.needs` keeps.
- `truncated`: a cap below was hit.

### routes (per machine)

Routes are a machine's: the captain's route file (`crate::crew_routes::load`) and the backoff and usage rows in that machine's host ledger (`model_routes`, `route_usage`, read through `crate::jobs::host_ledger`). One entry per route in the policy's `routes` map, plus any route the ledger holds backed off that the policy does not name (`name` null). `state`:

- `capped`: blocked (`crate::routes::blocked`) with `route_usage.capped_until_ms` in the future; `until_ms` is the block's end.
- `backing_off`: blocked without a cap; `until_ms` is `model_routes.until_ms`; `error` is its `last_error`, bounded.
- `idle`: not blocked, and its declared allowance has gone unused since it last renewed (the rule bearings print as "Idle allowances", `idle_line`).
- `available`: otherwise.

`allowance` is null for a route with no declared allowance. The classification now lives inline in `crate::crew_routes::summary`; it moves to a shared `crew_routes::states` (**new**) that both bearings and the snapshot call, so the two never disagree. No route is changed or probed.

### ships[]

One per active fleet member on its machine (`crate::fleet::members`), in its own home.

- `presence` (answer to question 2), one of:
  - `hosted`: a Herdr-hosted pane holds the presence lease (holder starts with `crate::presence::HOSTED`) and its harness runs.
  - `asleep`: that pane holds the lease with its harness stopped (`crate::host::sleep::sleep` or `asleep_at_start`).
  - `attended`: a live session that is not hosted holds the lease (the captain's own terminal, an attended adapter).
  - `absent`: no live lease; mail starts a headless turn (`wakes`).
  - `unavailable`: the home could not be read within its budget; the ship has no other live fields.
  - `unknown`: the machine was not reached and nothing was ever cached for it (ships listed from the directory only).

  Asleep is not readable today: `sleep` moves the lease to a fresh `host:` holder, the same shape as an awake hosted session, and records `host.slept` only as an audit event. A column `presence.asleep_since_ms` (**new**) is set in the same transaction as the holder change in `sleep` and in `asleep_at_start`, and cleared when the pane takes the harness back (where `woke` is called). `presence_since_ms` is that time, or the lease's `started_ms`.
- `hosted`: configured for a hosted pane (`crate::host::hosted` on the first-mate record), independent of presence, as `ship.profile` reports it.
- `unread`, `open_requests`: the counts the dashboard reads (`ShipState.unread`, `ShipState.requests`).
- `open_tasks`: tasks owned by the ship with status `open`, `running` or `blocked`, newest rowid first, at most 20; `open_tasks_total` is the full count.
- `jobs` (answer to the jobs-state question): **live jobs only**, each with its `state`. A job is live when its status is not terminal (`succeeded`, `failed`, `cancelled`, `timed_out`, `resumed`; the set `terminal` in `src/jobs.rs` keeps). `state` is the stored status verbatim: `starting`, `queued`, `launching`, `running`, `parked` or `orphaned`. There is no separate `waiting_capacity`: `queued` *is* the host capacity queue (`wait_in_queue`), and `wait` says what it waits for. At most 20 per ship, newest first; `jobs_total` is the full count of live jobs.

### jobs[]

- `kind`: `first_mate` (a wake's job, `wakes.job_id`), `crew` (`action` spawn with `agent_actor` set), or `command`.
- `heavy`, `created_ms`, `model_route` from the job record. No argv, cwd, environment or log is returned.
- `started_ms`: when the job began holding host capacity, the reservation's `created_ms` (`crate::jobs::capacity_holders`, `Holder.started_ms`); null while queued, and for light work, which takes no reservation. The record has no spawn time in ms (see open question 3).
- `doing` (the "doing now" line): the job's most recent `crew.report` of kind `progress`. Reports are mail from `crew` with id `crew-<job>-<incarnation>-<suffix>` and `payload.job_id`, so the read is a range on the unique `mail.id` index (`id >= 'crew-<job>-' AND id < 'crew-<job>.'`) filtered on `payload.job_id` and `payload.report = 'progress'`, newest `seq` first, `LIMIT 1`. `text` is the report body after the runtime's lead line ("Crew job … reports progress:"), first line, at most 200 chars. A parked crew's `doing` is its park's `next` line (`crew_parks.next`) with `at_ms` the park's `updated_ms`. Null when the crew never reported.
- `wait`: null unless the job waits:
  - `queued`: `{"for": "capacity" | "disk", "since_ms": created_ms, "behind": [{"job_id", "ship", "unhealthy"}]}` from `crate::jobs::capacity_waits` (the bearings source), `disk` by the same evidence rule `runtime.needs` uses (`reason` names "disk available").
  - `parked`: `{"for": "job", "job_id": <child>, "park": "park N of 6"}` from `crew_parks`.

### needs[]

Each home's typed `runtime.needs` items (`crate::needs::query`, operator, its own budget), with `machine` added and `observed_ms`, `waited_ms` dropped.

- **Question 1, each need its machine:** `machine` is the machine of the asking ship's home. A request from a ship on another machine is held twice when the captain's desk collected it: the original in the ship's home there, a desk copy in the captain's home here. The merge keeps one item per `mail_id`, the original's (the ship's own machine), so the map pins it to the ship that asked.
- **Question 5, each need its mail_id:** every need sourced from mail (`source: "mail"`, kinds `decision` and `approval`) carries `mail_id`, `mail_kind`, `urgency`, `deadline_ms` and `asked_ms` (the merged fields). Blockers (`source` `job`, `schedule`, `workspace`) are not mail and carry `mail_id: null`; they are named by `source` and `source_id` (a job id or task id).

### changed_ms (question 3)

`changed_ms` for a machine is **the `generated_ms` of the first snapshot that machine computed whose section had its current content**. The section is the machine's own row without `reachable`, `error`, `as_of_ms`, `changed_ms`, `unchanged`, plus its ships and its needs. Each machine computes it about itself, so every caller reads the same value:

- The section is serialized in its stable order and hashed (`crate::core::hash`). A registry table `fleet_snapshots` (**new**) keeps, per machine, `digest`, `changed_ms`, `as_of_ms` and `body`. If the digest differs from the stored one, `changed_ms = generated_ms`; otherwise it is kept. One upsert, in one write transaction on the registry.
- A route's backoff ending changes its `state`, so time alone can change the digest; that is a real change.
- Changes are seen only when someone calls: two changes between calls read as one, and a change undone before the next call reads as none. `changed_ms <= as_of_ms` always. The first call after upgrade sets `changed_ms` to its own time.

### since_ms

Accepted from the first round, because the digest makes it free. A machine whose `changed_ms <= since_ms` is returned as its row with `"unchanged": true` and none of its routes, ships or needs; the portal keeps what it had. `since_ms` is passed through the gateway, so an unchanged remote section does not cross the link either. No deltas within a section.

### Linked machines over the gateway

- `serve_request` (`src/machines.rs`) gains a `"fleet.snapshot"` arm. Every gateway command but delivery already requires the operator (`caller.require_operator`), and `call` already sends as `Caller::Operator`: the linking operator's SSH access is the authority, as for `fleet.link` and `stow.*`. No new write power: the arm reads homes and writes only the answering machine's own `fleet_snapshots` row about itself.
- A gateway answers **only its own section** (`machines` holding one `here` entry, its ships, its needs). It never asks its own links, so there is no recursion and no A→B→A loop.
- The answering machine calls each `reaches` machine with a per-machine timeout `SNAPSHOT_TIMEOUT` (**new**, 10 s, as `STOW_TIMEOUT`), one thread per machine (`std::thread::scope`), so the whole call waits at most one timeout beyond the local read, however many machines are down.
- A reply is stored in this registry's `fleet_snapshots` row for that machine (`body`, `as_of_ms`, the remote's `changed_ms`) and merged with `reachable: true`. The remote's own `link: "here"` is rewritten to `reaches`.

### Unreachable machines (question 4)

- A `reaches` machine that does not answer keeps its last-known section from `fleet_snapshots`: its ships and needs as last read, `reachable: false`, `error`, `as_of_ms` the time of that read, and its stored `changed_ms`. Each of its ships also carries `as_of_ms`.
- Never reached since upgrade: its ships come from the cached directory (`crate::machines::remote_ships`, the dashboard's source) with `presence: "unknown"` and no live fields; `as_of_ms` is the directory's `refreshed_ms`.
- A `reached_by` machine is never asked (links are one-way). Round 1 lists it with `reachable: false`, `error: "links one way; ask from <name>"` and its directory ships as above. Round 2 (open question 1) carries its section to this machine with each `fleet.collect` it makes, as stows ride along today.
- Bounds: one row per linked machine, `body` at most 768 KiB (under the machine peer's 1 MiB `max_response_bytes` default), deleted with the link in `machine.remove`. A row whose body fails to parse is dropped and the machine reads as never reached.

### Cost per call

Bounded per home, per machine and in wall time; no unbounded scan:

- **Homes:** every active member on the machine, each read in one deferred read transaction under an observation budget like `runtime.needs`' (`ObservationBudget`, made `pub(crate)`): 25 ms busy wait, 250 ms of statement work. A home over budget is `presence: "unavailable"` and is named in `unknown`; it does not fail the call. The machine's whole local read stops at 3 s (**new** `LOCAL_BUDGET_MS`); homes not reached by then are listed `unavailable`.
- **Jobs:** live jobs are found by status. The `jobs` table has no status column or index, and `job.list` reads every record; a partial expression index `jobs_live` (**new**) on `json_extract(record,'$.status')` for non-terminal statuses makes that a seek. At most 20 jobs per ship are expanded; each `doing` read is one index range with `LIMIT 1`.
- **Tasks:** `SELECT … FROM tasks WHERE owner=? AND status IN ('open','running','blocked') LIMIT 21` and a `COUNT`. The table has only its primary key; an index `tasks_owner` (**new**) on `(owner, status)`.
- **Needs:** `runtime.needs` with `limit` 40 per home; more is `truncated`.
- **Ledger:** one read-only open of the host ledger per machine for `capacity_holders`, `capacity_waits` and the route rows; process identity checks are bounded by the number of reservations.
- **Totals:** at most 200 ships per machine (`ROW_LIMIT`, as the dashboard caps), 768 KiB per machine section; past either, `truncated` and open tasks are dropped first, then jobs past 5 per ship.

### Secrets

No value from configuration, environment, argv, credentials or logs is read into the snapshot. Fields come from typed records only: names, statuses, times, counts, task titles, route identities (harness/provider/model, as bearings show them), bounded error text the ledger already records, need text `runtime.needs` already returns, and crew progress text the captain already reads in mail and `captain.feed`. No first-mate options are returned at all, so no masking is needed; if a later round adds them, they go through `ship.profile`'s `masked` (`crate::block_ask::sensitive_name`), never a second rule. A test checks a home whose first-mate options and a job's argv hold a canary secret, and that the canary appears nowhere in the output.

## Changes by file

- `src/fleet.rs` (or a new `src/snapshot.rs`): `fleet.snapshot` command, the local section reader, the merge, the digest and `fleet_snapshots` upsert; added to `OPERATOR_COMMANDS` in `src/lib.rs`.
- `src/machines.rs`: the `"fleet.snapshot"` gateway arm; parallel calls with `SNAPSHOT_TIMEOUT`; `fleet_snapshots` created in `init` and its row deleted in `remove`.
- `src/crew_routes.rs`: `states` factored out of `summary`; `summary` calls it.
- `src/presence.rs`, `src/host/sleep.rs`: `asleep_since_ms` column, set and cleared; a `presence::state` (**new**) returning the presence value.
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
5. `changed_ms` holds across two calls with nothing changed, moves on a new task, and survives a restart of the reading process; `since_ms` at that time returns the machine `unchanged` with no ships.
6. A route backed off in the ledger reads `backing_off` with `until_ms`, a capped one `capped`; bearings and the snapshot agree on both.
7. The canary secret appears nowhere; an agent caller is refused.
8. A home whose database is locked past the budget reads `unavailable` and is in `unknown`, and the others are complete.

`tests/machines.rs` (two machines: `fixture`, `link`, `remote_ssh`, `remote_ssh_with`, `connections`):

9. Laptop links mac; the snapshot on laptop lists both machines, mac's ships with live jobs and needs, `link: "reaches"`, `reachable: true`; one gateway connection per call.
10. A request from a mac ship collected to the laptop desk is one need with `machine: "mac"`.
11. Mac unreachable (`remote_ssh_with` an answer that fails, or one that sleeps past the timeout): mac keeps its last-known ships with `reachable: false`, `as_of_ms` of the earlier call, the same `changed_ms`, and the call returns within the timeout plus the local budget.
12. On mac, the snapshot lists the laptop as `reached_by`, `reachable: false`, directory ships `unknown`, and opens no connection.
13. A gateway that does not know `fleet.snapshot` (an older binary, faked by `remote_ssh_with`) reads as unreachable with that error, not as an empty machine.
14. `machine remove` drops the cached row.

Tests prove behaviour through commands and durable state, not snapshots of layout.

## Open questions

1. **State of a machine that reaches this one.** Round 1 lists it as not reachable with directory ships only. Proposal for round 2: each `fleet.collect` carries the collector's section when its digest changed since it last sent one, at most once a minute, stored in the same `fleet_snapshots` row; it arrives within `CONTACT_MS` and adds no connection. Confirm, or accept directory-only for `reached_by` machines.
2. **`asleep_since_ms`.** Asleep is not readable from presence today. Adding the column is the smallest fix; deriving it from a missing fresh `work_sessions` row is cheaper but cannot tell a sleeping pane from one still starting. Confirm the column.
3. **Job start time.** `started_ms` is the capacity reservation's time, so light jobs have none. Should the supervisor record `spawned_ms` on the job when the payload spawns? It is one field, but a job record change.
4. **`changed_ms` resolution.** It is only as fine as the calls. If the portal needs a change time without polling (a push or a long poll), that is a different command (a `fleet.watch`); out of scope here.
5. **Which machine answers for the portal.** Quarterdeck should call the machine that reaches the others (the one with `reaches` links), usually the captain's desk. If the desk is on a `reached_by` machine, the map is partial until question 1 is done.
