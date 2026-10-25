# Scoped door protocol v2 (delegated execution)

Scoped v2 runs beside trusted v1. Phase **2.3a**, the **partial 2.3b**
linked-gateway/file handoff increment, the **partial 2.3c** grant-bound
upgrade/admin increment, and **2.3d** reverse-link sends/liaison propagation
enable the command subset below with owner-issued grants. It is **not a complete
MCP front door**, HTTP OAuth server, file sandbox or confirmation surface.
Phase 4.1 adds the [standalone core OAuth state machine](door-oauth.md) and owner
pre-registration/login CLI; HTTP serving and browser defenses remain phase 4.2. Telegram and live-voice retain trusted v1 semantics.

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
mail or accept legacy `delivered` events. Phase 2.4 Part A adds the local pull
reader below; partial Part B adds ship-addressed reachable linked readers.
Scoped presence/held/digest calls use the existing desk owner. Full subscriptions
and desk generation handoff remain unimplemented.
A selected mailbox result is never a claim of fleet-wide inbox coverage.

The **complete pre-authentication allowlist** is `protocol {}` plus the exact
`oauth.authorize-begin`, `oauth.login-submit`, `oauth.consent-submit`, `oauth.token`,
`oauth.revoke`, `oauth.metadata` operations documented in [core OAuth](door-oauth.md).
The protocol request remains:

```json
{"type":"request","id":"p1","command":"protocol","args":{}}
```

It returns `{"version":2}`, takes no auth or args, and reads no fleet data.
There are no `meta.*`, `oauth.*`, arbitrary dispatch or bootstrap wildcards.
OAuth bootstrap results contain fixed OAuth `error` values for the future HTTP
adapter to map, not fleet data.

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
`approver`. A scope never licenses a raw operator RPC. The sole resource-field
exception is `auth.resource`, required for OAuth access tokens and checked
against their exact external audience and door. OAuth bootstrap has its own
strict field schemas rather than the delegated identity schema.

Authority/validation refusals return:

```json
{"type":"response","id":"r1","ok":false,"error":{"code":"not_found","message":"Not found"}}
```

An authentication-store outage instead returns `error.code:"unavailable"` with
`message:"Core unavailable"`, so HTTP adapters can return 503 rather than
misclassifying the outage as invalid credentials. No authority-denial reason is
revealed by this operational distinction. After authentication, uncertain write
outcomes return `error.code:"outcome_unknown"` with instructions not to resend
with a new key or without a key, and to check `mail_show` or the conversation.
Idempotency capacity refusals instead return `error.code:"retry_capacity"`:
nothing ran; retry later with the same key, or call without a key if retry
protection is not required. MCP surfaces both as explicit tool errors.
An admitted send exceeding the 65536-byte complete serialized mail/envelope
limit returns `error.code:"message_too_large"`, audited with that fixed reason.
This is a typed pre-commit refusal (including across a machine gateway), not an
execution failure inferred from exception text. A corrected keyed retry may reuse
the key. MCP records a value-free `tool.reject` and reports `invalid_arguments`
with field `body` and rule `maxEnvelopeBytes` rather than `not_found`.

Precise internal reasons go to core audit and the rotating diagnostic supervisor
log (1 MiB plus one previous file), without arguments, request IDs or tokens.
This is not an OAuth HTTP challenge; the future HTTP adapter must supply those.
Adapter stderr can forge log-looking text but cannot create core audit records.
A trusted MCP transport can submit authenticated `tool.reject {tool,field,rule}`
metadata instead of rejected arguments. Core admission checks a fixed tool/scope
inventory, field identifier (64 ASCII characters maximum), and validation-keyword
allowlist; it finalizes that client's normal bounded audit row as `request.deny`
with `invalid_arguments tool=... field=... rule=...`, and returns `{recorded:true}`.
It performs no fleet operation and is not an MCP tool. Only after successful
recording does MCP expose a value-free `invalid_arguments` diagnostic for a
visible tool. Hidden/unknown tools and authorization/route refusals remain
`not_found`; audit unavailability never yields an unaudited validation diagnosis.
The byte-limit rules `maxUtf8Bytes`, `maxJsonBytes`, and `maxEnvelopeBytes` are
also allowlisted; no observed size or argument value enters the denial metadata.

