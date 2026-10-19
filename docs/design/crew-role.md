# Design: a crew role at the runtime boundary

Status: round 1 (the brief) implemented; rounds 2 and 3 revised after the design review, for implementation. Task `privateer-crew-identity-scope`. The captain directed it on 2026-10-18 (notice 8e4da677): crews must not be able to push, merge, ack the first mate's inbox or set task status, and the runtime enforces it so that weaker models are safe to use. Read with [crew park and resume](crew-park-resume.md), [one block, one ask](one-block-one-ask.md) and [architecture](../architecture.md#operators-and-agents).

## Problem

A crew runs as its ship's agent. The runtime does not tell it apart from the first mate, so it can do anything the first mate can:

- **Evidence.** Crews acked the first mate's inbox, ran `task.update` (including `done` before the work was done), started an Opus errand (dea04208), and pushed, opened and merged PRs (182, 190, 206, 239, 245). On sysadmin a haiku crew sent `mail.send` as `sysadmin` to the captain and ran `task.update` as `sysadmin` (notice fb0d751d). A crew read a note written in the first mate's voice, held a merge and exited in 16 seconds (notice 27b6e2c4). Crews that resume from a park follow a `next` line they wrote themselves, and that line has said "push".
- **The brief invites it.** The control protocol in `brief` (`src/plugins.rs`, around line 412) lists `task.update {"status":"running"}` first, then `mail.inbox`/`mail.ack`, `mail.send`, `task.create` and `task.handoff`. A crew's brief and a first mate's brief differ only in where a heavy job's outcome goes. This crew's own first command was that `task.update`, run before it read its assignment.
- **The task note talks like the first mate.** `note.get` on `task:<id>` returns the assignment together with the first mate's handoff notes, and a crew reads those notes as its own role.
- **git and gh are pre-approved.** `claude_rules` (`plugins/tool_permissions.py:70`) grants `Bash(git *)` and `Bash(gh *)` to every Claude launch, crew included. Every job inherits the starter's environment, minus `PRIVATEER_CREDENTIAL_*` and `PRIVATEER_MCP_*` (`src/jobs.rs`, around line 2925), so a crew has the same `HOME`, gh login and git credential helpers as its first mate.

Assignment text does not stop any of this. The rule has to be enforced by the runtime.

## How a crew is identified today

| signal | set by | what it says |
|---|---|---|
| `PRIVATEER_ROLE=agent`, `PRIVATEER_ACTOR=<ship>` | supervisor, from the job's `agent_actor` (`src/jobs.rs:2912`) | the payload is that ship's agent; `Caller::resolve` (`src/store.rs:64`) turns it into `Caller::Agent { actor }` |
| `PRIVATEER_JOB=<job id>` | supervisor, for every job (`src/jobs.rs:2932`) | the supervised job the process runs in |
| job `action` | `jobs::start_agent` sets `spawn`; `job.start` allows only `implement`/`test`/`recovery`/`cleanup` | `spawn` means a harness was launched through `agent.start` |
| job `agent_actor` | `agent.start` (the task owner), or the agent that called `job.start` | the payload runs as this agent |
| `wakes` row with this `job_id` | `activation` before `agent.start` | a first mate's headless wake |
| `errand::is_errand` | `errand::start` | a first mate's errand |
| job `started_by` | `jobs::start`, from the starter's `PRIVATEER_JOB` when that job has the same `agent_actor` (`src/jobs.rs:1963`) | lineage of a crew's heavy jobs |

Today's crew test, used by `crew.report`, `crew.park` and `crew::may_park`, is: the caller is `Caller::Agent { actor }`; `PRIVATEER_JOB` names a job with `action` `spawn`, `agent_actor` = actor and status `launching`/`running`; and `wake_job` (`src/crew.rs:54`, a wake row or an errand) is false. `workspace::is_wake` gives the same answer at launch, and `plugins.rs` uses it to set `context.role` to `crew` or `first_mate`.

The test is only applied *to admit* crew commands. Nothing restricts a crew from first-mate commands, and the signals can be changed by the crew itself:

