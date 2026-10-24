# Scoped door protocol v2 (local delegated execution)

Scoped v2 runs beside trusted v1. Phase **2.3a** enables the local command
subset below with owner-issued grants. It is **not a complete MCP front door**,
OAuth server, cross-machine delegated channel, file sandbox or confirmation
surface. Telegram and live-voice retain trusted v1 semantics.

## Registration and trust

Only the operator can register a door:

```text
privateer door add NAME --manifest /absolute/manifest.json --protocol 2
```

A manifest can declare `"door_protocol":2`, with kind `front-door` or
`front-door-scoped` (the latter requires v2). Omission defaults a **new** row to
trusted v1. Re-adding without `--protocol` preserves the recorded protocol.
Explicit `--protocol 1` is necessary to downgrade, prints a captain-authority
warning, and cannot override a scoped manifest. Adapter ID `mcp` requires v2.
Restart uses registry-controlled trust, never a runtime hello/version claim.
A v1 row whose manifest has become scoped is refused before spawning.

A v2 registration points to a core-written private (0600) guard manifest:

```json
{"schema_version":1,"kind":"front-door-scoped","door_protocol":2,"adapter_manifest":"/absolute/adapter/manifest.json"}
```

The loader follows the source without changing its working directory or relative
argv. Atomic UUID staging replaces one stable `scoped-manifest.json` per door;
re-adding a guard resolves its original source, not another guard chain. Old
binaries expecting kind exactly `front-door` refuse this form rather than
silently launching a scoped adapter with captain authority. Initial experimental
unguarded rows must be re-added before relying on rollback safety.

Re-registering onto a **different adapter source path revokes all door clients**
in the registry transaction, including clients used by a currently running
supervisor. An unreadable old source also revokes (identity is unproven).
Re-adding the same source preserves clients and prints a carry-over notice only
when unrevoked clients exist. Changing bytes at the same path is still trusted
operator/plugin maintenance, not artifact identity verification. Removal and
v2-to-v1 downgrade also revoke, preventing name reuse from restoring tokens.
The supervisor reloads registration before restarting; an already loaded adapter
must be restarted to apply a new source. No keep-clients bypass is exposed.

## Wire contract

Existing bounded JSON-lines queues and supervision are reused. V2 `hello` has
version, door/adapter names, config path and state directory, but no fleet
inventory, default recipient or mail coverage. V2 does not poll/ack captain
mail or accept legacy `delivered` events. Reader subscriptions remain phase 2.4.

The **complete pre-authentication allowlist** is `protocol {}`:

```json
{"type":"request","id":"p1","command":"protocol","args":{}}
```

It returns `{"version":2}`, takes no auth or args, and reads no fleet data.
There are no `meta.*`, `oauth.*`, arbitrary dispatch or bootstrap wildcards.

Protected requests use an opaque owner-issued token:

```json
{"type":"request","id":"r1","command":"ships","args":{},"auth":{"token":"opaque-client-credential"}}
```

Only `type`, `id` (string/number), `command`, `args`, `auth` and optional
`version:2` are accepted. Args default to `{}`; unsupported fields, types,
actions and payload variants fail closed. Authentication is exactly one string
`token`. Reserved identity/provenance keys are refused at **every JSON depth**:
`from`, `actor`, `operator`, `captain`, `grant`, `liaison`, `provenance`,
`delegated_context`, `client`, `client_id`, `grant_id`, `resource`, `owner`,
`approver`. A scope never licenses a raw operator RPC.

All refusals, including runtime validation failures, return:

```json
{"type":"response","id":"r1","ok":false,"error":{"code":"not_found","message":"Not found"}}
```

Precise internal reasons go to core audit and the rotating diagnostic supervisor
log (1 MiB plus one previous file), without arguments, request IDs or tokens.
This is not an OAuth HTTP challenge; the future HTTP adapter must supply those.
Adapter stderr can forge log-looking text but cannot create core audit records.

## Command-to-scope execution table

`src/door/scoped.rs` is the strict command/args inventory; its command enum is
shared with v1. `src/door/delegated.rs` calls specific functions rather than
forwarding arbitrary args to v1 or generic operator dispatch.

