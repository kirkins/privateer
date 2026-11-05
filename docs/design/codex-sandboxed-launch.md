# Design: sandboxed callers launch through the dispatcher

Status (2026-11-05): contract only, for review. Task `privateer-codex-sandbox-launch-736`, issue #736 (part of #649). This replaces the sketch in part (b) of [sandboxed callers](codex-sandboxed-callers.md) and builds on its part (a), caller identity from kernel ancestry. Read it with [the crew role](crew-role.md).

## Problem

A native Codex first mate runs its Privateer CLI calls inside Codex's `workspace-write` sandbox. Mail and reads work there. Starting work does not:

- **Linux (bwrap).** The sandbox has its own PID namespace with its own `/proc`. `ensure_dispatcher_namespace` (`src/jobs.rs`) refuses `job.start`, `job.retry`, `agent.start` (and so `errand.start`, `review.start` and `crew.resume`), `verify.run` and the `workspace.*` commands that allocate, because a record made there names PIDs nobody outside can inspect, settle or signal.
- **macOS (Seatbelt).** Every child of a sandboxed CLI inherits the sandbox. A supervisor the CLI spawned would run its job (a build, or a crew's harness) under the first mate's profile. That job could not inspect or signal its own processes (`ps` is denied, and `(allow signal (target same-sandbox))` limits signals), and `job.cancel` from the sandbox could not signal a payload outside it.

The captain's requirement for qualification: on each OS, a real errand, crew and heavy job started from inside the sandbox must run, report their outcome and be cancellable.

## Summary of the contract

The sandboxed CLI does not run a launch command itself. It writes the command into a durable `launch_requests` row in the home, then **execs itself** as a waiter (`privateer --home H _launch-wait ID`). The exec keeps its PID and start time and puts the request id in its argv, which binds the row to the one process that wrote it. The home's resident dispatcher, outside the sandbox, starts one launcher process (`_launch-run ID`) per request. The launcher finds the requester from outside, checks the argv binding, resolves the requester's caller with the same ancestry rule a direct call uses (now with nothing hidden), and then runs the same `dispatch(store, command, args)` a direct call runs, as that caller. The waiter prints that command's answer, or its error, exactly as a direct call would.

Choosing this path is not a security decision. Any caller may take it. The dispatcher decides who the caller is, from the kernel and the records, never from the row.

## (a) Which commands go through the dispatcher

### Detecting a sandboxed caller

| OS | sandboxed when | how |
|---|---|---|
| Linux | the CLI's PID namespace differs from the one the home's dispatcher recorded | `metadata.pid_namespace` against `readlink /proc/self/ns/pid`. This is exactly the condition `ensure_dispatcher_namespace` refuses today. Before any dispatcher has run, nothing is recorded and the CLI starts work directly, as today. |
| macOS | the process is in a Seatbelt sandbox | `sandbox_check(getpid(), NULL, SANDBOX_FILTER_NONE)` from libsystem_sandbox (part of libSystem), looked up with `dlsym` so a missing symbol means "not sandboxed". `CODEX_SANDBOX=seatbelt`, which Codex sets for its sandboxed commands, also counts. |

A false positive costs one round trip through the dispatcher and gives the same answer. A false negative is today's behaviour. So the detection may be generous. Tests force the route with `PRIVATEER_LAUNCH_VIA=dispatcher`. That variable can only route a call to the dispatcher. It can never route one away from it.

If the sandboxed CLI's own caller resolution fails because of a sandbox limit (the two refusals of part (a) of the callers document: kernel walk refused, or a hidden Privateer ancestor no record accounts for), a forwardable command is still forwarded. The launcher resolves from outside, where the ancestry is visible. Any other resolution refusal, such as a role drop, ends the call in the sandbox as it does today.

### The commands

| command | from a sandboxed caller | why |
|---|---|---|
| `job.start`, `job.retry` | dispatcher | spawns a supervisor; admission writes the host ledger |
| `agent.start`, `errand.start`, `review.start`, `crew.resume` | dispatcher | each ends in `agent.start`: plugin planning, workspace allocation, a supervisor. `review.start` also runs `gh` outside the sandbox, as it does for an unsandboxed first mate |
| `verify.run` | dispatcher | `jobs::start_job` |
| `job.cancel` | dispatcher | may signal a payload group (orphaned job, macOS) |
| `workspace.acquire`, `workspace.recover`, `workspace.release` | dispatcher | Treehouse leases, the disk queue (which records its waiting process's identity), `workspace_idle` liveness checks |
| `resource.status` | dispatcher | reads supervisor and payload liveness (`ps` on macOS; an unaddressable PID reads as alive on Linux) |
| `job.status`, `job.list`, `workspace.status`, `workspace.list`, `crew.park`, `crew.report`, `crew.answer`, `errand.list`, `review.list`, `verify.status`, mail, notes, tasks, `wake.*` reads | unchanged | records only (see (d)) |
| deployment admission (`deploy_nonproduction`), `resource.recover`, `resource.recovery.*`, `upgrade.*`, `privileged.*`, `serve`, `service` | stay refused in a PID sandbox, with the sandbox-limit text of (e) | operator custody and production authority stay outside the sandbox. Nothing here is forwarded |
| any forwardable command over `rpc` | refused with the sandbox-limit text, naming the CLI | the binding below needs a process that can exec into the waiter. A long-lived RPC server cannot |

`pr.merge` is not a launch and is unchanged: it works or fails as the sandbox's network allows.

## (b) The request

### The row

A new table in the home's `privateer.sqlite`, which the sandbox can already write (mail works):

```sql
CREATE TABLE IF NOT EXISTS launch_requests(
  id TEXT PRIMARY KEY,          -- a fresh UUID
  command TEXT NOT NULL,
  args TEXT NOT NULL,           -- JSON, with ids filled in (below)
  requester TEXT NOT NULL,      -- JSON {pid, started, pid_namespace, uid}, as the requester sees itself
  claimed TEXT NOT NULL,        -- JSON {role, actor, env_job, sandbox:"pid_namespace"|"seatbelt"|"forced"}
  state TEXT NOT NULL,          -- recorded|pending|claimed|done|failed|refused|expired
  created_ms INTEGER NOT NULL,
  deadline_ms INTEGER NOT NULL, -- created_ms + the claim window
  launcher TEXT,                -- JSON process identity of the _launch-run that claimed it
  resolved TEXT,                -- JSON {actor|operator, job, crew}: what the launcher proved
  outcome TEXT,                 -- the command's answer, verbatim (done)
  error TEXT,                   -- the command's error or the refusal text (failed|refused|expired)
  claimed_ms INTEGER, finished_ms INTEGER);
```

- `requester.started` is the format `jobs::identity` records: `linux:BOOT:TICKS` on Linux. On macOS it is the `lstart` string the kernel walk formats. The probe for this contract (Arch Linux, bwrap `--unshare-user --unshare-pid --proc /proc`) showed that the start ticks and the boot id read inside the sandbox equal those read from outside for the same process.
- `claimed` is what a direct call would have taken from the environment and flags (`PRIVATEER_ROLE`, `PRIVATEER_ACTOR`, `PRIVATEER_JOB`). It is input to the same resolution rule, never a result.
- **Ids are filled in before the row is written.** `job.start` and `agent.start` get an `id`, and `job.retry` gets a `new_id`, when the caller gave none. A direct call would draw the same kind of UUID. The ids then exist before the launch, so an outcome the waiter could not read (below) can still be found with `job.status`. A repeated id replays the existing job, as it does today.
- **Bounds.** At most 16 rows per home in `recorded`, `pending` or `claimed`. A further request is refused ("too many launch requests are in flight"). The dispatcher deletes finished rows older than 7 days, and keeps at most 1,000.

### The sequence

1. **Record (sandbox).** The CLI runs `check_dispatch` (argument checks and `crew_scope::check`) as a quick pre-check, then inserts the row in state `recorded`.
2. **Bind (sandbox).** The CLI execs `privateer --home H _launch-wait ID` with the same environment. The PID and start time stay the same. The argv now names the request. The waiter reads the row, requires that its own pid, namespace and start equal `requester`, and moves the row from `recorded` to `pending` (compare and set). The dispatcher claims only `pending` rows, so it never reads the requester's argv before the exec. If the exec fails, the CLI marks the row `failed` itself and reports the error.
3. **Wait for a claim (sandbox), bounded.** If no resident dispatcher holds `H/serve.lock` (`service::running`, a `flock` test, which works across PID namespaces and inside Seatbelt), the waiter withdraws the row at once (`pending` to `expired`, compare and set) and refuses. Otherwise it polls the row every 100 ms until the claim window ends. The window is 30 s, long enough to cover a dispatcher restart. When the window ends, the waiter withdraws the row with the same compare and set. If the dispatcher won the race and claimed it first, the compare and set fails and the waiter goes on to step 6. A withdrawn row is never launched later.
4. **Start a launcher (dispatcher).** The resident dispatcher runs a launcher thread with its own connection. The thread does not wait for the tick, which can take tens of seconds with mail and machine work. Every 250 ms it starts `privateer --home H _launch-run ID`, detached (`setsid`, no stdio), for each `pending` row that is not past its deadline. At most 4 launchers run per home. A `pending` row past its deadline becomes `expired`. Only a resident `serve` starts launchers. `serve --once` does not.
5. **Claim, identify and run (launcher, outside the sandbox).**
   1. Refuse to run if the launcher itself is outside the dispatcher's PID namespace or inside a Seatbelt sandbox (`ensure_dispatcher_namespace`, `sandbox_check`). A launcher must never start work into a sandbox.
   2. Claim the row: `pending` to `claimed`, with `launcher` set to its own identity, compare and set, and only before `deadline_ms`. Exactly one launcher wins.
   3. **Find the requester.**
      - Linux: if `requester.pid_namespace` is the launcher's own, the PID is the same. Otherwise scan `/proc/[0-9]*` for the process whose `readlink /proc/P/ns/pid` equals the recorded namespace and whose `NSpid:` line in `/proc/P/status` ends in the recorded PID. Exactly one must match. The probe confirmed that bwrap's inner PID and namespace inode can be read from outside like this.
      - macOS: the same PID. The launcher is outside the sandbox and sees the same PIDs.
   4. **Check that it is the process that wrote this row.**
      - Its start identity equals `requester.started` and it is not a zombie or exiting.
      - Its real uid is the launcher's.
      - Its argv after `argv[0]` is exactly `--home H _launch-wait ID` for this home and this row. On Linux this comes from `/proc/P/cmdline`. On macOS it comes from `kern.procargs2`, which an unsandboxed process can read for any process of its own user. Another user's argv is hidden, so the check fails and the uid rule holds there too.
      - Any mismatch, or a requester that has gone, marks the row `refused` with the reason. Nothing is started.
   5. **Resolve the caller from outside** with the code a direct call uses: `crew_scope::resolve_from(pid, Some(H), claimed_caller, claimed_env_job)`. This is today's `resolve`, starting its walk at the requester's PID instead of `std::process::id()`:
      - The Linux `/proc` walk gains a starting PID.
      - The macOS `ancestry::walk` already takes one, and from outside every same-user argv is visible.
      - The walk now crosses the sandbox boundary. On Linux it reaches bwrap, Codex and then the wake's or crew's `_job-supervise`, which the sandboxed CLI could never see.

      The result is the same as a direct call would get: crew, first mate or captain's job, with the same role-drop and borrowed-job refusals. The refusals are audited and noticed as today. The launcher sets the process's resolved job and crew (`RESOLVED`) from this. So `started_by`, `crew_scope::job_id()`, `Store::crew()`, `crew::may_park` and `activation::mark_contact` see the requester, not the dispatcher. `_launch-run` never resolves its own ancestry: it is under the dispatcher, which is the operator.
   6. **Refuse what a sandbox must keep contained.** If the proven job is a review errand (`review::is_review_errand`) or a read-only launch (`codex_read_only_crew`, or launch options `sandbox: "read-only"`), the row is `refused`. Those jobs keep their containment. A Codex read-only sandbox cannot write the home at all, so it cannot even write the row; this check is defence in depth.
   7. **Run.** Open the store as the resolved caller and run `crate::dispatch(store, command, args)`. That repeats `check_dispatch` and `crew_scope::check` as the authority. It then runs charter authorization, admission (capacity, `heavy`, `wait_for_capacity`, `max_wait_secs`, the disk floor, routes) and the transactional start in `admit_and_launch`, unchanged. The supervisor is spawned from the launcher: in the dispatcher's namespace, outside any sandbox, detached with `setsid` as today. Audit `launch.forwarded` with the row id, command, requester and resolved caller.
   8. **Record the result.** On success, `outcome` is the answer and the state is `done`. On error, the state is `failed` and `error` is the command's `{error:#}`. This is a second write after the command's own transactions commit. It is not in the same transaction as the job. The pre-filled ids cover the gap.
6. **Answer (sandbox).** Once the row is claimed, the waiter waits for its outcome with no deadline of its own. The command's own bounds (`max_wait_secs`, admission) apply, as they do to a direct call that blocks. It prints `outcome` verbatim, or prints `error` the way `main` prints an error and exits nonzero. So `job.start` returns the same job record, `park` hint included. `errand.start` and `review.start` return their usual shapes, and a refusal reads as it does directly. Each poll also checks that the launcher's recorded identity is still alive. A launcher gone without a result ends the wait with an error naming the request and the pre-filled ids: "the dispatcher's launcher stopped before recording the outcome of request ID. Job J may have started: read it with job.status."

The waiter is not needed after the claim. If Codex kills it (a tool timeout), the launch goes on and the row keeps the result. `launch.status {"id"}` returns a row's state, `outcome` and `error`. It is open to the requester's resolved caller, and to a crew for its own requests. The crew allowlist in `crew_scope::check` gains it.

### Stale requests, gone requesters, no dispatcher, restarts and retries

- **Stale.** A row is claimable only in `pending` and before `deadline_ms`. Past the deadline the dispatcher or the waiter moves it to `expired`. It is never launched late.
- **Requester gone.** The requester's liveness, start and argv are part of the proof, checked at the claim. A gone requester gives `refused`. That is why a bounded wait ends in a refusal and not in "queued; the dispatcher starts it": a request whose requester has left can no longer be proved, so promising to launch it later would be false.
- **No dispatcher.** If the serve lock is not held, the refusal is immediate. If the dispatcher is running but does not claim within 30 s, the refusal comes at the end of the window. In both cases nothing was started, and the text says so (see (e)).
- **Dispatcher restart.** Launchers are detached, so a restart neither kills nor repeats them. The new dispatcher claims `pending` rows that are still within their window. A `claimed` row whose launcher is gone and that has no result is marked `failed` with the "stopped before recording" text above. It is never run again. A job it may have started is found by its pre-filled id, and its own supervisor and the reconciliation settle it as usual.
- **Idempotency.** The claim compare and set makes each row run at most once. A caller that retries the CLI call makes a new request. With the same explicit `id`, that request replays the existing job (`prepare_start`'s `Replay`, `agent_replay`). Without one it is a second launch, exactly as a repeated direct call is.

## (c) Security

**What the dispatcher trusts:**
- the kernel, read from outside the sandbox: the requester's PID mapping, start identity, uid, argv and process ancestry;
- the job records the runtime wrote (supervisor identities, launch options), exactly as part (a) of the callers document uses them;
- the row's `command` and `args`, as a request that the command's normal checks then judge, as they judge a direct call's arguments.

**What it never trusts:**
- The row's `claimed` role, actor or job as a result. They are the inputs a direct call takes from its environment, and the same rule judges them. A proven supervisor overrides them, and a mismatch is a role-drop refusal.
- A PID in the row without a matching start identity, namespace, uid and argv binding.
- Any ship, actor or home named in the row. There is no home field. A row is served only by the dispatcher of the home whose database holds it. A `ship`, `actor`, `from` or `address` argument is checked against the resolved actor by `check_dispatch`, as today.
- The waiter. `_launch-wait` can only move its own row from `recorded` to `pending`, or withdraw it, and only when its own identity equals the row's requester. Any process may run `_launch-run ID`, and it gains nothing by doing so. It can only carry out a request as the requester's own proven caller, and only from outside a sandbox.

| attempt | result |
|---|---|
| A crew writes a row naming its first mate's waiting CLI as requester | that process's argv names a different request: `refused` |
| A crew writes a row for itself, claiming the operator or its first mate | its own supervisor is found from outside: the role-drop refusal, audited and noticed |
| A crew in a PID sandbox unsets `PRIVATEER_JOB` and `PRIVATEER_ROLE` | inside, that call is unproven today (Linux). From outside, its supervisor is visible, so it is resolved as the crew. Stronger than a direct call from the sandbox |
| A crew writes a row into another ship's home | that home's launcher resolves it as the crew of its own ship: refused by `crew_scope::check` and charter authorization, as a direct call is |
| A crew runs `_launch-wait` on another process's request | its identity is not the requester's: the waiter refuses and changes nothing |
| PID reuse, or a requester that exits and is replaced | the start identity differs: `refused` |
| Replaying a finished row | only `pending` rows are claimed |
| A read-only crew or a reviewer tries to escape its sandbox through a launch | it cannot write the row. If it could, step 5.6 refuses it |
| A crew double-forks out of its tree, then makes a request | as today on both platforms, a residual of [the crew role](crew-role.md). This path does not make it easier |

**Deliberate consequence.** Work a sandboxed first mate starts runs outside its sandbox. That is the purpose of #736: Privateer's charter, not Codex's shell sandbox, is the authority for supervised work. An unsandboxed first mate gets the same today. The model's own shell commands stay sandboxed, and only charter-authorized `job.start` commands and launches leave it. Read-only sandboxes stay contained (step 5.6). Production deployment is never forwarded.

Under one OS account this remains a workflow control, not a security boundary, as the crew role states.

## (d) Follow-on calls from inside the sandbox

| call on a job the launcher started | works today? | after this change |
|---|---|---|
| `job.status`, `job.list` | yes: record reads (`load` and `public_job`), no process inspection | unchanged |
| `crew.park` | yes: records only. Its job is `crew_scope::job_id()`: on Linux in the sandbox, `PRIVATEER_JOB`; on macOS, the kernel proof. The child's `started_by` was set by the launcher from the proven job, so the lineage check passes | unchanged (no change to `src/crew.rs`) |
| `crew.report`, `crew.answer` | yes | unchanged |
| `workspace.status`, `workspace.list` | yes: records, plus a `git` read of the worktree under the home's lock file | unchanged |
| `job.cancel` | Linux: records the request when the supervisor is live, but cannot signal an orphaned job's payload in another namespace. macOS: the payload is outside the sandbox and cannot be signalled | through the dispatcher, which signals with today's identity and process-group checks |
| `resource.status` | Linux: unaddressable PIDs read as alive. macOS: `ps` fails | through the dispatcher |

The park's resume, a crew's relaunch and the dispatcher's own launches already run outside the sandbox. They are unchanged.

## (e) Refusal text

Every refusal this path adds names a sandbox limit. None says "permission" as a Privateer grant, and none points at `permissions.blocked` or any grant, because no Privateer permission would lift it. A tested assertion checks this. Refusals that come from the command itself (charter, crew scope, admission) pass through word for word, as a direct call gives them.

- No dispatcher: "job.start was not started: this caller runs inside a sandbox (another PID namespace than the home's dispatcher), so the home's dispatcher starts its work, and no dispatcher is running for H. Nothing was started. This is a limit of the sandbox, not a missing Privateer permission: start the dispatcher (`privateer service start`) or run the command outside the sandbox." On macOS the parenthesis reads "a macOS sandbox".
- Not claimed in time: the same, with "no dispatcher took the request within 30 s" in place of "no dispatcher is running".
- Requester not proved: "the dispatcher could not confirm the requesting process from outside its sandbox (REASON). Nothing was started. This is a limit of the sandbox, not a missing Privateer permission: run the command outside the sandbox."
- Contained sandbox: "job J runs read-only (a review errand | a read-only crew), and its sandbox stays closed: Privateer does not start work outside it. This is a limit of that job's sandbox, not a missing Privateer permission."
- Over `rpc`, and the commands that stay refused in a PID sandbox (`ensure_dispatcher_namespace`'s new text): "this caller is in another PID namespace than the home's dispatcher (a sandbox such as Codex's), and COMMAND cannot be carried out from inside it. This is a limit of the sandbox, not a missing Privateer permission: run it outside the sandbox." For `rpc` this adds "or through the privateer CLI, which hands launches to the dispatcher."

## (f) Qualification

The captain's requirement, as a test that runs on CI on each OS (`tests/sandboxed_launch.rs`). It uses no login, provider or model. Harnesses are fixture plugins, as in `tests/crew.rs`, and every name is fictional.

**Setup.**
- A temporary HOME, TMPDIR, XDG directories, `PRIVATEER_FLEET`, `PRIVATEER_RESOURCE_HOME` and `CODEX_HOME`.
- A ship `harbor` with a charter that allows `spawn`, `implement`, `test` and `cleanup`.
- A fixture harness manifest whose payload writes a marker and then sleeps until told to finish.
- A resident `serve` for the home, started outside the sandbox. At teardown it is stopped by identity, never by killing a wrapper.

The first mate's commands run inside the sandbox wrapper, as agent `harbor`, through a script that mimics a native Codex first mate's shell:
- **Linux:** `bwrap --unshare-user --unshare-pid --new-session --die-with-parent --ro-bind / / --dev /dev --proc /proc`, with the home, the ledger directory and a scratch directory bound writable. This reproduces the property #736 is about: a separate PID namespace with its own `/proc`. The implementation pins the exact Codex bwrap flag set it copies, with the openai/codex revision it was read from. CI installs `bubblewrap`. On Ubuntu runners it may also need `kernel.apparmor_restrict_unprivileged_userns=0`, a CI-only step. With `PRIVATEER_SANDBOX_QUALIFICATION=1` (set in CI) a sandbox that cannot be created fails the test. Elsewhere the test skips and prints why.
- **macOS:** `/usr/bin/sandbox-exec -p PROFILE`, where PROFILE is the base profile PR 737's probe copied (`CODEX_LIKE_PROFILE`) plus `workspace-write`'s `file-write*` rules for the home, the ledger and the scratch directory.

**Must pass, on each OS:**
1. **Heavy job.** `job.start {"heavy":true,"wait_for_capacity":true}` from inside returns a job record with the same keys as a direct `job.start` made outside. The payload proves it runs outside the sandbox: on Linux its `/proc/self/ns/pid` is the dispatcher's, and on macOS `/bin/ps -ax` succeeds. `job.status` from inside reaches `succeeded` with exit code 0. A second, sleeping heavy job is cancelled with `job.cancel` from inside. It ends `cancelled`, and from outside its payload's recorded identity is gone (the payload's, not just the supervisor's).
2. **Crew.** `agent.start` from inside launches the fixture crew, which runs. The crew's harness itself runs its CLI calls inside the same sandbox wrapper. It starts a heavy `job.start`, `crew.park`s on it, is resumed by the outcome, and reports with `crew.report`, which the first mate reads from inside. A second crew is cancelled from inside with `job.cancel` and its payload is gone.
3. **Errand.** `errand.start` from inside runs the fixture errand to its settled outcome, which `errand.list` shows from inside. A second errand is cancelled from inside and its payload is gone.

**Must refuse, on each OS:**
4. **No dispatcher.**
   - With no dispatcher running, `job.start` from inside is refused at once with the (e) text. The text contains neither `permissions.blocked` nor "permission denied".
   - With a lock holder that never claims, the call is refused after the window. A test-only `PRIVATEER_LAUNCH_CLAIM_MS` shortens the window for the requester's own wait and can never lengthen it. A real dispatcher started afterwards launches nothing for that row.
5. **Forgery.** A row naming another live waiting requester is `refused`, and no job exists.
6. **Role drop.** A crew call from inside with `PRIVATEER_ROLE` unset is refused as a role drop, and the refusal is audited.
7. **Requester gone.** A requester killed before the claim gives `refused`.
8. **Restart.** A dispatcher restarted after a claim does not run the row again. The job started under the pre-filled id settles normally.

Unit tests, on every platform:
- the row's state machine and its compare-and-set races;
- the `NSpid` parser and the namespace match;
- the argv binding check;
- expiry, the in-flight cap and pruning;
- `resolve_from` against an injected chain, for each row of the callers document's role table;
- the read-only refusal;
- the refusal texts.

## Where the change goes

- New `src/launch.rs`, with `src/launch/identify.rs` for the per-OS requester checks: the table, the request, the waiter, the launcher and `launch.status`.
- `src/lib.rs`: the forwarding decision at the top of `dispatch`, after `mark_contact`.
- `src/main.rs`: the internal `_launch-wait` and `_launch-run` commands. Neither resolves its own ancestry.
- `src/store.rs`: the schema.
- `src/activation.rs`: the resident launcher thread in `serve`.
- `src/crew_scope.rs`: `resolve_from(pid, …)`, and `launch.status` in `check`.
- `src/crew_scope/ancestry.rs`: a starting PID for the Linux walk.
- `src/jobs.rs`: only `ensure_dispatcher_namespace`'s refusal text, and `pub(crate)` access to `pid_namespace` and `identity` for the launcher. None of the resume paths.
- Tests: `tests/sandboxed_launch.rs` and CI setup in `.github/workflows/ci.yml`.

None of the files reserved for another owner (`src/crew.rs`, the `src/jobs.rs` resume paths, `src/workspace/process.rs`, `src/door/reader.rs`, `src/machines/delegated.rs`) needs a change. `crew.park` and the resume claims in `src/crew.rs` already work once the launcher has set the resolved caller.

## Limits

- The outcome is written after the command's own transactions, not in the same one. A launcher that dies in between leaves a `failed` row whose job is found by its pre-filled id. Commands without a caller-visible id (`errand.start`, `review.start`, `verify.run`, `crew.resume`) are then found through `errand.list`, `review.list` and `verify.status`.
- A request is served only while its waiter lives until the claim. A caller whose harness kills its shell within the first few hundred milliseconds loses the request, and is told so if it is still there to read.
- Starting a launch adds a round trip of roughly 0.25 to 0.5 s.
- Other commands that inspect processes and are not listed in (a) may still fail inside a macOS sandbox with `ps` errors. The move of `jobs::identity` onto the kernel read is still a follow-up.
- The launcher is detached from the dispatcher and is not counted against the host's capacity. At most 4 run per home, and each is bounded by its command.
