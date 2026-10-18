# Design: a crew role at the runtime boundary

Status: design only, for review. Task `privateer-crew-identity-scope`. The captain directed it on 2026-10-18 (notice 8e4da677): crews must not be able to push, merge, ack the first mate's inbox or set task status, and the runtime enforces it so that weaker models are safe to use. Read with [crew park and resume](crew-park-resume.md), [one block, one ask](one-block-one-ask.md) and [architecture](../architecture.md#operators-and-agents).

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

A **job caller** is a CLI process (`privateer COMMAND` or `privateer rpc`) whose nearest ancestor is the payload of a running supervised job `J` that has `agent_actor` set. Ancestry is checked by process identity (pid and start time, the form `jobs::process_identity` records), not by the environment.

- `J` is a **crew** when `action` is `spawn` and it has no `wakes` row and no errand, or when it is a job (any action) whose `started_by` lineage leads to such a job. A crew's heavy job, and a test it runs that calls the CLI, are crew.
- `J` is the **first mate's** when it is a wake or an errand, or when its `started_by` lineage leads to one.
- A process with no job ancestor is resolved as today: the captain at a terminal (operator), or an attended first mate (`PRIVATEER_ROLE=agent`, no job).

So a wake or an errand is never crew, and a crew is never a first mate, whatever `PRIVATEER_JOB` says. The job's role is decided from records the runtime wrote before the payload spawned (the wake row, the errand row, `started_by`). The crew wrote none of them.

### Making it hold when the crew changes its environment

Options, weakest first:

1. **Environment cross-check only.** Refuse `Caller::Operator` while `PRIVATEER_JOB` is set, and refuse a `PRIVATEER_JOB` that is not crew-or-wake of the actor. This is cheap, but `env -u PRIVATEER_JOB` gets around it.
2. **Per-job token** (`PRIVATEER_JOB_TOKEN`, new; its hash on the job row). This proves *which* job a caller is in when the token is present. A crew that unsets the token still becomes an operator, so it cannot prove "not crew". Rejected as the primary check.
3. **Socket credential** (`SO_PEERCRED` on a runtime socket). The CLI opens SQLite directly and has no daemon on the command path, so a caller can bypass a socket by not using it. Rejected.
4. **Process ancestry (recommended).** At caller resolution, walk `/proc/self` parent links (bounded to 64 levels) and look each ancestor's identity up in a host-wide table of running agent payloads, `agent_payloads(identity, home, job_id, actor, role)` (new). It lives in the shared resource ledger (`jobs::ledger_dir`), so a crew that runs `privateer --home <other>` is found too. The supervisor inserts the row right after spawn and deletes it when the job settles; recovery deletes rows whose identity is gone (`jobs::identity_alive`). A matching ancestor fixes the caller:
   - `Caller::Operator` becomes a refusal: "job `J` runs as agent `A`; a job cannot drop its role". It is audited as `caller.role_drop_refused` (new).
   - `PRIVATEER_JOB`, when set, must equal `J`. A missing value is taken from the ancestry.
   - The role (crew or first mate) comes from the row, not the environment.
   The supervisor also sets `PR_SET_CHILD_SUBREAPER` (new), so a double-forked or `setsid` descendant is reparented to the supervisor and stays in the tree. The existing descendant cleanup (`cleanup_descendants`, `escaped`) then has to reap those orphans. The implementer must check that interaction in the supervisor's wait loop.

**Residual.** Under one OS account this is not a security boundary. A crew can still escape by starting work outside its tree (a running tmux server, `systemd-run --user`, `at`, `ssh localhost`), by writing `privateer.sqlite` directly, or by reading another process's `/proc/<pid>/environ`. Each of these is a deliberate evasion rather than a habit, which is the failure we see. The `Caller` doc comment stays as it is. A per-crew OS user is the real boundary (open question 4).

The supervisor also stops passing first-mate presence to crews. It removes `PRIVATEER_PRESENCE_HOLDER`, `PRIVATEER_HOSTED_HOLDER`, `PRIVATEER_HOSTED` and `PRIVATEER_ATTENDED_HARNESS` from crew payloads (`src/jobs.rs`, next to the credential scrub).

## Runtime commands for a crew

