# Design: callers inside a Codex sandbox

Status (2026-11-04): (a) and (c) implemented (`src/crew_scope/ancestry.rs`, `src/crew_scope.rs`, `src/activation.rs`), with the first mate's review conditions; (b) is a sketch. Task `privateer-codex-mac-ancestry-732`, issue #732 (part of #649), with issue #736 sketched only. Read with [the crew role](crew-role.md), which defines a job caller and the ancestry rule this document keeps.

## Problem

A native Codex first mate runs its shell commands under Codex's `workspace-write` Seatbelt profile (`/usr/bin/sandbox-exec -p POLICY -- CMD`). Every Privateer CLI call it makes resolves its caller first (`crew_scope::resolve`). On macOS that resolution walks the process ancestry by running `/bin/ps -axww -o pid=,ppid=,args=` (`src/crew_scope.rs`, the non-Linux `ancestors`). Under the profile `ps` fails, so the CLI exits with "process ancestry inspection failed" before it does anything. The wake's job still exits 0, so the wake settles `succeeded`, covers its mail, and nothing was answered.

The relevant rules of Codex's base profile (`codex-rs/sandboxing/src/seatbelt_base_policy.sbpl` at openai/codex `696b4502`, read 2026-11-04):

| rule | effect on ancestry |
|---|---|
| `(deny default)` | anything not listed is denied |
| `(allow process-exec)`, `(allow process-fork)` | children run, and inherit the sandbox |
| `(allow process-info* (target same-sandbox))` | `proc_pidinfo`, `proc_pidpath` and `proc_pidinfo(PROC_PIDTBSDINFO)` work only on processes inside the same sandbox. The supervisor, the dispatcher and the Codex app-server are outside it |
| `sysctl-read`: names `kern.argmax`, `kern.osversion` and others, prefixes `kern.proc.pid.`, `kern.proc.pgrp.` and others | `sysctl {CTL_KERN, KERN_PROC, KERN_PROC_PID, pid}` is allowed for any pid. `kern.proc.all` (which `ps -ax` reads) is not listed and is denied. `kern.procargs2` (argv) is not listed either, but is **not** reliably denied: see below |
| `(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))` | `getpwuid` works, so the real fleet registry can still be found |

So, inside the sandbox, a caller can read each ancestor's parent pid, start time and state (`struct kinfo_proc`), but not its path or other `proc_pidinfo` fields.

Its argv may or may not be readable. This document first assumed that an unlisted `kern.procargs2` is denied. The macOS CI probe disproved that: under `sandbox-exec` with the base profile's `deny default`, `process-info*` and `sysctl-read` rules copied verbatim (no network or preference layer of Codex's adds a `kern.procargs2` rule), CI run 41963181277 read the argv of every same-user ancestor outside the sandbox (the test process, `cargo`, the runner's `Python`). Why Seatbelt lets that `KERN_PROCARGS2` read through has not been established, and it may differ between macOS releases, profiles or users (another user's argv is hidden from any unprivileged caller). So the design does not depend on it: a visible argv is proved by today's rule, and a hidden one by the records. The ship home is a writable root (`plugins/native.py` passes `--add-dir HOME`), so the store can be opened read-write.

## (a) Establishing a sandboxed caller's identity on macOS

### Options

1. **Direct-ancestor inspection the sandbox allows.** `proc_pidinfo` and `proc_pidpath` are `process-info*` and are denied for every ancestor that matters. `sysctl KERN_PROC_PID` is allowed and gives `e_ppid`, `p_starttime` and `p_stat`. Argv cannot be relied on: it may be denied.
2. **A launch-issued caller credential.** The supervisor creates a random secret per job, records its hash in the job record, and passes the secret to its payload in the environment. The CLI presents it.

### Choice: kernel ancestry, with the supervisor found from the records

Choose 1, using only `sysctl KERN_PROC_PID` and no subprocess. Recognize a supervisor by matching each ancestor's identity against the supervisor identities the job records hold, instead of reading the ancestor's argv.

The walk (`crew_scope::ancestry::walk`) starts at this process and follows parent links, at most 64, up to pid 1. On macOS it reads, for each ancestor:

