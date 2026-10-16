# Selected-only MCP factory: stage-two qualification

This change is **qualification only**. The production stage-one bundle registry,
plan schema and launcher still reject optional MCP definitions. No live profile,
registry, provider route, permission policy or installation is changed.

## API and execution boundary

Target: installed **Pi 1.2.0**, exact native executable supplied explicitly (not a
mise activation wrapper). The fixture imports these real host exports from
`@earendil-works/pi-coding-agent` inside that executable:

- `createMcpExtension(options = {}) -> ExtensionFactory`
- `createCodemodeExtension({models: false})`, `createToolSearchExtension()`
- `DefaultResourceLoader`, `createAgentSession`, `ModelRuntime`,
  `SettingsManager`, `SessionManager`

The relevant option is **synchronous**:

```typescript
createMcpExtension({
  loadConfig: (ctx) => ({
    servers: [{name: "selected", config: selectedConfig,
               source: "<offline-selected>", scope: "extension"}],
    errors: [],
    autoEnableCodemode: true,
  }),
  logPath: disposableLogPath,
  updateConfig: () => { throw new Error("writes forbidden"); },
  openUrl: () => { throw new Error("browser forbidden"); },
});
```

This is a loaded-entry result, **not** `{mcpServers: ...}` and not a Promise.
`config` has the native stdio/streamable-HTTP server shape. Supplying `loadConfig`
replaces the default global/project `mcp.json` reader; it is not a filter applied
after those files have already connected. The factory registers lifecycle
handlers without starting servers. `await session.bindExtensions(...)` emits
`session_start` and starts connections in the background.

The installed `docs/{mcp,extensions,sdk,codemode}.md` and related configuration,
settings, security, packages, CLI, environment and resource documentation were
read, alongside `examples/sdk/{06-extensions,12-full-control,14-codemode-mcp}.ts`
and extension examples. Session-format/message-types documentation was also read
for the synthetic in-memory call fixtures. The upstream source-qualified reference supplied for
this task is Pi commit `06a466935536f8c9bcacb2d5efc0fc021be1bdff`, MCP blob
`8a97d7fb2fa32180ca6d034a5c49bbb77452b5e8`. That reference alone is not runtime
proof; this fixture uses actual installed exports and native runtime code.

## Reproduction and containment

Capacity-admit through the active charter's `job.start`; run serially:

```sh
python3 tests/pi_offline_netns_test.py
python3 tests/pi_mcp_native.py /absolute/installed/native/pi
# Keep the stage-one native regression suite under the same boundary:
python3 tests/pi_offline_netns.py -- python3 tests/pi_bundles_native.py /absolute/installed/native/pi
```

The MCP entry point reexecutes the **entire suite, fake listeners and native
children**, including the version probe, through `unshare --user
--map-root-user --net`. The standard-library helper `tests/pi_offline_netns.py`
checks that both namespaces differ from the parent, refuses any interface other
than loopback and any route other than exact kernel local-loopback routes, then
brings up only that namespace's `lo`. There are no external interfaces, default
routes, veths, proxies or host network/configuration changes. Missing Linux
namespace support, `unshare` or `ip`, unexpected topology, or a failed guard
check **refuses without a host fallback**. No installs, privilege escalation or
host permission/limit changes are needed.

Before native exec, the guard records the namespace identities and topology and
requires immediate kernel `ENETUNREACH` for IPv4 `192.0.2.1` and IPv6
`2001:db8::1`, for both TCP and UDP. These are documentation-only addresses;
no DNS or real service is contacted. The topology is checked **before** those
socket probes. The native suite rechecks the guard before `--version` and every
invocation; namespace inheritance applies to Pi and its stdio payloads without
replacing or patching Pi's connector. This independent kernel boundary does not
rely on Pi's offline switches, extension events or a Python/JS socket shim.

Loopback remains reachable inside the namespace, including the ambient HTTP
traps: every selected and ambient listener first accepts a positive-control TCP
connection. Its completed control is excluded from the subsequent native traffic
count. Thus zero ambient TCP counts cannot be explained by containment blocking
the ambient traps. Only disposable fake listeners are started there; host
loopback listeners are not reachable in this fresh network namespace.

