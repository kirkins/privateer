# Task workspaces

Privateer automatically gives Git-backed coding tasks a durable Treehouse
worktree. `job.start` and crew `agent.start` use it without a separate plugin or
an interactive Treehouse shell. Different tasks get different branches and
directories; repeated and nested jobs for one task share its checkout. Both
the process working directory and `PWD` point there. Agent adapters receive
that path in their validated context, including sandbox workspace arguments.
Only one crew harness can hold a task checkout at a time. Create a child task
for another independent coding worker; that harness's build/test jobs can use
its existing task workspace.

Install [Treehouse](https://github.com/kunchenguid/treehouse#install) on PATH.
The CLI must support `get --lease --json`, `status --json`, `--worktree-path`,
and conditional return with `--if-lease-id` and `--if-lease-holder`. The local
contract test is qualified against Treehouse 3.1.2. A missing/incompatible
Treehouse fails a Git coding launch; Privateer never falls back to modifying
the shared checkout. Non-Git charters run in their declared directory.
Interactive first mates and dispatcher wake jobs retain their ship repository.

## Ownership and location

The workspace belongs to the task in the Privateer home, with custody following
the task's recorded owner. Agents can inspect/use/release only their own task
workspaces. A previous owner's workspace workers must stop before a new owner
can use the checkout; those workers lose execution authority on handoff.

Pools live below the configured host resource directory at
`worktrees/<home-hash>/`, separately from the project clones. Each task gets a
generated `privateer/...` branch. Privateer pins the source Git common directory,
charter directory, branch, Treehouse executable, lease ID and holder. Another
clone's worktree, a replaced lease, a changed branch, or a path outside the
managed pool is refused. A charter scoped to a repository subdirectory keeps
that same scope inside the worktree. Existing jobs and retries remain pinned to
their original lease; they cannot silently move into a recycled slot.
Replaying an identical completed job/agent request ID returns its recorded
result even after release, without acquiring another checkout. A retry with a
new ID still requires the original lease; after release, use a fresh task or
fresh agent planning rather than replaying an old plan into a recycled slot.

An operator can set an absolute `treehouse_binary` in the home's private
`config.json` when the CLI is not on the runtime's PATH. This affects new
allocations; existing workspace records retain their executable and pool path.

## Inspect and release

All commands accept the ordinary `--home` and `--json`/`--file` protocol:

```sh
privateer --home /absolute/home workspace.list --json '{}'
privateer --home /absolute/home workspace.status --json '{"task_id":"repair"}'
```

`workspace.list` is bounded (100 records by default, at most 1,000); use
`after_id` for pagination. `workspace.acquire {task_id}` explicitly allocates or
recovers a checkout before starting jobs. Ordinary launches do this themselves.

Job completion, failure, cancellation and task completion retain the lease.
After jobs stop, commit any outstanding edits and finish the PR/merge, then use
the exact `lease_id` from status:

```sh
privateer --home /absolute/home workspace.release \
  --json '{"task_id":"repair","lease_id":"<recorded lease>"}'
```

Release checks recorded supervisors and payload groups, Treehouse's process
observations, and tracked/untracked edits. It refreshes origin when present and
requires the current HEAD's changes to be contained in the recorded base branch;
a squash merge can qualify through a Git merge-tree content comparison. Unknown
or conflicting containment preserves the checkout. Ignored build caches remain
available for reuse under Treehouse's ordinary cleanup rules.

To retain clean committed work that has not landed, explicitly pass
`"archive":true` to release. Privateer saves HEAD under
`refs/privateer/workspaces/<home-hash>/<generation>` before returning the lease.
That ref can reconstruct a checkout with `git worktree add --detach`. Archive
mode still refuses uncommitted work and live/uncertain jobs. There is no
force-discard command. A stale lease ID cannot return a newer acquisition.

## Restart recovery

Allocation and release intents are durable SQLite records. Per-home operations
are serialized by an OS file lock, inherited by the helper while it executes.
Treehouse calls use explicit argv, null stdin, a 120-second deadline, bounded
output, and a separate process group. An unsuccessful or interrupted command
does not imply that a lease was released. Retrying acquisition finds an existing
lease by its unique holder instead of allocating a second checkout.

`workspace.recover {task_id,generation}` inspects an interrupted operation without
starting new work, including after a task ends or its charter is revoked. Use
the exact generation from status. It adopts a completed allocation, records
that no lease was acquired, or acknowledges a completed return. An unfinished
return with its lease still present remains retained until an authorized
release passes the checks again. This does not revoke or return foreign leases.

An externally changed/missing lease or damaged worktree is preserved for operator
inspection. Do not manually recycle a leased task checkout. Keep installed
Privateer binaries, adapters and persistent ship state outside disposable task
worktrees.

## Validation

`cargo test --locked --all-features --test workspaces` uses a private Treehouse
protocol fixture backed by real Git worktrees and public CLI subprocesses. It
does not require Treehouse or contact a model. To exercise the installed CLI
against a disposable local repository and private pool:

```sh
PRIVATEER_TEST_TREEHOUSE=/absolute/path/to/treehouse \
  cargo test --locked --all-features --test workspaces installed_treehouse_contract -- --ignored
```

The installed-CLI check is explicitly skipped in ordinary CI when Treehouse is
not provisioned. Neither check uses live ship homes or production installations.

Treehouse provisioning hooks are trusted setup work inside the bounded helper,
before payload resource admission. Schedule expensive setup as admitted jobs
after allocation; a provisioning hook must not recursively acquire a workspace
from the same home while the allocation lock is held.