- **Dropping the role.** `env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR privateer ...` resolves to `Caller::Operator`, which is stronger than the first mate. `Caller` says so ("a workflow control under one OS account, not a security boundary").
- **Dropping the job.** Without `PRIVATEER_JOB` a crew looks like an attended first mate (`PRIVATEER_ROLE=agent`, no job; `src/cli.rs:1084`). It also inherits `PRIVATEER_PRESENCE_HOLDER` and `PRIVATEER_HOSTED_HOLDER` from a hosted first mate, which this crew's environment has. The guards in `presence.rs:319` and `cursor_mail.rs:218` key on `PRIVATEER_JOB` being present.
- **Borrowing a job.** `PRIVATEER_JOB` can name any running job of the same ship, including a wake. `only_the_runtime_speaks_as_crew` (`tests/crew.rs:484`) relies on exactly this to forge `crew.report` from outside a job.

### Definition

A **job caller** is a CLI process (`privateer COMMAND`, `privateer rpc` or a hook) that has a `_job-supervise` process among its ancestors. The nearest such ancestor is its supervisor, and the job that supervisor runs is the caller's job `J`. `J` is found from process records, not from the environment (see "Resolving the job" below).

`J`'s role comes from the nearest **spawn job**: `J` itself when its `action` is `spawn`, otherwise the job `J.started_by` names, followed until a spawn job is reached (bounded, and a cycle is a refusal).

- That spawn job is a **first mate** when it has a `wakes` row or is an errand.
- Otherwise it is a **crew**.

The spawn job's own record decides, before any lineage. A crew started by a wake has `started_by` = the wake, but its own record is a spawn with no wake row and no errand, so it is crew. A crew's heavy job, and a test inside it that calls the CLI, reach the crew's spawn job through `started_by` and are crew too.

A process with no `_job-supervise` ancestor is resolved as today (the captain at a terminal, or an attended first mate), with one exception: it is never weaker than today. If `PRIVATEER_JOB` names a running crew job of the caller's actor, the caller is narrowed to that crew even though no ancestor proves it. This is the case of a harness inside a PID-namespace sandbox, which cannot see its supervisor.

So a wake or an errand is never crew, and a crew is never a first mate, whatever `PRIVATEER_JOB` says. The role is decided from records the runtime wrote before the payload spawned (the job row and its `supervisor` identity, the wake row, the errand row, `started_by`). The crew wrote none of them.

### Resolving the job

At caller resolution, walk the parent links of the current process (bounded to 64 levels; `/proc/<pid>/stat` on Linux, `ps -o ppid=,lstart=` or `sysctl kern.proc.pid` on macOS) to the nearest ancestor whose argv contains `_job-supervise`, using the argv form `jobs::is_supervisor` (`src/jobs.rs:1505`) already parses. Then:

1. Read `--home H` and the job id `J` from that ancestor's argv.
2. Open home `H` and load `J`.
3. Check that the ancestor's pid and start time equal `J.supervisor` (`pid`, `started`, `pid_namespace`), the identity the record already holds.

A match fixes the caller. A mismatch (a fake `_job-supervise` process, or a reused pid) is treated as no ancestor. The check needs no new table and no path taken from the environment: `--home` comes from the supervisor's own argv, so `HOME`, `PRIVATEER_RESOURCE_HOME` and `PRIVATEER_HOME` do not move it. An orphan that double-forks stays under its supervisor only while that supervisor is its ancestor. Once reparented to init it has no ancestor and falls back to the rule above (a residual, below). No subreaper is set, on either platform: it would adopt the `_job-supervise` processes that wakes and crews spawn, which tangles reaping, and it does not exist on macOS.

When a job ancestor is found:

- **No dropping the role.** `Caller::from_env` (`src/main.rs:103`) refuses `Caller::Operator`: "job `J` runs as agent `A`; a job cannot drop its role". The refusal lives there, not in `dispatch`, because `permissions`, `routes`, `door`, `grant`, `host` and the verbs return before a `Store` opens or `dispatch` runs, and several of those are the captain's own commands. The refusal is audited as `caller.role_drop_refused` (new), and a crew's refusal also sends its first mate a low-urgency `crew` notice. Dropping its role is exactly the misbehaviour a first mate should see.
- **No borrowing a job.** `PRIVATEER_JOB`, when set, must equal `J`. A missing value is taken from the ancestry.
- **The role** (crew or first mate) comes from the records, not the environment.
- **`started_by`** for a new job (`src/jobs.rs:1964`) comes from the resolved `J`, not from `PRIVATEER_JOB`. Otherwise a crew that unsets the variable starts heavy jobs with no lineage, and they would not be crew.