A crew caller gets a default-deny allowlist. It is checked in `dispatch` (`src/lib.rs:122`) right after `operator_only`, by `crew_scope::check(store, command, &args)` in a new module, `src/crew_scope.rs`. The identity is resolved once, when the store opens, and kept on the `Store` as `crew: Option<CrewCaller { job, task, actor }>` (new), so the 56 `Caller::Agent { actor }` matches do not change. A new command is therefore refused to crew until it is listed.

| command | crew may call it when |
|---|---|
| `crew.report`, `crew.answer`, `crew.park` | as today (`crew.rs` already checks the job) |
| `job.start`, `job.status`, `job.list`, `job.cancel`, `job.retry` | the job's `task_id` is the crew's own task |
| `note.get`, `note.list`, `focus.digest` | `ship` is the crew's ship and `focus` is `task:<own>` or `assignment:<own>` |
| `task.show` | the id is the crew's own task |
| `workspace.status`, `resource.status`, `runtime.needs`, `ship.list`, `ship.show`, `charter.show`, `upgrade.status` | read-only |

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

## git push and gh

The runtime never sees shell commands, so this layer is enforced through the harness, `PATH` and git configuration. The crew's environment is set by the supervisor from the job record, for crews and their lineage, not inherited, so a crew cannot drop it by starting a heavy job.

### Layers

- **A. Harness deny rules.**
  - *Claude.* `claude_disallowed(role)` (`plugins/tool_permissions.py:66`) gains the following for `role == "crew"`: `Bash(git push:*)`, `Bash(gh pr create:*)`, `Bash(gh pr merge:*)`, `Bash(gh pr edit:*)`, `Bash(gh pr close:*)`, `Bash(gh pr comment:*)`, `Bash(gh pr review:*)`, `Bash(gh pr ready:*)`, `Bash(gh api:*)`, `Bash(gh repo:*)`, `Bash(gh release:*)`, `Bash(gh workflow:*)`, `Bash(gh issue create:*)`, `Bash(gh issue close:*)` and `Bash(gh issue comment:*)`. `claude_rules` keeps `Bash(gh *)` for the first mate only and gives a crew the read-only `gh pr view|diff|checks|list`, `gh run view|list` and `gh issue view|list`. Claude applies a deny before an allow, and the denial is shown to the model with a message. It does not parse `git -C dir push`, `sh -c '...'`, `/usr/bin/git push` or an alias. `permission_mode` `auto` (the haiku and opus routes) and `bypassPermissions` must both be tested to respect `--disallowedTools` patterns (test 7).
  - *opencode.* `OPENCODE_PERMISSION` is already set by the launcher for yolo (`plugins/native_launch.py:133`). A crew gets a bash pattern map with the same denies. The exact schema must be checked against the installed opencode, and so must its headless `run` behaviour on deny.
  - *pi.* pi has no permission layer of its own. Hosted crews already load an extension (`plugins/attended/crew.ts`). A crew guard extension, `plugins/attended/crew-guard.ts` (new), would block bash tool calls that match the deny list, provided the installed pi's tool-call hook can refuse a call (to verify).
  - *agy* (`--dangerously-skip-permissions` on the agy-flash route) has no deny-rule mechanism that this repository uses. *codex* (luna, forced to `danger-full-access` by `codex_crew_sandbox_default`) may support command rules, but that is unverified here. Its `workspace-write` sandbox blocks network, which also blocks fetches and builds. Both rely on B and C.
- **B. git push refused by configuration (all harnesses, any git binary, aliases included).** The crew env gets `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` (git's command-scope configuration) setting `url.privateer-crew://refused.pushInsteadOf` to `https://`, `http://`, `ssh://`, `git@` and `file://`. Every push URL is rewritten to the `privateer-crew` scheme, and git runs the remote helper `git-remote-privateer-crew` (new, in `plugins/crew-bin/`, added to the crew's `PATH`). The helper prints the refusal text and exits 1. Fetch is unaffected. A crew can bypass it by clearing `GIT_CONFIG_COUNT`, or by adding a longer `pushInsteadOf` match of its own, since git picks the longest match. A push to a plain local path is not rewritten; see open question 3.
- **C. gh shim (all harnesses).** `plugins/crew-bin/gh` (new) is first on the crew's `PATH`. It runs the real `gh` (the next one on `PATH` once its own directory is removed) for the read-only subcommands listed under A, plus `gh api` with no `-X`/`--method`, `-f`/`-F`/`--field`/`--raw-field` or `--input`. It refuses everything else with the refusal text. It can be bypassed with an absolute path to `gh`, because the gh token stays readable under the same account.
- **D. A crew credential that cannot write (optional, captain decision).** The crew env could get `GH_CONFIG_DIR` pointing at a crew-only gh config holding a read-only fine-grained token, and `credential.helper` reset to empty (an empty value clears the helpers set in global and repository configuration). Then even `/usr/bin/gh` and a cleared git configuration cannot write. The cost is that the captain creates and rotates the token, and that a crew cannot fetch a private https remote unless the token is read-scoped to it. This is the only layer that holds against deliberate bypass of A to C.

