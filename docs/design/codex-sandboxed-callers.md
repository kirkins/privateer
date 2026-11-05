# Design: callers inside a Codex sandbox

Status (2026-11-04): contract, for review before implementation. Task `privateer-codex-mac-ancestry-732`, issue #732 (part of #649), with issue #736 sketched only. Read with [the crew role](crew-role.md), which defines a job caller and the ancestry rule this document keeps.

## Problem

A native Codex first mate runs its shell commands under Codex's `workspace-write` Seatbelt profile (`/usr/bin/sandbox-exec -p POLICY -- CMD`). Every Privateer CLI call it makes resolves its caller first (`crew_scope::resolve`). On macOS that resolution walks the process ancestry by running `/bin/ps -axww -o pid=,ppid=,args=` (`src/crew_scope.rs`, the non-Linux `ancestors`). Under the profile `ps` fails, so the CLI exits with "process ancestry inspection failed" before it does anything. The wake's job still exits 0, so the wake settles `succeeded`, covers its mail, and nothing was answered.

The relevant rules of Codex's base profile (`codex-rs/sandboxing/src/seatbelt_base_policy.sbpl`, read 2026-11-04):

| rule | effect on ancestry |
|---|---|
| `(deny default)` | anything not listed is denied |
| `(allow process-exec)`, `(allow process-fork)` | children run, and inherit the sandbox |
| `(allow process-info* (target same-sandbox))` | `proc_pidinfo`, `proc_pidpath` and `proc_pidinfo(PROC_PIDTBSDINFO)` work only on processes inside the same sandbox. The supervisor, the dispatcher and the Codex app-server are outside it |
| `sysctl-read`: names `kern.argmax`, `kern.osversion` and others, prefixes `kern.proc.pid.`, `kern.proc.pgrp.` and others | `sysctl {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid}` is allowed for any pid. `kern.proc.all` (which `ps -ax` reads) and `kern.procargs2` (argv) are not listed, so both are denied |
| `(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))` | `getpwuid` works, so the real fleet registry can still be found |

So, inside the sandbox, a caller can read each ancestor's parent pid, start time and state (`struct kinfo_proc`), but not its argv, path or other `proc_pidinfo` fields. The ship home is a writable root (`plugins/native.py` passes `--add-dir HOME`), so the store can be opened read-write.

## (a) Establishing a sandboxed caller's identity on macOS

### Options

1. **Direct-ancestor inspection the sandbox allows.** `proc_pidinfo` and `proc_pidpath` are `process-info*` and are denied for every ancestor that matters. `sysctl KERN_PROC_PID` is allowed and gives `e_ppid`, `p_starttime` and `p_stat`. Argv is denied.
2. **A launch-issued caller credential.** The supervisor creates a random secret per job, records its hash in the job record, and passes the secret to its payload in the environment. The CLI presents it.

### Choice: kernel ancestry, with the supervisor found from the records

Choose 1, using only `sysctl KERN_PROC_PID` and no subprocess. Recognize a supervisor by matching each ancestor's identity against the supervisor identities the job records hold, instead of reading the ancestor's argv.

The walk is the same on both platforms. Starting at this process, follow parent links (at most 64) to pid 1. For each ancestor, read:

- `pid`, its parent, and whether it is a zombie, from `kinfo_proc`;
- its start identity, formatted from `p_starttime.tv_sec` exactly as `jobs::identity` records it today from `ps -o lstart=` with `TZ=UTC0 LC_ALL=C`, for example `Wed Nov 4 12:00:00 2026`. Seconds resolution, as today;
- its argv, from `sysctl KERN_PROCARGS2`, or **hidden** when that sysctl is refused.

The nearest ancestor that is the recorded supervisor of a job is the caller's supervisor. That ancestor is found in one of two ways:

- **Argv readable** (no sandbox): as today. Take `--home H _job-supervise J` from the argv, load `J` from `H`, and require `J.supervisor` to have the same pid, the same start identity and no PID namespace. The only change is that the start time comes from the kernel rather than from `ps`.
- **Argv hidden** (sandbox): read the job records of the homes in the caller's world: the target home and every home the real registry lists, which is the same set `world` already treats as `Home`. A job whose `supervisor` has this pid and this start identity is the match. If two homes both claim the same pid and start, the caller is refused. One unindexed query per home (`json_extract(record,'$.supervisor.pid') IN (pids)`) takes about 30 ms on a home with 6,000 jobs.

Everything after the match is unchanged: `world`, `classify`, the role-drop and borrowed-job refusals, and the `PRIVATEER_JOB` narrowing for a caller with no proven supervisor.