## Command-to-scope execution table

`src/door/scoped.rs` is the strict command/args inventory; its command enum is
shared with v1. `src/door/delegated.rs` calls specific functions rather than
forwarding arbitrary args to v1 or generic operator dispatch.

| Command / variant | Required scopes | Execution in 2.3a |
| --- | --- | --- |
| `capabilities {}` | Valid token, no additional scope | Only the admitted client's effective scopes and ship limits, plus normal request provenance; no other clients or fleet data |
| `ships`, `status`, `waiting` | `fleet:read` | Local reads, ship-filtered; ships also filters cached linked inventory |
| `held` | `mail:read` | Desk-owned, ship/sender-filtered held mail, no acknowledgment |
| `inbox`, `show`, `thread`, `ack` | `mail:read` | Local or directly reachable linked per-grant pull reader; bounded durable served refs, no implicit acknowledgment; digest ack goes to the desk |
| `permissions {}` | `fleet:read` | Local machine mode only; no gateway fan-out |
| `stow {action:status}` (default) | `fleet:read` | Local or named reachable machine, filtered ship details; `all:true` refused |
| `away {action:status}` (default) | `fleet:read` | Desk-owned presence metadata, not held mail |
| `away {action:start,words?}` / `back` | `captain:presence` | Explicit desk-owned presence transition; no local fallback on link failure |
| `digest {}` | `captain:presence`, `mail:read` | Desk-built, grant-owned filtered durable digest returned synchronously |
| `send {kind:request}` (default) / `notice` | `ship:message` | Local ship mail, runtime-authored sender/provenance |
| `send {kind:reply,correlation}` | `ship:message`; also `approval:answer` for stored approval-bearing ask | Local exact correlation, sensitive asks refused |
| `dismiss {id,note?}` | `work:manage` | Local exact/unambiguous-prefix lookup, target/sender checked; sensitive asks refused |
| `upgrade` plan/confirm | `fleet:admin` | Durable grant/resource-bound, expiring one-shot plans, exact commit and target/route equality check; keeps existing upgrade gates |
| `permissions {mode}` | `fleet:admin` | Standard/yolo only, local plus direct linked fan-out, explicit per-machine confirmation/partial results |
| `stow` start/resume | `fleet:admin` | Local or named reachable machine; resume also accepts `all:true`; receiver owns host ledger and mail |
| `file.put` | `file:transfer` | Descriptor-relative bounded local upload or dedicated linked gateway transfer; final commit transport failure gives `outcome_unknown`, begin/append transport failures give `execution_failed` |
| `approval` grant/deny | `approval:answer` | Refused: `trusted_confirmation_required` |
| Send/reply resolving to linked ship | As above | Reachable gateway or versioned reverse-link outbox; queued is not delivered; explicit old-gateway refusals give `linked_provenance_unavailable`; known remote denials are preserved; direct transport failures give `outcome_unknown` |

Send accepts bounded reply deadlines and urgency, but only an empty payload.
Typed answers, arbitrary attachments and payload-level operations need a
separate argument review. Unsupported shapes are denied before credential
lookup; supported variants check their required scopes before execution-specific
refusals. Even a grant holding every scope cannot cross the deferred gates.

Ship restrictions are checked against the resolved send recipient, stored reply
sender, dismissal home/sender before id/prefix matching, and the resolved
liaison before any away start/back state write. Hidden dismiss ids behave like
missing ids and cannot make visible prefixes ambiguous. Read filtering occurs before aggregation and
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