Without an explicit binary the suite skips; a skip is **not evidence**. A binary
other than native Pi reporting exactly `1.2.0` refuses. No packages are installed
or fetched. Python's standard library implements only the **fake servers**; Pi's
own MCP connector, tool registry, codemode QuickJS, search and execution pipeline
remain in use.

Each invocation gets a new authless agent directory and project, a clean
environment allowlist, `--offline`, offline/version/telemetry environment
controls, `--no-session`, and disabled ambient resource discovery. `HOME` remains
unchanged. Auth/models paths point only at synthetic disposable JSON. No machine
credential stores are searched or copied. Only selected fake stdio argv or an
explicit `127.0.0.1` fake URL reaches the callback. Candidate unselected entries
remain outside the selected serialized input. No real Brave or external service
is used, and neither the CLI nor SDK receives a model prompt. `agent_start`
guards terminate the process if a model turn is accidentally attempted.

The recognized CLI inspection command creates a native SDK session with the
official inline factories, reloads the resource loader, and binds extensions.
It seeds synthetic zero-usage assistant tool-call messages through the in-memory
`SessionManager` and `session.refreshContext()` because native nested execution
requires an issuing assistant message. It then invokes Pi's **bound active tool
wrapper** for codemode/search directly, without sampling a model. These are
explicitly fixture messages, not model output; their inert `fixture` provider/API
labels do not register or change a route. MCP calls made by codemode still go
through native nested argument validation, `tool_call`/`tool_result` handlers and
execution machinery. This is an SDK/CLI qualification fixture, not a replacement
harness and not a scripted demonstration of autonomous model behavior.

Cleanup explicitly emits native `session_shutdown` twice, then disposes the SDK
session. `session.dispose()` invalidates contexts but **does not itself emit**
that shutdown event. Stdio start markers record PID, process-group ID and kernel
start identity; assertions wait for the actual payload identity to disappear,
not merely for the Pi wrapper to exit. Failure recovery kills only matching
fixture-owned identities. The local HTTP listeners shut down and their serving
threads join. Accepted TCP counts on the separate ambient listener must be zero,
including idle handshakes; absence of requests alone is insufficient.

## Qualification cases

The native suite establishes, with its emitted JSON inventory and transport
markers:

- Selected stdio and streamable HTTP echo tools are present and callable, return
  concise structured content, and carry native annotation hints.
- MCP schemas are registered after server discovery (not lazily fetched from the
  server). With default `codemode` exposure they are absent from active model
  declarations and the codemode description; `describeTool` exposes a schema on
  demand and `tool_search` activates it. This is **lazy model exposure**, not lazy
  server startup or lazy schema download.
- Native nested hooks block a denied echo, and argument validation blocks a
  malformed echo, without extra fake tool calls.
- Two tool names that normalize alike (`echo-item`, `echo_item`) receive distinct
  native names. Live-runtime public registration rejects `conflict-a`/`conflict_a`.
  Factory-time registration accepts both aliases because mutations are deferred;
  a separate negative fixture observes this and removes both before the MCP
  startup handler, with zero starts. Never rely on this API to prevalidate a
  complete static selection; validate normalized server names independently.
- Explicit empty selection has no selected tools/transports. Seeded ambient
  global/project stdio, HTTP and executable extensions never run/connect.
- Disconnected startup, bounded call timeout, AbortSignal cancellation and
  shutdown during incomplete initialization clean up the real stdio payload.
- A required extension factory failure refuses before inspection; a thrown
  `loadConfig` is reported by native lifecycle error handling.
- Concurrent selected/empty sessions use distinct temporary inputs and identities;
  cleanup is idempotent and no selection is inherited through environment state.

### Historical native evidence (before independent containment)

The original run below predates the namespace guard. It demonstrated native
behavior and ambient-listener non-use, **not external containment**, and must not
be reproduced unguarded. Guarded review-fix evidence is recorded separately below.