**The world.** The rule applies to a command only when it is in the job's world. A command's target home (`--home`, `PRIVATEER_HOME` or the discovered home) is in the world when it is:

- the job's own home `H`, or
- a home listed in this OS account's real fleet registry (`~/.local/state/privateer/fleet.sqlite`, read from the password entry), or
- reached only through the registry in use when that registry is the real one. A command with no target home is in the world this way.

A command outside the world is resolved as before. A refusal of a role drop is audited and the first mate is noticed only when the target is `H` or a registry home. A refusal reached only through the registry is made and recorded nowhere, so a test that changes only `--home` cannot write into a live home. A test that names its own `PRIVATEER_FLEET` or `XDG_STATE_HOME` is outside the world and is not held to the role.

**As_runtime exception.** A `_job-supervise` process is the runtime, not a job caller. Its `PRIVATEER_JOB` is its starter's (for a parked crew's resume, the parent job), so it is not refused as a borrowed job and its crew allowlist is not applied. It is identified from its parsed command (`Caller::from_env_at` takes `supervisor`), not from raw argv.

**Crew launches.** A crew's payload has no first-mate presence: the supervisor removes `PRIVATEER_PRESENCE_HOLDER`, `PRIVATEER_HOSTED_HOLDER`, `PRIVATEER_HOSTED` and `PRIVATEER_ATTENDED_HARNESS` from it. A crew launch is an agent's `spawn` job that has no `wakes` row and is not an errand.

**Residual.** Under one OS account this is not a security boundary. A crew can still escape by starting work outside its tree: a double fork or `setsid` that is reparented to init, a running tmux server, `systemd-run --user`, `at` or `ssh localhost`. It can also write `privateer.sqlite` directly, or read another process's `/proc/<pid>/environ`. Each of these is a deliberate evasion rather than a habit, which is the failure we see. The `Caller` doc comment stays as it is. A per-crew OS user is the real boundary, and it is out of scope (answered question 4).

The supervisor also stops passing first-mate presence to crews. It removes `PRIVATEER_PRESENCE_HOLDER`, `PRIVATEER_HOSTED_HOLDER`, `PRIVATEER_HOSTED` and `PRIVATEER_ATTENDED_HARNESS` from crew payloads (`src/jobs.rs`, next to the credential scrub).

### Options considered and rejected

- **Environment cross-check only.** Refuse `Caller::Operator` while `PRIVATEER_JOB` is set. `env -u PRIVATEER_JOB` gets around it. It survives only as the no-ancestor fallback above.
- **Per-job token** (`PRIVATEER_JOB_TOKEN`). It proves which job a caller is in when the token is present, but a crew that unsets it becomes an operator, so it cannot prove "not crew".
- **Socket credential** (`SO_PEERCRED`). The CLI opens SQLite directly and has no daemon on the command path, so a caller can bypass the socket by not using it.
- **A host-wide `agent_payloads` table in the ledger** (the first draft of this note). The ledger's location comes from `HOME`, `PRIVATEER_RESOURCE_HOME` or the home's config, so a crew that sets either variable and drops its role finds no row and becomes an operator again. The supervisor's argv and `job.supervisor` give the same answer without a new table.

## Runtime commands for a crew

A crew caller gets a default-deny allowlist. It is checked in `dispatch` (`src/lib.rs:122`) right after `operator_only`, by `crew_scope::check(store, command, &args)` in a new module, `src/crew_scope.rs`. The identity is resolved once, when the store opens, and kept on the `Store` as `crew: Option<CrewCaller { job, task, actor }>` (new), so the 41 `Caller::Agent { actor }` matches do not change. A new command is therefore refused to crew until it is listed.

