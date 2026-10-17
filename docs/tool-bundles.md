# Explicit Pi and Claude resource bundles (first stage)

This is an **opt-in, limited Pi 1.2.0 and Claude Code resource-loading path**
(see [Bounded Claude launches](#bounded-claude-launches)), not a sandbox,
provider router, permission grant, or replacement harness. The runtime does not
install packages, edit native configuration, replace `HOME`/the agent directory,
search credential stores, or infer which personal extensions are required.

The first stage supports named/versioned **local skills and their supporting
scripts/files**. Optional MCP servers and optional executable tool extensions
are deferred: definitions containing those fields fail validation, rather than
silently dropping them. The separate [stage-two offline MCP factory qualification](pi-mcp-stage2-qualification.md)
adds test fixtures/design evidence only; it does not enable MCP in this registry. A bundle
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
  "pi_version": "1.2.0",
  "claude_version": "<exact Claude Code version, optional>",
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
Names, roles, unknown/duplicate refs, conflicting selected skill names, hashes,
unsupported resource fields and missing/unsafe paths fail before launch. A ref's
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

**Flags alone are insufficient in Pi 1.2.0.** Its exact-version source resolves
configured packages before resource filtering, and discovers `SYSTEM.md` and
`APPEND_SYSTEM.md` independently of `--no-context-files`. Therefore the loader:

1. Checks settings only in the original agent directory and cwd's `.pi` directory.
   Nonempty/malformed package settings, malformed/unreadable relevant settings,
   unsafe settings paths and any ambient `SYSTEM.md`/`APPEND_SYSTEM.md` refuse
   before native probing/startup. It never reads auth/models/MCP credential files.
2. Rejects activation-script wrappers and requires the actual native `pi` on PATH
   to report exactly `1.2.0`. Other harnesses/versions refuse, not fall back.
3. Rechecks selected pins and rejects session/resource overrides before launching.

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
with no skills. The registry must pin `claude_version`, the exact version
`claude --version` reports; registries without it still serve Pi. Pi extension
integrations cannot load into Claude, so a Claude plan needs `extensions:[]`.

`plugins/claude_bundles.py` adds Claude's supported flags:

```text
--setting-sources project,local --strict-mcp-config
--settings <carried JSON> --plugin-dir <selected skill root>...
```

Dropping the user setting source drops personal hooks, skills and plugins;
`--strict-mcp-config` without `--mcp-config` drops personal and claude.ai MCP
connectors. The working folder's `.claude` settings (Privateer's own hooks and
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
the runtime's exact `--resume`; any change starts fresh. Crew never resume a
bounded session. Live and hosted starts are always fresh, since `--continue`
cannot name the session a digest was recorded for.

`--bare` and `--safe-mode` are not used: `--bare` accepts only an API key, which
would change the auth route, and `--safe-mode` drops `--plugin-dir` skills.
This is exposure hygiene, not credential isolation; whether a personal
`~/.claude/CLAUDE.md` or auto-memory follows the setting sources is not yet
qualified.

## Test evidence and reproduction

Always capacity-admit heavy suites through `job.start` in an active charter.

```sh
cargo test -j1 --test tool_bundles --test plugins
python3 tests/pi_bundles.py
python3 tests/pi_bundles_native.py /absolute/installed/native/pi
python3 tests/claude_bundles.py
python3 tests/claude_bundles_native.py /absolute/installed/native/claude
```

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

The opt-in native fixture uses **installed Pi 1.2.0**, a disposable auth-less
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
