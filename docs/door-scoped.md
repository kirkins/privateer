# Scoped door protocol v2 (foundation)

Scoped v2 runs beside trusted v1. It is **not yet a usable MCP front door**:
there is no client grant/token store or delegated execution in this phase.
Every protected request is denied, including requests with credentials.
Telegram and live-voice continue to use trusted v1, unchanged.

## Registration and trust

Only the operator can register a door:

```text
privateer door add NAME --manifest /absolute/manifest.json --protocol 2
```

The adapter manifest may declare `"door_protocol": 2`, with kind `front-door`
or `front-door-scoped` (the latter requires protocol 2). Omission means trusted
v1 for a **new** registration, for compatibility. Re-running `door add` without
`--protocol` preserves the stored protocol, never the manifest's default.
`--protocol 2` can restrict a legacy manifest; `--protocol 1` cannot override
a scoped adapter manifest. A change from scoped v2 to trusted v1 requires an
explicit `--protocol 1` and prints a plain captain-authority warning. Unsupported
versions fail registration. A manifest with adapter ID `mcp` must explicitly
declare v2.

Registration stores the selected protocol in the fleet registry. Existing
rows migrate transactionally to v1. Restart uses the recorded choice, not the
adapter's current manifest or a runtime version claim. A v2 adapter cannot
downgrade itself by editing its manifest or sending a v1 request. Re-registration
is an operator action; it is not a runtime negotiation.

For every scoped registration the registry's manifest path points to a private,
core-written **guard manifest**, not directly to the adapter's manifest:

```json
{"schema_version":1,"kind":"front-door-scoped","door_protocol":2,"adapter_manifest":"/absolute/adapter/manifest.json"}
```

The guard references the original adapter manifest; the current core loads that
adapter and preserves its original working directory and relative argv. A fresh
immutable guard is written per registration so an existing supervisor can still
restart from its prior registration. Adapter edits do not change the guard's
protocol. Re-adding from a guard resolves to the original adapter, not a chain.

**Rollback safety:** before scoped v2, `load_manifest` accepts schema version 1
but requires kind exactly `front-door` (and an adapter executable/id). Those
binaries reject this distinct kind/form rather than ignoring the registry's
protocol column and launching a v2 door with captain authority. We chose a
distinct kind instead of a schema bump because the incompatible part is the
channel trust and registration form, not the v1 adapter schema. Telegram and
live-voice registrations still point directly to their unchanged v1 manifests.
Any experimental scoped rows created by the initial foundation commit must be
re-added with this version before relying on rollback safety; no older binary
can retroactively protect an unguarded row. As with v1, these are channel
authority checks, **not a sandbox against a same-user adapter**.

## Wire contract

JSON lines use the existing supervisor and bounded queues. The runtime sends
`hello` with `version: 2`, door/adapter names, config path and state directory.
Unlike v1, this hello contains no fleet inventory, default recipient or mail
coverage. V2 does not poll captain mail, initialize delivery cursors, build
digests or accept legacy `delivered` acknowledgments. Grant-bound reader
subscriptions remain future work.

The complete pre-authentication request allowlist is **`protocol {}`**:

```json
{"type":"request","id":"p1","command":"protocol","args":{}}
```

It returns `{"version":2}` without reading fleet state. No `meta.*`, `oauth.*`,
hello claim or wildcard bootstrap operation is accepted. `protocol` takes no
credential and no arguments. The supervisor's initial hello is transport
configuration, not an adapter assertion of identity.

Protected request shape (currently always refused):

```json
{"type":"request","id":"r1","command":"ships","args":{},"auth":{"token":"opaque-client-credential"}}
```

`id` is a string or number. `args` defaults to `{}` and must have a supported
object shape. The only envelope fields are `type`, `id`, `command`, `args`,
`auth`, and an optional `version` equal to 2. Authentication is exactly an
object containing one string `token`. Identity/provenance fields (`from`,
`actor`, `operator`, `captain`, `grant`, `liaison`, `provenance`,
`delegated_context`) are refused at every nesting level, never trusted.

Denials return the same nonrevealing structured error:

```json
{"type":"response","id":"r1","ok":false,"error":{"code":"not_found","message":"Not found"}}
```

The core records precise internal `scoped.deny reason=...` codes in the rotating
supervisor log (1 MiB, one previous file). It does not log request arguments,
IDs, commands or credentials. This is a door-channel error, not an OAuth HTTP
challenge; the later HTTP adapter must implement appropriate OAuth challenges.

**Known audit limit (phase 2.1):** these `scoped.deny` lines share `door.log`
with adapter stderr, so an adapter can inject lookalike lines or crowd out core
events. This is not an independently authoritative audit sink. Phase 2.2's
bounded, core-owned audit sink must close this gap.

## Central scope table

[`src/door/scoped.rs`](../src/door/scoped.rs) owns the command inventory,
strict argument shapes and required scope classes. V1 execution and v2
classification exhaustively match the same command enum. A new command needs
both an execution branch and a classification branch; inventory tests require
a valid mapped example and refusal of unknown fields for every command.

| Command / variant | Required scopes |
| --- | --- |
| `ships`, `status`, `waiting` | `fleet:read` |
| `held` | `mail:read` |
| `away` status (default) | `fleet:read` |
| `away` start/back | `captain:presence` |
| `digest` | `captain:presence`, `mail:read` |
| `send` request (default)/notice | `ship:message` |
| `send` reply with correlation | `ship:message`, `approval:answer` (conservative pending stored-request classification) |
| `dismiss` | `work:manage` |
| `file.put` | `file:transfer` |
| `approval` grant/deny | `approval:answer` (trusted confirmation still required before execution exists) |
| `permissions` without mode | `fleet:read` |
| `permissions` standard/yolo | `fleet:admin` |
| `stow` status (default) | `fleet:read` |
| `stow` start/resume | `fleet:admin` |
| `upgrade` plan (default)/confirm | `fleet:admin` |

Unknown commands, fields, actions, scalar types and unsupported payload variants
fail closed. Scope classification supports a deliberately narrow send payload
(empty object only); arbitrary payloads need the delegated provenance and
approval review, not just a top-level scope label. All replies conservatively
require approval authority until classification can resolve the stored request.
These restrictions do not change trusted v1 argument handling.

## Phase 2.2 integration boundary and remaining gates

`check_credential(door: &str, token: &str, required: &[&str]) ->
Result<DelegatedContext, Denial>` is the single core credential resolver. It
currently always returns `InvalidCredential`; absent credentials are separately
classified `MissingCredential`. A future resolver must bind the opaque token to
core-owned door/resource/grant state and re-read current grant validity, never
accept plugin-supplied identity. Its private context is not client-constructible.

Even after successful resolution, this admission path has an independent
`delegated_execution_unavailable` guard. It never calls v1's operator dispatch.
Removing that guard requires delegated execution, target/resource enforcement,
stored-request reply classification, presence handling, safe file handoff,
grant-bound confirmations and reader-bound mail acknowledgment. A scope label
alone is not authority to bypass any runtime or sensitive-confirmation gate.
This phase neither issues credentials nor exposes protected reads or writes.
