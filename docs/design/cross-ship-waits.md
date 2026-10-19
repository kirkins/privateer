# Design: waits on another ship's task

Status: proposed, awaiting the captain's approval; design only, no code. Read with [scheduling](../scheduling.md) (the typed waits `task.defer` accepts), [cross-machine fleet](cross-machine-fleet.md) (machine links and peers), and [architecture](../architecture.md).

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
2. **Report.** When the owner's task reaches a final status (`done`, `cancelled`), the owner sends each subscriber a runtime notice `payload.task_end: {task, status, reason}`. If the task had already ended when the subscription arrives, the owner answers at once with its final status, so a late defer does not wait forever. The owner sends `wait_closed` `{task, reason}` instead when it refuses the subscription (`reason: refused`) or is retired (`reason: retired`). `wait_closed` is not a final status and reports none.
3. **Resume.** The waiting home verifies the notice (below), records the satisfied wait in its own transaction, and the scheduler resumes the task with the far status in the resume reason. A `cancelled` status, or a `wait_closed` notice, faults the waiting task; it never resumes as if the dependency succeeded.

`wait_subscribe`, `wait_unsubscribe`, `task_end` and `wait_closed` are runtime-to-runtime. They are handled before any inbox is written, so they never wake the owner's first mate and never appear as mail. They are reserved the way `request_end` is (`src/mail.rs`, the reserved-payload check near line 1091): an agent's `mail.send` that sets any of these keys is refused, so only the runtime can author them. The notices ride the existing outbox and peer link; no new transport.

The waiting home never opens the owner's database. It learns a state only from a `task_end` the owner chose to send.

## Anti-forgery

The link authenticates a machine, not a ship. A notice is accepted only when all of these hold, otherwise it is dropped and audited:

- `from` equals the ship named in the wait's ref. A notice from any other ship is dropped, even one that can reach the machine.
- `via` names the machine in the ref, or is absent when the ref names no machine. `via` is the sending machine's declared name, checked against the machine registry (`src/mail.rs`, around line 1466). The incoming path trusts a `from` ship only because that ship is registered with a remote peer (around line 2001).
- The subscription row matches. The owner accepts a `wait_subscribe` only for a task that exists and is shared with that subscriber. The waiting home accepts a `task_end` or `wait_closed` only for a task whose wait names this far task, so unsolicited reports resume nothing.
- For `task_end`, `task` matches the wait and `status` is one of `done` or `cancelled`.

On one machine this is the same trust `request_end` already gets: the ship that sends is the one the ref names, and the machine is the one the link came in on.

## Visibility rule

The owner shares a task explicitly. A task has a `shared_with` list of ships allowed to subscribe to it, set by the owner through `task.update`. A subscription from a ship not on the list is refused with a `wait_closed` notice, `reason: refused`, which reveals nothing about whether the task exists. The waiting task faults.

I chose explicit marking over "tasks linked to a request thread the subscriber is in". A request is ended only by its sender, so a requester who opens a thread should not decide what the owner may reveal: inferring visibility from threads would let a requester open access the owner never granted. An explicit list is default-deny, is visible in `task.show`, and is audited when it changes. The cost is that the owner must mark a dependency before a remote ship can wait on it; that is acceptable, because the owner is the one who decides what another ship may see.

What crosses the link is the final status and a short reason, and nothing else: no notes, brief, logs or task table. A subscription names one task, not a query.

## Unsubscribe and cleanup

- The waiting task is cancelled, or re-deferred with a different ref: the waiting home sends `wait_unsubscribe` for the old subscription before it changes the wait. The owner deletes the row.
- The waiting task ends by any other path (faulted, or its wait is manually stopped): the same unsubscribe is sent when the task leaves `waiting`.
- The owner's task ends: the owner sends `task_end` once to each subscriber and deletes its subscription rows, so none outlive the task.
- The owner ship is retired: the owner's home sends `wait_closed` with `reason: retired` to each subscriber before it stops delivering, then deletes its rows. The waiting task faults with that reason. A retired owner's task can never finish, so the wait must not resume.
- The waiting ship is retired: `ship.retire` (`src/fleet.rs`) today only revokes the charter, stops the dispatcher and removes registry entries. It cancels no tasks and does not empty the outbox first. So retire gains a step: before stopping the dispatcher, send `wait_unsubscribe` for each remote wait (and `wait_closed` for each subscription it owns) and let the outbox drain. Its waiting tasks need no cancelling: with the charter revoked they are no longer eligible to run.
- A refused wait notice is never retried. Today a refused message that is not from the captain stays pending, and one peer's failure pauses that peer's whole queue (`src/mail.rs`, near the delivery loop). So a refused `task_end`, `wait_closed`, `wait_subscribe` or `wait_unsubscribe` is dropped, not held, and the owner deletes the matching row. Otherwise one notice to a retired ship would hold back all later mail to that peer.
- When `wait_closed` faults a waiting task, the waiting home does not send `wait_unsubscribe` back: the owner has already removed the row.
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
| Owner ship retired | `wait_closed` `reason: retired` if the owner's home is still up, and the waiting task faults; otherwise the staleness path. |
| Ship renamed or scope moved | The ref names the ship; the subscription fails with a clear refusal and the first mate re-defers with the new name. |
| Far task not shared with this ship | `wait_closed` `reason: refused`, and the waiting task faults. Nothing about the task is revealed. |

## Implementation order (not part of this task)

1. Owner side: `shared_with` on tasks, `wait_subscribe` and `wait_unsubscribe` handling, and `task_end` on final status, with idempotent rows and the refusal path.
2. Waiting side: accept `kind: remote` in `task.defer`, the reserved-payload guard, the anti-forgery checks, and resume with the far status.
3. Resync on reconnect and the staleness surfacing in needs.
4. Retire: send `wait_unsubscribe` and `wait_closed` and drain the outbox before stopping the dispatcher; drop a refused wait notice rather than leave it pending.
5. Cross-machine tests through the isolated fleet fixture (two homes, one link), including replay, reconnect and a refused subscription.

Each step is reviewable on its own. There is no same-home step: a dependency on the same home is already a `task` wait.

## Open questions

- Should `shared_with` default to empty for existing tasks? Proposed: yes, so nothing becomes visible on upgrade.
- Is one hour the right staleness threshold? It is a guess to be tuned from real outages, and it should not be the only signal.
