# The MCP broker (Xcode MCP, steps 2 and 3)

`privateer-mcp-broker` holds the one stdio connection to an upstream MCP server
and lets many crew clients share it. On a Mac the upstream is
`/usr/bin/xcrun mcpbridge`, and Xcode asks the user to Allow each new client
binary. With the broker in between, Xcode only ever sees one client: the
broker. Crew reach the broker through its `proxy` mode, which a crew bundle
pins as a [stdio MCP server](tool-bundles.md#selected-mcp-servers-pi-and-claude).

Step 2 added the broker, its install, status and stop commands, and the
dispatcher that keeps it running. Step 3 routes each crew client to the Xcode
window of its own task workspace, opens that window when the workspace first
needs it, and closes it when the workspace is released (see
[Windows and routing](#windows-and-routing-step-3)). Admitting calls by
capacity, pending-Allow notices, and registry entries are steps 4 to 6.

## Why a separate, stable binary

Reinstalling Privateer must never invalidate Xcode's Allow. A release lives at
`releases/<commit>/bin/`, so the main binary's path and hash change at every
upgrade. The broker is therefore its own binary:

- It is `src/bin/privateer-mcp-broker.rs`, with modules beside it. It uses std,
  libc and serde_json, never the Privateer library, and it prints
  `privateer-mcp-broker <BROKER_VERSION>` for `--version`.
- It is installed at one unversioned path:
  `$XDG_DATA_HOME/privateer/mcp-broker/bin/privateer-mcp-broker` (by default
  `~/.local/share/privateer/...`).
- A release ships it as `releases/<commit>/bin/privateer-mcp-broker` (see
  [releases](releases.md#layout)). It is only ever run from the stable path.

## Files

Every broker file is in the run directory,
`$XDG_DATA_HOME/privateer/mcp-broker/run/` (by default
`~/.local/share/privateer/mcp-broker/run/`). It is 0700. The ledger is not used:
sandboxed agents may write the ledger and the ship homes (they are passed to
Codex and OMP as writable directories), and the broker and dispatcher run
outside every sandbox. A run directory that lies inside the ledger or the ship's
home is refused. The files are named for the broker (`xcode` in this step):

| File | Written by | What |
|---|---|---|
| `xcode.json` | `mcp-broker install` | `{"upstream": [ARGV...], "workspace_pools": [ABS...], "window_open": [ARGV...], "window_close": [ARGV...]}` (the helpers only where configured). While it exists, the owner's dispatcher keeps the broker running. |
| `xcode.stopped` | `mcp-broker stop` | Keeps the dispatcher from starting it again until the next install. |
| `xcode.lock` | broker | flock held for the broker's life: one broker per directory and name. |
| `xcode.sock` | broker | The socket, 0600. |
| `xcode.pid` | broker | Its pid. |
| `xcode.state.json` | broker | Version, pid, upstream state, pid, generation, restarts and last exit; the clients (claimed pid and cwd from their hello, the socket's peer pid, the peer's verified cwd, its workspace `root` or null, and its window's `tab`); the workspace pools; and `windows`, the record below. |
| `xcode.windows.json` | broker | `{"windows": [{root, project, tab, opened_ms}]}`: the Xcode windows the broker opened, and only those. Read back at broker start, so a restarted broker still closes them. Kept when the broker stops. |
| `xcode.log` | broker | One-line events, never message bodies. Cut over to `xcode.log.1` past 1 MiB. |

`install` also writes `$XDG_DATA_HOME/privateer/mcp-broker/xcode.json.sha256`
beside the stable binary, the hash of the config it wrote. The dispatcher starts
the broker only while `xcode.json` still matches that pin, so a rewritten config
is never run, even if something did reach the run directory.

As defence in depth, the broker and the dispatcher open each of these files with
`O_NOFOLLOW` (and `O_NONBLOCK`, so a FIFO cannot hang them), then check with
`fstat` that it is a regular file this user owns with one link, and refuse it
otherwise. Files are replaced through a new temporary file beside them, created
with `O_EXCL`, then renamed, so neither a planted temporary file nor a symlink at
the target is ever written through. Client- and upstream-supplied text in the log
(the hello's cwd, method names, upstream stderr) is quoted with quotes,
backslashes and control characters escaped.

Because the run directory follows `XDG_DATA_HOME` (or `HOME`), not the home's
`resource_home`, `install` resolves the same directory whether or not it runs
in a ship folder, and so does the owner's dispatcher. Both must see the same
`XDG_DATA_HOME`: a service unit that sets a different one than the operator's
shell looks somewhere else, as it would for the stable binary.

## Where the socket lives, and the sandboxes

The socket is `run/xcode.sock`. A crew must be able to `connect()` to it but
never unlink, replace or bind it, or plant a file beside it. Crew sandboxes
allow writes only to the task's workspace, the ship home, the ledger and, by
default, the temporary directories. The run directory is none of these, so:

- `connect()` works. On Linux it needs write permission on the socket inode,
  which DAC grants (same uid, 0600), and a read-only bind mount does not apply
  to socket inodes. Landlock does not mediate connecting to a path socket.
- `unlink`, `bind` and creating files fail, because the directory is outside
  every writable root.

Measured on this Linux host (aarch64, kernel 7.1) with a broker in a run
directory outside the writable roots:

| Sandbox | connect | unlink | rebind | create | symlink |
|---|---|---|---|---|---|
| `bwrap --ro-bind / /` (+ `--unshare-net`), as Claude's Linux sandbox mounts | works | EROFS | EROFS | EROFS | EROFS |
| `codex sandbox`, workspace-write, network on | works | EROFS | EROFS | EROFS | EROFS |
| `codex sandbox`, workspace-write, network off (the default) | EPERM | EROFS | EROFS | EROFS | EROFS |

Codex's network seccomp filter refuses `connect()` even on Unix sockets.
Claude's own sandbox, where enabled, also restricts Unix sockets
(`allowUnixSockets` on macOS; a seccomp filter on Linux unless
`allowAllUnixSockets`). None of this blocks the broker today, because the proxy
is never run inside those sandboxes:

- The proxy is a stdio MCP server that the harness itself spawns. Claude Code's
  sandbox wraps its Bash tool, not its MCP servers, and Pi has no OS sandbox.
  Step 6 limits this bundle to Claude and Pi.
- Privateer does not enable Claude's sandbox. A user `sandbox` setting is
  refused, not carried (`plugins/claude_bundles.py`).

If a sandbox ever does wrap the proxy, the smallest change is to allow just this
socket path: for Claude, `sandbox.network.allowUnixSockets: ["<run>/xcode.sock"]`
on macOS (Claude's documentation gives Linux only `allowAllUnixSockets`, since
its seccomp filter cannot match paths). Codex has no
per-path Unix-socket allowance; its `network_access` would open all networking,
which is too wide. No sandbox setting is changed by this step. Not measured:
macOS (seatbelt) and Claude's MCP-server spawning.

## Commands

```sh
privateer mcp-broker install [--workspace-pool ABS]... [--window-open JSON_ARGV] [--window-close JSON_ARGV]
                             [--upstream /ABS/COMMAND [ARG...]]   # operator only
privateer mcp-broker status                                       # anyone, agents included
privateer mcp-broker stop                                         # operator only
```

**install** copies the broker shipped with the running Privateer to the stable
path. That is the running release's `bin/privateer-mcp-broker`, or, for a
source build, the one beside the running binary.

- It copies only when the stable file is missing or its `--version` output
  differs from the shipped one. The copy goes to a temporary file in the same
  directory, which is fsynced, made 0755 and renamed over the old one.
- With the same version, nothing changes, not even the inode.
- It writes `xcode.json` with the upstream (default `/usr/bin/xcrun mcpbridge`;
  element 0 must be an absolute, normalized path), the workspace pools and the
  window helpers (see [Windows and routing](#windows-and-routing-step-3)), and
  clears `xcode.stopped`. `--upstream` takes the rest of the command line, so
  it comes last.
- It prints the binary's path and `sha256`, and `bundle_command`:
  `[STABLE_PATH, "proxy", "--socket", SOCKET]`. That command and that hash are
  what a crew bundle pins. The pin follows the broker version: a new version
  means a new hash.

A running broker keeps its binary and upstream until it stops; install never
restarts it. To change the upstream of a running broker, run `stop`, then
`install`.

**status** reads the files above and runs the stable binary's `--version`. It
reports the version, sha256, whether a broker holds the lock, its pid, the
socket, and the broker's own state (upstream `starting`, `ready`, `restarting`
or `stopping`, with its pid and restart count).

**stop** writes `xcode.stopped` first, so the dispatcher does not start the
broker again meanwhile. It checks that the pid file names a process running the
broker binary. Before signalling anything, it checks the upstream process group
named in the state file: the group is signalled only if it is a group leader in
the verified broker's session (a dispatcher-started broker leads its own) or the
broker's child. A pgid that is neither is reported as `unverified_upstream_pid`
and never signalled. Then stop sends SIGTERM to the broker. The broker ends its
upstream's whole process group (SIGTERM, then SIGKILL after 3 s) and exits. If
the lock is still held after 10 s, stop sends SIGKILL to the broker. It then
checks the verified upstream group by itself. If anything is left and the group
is still in the broker's session, it sends SIGKILL. It reports `upstream_gone`
(null when no group was verified). A killed broker is not taken as proof that
its upstream stopped.

## Lifetime

This replaces the plan's "the broker lives as long as the dispatcher". Keeping
the broker running across Privateer restarts keeps Xcode's Allow.

- Each dispatcher pass (`activation::tick`, beside the machine-owner alerts)
  checks the broker, at most once every 30 s.
- Only the machine owner's dispatcher does this: the ship named by
  `privateer machine owner`, in its own home.
- If `xcode.json` exists and matches its pin, `xcode.stopped` is absent, and no
  process holds the lock, it starts the installed stable binary as
  `serve --state-dir $XDG_DATA_HOME/privateer/mcp-broker/run --name xcode -- UPSTREAM...`.
- The broker starts detached: in its own session (`setsid`), with stdin
  `/dev/null`, stdout and stderr on `xcode.log`, and no inherited descriptor. Its
  environment is only `PATH=/usr/bin:/bin`, `HOME`, `LANG`, `LC_*`, `TMPDIR` and
  `DEVELOPER_DIR` if set.
- The dispatcher never installs or replaces the binary. Configured but not
  installed is reported as an error.
- The broker outlives dispatcher restarts and upgrades. Service units use
  `KillMode=process`, and the broker is in its own session, so stopping the
  dispatcher does not reach it.
- It stops on `mcp-broker stop`, or when the owner's dispatcher finds
  `xcode.json` gone, which runs the same stop.

## How the broker serves

`serve --state-dir DIR --name NAME [--ready-wait-secs 60] [--workspace-pool ABS]... [--window-open JSON_ARGV] [--window-close JSON_ARGV] -- UPSTREAM...`

The dispatcher passes the pools and helpers from the pinned config.

**One per directory and name.** The broker takes `DIR/NAME.lock` with
`flock`. A second `serve` exits non-zero within half a second: it retries
briefly, so that a status probe, which holds the lock for an instant, never
stops a start. Under the lock, the broker makes `DIR` 0700, replaces a stale
socket, and binds `NAME.sock` 0600. It accepts only peers with its own uid
(`SO_PEERCRED` on Linux, `getpeereid` on macOS). It also reads the peer's pid
from the socket: `SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS.

**Upstream.**

- The broker spawns the upstream in `/`, with only `PATH=/usr/bin:/bin`, `HOME`,
  `LANG`, `LC_*`, `TMPDIR` and `DEVELOPER_DIR` if set, in a process group of its
  own. On Linux it also gets a parent-death SIGKILL, so a broker killed outright
  takes its upstream with it.
- The broker sends its own `initialize`, with protocol `2025-06-18` and client
  `privateer-mcp-broker`, then `notifications/initialized` with `"params":{}`.
  It caches the InitializeResult.
- The broker never times out an initialize. Xcode may be waiting for the user to
  Allow, and killing the upstream would discard that prompt.
- An upstream that exits is restarted after 1 s, doubling to 60 s, and
  re-initialized. After 60 s ready, the next exit starts again from 1 s.

**Messages.** Messages are newline-delimited JSON-RPC, at most 16 MiB a line.
An oversize line is read through and refused (`-32600`, id null), and the
connection carries on. Batches are refused (`-32600`).

Per client:

- `initialize` is answered from the cache. `notifications/initialized` is
  swallowed. `ping` is answered locally.
- Every other request goes upstream under a broker-unique id, and the answer
  comes back under the client's own id, so two clients can both use id `1`.
- `params._meta.progressToken` becomes `privateer-<broker id>` upstream and is
  restored on the way back. A client's `notifications/cancelled` is forwarded
  with the broker id. Other client notifications are dropped.
- When a client disconnects, each request it left in flight gets
  `notifications/cancelled` upstream (`reason: "client disconnected"`), and
  their answers are dropped.
- The first line from a proxy is its hello,
  `{"privateer_proxy":1,"cwd":...,"pid":...,"project":...}`. Its pid and cwd
  are claims: the broker records them in the state file and log, beside the
  peer pid the socket reports and the cwd the system reports for that pid, and
  logs a mismatch. Routing goes by the socket peer, never the hello (see
  [identity](#which-workspace-a-client-is-in)). The optional `project` comes
  from the proxy's `--project`.
- The first line may instead be a [control line](#closing-on-release).
- A client that stops reading has 256 lines queued for it, then it is
  disconnected rather than allowed to hold the broker up.

From the upstream:

- An answer goes only to the client that asked.
- Notifications go to every client, except progress, which goes only to its
  token's owner.
- A server-to-client request (`sampling/createMessage`, `roots/list`, and so
  on) gets error `-32601`, because no one client may answer for the others.
- If the upstream dies mid-call, each in-flight `tools/call` returns
  `isError: true` with `upstream exited; the call may or may not have run`.
  Other in-flight requests get error `-32603` with the same meaning. A call is
  never retried, because it may have run.

**Busy, never a hang.** No tool call is held open waiting for capacity. While the
upstream is not initialized, a request waits at most `--ready-wait-secs`
(default 60). After that it gets the busy answer from `busy::busy`, the one
function admission (step 4) will reuse:

```json
{"isError": true,
 "content": [{"type": "text", "text": "Xcode tools busy: <reason>; retry after N s"}],
 "_meta": {"privateer/retry_after_secs": N}}
```

That is the result for a `tools/call`. Any other request gets a JSON-RPC error,
code `-32000`, with the same message and `data.retry_after_secs: N`. N is 15
while the upstream starts or waits to be allowed, and the time left in the
restart backoff plus 5 s after an exit.

On SIGTERM or SIGINT the broker ends its upstream's group, gives clients the
answers already queued for them (the `upstream exited` results for calls the
stop cut short), waiting at most 1 s, then removes the socket, pid and state
files and exits 0. SIGHUP is ignored.

## The proxy

`proxy --socket ABS [--connect-wait-secs 60] [--project REL]` is the command
crew bundles pin.
The step-1 launcher (`plugins/mcp_stdio.py`) runs it with a minimal
environment, so everything it needs is in argv.

- It connects, retrying every 200 ms until the wait runs out. Then it prints
  one line on stderr and exits 69.
- It checks that the socket's peer has its own uid.
- It sends the hello: its working directory, resolved by `realpath`, its pid,
  and `--project` if given (a path relative to the workspace root that picks
  the project its workspace's window opens).
- Then it copies bytes both ways. It exits when stdin closes, which closes the
  connection, so the broker cancels anything left in flight. It also exits when
  the broker closes.

## Windows and routing (step 3)

Many crews share the one broker, and Xcode builds or edits in whichever window
a tool call names. Without routing, a call that names no window lands in
whichever window Xcode picks. The broker therefore sends each crew's calls to
the window of that crew's own task workspace. Routing prevents mistakes between
honest crews; it is not a barrier against a hostile one (see [Limits](#limits)).

### Which workspace a client is in

A client is identified by its socket peer, never by its hello:

- Its pid comes from the socket (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on
  macOS).
- At accept, before the hello is read, the broker reads that pid's working
  directory from the system (`/proc/PID/cwd` on Linux, `proc_pidinfo`
  `PROC_PIDVNODEPATHINFO` on macOS) and resolves it with `realpath`.
- The client's workspace root is the nearest ancestor of that directory (or
  the directory itself) that holds a `.git` entry and lies strictly inside a
  configured workspace pool.
- Anything else leaves the client unrouted: no pool, no `.git` inside a pool,
  no peer pid, or no readable cwd. The hello's cwd is never a fallback. When it
  differs from the verified cwd, the mismatch is logged, escaped.

The proxy runs in the crew's working directory (the harness spawns it there),
so a crew's calls route to its own worktree.

**Pools.** `xcode.json` lists `workspace_pools`. `mcp-broker install` always
writes `<ledger>/worktrees`, the directory each home's Treehouse pool is made
in (`<ledger>/worktrees/<home hash>`), with the ledger resolved as install
resolves `resource_home`. `--workspace-pool ABS`, repeatable, adds others. Each
must be absolute, normalized and existing, and is written resolved. The config
pin covers them.

### Tool calls

After each `initialize`, the broker sends its own `tools/list` under a broker
id that is never delivered to a client, following `nextCursor`. It caches each
tool's `inputSchema`, and lists again on `notifications/tools/list_changed`
(which still reaches every client). A tool is **window-bound** when its
`inputSchema.properties` has `tabIdentifier`, or when a call passes a
`tabIdentifier`. A tool the cache does not show makes the broker list the tools
again, once per call. Still missing, a call that passes a `tabIdentifier` is
refused (`the upstream does not list the tool X`), and any other call is
forwarded as window-free.

For each client `tools/call`:

- **Path arguments** are checked first, for every tool. An argument is a path
  when it is named `path`, `file`, `uri`, `url`, `source`, `destination`,
  `target`, `dir` or `directory` (or a plural of these), or when its name ends
  in `Path`, `path`, `File`, `Dir`, `Directory`, `Uri` or `Url` (or a plural).
  Its string value, and every string in arrays under it, must stay in the
  client's workspace. Nested objects and arrays are searched too.
  - A `file:` URL is checked as the path it names, percent-decoded. A URL for
    another host is refused. Under a `uri` or `url` name, another scheme
    (`https:`) names no file and passes.
  - An absolute value must resolve inside the root: the realpath of its deepest
    existing ancestor, with no `..` in the rest.
  - A relative value is taken from the root and must resolve inside it the same
    way, so neither `..` nor a symlink in the worktree leads out.
  - An unrouted client may pass no absolute path, and no relative one holding
    `..`.
- **`XcodeListWindows`** is forwarded, and its answer is filtered to the
  client's root, in `structuredContent.windows`, in a text item holding the
  same JSON, and in text lines (empty when the client is unrouted). The broker
  also learns every root's window from it.
- **A window-bound tool from a routed client** gets the root's window. An
  absent `tabIdentifier` is filled in. The same value passes. Any other value
  is refused with `tabIdentifier X is not this workspace's Xcode window; call
  XcodeListWindows`.
- **A window-bound tool from an unrouted client** is refused with `Xcode window
  tools work only from a Privateer task workspace`.
- **Window-free tools** (`DocumentationSearch`, for example) pass for any
  client.

A refusal is a tool result with `isError: true`, and the call is never
forwarded.

**Which window is the root's.** `windows(result)` in
`src/bin/privateer-mcp-broker/route.rs` is the one parser of an
XcodeListWindows result. It reads `structuredContent.windows[{tabIdentifier,
workspacePath}]`, the same JSON as text, and text lines that carry
`tabIdentifier: X` and `workspacePath: Y`. A window belongs to a root when the
realpath of its `workspacePath` lies inside the root, by path components (not
a string prefix). Those realpaths are taken before the broker's state lock, so
no filesystem call runs under it. The root's tab is cached. The cache is filled by a
broker-internal XcodeListWindows when a call finds no cached tab, and refreshed
by every window list that passes through the broker. A window the user closed
therefore stays cached until the next list: the call fails upstream, and the
client's XcodeListWindows (as the refusal text suggests) clears it.

### Opening, lazily

A window-bound call whose root has no window opens one:

- **The project** is the proxy's `--project REL` when given. It must resolve
  inside the root to a `.xcworkspace`, a `.xcodeproj` or a directory holding
  `Package.swift`; otherwise the call is refused. Without it, the broker
  searches the client's verified cwd, then the root, at most 3 levels deep. It
  skips hidden directories (`.build` included), `Pods`, `DerivedData`,
  `node_modules`, symlinks, and anything inside a `.xcodeproj` or
  `.xcworkspace`. A single `.xcworkspace` wins, then a single `.xcodeproj`, then
  a single `Package.swift` directory. More than one gives an error listing the
  candidates relative to the root.
- **One open per root at a time.** The broker runs `window_open` with the
  project's absolute path appended, on a thread of its own. The call that
  started it, and other calls of that root, wait on the same opening, but only
  until their call's deadline (`--ready-wait-secs` from its start): then they
  get `busy::busy` with `Xcode is opening this workspace's window`, retry after
  15 s. The helper keeps running, and its outcome is recorded when it ends. For
  120 s after a successful open, a root whose window has not shown is still
  opening, and its calls wait for the window rather than open again.
- **Waiting for the window.** Once the helper has exited 0, the broker lists
  windows every second until one belongs to the root, within the same
  deadline. Past that, the call gets the same busy answer. No tool call is held
  past `--ready-wait-secs` (default 60 s).
- **A failed open.** If the helper exits non-zero, the calls waiting on it get
  an error with the helper's first stderr line, escaped. It is never retried
  within a call; the next call opens again.
- **The record.** `{root, project, tab, opened_ms}` goes into
  `run/xcode.windows.json` when the helper starts, and is dropped if it fails.
  The file is written with the same temporary-file, `O_NOFOLLOW` and `fstat`
  rules as every broker file, and `tab` is filled in once a list shows the
  window. Only windows the broker opened are recorded.

### Closing on release

A connection may send a control line instead of a hello:

```json
{"privateer_control": 1, "close_workspace": "/ABS/ROOT"}
```

`ROOT` must be absolute and normalized, and it (or its realpath) must lie
strictly inside a configured workspace pool: `/`, a pool itself and anything
outside the pools are refused with an error. The broker runs `window_close` for
each recorded window under that root, drops those windows from the record and
the root's tab from the cache, and answers one line: `{"closed": [PROJECT...]}`
or `{"error": "..."}`. An open still running for a root under it is marked to
close its window when the helper ends, and is listed as `"opening":
[PROJECT...]` beside `closed`. Windows the broker did not open are never
closed. Any same-uid peer may send the line. A spurious close is only an
inconvenience: the window reopens on the next call.

`close_workspace(root)` in `src/mcp_broker.rs` sends it. It first reads
`xcode.windows.json` (with the same link and owner checks as every broker file)
and returns at once, without connecting, when no window is recorded under the
root, so a broker that hangs holds up only the release of a workspace it opened
a window for. Otherwise, with no socket or a free lock, the broker is not
running: it returns `NotRunning` and audits `workspace.xcode_window_unclosed`.
With a running broker, it connects without blocking (a hung broker's full
listen backlog cannot stall it) and checks the peer's uid, all within a 35 s
overall timeout. That is longer than the broker's 30 s helper deadline, so a
close helper the broker had to kill comes back as the broker's error rather
than as silence. Workspace release calls it once the workspace's jobs are idle
and before the uncommitted-work and process checks. Xcode can write into the worktree and
hold files there, and those checks must see it gone. An error from a running
broker refuses the release (`could not close the Xcode window for this
workspace: ...`), and the lease is kept. The dispatcher's sweep calls it too,
before any path that marks a workspace released; a failure there goes through
the sweep's alert to the task's owner. When the sweep finds that a record's
Treehouse lease moved to another task, it skips the close if another live lease
holds the same checkout, since that window is the new holder's now. See
[workspaces](workspaces.md#inspect-and-release).

### Helpers and defaults

`xcode.json` may name `"window_open": [ABS...]` and `"window_close":
[ABS...]`, set with install's `--window-open` and `--window-close` (each a JSON
array). Each is checked like the upstream: element 0 is an absolute, normalized
path, and it may not be a shell (`sh`, `bash`, `zsh`, `env` and the like). The
project's absolute path is appended as the last argument. It is never part of
a script.

- **macOS defaults**, written by install:
  - open: `["/usr/bin/open", "-g", "-a", "Xcode"]`. `-g` keeps the captain's
    focus; drop it if the Mac shows that the window then fails to open.
  - close: `/usr/bin/osascript` with a fixed script, one `-e` per line. It
    takes the recorded project path as `item 1 of argv`. If Xcode is running,
    it closes, `saving no`, every workspace document whose `path` is that path
    or starts with it followed by `/` (Xcode may report an `X.xcodeproj` as
    `X.xcodeproj/project.xcworkspace`). It walks the documents last first, so
    closing one does not shift the rest, and exits 0 when none matches. It
    never launches Xcode just to close a window. The path is never part of the
    script.
- **Elsewhere there are no defaults.** A window-bound call that needs a window
  opened is refused with `no window helper configured`. Tests configure fakes.

Helpers run with the broker's minimal environment (as the upstream's), in `/`,
with stdin `/dev/null`, at most 64 KiB of stderr read, in their own process
group, and with a 30 s deadline that kills the group. A helper never runs while
the broker's state lock is held.

### Not verified (Mac only, step 3)

- the real XcodeListWindows result shape, which tools take `tabIdentifier`, and
  whether Xcode's file tools take absolute or project-relative paths;
- `open -g -a Xcode PROJECT` opening a window that XcodeListWindows then lists,
  and how long that takes;
- the AppleScript close (`workspace document`, `path`, `saving no`, `starts
  with`, and walking the documents last first);
- that the close exits 0 when no document matches, and when Xcode is not
  running;
- which document path Xcode reports for an opened `.xcodeproj` (the project
  itself, or `X.xcodeproj/project.xcworkspace`) and for a `Package.swift`
  directory;
- a denied Automation prompt: the close then fails, so the release of every
  workspace with a recorded window is refused (and the sweep alerts its owner)
  until the Allow is given in System Settings. `privateer mcp-broker stop` is
  the way out: with no broker running, release goes ahead and audits the window
  left open;
- whether any Xcode tool passes source text or a build target name under one
  of the path argument names (`source`, `target`); such a value is checked as a
  path, and source text could be refused;
- the one-time Automation prompt ("privateer-mcp-broker wants to control
  Xcode"), and which binary macOS holds responsible for it: the stable broker,
  or the launchd dispatcher's release binary. In the second case the Allow
  would lapse at every upgrade;
- whether Xcode writes non-ignored files (`xcshareddata/IDEWorkspaceChecks.plist`)
  into the worktree, which would block release's clean check;
- whether Treehouse's process check counts Xcode as a process in the worktree;
- the cwd from which Claude and Pi spawn the stdio proxy (expected: the job's
  workspace);
- `LOCAL_PEERPID` and `proc_pidinfo`, beyond what macOS CI covers.

## Tests

`cargo test --test mcp_broker -- --test-threads=1` runs the real broker, proxy
and dispatcher processes against `tests/fixtures/mcp-broker/fake_mcpbridge.py`.
The fake is scriptable by argv to delay initialize, crash after N calls, emit
notifications, progress or a server request, and record every message. It also
offers `XcodeListWindows` (reading a windows file, in either result form),
window-bound `XcodeRead` and `BuildProject`, window-free `DocumentationSearch`,
and a tool that changes the tool list. `fake_xcode_window.py` stands in for the
open and close helpers: open adds a window to that file (optionally later, or
never, or fails), close removes it, and both record their argv. Workspaces are
real git worktrees under a temporary pool. Each test uses its own HOME, XDG
data directory, ledger and fleet registry.

The suite covers:

- two clients both using id 1;
- cancel on disconnect;
- an upstream crash mid-call, followed by restart and re-initialize;
- busy within the ready wait plus 1 s;
- one broker per directory, the 0700 and 0600 modes, and a stale socket;
- a symlinked lock refusing a start, and a symlinked log, state, pid and state
  temporary file never written through (their targets untouched); the
  dispatcher refusing a symlinked log;
- `-32601`, broadcast, the progress-token round trip, batches and oversize
  lines;
- the proxy's realpath hello and peer pid, its three exits, and a call in flight
  at SIGTERM getting its `upstream exited` answer;
- a pinned copy of the broker run through `plugins/mcp_stdio.py`
  (`tests/mcp_broker_launcher.py`). This one skips where the executable rule
  refuses the checkout's path. The Rust test prints `SKIPPED ...: <reason>` (or
  `RAN ...`), visible with `--nocapture`, and with `CI` set any other skip
  fails;
- install's version decision and sha256, and its config pin; the run directory
  outside the ledger, and refused inside it; install from outside any ship
  folder found by an owner whose home sets another `resource_home`;
- the dispatcher starting the broker in its own session, only for the machine
  owner and not while the lock is held, and never for a rewritten config;
- a forged upstream pgid in the state file not signalled;
- stop leaving no upstream process, and config removal stopping it;
- two workspaces' calls each reaching their own tab, another workspace's tab
  refused and never forwarded;
- XcodeListWindows filtered per client in both forms, and empty outside every
  pool;
- a raw client whose hello claims another worktree routed by its real cwd,
  with the mismatch logged; window tools refused and DocumentationSearch passed
  outside the pools; the peer pid in the state file;
- path arguments outside the root or with `..` refused and not forwarded;
- lazy open: discovery's preferences, ambiguous projects, `--project` inside
  and outside the root, and a failing helper (whose window leaves the record);
- a slow open (`fake_xcode_window.py --hold`): two concurrent clients cause one
  open, and the opener and the waiter both get busy within the ready wait plus
  1 s, the waiter on the running open (no window list while it waits), then the
  next call uses the window;
- a window that never shows giving the busy answer within the ready wait plus
  1 s;
- close: only the windows the broker opened, and after a broker restart;
  control roots of `/`, a pool, a path outside the pools, a relative or
  non-normalized path refused; a close during an open closing the window when
  the open ends;
- a changed tool list refreshing which tools are window-bound; a tool missing
  from the list listed again once, refused with a `tabIdentifier`, forwarded
  without one;
- install writing the pools and helpers into the pinned config, and refusing
  relative, non-normalized or shell helpers and bad pools.

`cargo test --test workspaces` checks that release closes a recorded window
before its clean check, that a failing close keeps the lease, that release
proceeds and audits the window with no broker, and that sweep closes too. A
broker that accepts and never answers does not delay a release with no
recorded window, and refuses one with a recorded window after 35 s, keeping the
lease. Sweep leaves the window of a checkout another task's lease now holds,
and closes the one of a checkout nobody holds.

`cargo test --bin privateer-mcp-broker` checks the routing rules directly:
window parsing and filtering, path arguments (names, `file:` URLs, arrays,
relative paths and symlinks), discovery and roots.

`cargo test --lib mcp_broker` checks the dispatcher's temporary-file and
upstream-group checks directly, a connect to a listener with a full backlog
failing by its deadline, and the Mac close script passing the helper checks, and `cargo test --test releases` checks that the
release layout ships the broker and that install takes it from a release.

## Not verified (Mac only)

None of these can be checked on Linux:

- the real `xcrun mcpbridge` behind the broker;
- whether the Allow survives a broker restart, a Privateer upgrade and a broker
  reinstall;
- whether mcpbridge takes concurrent calls (the broker sends them as they
  come);
- whether a detached broker survives launchd restarting the dispatcher;
- `DEVELOPER_DIR` and `xcode-select` selection through the broker's minimal
  environment;
- codesigning of the copied binary, and whether Gatekeeper or Xcode treat the
  copy differently from the release's original.

## Limits

- Any process running as this user can send the close control line for any
  workspace, or `chdir` into another workspace and act as its crew. Routing
  prevents mistakes; it is not a barrier against a hostile crew.
- A pid is read once, at accept. A client that changes directory afterwards
  keeps the root it connected with.
- A window the user closes stays cached for its root until the next window
  list; the call that finds it gone fails upstream.
- A tool call that needs a window blocks that client's connection while the
  window is looked up or opened, up to `--ready-wait-secs`.
- A `close_workspace` runs the close helper once per recorded window under the
  root, each with a 30 s deadline. A root with more than one recorded window
  can therefore outlast the release's 35 s wait; the release is then refused
  and the lease kept.
- A window whose open helper exits before Xcode shows it (`open -g` returns at
  once) can appear after a close that found no document to close. It is then
  left open, and no longer recorded.

- Sandboxes keep crews out of the run directory, not the user's own unsandboxed
  processes. Any process running as this user outside a sandbox can still stop
  the broker and listen on its socket in its place. The peer-uid check and the
  0700 directory keep out other users.
- A stuck upstream that neither answers nor exits holds its in-flight calls
  until it does. Admission (step 4) bounds this.
- One broker name, `xcode`, is wired into the commands and the dispatcher.
- The owner's dispatcher tick can block for up to 16 s in `stop_running` when
  it stops a broker whose config is gone (10 s for the lock, then up to 3 s
  each for SIGKILL and the upstream group).
- Clients get the cached InitializeResult, including the `protocolVersion` the
  broker negotiated, whatever version they asked for.
- `resources/subscribe` and `resources/unsubscribe` go upstream as they are:
  subscriptions are shared, so one client's unsubscribe ends the subscription
  for every client.