| command | crew may call it when |
|---|---|
| `crew.report`, `crew.answer`, `crew.park` | as today (`crew.rs` already checks the job) |
| `job.start`, `job.status`, `job.list` | the job's `task_id` is the crew's own task |
| `job.cancel`, `job.retry` | the job is not a spawn job, and its `started_by` is the crew's own job |
| `note.get`, `note.list`, `focus.digest` | `ship` is the crew's ship and `focus` is `task:<own>` or `assignment:<own>` |
| `task.show` | the id is the crew's own task |
| `workspace.status`, `resource.status`, `runtime.needs`, `ship.list`, `ship.show`, `charter.show`, `upgrade.status` | read-only |

`job.retry` of a spawn job relaunches an agent (`src/jobs.rs`, the `job.retry` arm), which is `agent.start` by another name. On "own task" alone, `job.cancel` and `job.retry` would also reach the first mate's gate jobs and sibling crews on the same task. Hence the narrower rule.

Every other command is refused. The ones crews have actually called, and the ones they could reach, are:

- mail: `mail.ack`, `mail.inbox`, `mail.send`, `mail.withdraw`, `mail.flush`, `mail.deliver`, `message.send`, `message.inbox`, `event.emit`, `event.ack`
- tasks: `task.update`, `task.create`, `task.handoff`, `task.accept`, `task.rank`, `task.enqueue`, `task.defer`
- launches: `errand.start`, `agent.start`, `crew.resume`, `work.*`
- notes: `note.put`, `note.pin`
- other homes: `peer.call`, `peer.put`
- ship, routes and permissions: `ship.create`, `ship.scope`, `ship.actions`, `ship.retire`, `ship.attend`, `routes.set`, `permissions.set`
- capacity and workspaces: `resource.configure`, `workspace.acquire`, `workspace.release`, `workspace.recover`

One refusal text covers all of them, so that a weak model gets one instruction:

> `{command} is the first mate's, not crew's. Report instead: crew.report {"kind":"progress","body":"..."} (or "decision" to ask). Your first mate mails, updates tasks, pushes and merges.`

`job.*` on another task is refused with "crew job `J` works only on task `T`".

`note.get` for a crew returns the assignment first and labels the task notes `first_mate_handoff` (a new field name; the response is otherwise unchanged). The notes still tell the crew what is going on, but they say plainly whose voice they are in.

The rest of the allowlist covers what crews actually do. Hosted crews load only `crew.mjs`, and the Claude hooks already return early under `PRIVATEER_JOB`.

## git push and gh

The runtime never sees shell commands, so this layer is enforced through the harness, `PATH` and git configuration. The crew's environment is set by the supervisor from the job record, for crews and their lineage, not inherited, so a crew cannot drop it by starting a heavy job.

### Layers

- **A. Harness rules.**
  - *Claude.* A runtime `PreToolUse` hook, passed with `--settings` on crew launches (new). The hook is `privateer hook claude PreToolUse`, already the hook entry point. It parses the Bash command and refuses pushes and gh writes in every form we have seen or can foresee: `git -C dir push`, `git -c k=v push`, `/usr/bin/git push`, `sh -c '...'`, `bash -lc`, `env X=1 git push`, `mise x gh -- gh ...`, and a command after `&&`, `;` or `|`. A hook sees the command after the model writes it and before it runs, so it does not depend on Claude's prefix matching of `--disallowedTools`, which does not parse any of those forms. `claude_rules` (`plugins/tool_permissions.py:70`) keeps `Bash(gh *)` for the first mate only. Whether the hook is honoured under `permission_mode` `auto` (the haiku and opus routes) and `bypassPermissions` is check V1 below. `--disallowedTools` patterns remain a cheap second line, and V1 checks them too.
  - *pi.* A crew guard extension, `plugins/attended/crew-guard.ts` (new), handles pi's `tool_call` event and returns `{block: true, reason}` for the same commands. The installed pi's examples already show `tool_call` blocking. It calls the same parser as the Claude hook (`privateer crew-guard --json`, new, or a shared table), so the rule lives in one place.
  - *opencode.* `OPENCODE_PERMISSION` is already set by the launcher for yolo (`plugins/native_launch.py:133`). A crew gets a bash pattern map with the same denies. The schema and the headless `run` behaviour on deny are check V3.
  - *codex* (luna, forced to `danger-full-access` by `codex_crew_sandbox_default`) may honour execpolicy `forbidden` rules. Whether they hold under `danger-full-access`, and how to apply them without editing the captain's global `~/.codex`, is check V4. Until then codex relies on B and D.
  - *agy* (`--dangerously-skip-permissions` on the agy-flash route) has no rule mechanism this repository uses. It relies on B and D.
