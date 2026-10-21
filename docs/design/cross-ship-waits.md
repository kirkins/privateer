# Design: waits on another ship's task

Status: approved by the captain on 2026-10-20. Step 1 (the owner side, `src/waits.rs`), step 2 (the waiting side, `src/scheduling/remote.rs`), step 3 (resync on reconnect, staleness, subscription nonces), step 4 (retire, dropped refusals) and step 5 (cross-machine tests, `ship.move`) are implemented. Read with [scheduling](../scheduling.md) (the typed waits `task.defer` accepts), [cross-machine fleet](cross-machine-fleet.md) (machine links and peers), and [architecture](../architecture.md).

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

1. **Subscribe.** When the defer is applied, the waiting home sends the owner a runtime notice `payload.wait_subscribe: {task, subscriber, subscriber_task, nonce}`. The owner records one subscription row keyed by (subscriber ship, subscriber task, far task), with the nonce. Re-sending the same subscription adds nothing; a new nonce for the same key replaces the row's.

   The nonce names one generation of the wait: a wait that leaves and later re-enters a remote wait on the same far task gets a new one, recorded on the schedule (`subscription: {nonce, since_ms}`). The owner echoes it in every `task_end` and `wait_closed` it sends about that row, one notice per subscription, and the waiting home applies a notice that names a nonce only to the wait holding it, so a late answer to an old subscription (a refusal that was still in flight when the wait was made again) resolves nothing newer and is dropped as `nonce`. Peers from steps 1 and 2 send and store no nonce: a notice without one applies to every matching wait as before, and a wait without one takes any answer. A `wait_unsubscribe` carries its nonce too, and removes a row only of that generation (or one without a nonce).
2. **Report.** When the owner's task reaches a final status (`done`, `cancelled`), the owner sends each subscriber a runtime notice `payload.task_end: {task, status, reason}`. If the task had already ended when the subscription arrives, the owner answers at once with its final status, so a late defer does not wait forever. The owner sends `wait_closed` `{task, reason}` instead when it refuses the subscription (`reason: refused`), later removes the subscriber from `shared_with` (`reason: refused`), hands the task to another ship (`reason: handed_off`, with `owner` naming the new owner), or is retired (`reason: retired`). `wait_closed` is not a final status and reports none.
3. **Resume.** The waiting home verifies the notice (below), records the satisfied wait in its own transaction, and the scheduler resumes the task with the far status in the resume reason. A `cancelled` status, or a `wait_closed` notice, faults the waiting task; it never resumes as if the dependency succeeded.

   A `task_end` names the far task, not the subscriber's task, so it resolves every waiting task of the addressed ship whose wait names that far ship and task (all of them, a page at a time), narrowed by its nonce when it carries one. The resolution is recorded on the wait (`wait.end`), and the task is checked at once: a `done` reopens it as `ready`, with `resumed_by` (shown in `work.status`, bearings and the next offer's prompt) naming the far task and its status. Anything else marks the schedule `fault` with the reason, which raises the scheduling-fault notice to the ship and appears among the captain's needs, as every scheduling fault does. Each fault has its own notice, so a second fault in the same revision (a re-wait refused again) wakes the first mate again; the same fault recorded twice on a faulted schedule sends one. Parking on a block ask or the captain's hold clears `resumed_by`, so an old resume never reaches a later offer. A resolved wait no longer matches, so a repeated `task_end` or a later `wait_closed` changes nothing.

`wait_subscribe`, `wait_unsubscribe`, `task_end` and `wait_closed` are runtime-to-runtime. They are handled before any inbox is written, so they never wake the owner's first mate and never appear as mail. They are reserved the way `request_end` is (`src/mail.rs`, the reserved-payload check near line 1091): an agent's `mail.send` that sets any of these keys is refused, so only the runtime can author them. The notices ride the existing outbox and peer link; no new transport.

The waiting home never opens the owner's database. It learns a state only from a `task_end` the owner chose to send.

## Anti-forgery

The link authenticates a machine, not a ship. A notice is accepted only when all of these hold, otherwise it is dropped and audited:

- `from` equals the ship named in the wait's ref. A notice from any other ship is dropped, even one that can reach the machine.
- `via` names the machine in the ref, or is absent when the ref names no machine. `via` is the sending machine's declared name, checked against the machine registry (`src/mail.rs`, around line 1466). The incoming path trusts a `from` ship only because that ship is registered with a remote peer (around line 2001).
- The subscription row matches. The owner accepts a `wait_subscribe` only for a task that exists and is shared with that subscriber. The waiting home accepts a `task_end` or `wait_closed` only for a task whose wait names this far task, so unsolicited reports resume nothing.
- For `task_end`, `task` matches the wait and `status` is one of `done` or `cancelled`.