Liaison propagation in **2.3d** uses the source mail's runtime-owned stored
context, never a payload's provenance claim. `captain.relay` requires
`ship:message`, an allowed liaison, target home and asking ship. It propagates
the exact original context (including result ID, scopes and ship restrictions),
records source provenance, and never sets an operator marker. The existing
source-address and 24-hour age checks still apply. A relay is a **new write**:
immediately before writing, the runtime rechecks the source's client ID in the
local grant registry. The grant must still be bound to the same door/resource,
not revoked or paused, and not expired; denials are respectively `wrong_door`,
`revoked_grant`, `paused_grant`, and `expired_credential`. This does not refresh
the original scope/ship snapshot or recall the admitted source mail. A source
whose client ID is absent locally (including a remote-origin context) is refused
with `linked_provenance_unavailable`: this receiver cannot establish liveness
from a remote registry, even if a local door has the same name. There is no
gateway recheck or fallback to ordinary captain mail. Registry failure refuses
with `auth_store_unavailable`. As with initial send admission, a revocation after
the liveness check does not recall the admitted write. All stored
approval-bearing asks are refused through delegated relay, even with every
scope; production/privileged asks remain refused as before. A provenance-bearing
source without a runtime context fails closed (including legacy delivered mail).
Scoped replies do not fan out to attached asks in other homes, and scoped
dismissals do not close those attachments. Their durable open facts remain
visible independently when their primary closes. Attached-ask fan-out remains
deferred; each ask requires its own authorized answer. Reachable gateway admission
is described below. No model-instruction text is relied on as
an authority check.

A scoped send, reply or dismissal **never ends away**, including failed sends
and clients also holding `captain:presence`. Only explicit away start/back
changes it. Presence transitions retain fleet-wide routing/catch-up behavior;
their generated liaison mail, away transition fields and catch-up digest rows
carry delegated provenance. This scope deliberately authorizes fleet-wide
presence side effects, not arbitrary liaison relay authority; a ship-restricted
grant excluding the liaison cannot start or end away. Delegated away notes
describe relay as grant-constrained; presence alone grants no messaging scope. V1 send still calls back
before validation, as before.

Explicit scoped digest generation uses a separate `client/UUID` stream locally
and `client/ORIGIN_UUID/CLIENT_UUID` on a remote desk (`/` is forbidden in door
names; `:` is not). Stored provenance always retains the original grant UUID. Earlier ambiguous `client:UUID` rows
are ignored, not migrated or deleted: they could belong to a real v1 door. It
filters before rendering, stores provenance, and retries an existing pending
digest verbatim. At most 128 historical rows are retained per grant. It never
uses a plugin-global cursor or legacy acknowledgments. No pending digest is
claimed delivered merely by generation. The returned `ref` is served durably and
can be explicitly acknowledged using `ack {refs:[REF]}` (without `ship`). Raw
inbox cursors and digest summary cursors intentionally remain independent:
acknowledging a summary never consumes raw mail, or vice versa. Digest ack updates
its existing `client/UUID` row and removes its served ref in one registry
transaction. Pending summaries are bounded to 128; up to 128 nonpending history
rows are retained separately. No v1 digest/door stream is reused.

### Pull reader (phase 2.4 Part A and partial Part B)

See [the reader wire contract, schema and bounds](door-reader.md) for
`inbox`, `show`, `thread`, `ack`, continuation tokens and selected-mailbox coverage.
Reachable raw readers use the authenticated sibling gateway `fleet.reader.v1`;
reverse-only links explicitly report unavailable, and old peers unsupported.
Presence/held/digest and digest acknowledgment execute at the existing desk via
allowlisted `fleet.delegated.v1` command `desk.v1`, without local fallback. That
desk's summaries still read its local/already-collected homes only. Cross-machine
copy deduplication, aggregate inboxes, v1 push subscriptions and desk moves remain
deferred.

The receiving core also requires `desk_here` in its own registry before any
`desk.v1` operation; otherwise it refuses with
`linked_provenance_unavailable` and writes no presence, held or digest state.
Desk placement must agree on both machines, not just the calling core. In the
common topology where A links to B and then runs `machine captain B`, B still
records A as its collector. Thus `desk_here(B)` is false and `desk.v1` refuses,
even though A routes desk calls to B. There is no local fallback or recursive
forwarding. Resolving this two-machine placement disagreement is deferred to
part C; this increment does not make that topology a working remote desk.

## Linked gateway and file handoff (partial 2.3b)