### Per harness

| harness (route) | A deny rules | B git config | C gh shim | bypass without D |
|---|---|---|---|---|
| claude (haiku, opus) | yes, model sees the denial | yes | yes | `GIT_CONFIG_COUNT=0`, absolute `gh`, `sh -c` |
| opencode (opencode-free, opencode-deepseek) | yes, schema to verify | yes | yes | same |
| pi (deepseek-flash) | via extension, to verify | yes | yes | same |
| agy (agy-flash) | none | yes | yes | same |
| codex (luna) | unverified | yes | yes | same |

**Recommendation.** Ship A, B and C together in round 2, and offer D to the captain (open question 1). A gives the model a clear denial before it runs anything. B is the strongest default for git, because it does not depend on how the harness parses shell commands. C covers gh on every harness. The rule a crew sees is the same at each layer: report, and the first mate pushes.

**Block asks.** `block_ask` turns a Claude denial into a captain decision and parks the task (`a_crew_denial_becomes_one_typed_decision_and_parks_its_task_quietly`). A denial covered by a crew role rule is policy, not a gap, so it opens no ask and does not park the task. `block_ask` gets a `role_rule(rule)` filter (new). The captain's `permissions allow` cannot lift a crew role rule.

## The brief

For a crew (`context.crew == true`), `brief` (`src/plugins.rs`) drops the `task.update`, `mail.inbox`/`mail.ack`, `mail.send`, `peer.call`, `task.create`, `task.handoff` and `resource.configure` lines. It says, in this order:

1. "You are crew for task T. Your assignment is in `note.get {"ship":"S","focus":"task:T"}` (field `assignment`). The notes beside it are your first mate's handoff, not your role: never act on your first mate's behalf."
2. The `job.start` and `crew.park` line, as today.
3. "Report with `crew.report` (progress, or a decision to ask) and read answers with `crew.answer`. Your first mate mails, updates the task, pushes and merges: commit locally and report the commit. The runtime refuses `git push`, gh writes and first-mate commands to crew."

The first mate's brief does not change. That keeps its bytes stable, and its cached prefix, which `native_harness_plans_and_brief_are_generic_and_lean` guards. The crew's brief must also stay within that test's size bound. The `## Crew` section of the ship guide (`src/cli.rs:290`) keeps saying that routine `git` and `gh` are pre-approved, and adds "for first mates; crew are refused pushes and gh writes".

Step 1 (the wording, which needs no captain approval) can land before the enforcement.

## Failure modes

- **A crew needs to push a branch for CI.** It does not push. It commits locally and reports the commit, and the first mate pushes. For CI evidence the crew parks on a local gate job (`crew.park`), and remote CI belongs to the first mate. No exception is planned.
- **Errands are not crew.** They keep the first mate's role and their own rules (`one-block-one-ask.md`: an errand's denial asks without parking). A crew cannot start one.
- **A wake or errand mistaken for crew.** Possible only if the wake or errand row is missing when the supervisor writes `agent_payloads`. Activation inserts the wake row before `agent.start`, and errands record theirs first, so the role is fixed at spawn and never re-derived later.
- **A crew resumed from a park.** The resume is a new crew job (`crew-resume-<park>-<n>`). It gets the crew role by the same rule, and its `next` line cannot widen it.
- **A hosted crew** runs in a pane whose payload is the supervisor's child, so the ancestry check holds. Its env scrub and `PATH` come from the same supervisor code.
- **Stale `agent_payloads` rows** after a crash are removed by recovery. A reused pid with a new start time never matches.
- **A test that calls the CLI** inside a crew's heavy job is crew. Tests that need operator commands must run outside crew jobs, which is already true of the supervised gates.