A dropped notice is still accepted on the wire, so it never holds up the peer's queue, and is audited as `wait.dropped` with why (`no_wait`, `via` or `status`). A malformed one (no task, two instructions, a non-string field) is refused.

At defer time the ref must name a ship this home reaches through a peer by the route the ref gives: without `machine`, a peer to another home on this machine; with `machine`, that machine's link. A ship hosted by this home is refused (use `task`), as is a ship on another machine named without its machine, so a wait cannot be set that no notice could ever satisfy. The stored wait holds `kind`, `reason` and `ref` only: a resolution in the caller's arguments is not kept.

On one machine this is the same trust `request_end` already gets: the ship that sends is the one the ref names, and the machine is the one the link came in on.

## Visibility rule

The owner shares a task explicitly. A task has a `shared_with` list of ships allowed to subscribe to it, set by the owner through `task.update`. A subscription from a ship not on the list is refused with a `wait_closed` notice, `reason: refused`, which reveals nothing about whether the task exists. The waiting task faults.

I chose explicit marking over "tasks linked to a request thread the subscriber is in". A request is ended only by its sender, so a requester who opens a thread should not decide what the owner may reveal: inferring visibility from threads would let a requester open access the owner never granted. An explicit list is default-deny, is visible in `task.show`, and is audited when it changes. The cost is that the owner must mark a dependency before a remote ship can wait on it; that is acceptable, because the owner is the one who decides what another ship may see.

What crosses the link is the final status and a short reason, and nothing else: no notes, brief, logs or task table. A subscription names one task, not a query.

## Unsubscribe and cleanup

- The waiting task is cancelled, or re-deferred with a different ref: the waiting home sends `wait_unsubscribe` for the old subscription before it changes the wait. The owner deletes the row.
- The waiting task is handed to another ship: its schedule still names the old owner, which every subscription speaks for, so `task.defer` refuses until the new owner enqueues its own assignment; its subscriptions then name the new owner, and always go to the ship the ref names.
- The waiting task ends by any other path (faulted, or its wait is manually stopped): the same unsubscribe is sent when the task leaves `waiting`. Every schedule write compares the old and new record, so this covers a re-defer of any kind, a fresh `task.enqueue`, a park on a block ask, the captain's hold, a stopped ancestor, a lost charter authority, a check failure, the task finishing or being cancelled, and the task's handoff to a new owner. Cancelling, finishing or handing off the waiting task settles its schedule in the same transaction, so the unsubscribe is queued at once.
- The owner hands its task to another ship (`task.accept`): the old owner, whom every ref names, will never report the end, so it sends each subscriber `wait_closed` with `reason: handed_off` and `owner` naming the new owner, and deletes its rows. The waiting task faults naming the new owner, so its first mate can re-wait on that ship (which must share the task again). An offer that is not accepted changes nothing.
- The owner removes a ship from `shared_with`: that ship's subscriptions on the task are deleted and it is sent `wait_closed` with `reason: refused`, as a new subscription would be. Its waiting tasks fault.
- The owner's task ends: the owner sends `task_end` once to each subscriber and deletes its subscription rows, so none outlive the task.
- The owner ship is retired: the owner's home sends `wait_closed` with `reason: retired` to each subscriber before it stops delivering, then deletes its rows. The waiting task faults with that reason. A retired owner's task can never finish, so the wait must not resume.
- The waiting ship is retired: `ship.retire` (`src/fleet.rs`) today only revokes the charter, stops the dispatcher and removes registry entries. It cancels no tasks and does not empty the outbox first. So retire gains a step: before stopping the dispatcher, send `wait_unsubscribe` for each remote wait (and `wait_closed` for each subscription it owns) and let the outbox drain. Its waiting tasks need no cancelling: with the charter revoked they are no longer eligible to run.
- A refused wait notice is not retried for ever. Today a refused message that is not from the captain stays pending, and one peer's failure pauses that peer's whole queue (`src/mail.rs`, near the delivery loop). So a `task_end`, `wait_closed`, `wait_subscribe` or `wait_unsubscribe` refused three times is dropped, not held, and the owner deletes the matching row. Otherwise one notice to a retired ship would hold back all later mail to that peer. An error that says only the peer could not work just then is not a refusal and is retried as before.
- When `wait_closed` faults a waiting task, the waiting home does not send `wait_unsubscribe` back: the owner has already removed the row.
- The owner's home is lost without notice: the subscriber cannot tell the difference, so it falls back to the unreachable rule below.

