# Design: waits on another ship's task or request

Status: design only, no code. Task `privateer-typed-dependency-waits`, part 2. Read with [scheduling](../scheduling.md) (the typed waits `task.defer` already accepts), [cross-machine fleet](cross-machine-fleet.md) (gateways and machine peers), and [peers](../architecture.md).

## Problem

A task waits on a dependency in another ship's home, perhaps on the same machine, perhaps on another machine. Today the only typed waits are `task` and `request`, and `task.defer` checks both against this home only: `store.task(dependency)` and `mail::find` on this home's mail. So a first mate writes the dependency as a `time` wait with a prose reason. That drifts: the clock is a guess, and the wait ends whether or not the other ship has finished. The captain's incident had exactly this shape.

The fix is a wait kind that names a dependency owned by another ship and resumes when that ship says it is done. This note fixes the model before any code.

## Wait shape

A new wait kind, `remote`, with a `ref` that names the far end:

```json
{"kind":"remote","reason":"...","ref":{"ship":"storefront","task":"icons-5b57c97"}}
{"kind":"remote","reason":"...","ref":{"ship":"storefront","request":"<mail id>"}}
```

- `ship` is the owning ship's name in its own home, and `machine` is optional. When it is absent the ship is on this machine. When it is present the name resolves through the machine link (`machine list`), the same way mail addresses a ship on another machine.
- Exactly one of `task` or `request` is set. Task ids follow the existing rule (`^[a-z0-9][a-z0-9-]*$`). Request ids are the mail id the owner issued.
- The waiting home stores the ref as the wait, as it stores every other wait. It does not store the far end's state. `state` is `waiting` and `not_before_ms` is 0, so the scheduler does not poll on a clock.

Local `task` and `request` waits stay as they are. `remote` is the only kind that names another home.

## How the waiting home learns of completion

Push, not polling. Two existing mechanisms carry it:

1. **Mail closure.** A request ends with a runtime `request_end` notice (`src/mail.rs`, the closure notices carrying `payload.request_end`). The owner's home already sends that notice back to the requester when a request closes. A `remote` request wait is satisfied by the same closure, matched by the request id in the notice's correlation. This needs no new transport: the notice travels over the peer link mail already uses.
2. **Task completion.** A `remote` task wait needs the owner to say that a task reached a final status. The owner's home sends one notice, `task_end`, to each waiting home when the task closes. That notice is a runtime-authored mail with the same closure shape as `request_end`, not a field read from the other table. The waiting home records the satisfied wait in its own transaction, and the scheduler resumes the task on the next pass.

`peer.call` and the gateway are used only to deliver these notices and to subscribe. A subscription is one mail from the waiting home, `kind: notice`, `payload.wait_subscribe: {task}`, which the owner records against its task. The owner sends `task_end` when that task closes. A subscription is idempotent on (waiting task, far task), so a replay of the same defer adds nothing.

The waiting home does not read the owner's task table. It learns a state only through a notice the owner chose to send.

## Failure cases

| Case | Behaviour |
|---|---|
| Peer unreachable at defer time | The defer is applied. The subscription is queued as mail and delivered when the link returns. Mail already retries with backoff, so this needs no new retry loop. |
| Peer unreachable for a long time | The wait stays `waiting`. The existing backlog alarm and needs path show it, with the far end named. There is no automatic timeout; a deadline is a separate `time` wait added on top, if the captain wants one. |
| Owner ship retired | The owner's home closes outstanding subscriptions with `task_end` reason `retired` before it stops delivering. The waiting task resumes and sees the reason. If the owner is retired without that notice (for example a machine lost), the wait is handled as unreachable above. |
| Far task cancelled or closed without success | `task_end` carries the final status. The waiting task resumes with that status. It does not resume as if the dependency succeeded. |
| Far request withdrawn or dismissed | The same closure path as a local request: `request_end` with the end state. |
| Subscription for a task that already ended | The owner answers at once with `task_end` for its final status, so a late defer does not wait forever. |
| Ship renamed or scope moved | The ref names the ship. A rename breaks the ref; the defer fails loudly at its next check, and the first mate re-defers with the new name. |

## Security

- **Only what the peer shares.** The far end sends its task's final status and nothing else, and only for a task that a subscription names. It does not send its task table, its notes, its brief or its logs. A subscription is one task id, not a query.
- **No reads of another home's tables.** A waiting home never opens the other home's database. Everything crosses as a notice or a `peer.call` response the owner built.
- **Authority stays with the owner.** A ship may subscribe to a task only if the owner's charter or scope lets the subscriber see that task. The owner refuses otherwise with a `task_end` carrying `reason: refused`, which reveals nothing about whether the task exists.
- **Notices stay bounded.** `task_end` payloads are small and fixed-shape, under the mail envelope limit already enforced (`envelope_too_large`).

## Implementation order (not part of this task)

1. Accept `kind: remote` in `task.defer` with a local-only ref check, behind the same validation as the other kinds.
2. Subscription and `task_end` mail, with idempotent subscriptions and the refusal path.
3. Resume on satisfied `remote` waits, with the far status in the resume reason.
4. Cross-machine delivery through the existing machine link, tested with the isolated fleet fixture.

Each step is reviewable on its own. Step 1 by itself already lets a first mate name the dependency instead of a clock, if the far end is on the same home.

## Open questions

- Does `task_end` need its own mail kind, or can it ride `notice` with `payload.request_end`-style closure? Preference: a notice with a `task_end` payload, so the existing closure routing applies without a new kind.
- Should a `remote` wait with a far task that has already closed resume immediately, or fault? Preference: resume with the status, so the waiting task decides.
