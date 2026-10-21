# The MCP broker (Xcode MCP, step 2)

`privateer-mcp-broker` holds the one stdio connection to an upstream MCP server
and lets many crew clients share it. On a Mac the upstream is
`/usr/bin/xcrun mcpbridge`, and Xcode asks the user to Allow each new client
binary. With the broker in between, Xcode only ever sees one client: the
broker. Crew reach the broker through its `proxy` mode, which a crew bundle
pins as a [stdio MCP server](tool-bundles.md#selected-mcp-servers-pi-and-claude).

This step adds the broker, its install, status and stop commands, and the
dispatcher that keeps it running. It does not route by window or tab, open or
close Xcode windows, admit calls by capacity, send pending-Allow notices, or add
any bundle to a registry. Those are steps 3 to 6.

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
| `xcode.json` | `mcp-broker install` | `{"upstream": [ARGV...]}`. While it exists, the owner's dispatcher keeps the broker running. |
| `xcode.stopped` | `mcp-broker stop` | Keeps the dispatcher from starting it again until the next install. |
| `xcode.lock` | broker | flock held for the broker's life: one broker per directory and name. |
| `xcode.sock` | broker | The socket, 0600. |
| `xcode.pid` | broker | Its pid. |
| `xcode.state.json` | broker | Version, pid, upstream state, pid, generation, restarts and last exit, and the clients (claimed pid, the socket's peer pid, and cwd from their hello). |
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
privateer mcp-broker install [--upstream /ABS/COMMAND [ARG...]]   # operator only
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
  element 0 must be an absolute, normalized path) and clears `xcode.stopped`.
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

`serve --state-dir DIR --name NAME [--ready-wait-secs 60] -- UPSTREAM...`

**One per directory and name.** The broker takes `DIR/NAME.lock` with
`flock`. A second `serve` exits non-zero within half a second: it retries
briefly, so that a status probe, which holds the lock for an instant, never
stops a start. Under the lock, the broker makes `DIR` 0700, replaces a stale
socket, and binds `NAME.sock` 0600. It accepts only peers with its own uid
(`SO_PEERCRED` on Linux, `getpeereid` on macOS).

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
- The first line from a proxy is its hello, `{"privateer_proxy":1,"cwd":...,"pid":...}`.
  The broker records it in the state file and log, beside the peer pid the
  socket reports (`SO_PEERCRED` on Linux; macOS's `getpeereid` gives no pid, so
  none is recorded there), and logs a mismatch. Nothing is routed by it yet;
  step 3 maps it to the window for that cwd's workspace.
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

`proxy --socket ABS [--connect-wait-secs 60]` is the command crew bundles pin.
The step-1 launcher (`plugins/mcp_stdio.py`) runs it with a minimal
environment, so everything it needs is in argv.

- It connects, retrying every 200 ms until the wait runs out. Then it prints
  one line on stderr and exits 69.
- It checks that the socket's peer has its own uid.
- It sends the hello: its working directory, resolved by `realpath`, and its
  pid.
- Then it copies bytes both ways. It exits when stdin closes, which closes the
  connection, so the broker cancels anything left in flight. It also exits when
  the broker closes.

## Tests

`cargo test --test mcp_broker -- --test-threads=1` runs the real broker, proxy
and dispatcher processes against `tests/fixtures/mcp-broker/fake_mcpbridge.py`.
The fake is scriptable by argv to delay initialize, crash after N calls, emit
notifications, progress or a server request, and record every message. Each
test uses its own HOME, XDG data directory, ledger and fleet registry.

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
- stop leaving no upstream process, and config removal stopping it.

`cargo test --lib mcp_broker` checks the dispatcher's temporary-file and
upstream-group checks directly, and `cargo test --test releases` checks that the
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
