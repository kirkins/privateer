# The MCP broker

`privateer-mcp-broker` holds the one stdio connection to an upstream MCP server
and lets many crew clients share it. An upstream that must see only one client
(one that asks the user to approve each new client binary, for example) then
sees only the broker. Crew reach the broker through its `proxy` mode, which a
crew bundle pins as a
[stdio MCP server](tool-bundles.md#selected-mcp-servers-pi-and-claude).

The broker names no use case. A machine runs any number of **instances**, each
with a name, an upstream, and an optional **routing profile** that says how the
upstream's tools bind to the windows of task workspaces: which tool lists the
windows, which argument names one, how the list reads, what a workspace's
project looks like, and which helpers open and close a window. Specific tools
ship as example profiles. [Xcode](mcp-broker-xcode.md) is the first:
`docs/examples/mcp-broker/xcode.json`.

Step 2 added the broker, its install, status and stop commands, and the
dispatcher that keeps it running. Step 3 routes each crew client to the window
of its own task workspace, opens that window when the workspace first needs
it, and closes it when the workspace is released (see
[Windows and routing](#windows-and-routing)). The steps after it configure,
rather than build in:

- step 4 makes admission instance config: which tools count as heavy, and a
  limit per target, with Xcode's builds as the example;
- step 5 raises pending-approval notices from a signal the profile declares,
  with Xcode's Allow prompt as the example;
- step 6 adds the Xcode example as an opt-in bundle and registry entry, the
  Claude/Pi-only rule, and the Mac runbook.

## Why a separate, stable binary

Reinstalling Privateer must never invalidate what an upstream allowed. A
release lives at `releases/<commit>/bin/`, so the main binary's path and hash
change at every upgrade. The broker is therefore its own binary:

- It is `src/bin/privateer-mcp-broker.rs`, with modules beside it. It uses std,
  libc and serde_json, never the Privateer library, and it prints
  `privateer-mcp-broker <BROKER_VERSION>` for `--version`.
- It is installed at one unversioned path:
  `$XDG_DATA_HOME/privateer/mcp-broker/bin/privateer-mcp-broker` (by default
  `~/.local/share/privateer/...`).
- A release ships it as `releases/<commit>/bin/privateer-mcp-broker` (see
  [releases](releases.md#layout)). It is only ever run from the stable path.
- It holds the one profile validator, `check-profile` (see
  [The profile](#the-profile)).

## Instances

An instance is a name with a config pin (below). Its name matches
`^[a-z][a-z0-9-]{0,15}$`: it names every file of the instance, so it never
holds a path, and it is short so that the socket path fits macOS's limit.
`serve` takes the same rule. Each instance has its own files, lock, socket,
upstream, log and window record, and one instance's stop, error or hang never
touches another's.

## Files

Every broker file is in the run directory,
`$XDG_DATA_HOME/privateer/mcp-broker/run/` (by default
`~/.local/share/privateer/mcp-broker/run/`). It is 0700. The ledger is not used:
sandboxed agents may write the ledger and the ship homes (they are passed to
Codex and OMP as writable directories), and the broker and dispatcher run
outside every sandbox. A run directory that lies inside the ledger or the ship's
home is refused. The files are named for the instance:

| File | Written by | What |
|---|---|---|
| `NAME.json` | `mcp-broker install` | The config: `{"upstream": [ARGV...], "workspace_pools": [ABS...], "label": ..., "path_arguments": {...}, "routing": {...}}`, the last three as `check-profile` printed them. While it exists, the owner's dispatcher keeps the instance running. |
| `NAME.stopped` | `mcp-broker stop` | Keeps the dispatcher from starting it again until the next install. |
| `NAME.lock` | broker | flock held for the broker's life: one broker per directory and name. |
| `NAME.sock` | broker | The socket, 0600. |
| `NAME.pid` | broker | Its pid. |
| `NAME.state.json` | broker | Version, pid, name, label, whether it routes, upstream state, pid, generation, restarts and last exit; the clients (claimed pid and cwd from their hello, the socket's peer pid, the peer's verified cwd, its workspace `root` or null, and its window's `target`); the workspace pools; and `windows`, the record below. |
| `NAME.windows.json` | broker | `{"windows": [{root, project, target, opened_ms}]}`: the windows the broker opened, and only those. Read back at broker start, so a restarted broker still closes them. Kept when the broker stops. |
| `NAME.log` | broker | One-line events, never message bodies. Cut over to `NAME.log.1` past 1 MiB. |

`install` also writes `$XDG_DATA_HOME/privateer/mcp-broker/NAME.json.sha256`
beside the stable binary, the hash of the config it wrote. The dispatcher starts
an instance only while `NAME.json` still matches that pin, so a rewritten config
is never run, even if something did reach the run directory. The profile is
copied into the config, so the pin covers it too.

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

The socket is `run/NAME.sock`. macOS caps a socket path at 104 bytes
(`sun_path`), so `install` refuses an instance whose socket path is 104 bytes or
longer and says to set `XDG_DATA_HOME` to a shorter directory.

A crew must be able to `connect()` to the socket but never unlink, replace or
bind it, or plant a file beside it. Crew sandboxes allow writes only to the
task's workspace, the ship home, the ledger and, by default, the temporary
directories. The run directory is none of these, so:

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
  Step 6 limits the bundle to Claude and Pi.
- Privateer does not enable Claude's sandbox. A user `sandbox` setting is
  refused, not carried (`plugins/claude_bundles.py`).

If a sandbox ever does wrap the proxy, the smallest change is to allow just this
socket path: for Claude, `sandbox.network.allowUnixSockets: ["<run>/NAME.sock"]`
on macOS (Claude's documentation gives Linux only `allowAllUnixSockets`, since
its seccomp filter cannot match paths). Codex has no
per-path Unix-socket allowance; its `network_access` would open all networking,
which is too wide. No sandbox setting is changed by this step. Not measured:
macOS (seatbelt) and Claude's MCP-server spawning.

## Commands

```sh
privateer mcp-broker install --name NAME [--profile FILE] [--workspace-pool ABS]...
                             [--window-open JSON_ARGV] [--window-close JSON_ARGV]
                             [--upstream /ABS/COMMAND [ARG...]]   # operator only
privateer mcp-broker status [--name NAME]                         # anyone, agents included
privateer mcp-broker stop --name NAME                             # operator only
```

**install** copies the broker shipped with the running Privateer to the stable
path, and writes and pins the instance's config. The broker is the running
release's `bin/privateer-mcp-broker`, or, for a source build, the one beside
the running binary.

- It copies only when the stable file is missing or its `--version` output
  differs from the shipped one. The copy goes to a temporary file in the same
  directory, which is fsynced, made 0755 and renamed over the old one.
- With the same version, nothing changes, not even the inode.
- It reads `--profile FILE` (at most 1 MiB), then lets the flags override it:
  `--upstream` sets its `upstream`, and `--window-open` and `--window-close`
  (each a JSON array) set its `routing.open` and `routing.close`, which needs a
  profile with `routing`. That is how tests swap in fake helpers. A profile
  without a `label` gets the instance's name. The result goes through the
  shipped broker's `check-profile`, and the canonical form it prints is what is
  written. A refusal prints `check-profile`'s one line and writes nothing.
- There are no defaults: the upstream comes from `--upstream` or the profile's
  `upstream`, and with neither, install refuses. `--upstream` takes the rest of
  the command line, so it comes last.
- It writes `NAME.json` with the upstream, the workspace pools (see
  [Pools](#which-workspace-a-client-is-in)) and the profile's label, path
  arguments and routing, writes its pin, and clears `NAME.stopped`.
- It prints the binary's path and `sha256`, the config, and `bundle_command`:
  `[STABLE_PATH, "proxy", "--socket", SOCKET]`. That command and that hash are
  what a crew bundle pins. The pin follows the broker version: a new version
  means a new hash.

A running broker keeps its binary and config until it stops; install never
restarts it. To change a running instance, run `stop --name NAME`, then
`install`. There is no uninstall command yet.

**status** reads the files above and runs the stable binary's `--version`. It
reports the binary's version and sha256 once, and under `instances`, for every
instance (or only `--name NAME`, which must be one): its config, label,
upstream, pools, whether it routes, its recorded windows, whether it is stopped
or running, its pid, socket and log, and the broker's own state (upstream
`starting`, `ready`, `restarting` or `stopping`, with its pid and restart
count).

**stop --name NAME** stops that instance only. It writes `NAME.stopped` first,
so the dispatcher does not start it again meanwhile. It checks that the pid
file names a process running the broker binary. Before signalling anything, it
checks the upstream process group named in the state file: the group is
signalled only if it is a group leader in the verified broker's session (a
dispatcher-started broker leads its own) or the broker's child. A pgid that is
neither is reported as `unverified_upstream_pid` and never signalled. Then stop
sends SIGTERM to the broker. The broker ends its upstream's whole process group
(SIGTERM, then SIGKILL after 3 s) and exits. If the lock is still held after
10 s, stop sends SIGKILL to the broker. It then checks the verified upstream
group by itself. If anything is left and the group is still in the broker's
session, it sends SIGKILL. It reports `upstream_gone` (null when no group was
verified). A killed broker is not taken as proof that its upstream stopped.

## The profile

A profile is one JSON object. Unknown keys are refused at every level, since a
typo must not silently turn routing off. Every key but `routing` applies to any
instance; without `routing`, the instance is a plain shared broker: no tool is
window-bound and nothing is recorded. If an earlier install of the same name
recorded windows, a release still asks it about them, and it answers that it
cannot close them (see [Closing on release](#closing-on-release)), so a plain
instance never holds a release.

```json
{"label": "Fake",
 "upstream": ["/ABS/COMMAND", "ARG"],
 "path_arguments": {"names": ["where"], "suffixes": ["Location"]},
 "env_pass": ["FAKE_HOME_DIR"],
 "routing": {
   "target_argument": "windowId",
   "list": {"tool": "ListWindows", "items": "/structuredContent/windows",
            "target_field": "windowId", "path_field": "root",
            "text_target": "windowId: ", "text_path": "root: "},
   "project": {"kinds": [{"dir_suffix": ".proj"}, {"dir_with_file": "Fake.toml"}],
               "max_depth": 3, "skip_dirs": ["vendor"], "skip_hidden": true},
   "open": ["/ABS/OPEN-HELPER"],
   "close": ["/ABS/CLOSE-HELPER"]}}
```

| Key | Meaning and limits |
|---|---|
| `label` | How busy answers and refusals name the upstream ("Fake tools busy: ..."). 1 to 32 printable characters. Defaults to the instance's name. |
| `upstream` | The upstream's argv: 1 to 32 arguments of at most 4096 bytes, no control characters, the first an absolute, normalized path. |
| `path_arguments.names`, `.suffixes` | Argument names, and endings of argument names, that hold paths, at most 32 of each, each 1 to 64 characters. They only add to the built-in rule (see [Tool calls](#tool-calls)); nothing removes it. |
| `env_pass` | Environment variables passed through, when set, to the broker, its upstream and its window helpers, beyond the minimal environment (see [Upstream](#how-the-broker-serves)). At most 32 names of `A`-`Z`, `0`-`9` and `_`, not starting with a digit, at most 64 characters each. `PATH` (always `/usr/bin:/bin`), `LD_*` and `DYLD_*` (they load code into every helper) and the variables always passed are refused. |
| `routing.target_argument` | The argument that names a window. A tool is window-bound when its `inputSchema.properties` has it. |
| `routing.list.tool` | The tool that lists the upstream's windows. |
| `routing.list.items` | A JSON pointer to the array of windows. It is applied to the result, and to each text item that parses as JSON; because MCP mirrors `structuredContent` in a text item, a pointer starting with `/structuredContent` is also applied to the text JSON without that first part, and a text item whose JSON is a bare array is taken as the list itself. |
| `routing.list.target_field`, `.path_field` | Each window entry's fields: its target (what `target_argument` takes) and its path. |
| `routing.list.text_target`, `.text_path` | Both or neither, each 1 to 64 characters. With them, a text line names a window: the token after `text_target` (up to a comma, semicolon or space) and the rest of the line after `text_path`. |
| `routing.project.kinds` | 1 to 16 kinds of project, in preference order. `{"dir_suffix": ".EXT"}` is a directory whose name ends in a suffix that starts with `.` and holds no `/`. `{"dir_with_file": "NAME"}` is a directory holding a regular file of that plain name. |
| `routing.project.max_depth` | How many levels below a base discovery looks, 1 to 6 (default 3). |
| `routing.project.skip_dirs` | Directory names discovery never enters, at most 32 plain names. |
| `routing.project.skip_hidden` | Whether discovery passes over directories whose names start with `.` (default false). |
| `routing.open`, `routing.close` | The helpers that open and close a workspace's window, checked like the upstream and never a shell (`sh`, `bash`, `zsh`, `env`, `xargs` and the like). `open` requires `close`, since a window the broker opens must close on release. Without `open`, a window-bound call that finds no window is refused with `no window helper configured`. |

Without `routing.project`, a window opens on the workspace root itself.

Kinds replace globs: the broker uses std, libc and serde_json only, and a
directory suffix and a marker file cover what project discovery needs without a
pattern language.

**One validator.** `privateer-mcp-broker check-profile` reads a profile on
stdin and prints its canonical JSON (every default spelled out, keys in order),
or exits 2 with one error line naming the key. `install` runs it on the shipped
binary and pins its output. The dispatcher passes the pinned parts to `serve`
as `--label`, `--path-arguments JSON`, `--env-pass JSON` and `--routing JSON`, and `serve`
validates them again.

## Lifetime

Keeping the broker running across Privateer restarts keeps whatever its
upstream allowed.

- Each dispatcher pass (`activation::tick`, beside the machine-owner alerts)
  checks the instances, at most once every 30 s for all of them.
- Only the machine owner's dispatcher does this: the ship named by
  `privateer machine owner`, in its own home.
- It walks every instance (every name with a pin), and for each one: if
  `NAME.json` is gone and a broker holds the lock, it stops it; if
  `NAME.stopped` exists or a broker holds the lock, it leaves it; if the config
  does not match its pin, that instance is an error; otherwise it starts the
  installed stable binary as
  `serve --state-dir $XDG_DATA_HOME/privateer/mcp-broker/run --name NAME [--label ...] [--workspace-pool ...]... [--path-arguments ...] [--env-pass ...] [--routing ...] -- UPSTREAM...`.
- It reports `{NAME: outcome}` for the instances where something happened
  (`started`, `stopped` or `error`), and null when nothing did. One instance's
  error never skips another.
- The broker starts detached: in its own session (`setsid`), with stdin
  `/dev/null`, stdout and stderr on `NAME.log`, and no inherited descriptor. Its
  environment is only `PATH=/usr/bin:/bin`, `HOME`, `LANG`, `LC_*`, `TMPDIR` and
  the profile's `env_pass`, each if set.
- The dispatcher never installs or replaces the binary. Configured but not
  installed is reported as that instance's error.
- The broker outlives dispatcher restarts and upgrades. Service units use
  `KillMode=process`, and the broker is in its own session, so stopping the
  dispatcher does not reach it.
- It stops on `mcp-broker stop --name NAME`, or when the owner's dispatcher
  finds `NAME.json` gone, which runs the same stop.
- A config of the first shape (`{"upstream": [...]}` alone, as step 2 wrote
  `xcode.json`), with its pin, still starts: as a plain instance of that name,
  with no routing.

## How the broker serves

`serve --state-dir DIR --name NAME [--label TEXT] [--ready-wait-secs 60] [--workspace-pool ABS]... [--path-arguments JSON] [--env-pass JSON] [--routing JSON] -- UPSTREAM...`

The dispatcher passes the label, pools, path arguments, env_pass and routing from the
pinned config.

**One per directory and name.** The broker takes `DIR/NAME.lock` with
`flock`. A second `serve` exits non-zero within half a second: it retries
briefly, so that a status probe, which holds the lock for an instant, never
stops a start. Under the lock, the broker makes `DIR` 0700, replaces a stale
socket, and binds `NAME.sock` 0600. It accepts only peers with its own uid
(`SO_PEERCRED` on Linux, `getpeereid` on macOS). It also reads the peer's pid
from the socket: `SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS.

**Upstream.**

- The broker spawns the upstream in `/`, with only `PATH=/usr/bin:/bin`, `HOME`,
  `LANG`, `LC_*`, `TMPDIR` and the profile's `env_pass`, each if set, in a process group of its
  own. On Linux it also gets a parent-death SIGKILL, so a broker killed outright
  takes its upstream with it.
- The broker sends its own `initialize`, with protocol `2025-06-18` and client
  `privateer-mcp-broker`, then `notifications/initialized` with `"params":{}`.
  It caches the InitializeResult.
- The broker never times out an initialize. The upstream may be waiting for the
  user to approve the broker, and killing it would discard that prompt.
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
 "content": [{"type": "text", "text": "LABEL tools busy: <reason>; retry after N s"}],
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

## Windows and routing

Many crews share one broker, and an upstream with windows (an IDE's open
projects, for example) works in whichever window a tool call names. Without
routing, a call that names no window lands in whichever window the upstream
picks. An instance with a `routing` profile therefore sends each crew's calls
to the window of that crew's own task workspace. Routing prevents mistakes
between honest crews; it is not a barrier against a hostile one (see
[Limits](#limits)). Below, `TARGET` is the profile's `target_argument`, `LIST`
its `list.tool`, and `LABEL` its label.

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

**Pools.** `NAME.json` lists `workspace_pools`. `mcp-broker install` always
writes `<ledger>/worktrees`, the directory each home's Treehouse pool is made
in (`<ledger>/worktrees/<home hash>`), with the ledger resolved as install
resolves `resource_home`. `--workspace-pool ABS`, repeatable, adds others (at
most 16 pools in all). Each must be absolute, normalized and existing, and is
written resolved. The config pin covers them.

### Tool calls

**Path arguments** are checked first, for every tool of every instance, routed
or not. An argument is a path when it is named `path`, `file`, `uri`, `url`,
`source`, `destination`, `target`, `dir` or `directory` (or a plural of these),
or when its name ends in `Path`, `path`, `File`, `Dir`, `Directory`, `Uri` or
`Url` (or a plural), or when the profile's `path_arguments` adds its name or
ending. Its string value, and every string in arrays under it, must stay in the
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

With routing, after each `initialize` the broker sends its own `tools/list`
under a broker id that is never delivered to a client, following `nextCursor`.
It caches each tool's `inputSchema`, and lists again on
`notifications/tools/list_changed` (which still reaches every client). A tool is
**window-bound** when its `inputSchema.properties` has `TARGET`, or when a call
passes a `TARGET`. A tool the cache does not show makes the broker list the
tools again, once per call. Still missing, a call that passes a `TARGET` is
refused (`the upstream does not list the tool X; a call naming a TARGET is not
forwarded`), and any other call is forwarded as window-free.

For each client `tools/call` of a routed instance:

- **`LIST`** is forwarded, and its answer is filtered to the client's root, in
  every form the profile reads (empty when the client is unrouted). The broker
  also learns every root's window from it.
- **A window-bound tool from a routed client** gets the root's window. An
  absent `TARGET` is filled in (arguments that are not an object are replaced).
  The same value passes. Any other value is refused with
  `TARGET X is not this workspace's LABEL window; call LIST`.
- **A window-bound tool from an unrouted client** is refused with
  `LABEL window tools work only from a Privateer task workspace`.
- **Window-free tools** pass for any client.

A refusal is a tool result with `isError: true`, and the call is never
forwarded. A plain instance forwards every call once its path arguments pass.

**Which window is the root's.** `windows(result, list)` in
`src/bin/privateer-mcp-broker/route.rs` is the one parser of a window list,
driven by the profile's `list` (see [The profile](#the-profile)). A window
belongs to a root when the realpath of its path lies inside the root, by path
components (not a string prefix). Those realpaths are taken before the broker's
state lock, so no filesystem call runs under it. The root's target is cached.
The cache is filled by a broker-internal `LIST` call when a call finds no
cached target, and refreshed by every window list that passes through the
broker. A window the user closed therefore stays cached until the next list:
the call fails upstream, and the client's `LIST` (as the refusal text suggests)
clears it.

### Opening, lazily

A window-bound call whose root has no window opens one:

- **The project** is the proxy's `--project REL` when given. It must resolve
  inside the root to a directory of one of the profile's kinds (or to a file a
  `dir_with_file` kind names, which stands for its directory); otherwise the
  call is refused. Without it, the broker searches the client's verified cwd,
  then the root, at most `max_depth` levels deep. A base that is itself of a
  kind is a candidate. It skips `skip_dirs`, hidden directories with
  `skip_hidden`, and symlinks, and it never descends into a directory that
  matched a kind. The kinds are in preference order: a single match of the
  first kind wins, then of the next. More than one of a kind gives `more than
  one project in this workspace: A, B; start the proxy with --project REL`,
  relative to the root; none gives `no project in this workspace (looked for
  K1, K2, ...)`. Without `project` kinds, the window opens on the root, and
  `--project` may name any directory inside it.
- **One open per root at a time.** The broker runs `routing.open` with the
  project's absolute path appended, on a thread of its own. The call that
  started it, and other calls of that root, wait on the same opening, but only
  until their call's deadline (`--ready-wait-secs` from its start): then they
  get `busy::busy` with `LABEL is opening this workspace's window`, retry after
  15 s. The helper keeps running, and its outcome is recorded when it ends. For
  120 s after a successful open, a root whose window has not shown is still
  opening, and its calls wait for the window rather than open again.
- **Waiting for the window.** Once the helper has exited 0, the broker lists
  windows every second until one belongs to the root, within the same
  deadline. Past that, the call gets the same busy answer. No tool call is held
  past `--ready-wait-secs` (default 60 s).
- **A failed open.** If the helper exits non-zero, the calls waiting on it get
  `could not open this workspace's LABEL window: ...` with the helper's first
  stderr line, escaped. It is never retried within a call; the next call opens
  again.
- **The record.** `{root, project, target, opened_ms}` goes into
  `run/NAME.windows.json` when the helper starts, and is dropped if it fails.
  The file is written with the same temporary-file, `O_NOFOLLOW` and `fstat`
  rules as every broker file, and `target` is filled in once a list shows the
  window. Only windows the broker opened are recorded.

### Closing on release

A connection may send a control line instead of a hello:

```json
{"privateer_control": 1, "close_workspace": "/ABS/ROOT"}
```

`ROOT` must be absolute and normalized, and it (or its realpath) must lie
strictly inside a configured workspace pool: `/`, a pool itself and anything
outside the pools are refused with an error. The broker runs `routing.close`
for each recorded window under that root, drops those windows from the record
and the root's target from the cache, and answers one line:
`{"closed": [PROJECT...]}` or `{"error": "..."}`. An open still running for a
root under it is marked to close its window when the helper ends, and is listed
as `"opening": [PROJECT...]` beside `closed`. An instance with no
`routing.close` (one reinstalled without the profile whose routing recorded
its windows) drops those windows from its record and lists them as
`"unclosable": [PROJECT...]` beside `closed`, rather than failing. Windows the broker did not open
are never closed. Any same-uid peer may send the line. A spurious close is only
an inconvenience: the window reopens on the next call.

**The release hook.** Workspace release calls `release_mcp_windows`
(`src/workspace/lifecycle.rs`), which calls
`mcp_broker::release_workspace(store, root)`. It serves every instance:

- It reads every `NAME.windows.json` in the run directory (with the same link
  and owner checks as every broker file) and asks only the instances that
  record a window under the root. An instance with nothing recorded there is
  never connected to, so a broker that hangs holds up only the release of a
  workspace it opened a window for.
- A recorded instance with no socket or a free lock is not running: its
  windows are audited as `workspace.mcp_window_unclosed`, with `instance`,
  `root`, `windows` and `reason`, and the release proceeds. (Before brokers
  were named this was `workspace.xcode_window_unclosed`.)
- The running ones are asked in parallel, each connecting without blocking (a
  hung broker's full listen backlog cannot stall it) and checking the peer's
  uid, all under one shared 35 s deadline. A release therefore waits at most
  35 s however many instances it asks. That is longer than the broker's 30 s
  helper deadline, so a close helper the broker had to kill comes back as the
  broker's error rather than as silence. Each wait for the answer is at most a
  second against the deadline, because a long socket timeout fires late (about
  38.6 s for 35 s on this Linux host).
- A running instance's `unclosable` windows are audited the same way, with the
  reason `the MCP broker has no close helper`, and the release proceeds.
- Any running instance that answers with an error, or not in time, refuses the
  release, and the lease is kept. The error names each such instance:
  `could not close the NAME window for this workspace: ...`.

**A release that keeps failing.** A window stays in `NAME.windows.json` until
its close succeeds. The record survives a broker stop and restart, and the next
task in the same reused workspace slot is asked to close it again. So a close
helper that fails every time refuses every release of that slot. The way out:

- `privateer mcp-broker stop --name NAME`. A stopped instance is never asked:
  the release proceeds and audits the window as `workspace.mcp_window_unclosed`
  (the window itself stays open, and the record stays until a later close
  succeeds or it is removed).
- Or, once the window is closed by hand, remove `run/NAME.windows.json` (with
  the instance stopped, since a running broker rewrites it from memory), which
  forgets every window that instance recorded.

See `privateer mcp-broker status --name NAME` for the recorded `windows`.

Release calls the hook once the workspace's jobs are idle and before the
uncommitted-work and process checks: an upstream's window can write into the
worktree and hold files there, and those checks must see it gone. Every exit
from `ready` goes through it. The dispatcher's sweep calls it too, before any
path that marks a workspace released; a failure there goes through the sweep's
alert to the task's owner. When the sweep finds that a record's Treehouse
lease moved to another task, it skips the close if another live lease holds the
same checkout, since that window is the new holder's now. See
[workspaces](workspaces.md#inspect-and-release).

### Helpers

`routing.open` and `routing.close` are checked like the upstream: element 0 is
an absolute, normalized path, at most 32 arguments, no control characters, and
never a shell. The project's absolute path is appended as the last argument. It
is never part of a script.

A helper reports success by exiting 0, and failure by exiting non-zero with the
reason on its first stderr line. **A close helper must exit 0 when there is
nothing to close**: the window already closed by hand, the application not
running, or no window showing that project. A failed close keeps the window in
the record and refuses the release, and the next release of the same slot asks
again, so a helper that fails on an already-closed window refuses releases
until it is stopped (see [A release that keeps failing](#closing-on-release)).
A close helper should also never start the application just to close it.

Helpers run with the broker's minimal environment (as the upstream's), in `/`,
with stdin `/dev/null`, at most 64 KiB of stderr read, in their own process
group, and with a 30 s deadline that kills the group. A helper never runs while
the broker's state lock is held.

## Tests

`cargo test --test mcp_broker -- --test-threads=1` runs the real broker, proxy
and dispatcher processes against `tests/fixtures/mcp-broker/fake_mcpbridge.py`,
a generic fake upstream. Its `--profile FILE` (by default
`tests/fixtures/mcp-broker/fake_profile.json`) gives it the list tool, target
argument and fields to answer with, from the same file the broker pins. The
fake is scriptable by argv to delay initialize, crash after N calls, emit
notifications, progress or a server request, and record every message. It also
offers the profile's list tool (reading a windows file, in either result form),
window-bound `Read` and `Build`, window-free `Search`, and `Grep`, which
`retool` makes window-bound. `fake_window.py` stands in for the open and close
helpers: open adds a window to that file (optionally later, or never, or
fails), close removes it, and both record their argv. Workspaces are real git
worktrees under a temporary pool. Each test uses its own HOME, XDG data
directory (a short `/tmp/pd...` one, so socket paths fit macOS), ledger and
fleet registry.

The routing tests use the fake profile (label `Fake`, list tool `ListWindows`,
target `windowId`, fields `windowId` and `root`, kinds `.proj` then a
`Fake.toml` directory, `skip_dirs: ["vendor"]`). The suite covers:

- two clients both using id 1;
- cancel on disconnect;
- an upstream crash mid-call, followed by restart and re-initialize;
- busy within the ready wait plus 1 s, labelled by the instance's name;
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
- stop needing `--name`, leaving no upstream process, and config removal
  stopping it;
- two instances, `fake` with routing and `two` plain: one's pin mismatch an
  error of its own while the other starts, status listing both, `stop --name
  fake` leaving `two` running, and each with its own files;
- a pinned config of the first shape (`xcode.json`, upstream only) starting
  and serving as a plain instance;
- install refusing, each with its cause: no upstream; a bad name (`Xcode`, 17
  characters, `../x`); an unknown profile key at each level; a shell helper;
  open without close; a relative or non-normalized helper; a bad kind suffix; a
  helper flag with no routing; a bad pool; and a socket path of 104 bytes or
  more, from a long `XDG_DATA_HOME`;
- two workspaces' calls each reaching their own window, another workspace's
  target refused and never forwarded;
- the list filtered per client in both forms, and empty outside every pool;
- a raw client whose hello claims another worktree routed by its real cwd,
  with the mismatch logged; window tools refused and window-free tools passed
  outside the pools; the peer pid in the state file;
- path arguments outside the root or with `..` refused and not forwarded;
- lazy open: discovery by kinds (a `.proj` over a `Fake.toml` directory,
  nothing inside a matched directory, under `vendor` or hidden), ambiguous
  projects, no project, `--project` inside and outside the root, and a failing
  helper (whose window leaves the record); no open helper refused;
- a slow open (`fake_window.py --hold`): two concurrent clients cause one
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
  from the list listed again once, refused with a target, forwarded without
  one;
- install writing the profile, pools and helper overrides into the pinned
  config;
- the Xcode example, installed from `docs/examples/mcp-broker/xcode.json` with
  the fake helpers and the fake upstream reading the same file (see
  [the Xcode example](mcp-broker-xcode.md#tests)).

`cargo test --test workspaces` checks that release closes a recorded window
before its clean check, that a failing close keeps the lease and names the
instance, that release proceeds and audits the window (with its instance) when
no broker runs, and that sweep closes too. With two routing instances that both
record a window under the root, the release asks both, and when one fails it is
refused naming that one; an instance recording only another root's window is
never asked. A broker that accepts and never answers does not delay a release
with no recorded window, and refuses one with a recorded window after 35 s,
keeping the lease; two such brokers fail the release within 35 s plus 3, not 70.
Sweep leaves the window of a checkout another task's lease now holds, and
closes the one of a checkout nobody holds.

`cargo test --bin privateer-mcp-broker` checks the profile and routing rules
directly: a profile's canonical round trip and every refusal's cause, window
parsing and filtering by the profile, window-bound tools, path arguments
(names, profile additions, `file:` URLs, arrays, relative paths and symlinks),
discovery by kinds and roots.

`cargo test --lib mcp_broker` checks instance names, the first config shape,
the dispatcher's temporary-file and upstream-group checks, and a connect to a
listener with a full backlog failing by its deadline. `cargo test --test
releases` checks that the release layout ships the broker and that install
takes it from a release.

## Not verified (Mac only)

None of these can be checked on Linux. What is specific to Xcode is listed in
[the Xcode example](mcp-broker-xcode.md#not-verified-mac-only).

- whether a detached broker survives launchd restarting the dispatcher;
- codesigning of the copied binary, and whether Gatekeeper or an upstream treat
  the copy differently from the release's original;
- `LOCAL_PEERPID` and `proc_pidinfo`, beyond what macOS CI covers;
- the cwd from which Claude and Pi spawn the stdio proxy (expected: the job's
  workspace).

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
  root, each with a 30 s deadline. A root with more than one recorded window in
  one instance can therefore outlast the release's 35 s wait; the release is
  then refused and the lease kept.
- A window whose open helper exits before the upstream shows it can appear
  after a close that found nothing to close. It is then left open, and no
  longer recorded.
- Sandboxes keep crews out of the run directory, not the user's own unsandboxed
  processes. Any process running as this user outside a sandbox can still stop
  a broker and listen on its socket in its place. The peer-uid check and the
  0700 directory keep out other users.
- A stuck upstream that neither answers nor exits holds its in-flight calls
  until it does. Admission (step 4) bounds this.
- The owner's dispatcher tick can block for up to 16 s per instance in
  `stop_running` when it stops one whose config is gone (10 s for the lock,
  then up to 3 s each for SIGKILL and the upstream group).
- There is no uninstall command: removing an instance's config stops it, and
  its pin stays until removed by hand.
- An instance reinstalled without `routing` keeps the window record of its
  routed past. It has no close helper, so it answers a release that finds one
  of those windows with an error; stop the instance (the windows are then
  audited) or remove `NAME.windows.json`.
- Clients get the cached InitializeResult, including the `protocolVersion` the
  broker negotiated, whatever version they asked for.
- `resources/subscribe` and `resources/unsubscribe` go upstream as they are:
  subscriptions are shared, so one client's unsubscribe ends the subscription
  for every client.
