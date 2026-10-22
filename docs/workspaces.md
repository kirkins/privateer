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

A ship's charter can cover its whole folder while its projects are nested Git
repositories. Set `cwd` to the project directory for the first `job.start`,
`agent.start` or explicit `workspace.acquire`. Privateer detects Git there and
pins the task to that source directory. Later jobs that omit `cwd` reuse the
task checkout. Another project requires a separate task or an explicit release
before retargeting. Paths outside the charter remain refused. A non-Git `cwd`
runs directly in its declared directory.

```sh
privateer --home /absolute/home job.start \
  --json '{"task_id":"repair","cwd":"/absolute/ship/projects/app","command":["cargo","test"],"heavy":true,"wait_for_capacity":true}'
```

Install [Treehouse](https://github.com/kunchenguid/treehouse#install) on PATH.
The CLI must support `get --lease --json`, `status --json`, `--worktree-path`,
and conditional return with `--if-lease-id` and `--if-lease-holder`. The local
contract test is qualified against Treehouse 3.1.2. A missing/incompatible
Treehouse fails a Git coding launch; Privateer never falls back to modifying
the shared checkout. Non-Git charters run in their declared directory.
Interactive first mates and dispatcher wake jobs retain their ship repository.

### Machine readiness

Privateer checks the tools it needs before a launch does, on the PATH the
dispatcher launches with (the one its user service records):

- git 2.31 or later, always;
- Treehouse 3.1.2 or later whose `get --help` lists `--lease` and `--json`
  (`treehouse_binary` in config.json, or `treehouse`), when an active
  charter's jobs can land in Git: its repository is in a Git work tree, a
  project under its `projects/` folder is one, or one of its tasks already
  holds a workspace;
- the executables a harness manifest lists in `requires`, such as
  `["python3", "claude"]`, for each local ship's first mate and each route in
  the routes file. A custom manifest without `requires` is reported unchecked.

Missing git, Treehouse, or a first mate's harness blocks: that work cannot
start. A route's missing harness only warns, and route choice passes that
route over for the next on the list, as it does one backing off: the chosen
route's `why` adds `passed over <route>: unavailable: <tool> not installed`.
A harness that is installed but fails its check gives that check's reason
instead. A launch that names such a route is refused with that reason, never
moved to another, and a parked crew on it starts fresh on the next route
rather than resuming; when no route of the kind is left, the refusal lists
each route's reason and how to install every missing tool. This reads the
stored results only: a tool not checked yet counts as installed, and so does
a failing result older than 15 minutes, since a running dispatcher checks
failures again every 5 and an older one means nothing is checking. `routes.set`
and a `ship.put` that sets a first mate check the harnesses they name live on
the command's own PATH and reply with `readiness.missing`, each tool with why
and how to install it; the dispatcher's own check follows within a minute,
and the setting is saved all the same. A tool that is installed but only
on your login shell's PATH is named as such, with the fix: run
`privateer service start` from that shell, so the dispatcher records it.

The check runs live when `privateer init`, `ship.create` or `privateer service
start` starts a dispatcher, whose output then ends with a `Readiness:` summary
of what fails, and once a minute in a running dispatcher, which only stats
the binaries and spawns version probes when a binary changed or its last
result is old (five minutes for a failure, six hours for a pass). That check
runs on its own thread, one at a time, so a slow probe never holds the
dispatcher's pass, which reports what was last stored; `serve --once` only
reports. Each probe, and the login shell asked for its PATH, is killed with
everything it started after five seconds. What fails appears
as a `machine_tool_missing` item in [runtime.needs](runtime-needs.md), as an
alert to the machine's owner on a linked machine, and as a `Machine:` line in
the bearings of each ship whose work it stops. Readiness never installs
anything, and refuses nothing but a launch on a route it found missing. See
[the design](design/machine-readiness.md).

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
`after_id` for pagination. Released records are listed too; `{"active":true}`
leaves them out. `workspace.acquire {task_id}` explicitly allocates or
recovers a checkout before starting jobs. Ordinary launches do this themselves.
Both the host `min_free_disk_bytes` and charter `min_free_bytes` floors apply to
the worktree pool's filesystem. A workspace needs only disk: when the floors are
met it is allocated at once, whatever jobs wait for CPU or memory. Below either
floor it is refused, unless it passes `wait_for_capacity:true` (and optionally
`max_wait_secs`) to join the host queue, where it waits behind earlier
workspaces waiting for disk; `resource.status` includes the workspace queue
entry.

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

Before those checks, once the workspace's jobs are idle, release asks each
[MCP broker](mcp-broker.md#closing-on-release) instance that opened a window
for the workspace to close it: an upstream's window (an Xcode window, in the
[Xcode example](mcp-broker-xcode.md)) can write into the worktree and hold
files there, and the checks must see it gone. Only the instances whose record
lists a window under the workspace are asked, all in parallel under one 35 s
deadline, so a hung broker holds up no other release and a release waits at
most 35 s however many instances there are. If a running instance cannot close
its window, or does not answer in time, the release is refused, the error names
the instance (`could not close the NAME window for this workspace: ...`), and
the lease is kept; `privateer mcp-broker stop --name NAME` is the way out of a
release that keeps failing (see
[A release that keeps failing](mcp-broker.md#closing-on-release)). A recorded
but stopped instance cannot close anything, nor can a running one with no close
helper: the release goes ahead and the window left open is audited as
`workspace.mcp_window_unclosed`, with the `instance` (this was
`workspace.xcode_window_unclosed` before brokers were named). The dispatcher's
sweep closes the windows the same way before it releases a workspace, and a
failure there alerts the task's owner. A record whose Treehouse lease moved to
another task is marked released without the close when another live lease
holds the same checkout: that window is the new holder's now.

To retain clean committed work that has not landed, explicitly pass
`"archive":true` to release. Privateer saves HEAD under
`refs/privateer/workspaces/<home-hash>/<generation>/<head>` before returning the lease.
Each distinct snapshot has its own ref, including when a return fails and a
later release attempt archives different work.
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
