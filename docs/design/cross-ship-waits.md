# Design: waits on another ship's task

Status: design only, no code. Task `privateer-typed-dependency-waits`, part 2. Read with [scheduling](../scheduling.md) (the typed waits `task.defer` accepts), [cross-machine fleet](cross-machine-fleet.md) (machine links and peers), and [architecture](../architecture.md).

## Problem

A task may depend on work owned by another ship, on this machine or another. `task.defer` checks `task` waits against this home's tasks and `request` waits against this home's mail. So a first mate writes the dependency as a `time` wait with a prose reason. The clock is a guess, and the wait ends whether or not the other ship finished.

A request wait is not enough for this. Requests already work across ships: the reply is correlated back into the requester's home and sets the request `answered` locally. But `answered` is not `done`. A request wait resumes on the first reply, even an interim "on it". The gap is the far task's final status, so the new wait names a task, not a request.

## Wait shape

A new wait kind, `remote`, which names a task in another ship's home:

```json
{"kind":"remote","reason":"...","ref":{"ship":"storefront","task":"icons-5b57c97"}}
{"kind":"remote","reason":"...","ref":{"ship":"storefront","task":"icons-5b57c97","machine":"studio"}}
```

- `ship` is the owning ship's name in its own home. `machine` is optional: absent means this machine, present means the name resolves through the machine link (`machine list`), as mail addresses a ship on another machine.
- `task` follows the existing id rule (`^[a-z0-9][a-z0-9-]*$`). Only tasks are remote. A request from another ship stays a `request` wait, because it is already cross-ship.
- The waiting home stores the ref as its wait and does not store the far end's state. The wait is `waiting` with `not_before_ms` 0, so the scheduler never polls on a clock.
- A task cannot be remote-waited on itself, and a `remote` wait is refused if its ref equals this home's own ship and task (use `task`).

## How the waiting home learns of completion

Push only. There is no next-check timer.

1. **Subscribe.** When the defer is applied, the waiting home sends the owner a runtime notice `payload.wait_subscribe: {task, subscriber, subscriber_task}`. The owner records one subscription row keyed by (subscriber ship, subscriber task, far task). Re-sending the same subscription adds nothing.
2. **Report.** When the owner's task reaches a final status (`done`, `cancelled`), the owner sends each subscriber a runtime notice `payload.task_end: {task, status, reason}`. If the task had already ended when the subscription arrives, the owner answers at once with its final status, so a late defer does not wait forever.
3. **Resume.** The waiting home verifies the notice (below), records the satisfied wait in its own transaction, and the scheduler resumes the task with the far status in the resume reason. A `cancelled` status faults the waiting task; it never resumes as if the dependency succeeded.

Both `wait_subscribe` and `task_end` are runtime-to-runtime. They are handled before any inbox is written, so they never wake the owner's first mate and never appear as mail. They are reserved the way `request_end` is (`src/mail.rs`, the reserved-payload check near line 1091): an agent's `mail.send` that sets either key is refused, so only the runtime can author them. The notices ride the existing outbox and peer link; no new transport.

The waiting home never opens the owner's database. It learns a state only from a `task_end` the owner chose to send.

## Anti-forgery

A `task_end` is accepted only when all three hold, otherwise it is dropped and audited:

- it arrives over the authenticated peer link, and the link's identity is the ship named in the subscription's ref (the owner's ship, not any ship that can reach the machine);
- a subscription row exists in the waiting home for (this waiting task, this far task), so unsolicited reports resume nothing;
- its `task` matches that subscription and its `status` is one of the two final states.

## Visibility rule

The owner shares a task explicitly. A task has a `shared_with` list of ships allowed to subscribe to it, set by the owner through `task.update`. A subscription from a ship not on the list is refused with `task_end` `reason: refused`, which reveals nothing about whether the task exists.

I chose explicit marking over "tasks linked to a request thread the subscriber is in". Mail threads are created and closed by either side, so inferring visibility from them lets a request grant access it was never meant to. An explicit list is default-deny, is visible in `task.show`, and is audited when it changes. The cost is that the owner must mark a dependency before a remote ship can wait on it; that is acceptable, because the owner is the one who decides what another ship may see.

What crosses the link is the final status and a short reason, and nothing else: no notes, brief, logs or task table. A subscription names one task, not a query.

## Unsubscribe and cleanup

- The waiting task is cancelled, or re-deferred with a different ref: the waiting home sends `wait_unsubscribe` for the old subscription before it changes the wait. The owner deletes the row.
- The waiting task ends by any other path (faulted, or its wait is manually stopped): the same unsubscribe is sent when the task leaves `waiting`.
- The owner's task ends: the owner sends `task_end` once to each subscriber and deletes its subscription rows, so none outlive the task.
- The owner ship is retired: the owner's home sends `task_end` with `reason: retired` to each subscriber before it stops delivering, then deletes its rows. A waiting task resumes and sees the reason.
- The owner's home is lost without notice: the subscriber cannot tell the difference, so it falls back to the unreachable rule below.

## Unreachable peer

"Unreachable" means the far status is unknown, not that the task is unfinished. The wait stays `waiting`. Two mechanisms, with no polling:

1. **Reconnect resync.** When a machine link comes back, each side re-sends its live subscriptions, and the owner re-sends the final status of any subscribed task that ended while the link was down. Both are idempotent, so a replay is harmless. This is what makes push reliable across an outage.
2. **Staleness surfacing.** A `remote` wait with no `task_end` after a threshold (proposed: one hour, configurable) is shown by the existing needs path and the backlog alarm, naming the far ship and machine and the time the link was last heard. It never resumes or faults by itself. A deadline, if the captain wants one, is a separate `time` wait added on top.

Staleness is the honest answer to "no next check": the runtime reports how long it has been waiting on a silent peer rather than inventing a result.

## Failure cases

| Case | Behaviour |
|---|---|
| Peer unreachable at defer time | Defer applied; subscription queued in the outbox and delivered on reconnect. |
| Peer unreachable for a long time | Stays `waiting`; staleness surfaced with the far end named. No automatic resume. |
| Far task cancelled or closed unsuccessfully | `task_end` carries the status; the waiting task faults. |
| Subscription for an already-ended task | Owner answers at once with its final status. |
| Owner ship retired | `task_end` `reason: retired` if the owner's home is still up; otherwise the staleness path. |
| Ship renamed or scope moved | The ref names the ship; the subscription fails with a clear refusal and the first mate re-defers with the new name. |
| Far task not shared with this ship | `task_end` `reason: refused`, nothing else. |

## Implementation order (not part of this task)

1. Owner side: `shared_with` on tasks, `wait_subscribe` and `wait_unsubscribe` handling, and `task_end` on final status, with idempotent rows and the refusal path.
2. Waiting side: accept `kind: remote` in `task.defer`, the reserved-payload guard, the anti-forgery checks, and resume with the far status.
3. Resync on reconnect and the staleness surfacing in needs.
4. Cross-machine tests through the isolated fleet fixture (two homes, one link), including replay, reconnect and a refused subscription.

Each step is reviewable on its own. There is no same-home step: a dependency on the same home is already a `task` wait.

## Open questions

- Should `shared_with` default to empty for existing tasks? Proposed: yes, so nothing becomes visible on upgrade.
- Is one hour the right staleness threshold? It is a guess to be tuned from real outages, and it should not be the only signal.