- `pid`, its parent, whether it is a zombie or exiting (`P_WEXIT`), and its kernel command name (`p_comm`), from `kinfo_proc`;
- its start identity, formatted from `p_starttime.tv_sec` exactly as `jobs::identity` records it today from `ps -o lstart=` with `TZ=UTC0 LC_ALL=C`, for example `Wed Nov 4 12:00:00 2026`. Seconds resolution, as today;
- its argv, from `sysctl KERN_PROCARGS2`. If that read fails while the process is still there with the same start, the argv is **hidden**. It is never read as empty. Linux still reads `/proc`, where argv is always visible.

The nearest ancestor that is the recorded supervisor of a job is the caller's supervisor. That ancestor is found in one of two ways:

- **Argv readable** (no sandbox, or a sandbox that lets `kern.procargs2` through): as today. Take `--home H _job-supervise J` from the argv, load `J` from `H`, and require `J.supervisor` to have the same pid, the same start identity and no PID namespace. The only change is that the start time comes from the kernel rather than from `ps`.
- **Argv hidden** (a sandbox that denies `kern.procargs2`, or another user's process): every supervisor is a Privateer process, so only a hidden ancestor whose kernel command name is `privateer` or this CLI's own name (or is unknown) is looked up. Any other hidden ancestor, such as the root-owned `login` above every unsandboxed Terminal shell, is passed over, as the visible rule passes over any argv that is not `_job-supervise`. For a hidden Privateer ancestor, read the job records of the homes the caller can name: the target home and every home the real registry lists, which is the same set `world` already treats as `Home`. A job whose `supervisor` has this pid and this start identity is the match. If two records claim the same pid and start, the caller is refused. One unindexed query per home (`json_extract(record,'$.supervisor.pid') IN (pids)`) takes about 30 ms on a home with 6,000 jobs. It runs, and the registry is opened, only when some hidden ancestor is a Privateer process.

**Failing closed.** A hidden ancestor that matches no record might still be a supervisor whose job is recorded in a home the caller cannot name. Every supervisor is a Privateer process, so a hidden ancestor whose kernel command name is `privateer` (or this CLI's own name) must be accounted for before the walk passes it. The two Privateer processes that legitimately sit above a first mate's harness record their identity:
- an attended `privateer start` has a live presence lease `start:PID:...`;
- a hosted `privateer start --hosted` has its pid and start in `hosted_panes`.

A hidden Privateer ancestor that none of these records accounts for refuses the caller. It is never passed over, so a crew is never resolved as the operator or a first mate because its supervisor's home was out of reach.

| ancestry seen by the caller | result |
|---|---|
| argv visible for every ancestor | today's rule: the nearest `_job-supervise` ancestor its record proves |
| some argv hidden; the nearest proven ancestor is a recorded supervisor (one record) | that job, then `world` and `classify` as today: crew, first mate or captain's job |
| some argv hidden; two records claim the nearest such ancestor | refused (ambiguous) |
| some argv hidden; a hidden Privateer ancestor nearer than any match, and no job, hosted pane or live `start` lease records it | refused (fail closed) |
| some argv hidden; no match, and every hidden Privateer ancestor is a recorded `start` session (or there is none) | no job caller: resolved as before, including the `PRIVATEER_JOB` narrowing. This is what the visible rule gives for the same tree, which has no supervisor in it |
| some argv hidden, but no hidden ancestor is a Privateer process (for example `login` above a Terminal shell) | no records read; the visible argv decide, as in the first row |
| the kernel walk itself is refused | refused (sandbox limit) |

Unit tests cover each row with an injected process chain and temporary homes, so they run on Linux CI as well (`crew_scope::tests`, `crew_scope::ancestry::tests`).

Everything after the match is unchanged: `world`, `classify`, the role-drop and borrowed-job refusals, and the `PRIVATEER_JOB` narrowing for a caller with no proven supervisor.

**Refusals name a sandbox limit.** If `sysctl KERN_PROC_PID` fails for any link (a stricter profile than Codex's), the CLI refuses and does not fall back to the operator:

> this caller's sandbox hides its process ancestry (sysctl kern.proc.pid for process N: Operation not permitted), so Privateer cannot tell which job it runs in and refuses the command. This is a limit of the sandbox, not a missing Privateer permission: run the command outside the sandbox.

The fail-closed refusal says the same and adds "or name the job's home with --home". Naming the home adds it to the homes searched. Neither refusal points at `permissions.blocked` or any grant, because no Privateer permission would lift it.

A pid that has disappeared ends the walk, as `/proc` does on Linux. That includes an ancestor that exits (or whose pid is reused) between its `kinfo_proc` read and its argv read: neither it nor anything above it is reported. So does an ancestor whose argv read fails while it is exiting (the kernel tears down an exiting process's arguments before it becomes a zombie), or once the process below it has been reparented: a detached runner whose Privateer parent replies and exits is not refused as sandboxed. Only a live, non-exiting ancestor that is still its child's parent is read as hidden.

### What a crew can and cannot forge

The proof is the pair (pid, start time), checked against a `supervisor` identity the runtime wrote before the payload was spawned. The crew wrote none of these, and both values come from the kernel.

- **Naming the first mate's job** (setting `PRIVATEER_JOB`, `--home`, `HOME` or XDG variables): no effect. The job is not taken from the environment, and the first mate's supervisor is not an ancestor, so its identity matches no link in the walk.
- **A fake `_job-supervise` process** (exec'ing `privateer --home H _job-supervise W`): its pid and start match no record. With argv hidden its argv is never read at all.
- **Reusing a pid**: the start time differs.
- **Dropping the role** (`env -u PRIVATEER_ROLE ...`): the crew's own supervisor is still its nearest recorded ancestor, so this is refused as today.
- **Hiding its supervisor's home** (a crew in a home the caller cannot name, with `PRIVATEER_JOB` unset): refused by the fail-closed rule above.
- **Escaping the ancestry** (double fork, after which the orphan is adopted by launchd): possible, as it is today on both platforms. [The crew role](crew-role.md) records this as a residual: with no subreaper, it is a workflow control under one OS account, not a security boundary. The sandbox does not make it easier.

This is at least as strong as today's check. The proof is the same (pid and start identity against the record), and it is never weaker than the visible rule for the same tree: an ancestor it cannot place refuses. Unsandboxed macOS callers now read the exact argv from the kernel rather than from `ps`'s space-joined columns, so a home path with a space is recognised.

Residuals:
- A Privateer binary installed under a name that is neither `privateer` nor this CLI's own name is not recognised as a possible supervisor: with its argv hidden it is neither looked up nor refused. A supervisor is spawned from the executable of the process that started its job, normally the same installation as this CLI.
- An attended `start` lease names a pid without a start time. That lease lives at most 90 s after its process is gone, and the walk only reaches live processes.

### Why not a credential

- **It can be dropped.** A credential is a proof the caller chooses to present. A crew that unsets it is, by the existing fallback, the operator, unless a missing credential is refused. Refusing would also refuse the captain's own sandboxed Codex, which has no job. Ancestry cannot be dropped without escaping the process tree.
- **It can be transferred.** It lives in the environment and in every child's environment. A crew that runs unsandboxed under the same uid can read the first mate's environment (`/proc/PID/environ`, or `KERN_PROCARGS2` on macOS) and then act as the first mate from anywhere, even after escaping its own tree. A pid and start pair cannot be carried out of the tree.
- **Codex filters the shell environment** (`shell_environment_policy`, which can exclude secret-looking names such as `*KEY*`, `*SECRET*` and `*TOKEN*`; its current defaults were not verified here), so a credential's delivery would depend on a filter we do not own.
- **It adds state** (issuing, hashing, rotating, resuming) to `src/jobs.rs` launch paths that are another owner's.

A credential would still be needed if a profile denied `kern.proc.pid` too. The refusal above covers that case and says so. The macOS CI probe in this task shows whether Codex's profile allows the walk.

### Out of scope for (a): follow-ups

On macOS, these CLI paths in `src/jobs.rs` still run `ps` and are left unchanged here:
- `jobs::identity` (and with it `process_identity`, `own_identity`, `is_recorded_supervisor`);
- `child_pids`;
- `group_members`;
- `process_table`.

Inside a sandbox, commands that use them (for example `job.status` checking a running job's liveness) may still fail. Caller identity, which every command needs, is fixed here. Moving `jobs::identity` onto the same kernel read (`crew_scope::ancestry::kinfo` and `lstart`, which the macOS test checks byte for byte against `ps`) is a follow-up in a file another owner is changing.

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

- **Contact.** The first Privateer command that gets past caller resolution in a job sets `wakes.contact_ms` on that job's running wake row (`activation::mark_contact`, called from `dispatch`): one read, plus one write only the first time. It counts whether the command then succeeds or is refused (a refused command is still the first mate running a command). The job is the resolved job from (a): proven, or named by `PRIVATEER_JOB` where nothing proves it (Linux PID sandboxes). The mark is made in the home the command opens. A first mate whose only commands went to another home is judged unreached.
- **Caller refusal.** A CLI process that is refused while resolving its caller (for example, ancestry hidden from it) writes the refusal to `wakes.caller_refusal` on the running wake row that `PRIVATEER_JOB` names, if that row has no contact yet. It is best effort and never changes a status. The environment is unproven here, but the field is diagnostic only and only explains a failure the absence of contact already causes.
- **Settlement.** A wake whose job `succeeded` but whose row has no `contact_ms` settles `failed`, not `succeeded`, with the error `first mate ran no Privateer command` followed by `: <caller_refusal>` when one was recorded. Being `failed`:
  - leaves its mail uncovered, so the next wake carries it again;
  - keeps the focus's warm session, because the cause is usually the environment (a sandbox), not the conversation. A wake whose job itself failed still drops its session, as before;
  - and applies the existing `retry_backoff_secs` and `max_wakes_per_hour` limits, so a sandbox that stays broken costs at most the hourly wake budget, visibly in `wake.list` instead of silently.
- **Scope.** This applies only to headless mail wakes created by a release that marks contact. The insert sets `tracks_contact=1`, so a wake started by an older release and settled after an upgrade is judged as before. Work wakes (`reason='work'`) already need `work.started` and `work.settled` and are unchanged.

Not covered: a first mate that runs one command and then fails every other one. Contact means the runtime was reachable, not that the brief was followed.

## Tests

- **Every platform** (`crew_scope::ancestry::tests`, `crew_scope::tests`), using an injected process chain:
  - the walk reaches pid 1 and stops at its depth bound;
  - a hidden argv is never read as empty, and an ancestor gone, exiting or no longer the parent mid-read ends the walk;
  - a hidden ancestor that is not a Privateer process reads no records;
  - a refused kernel read is the sandbox-limit refusal, not a crash;
  - the `lstart` formatter matches `date -u` references;
  - `procargs` parsing and the `kinfo_proc` layout (size 648; `p_stat` 36, `p_pid` 40, `p_comm` 243 and `e_ppid` 560, asserted at compile time);
  - one test per row of the role table.
- **macOS CI only** (`cfg(target_os = "macos")`; nothing on a Linux machine compiles or runs them):
  - the kernel read's parent is `getppid()`;
  - its start identity equals `jobs::process_identity` (from `ps`) for this process and its parent;
  - its argv equals `std::env::args`;
  - a probe re-runs the test binary under `/usr/bin/sandbox-exec` with Codex's base rules and reports on its own stderr line. It checks that `/bin/ps -ax` fails there (reproducing #732), and that the walk still reaches the test process with its pid, start and command name. It accepts either argv outcome but checks each: a visible argv must equal the test process's `std::env::args` and resolution must not error; a hidden one, whose ancestor carries this CLI's command name and no record, must be refused with the sandbox-limit refusal and no mention of `permissions.blocked`.
- **Settlement for (c)** (`activation::tests`):
  - no contact gives `failed`, with the refusal in the error, the mail uncovered and the session kept;
  - contact gives `succeeded`;
  - an untracked wake is judged as before;
  - a failed job still drops its session.
