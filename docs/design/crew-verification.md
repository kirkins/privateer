# Design: crew close their own loop

Status: round 1 **implemented** 2026-11-01 (`src/verify.rs`); the merge record and the later rounds below are not. Read with [crew outcomes](crew-outcomes.md), [crew role](crew-role.md), [crew park and resume](crew-park-resume.md) and [review gate and route strength](review-gate-and-route-strength.md).

## Problem

A first mate writes "how to verify" into a crew's assignment (`assignment:<task>`, prose, `src/cli.rs:443`). After that, the runtime does not know what verification was asked for, whether the crew ran it, at which commit, or what it showed. The crew brief (`src/plugins.rs:609`) says "do not claim a test or commit succeeded without evidence", but nothing stops a crew from claiming it anyway. The outcome mail (`settlement_text`, `src/crew.rs:432`) reports the crew's own exit status, which only says that the harness exited.

So every first mate redoes the verification itself, or runs an errand to do it, or takes the crew's report on trust. The crew role closed the authority gap: crew cannot set task status, push or merge. This proposal closes the evidence gap the same way. The runtime records the facts. The model doesn't get to report them.

## What we take from pstack

Lauren Tan's pstack (`cursor/plugins`, `pstack/`, read at c742dd6) is a set of skills and playbooks for teams of coding agents. These ideas carry over:

- **Name the evidence before work starts.** "If you can't name the evidence, the unit isn't scoped yet" (`playbooks/orchestrate.md`). Done is a predicate that can pass or fail, not a duration (`docs/guide/07-overnight.md`).
- **The checks belong to the project, not to the agent.** Each repository commits a verification skill (launch, doctor, drive, evidence, cleanup) plus a feature map. Agents run it. They don't improvise checks (`skills/create-verification-skill/SKILL.md`).
- **Never relax the predicate.** A plateau is no reason to stop, and an agent may not change the check to make it pass. The maintenance skill may edit only the verification skill's own folder, never product code, and a broken app is reported, not papered over.
- **A verdict covers one head SHA.** An old verdict doesn't carry over to a new commit.
- **Retries are bounded.** "After two retries, drop the unit and replan around it". A missing piece of evidence is a gap, and "a gap does not count as a pass".
- **Green CI is an input to a verdict, not the verdict itself.** The coordinator reads the artifact, and a reviewer from a different model family checks it.

Two parts we leave out. Continuous `/loop` sessions chasing a `/goal`: Privateer wakes on events for cost and visibility, and a crew already has a deadline (`timeout_secs`) and parks on its heavy jobs. And Cursor-specific skill files: the checks here are plain argv, so they work in every harness.

## Design

### 1. The first mate sets the checks, as data

`verify.set {"task_id": "<child>", "checks": [...]}`, run by the task's owner (or the captain) before `agent.start`:

```json
{"task_id": "fix-parser", "checks": [
  {"name": "clippy", "command": ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]},
  {"name": "parser tests", "command": ["cargo", "test", "--test", "parser"], "env": {"CARGO_TARGET_DIR": "{cwd}/target"}},
  {"name": "lint", "use": "fmt"}
]}
```

- `command` is argv, run in the crew's workspace. `env` adds variables (none starting `PRIVATEER_`), with `{cwd}` replaced by the workspace path. `heavy` defaults to true, as for `job.start`.
- `use` takes a check from the repository's manifest (section 4), and `"checks": "default"` takes the manifest's default list.
- `"none": "<why>"` records that no check applies. The crew's outcome then says so, so leaving checks out is a stated decision, not an accident.
- At most 16 checks, with unique names. Each `verify.set` starts a new revision of the task's checks (table `verify_checks`), with fresh attempts, and is audited.
- A crew cannot call `verify.set`: the crew role refuses every command it does not list.

### 2. The runtime runs them, at a recorded commit

`verify.run {"task_id": "<own>", "names": [...]?}` is for crew, on their own task:

1. It is refused when the workspace has uncommitted changes ("commit first: a verification covers one commit"), when another verification of the task is unfinished, and for a check that already failed `ATTEMPTS` (3) times under the current revision.
2. It records the run (`verify_runs`: task, revision, crew job, head, workspace, and the checks as resolved) and one `pending` row per check (`verify_results`), then starts a single supervised `test` job with id `verify-<crew job>-<n>`. That job's payload is the runtime's own runner, `privateer --home H _verify-run <id>`. A crew therefore parks once per run (`crew.park`), not once per check.
3. The runner refuses to run unless its parent process is the job's recorded supervisor, so a crew cannot run it by hand. It runs each check in turn in the workspace, prints a header for each into the job's log, and records each result (`passed` or `failed`, with exit code and times). It exits 0 only when all of them passed.

A crew never writes a result. A result is a row the runner wrote, read together with the run's job record: a check still `pending` or `running` when the job has ended is reported as `interrupted`.