- **B. git push refused by configuration (all harnesses, any git binary, aliases included).** The crew env gets `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` (git's command-scope configuration) setting `url.privateer-crew://refused.pushInsteadOf` to `https://`, `http://`, `ssh://` and `git@`. Every push URL is rewritten to the `privateer-crew` scheme, and git runs the remote helper `git-remote-privateer-crew` (new, in `plugins/crew-bin/`, added to the crew's `PATH`). The helper prints the refusal text and exits 1. Fetch is unaffected. **Hole:** git ignores `pushInsteadOf` for a remote with an explicit `pushurl`, and `git remote set-url --push` sets one. A workspace is a worktree of `projects/<repo>`, so that write lands in the shared clone's config and outlives the crew. B also falls to a cleared `GIT_CONFIG_COUNT`, or to a longer `pushInsteadOf` match of the crew's own, since git picks the longest match. B is a default that catches habits, not a lock. D is the lock.
- **C. gh shim (all harnesses).** `plugins/crew-bin/gh` (new), first on the crew's `PATH`. It allows `gh auth git-credential` (git calls it for fetches) and refuses everything else, `gh auth token` included, with the refusal text. gh here is a mise wrapper, so `mise x gh -- gh ...` and the absolute install path both bypass the shim. C exists to give a plain `gh` call a clear refusal, not to stop a crew that looks for the real binary.
- **D. No push credential (recommended for round 3, pending the captain).** The crew env gets an empty `GH_CONFIG_DIR` (a crew-only directory with no token) and an empty command-scope `credential.helper=`, which clears the helpers set in global and repository configuration. The credential helper here is URL-scoped (`credential.https://github.com/.helper`), so whether an empty `credential.helper=` clears it too is check V5; if it does not, D also sets the URL-scoped key empty. Then `/usr/bin/gh`, `mise x gh` and a cleared `GIT_CONFIG_COUNT` have nothing to write with. This repository is public, so fetches still work. A read-only token comes later, and only if a private-repo ship needs one. D is the only layer that holds against a crew that sets out to bypass A to C, and it is cheap because it needs no token.

**Local pushes are not refused.** A workspace shares its clone's object store, so a push to the local `projects/<repo>` gains a crew nothing. A `git` shim to refuse it would also break `tests/workspaces.rs:770` and `tests/machines.rs:2064` when they run inside a crew's heavy job. B rewrites only network schemes, and `file://` is left alone for the same reason.

### Per harness

| harness (route) | A rules | B git config | C gh shim | D no credential |
|---|---|---|---|---|
| claude (haiku, opus) | PreToolUse hook, V1 | yes | yes | yes |
| pi (deepseek-flash) | `tool_call` block | yes | yes | yes |
| opencode (opencode-free, opencode-deepseek) | bash permission map, V3 | yes | yes | yes |
| codex (luna) | execpolicy, V4 | yes | yes | yes |
| agy (agy-flash) | none | yes | yes | yes |

**Recommendation.** Ship A, B, C and D together in round 3, after the checks below. A gives the model a clear denial before it runs anything. B and C give the same refusal text to a plain `git push` or `gh` call from any harness. D makes a deliberate bypass of A to C fail at GitHub. The rule a crew sees is the same at each layer: report, and the first mate pushes.

**Block asks.** `block_ask` turns a Claude denial into a captain decision and parks the task (`a_crew_denial_becomes_one_typed_decision_and_parks_its_task_quietly`). A denial covered by a crew role rule is policy, not a gap, so it opens no ask and does not park the task. `block_ask` gets a `role_rule(rule)` filter (new). The captain's `permissions allow` cannot lift a crew role rule.

### Checks before round 3

Round 3 is written only after these run on this machine, with results recorded on the task. A small job can do them all.