## Unreachable peer

"Unreachable" means the far status is unknown, not that the task is unfinished. The wait stays `waiting`. Two mechanisms, with no polling:

1. **Reconnect resync.** When a machine link comes back, each side re-sends its live subscriptions, and the owner re-sends the final status of any subscribed task that ended while the link was down. Both are idempotent, so a replay is harmless. This is what makes push reliable across an outage.
2. **Staleness surfacing.** A `remote` wait with no `task_end` after a threshold (proposed: one hour, configurable) is shown by the existing needs path and the backlog alarm, naming the far ship and machine and the time the link was last heard. It never resumes or faults by itself. A deadline, if the captain wants one, is a separate `time` wait added on top.

Staleness is the honest answer to "no next check": the runtime reports how long it has been waiting on a silent peer rather than inventing a result.

### As built (step 3)

A home records each peer's link in `wait_links` (`heard_ms`, `down_ms`). A peer is heard when a delivery to it is answered (even refused), when mail from a ship behind it is delivered here, when it acknowledges mail it collected, and, for a machine that collects from this one, on each of its `fleet.collect` calls. A delivery that cannot reach it marks the link down. The link has come back when it is heard while marked down or, for a machine that only ever calls this home and so is never seen to fail, when it is heard after a silence of four contact intervals (two minutes). The machine that does the calling also resyncs every home here when a contact succeeds after failures (`machines::tick`). The first time a peer is heard only records it.

A resync runs in the home's own transaction and is audited as `wait.resync` `{peer, resubscribed, answered}`:

- the waiting side sends `wait_subscribe` again for each live remote wait whose far ship is behind that peer, with the nonce it already holds (a wait from before nonces gains one), paging through all of them;
- the owner, receiving it, stores nothing new for a live subscription and answers at once with `task_end` for a far task that ended meanwhile: this is how a final status lost during the outage (refused on a machine link and quarantined, say) is sent again;
- the owning side checks each subscription a ship behind the peer holds: one whose task ended without its `task_end` leaving gets it now, one no longer shared is closed `refused`, and both rows go.

A final status still pending in the owner's outbox is delivered as before when the link returns; the resync's answer then arrives as a repeat and is dropped as `no_wait`.

A link resyncs at most once a minute (`resynced_ms`). One that comes back sooner is recorded as heard but stays owed a resync (`down_ms` stays set), which runs the first time it is heard after the spacing; this holds too for a machine link contacted again after failing moments after it was last heard. Whether a link has come back is decided under the home's write lock, so two processes hearing it at once resync it once. A resync that fails is rolled back whole (nothing half sent) and audited as `wait.resync_failed`; the link is recorded as heard and stays owed, so the resync is tried again after the spacing rather than on every delivery. A schedule or task the resync cannot read is skipped and audited as `wait.resync_skipped` `{peer, task, error}`, and the rest of the link's waits resync. A resync that sends nothing is not audited. A waiting task whose charter is revoked is not resubscribed; one whose charter cannot be checked is skipped and audited as `wait.resync_skipped` too.

On one machine there is no network link, but a peer is still a subprocess call into another home, which fails if that home or its executable cannot be reached. The same rule applies: the failure marks the link down, the next answered delivery or mail from that home resyncs, and a single home has no peers and nothing to resync.

Staleness is measured from the subscription's `since_ms` (a wait from before step 3 from its schedule's creation) against the waiting ship's `first_mate.remote_wait_stale_secs` (default 3600, `0` shows none, at most a week). A stale wait is a `runtime.needs` blocker with `evidence: "stale_remote_wait"`, and a backlog alarm candidate labelled `stale remote wait`, each naming the far task, ship and machine and when the link was last heard. Neither changes the wait.

### As built (step 4)