**Refusal when the kernel walk itself is denied.** If `sysctl KERN_PROC_PID` returns `EPERM` for any link (a stricter profile than Codex's), the CLI refuses with a clear error and does not fall back to the operator:

> cannot inspect this process's ancestry (sysctl kern.proc.pid: Operation not permitted), so Privateer cannot tell which job it runs in; run Privateer outside the sandbox, or allow sysctl-read of kern.proc.pid.

A pid that has disappeared ends the walk, as `/proc` does on Linux.

### What a crew can and cannot forge

The proof is the pair (pid, start time), checked against a `supervisor` identity the runtime wrote before the payload was spawned. The crew wrote none of these, and both values come from the kernel.

- **Naming the first mate's job** (setting `PRIVATEER_JOB`, `--home`, `HOME` or XDG variables): no effect. The job is not taken from the environment, and the first mate's supervisor is not an ancestor, so its identity matches no link in the walk.
- **A fake `_job-supervise` process** (exec'ing `privateer --home H _job-supervise W`): its pid and start match no record. With argv hidden its argv is never read at all.
- **Reusing a pid**: the start time differs.
- **Dropping the role** (`env -u PRIVATEER_ROLE ...`): the crew's own supervisor is still its nearest recorded ancestor, so this is refused as today.
- **Escaping the ancestry** (double fork, after which the orphan is adopted by launchd): possible, as it is today on both platforms. [The crew role](crew-role.md) records this as a residual: with no subreaper, it is a workflow control under one OS account, not a security boundary. The sandbox does not make it easier.

This is at least as strong as today's check. The proof is the same (pid and start identity against the record), and a sandboxed caller never relies on argv, which `ps` joined with spaces. The one difference is narrower, not weaker: with argv hidden, a nearer supervisor from a home outside the caller's world is passed over, because its home cannot be named. Today that case only matters when a test suite inside a crew starts its own jobs in a temporary home, and passing over it lands on the crew's own supervisor, which is the stricter answer.

### Why not a credential

- **It can be dropped.** A credential is a proof the caller chooses to present. A crew that unsets it is, by the existing fallback, the operator, unless a missing credential is refused. Refusing would also refuse the captain's own sandboxed Codex, which has no job. Ancestry cannot be dropped without escaping the process tree.
- **It can be transferred.** It lives in the environment and in every child's environment. A crew that runs unsandboxed under the same uid can read the first mate's environment (`/proc/PID/environ`, or `KERN_PROCARGS2` on macOS) and then act as the first mate from anywhere, even after escaping its own tree. A pid and start pair cannot be carried out of the tree.
- **Codex filters the shell environment** (`shell_environment_policy`, which can exclude secret-looking names such as `*KEY*`, `*SECRET*` and `*TOKEN*`; its current defaults were not verified here), so a credential's delivery would depend on a filter we do not own.
- **It adds state** (issuing, hashing, rotating, resuming) to `src/jobs.rs` launch paths that are another owner's.

A credential would still be needed if a profile denied `kern.proc.pid` too. The refusal above covers that case and says so. The macOS CI probe in this task shows whether Codex's profile allows the walk.

### Out of scope for (a)

On macOS, other CLI paths still run `ps` (`jobs::identity`, `child_pids`, `group_members` and `process_table` in `src/jobs.rs`). Inside a sandbox, commands that inspect job liveness (for example `job.status` on a running job) may still fail after this change. Caller identity, which every command needs, is fixed here. Moving `jobs::identity` onto the same kernel read is a follow-up in a file another owner is changing. The start-identity formatter added here produces the same string, and the macOS test checks this against `ps`.

## (b) One launch contract for sandboxed callers (sketch only, issue #736)

**The problem on both platforms.** A sandboxed caller can record work but cannot start a process outside its sandbox:

- On Linux under bwrap it is in its own PID namespace, so `ensure_dispatcher_namespace` (`src/jobs.rs`) refuses `job.start`, `agent.start`, `errand.start` and `workspace.*`. A record made there could never be inspected or settled from outside.
- On macOS every child of a Seatbelt-sandboxed CLI inherits the sandbox. So a supervisor the CLI spawns would run its job (a build, or a crew's harness) under the first mate's sandbox. That job would be unable to inspect its own processes, and its children would show their ancestry as hidden.

**Sketch: a durable launch request, carried out by the dispatcher.**

1. **Request.** The sandboxed CLI does everything except spawning. It runs the command's checks (`check_dispatch`, `crew_scope::check`, admission planning), then in one transaction writes a `launch_requests` row: request id, command and args, the resolved caller (actor, job, crew, as found by (a)), and the requesting process (pid, start identity, PID namespace). It does not spawn anything.
2. **Wake the dispatcher.** The CLI signals through the existing dispatcher wake path (a file or socket the dispatcher already watches), then waits a bounded time for the row's outcome. While it waits it keeps its pid alive, so it can be identified.
3. **Re-establish identity outside the sandbox.** The dispatcher claims the row in a transaction and finds the requester by its recorded identity:
   - On macOS, the same pid and start; the dispatcher is outside the sandbox and sees the same pids.
   - On Linux, the process whose `NSpid` chain ends in the recorded pid inside the recorded namespace, then its outer pid.

   The dispatcher then resolves the requester's caller with the same ancestry rule from (a), run with full argv from outside the sandbox. It also re-runs `crew_scope::check` for that caller. The recorded caller is only a cross-check: a mismatch, or a requester that has gone, refuses the request. A stale request (past its deadline, or whose job is no longer running) is refused and is never launched late.
4. **Launch and answer.** The dispatcher spawns the supervisor as it does for its own queued work, in its own namespace and outside any sandbox. It records the outcome on the row in the same transaction that records the job, so a restart neither loses the job nor launches it twice. The CLI returns the job record, or `queued` with the request id after its bounded wait.

**How (a) fits.** The identity in (a) is a property of the process tree: the nearest recorded supervisor, proved by pid and start. It can be computed again by the dispatcher from outside, using the requester's pid. A credential would have to be carried in the request, where any process that saw it could replay it. With (a), the request names a process and the dispatcher checks it. The ancestry rule is the same function, given a starting pid instead of `std::process::id()`. The implementation in this task takes that parameter so (b) can reuse it.

Not decided here: the exact table, the wake signal, the wait bound, and how `workspace.*` commands split between recording (in the sandbox) and acting (in the dispatcher).

## (c) A wake that could not run any CLI command

**Today.** `activation::settle` sets a wake's status from its job's exit status alone. A first mate whose every Privateer call failed exits 0, so its wake is `succeeded`. `MAX(through_seq) WHERE status='succeeded'` then covers its mail, and no reply was ever sent: replies, acks and handoff notes all go through the CLI. The wake brief requires at least `mail.ack` before finishing (`activation.rs`), so a wake that ran no command did not follow it.

**Recorded instead.**

- **Contact.** The first Privateer command that gets past caller resolution in a job sets `wakes.contact_ms` on that job's running wake row: one read, plus one write only the first time. It counts whether the command then succeeds or is refused (a refused command is still the first mate running a command). The job is the resolved job from (a): proven, or named by `PRIVATEER_JOB` where nothing proves it (Linux PID sandboxes).
- **Caller refusal.** A CLI process that is refused while resolving its caller (for example, ancestry hidden from it) writes the refusal to `wakes.caller_refusal` on the running wake row that `PRIVATEER_JOB` names, if that row has no contact yet. It is best effort and never changes a status. The environment is unproven here, but the field is diagnostic only and only explains a failure the absence of contact already causes.
- **Settlement.** A wake whose job `succeeded` but whose row has no `contact_ms` settles `failed`, not `succeeded`, with the error `first mate ran no Privateer command` followed by `: <caller_refusal>` when one was recorded. Being `failed`:
  - leaves its mail uncovered, so the next wake carries it again;
  - drops the focus's warm session, so the next wake starts fresh;
  - and applies the existing `retry_backoff_secs` and `max_wakes_per_hour` limits, so a sandbox that stays broken costs at most the hourly wake budget, visibly in `wake.list` instead of silently.
- **Scope.** This applies only to headless mail wakes created by a release that marks contact. The insert sets `tracks_contact=1`, so a wake started by an older release and settled after an upgrade is judged as before. Work wakes (`reason='work'`) already need `work.started` and `work.settled` and are unchanged.

Not covered: a first mate that runs one command and then fails every other one. Contact means the runtime was reachable, not that the brief was followed.

## Tests

- Unit tests run on every platform, using a fake process table:
  - the walk stops at pid 1 and at its depth bound;
  - a hidden argv is matched through the records;
  - a refused kernel read gives the refusal above, not a crash or a panic;
  - two homes claiming one identity are refused.
- `cfg(target_os = "macos")` tests (only macOS CI runs them):
  - the kernel walk's parent is `getppid()`;
  - the formatted start identity equals `jobs::process_identity` (from `ps`) for this process;
  - a probe re-runs the test binary under `sandbox-exec` with a profile modelled on Codex's base profile. It shows that `/bin/ps -ax` fails there, that the kernel walk still reaches the test process with the right start identity, and that its argv is reported hidden.
- Settlement tests for (c): no contact gives `failed` with the refusal in the error; contact gives `succeeded`; an untracked wake is judged as before.