| Command / variant | Required scopes | Execution in 2.3a |
| --- | --- | --- |
| `ships`, `status`, `waiting` | `fleet:read` | Local reads, ship-filtered; ships also filters cached linked inventory |
| `held` | `mail:read` | Ship/sender-filtered held mail, no acknowledgment |
| `permissions {}` | `fleet:read` | Existing read-only machine mode query; partial failures preserved |
| `stow {action:status}` (default) | `fleet:read` | Specific read-only stow function, filtered ship details |
| `away {action:status}` (default) | `fleet:read` | Presence metadata, not held mail |
| `away {action:start,words?}` / `back` | `captain:presence` | Explicit fleet-wide presence transition |
| `digest {}` | `captain:presence`, `mail:read` | Grant-owned, filtered durable digest returned synchronously |
| `send {kind:request}` (default) / `notice` | `ship:message` | Local ship mail, runtime-authored sender/provenance |
| `send {kind:reply,correlation}` | `ship:message`; also `approval:answer` for stored approval-bearing ask | Local exact correlation, sensitive asks refused |
| `dismiss {id,note?}` | `work:manage` | Local exact/unambiguous-prefix lookup, target/sender checked; sensitive asks refused |
| `upgrade` plan/confirm | `fleet:admin` | Refused: `grant_bound_upgrade_unavailable` |
| `permissions {mode}` / `stow` start/resume | `fleet:admin` | Refused: `delegated_admin_unavailable` |
| `file.put` | `file:transfer` | Refused: `descriptor_handoff_unavailable` |
| `approval` grant/deny | `approval:answer` | Refused: `trusted_confirmation_required` |
| Send/reply resolving to linked ship | As above | Refused: `linked_provenance_unavailable` |

Send accepts bounded reply deadlines and urgency, but only an empty payload.
Typed answers, arbitrary attachments and payload-level operations need a
separate argument review. Unsupported shapes are denied before credential
lookup; supported variants check their required scopes before execution-specific
refusals. Even a grant holding every scope cannot cross the deferred gates.

Ship restrictions are checked against the resolved send recipient, stored reply
sender, and dismissal home/sender. Read filtering occurs before aggregation and
caps for ships/status/waiting/held/digests, including stored senders in desk
copies. Reads do not advance captain or v1 door cursors. Default-recipient
resolution does not silently choose another allowed ship if the actual default
is forbidden. Global permission modes and presence remain fleet-wide scope
classes, not per-ship settings. Presence results omit mail routing lists/source
IDs; `back` returns whether it transitioned, not an unfiltered catch-up.

### Replies, secondary paths and presence

Mail classifies approval-bearing replies against the **stored request inside
its write transaction**: declared `approves`, runtime tool/block asks, project
permission asks and placement asks require `approval:answer`. An unauthorized
reply fails before inserting mail, operator markers, approvals or closing the
ask. Production/privileged kinds always require separate trusted confirmation,
even with `approval:answer`, and remain open. Ordinary replies create no
operator marker. Authorized non-sensitive approvals retain existing gates and
record authoritative provenance alongside their marker.

Liaison propagation is **not implemented in 2.3a**: `captain.relay` refuses a
source with delegated provenance instead of laundering it into captain words.
Scoped replies do not fan out to attached asks in other homes, and scoped
dismissals do not close those attachments. Their durable open facts remain
visible independently when their primary closes. Full constrained propagation
and gateway admission belong to 2.3b. No model-instruction text is relied on as
an authority check.

A scoped send, reply or dismissal **never ends away**, including failed sends
and clients also holding `captain:presence`. Only explicit away start/back
changes it. Presence transitions retain fleet-wide routing/catch-up behavior;
their generated liaison mail, away transition fields and catch-up digest rows
carry delegated provenance. This scope deliberately authorizes fleet-wide
presence side effects, not arbitrary liaison relay authority. V1 send still
calls back before validation, as before.

Explicit scoped digest generation uses a separate `client-UUID` stream. It
filters before rendering, stores provenance, and retries an existing pending
digest verbatim. At most 128 historical rows are retained per grant. It never
uses a plugin-global cursor or legacy acknowledgments. No pending digest is
claimed delivered: reader-bound acknowledgments/subscriptions remain phase 2.4.