`ship.retire` (`src/fleet.rs`) revokes the charter, then, in the retiring ship's home and one transaction (`waits::retire`), closes every subscription on a task that ship owns with `wait_closed` `{reason: retired}` (one per subscription generation, nonce echoed) and deletes the rows, and queues `wait_unsubscribe` for each live remote wait the ship holds, with its nonce. Its schedules are left as they are. This is audited as `wait.retired` `{ship, tasks, closed, unsubscribed}` when it did anything. The ship's outbox then drains, for at most 15 seconds, before the dispatcher stops: when the operator retires, this process delivers the mail itself, retrying an unreachable peer between passes; an agent cannot carry another ship's mail (the peer would refuse it, and a job cannot drop its role), so its retire only waits while the retiring ship's own dispatcher delivers. Mail behind a collecting machine is not waited for. What is still pending at the end stays queued, and the far side falls back to the staleness path. The reply's `waits` reports what was closed, unsubscribed and drained.

A peer's rpc loop and a machine gateway answer every error as `ok: false`, so an answered error is not always a refusal. One that says the peer could not work just then (`waits::temporary`: a database busy or locked past its busy timeout, a home or the fleet registry it could not open, an I/O error, a hop beyond it it could not reach) is retried with the usual per-peer backoff, as an unreachable peer is, and never counts towards a drop. Any other error refusing a `task_end`, `wait_closed`, `wait_subscribe` or `wait_unsubscribe` is counted on the message (`delivery_refusals`), and the notice is retried with the usual backoff; the third refusal marks it `delivery: dropped` with the refusal, so it is not retried again and the peer's later mail is no longer held behind it or backed off for it. The same applies when a collecting machine reports one refused: it stays pending to be collected again until its third refusal. Only when a `task_end` or `wait_closed` is dropped does the owner delete its row of the generation it answered (same subscriber, task and nonce), if one was stored again meanwhile; until then the row stays, so a refusal that clears still resumes the wait. Each drop is audited as `wait.notice_dropped` `{id, notice, from, to, task, reason, refusals, rows}`. `mail.outbox` lists dropped mail with `delivery: "dropped"`; it is not quarantined, so it raises nothing for the captain. A notice that could not reach the peer at all is still retried as before.

A refused `wait_subscribe` leaves the waiting task `waiting`, shown as stale after the threshold. Faulting it at once is a possible follow-up, but a refusal can be transient.

Fault notices: a schedule that faulted before fault counts were stored (under step 2) counts its existing first notice, so its next different fault in the same revision gets its own id and wakes the first mate.

The backlog alarm does not advise re-deferring a stale remote wait. It says the wait is already deferred and resumes on the owner's report, and to check the far ship and the link it rides, or to close or hand off the task.

### As built (step 5)

`tests/cross_machine_waits.rs` runs each case over a real machine link: two machines with their own registries and homes, the laptop reaching the Mac's gateway through a fake `ssh` subprocess, so what the laptop sends is delivered at once and what the Mac sends waits until the laptop collects it. It covers a far `done` resuming and a far `cancelled` faulting in each direction, an unshared and a missing task refused alike, a report lost during an outage answered on the resync (and the same report collected again dropped as `no_wait`), forged reports through the gateway (another ship of the linked machine, a `via` the gateway does not know or none, a status that is not final), retire of either ship, a notice the gateway refuses dropped on its third refusal while later mail flows, a locked home behind the gateway retried and never dropped, and both kinds of `ship.move`.

`ship.move` changes the route a ref names, and a report that comes by another route than the ref's is dropped as `via`, so a moved ship's waits could never end. The runtime does not rewrite refs: the waiting task faults with a clear reason, and its first mate re-waits with the ref as its home now reaches the far ship.

