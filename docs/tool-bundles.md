# Explicit Pi and Claude resource bundles (first stage)

This is an **opt-in, limited Pi and Claude Code resource-loading path**
(see [Bounded Claude launches](#bounded-claude-launches)), not a sandbox,
provider router, permission grant, or replacement harness. The runtime does not
install packages, edit native configuration, replace `HOME`/the agent directory,
search credential stores, or infer which personal extensions are required.

The first stage supports named/versioned **local skills and their supporting
scripts/files**. Remote streamable-HTTP MCP servers and pinned local stdio MCP
servers (crew jobs only) load for Pi and Claude runs that select them, see
[Selected MCP servers](#selected-mcp-servers-pi-and-claude). Optional executable
tool extensions are deferred: definitions containing those fields fail
validation, rather than silently dropping them. The separate
[stage-two offline MCP factory qualification](pi-mcp-stage2-qualification.md)
holds the Pi factory fixtures and limits. A bundle
may name [credentials](#credentials) its scripts need; a launch gets the path of
each selected one's captain-stored file, never the value. Never put credential
values in bundles, prompts, argv, registry definitions or scripts.

## Compatibility and custody

- **Omit `tool_bundles`** to retain the existing native launch behavior. Ambient
  resources may load. This mode is **not bounded or isolated**.
- **`"tool_bundles": []`** explicitly requests bounded resource loading. It still
  requires an operator-approved registry and integration review; omission does
  not mean approval that no integrations are needed.
- Set the field on a `ship.put` **first_mate** profile, or explicitly on each
  **`agent.start`** request. It is **not** a harness `options` field.
- Crew still inherit the captain's harness/provider/model options as before,
  but never inherit the first mate's bundle selection. Nested crew and sibling
  tasks must select explicitly too. Registries are per operational home; creating
  another ship does not copy this registry or its selections.
- Selection cannot grant charter actions or change fleet tool permission policy.
  Only the operator can call `bundle.configure`; agents can select already
  approved refs for their role.

For example (operator configuration, not an instruction to enable a live ship):

```json
{"command":"ship.put","args":{"name":"example","first_mate":{
  "manifest":"bundled:pi","duty_task":"example-duty",
  "options":{},"tool_bundles":["local-lookup@1"]
}}}
```

An explicit crew request is independent:

```json
{"command":"agent.start","args":{"task_id":"lookup-task","tool_bundles":["local-lookup@1"]}}
```

## Operator registry

`bundle.configure {"registry": ...}` validates before atomically recording the
registry and immutable ref history in the home's SQLite metadata. Its audit
contains only refs and status, not paths or file contents. This is
an operator-only configuration command, not authority an agent can give itself.
No registry is installed/enabled by shipping this code.

```json
{
  "schema_version": 1,
  "pi_version": "<version the registry was made with>",
  "claude_version": "<version the registry was made with, optional>",
  "integrations": {
    "reviewed": true,
    "extensions": []
  },
  "bundles": [{
    "ref": "local-lookup@1",
    "roles": ["first_mate", "crew"],
    "credentials": [{"name": "BRAVE_API_KEY", "ref": "brave-search"}],
    "skills": [{
      "name": "local-lookup",
      "root": "/absolute/operator-reviewed/local-lookup",
      "sha256": "<tree digest>"
    }]
  }]
}
```

`integrations.reviewed:true` is an explicit **operator assertion** that all
required auth/provider/permission integrations were identified and reviewed for
this bounded loading mode. `extensions:[]` asserts that none are required; it
must not be guessed from ambient discovery. Where required, list each approved
extension as `{"path":"/absolute/integration.ts","sha256":"<file sha256>"}`.
These are mandatory integrations, not a way to sneak optional MCP/tool bundles
into the first stage. Review them for ambient-resource discovery/connections and
provider/permission preservation. Only explicitly approved refs load. The runtime
does not copy, discover, substitute or silently omit them. Pi itself refuses
extension factory/load errors before starting an agent turn; an actual-native
fixture verifies this behavior. Session-handler behavior remains trusted code.
No real installation's integration paths have been approved by this change.

Registry fields are strict. Refs must have a name and version (`name@version`).
`skills` is optional: a bundle that carries only `mcp_servers` omits the key,
and an absent key reads as no skills. Every bundle needs at least one skill or
one server; an empty bundle is refused. Names, roles, unknown/duplicate refs,
conflicting selected skill names, hashes, unsupported resource fields and
missing/unsafe paths fail before launch.

A refused registry names the failing bundle and the field or rule, for example
`bundle docs@1 refused: mcp_servers.url`. Messages never include a value from the
registry: no URL query string, credential value, header, path or file content. A ref's
path/content/roles cannot change, even after removal/re-addition: publish a new
version instead. Removal revokes future selection. An already recorded agent-job
replay returns that same job rather than planning another launch.

Resource paths must be absolute, regular/user-owned, without symlink components
or group/world-writable components (root-owned sticky temporary-directory
ancestors are allowed for fixtures). Trees are limited to 128 entries/2 MiB; registry/selection/plan sizes
are bounded. Pins are rechecked before native execution; drift anywhere in the
approved registry refuses new selection, including explicit empty selection.
There is no per-run directory to leak or clean up.

A skill root contains `SKILL.md` and supporting scripts/files. For deterministic
Pi-compatible validation this first stage accepts exactly `name` and
`description` frontmatter keys: an unquoted name, and a single-line description
in the documented narrow plain-text subset or a JSON-quoted string. Other YAML
forms/frontmatter are rejected, not interpreted by a partial YAML implementation.
Descriptions are at most 1024 characters. Supporting files are not eagerly added
to context; Pi advertises the skill descriptor and loads its instructions on use.

Compute a tree pin from this checkout:

```sh
python3 plugins/bundles.py --digest /absolute/operator-reviewed/local-lookup
```

The tree hash is SHA-256 of all files in relative POSIX-path order, each encoded
as `relative-path + NUL + lowercase-sha256(file) + LF`. It covers supporting
files, not external dependencies or arbitrary imports made by trusted code.

## Credentials

A bundle's optional `credentials` list holds names and refs only. `name` is
the environment-safe variable stem (`[A-Z][A-Z0-9_]*`, at most 64), `ref` the
stored credential (lowercase words joined by hyphens, at most 40). Any other
field, such as an inline `value`, or a ref shaped unlike a short identifier
refuses the registry. Two selected bundles may share a credential but cannot
give one name two refs.

The captain stores the secret, from the CLI only:

```sh
privateer credential.set --json '{"ref":"brave-search"}'   # at a terminal: prompts, input hidden
printf '%s' "$BRAVE_API_KEY" | privateer credential.set --json '{"ref":"brave-search"}'
privateer credential.list --json '{}'
privateer credential.remove --json '{"ref":"brave-search"}'
```

`credential.set` reads the value from stdin (one trailing newline dropped, at
most 16 KiB), never from argv or JSON arguments. At a terminal it prompts with
echo off, so the secret stays out of scrollback and no key file is needed. It
refuses any argument but
`ref`. It writes `<ref>` as a 0600 file, through a same-directory rename, in a
0700 directory beside the fleet registry: `fleet.credentials` next to
`PRIVATEER_FLEET`, else under `$XDG_STATE_HOME/privateer/`, else
`~/.local/state/privateer/`. That is outside every home, repository and SQLite
file. `credential.list` shows refs only. All three are operator commands:
agents are refused, and RPC and `peer.call` refuse them since there is no
stdin to read a secret from.

At launch, `native_launch.py` checks each of the plan's refs before any native
probe: the directory is 0700 and the file is a regular 0600 file this user owns,
with a single link, and every path component passes the bundle-path checks (no
symlinks, nothing group or world writable). A missing or wrongly moded ref
refuses the launch. It then sets only
`PRIVATEER_CREDENTIAL_<NAME>_FILE=<path>` for the run, Claude and Pi alike.
The plan, its digest, argv and prompts carry names and refs, never the value
or the file's content; a selected skill's script reads the file itself.

Crew never inherit a parent's credentials: every job starts without any
inherited `PRIVATEER_CREDENTIAL_*` variable, and each native launch scrubs them
again before adding its own. A crew that needs one selects the bundle itself,
as with bundles. The directory is resolved from the launch's environment the
way the runtime resolves the fleet registry; a process serving another fleet
through `--fleet` would look in the environment's, and refuse a ref missing
there.

**Honest limit.** The agent runs as the same OS user and can read the file:
this keeps the key out of prompts, argv, logs, the repository, the registry
and SQLite, but not out of the model's reach. A later stage may add a local
proxy that adds the header itself, so the agent's environment never names the
key. It is not built.

## Per-run native loader and fail-closed limits

Both `plugin.command`/headless jobs and `privateer start`/hosted first-mate
launches pass the same selected-only pinned plan to `native_launch.py`. The
loader leaves the original native auth, models, settings, provider/model argv,
Privateer brief/actor/control and permission policy intact. Attended runs retain
the explicit bundled Privateer mail adapter. Hosted crew retain #96's explicitly
injected bundled `attended/crew.ts` lifecycle adapter: it is recognized only with
the runtime hosted-crew marker and exact injected path. A stale inherited marker
alone never adds it or a first-mate selection. No bundle plan or selection is
exported as an inherited environment variable.

The loader uses Pi's supported flags:

```text
--no-extensions --no-mcp --no-skills --no-prompt-templates
--no-context-files --no-themes
```

It then supplies only approved mandatory `-e` paths, Privateer's attended mail
adapter where applicable, and selected `--skill .../SKILL.md` paths. Explicit
unapproved resource overrides and bounded resume/continue/fork are refused.
Bounded first-mate wakes use fresh sessions; hosted starts do not automatically
continue the old session. A repeated `init` preserves explicit bounded mode
rather than silently switching the ship back to legacy ambient loading.

Bounded Pi loading depends on the installed Pi's flags, not on its version
string. Pi's package resolution and `SYSTEM.md`/`APPEND_SYSTEM.md` discovery are
not all covered by flags, so the loader also checks settings itself:

1. Checks settings only in the original agent directory and cwd's `.pi` directory.
   Nonempty/malformed package settings, malformed/unreadable relevant settings,
   unsafe settings paths and any ambient `SYSTEM.md`/`APPEND_SYSTEM.md` refuse
   before native probing/startup. It never reads auth/models/MCP credential files.
2. Rejects activation-script wrappers, then asks the installed `pi` for `--version`
   and `--help` (never its config). It must declare every flag the bounded launch
   passes: `--no-extensions`, `--no-mcp`, `--no-skills`, `--no-prompt-templates`,
   `--no-context-files`, `--no-themes`, `--skill` and `-e`. Any version declaring
   them is accepted; a missing flag refuses and the error names it. Flags are read
   from each option row only, so a description mentioning a flag does not count.
   The result is cached per executable path and version for the run.
3. Rechecks selected pins and rejects session/resource overrides before launching.

Claude Code launches get the same capability check: the installed `claude` must
declare `--setting-sources`, `--strict-mcp-config`, `--settings`, `--plugin-dir`
and `--print`. Its version is not compared with the registry.

This can refuse a perfectly legitimate personal Pi setup. Use legacy mode until
the captain explicitly approves a compatible bounded profile; do not remove
provider/permission extensions or change a provider route to make it pass.

This is **exposure hygiene**, not access isolation: built-in shell/read tools and
trusted extensions retain the launching user's OS access. Content pins are
launch-time drift checks, not immutable filesystem snapshots or protection from
same-user concurrent mutation. Trusted integration imports/scripts can access
other files/network and must be reviewed; the loader cannot sandbox them or
redact arbitrary output from trusted/native code. No selected-MCP connection,
credential brokering beyond the stored-file path above, or full
real-profile/hosted production qualification is claimed. Bounded resume is
intentionally unsupported rather than pretending old session context can be
erased by changing today's inventory.

## Bounded Claude launches

A `bundled:claude` first mate or crew request takes the same `tool_bundles`
field and registry. Omitting it keeps today's launch exactly; `[]` is bounded
with no skills. A registry with `claude_version` serves Claude; registries without it still
serve Pi. The version is a record, not a pin: the installed Claude Code must
declare the bounded-loading flags (see the capability check below). Pi extension
integrations cannot load into Claude, so a Claude plan needs `extensions:[]`.

`plugins/claude_bundles.py` adds Claude's supported flags:

```text
--setting-sources project,local --strict-mcp-config
--settings <carried JSON> --plugin-dir <selected skill root>...
```

Dropping the user setting source drops personal hooks, skills and plugins;
`--strict-mcp-config` drops personal and claude.ai MCP connectors, and only the
run's selected servers load, through Privateer's own `--mcp-config` (see
[Selected MCP servers](#selected-mcp-servers-pi-and-claude)). The working folder's `.claude` settings (Privateer's own hooks and
permissions), its project instructions and its own `.claude/skills` still
load. Each selected skill root is passed straight to `--plugin-dir`, where
Claude loads it as `<name>:<name>`. Claude's built-in skills and plugins load in
every mode; no flag removes them without also removing selected skills.
Attended runs keep Privateer's `attended-claude` mail plugin.

Only `model`, `modelSettings`, `effort`, `autoMode` and
`skipDangerousModePermissionPrompt` carry over from the user settings file
(`$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`), through
`--settings`. They are part of the plan. `$schema`, `theme`, `statusLine`,
`includeCoAuthoredBy`, `cleanupPeriodDays` and `disableLoginPrompt` are
cosmetic or local housekeeping and are dropped, as are `enabledPlugins` and
`hooks`, the personal exposure a bounded launch exists to drop (the project's
own hooks still load). The loader refuses before launch when:

- the user settings hold any other key: `env`, `apiKeyHelper` and other
  provider or auth keys would change the route, and `permissions`, `sandbox`
  or `disableBypassPermissionsMode` would leave a bounded launch freer than an
  unbounded one; or they are malformed, a symlink or unsafe, or no longer
  match the planned carried settings;
- an `--add-dir` folder (ship home, resource ledger) is relative, or holds
  `.claude/skills`, `.claude/commands`, `.claude/agents` or `.mcp.json`, which
  Claude would load although they are not the project;
- a selected skill root holds plugin parts (`hooks`, `.mcp.json`, `.lsp.json`,
  `agents`, `commands`, `skills`, `bin`, `settings.json`, `monitors`,
  `output-styles`, `.claude-plugin`): stage one is skills only;
- the caller passes `--plugin-dir` (other than the attended adapter),
  `--plugin-url`, `--settings`, `--setting-sources`, `--mcp-config`, `--bare`,
  `--safe-mode`, `--agents`, or a continue/fork/session flag;
- `claude` on PATH is an activation-script wrapper, or reports another version.

**Resume.** The plan digest (selected refs, pins, Claude version and carried
settings) is recorded with the wake or errand and the session it starts. A
bounded first mate resumes only a session recorded under the same digest, as
the runtime's exact `--resume`; any change starts fresh. A bounded crew's
digest is recorded in its job's launch record. When it parks, its resume
continues its own session only: the runtime records that session on the park
before launching, and the launch must be a crew of the same task, under the
digest the crew launched with. If the plan changed while the crew was parked,
the resume is refused with a message naming the task, and the crew's outcome
says its session was not resumed because its bundle plan changed. A crew never
resumes a first mate's or an errand's session, nor they a crew's. Live and
hosted starts are always fresh, since `--continue` cannot name the session a
digest was recorded for.

`--bare` and `--safe-mode` are not used: `--bare` accepts only an API key, which
would change the auth route, and `--safe-mode` drops `--plugin-dir` skills.
This is exposure hygiene, not credential isolation; whether a personal
`~/.claude/CLAUDE.md` or auto-memory follows the setting sources is not yet
qualified.

## Selected MCP servers (Pi and Claude)

A bundle may hold `mcp_servers`, at most four. Each is either a remote
streamable-HTTP server (`url`) or a local stdio server (`command`), never both.

An HTTP server has exactly `name` (a lowercase slug, at most 32 characters),
`url`, `credential` (a name the same bundle declares under `credentials`),
`timeout` (an integer from 1 to 300) and an optional `description` (printable, at
most 200 characters). The URL is `https`, or `http` only to `127.0.0.1`, `[::1]`
or `localhost`, with no userinfo, fragment, backslash or whitespace. The host is
letters, digits, dots and hyphens or a bracketed IPv6 literal, and any port must
parse.

A stdio server has `name`, `command`, `sha256` and `timeout`, and optionally
`credential` and `description`:

```json
{"name": "xcode", "command": ["/usr/bin/xcrun", "mcpbridge"],
 "sha256": "<sha256 of /usr/bin/xcrun>", "timeout": 30}
```

- `command` is an argv array of 1 to 32 strings. Each is at most 4096 characters
  and holds no `$`, leading `!`, leading `~` or control character: Pi expands `$`
  and a leading `~/`.
- Element 0 is the executable: an absolute, normalized path. Its file must be a
  regular file its owner may execute, not a `#!` script, and its contents must
  match `sha256`.
  Every component from `/` down must be owned by root or this user, must not be
  a symlink and must not be group- or world-writable. Root-owned
  `/usr/bin/xcrun` qualifies; a temporary directory never does, because `/tmp`
  is world-writable.
- Shells and `env` refuse by basename (`sh`, `bash`, `zsh`, `dash`, `fish`,
  `ksh`, `mksh`, `csh`, `tcsh`, `ash`, `busybox`, `env`), since either runs
  whatever its arguments say.
- The pin covers the executable only, not what it loads. An interpreter
  (`python3 server.py`) or a trampoline (`xcrun`) runs files the pin does not
  name. A `#!` script refuses (`mcp_servers.command`) on every platform: it
  runs its interpreter by path, which its pin does not cover, and it cannot be
  exec'd through the descriptor the launcher hashed. Pin the interpreter and
  pass the script as an argument instead.
- There are no `env`, `cwd`, `shell`, `args`, `type` or `headers` keys.
- `credential` is optional. When given, the server receives that credential's
  file path, never its value.

**Stdio servers are crew-only.** A crew job's teardown ends a stdio server and
everything it started (see below). A hosted first mate's pane is not a job:
sleep, approval refresh and upgrade stop it by signalling the harness pid
alone, so a server that ignores SIGTERM, or one orphaned when the harness is
killed, would outlive the pane, and every relaunch could leave another. A
bundle's `roles` may still name `first_mate`, but any plan for a role other
than `crew` that selects a stdio server refuses with `stdio MCP servers run only
in crew jobs, whose teardown ends them; first_mate launches are not jobs`. That
covers a first mate's profile (`ship.put`), its hosted or `privateer start`
launch, and its wakes, which plan as `first_mate`. The same refusal is
repeated at each job's adapter preflight (by its role) and by the attended
launcher, and Pi's gate gets the launcher's interpreter only for a job's
launch, so it refuses any stdio record otherwise. HTTP servers stay allowed for
every role that may carry bundles.

The pin is checked when the registry is configured, when a run is planned, at
each launch's preflight, and again by the launcher just before exec (Pi's gate
also re-hashes it as it loads). A drifted executable refuses
(`mcp_servers.sha256`) before any probe or model turn.

A plan holds at most eight servers of either kind, unique after lower-casing and
`-` to `_`. A bundle needs a skill or a server. Any other key (`headers`, `env`,
`oauth`, `exposure`...) refuses, as does any string holding `$`, a leading `!` or
a control character.

Only a run that selects the bundle loads its servers. For Pi, the launcher sets
`PRIVATEER_MCP_SERVERS` to the selected records (names, URLs or pinned commands,
credential names, timeouts, descriptions; no credential paths or values) and adds Privateer's
`plugins/pi-mcp/selected.ts` gate. Each Bearer token is read from its
captain-stored credential file inside the Pi process, and is sent only as an
`Authorization` header to that server, with `exposure: direct`. The gate passes
when every selected server lists tools and no other `mcp__` tool exists. It
refuses, before any model turn, when another extension registered a server, a
token file is missing or malformed, a server is refused (401) or lists no tools
within `min(30, timeout)` seconds. A refusal exits 78 with one line naming only
the server. Plans without servers are unchanged, and a run gets no MCP tool unless
it selects one. A stdio server gets the same gate; its entry is the launcher
below, and its readiness is the same tools check.

A bounded Claude run gets the same records in `PRIVATEER_MCP_SERVERS` and one
`--mcp-config=<file>` beside `--strict-mcp-config`, so no other server loads. The file
is 0600 in the 0700 `fleet.mcp-configs` directory beside the credential store, named by
its content; a URL may carry a secret, and argv is readable by other local users. Each
selected server is `{"type":"http","url":...,"headersHelper":...}`, where the helper
is `plugins/claude_mcp_headers.py CREDENTIAL`, run as `python3 -I`. Claude runs it
when it connects. It reads the token from the
`PRIVATEER_CREDENTIAL_<NAME>_FILE` the launcher set, with the same token rules as
Pi's reader, and prints only the `Authorization` header. On a missing or malformed
token it exits 78, naming only the credential. The config holds no token. Because
Claude connects without the header when its helper fails, the launcher first runs
each helper once with its output discarded and refuses (`mcp_servers.credential`)
unless it succeeds; the token stays in the helper's process. A headless run
(`claude_turn.py`) also checks Claude's init event before any tool runs: every
selected server must report `connected` and list tools, and any other server fails
the run (`mcp-unavailable:<name>`, `mcp-foreign:<name>`). Hosted (attended) sessions
get the pre-launch token check but no connection check; Claude's `/mcp` shows each
server's state there. The record's `timeout` and `description` are not mapped into
Claude's config.

### How a stdio server starts, and what it receives

Neither harness runs a stdio server's command directly. Both run Privateer's
launcher, `plugins/mcp_stdio.py`:

- The launcher is the runtime's own copy, beside the bundle loader; a bundle
  cannot name another.
- It runs under the runtime's own interpreter by absolute path, with `-I`. That
  path is `/proc/self/exe` on Linux, or `sys.executable` resolved elsewhere, and
  must be named `python*`. It is never a PATH lookup, which could find a mise
  shim.
- Its one argument is a runtime-written spec: the pinned command, its `sha256`,
  and the server's credential name and file path if it has one. It never reads
  the record from its environment.
- It re-pins the executable, opening it with `O_RDONLY|O_NOFOLLOW|O_CLOEXEC`
  and hashing that descriptor.
- It closes every inherited descriptor from 3 up to the soft `RLIMIT_NOFILE`,
  so the server holds only its stdio.
- It then `execve`s in place: the same pid and process group, with no fork or
  `setsid`. Where the platform execs a descriptor (`os.execve` in
  `os.supports_fd`, which includes Linux), it execs the very descriptor it
  hashed, so the bytes hashed are the bytes run; the descriptor is
  close-on-exec, so the server never holds it. Elsewhere (macOS) it execs the
  path, rechecked just before.
- On any drift or unexpected spec it exits 78 with one fixed line and starts
  nothing.

For Claude, the `--mcp-config` entry is
`{"type":"stdio","command":"<interpreter>","args":["-I","<plugins>/mcp_stdio.py","<spec>"],"env":{}}`.
No header helper runs for a stdio server. A credential path Claude would expand
(holding `$`) refuses (`mcp_servers.credential`). For Pi, the launcher sets
`PRIVATEER_MCP_PYTHON` to the same interpreter, and the gate builds the
equivalent entry (`command`, `args`, an empty `env`, the record's `timeout`,
`exposure: direct`).

Both harnesses give a stdio server their whole environment, measured with
Claude Code 2.1.300 and Pi 1.3.0. That includes `ANTHROPIC_API_KEY` and
`ANTHROPIC_BASE_URL` when they are set, Claude's `CLAUDE_CODE_MESSAGING_SOCKET`,
`CLAUDE_CODE_MESSAGING_TOKEN` and `CLAUDE_CODE_SESSION_ID`, `PI_CODING_AGENT_DIR`,
`PRIVATEER_MCP_SERVERS`, `PRIVATEER_FLEET`, every selected credential's
`_FILE` path, and anything else in the harness environment. **None of that
reaches the server.** The launcher replaces it with exactly:

- `PATH=/usr/bin:/bin`;
- `HOME`, `LANG`, `TMPDIR` and any `LC_*`, when the harness has them;
- `PRIVATEER_CREDENTIAL_<NAME>_FILE` for the server's own credential, if it
  names one.

Nothing else is passed, including `DEVELOPER_DIR` (`xcrun` falls back to
`xcode-select`) and `PYTHONPATH`. The native probes assert this exact set under
both harnesses, and `tests/pi_bundles.py` asserts it for the launcher alone. The
server's working directory is the harness's.

A server's processes end with the crew's job, which is why only crew may run
one. The harness spawns the launcher, the launcher execs the server, and the
server stays in the process group the harness gave it, inside the job's session. A SIGTERM-ignoring grandchild
outlives the harness itself:

- Claude exits normally and leaves its group running.
- Pi 1.3.0's `--print` exit leaves it too.

Until the job ends, that grandchild keeps running. A crew job is started with
`cleanup_descendants`. When its leader exits, the job signals its process group,
and every recorded descendant that left the group, with SIGTERM and then SIGKILL
after 2 s. `tests/tool_bundles.rs` proves this for a grandchild in the server's
group and for one in a session of its own. The job records descendants that left
its group by sampling every 500 ms. A process that leaves the group, and whose
parent exits, between two samples is not found.

Limits: the token sits in the Pi process's memory; a same-user agent can still
read the credential file; URLs are not content-pinned. A token that goes bad
after launch (the file changed mid-run) makes Claude reconnect without the
header; a hosted session has no connection check. A stdio pin covers the
executable, not what it loads. On Linux the launcher runs the bytes it hashed.
On macOS, which cannot exec a descriptor, the path can still be swapped between
the launcher's re-pin and its `execve`, but only by root or this user, who own
every component.

- The agent can read its own credential file; this is an accepted limit.
- Redirects are followed, but `Authorization` is dropped on a cross-origin redirect.
- A URL, including its query string, appears in argv and in the environment. No
  secret belongs in a URL; use a credential reference for any token.
- After startup, a server registered by any other extension refuses the run
  (`mcp_servers_change`), and a tool call outside the selected `mcp__` namespaces
  is blocked (`tool_call`).

## Test evidence and reproduction

Always capacity-admit heavy suites through `job.start` in an active charter.

```sh
cargo test -j1 --test tool_bundles --test plugins
python3 tests/pi_bundles.py
node tests/pi-mcp-selected.mjs
python3 tests/pi_offline_netns.py -- python3 tests/pi_bundles_native.py /absolute/installed/native/pi
python3 tests/claude_bundles.py
python3 tests/claude_bundles_native.py /absolute/installed/native/claude
```

The stdio cases: the deterministic suites cover every refusal (shells and `env`,
relative or unnormalized paths, symlinks, writable ancestors, non-executable
files, pin drift at selection, preflight and the launcher, `$`/`!`/`~` and
control characters, `env`/`cwd`/`shell` keys, `url` beside `command`, `#!`
scripts), the crew-only rule (a first mate's plan, profile, job preflight and
attended launch refuse a stdio server, while its HTTP servers and a crew's stdio
server still load), the Claude and Pi entries, and the launcher's environment,
closed descriptors (the fake records its own, and sees only 0, 1 and 2 though
the harness passed more) and in-place exec. Each
native probe's `test_selected_stdio_server_env_ambient_and_lifetime` runs a
pinned stdio fake (`tests/fixtures/pi-mcp/server.py`) through the real launcher
and harness. It asserts:

- the server connects with tools (the Pi gate passes; Claude's init reports it
  `connected` and `claude_turn.py`'s check passes);
- seeded ambient stdio servers never start: user and project Claude config,
  `.mcp.json`, and Pi's agent and project `mcp.json`;
- the exact environment above reaches the server.

The probe also reports which of the fake's processes outlive the harness,
before killing them; it runs outside a job.

The Claude suites run in a temporary HOME, `CLAUDE_CONFIG_DIR` and XDG. The
deterministic one uses a fake `claude` for argv plans and each refusal; the
Rust suite checks planning and digest-gated resume, and drives real `serve`
wakes through a compiled fake `claude` (fresh, then resumed, then fresh after
a carried setting changes). The opt-in native probe
points `ANTHROPIC_BASE_URL` at a dead loopback port with a fixture API key,
reads `system/init` and kills the process: no model call. It seeds a personal
skill, hook and MCP server, and checks that only the selected skill, the
project's own skill and Claude's built-ins load, no MCP server or hook starts,
and the carried model applies. Without an explicit binary it skips.

The deterministic suite checks registry validation, path safety/pins, malformed
settings, source-qualified preflight refusal before native probes, redacted
protocol errors, operator boundaries and legacy/empty separation. Runtime tests
use fake native subprocesses to exercise ordinary/nested crew and job replay;
mocked-host tests retain hosted custody/lifecycle injection and refuse an
unsupported binary without silently running it. Those are not actual Pi
inventory evidence.

The opt-in native fixture uses the exact native Pi binary it is given (its reported version is recorded in the output), a disposable auth-less
`PI_CODING_AGENT_DIR` and project, a clean credential-free environment allowlist,
unchanged `HOME`, offline/no-session controls, and a recognized inspection slash
command. Its inspector exits if an agent turn starts: no inference or real
service is needed. It checks actual selected/empty inventories, the attended and
hosted-crew adapter injection paths, concurrent launches, lazy skill instructions, required-extension
load failure, and zero starts/connections from seeded ambient global/project
stdio/HTTP MCP servers and extensions. The selected supporting API script really
calls a loopback fake HTTP server; response projection, redirects, server errors,
timeout and oversized results are bounded/redacted. Temporary fixture cleanup is
per-run and does not edit the real agent directory. Without an explicit binary,
the native suite skips and must **not** be cited as inventory evidence.

The deterministic suites and `tests/tool_bundles.rs` also check credentials:
`credential.*` are operator-only and stdin-only with 0700/0600 modes, list
shows refs only, a plan carries only selected bundles' names and refs, a launch
gets only their `_FILE` paths, a missing, re-moded, moved or symlinked file
refuses before any probe, inherited paths are scrubbed and no value appears in
argv, the plan or the environment.

A full attended Herdr terminal/production profile, selected MCP/deferred schemas,
a real credential-bearing client, malicious same-user code and paid-model
completion are outside this first-stage evidence. No production deployment or merge is
implied by passing these fixtures.
