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

The broker's state directory is `<ledger>/mcp-broker/`. `<ledger>` is the
resource ledger: the home's `resource_home`, else `PRIVATEER_RESOURCE_HOME`,
else `~/.local/state/privateer-host`. The directory is 0700. Its files are
named for the broker (`xcode` in this step):

| File | Written by | What |
|---|---|---|
| `xcode.json` | `mcp-broker install` | `{"upstream": [ARGV...]}`. While it exists, the owner's dispatcher keeps the broker running. |
| `xcode.stopped` | `mcp-broker stop` | Keeps the dispatcher from starting it again until the next install. |
| `xcode.lock` | broker | flock held for the broker's life: one broker per directory and name. |
| `xcode.sock` | broker | The socket, 0600. |
| `xcode.pid` | broker | Its pid. |
| `xcode.state.json` | broker | Version, pid, upstream state, pid, generation, restarts and last exit, and the clients (pid and cwd from their hello). |
| `xcode.log` | broker | One-line events, never message bodies. Cut over to `xcode.log.1` past 1 MiB. |

`install` also writes `$XDG_DATA_HOME/privateer/mcp-broker/xcode.json.sha256`,
the hash of the config it wrote. The ledger is writable by sandboxed agents, so
the dispatcher starts the broker only while `xcode.json` still matches that pin.
An agent that rewrites the config cannot make the dispatcher run its own
command outside its job.

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
broker binary, then sends SIGTERM. The broker ends its upstream's whole process
group (SIGTERM, then SIGKILL after 3 s) and exits. If the lock is still held
after 10 s, stop sends SIGKILL to the broker. It then checks the upstream's
process group by itself, killing it if anything is left, and reports
`upstream_gone`. A killed broker is not taken as proof that its upstream
stopped.

## Lifetime

This replaces the plan's "the broker lives as long as the dispatcher". Keeping
the broker running across Privateer restarts keeps Xcode's Allow.

- Each dispatcher pass (`activation::tick`, beside the machine-owner alerts)
  checks the broker, at most once every 30 s.
- Only the machine owner's dispatcher does this: the ship named by
  `privateer machine owner`, in its own home.
- If `xcode.json` exists and matches its pin, `xcode.stopped` is absent, and no
  process holds the lock, it starts the installed stable binary as
  `serve --state-dir <ledger>/mcp-broker --name xcode -- UPSTREAM...`.
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

- The broker spawns the upstream with only `PATH=/usr/bin:/bin`, `HOME`,
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
  The broker records it in the state file and log. Nothing is routed by it yet;
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

On SIGTERM or SIGINT the broker ends its upstream's group, removes the socket,
pid and state files, and exits 0. SIGHUP is ignored.

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
- `-32601`, broadcast, the progress-token round trip, batches and oversize
  lines;
- the proxy's realpath hello and its three exits;
- a pinned copy of the broker run through `plugins/mcp_stdio.py`
  (`tests/mcp_broker_launcher.py`). This one skips with the reason where the
  executable rule refuses the checkout's path;
- install's version decision and sha256, and its config pin;
- the dispatcher starting the broker in its own session, only for the machine
  owner and not while the lock is held, and never for a rewritten config;
- stop leaving no upstream process, and config removal stopping it.

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

- Any process running as this user can connect to the socket, replace the
  ledger files, or stop the broker and listen on its socket in its place. The
  peer-uid check and the 0700 directory keep out other users, not other agents
  running as the same user.
- A stuck upstream that neither answers nor exits holds its in-flight calls
  until it does. Admission (step 4) bounds this.
- One broker name, `xcode`, is wired into the commands and the dispatcher.