- **The owner ship moves.** Its home moves whole, with its tasks, its subscription rows and its outbox. Homes that learn its new route (the machines on both ends update their ships' directory entries as the move finishes) fault each live wait on it at the next scheduler check: `Remote wait on task T of S on machine M no longer reaches it: S is now on this machine; re-wait without ref.machine` (or `is now on machine N; re-wait with ref.machine N`, or `is now hosted by this home; wait on its task with kind task`). Faulting sends `wait_unsubscribe` by the new route, and the moved owner deletes its row. A home that does not know the new route keeps waiting, and the staleness path shows it; a report that reached it by the new route first was dropped as `via`, and the re-wait is answered at once with the far task's final status.
- **The waiting ship moves.** Its refs were written as the old machine reached the far ships. On arrival (`moves::activate`), each live remote wait whose far ship is now reached by another route, or is not reachable from the new machine at all, faults (`S moved here from X, and its remote wait on ... no longer reaches it: ...`), audited as `wait.arrived` and noted in the move's reply. Faulting queues `wait_unsubscribe` by the route the far ship has from the new machine. A ref that still holds (a third machine both machines reach under one name) keeps waiting.

A collecting machine that cannot write a message it collected just then (a home's database locked, `waits::temporary`) still reports it, but it is no refusal: the machine it came from keeps it pending, with the attempt and the error, and offers it again at the next collection, as a delivery that machine makes is retried. A captain's message refused that way is not put before the captain. `waits::temporary` matches SQLite's and the runtime's own phrases only, never a bare word a ship's name can hold, so `unknown ship busy-worker` is a refusal.

Not built: an owner retired on a machine that is collected from does not get its `wait_closed` out. Retire drains only what this machine delivers itself, and a retired ship's home leaves the fleet registry, so a collecting machine no longer gathers its outbox: the waiter falls to the staleness path, as for a lost home.

Not built: `mail.deliver` still accepts wait notices from an agent caller. An agent's flush delivers its ship's runtime notices under the agent's identity, and a peer call from inside a job cannot drop to the runtime's role. Refusing agent callers would therefore first need the agent's flush to leave wait notices to the dispatcher, without holding the agent's own mail behind them.

## Failure cases

| Case | Behaviour |
|---|---|
| Peer unreachable at defer time | Defer applied; subscription queued in the outbox and delivered on reconnect. |
| Peer unreachable for a long time | Stays `waiting`; staleness surfaced with the far end named. No automatic resume. |
| Far task cancelled or closed unsuccessfully | `task_end` carries the status; the waiting task faults. |
| Subscription for an already-ended task | Owner answers at once with its final status. |
| Owner ship retired | `wait_closed` `reason: retired` if the owner's home is still up, and the waiting task faults; otherwise the staleness path. |
| Waiting ship retired | `wait_unsubscribe` for each live remote wait; the owner deletes its rows. Its tasks are not cancelled. |
| A peer refuses a wait notice | Retried with backoff, dropped (`delivery: dropped`) on the third refusal; later mail to that peer then flows. A dropped report deletes the matching owner row. An error saying the peer could not work just then (a busy database, a home it could not open) is retried like an unreachable peer and never dropped. |
| Ship renamed or scope moved | The ref names the ship; the subscription fails with a clear refusal and the first mate re-defers with the new name. |
| Owner ship moved (`ship.move`) | Its rows move with it. A waiting home that knows the new route faults the wait at its next check, naming the route to re-wait by; the fault unsubscribes by the new route. One that does not know it yet keeps waiting, shown as stale. |
| Waiting ship moved (`ship.move`) | On arrival each live remote wait whose far ship is now reached by another route, or not at all, faults naming the route to re-wait by, and unsubscribes by the far ship's new route. |
| Owner retired on a machine that is collected from | Its `wait_closed` is not collected once it leaves the fleet; the waiting task falls to the staleness path. |
| A collecting machine cannot write a message just then | It stays pending where it was sent, with the error, and is collected again; it is not quarantined. |
| Far task not shared with this ship | `wait_closed` `reason: refused`, and the waiting task faults. Nothing about the task is revealed. |
| Ship removed from `shared_with` while waiting | `wait_closed` `reason: refused`; the row is deleted and the waiting task faults. |
| Far task handed to another ship | `wait_closed` `reason: handed_off`, naming the new owner; the waiting task faults, and its first mate re-waits on the new owner. |
| Report from another ship, another machine, or with a non-final status | Dropped and audited (`wait.dropped`); nothing resumes. |
| Repeated `task_end` or `wait_closed` | Nothing: the wait is already resolved. |

## Implementation order (not part of this task)

1. Owner side: `shared_with` on tasks, `wait_subscribe` and `wait_unsubscribe` handling, and `task_end` on final status, with idempotent rows and the refusal path.
2. Waiting side: accept `kind: remote` in `task.defer`, the reserved-payload guard, the anti-forgery checks, and resume with the far status.
3. Resync on reconnect and the staleness surfacing in needs.
4. Retire: send `wait_unsubscribe` and `wait_closed` and drain the outbox before stopping the dispatcher; drop a wait notice refused three times rather than leave it pending.
5. Cross-machine tests through the isolated fleet fixture (two homes, one link), including replay, reconnect and a refused subscription.

Each step is reviewable on its own. There is no same-home step: a dependency on the same home is already a `task` wait.

## Open questions

- Should `shared_with` default to empty for existing tasks? Proposed: yes, so nothing becomes visible on upgrade.
- Is one hour the right staleness threshold? It is a guess to be tuned from real outages, and it should not be the only signal.
