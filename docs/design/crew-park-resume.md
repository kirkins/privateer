# Design: crews park on their own heavy jobs and resume

Status: rounds 1 to 3 implemented; round 5 (PR 224 review) changes the resume path, the budget, the caps, task close and recovery as described under Rounds. Round 3 added the tests for parked time, dispatcher restart, stow-held resume and the incident replay: a stow-held resume waits as `parked` instead of failing, and the next tick resumes it once the stow is lifted. Task `privateer-crew-park-resume` (findings 11, 12, 14). Read with [crew outcomes](crew-outcomes.md) (the notice a first mate gets when a crew job ends) and [architecture](../architecture.md).

## Problem

Three behaviours fight each other:

- **Wrong sender in the brief.** The crew brief (`src/plugins.rs` around line 370) says a job outcome "arrives as mail from crew", but `crew::outcome` (`src/crew.rs:96`) mails the first mate. A crew that starts a heavy job therefore has no way to learn the outcome except by polling.
- **No wait primitive.** There is no `job.wait`, so a crew sleeps and polls while its own process clock runs.
- **Deadline counts waiting.** A crew is a supervised job whose `timeout_secs` is the charter's `job_timeout_secs` (3600 here). `supervise_inner` (`src/jobs.rs` around 2722) sets `deadline = now + timeout_secs` when the payload spawns, so a crew that is polling a 40-minute build is killed at 60 minutes, and the first mate is told only that it timed out. The incident replay: the heavy job ends after the crew deadline, and the first mate starts a duplicate.

## Model

A park is a new durable record, not a `task.defer` wait and not a job state. The crew job's payload exits; the crew job stays unfinished in state `parked`; the child heavy job keeps running (or is already done). When the child reaches a final status, the runtime resumes the crew.

### Park record

New table `crew_parks` (in the same transaction style as `wakes`):

| column | meaning |
|---|---|
| `id` | `park-<crew job id>-<n>` where `n` is the park count, so a replay is idempotent |
| `crew_job_id`, `child_job_id`, `task_id`, `ship` | what it parks, and who owns the result |
| `state` | `parked`, `resuming`, `resumed`, `held` (deadline with no child), `cancelled`, `failed` |
| `next` | the one-line `next` from `crew.park` |
| `harness`, `session` | the native session to resume (`session` null for Pi ephemeral, a terminal pane, or hosted crews without a log) |
| `parks`, `created_ms`, `updated_ms` | cap accounting and recovery |

Counts toward caps: `parks` is compared with the park cap (6), `created_ms` with the elapsed cap (24 h). Both are configurable through charter/settings with bounds, like the existing `job_timeout_secs` (`src/core.rs:121`).

### `crew.park`

`crew.park {"job_id": "<own unfinished job>", "next": "<one line>"}`

- Caller must be the agent for the job's ship; the job must be a crew job (`action` spawn, `agent_actor` = ship, not a first-mate wake) that is still `running`.
- The child must have `started_by` equal to the crew job id, and must still be unfinished. Check and write happen in one transaction; if the child has already finished, refuse with "already finished" and the crew reads its outcome instead of parking. Refusal is not an error that loses the outcome, because the outcome notice is written on the child's final status regardless.
- The crew job's status becomes `parked`. Its supervisor sees the payload exit and records `parked` (non-final), not `succeeded`. Payload exit with an unparked crew is unchanged.
- Parking releases the crew's memory reservation (`crew_memory` in `src/admission.rs`, from PR 214). The workspace lease is kept and counted in the pool budget, same rule as a running crew. (Pool budget PR 201 was not checked in this read; the implementer should confirm the hook.)
- The child is not cancelled on park. It is cancelled only by the rules in "Heavy jobs of a parked crew" below.

### Resume

Trigger: the final settlement of a child job, at the same place `crew_outcome` is called (end of `supervise_inner`, the supervisor-error path, `reconcile`, `cancel`, the queue endings in `wait_in_queue`). A helper `crew_resume_check(store, &child)` moves the park `parked → resuming` in a transaction, then starts the resume.

