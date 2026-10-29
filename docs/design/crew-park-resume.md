# Design: crews park on their own heavy jobs and resume

Status: rounds 1 to 3 implemented; round 5 (PR 224 review) changes the resume path, the budget, the caps, task close and recovery as described under Rounds; round 6 makes recovery leave a resume that is still launching alone, applies the caps to `crew.resume` and held crews, and keeps task-close notices from the owner that closed the task. Task `privateer-crew-park-resume` (findings 11, 12, 14). Read with [crew outcomes](crew-outcomes.md) (the notice a first mate gets when a crew job ends) and [architecture](../architecture.md).

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
| `id` | `park-<root crew job id>-<n>` where `root` is the lineage's first crew job and `n` is the lineage's park count, so a replay is idempotent and the id does not grow with each resume |
| `crew_job_id`, `child_job_id`, `task_id`, `ship` | what it parks, and who owns the result |
| `state` | `parked`, `resuming`, `resumed`, `held` (deadline with no child), `cancelled`, `failed` |
| `next` | the one-line `next` from `crew.park` |
| `harness`, `session` | the native session to resume (`session` null when no resumable session was logged) |
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
- **Session (round 5).** The session resumes only when the resume launches a harness equal to the park's `harness` (the harness the current route would launch, compared with the one the park recorded) and the session was read from the crew's log. Otherwise it starts fresh with the handoff. The prompt for a resumed session is the job outcome. A bounded crew (`tool_bundles`) resumes its session only under the plan digest it launched with; a plan that changed while it was parked fails the resume, and the crew's outcome says why (see `docs/tool-bundles.md`).
- Otherwise (different harness or no resumable logged session) it starts a **fresh crew** whose brief is the assignment plus a runtime handoff: the `next` line, the job outcome, and the last actions from the streamed job log (the live tail from PR 208).
- **Admission (round 5).** The resume passes `wait_for_capacity: true`, so a busy host, or a stow, queues it rather than refusing. A queued resume's park is `resuming` and the crew reads `resumed` with `resumed as <job>`; the resume job itself is `queued` until admitted. A refusal other than a stow fails the crew. (This replaces the round-3 rule that a stowed resume leaves the crew `parked`: the stowed resume is now recorded and queued, and nothing launches while stowed.)
- **Memory (round 5).** A resume on the same route reuses the crew's recorded estimate (`_resume_estimate`, honoured only for the runtime's own resume), so the route's memory figure is not lost when the launch record strips it. A fresh crew on another route is estimated from that route.
- Resume job ids are `crew-resume-<park id>-<attempt>`, so a restart that re-runs the step reuses the same id.
- **Recovery at boot and on each tick (round 5, round 6).** A `parked` park whose child is already final is resumed. A `resuming` park whose resumed job exists is `resumed`. Whoever claims a park as `resuming` (the child's supervisor, `crew.resume`, or recovery's own retry) records its process identity on the park row (`claimer`), because `agent.start` may wait (for the workspace lock or a workspace, up to its queue wait) before it writes the resume job. A `resuming` park with no resumed job is left alone while its claimer still runs (checked by process identity, as for supervisors) and the claim is younger than the longest launch wait plus 15 minutes. Once the claimer is gone (it died mid-launch) or overdue, the resume is retried **once** under the same resume id; if that also leaves no job, the park is `failed` and the crew gets a failed notice with the reason.

Resume linking is monotonic: only a park still in `resuming` accepts the
launcher's link write. Recovery can see the new job running and mark the park
`resumed` before `agent.start` returns an older `starting` or `queued` response
to its launcher. That response must not regress the park to `resuming`, even
when the new crew has already parked again. Delayed linking also leaves
cancelled or failed parks closed; no extra dispatcher tick is needed to repair
a stale response. When the guarded link changes no row, the launcher rereads
its park: only an already `resumed` park linked to that same job accepts the
late response. Otherwise it audits the late job's park and crew, cancels the
job through native cancellation, and does not finish the old crew as resumed.
Recovery uses the same check so a park closed since its scan cannot orphan a
continuation either. The cancellation intent is committed with its audit in
`crew_resume_cancels`, bound to the late job's incarnation. Native cancellation
errors leave that intent for the next dispatcher pass, including while stowed
and after restart; success or a missing/replaced incarnation retires it. The
retry does not rely on audit retention or reopen the closed park.

### Deadline