Admitted serial job `87665241-7122-4216-90f0-a0cdf2bb11a0`, incarnation
`15d61332-3478-44db-93c7-79e3653395a7`, **succeeded, exit 0**: five unittest
methods / thirteen disposable native invocations, 18.898 seconds. Logs are
`<privateer-home>/jobs/<job>/<incarnation>/{stdout,stderr}.log`. Stdout includes
binary identity, complete native tool inventories, structured echo projection,
pipeline hook names, per-transport method markers and accepted TCP counts.

Native executable SHA-256:
`8f9ba126d73ffa86df7b3c8454ab66d93c394c65dd4f6bafe250f3787515ee57`.

Observed names, all initially `deferred` in the native inventory:

```text
mcp__selected__echo
mcp__selected__echo_item_65a126c3
mcp__selected__echo_item_56a70677
mcp__selected__slow
```

Normal stdio and HTTP each show `initialize`, `notifications/initialized`,
`tools/list`, then exactly one `tools/call`. The structured projection is
`{"ok":true,"identity":"selected","value":"hello"}`; concurrent selected work
returns its distinct `concurrent-A` identity. Denied/invalid inputs add no fake
calls. The HTTP run accepted five selected TCP connections; **every ambient
listener accepted zero**, and every seeded unselected/ambient stdio marker was
absent. All six started stdio payload identities disappeared after native
cleanup. Timeout reports `MCP request timed out after 400ms`; timeout and explicit
abort both record `notifications/cancelled`. Starting shutdown records only
`initialize`, then payload disappearance. No inference or real credentials were
needed.

The two negative observations are deliberate **passing limit checks**, not
claims of native fail-closed behavior: `load-failure` reports a `session_start`
error while bind returns; `factory-server-conflict` accepts both normalized
aliases before the fixture removes them ahead of connection. Live-runtime
`server-conflict` rejects the second alias and removes the first. All three
cases have zero transport starts/connections. Keep production optional MCP
unsupported until the additional gates below are implemented and qualified.

### Guarded review-fix evidence

First frozen review-fix attempt: SHA
`f9c7a2c331c701f77c9bd9720f677a1f156c3b7c`, base `2daef25`, admitted job
`c851d9a5-61af-43d9-ac7b-64904e62e468`, incarnation
`7ac895ce-9ad5-4c75-878f-45f567e33b77`. Syntax, eight boundary unit tests,
twenty stage-one Python tests and all five native MCP methods / thirteen
invocations passed (native MCP: 16.463 seconds). Full logs are
`<privateer-home>/jobs/<job>/<incarnation>/{stdout,stderr}.log`.

The guard logged distinct parent/child user and network identities, only `lo`
with exact local routes, and all four IPv4/IPv6 TCP/UDP probes immediately
refused with `ENETUNREACH`. All thirteen cases logged selected/ambient loopback
positive controls, zero ambient native TCP connections, no ambient/unselected
stdio starts and zero remaining payload identities **before** fallback recovery.
Selected HTTP accepted five native connections and made one tool call. All six
selected stdio payload identities disappeared after native shutdown. Native
binary SHA-256 remains the value recorded above; the native factory, stdio/HTTP
connector and codemode fixture are unchanged.

That first job nevertheless **failed overall, exit 1**: all three stage-one
native tests errored before native execution because their fixture paths passed
through `/tmp`. Host-root's UID is unmapped in this unprivileged user namespace,
so the production ancestor-ownership check correctly refused the sticky directory.
The stage-one native test now creates disposable roots below the owned repository
instead. Production path validation, host permissions, namespace containment and
provider routes remain unchanged; no unguarded fallback was attempted.

The complete frozen rerun at SHA
`030bb84013d9c3d8bb5ad74fec19a9b565bae391`, base `2daef25`, **succeeded,
exit 0**: admitted serial job `f835e4ff-6057-45de-8d60-da1dfb15c832`, incarnation
`4d0db5b7-7921-4c15-9ba6-ee4db0e2ec32`. No edits occurred while queued/running.
Its full stdout/stderr logs use the same job/incarnation path pattern above.

| Check | Result |
| --- | --- |
| Python syntax (guard/tests/native fixtures) | Pass |
| Fail-closed boundary unit tests | 8 passed, 0.006s |
| Stage-one Python regressions | 20 passed, 0.106s |
| Guarded native MCP | 5 methods / 13 invocations passed, 16.945s |
| Guarded stage-one native regressions | 3 passed, 5.789s |
| Base diff whitespace and frozen clean worktree | Pass |