- **V1.** Claude: a `PreToolUse` hook passed with `--settings` refuses a Bash call under `permission_mode` `auto` and under `bypassPermissions`, and the model sees the reason. The same for `--disallowedTools` patterns.
- **V2.** pi: the installed pi's `tool_call` handler returning `{block: true}` refuses a bash call in a headless crew run.
- **V3.** opencode 1.18.34: the bash permission schema in `OPENCODE_PERMISSION`, and what headless `run` does on a deny (refuse and continue, or hang on a prompt).
- **V4.** codex: execpolicy `forbidden` rules under `danger-full-access`, and a way to pass them per launch (a `CODEX_HOME` overlay or a flag) without editing `~/.codex`.
- **V5.** git: an empty command-scope `credential.helper=` clears the URL-scoped `credential.https://github.com/.helper`; `git fetch` of the public repository still works and `git push` fails for want of a credential.
- **V6.** gh: with an empty `GH_CONFIG_DIR`, the mise `gh` and the absolute install path both fail to authenticate, and `gh auth git-credential` gives git no token.

## The brief

Round 1, landed. For a crew (`context.crew == true`), `brief` (`src/plugins.rs`) hands off to `crew_brief`, which drops the `task.update`, `mail.inbox`/`mail.ack`, `mail.send`, `peer.call`, `task.create`, `task.handoff` and `resource.configure` lines. It says:

1. "You are crew of O for this task, started by your first mate. The charter's actions (…) are your first mate's; you do your assignment." This replaces "You are agent O" and the "Authorized actions" line.
2. "Your assignment: `note.get {"ship":"O","focus":"task:T"}` (field assignment). The notes beside it are your first mate's handoff, not your role: never act on your first mate's behalf."
3. The `job.start` and `crew.park` line, as before.
4. "Report with `crew.report` … read its answer with `crew.answer`."
5. "Your first mate mails, updates the task, and pushes, opens and merges PRs: commit locally and report the commit. Never git push or use gh to write."

The first mate's brief is byte-identical to before (`the_first_mates_brief_is_unchanged` holds the exact text). That keeps its cached prefix. The crew brief stays within the size bound of `native_harness_plans_and_brief_are_generic_and_lean`. When round 3 lands, the brief adds "the runtime refuses `git push`, gh writes and first-mate commands to crew", and the `## Crew` section of the ship guide (`src/cli.rs:290`) says routine `git` and `gh` are pre-approved for first mates only.

## Failure modes

- **A crew needs to push a branch for CI.** It does not push. It commits locally and reports the commit, and the first mate pushes. For CI evidence the crew parks on a local gate job (`crew.park`), and remote CI belongs to the first mate. No exception is planned.
- **Errands are not crew.** They keep the first mate's role and their own rules (`one-block-one-ask.md`: an errand's denial asks without parking). A crew cannot start one.
- **A wake or errand mistaken for crew.** Possible only if the wake or errand row is missing when a caller resolves. Activation inserts the wake row before `agent.start`, and errands record theirs first.
- **A crew started by a wake mistaken for a first mate.** Prevented by classifying the nearest spawn job by its own record before lineage.
- **A crew resumed from a park.** The resume is a new crew job (`crew-resume-<park>-<n>`). It gets the crew role by the same rule, and its `next` line cannot widen it.
- **A hosted crew** runs in a pane whose payload is the supervisor's child, so the ancestry check holds. Its env scrub and `PATH` come from the same supervisor code.
- **A supervisor that died** leaves its payload without a `_job-supervise` ancestor. The fallback narrows it to crew through `PRIVATEER_JOB` while the job record is still `running`, and recovery settles the job.
- **A reused pid** with a new start time never matches `job.supervisor`.
- **A test that calls the CLI** inside a crew's heavy job is crew. Tests that need operator commands must run outside crew jobs, which is already true of the supervised gates.

## Migration

No schema change, in a home or the ledger. Resolution reads job records that already exist. Jobs already running when the release lands are resolved by the same rule as soon as a caller inside them runs the new binary, because their `supervisor` identity is already recorded. The brief change takes effect at the next launch. The git and gh layers apply only to payloads spawned by a supervisor running the new release.

## Rounds