## Local client lifecycle and provenance

Operator-only CLI, outside agent/job sessions (not dispatch/RPC/gateways):

```text
privateer door client issue DOOR --name NAME [--scopes CSV] [--ships CSV] [--expires-ms UNIX_MS]
privateer door client list [DOOR]
privateer door client inspect ID
privateer door client pause ID
privateer door client resume ID
privateer door client revoke ID
privateer door client rotate ID
privateer door client expiry ID --expires-ms UNIX_MS|none
privateer door client audit
```

Issuance/rotation/resume and widening expiry require explicit controlling-terminal
consent with the grant summary; a concurrent shortening cannot evade consent.
There is no noninteractive consent flag. Tokens are displayed **once on the
terminal**, not stdout, and lost output requires rotation. This is local owner
issuance, not OAuth. Environment markers and `/dev/tty` do not isolate same-user
agents with shell access: the harness must forbid lifecycle actions.

`door_clients` is distinct from ship-management grants. One UUID currently
identifies both client and grant; both names are explicit in provenance for
future multi-grant issuance. It binds owner `local-captain`, registration
`local-terminal`, door and exact resource `door:NAME`, scopes/optional ships,
expiry, pause/revoke, creation/update and last-use times. Empty scopes grant
nothing; omitted ships means unrestricted *within* scopes. Presets/in-place
scope editing and OAuth/public URL audiences are future work.

Two OS-random UUID v4 values supply 244 random bits per opaque token. Only the
SHA-256 digest persists. Indexed lookup is not timing-hidden; the subsequent
constant-time comparison adds no protection to that lookup. Every protected
admission rechecks current grant state under the lifecycle registry write lock.
**Admission is the revocation ordering point**: revocation denies the next
admission; it cannot undo an already admitted side effect. Execution uses local
home transactions; no claim of a distributed registry/home atomic transaction
or revocation-as-cancellation is made.

Every admitted action has a runtime UUID result ID. Results and created mail
payloads contain core-authored `{client,grant,resource,result_id,origin}`
provenance. Dismissed originals keep `dismissed_provenance`; dismissal notices
carry provenance. Explicit store-local audit guards propagate provenance to
secondary approval/permission audit calls and restore on exit/unwind, without
changing Caller authority or automatically inheriting into another home.
Approval records remain linked to their authoritative reply mail ID.

Core audit is independently bounded: 4096 lifecycle rows fleet-wide, 1024
request rows per door. Requests cannot evict lifecycle facts. Unattributed
denials coalesce by door/event/reason/minute, with count, first timestamp and
last-seen timestamp. Retention prunes by row count, not sequence span. Valid
admission first commits one `auth.success/admitted` row with result ID; execution
finalizes that same row to `request.success/executed` or `request.deny/REASON`.
An admitted row left after a crash means **outcome unknown**, not permission to
retry. This is not yet an idempotency/recovery protocol. Store/audit failures
fail admission closed; a post-effect audit/output failure cannot undo committed
mail and must not be interpreted as safe retry. No tokens, headers or arbitrary
args are audited. Old mixed-ring facts migrate on first use.

## Evidence and remaining gates

`tests/door.rs` includes real subprocess fixtures in
`tests/fixtures/door_clients.rs` and `tests/fixtures/door_delegated.rs`: owner PTY
issuance, unequal simultaneous grants, lifecycle/restart/redaction/retention,
command scopes, forged identities, approval replies, away preservation,
ship-filtered reads/targets, and source-change revocation. Existing trusted v1
Telegram, linked-mail and live-voice coverage remains separate.

Deferred: linked delegated execution/liaison propagation, grant-bound upgrades,
fleet administration, descriptor-safe file transfer, sensitive trusted
confirmations, reader-bound mail paging/acks, idempotency, HTTP/MCP/OAuth, and OS
containment. Same-user adapters remain trusted processes capable of bypassing
the intended channel via their account; these checks constrain external client
authority, not hostile plugin code. See the [design contract](design/mcp-front-door.md).