- Resume is a new launch. It goes through `agent.start` admission and the stow hold, and never bypasses the captain's routes. It is launched as the runtime (operator), so the route check that an agent start runs is repeated here (round 5).
- **Route policy (round 5).** The parked route is still valid when the captain lists it for the crew's ship (or lists no crew routes) and it is not backing off (`crew_routes::resume_route`). If it is valid, the launch is the parked one. If not, the resume is a **fresh crew** on the first available route (`crew_routes::choose`, through `agent.start`'s own choice), with the handoff; no session is carried to a route the captain no longer offers.
- **Session (round 5).** The session resumes only when the resume launches a harness equal to the park's `harness` (the harness the current route would launch, compared with the one the park recorded) and the session was read from the crew's log. Otherwise it starts fresh with the handoff. The prompt for a resumed session is the job outcome.
- Otherwise (different harness, no session, Pi ephemeral, a terminal pane, hosted crew) it starts a **fresh crew** whose brief is the assignment plus a runtime handoff: the `next` line, the job outcome, and the last actions from the streamed job log (the live tail from PR 208).
- **Admission (round 5).** The resume passes `wait_for_capacity: true`, so a busy host, or a stow, queues it rather than refusing. A queued resume's park is `resuming` and the crew reads `resumed` with `resumed as <job>`; the resume job itself is `queued` until admitted. A refusal other than a stow fails the crew. (This replaces the round-3 rule that a stowed resume leaves the crew `parked`: the stowed resume is now recorded and queued, and nothing launches while stowed.)
- **Memory (round 5).** A resume on the same route reuses the crew's recorded estimate (`_resume_estimate`, honoured only for the runtime's own resume), so the route's memory figure is not lost when the launch record strips it. A fresh crew on another route is estimated from that route.
- Resume job ids are `crew-resume-<park id>-<attempt>`, so a restart that re-runs the step reuses the same id.
- **Recovery at boot and on each tick (round 5).** A `parked` park whose child is already final is resumed. A `resuming` park whose resumed job exists is `resumed`. A `resuming` park with no resumed job (the process died mid-launch) is retried **once** under the same resume id; if that also leaves no job, the park is `failed` and the crew gets a failed notice with the reason.

### Deadline