1. The brief wording. Test 1. Done.
2. Identity: ancestry resolution through `_job-supervise` and `job.supervisor`, the Operator refusal in `Caller::from_env`, the no-ancestor fallback, `started_by` from the resolved job, the env scrub, the role-drop audit and notice, the `note.get` label, and `crew_scope::check` with its refusal text. Tests 2 to 6.
3. git and gh: layers A to D and the `block_ask` role-rule filter, after checks V1 to V6. Tests 7 to 10.

Each round is committed after `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the affected suites (`--test-threads=1`) pass as supervised heavy jobs.

## Test plan

Real subprocess boundaries: a fake harness script launched through `agent.start` in isolated homes (HOME, PRIVATEER_FLEET, XDG), as `tests/crew.rs` does today.

1. `tests/plugins.rs` `the_crew_brief_lists_only_crew_commands` and `the_first_mates_brief_is_unchanged` (landed): the crew brief has no first-mate commands, no "Authorized actions" and no "You are agent", says the notes are the first mate's handoff and that the first mate pushes and merges; the first mate's brief equals the exact prior text.
2. `tests/crew.rs` `a_crew_is_refused_first_mate_commands_and_told_to_report` (new): from inside a crew job, every refused command in the list fails with the refusal text and writes nothing. `crew.report`, `job.start` on its own task and `note.get` of its assignment succeed. `job.retry` of a spawn job, and `job.cancel` of a first mate's gate job on the same task, are refused.
3. `tests/crew.rs` `a_crew_that_drops_its_role_is_still_crew` (new): `env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR -u PRIVATEER_JOB`, a changed `HOME` and `PRIVATEER_RESOURCE_HOME`, and `--home` of a second home are all refused, audited, and noticed to the first mate. `permissions` and `routes` (early-return commands) are refused too. The same commands from the test process (no job ancestor) run as operator.
4. `tests/crew.rs` `a_crew_cannot_borrow_a_wakes_job` (new): a crew that sets `PRIVATEER_JOB` to a running wake is refused. A process outside any job whose `PRIVATEER_JOB` names a running crew is narrowed to that crew's allowlist (the fallback), so it gains nothing a crew lacks; a crew in a PID-namespace sandbox still reports this way, and `only_the_runtime_speaks_as_crew` is updated to assert the narrowing rather than a refusal. A process named `_job-supervise` that is not the recorded supervisor proves nothing.
5. `tests/crew.rs` `wakes_and_errands_keep_first_mate_commands` (new): a wake job's script can `mail.ack` and `task.update`, and an errand's can too. A crew started from inside a wake is crew.
6. `tests/crew.rs` `a_crews_heavy_job_is_crew_too` (new): the CLI called from a crew's heavy job is refused as crew, including after the crew unset `PRIVATEER_JOB` before `job.start`.
7. `tests/permissions.rs` `crew_launches_carry_the_guard` (new): a crew's Claude argv carries the `--settings` hook and a first mate's does not; the hook refuses each push and gh-write form listed under A and allows `git status`, `git commit` and `git fetch`. The opencode crew env carries its bash denies. V1 is recorded in the PR.
8. `tests/permissions.rs` `a_crew_role_denial_opens_no_captain_ask` (new).
9. `tests/crew.rs` `a_crew_git_push_is_refused_by_its_remote_helper` (new): `git push` to an `https://` remote (rewritten before any network access), via the remote name, an explicit URL and an alias, fails with the refusal text, while a local `file://` push and `git fetch` work.
10. `tests/crew.rs` `the_crew_gh_shim_refuses_all_but_git_credential` (new), with a fake `gh` later on `PATH` that logs its argv, and a check that a crew env has an empty `GH_CONFIG_DIR` and `credential.helper`.

## Answered questions

The design review answered the first draft's open questions. Two remain the captain's.

1. **Layer D.** For the captain. Recommended: an empty credential helper and an empty `GH_CONFIG_DIR` for crews now, with no token; a read-only token later only for a private-repo ship.
2. **Crew `gh` reads.** For the captain. Recommended: no gh for crews at all, since CI belongs to the first mate. If the captain takes D, this follows.
3. **Local pushes.** Not refused (see above). Needs no captain decision.
4. **A per-crew OS user.** Out of scope. The captain should know it is the only real boundary, and it is not planned.
5. **Role-drop refusals.** Audited, and sent to the first mate as a low-urgency `crew` notice.
