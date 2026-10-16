# Automatic prepared work

Privateer schedules explicitly prepared tasks independently of mailbox traffic.
The existing resident dispatcher reconciles deadlines, recorded offers and
supervised jobs. No additional daemon or provider is required. Empty queues and
periodic eligibility checks make no model calls.

## Prepare, then enqueue

`open` and ranked tasks remain drafts until explicitly enqueued. This preserves
existing homes and avoids racing the established sequence of creating a task,
writing its assignment and starting its worker. Upgrading alone never launches
legacy tasks. Bearings, `work.status` and the dashboard label them `unscheduled`.

After creating a task owned by a local ship with a first-mate profile, prepare
its assignment and enqueue it:

```sh
privateer --home /absolute/ship/.privateer note.put --json '{"ship":"apps","focus":"assignment:repair","body":"Repair the failing fixture; verify the regression and report the tested artifact."}'
privateer --home /absolute/ship/.privateer task.enqueue --json '{"id":"repair"}'
```

Alternatively provide `brief` in `task.enqueue`. The runtime snapshots that
bounded assignment and assigns a new revision. Notes edited later do not
silently rewrite an active offer. Only the task owner (or operator) can enqueue;
existing charter and task authority applies. This adds no human approval step.
An outstanding claim or live/uncertain job prevents re-enqueueing. Use a child
task for separately prepared work. Rank controls selection among ready tasks.

## Delivery and acknowledgment

Without an owning attended session the dispatcher starts a fresh, task-focused
coordinator through the existing `agent.start`/job supervisor. Its durable work
ID is also its job ID. A dispatcher restart reuses that ID instead of launching
a second payload. Wake budgets, provider backoff, fleet stow and ordinary job
admission remain in force; work offers count toward the same hourly wake budget
as mail wakes. Heavy payloads still go through heavy-job admission.

For runtime-hosted Pi / Pi-signed / OMP sessions, the attended adapter reports
idle/busy state and a current context sample through `work.poll`. Healthy idle
sessions receive one work offer through the native custom-message interface.
The native start and settle events acknowledge acceptance and the result.
Mailbox receipts and work offers are separate: reading or acknowledging mail
cannot consume prepared work. Ordinary person-started sessions are not opted
into automatic work injection; they retain their existing presence protections.
Prepared work waits until that person-started session leaves, without treating
the absence of a hosted adapter as a delivery failure.

An idle hosted adapter with high or unavailable attention explicitly hands off
one offer to a fresh task-focused coordinator. It does not copy or discard the
person's conversation. The task claim remains authoritative. A quiet pane alone
never authorizes a competing controller; an unresponsive adapter instead yields
a visible scheduling fault after the delivery grace period.

Scheduled coordinators receive the work ID in their prompt. Include
`work_offer: "work-..."` on `agent.start`/`job.start` when launching payloads under
that claim from a hosted coordinator. Headless coordinator jobs and their
recorded descendants inherit their claim. Admission checks this in the same
write transaction as job insertion. A different launcher cannot race the claim.

Delivery is not execution proof. Unacknowledged/ambiguous native delivery, an
expired claim without a live payload, or a coordinator that exits without
completion, child work or a typed wait becomes `fault`. It is not automatically
replayed indefinitely. A failed native queue submission has a 60-second cooldown
and at most three delivery attempts. Reassessment and an explicit new enqueue
are required after a fault. A live or uncertain supervised payload retains its
ownership even after the delivery deadline; lease expiry never proves it stopped.

## Typed waits

A scheduled task cannot arrange resumption through prose notes or a `waiting`
pin alone. Use `task.defer`, including the current `offer_id` when it has a claim:

```json
{"id":"repair","offer_id":"work-...","wait":{"kind":"time","until_ms":1900000000000,"reason":"Wait for the agreed maintenance window"}}
```

Wait kinds:

| Kind | Required condition | Resumption |
|---|---|---|
| `time` | Future `until_ms` | Next dispatcher/adapter check at or after the deadline |
| `attention` | Future `until_ms` | Recheck then; the adapter supplies fresh attention or hands off to a fresh context |
| `task` | Local task `ref` | Dependency becomes `done`; cancellation faults instead of pretending success |
| `request` | This task's own open request `ref` | Correlated answer/approval; denial or withdrawal does not silently resume |
| `manual` | Reason | Explicit new enqueue after new direction; never a timed auto-resume |

All waits require a reason. Dependencies are checked for scheduling cycles.
Waiting on a correction does not block the corrective child: actual dependencies
and cancelled/revoked authority govern eligibility. A manually stopped scheduled
ancestor prevents new automatic descendant work. Manual parking does not kill
an already admitted payload; use existing job/task cancellation for that.

## Context freshness and observation

Attention reports bind to the current presence holder. The latest measured
fullness is retained separately from the rolling growth/compaction window.
A stale sample or a different session reads `unknown`, with the previous
measurement retained as history. Time passing cannot make a full context healthy.
The idle adapter supplies a fresh sample without a model call before claiming
hosted work. Compaction's before-size does not become a current fullness sample.

`work.status {ship}` and bearings show readiness, waits, claims, faults, last
check and next deadline. The dashboard includes scheduling state beside tasks;
`runtime.needs` exposes persistent scheduling faults as typed observation-only
blockers. Observing these records never starts, repairs or acknowledges work.

The fixture suites qualify runtime behavior, real subprocess recovery and the
adapter/CLI boundary. They do not establish autonomous judgment or qualify every
native harness/version. Use isolated homes and disposable work for qualification;
this change neither upgrades a live fleet nor changes production approval rules.