- Each incarnation gets a **fresh budget**: its supervisor deadline is `spawn + job_timeout_secs` (the charter's value), counted from the payload spawn. Queued time (`wait_in_queue`) and parked time are not charged, so a crew's own waits are not in its deadline. A budget overrun still times out as today.
- `run_ms` is still summed across incarnations on the crew's job record (not the park row), written at each payload exit (park, final, or kill). A resume carries it into the new job through the runtime's own `_run_ms`, which only an operator-side caller can set. It is for reporting and bearings only and never shortens a deadline.
- The total a crew can run is bounded by the caps below, not by the budget: a deadline park is only taken while the caps allow another resume.
- Caps: 6 parks and 24 h elapsed in total, both configurable. They are checked when a park opens, at the deadline (before the park is chosen), by `crew.resume` (which refuses a spent crew and says why), and on each tick while a crew is parked or held. A spent cap ends the crew: its waiting child is cancelled, and the crew ends `cancelled` (while parked or held, with a normal cancelled notice to its first mate) or `timed_out` with the reason (at the deadline), never as a silent stall.
- **The real bound.** A crew runs at most 7 times: the first run and one resume per park, with at most 6 parks. Its elapsed time is at most the 24 h cap plus one `job_timeout_secs`: the elapsed cap is checked when a crew would park or resume and while it waits, but a crew that is running is stopped at its next deadline, not at the moment the 24 h pass.

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

Closing a task (`done` or `cancelled`) ends its parked and held crews as `cancelled`, with the reason `task <id> is <status>`, and cancels the heavy job each one waits on. Only the task's owner can close it, and it already knows, so neither cancellation is mailed to it (round 6): both jobs are marked `quiet` before their status changes, and `crew::outcome` writes nothing for a quiet job. A crew ended by a spent cap is not quiet; its first mate gets the cancelled notice with the reason. The park closes before the child is cancelled, so the child's settlement does not resume the crew.

### Park grace (round 5)

After `crew.park`, the supervisor gives the harness up to 5 seconds to end its own turn before it signals the payload. A payload that exits in that window is not signalled at all; one that does not is stopped as before. During the grace the crew job still reads `running`, with its park already open, so the task's launch refusal and bearings read the park row (state `parked` or `held`), not the job status. A task closed during the grace ends the crew through the running-job cancel path; it is marked quiet first, so its owner gets no cancelled notice then either.

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
3. **Hosted crews (resolved).** Hosted crews have supervised payloads and event logs now. They park and resume through the same path; see Hosted crews below.
4. **Duplicate-start rule.** Proposed: the child's outcome notice is suppressed while its crew is parked or resuming on it, and sent when the crew is `held`. Confirm.

## Decision: a fresh budget per incarnation (round 5)

The round 5 review asked for one budget summed across resumes. The first mate chose the per-incarnation grant instead (option A): a resume of a deadline park, and any resume, gets a fresh `job_timeout_secs`. A summed budget leaves a crew that parked at its deadline with no run time to resume on, so the resume is held at once and a crew that was meant to continue cannot. The 6-park and 24 h caps bound the total (see The real bound under Deadline), and they are checked at the deadline, by `crew.resume`, and while parked or held. `run_ms` is still recorded across incarnations for reporting.

## Hosted crews

A hosted crew can use `crew.park` just like a detached crew. The record opens
while its native session is still alive, and the supervisor gives it the same
five-second grace to end its turn, then stops its process group if necessary.
The crew remains unfinished, without a final outcome notice. Once the payload
has stopped and its event log has drained, the child's settlement claims the
park once and relaunches through `agent.start`, in a new hosted pane when the
host is available. The logged session resumes when compatible; otherwise the
assignment and outcome seed a fresh crew. This does not inject a prompt into
an arbitrary live terminal or change provider routes. A first mate's own
session still cannot park.

The old hosted refusal assumed the pane kept a session outside the park's
custody. Starting another session against it would risk concurrent agents
using the same session and checkout. The job supervisor now owns that lifetime:
settlement waits while the crew is still running, and additionally verifies
that its payload group and recorded escaped descendants are gone before
claiming even a `parked` record. Killing a wrapper, or writing `parked`, is not
proof the payload ended. Recovery uses the existing atomic park claim and
in-flight launch identity, so repeated outcomes and dispatcher passes cannot
launch a second resume.

If a hosted session exits without explicitly parking while its lineage has a
starting, queued, launching or running job, the supervisor opens a park on that
job instead of reporting success or failure. If the selected child settles
between the scan and park, its outcome still resumes the crew. An ordinary exit
with no outstanding work finishes normally; a deadline with none is held as
for detached crews. Cancellation and lost authority do not auto-park. A spent
cap cancels the selected child and ends the crew as `timed_out` with the cap
reason. Other auto-park errors are audited as `crew.park_failed` and preserve
the harness's original success or failure outcome, rather than mislabelling an
operational error as a timeout. Park notices and outcome suppression are the
same as for detached crews; only the eventual resumed crew's final outcome
reaches the first mate.

`crew.resume` also checks the old payload before claiming a held crew. Its
response includes `skipped` entries with the crew and park ids and a reason
when a live payload or descendant prevents a resume; retry after it stops.

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
- Hosted crews support `crew.park`, including the crew-only `job.start` park hint; see Hosted crews below.
- Round 2 is done in commit d8f7b6d: the deadline-kill park, `held` crews and `crew.resume`, `ship.bearings` listing parks, and the `agent.start` refusal on a parked task (`replace:true` cancels the crew and its jobs).
- The budget is a fresh `job_timeout_secs` per incarnation, as round 2 granted. The round 5 proposal to sum it across resumes was not taken; the first mate chose option A (see Decision). `run_ms` is kept for reporting.
- Restart recovery (Recovery at boot): a park whose child settled while the dispatcher was down resumes once, on the next tick. Tested by `a_park_survives_a_dispatcher_restart_and_resumes_once`.
- Round 5 (PR 224 review): the resume route policy, `wait_for_capacity` on resume, the recorded memory estimate, the per-incarnation budget (the first mate's option A), the caps at the deadline and while parked, task close, park grace, the `resuming` recovery (one retry under the same id), the bearings park count, the crew-only brief wording and the identical-repeat rule are as described above. Tests: `tests/crew_park.rs`, one per behaviour listed in the round 5 assignment.
- Round 6 (PR 224 review): the park row records its `claimer` (a column added to existing homes at open), and recovery retries a `resuming` park only once its claimer is gone or overdue (`recovery_leaves_a_resume_whose_launch_is_still_in_flight` holds the workspace lock so the claimer's launch waits); `crew.resume` checks the caps (`crew_resume_is_refused_once_the_crews_caps_are_spent`); the per-tick cap check covers held crews (`a_held_crew_past_its_elapsed_cap_ends`); an owner's task close mails no cancelled notice (`closing_the_task_ends_a_parked_crew_and_its_job_quietly`).