## Migration

No change to a home's schema. The `agent_payloads` table is created in the ledger with `CREATE TABLE IF NOT EXISTS`, as `admission::init` creates its tables. Jobs that are already running when the release lands have no row. They keep today's behaviour until they end, and recovery never inserts a row after the fact. The brief change takes effect at the next launch. The git and gh layers apply only to payloads spawned by a supervisor running the new release.

## Rounds

1. The brief wording and the `note.get` label. Test 1.
2. Identity: `agent_payloads`, ancestry resolution, the subreaper, the env scrub, and `crew_scope::check` with its refusal text. Tests 2 to 6.
3. git and gh: layers A to C, the `block_ask` role-rule filter, and the per-harness checks. Tests 7 to 10. D only if the captain wants it.

Each round is committed after `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the affected suites (`--test-threads=1`) pass as supervised heavy jobs.

## Test plan

Real subprocess boundaries: a fake harness script launched through `agent.start` in isolated homes (HOME, PRIVATEER_FLEET, XDG), as `tests/crew.rs` does today.

1. `tests/plugins.rs` `the_crew_brief_lists_only_crew_commands` (new): the crew brief has no `task.update`, `mail.ack`, `mail.send` or `task.handoff`, says the notes are the first mate's handoff and that the first mate pushes, and stays within the size bound. The first mate's brief is byte-identical to before.
2. `tests/crew.rs` `a_crew_is_refused_first_mate_commands_and_told_to_report` (new): from inside a crew job, every refused command in the list fails with the refusal text and writes nothing. `crew.report`, `job.start` on its own task and `note.get` of its assignment succeed.
3. `tests/crew.rs` `a_crew_that_drops_its_role_is_still_crew` (new): `env -u PRIVATEER_ROLE -u PRIVATEER_ACTOR -u PRIVATEER_JOB` and `setsid -f` variants are refused and audited. The same command from the test process (no job ancestor) runs as operator.
4. `tests/crew.rs` `a_crew_cannot_borrow_a_wakes_job` (new): a crew that sets `PRIVATEER_JOB` to a running wake is refused, and `crew.report` from outside any job is refused even when `PRIVATEER_JOB` names a running crew. `only_the_runtime_speaks_as_crew` is updated for this.
5. `tests/crew.rs` `wakes_and_errands_keep_first_mate_commands` (new): a wake job's script can `mail.ack` and `task.update`, and an errand's can too.
6. `tests/crew.rs` `a_crews_heavy_job_and_another_home_are_crew_too` (new): the CLI called from a crew's child job, or with `--home` of a second home, is refused as crew.
7. `tests/permissions.rs` `crew_launches_deny_pushes_and_gh_writes` (new): a crew's Claude argv carries the deny rules and the read-only gh allows, and a first mate's does not. The opencode crew env carries its bash denies. A live Claude check that `auto` and `bypassPermissions` honour them is a manual step, recorded in the PR.
8. `tests/permissions.rs` `a_crew_role_denial_opens_no_captain_ask` (new).
9. `tests/crew.rs` `a_crew_git_push_is_refused_by_its_remote_helper` (new): `git push` to a `file://` bare repository, via the remote name, an explicit URL and an alias, fails with the refusal text, while `git fetch` works.
10. `tests/crew.rs` `the_crew_gh_shim_allows_reads_and_refuses_writes` (new), with a fake `gh` later on `PATH` that logs its argv.

## Open questions for the captain

1. **Layer D.** Should crews get a read-only GitHub token and an empty git credential helper? It is the only layer a determined model cannot get around under one account. You would create and rotate the token, and a crew could no longer fetch a private https remote without it.
2. **Crew `gh` reads.** Are `gh pr view|diff|checks` and `gh run view` enough for crews to read, or should crews have no gh at all and ask the first mate?
3. **Local pushes.** Should a crew be refused `git push` to a plain local path (the shared clone under `projects/`) too? That needs a `git` shim as well as layer B.
4. **A per-crew OS user.** That is the only true boundary. It changes the workspace, ledger and harness login layout, so it is out of scope here unless you want it planned.
5. **Role-drop refusals.** Should a refused role drop also notify the first mate (a `crew` notice), or only be audited?