Reachable-machine sends and replies use the new operator-authenticated machine
command `fleet.delegated.v1`, never `captain.send`, generic dispatch or ordinary
`mail.deliver`. Its arguments are `{context,command,args}`. Context is
`{version:1,client,resource,scopes,ships,result_id}`; client/grant identity, scope
and ship restrictions are a core-authored admitted snapshot. No bearer token
travels. Only `send`, `file.begin`, `file.append`, `file.commit`, `permissions`
`stow`, and versioned `desk.v1` are accepted. Admin receiver arguments cannot include `machine` or
`all`: fan-out belongs to the originating core, not to a recursive gateway.
The receiver validates the context version and operation/target, records the
same provenance, and executes against its own local home only. Replies are
classified against the receiver's stored ask inside the mail write transaction,
with the same scope and sensitive-confirmation checks as local sends. Gateway
arrival does not confer v1 captain approval authority.

Local registry admission is the revocation ordering point: revocation after
admission does not undo that send, even while transport is in progress. Old
binaries do not know this command and refuse it; there is no version negotiation
or downgrade to an unprovenanced captain envelope. A failed transport never
triggers an operator retry. It can leave outcome unknown (including after the
remote committed); failure is not proof that retrying with a new mail ID is
safe. Explicit same-ID mail retries keep the original issuer/provenance.
### Reverse-link outbox (2.3d)

Reverse-only `send`/`reply` admission writes a separate durable per-home outbox,
not an ordinary captain mail row. Collection carries
`{id,from:"captain",to,kind:"delegated.send.v1",context,args}`. This kind is
outside the ordinary mail kind allowlist. A pre-2.3d collector routes it to the
home then refuses `mail.deliver` with `unknown message kind`; it cannot silently
ignore new fields and deliver captain mail. An old reachable gateway likewise
refuses `fleet.delegated.v1`. Neither path retries with an operator envelope.
The new collector accepts the exact six-field envelope, checks routing/id
consistency, decodes the versioned context (including SCOPES), validates send
arguments/target, then uses the same delegated write path as the gateway.
Replies are classified against the receiving home's own stored ask, not an
origin desk copy or a sender's claimed request kind.

The immediate response has `queued:true,delivered:false` and a stable mail ID;
it is **not** a delivery confirmation. The collector later acknowledges the
outcome, stored in `delegated_outbox.outcome`; the origin creates an idempotent
captain notice carrying the delivery ID, outcome and original provenance.
The runtime notice and outbox completion commit in one per-entry transaction
on the same home connection; a failure before commit leaves both unwritten.
A valid matching notice left by the older implementation's crash window is
preserved and its outcome completes the row, even when the replay reports a
different error. An unrelated notice-ID collision is not adopted. Individual
outcome failures are recorded as `mail.delegated.outcome_failed` and skipped
(best effort if the audit store also fails); other delegated outcomes and
ordinary collected-mail acknowledgments continue. Failed entries remain pending
for retry rather than reporting completion without a notice.
Ordinary collected IDs are checked against the outbox before acquiring a write
transaction. A failing outcome is retried at most three times, with bounded error
text and an audited `delegated_outcome_retry` terminal state on the third failure.
Terminal rows are not offered by subsequent collection, do not churn audit, and
are **not** reported as delivered; they require operator investigation. Retry
state expires with its outbox row. Temporary remote refusals remain queued.
Duplicate collection is idempotent by mail
ID; pending entries survive restart. Local admission remains the ordering
point: revocation after admission does not recall queued mail. Same-ID retries
retain the original queued provenance in `mail.payload.provenance` (the outer
response provenance identifies the new request), and cannot adopt another
grant's entry or an existing ordinary mail ID. No token is stored in the envelope. The outbox is bounded to 1,000 entries per home and
64 KiB per envelope; terminal entries expire after 30 days on new admissions.
A lost acknowledgment is not permission to resend under a new ID. Reverse-link
file transfers and machine administration still require a reachable gateway.

Scoped file sources are opened relative to an opened door `state/` directory
using `openat`. Each directory component and the final source use `O_NOFOLLOW`;
opens use `O_NONBLOCK` so a FIFO cannot stall the supervisor. Parent traversal,
symlink directories/sources, nonregular files and multiple hard links are
refused. Regular-file/link-count/size checks use the opened descriptor. The
same descriptor is hashed, rewound and copied, never re-resolved by path.
Renaming/replacing the original name cannot redirect the admitted descriptor.
Both reads enforce the existing 50 MiB quota; remote commit verifies size and
SHA-256. Concurrent edits can fail transfer, not substitute an unchecked path.
The source directory/descriptor admission is not hostile same-user-plugin
containment. A failed upload is not a committed file; remote partial staging
retains the existing expiry cleanup. Each linked chunk carries the same context,
and local upload/remote gateway audit records preserve provenance.