### 3. The outcome reports the checks

A crew's outcome (`crew::outcome`, for `spawn` jobs) adds a verification block for the crew's final commit, from those records alone:

```
Verification at 3f2a91c04e1b: 1 of 2 passed, unverified.
  clippy passed 41.0 s
  parser tests failed exit 101 3.2 s [verify-crew-1-2]
```

Each check is `passed`, `failed`, `interrupted`, `running`, `stale` (it ran only at an earlier commit) or `not run`. A crew that `succeeded` with checks set but not all passed at its head gets `high` urgency instead of `normal`. With no checks set the block is one line, "Unverified: no checks were set for this task (verify.set)", and the urgency is unchanged. The payload carries the same state as `verification`. A verification job's own outcome, and the text a parked crew resumes with (`settlement_text`), list that run's checks. A failure to read any of this never stops the outcome itself.

The first mate still judges the work. It reads the diff, and the review gate still runs a reviewer route that didn't write the code. But it starts from facts the runtime recorded, not from what the crew says it did. `verify.status {"task_id"}` shows the same state at any time.

### 4. Repositories declare their checks

A repository can commit `.privateer-verify.json`:

```json
{"checks": {
  "fmt":    {"command": ["cargo", "fmt", "--check"], "heavy": false},
  "clippy": {"command": ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]},
  "test":   {"command": ["cargo", "test"], "env": {"CARGO_TARGET_DIR": "{cwd}/target"}}
},
"default": ["fmt", "clippy", "test"]}
```

`verify.set` reads it with `git show HEAD:.privateer-verify.json` in the charter repository, and stores the resolved argv. A crew's branch therefore cannot change what a check runs. The manifest is plain argv and environment, with no harness or forge assumption. A ship whose product is an app adds a live check that drives the real app with its own script; that is pstack's live lane, written as a command.

### 5. A bounded fix loop

A crew may fix and run again. A check that failed 3 times under the task's current revision is refused with: "check `<name>` failed 3 times for task `<task>`; report with crew.report kind decision: say what fails and what you need". This is pstack's "two retries, then replan". The count belongs to the task's check set, not to a crew incarnation, so parking or a relaunch does not reset it. Only a new `verify.set` by the first mate does.

The crew can't relax the predicate: it can't change the checks, and manifest checks were resolved when they were set. If a check itself is wrong, the crew asks its first mate.

## Not in round 1

- **Merge records cite verification.** `pr.merge` already records which review covered the merged head; it should also record the verification at that head, and say when there is none. Like the review, it would not block the merge.
- **`agent.start` takes `verify`**, so a first mate sets the checks in the same call.
- **Keeping the manifest honest.** A scheduled errand runs the manifest's checks on the protected branch and reports `clean`, `changed` or `blocked`, like pstack's `/maintain-verification-skill`. It may fix only the manifest and its scripts, never product code.
- **Feature maps.** Per-feature driving notes next to the manifest, for ships whose product has a UI, so a live check covers every way into a feature, not just the convenient one.

## Residuals

- Checks run in the crew's own workspace, not a fresh checkout, so a build cache is reused. A crew that edits files while its checks run, then commits, gets a result recorded against the earlier commit; the outcome shows it as `stale` against the new one. A change that is never committed is not caught by the head comparison.
- Under one OS account the runner's parent check, like the crew role, is a workflow control, not a security boundary: a crew could write the database directly.

## Changes by file

- `src/verify.rs` (new): tables, `verify.set`, `verify.run`, `verify.status`, the runner, and the outcome block.
- `src/main.rs`: the `_verify-run` internal command.
- `src/jobs.rs`: `start_job`, `job.start` for the runtime's own callers.
- `src/crew.rs`: the verification block and urgency in outcomes, and run results in `settlement_text`.
- `src/crew_scope.rs`: crews may call `verify.run` and `verify.status` on their own task.
- `src/plugins.rs` (crew brief) and `src/cli.rs` (first-mate guidance): run `verify.run` and park; set checks as data, not prose.

## Acceptance (`tests/verify.rs`)

Real supervised crews through `agent.start`, a real Git workspace and a fake `claude`:

1. A crew's check fails, its resume reads which one, it fixes the work, and its final outcome is `verified` with normal urgency.
2. A crew that never runs its checks is reported unverified with high urgency; its `verify.set` is refused, and so is `verify.run` with uncommitted changes.
3. After three failures `verify.run` sends the crew to its first mate, and running the runner by hand is refused.

## Open questions for the captain

1. Should `agent.start` refuse an implementation crew launched with no checks set? Round 1 allows it and reports it as unverified.
2. Should a ship keep a fallback manifest for repositories it doesn't control? Round 1 reads only the repository's own.
3. Should `pr.merge` refuse a head that isn't verified when its task set checks? The recommendation is to record only, matching the review gate.
