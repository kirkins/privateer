# Explicit Pi resource bundles (first stage)

This is an **opt-in, limited Pi 1.2.0 resource-loading path**, not a sandbox,
provider router, permission grant, or replacement harness. The runtime does not
install packages, edit native configuration, replace `HOME`/the agent directory,
search credential stores, or infer which personal extensions are required.

The first stage supports named/versioned **local skills and their supporting
scripts/files**. Optional MCP servers and optional executable tool extensions
are deferred: definitions containing those fields fail validation, rather than
silently dropping them. There is no credential broker or secret injection in
this stage. Keep API adapters needing credentials disabled until their
credential-reference/selected-client boundary is separately qualified. Never
put credential values in bundles, prompts, argv, registry definitions or scripts.

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
  "integrations": {
    "reviewed": true,
    "extensions": []
  },
  "bundles": [{
    "ref": "local-lookup@1",
    "roles": ["first_mate", "crew"],
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
There is no per-run directory or credential material to leak or clean up.

A skill root contains `SKILL.md` and supporting scripts/files. For deterministic
Pi-compatible validation this first stage accepts exactly `name` and
`description` frontmatter keys: an unquoted name, and a single-line description
in the documented narrow plain-text subset or a JSON-quoted string. Other YAML
forms/frontmatter are rejected, not interpreted by a partial YAML implementation.
Descriptions are at most 1024 characters. Supporting files are not eagerly added
to context; Pi advertises the skill descriptor and loads its instructions on use.

Compute a tree pin from this checkout:

```sh
python3 plugins/pi_bundles.py --digest /absolute/operator-reviewed/local-lookup
```

The tree hash is SHA-256 of all files in relative POSIX-path order, each encoded
as `relative-path + NUL + lowercase-sha256(file) + LF`. It covers supporting
files, not external dependencies or arbitrary imports made by trusted code.

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
credential resolution, or full real-profile/hosted production qualification is
claimed. Bounded resume is intentionally unsupported rather than pretending old
session context can be erased by changing today's inventory.

## Test evidence and reproduction

Always capacity-admit heavy suites through `job.start` in an active charter.

```sh
cargo test -j1 --test tool_bundles --test plugins
python3 tests/pi_bundles.py
python3 tests/pi_bundles_native.py /absolute/installed/native/pi
```

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

A full attended Herdr terminal/production profile, selected MCP/deferred schemas,
credential-bearing clients, malicious same-user code and paid-model completion
are outside this first-stage evidence. No production deployment or merge is
implied by passing these fixtures.