### Grant-bound upgrades and machine administration (partial 2.3c)

Scoped upgrade plans live in the registry's `door_upgrade_plans`, never the
trusted v1 session slot. The opaque confirmation binds client/grant (currently
one ID), exact resource, immutable plan including commit/artifact, ordered
machines, installation and route hashes, and a five-minute expiry. There is no
plan-read endpoint. A new plan replaces only that grant/resource's prior plan;
expired rows are pruned and fleet-wide storage is bounded. Confirm selects and
consumes under an immediate SQLite transaction, checking owner/resource/expiry.
A different client cannot read or consume it. Consumption survives restart and
is never undone, even if re-planning or dispatch subsequently fails. A crash
between consumption and durable upgrade custody is an uncertain handoff, not
permission to replay the confirmation. Two connections racing to consume the
same confirmation have one winner.

After consumption the typed upgrade entry re-plans and compares the **whole**
plan, then uses the existing upgrade custody, worker, route-bound transport and
release verification gates. Provenance persists in upgrade request metadata,
audit and round state. This is runtime administration, not production approval:
no production approval is created or consumed. Ship-restricted grants excluding
an affected ship are refused. The operator/agent upgrade entry paths and v1
session confirmations are unchanged.

Permission mutations accept only `mode:standard|yolo`, preserving machine-local
standing authorizations and other policy fields under the existing lock and
atomic rename. Provenance is recorded before mutation and after success, and
propagates to secondary permission sweeps. Reads without `mode` remain local.
Stow mutations retain host-ledger custody, captain/owner checks and mail
semantics. Host hold/resume selection is serialized; the ledger's ship snapshot,
requests, notices and audits carry provenance. Restricted clients must include
every affected home (including the recorded stow snapshot), since a host hold
cannot be narrowed to one mailbox. Stow status filters before opening ship homes;
when any ship is excluded, host-wide `ready` is unknown (`null`), not an aggregate
of hidden jobs/replies. Stow-generated delegated mail can be relayed only when its original grant
also has `ship:message`, and only to allowed ships; it is never approval evidence.

Linked admin calls use `fleet.delegated.v1` only. Each receiver validates the
same scope/shape/target rules, touches only its own machine, and never retries
as operator. Permissions and resume-all return `machines` rows with independent
`confirmed`, `result` or `denial_code`, plus `partial`. Nominal remote successes
must contain the expected mode/hold and matching provenance before confirming
a write; malformed success is uncertain. A named stow returns that
machine's result, not a fleet-wide claim. Reverse-only targets are explicitly
unavailable; dispatched writes losing transport are `outcome_unknown`, not a
known refusal. Resume-all asks reachable machines to resume directly, without
requiring an unscoped status probe or implying read authority from admin scope.
Local resume notices name unqueried linked holds as unknown. No response claims
that a hold change stopped already-running payloads.

Origin-side ship preflight for permissions and upgrade uses the **cached**
linked inventory, not a forced refresh. A ship added remotely since the last
exchange may therefore pass origin preflight. This is not an atomic fleet
preflight guarantee: the receiver independently checks its current homes against
the carried grant. In particular, permissions may change an allowed local
machine then receive `ship_not_allowed` from a remote with a newly added ship;
the response reports `partial:true` and that machine unconfirmed, never fleet
success. Upgrade plans/rounds retain their existing per-target outcome semantics.
This increment documents the gap rather than adding a network refresh that
would still race remote ship creation.

Receiver post-commit `file.commit` audit and delegated mail read-back failures
also carry `outcome_unknown`. Remote denial decoding accepts execution-stage
codes only; credential expiry/revocation, wrong-door and other authentication
codes cannot be asserted by an already admitted remote.

## Local client lifecycle and provenance