- Each incarnation gets a **fresh budget**: its supervisor deadline is `spawn + job_timeout_secs` (the charter's value), counted from the payload spawn. Queued time (`wait_in_queue`) and parked time are not charged, so a crew's own waits are not in its deadline. A budget overrun still times out as today.
- `run_ms` is still summed across incarnations on the crew's job record (not the park row), written at each payload exit (park, final, or kill). A resume carries it into the new job through the runtime's own `_run_ms`, which only an operator-side caller can set. It is for reporting and bearings only and never shortens a deadline.
- The total a crew can run is bounded by the caps below, not by the budget: a deadline park is only taken while the caps allow another resume.
- Caps: 6 parks and 24 h elapsed in total, both configurable. They are checked when a park opens, at the deadline (before the park is chosen), and on each tick while a crew is parked. A spent cap ends the crew: its waiting child is cancelled, and the crew ends `cancelled` (while parked) or `timed_out` with the reason (at the deadline), never as a silent stall.

### Deadline kill

When the crew deadline is reached:

1. If the crew has an unfinished child job, the kill becomes a park on that child (state `parked`). No process is killed for waiting.
2. Otherwise the runtime writes the handoff (as above), marks the park `held`, keeps the session, and sends the first mate one notice. The first mate resumes it explicitly (see open question 2).

### Heavy jobs of a parked crew

Child jobs stay alive while the crew will read their result. They are cancelled when:

- the task closes,
- the first mate cancels the crew,
- the park cap or elapsed cap runs out (the park becomes `cancelled`).

Cancelling uses the existing cancel path, so the wrapper and its payload are checked by process identity, not by the wrapper's exit.

### Task close (round 5)

Closing a task (`done` or `cancelled`) ends its parked and held crews quietly as `cancelled`, with the reason `task <id> is <status>`, and cancels the heavy job each one waits on. No crew-failed notice is sent. The park closes before the child is cancelled, so the child's settlement does not resume the crew.

### Park grace (round 5)

After `crew.park`, the supervisor gives the harness up to 5 seconds to end its own turn before it signals the payload. A payload that exits in that window is not signalled at all; one that does not is stopped as before. During the grace the crew job still reads `running`, with its park already open, so the task's launch refusal and bearings read the park row (state `parked` or `held`), not the job status. A task closed during the grace ends the crew through the running-job cancel path, so its first mate may get the usual cancelled notice for that crew.

### Notices and visibility

- Park: a **low** notice from `crew` ("parked, will resume when X finishes"). It does not wake the first mate.
- Failed resume: a normal notice with the failure reason. Resume that is held by a stow is not a failure.
- Child final with the crew resumed: no notice from the child's outcome, since the crew reads it. The existing outcome notice for the child is still written for the first mate only when the crew is not parked on it (the duplicate-start fix).
- `ship.bearings` lists parked crews, the child job each waits on, `next`, and the park count (`park N of 6`).
- The crew brief says where a heavy job's outcome goes for a crew only: a crew parks with `crew.park` and its outcome arrives as its resume prompt. A first mate's brief keeps "its outcome goes to your first mate".
- An identical repeated `agent.start` (same id and request) for a task whose crew is already parked returns the existing job; the parked-task refusal applies only to a new launch.

### agent.start on a parked task

`agent.start` on a task that has a park in `parked`, `resuming` or `held` is refused, naming the crew and the child. With `replace: true` it cancels the park and its child jobs first, in one transaction, and then starts. Never run alongside a parked crew.

## Changes by file

- `src/plugins.rs` (crew brief, ~370): say "park with crew.park on a heavy job, then end your turn; the outcome arrives as your resume prompt". Remove "mail from crew" for the job outcome.
- `src/jobs.rs`: `parked` status; supervisor treats a payload exit on a parked crew as non-final; deadline accounting (`run_ms`); the deadline-kill branch; the job.start reply text; settlement hook for resume; `crew_resume_check`.
- `src/crew.rs`: `crew.park` command; `outcome` skips jobs whose child is parked on a crew; park/resume notices.
- `src/admission.rs`: release memory on park, re-admit on resume.
- `src/activation.rs` / `src/errand.rs`: reuse session read and resume; the handoff builder.
- `src/jobs.rs` (or a new `src/parks.rs`): `crew_parks` table, created alongside the `jobs` schema; the table is created the same way `wakes` is (`CREATE TABLE IF NOT EXISTS`, `src/activation.rs:45`).
- `src/context.rs:287` (`bearings`) and `ship.bearings` dispatch: parked list.

## Test plan

Fake harness that prints a session id and logs its arguments, in isolated homes (HOME, PRIVATEER_FLEET, XDG).

1. Park then resume: the resume gets the same session and the outcome as its prompt.
2. Queued and parked time are not charged; a budget overrun still times out.
3. Deadline kill parks when a child runs; otherwise it writes a handoff and holds the session.
4. No session: a fresh crew whose brief includes the handoff (park line, outcome, last actions).
5. Cancel or task close stops child jobs (and checks the process, not the wrapper).
6. A park survives a dispatcher restart; a resume whose start did not land is retried once under the same id.
7. A resume held by a stow goes through admission.
8. `agent.start` is refused on a parked task; `replace: true` cancels the park and its jobs.
9. Incident replay: a heavy job that ends after the crew deadline resumes the crew, and the first mate receives no duplicate start.

Tests prove behaviour through commands and durable state, not snapshots of layout.

## Open questions

1. **Park cap defaults and where they live.** Proposed: charter keys `crew_max_parks` (6) and `crew_max_elapsed_secs` (86400), bounded like `job_timeout_secs`. Confirm the names.
2. **How a first mate resumes a `held` crew.** The command set has no resume. Proposal: `crew.resume {"task_id": ...}`, a first-mate command that goes through the same admission path as the runtime resume. Needs an explicit decision because it adds a command.
3. **Hosted crews.** They have no log and no session in this runtime, so a park on a hosted crew can only resume as a fresh crew with the handoff. Confirm that is acceptable or that hosted crews should be refused `crew.park` entirely.
4. **Duplicate-start rule.** Proposed: the child's outcome notice is suppressed while its crew is parked or resuming on it, and sent when the crew is `held`. Confirm.

## Decision: a fresh budget per incarnation (round 5)

The round 5 review asked for one budget summed across resumes. The captain chose the per-incarnation grant instead (option A): a resume of a deadline park, and any resume, gets a fresh `job_timeout_secs`. A summed budget leaves a crew that parked at its deadline with no run time to resume on, so the resume is held at once and a crew that was meant to continue cannot. The 6-park and 24 h caps bound the total, and they are checked at the deadline and while parked. `run_ms` is still recorded across incarnations for reporting.

## Rounds

Per the crew assignment: round 1 is `crew.park`, the resume path and the deadline accounting with their tests; round 2 is the deadline-kill park, heavy job policy, bearings and the `agent.start` refusal. Each round is committed only after `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the affected tests (`--test-threads=1`) pass as supervised heavy jobs.

## Implementation notes

Decisions made while implementing round 1, where the text above was not exact:

- `crew.park` takes the **child** job id as `job_id` (the heavy job the crew started) and the crew is the caller's `PRIVATEER_JOB`. The child must have been started by that crew or a crew it resumed.
- A parked crew is job status `parked`: non-terminal, with no payload. Its park is named by the job's `park` field, and the park row is `crew_parks`.
- A resumed crew is a new job `crew-resume-<park>-<attempt>`. The parked record ends as status `resumed` (terminal, not reported), so it reports nothing and its work is found through the park's `resumed_job_id`.
- Child lineage: a child started by a resumed crew counts as the original crew's, through `crew_parks.resumed_job_id`.
- Outcome suppression: a child's outcome is not mailed while a crew with status `parked` is parked on it. Its resume reads the outcome. A failed resume is a normal `failed` notice on the crew, carrying the resume error.
- Resume runs in the supervisor's settlement hook (`supervise`), after the child's final status commits, through `agent.start`. The launch arguments (manifest, options, config, tool bundles, cwd, heavy) are recorded on the crew's job when it starts.
- The native session is read from the crew's complete log when it resumes, not at `crew.park`: the park is finalised after the payload's output is captured, so that log is whole only then.
- The job.start reply to a crew carries a `park` hint, since that is where the crew learns to park rather than poll; the crew brief only names `crew.park`, to stay within its size budget.
- Hosted crews are refused `crew.park` for now.
- Round 2 is done in commit d8f7b6d: the deadline-kill park, `held` crews and `crew.resume`, `ship.bearings` listing parks, and the `agent.start` refusal on a parked task (`replace:true` cancels the crew and its jobs).
- The budget is a fresh `job_timeout_secs` per incarnation, as round 2 granted. The round 5 proposal to sum it across resumes was rejected by the captain (see Decision); `run_ms` is kept for reporting.
- Restart recovery (Recovery at boot): a park whose child settled while the dispatcher was down resumes once, on the next tick. Tested by `a_park_survives_a_dispatcher_restart_and_resumes_once`.
- A stow holds a resume like any launch. While stowed, `resume_pending` takes no park, and a resume refused by the stow leaves its park `parked` rather than failing the crew. Tested by `a_resume_held_by_a_stow_goes_through_admission`.
- Round 5 (PR 224 review): the resume route policy, `wait_for_capacity` on resume, the recorded memory estimate, the per-incarnation budget (the captain's option A), the caps at the deadline and while parked, task close, park grace, the `resuming` recovery (one retry under the same id), the bearings park count, the crew-only brief wording and the identical-repeat rule are as described above. Tests: `tests/crew_park.rs`, one per behaviour listed in the round 5 assignment.
