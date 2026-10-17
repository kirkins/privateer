# Handoff: crew outcomes wake the first mate

Status: **implemented** (see [architecture](../architecture.md#crew-outcomes)); handoff written 14 October 2026. Where the implementation differs from this handoff is listed under [Implementation notes](#implementation-notes). This is step 3 of the build order in [conversation and activation](conversation-and-activation.md). Read [architecture](../architecture.md) ("Waking first mates", "Attended mode") for what exists.

## Goal

A first mate starts crew as supervised jobs and should not have to poll them. When a crew job finishes, fails, times out or is orphaned, or a crew member needs a decision from its first mate, the first mate is told: woken if it is idle, shown it if the captain's session is aboard. The outcome is recorded by the runtime from the job's real state, never by a worker remembering to report.

## What exists today

- **Jobs** (`src/jobs.rs`): `job.start` runs a supervised payload; the detached supervisor (`supervise_inner`) records the final status (`succeeded`, `failed`, `cancelled`, `timed_out`, or `orphaned` when descendants outlive the leader) and writes a `job.finished` audit row. `reconcile` settles jobs whose supervisor vanished (`failed`, or the recorded leader result for orphans). Jobs started by an agent record `agent_actor` (the ship) and run with `PRIVATEER_ROLE=agent PRIVATEER_ACTOR=<ship>`.
- **Activation** (`src/activation.rs`): `privateer serve` wakes a ship's first mate when the ship has mail newer than its acknowledgement and last successful wake. Mail is labelled by focus (`context::focus_of`): mail with a `task_id` the ship owns belongs to `task:<id>`. Low-urgency notices do not trigger a wake on their own. A wake's own job ids are stored in the `wakes` table.
- **Attended mode** (`src/presence.rs`): while the captain's session holds the ship's presence lease, `serve` skips the ship and new mail reaches the session through hooks.
- **Mailboxes** (`src/mail.rs`): `captain` is a reserved address; `mail.send` refuses a mailbox writing to itself.

## Recommended design

### Outcomes as mail from a reserved `crew` sender

When a crew job reaches a final status, the runtime writes a notice to the ship that owns the job's task, from a reserved sender `crew` (reserved like `captain`: `ship.put` must refuse it, and only the runtime may send as it). Everything downstream then works unchanged: activation wakes the first mate, the mail lands in the task's focus with its handoff note, attended sessions see it through hooks, acknowledgement, retention and `focus.digest` apply.

- **Which jobs:** jobs whose task is owned by a ship hosted in this home, excluding the first mate's own wake jobs (ids present in `wakes.job_id`).
- **Idempotent id:** `crew-<job id>-<incarnation>`, so a retried or replayed settlement never sends twice.
- **Where to emit:** at every place a job becomes final: the end of `supervise_inner`, the supervisor-error path in `supervise`, `reconcile`'s settlement of orphans and vanished supervisors, `cancel`, and the capacity-queue endings in `wait_in_queue`. A helper such as `crew_outcome(store, &job)` called from each keeps it in one place; the idempotent id makes a crash between recording the status and sending the mail recoverable by reconcile.
- **Content:** `task_id` set to the job's task; body with job id, a truncated command, final status, exit code, reason and duration, plus a bounded tail of stderr/stdout (about 2 KB) or the log path; payload with the same fields structured.
- **Urgency:** `failed`, `timed_out` and `orphaned` are `high`; `succeeded` is `normal`, since the first mate usually has to verify, report or continue; `cancelled` by the ship itself is `low`, which waits for the next real wake.

### Crew asking their first mate

Crew run as the ship itself, so they cannot mail their own first mate. Add a command (*new*) for it, for example `crew.report {kind: "decision"|"progress", body}`:

- The supervisor sets `PRIVATEER_JOB=<job id>` in the payload's environment.
- `crew.report` requires an agent caller whose `PRIVATEER_JOB` names a running job with `agent_actor` equal to the caller, and writes mail from `crew` to the ship with the job's `task_id`.
- `decision` is `high` urgency and expects an answer: make it a `request` so the first mate's `reply` is tracked, and let the crew read the answer (for example `crew.answer {job_id}` or reading the reply by correlation).
- `progress` is a `low` notice.

### First-mate guidance

Extend the wake text and the generated `AGENTS.md` (`instructions` in `src/cli.rs`): crew outcomes arrive as mail from `crew` under the task's focus; check results independently rather than trusting an exit status, record judgement in the task's note, and answer crew decisions with `mail.send` kind `reply`.

## Constraints

- Facts come from the job record and its logs, never from model output.
- Exactly one outcome per job incarnation, across supervisor crashes, `reconcile` and `job.retry` (a retry is a new job and a new outcome).
- Bound everything: log tails, body sizes, and how many outcomes one wake shows (the wake text already caps its size).
- The runtime alone sends as `crew`; agents, the operator and peers cannot.

## Acceptance

- A first mate starts a crew job; when it succeeds, `serve` wakes the first mate with the outcome under the task's focus, and the scripted first mate in `tests/activation.rs` can see and acknowledge it.
- A failing crew job produces a high-urgency outcome with its exit code and log tail.
- Killing the supervisor mid-job still produces exactly one outcome after `reconcile`.
- With the captain's session aboard, the outcome reaches the session (the Claude `Stop`/`UserPromptSubmit` hook path) and no headless wake starts.
- A crew `decision` reaches the first mate; its reply is recorded and readable by the crew.
- `ship.put` refuses the name `crew`, and no caller can send mail as `crew`.
- Tests use real supervised subprocesses, as `tests/jobs.rs` and `tests/activation.rs` do.

## Implementation notes

- **One emission point, one transaction.** Every path that ends a job already changes it through `jobs::alter`, so the outcome is written there, inside the transaction that first sets a reported status. There is no window between status and mail for `reconcile` to repair; the idempotent id remains as a second guard.
- **`orphaned` is reported, once.** It is the point at which nobody supervises the job or enforces its deadline. A later settlement of the same incarnation (for example to its leader's result) does not send a second outcome; the first mate checks `job.status` for that. An orphan still holds its capacity, so its outcome names `job.cancel` for that job: the ship frees it itself, without waiting for the captain's `resource.recover`.
- **Wake rows are recorded before the wake job starts.** `serve` used to insert the `wakes` row after `agent.start` returned, so a wake job that ended at once could be reported as crew and wake its own first mate. A wake row whose job never appeared (a crash in between) is settled as failed.
- **`serve` reconciles unsupervised jobs.** Otherwise a vanished supervisor's outcome waited until someone asked for the job's status. Live supervisors are checked with a cheap process identity read and skipped; a job that cannot be inspected is reported in the pass instead of stopping the dispatcher.
- **Bounds.** The whole outcome body, header and log tails, is at most 2,000 characters (the tail gets what the header leaves, up to 1,400 bytes, stderr first), so it fits a wake's and a hook's per-message limit; the hook limit was raised from 1,500 to 2,000 characters to match the wake. The payload is not repeated in the wake text.
- **Cancellation records who asked** (`cancelled_by`), which decides `low` (the ship itself) or `normal` (the operator, or authority ending).
- **`crew.answer {id, wait_secs}`** reads a decision's state and reply; `crew.report` accepts an optional `id` so a retried report is not duplicated.