Operator-only CLI, outside agent/job sessions (not dispatch/RPC/gateways).
**Run each confirmation command on its own, complete its prompt, then run the
next; never paste the list as a block.** Type-ahead is discarded before the
prompt where supported; mismatches report the expected action without echoing
arbitrary potentially secret input:

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
scope editing remain future work. OAuth applications, explicit consent and exact
external audiences are described in [core OAuth](door-oauth.md); their grants
reuse these lifecycle and enforcement paths.

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
finalizes that same row to `request.success/executed`, `request.deny/REASON`,
or `request.replay/cached` when returning a saved idempotency result.
An admitted row left after a crash means **outcome unknown**, not permission to
retry. The admitted audit row alone is not an idempotency protocol. Keyed scoped
`send`, `dismiss`, `ack` and away start/back now support optional `idempotency_key`:
a nonempty control-free string up to 128 UTF-8 bytes. The core registry's
`door_write_results` reserves `(grant,command,SHA256(key))` before effects,
binds a normalized argument/authority hash, and stores the original result or
denial. Authentication/current scopes precede lookup; changed restrictions
conservatively refuse cached results. Reservations and results retain for 24 h
from admission, with hard caps of 256 per grant/4096 global and 128 KiB per
result. Capacity refuses with `retry_capacity` rather than evicting unexpired
protection. Revocation/removal purge grant rows transactionally; expired-grant
rows are pruned before capacity counting. Explicitly proven pre-effect errors
release reservations for same-key retries; generic execution failures remain
protected. Pending reservations (crash/save failure) return `outcome_unknown`, never execute again
within retention. There is no registry/home/network atomic transaction or
exactly-once claim. See [MCP retry semantics](../plugins/mcp/README.md#writes-retries-and-asynchronous-delivery-phase-32a)
for normalization, expiry and asynchronous status limitations. Without a key,
each invocation is a new write. A send retried
with the same mail `id`, content and grant returns the stored duplicate with
its original provenance; the fresh request result ID does not change content
identity. Another grant or changed content cannot adopt that id. Store/audit
failures fail admission closed; failure to finalize the outcome after execution
returns `outcome_unknown`, never the retryable-looking `auth_store_unavailable`.
A post-effect audit/output failure cannot undo committed mail and must not be
interpreted as safe retry. No tokens, headers or arbitrary
args are audited. Old mixed-ring facts migrate on first use.

## Evidence and remaining gates

`tests/door.rs` includes real subprocess fixtures in
`tests/fixtures/door_clients.rs` and `tests/fixtures/door_delegated.rs`: owner PTY
issuance, unequal simultaneous grants, lifecycle/restart/redaction/retention,
command scopes, forged identities, approval replies, away preservation,
ship-filtered reads/targets, and source-change revocation. Existing trusted v1
Telegram, linked-mail and live-voice coverage remains separate. The linked
fixture now covers reachable delegated sends/replies, stored-ask denial,
unsupported gateways, transport failure without operator fallback and linked
upload/failure. Scoped file subprocess cases cover scope/target enforcement,
symlink/hard-link/FIFO/escape/quota refusals; deterministic rename-after-open
coverage is a descriptor unit test, not a probabilistic race claim.

`tests/fixtures/door_delegated_admin.rs` adds supervisor/subprocess admin scope,
provenance, upgrade restart/expiry/cross-client/replay coverage; the linked fixture
adds admin fan-out, per-host stow custody and post-commit transport failures.
A two-connection unit race tests atomic confirmation consumption. This is not a
claim of a simultaneous two-supervisor upgrade-dispatch race test.

Deferred: reverse-link file/admin operations, attached-ask fan-out,
sensitive trusted
confirmations, full desk-owned remote subscriptions, guaranteed crash-result
recovery, registration compatibility, and OS containment. Core standalone OAuth state is separate from those transport gates. The opt-in
[MCP door](../plugins/mcp/README.md) supplies Streamable HTTP reads and scoped
message/work/presence writes, owner-issued bearer tokens, and standalone OAuth
with explicit opt-in remote HTTPS proxy configuration. Same-user adapters remain trusted processes capable of bypassing
the intended channel via their account; these checks constrain external client
authority, not hostile plugin code. See the [design contract](design/mcp-front-door.md).