Both guarded native suites logged fresh user/network namespaces distinct from
the host, only loopback/local routes, real HOME preservation, and all four
external `ENETUNREACH` proofs. The MCP rerun preserved the complete selection,
concurrency, error/collision, lazy-exposure, timeout/cancellation and shutdown
observations above: all thirteen ambient TCP counts zero **after successful
per-listener positive controls**, all unselected/ambient stdio markers absent,
and all six selected stdio payload identities gone before fallback cleanup.
The stage-one native inventory retained selected/empty/attended/hosted-crew/
concurrent behavior, zero ambient MCP connections and exactly four fake API calls.
This result qualifies the test boundary, not a production deployment or general
hostile-code sandbox. The following documentation-only evidence update does not
change the code tested at the frozen SHA.

## Important limits and minimal future design

**This API is useful but is not a turnkey fail-closed policy boundary.** Native
`bindExtensions()` reports `session_start` handler errors rather than necessarily
rejecting. Connection failure is normally a notification, not a process refusal.
An implementation must not equate “bind returned” with “required MCP is ready.”
Do not ship a loader that silently continues after missing required tools.

The factory also merges `pi.getMcpServers()` registrations with loaded entries.
Replacing `loadConfig` does not suppress an approved extension that itself
registers another server. The stage-one mandatory-integration review must cover
this behavior; arbitrary optional executable extensions remain unsupported.
Injected loaded entries bypass file-reader validation. Factory-time extension
registrations also defer registry mutation, so alias collision checks cannot see
one another during the factory. Native naming, live-registration checks, tool
exposure or notifications must not substitute for validating the complete
registry/selection input before startup.

A separately authorized implementation should retain the existing operator-owned
registry, immutable versioned refs, role checks, explicit empty/omitted distinction,
pin rechecks and bounded fresh-session rules, then:

1. Add a small strict MCP record type under selected refs only. Validate argv,
   absolute executable/cwd paths, allowed transport/exposure, bounded timeout,
   names and normalized namespace collisions **before** native startup. Reject
   unsupported fields rather than silently dropping them. No inline secrets,
   shell substitutions, package executors or OAuth/browser flows in an initial
   authless stage. Credential references need their own qualification later.
2. Produce a selected-only pinned plan, never the full registry or an inherited
   environment selection. Keep first-mate/crew/nested selections independent.
3. Keep ambient built-in MCP disabled; load a reviewed factory adapter explicitly
   and supply only selected loaded entries through `loadConfig`. Preserve the
   stage-one package/system-prompt preflight and mandatory auth/provider/permission
   integration review. Reject competing MCP registrations or unsupported
   integrations rather than deleting them or changing a route to make it work.
4. Prevalidate synchronously, capture extension load/start errors, and add a
   bounded required-capability readiness gate before any model turn. Missing,
   disconnected, conflicting or timed-out required capabilities refuse the run.
   Use a fresh closure per runtime and native shutdown for idempotent cleanup.
5. Preserve Privateer actor/control/brief, explicit mail and hosted-crew adapters,
   native auth/model locations, captain-selected provider/model argv, charter
   actions and existing tool permission policy. Bundle selection grants neither
   authority nor a provider route. A captain must authorize any production
   deployment against an exact artifact and environment.

A future loader needs its own direct CLI adapter, fail-closed startup/readiness,
registered-server exclusion, registry/role/replay tests, and attended/hosted native
qualification before implementation can be called production-ready. This fixture
does not qualify credentials, live inference, a complete personal profile, a
Herdr terminal, hostile same-user extensions, resume/fork or a production sandbox.
The qualification now independently contains ordinary IPv4/IPv6 transport in a
verified disposable network namespace. It is **not filesystem/process isolation**:
real HOME is retained, host files and pathname Unix sockets remain accessible to
trusted code, and this is not protection against hostile code deliberately using
those paths. No real credentials are read by the fixtures. No production launcher
or policy inherits this guard. Offline controls and inference refusal are retained
as defense in depth, not cited as the transport boundary.
