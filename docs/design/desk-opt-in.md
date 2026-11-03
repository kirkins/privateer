# Per-door local desk opt-in (operator state only)

This documents operator state and the PR5–PR7 local raw subscription slice of the [approved exposure contract](desk-exposure-plan.md#31-operator-opt-in-not-grant-authority). PR5 adds scoped reads, PR6 ledger-owned acknowledgment and PR7 qualified MCP discovery/tools, with no new scopes. An enabled binding is not a claim that the full roadmap is complete. See the [MCP surface and recovery contract](../../plugins/mcp/README.md#operator-enabled-local-raw-desk-subscriptions).

## Commands

Native operator commands (closed JSON objects):

- `door.desk.enable {door, domain, mode:"fresh_replay", replay_policy:"open_and_new"}`
- `door.desk.status {door}`
- `door.desk.disable {door}`

Equivalent operator CLI:

```text
privateer door desk enable beta --domain beta-domain --mode fresh_replay --replay-policy open_and_new
privateer door desk status beta
privateer door desk disable beta
```

The door must already be registered as scoped v2. Agents and adapter requests cannot run these commands. No registration, grant expansion, placement acceptance, intake enrollment, contact, route change or deployment is performed. Placement consent and background intake remain separate prerequisites. Only a certified generation-1 **local** owner can be enabled; a callable remote owner is refused just as a reverse-only one is.

First enable stores the domain, owner core, generation/incarnation, exact manifest revision/hash, approved replay policy, local switch-on recording time and a dated source-journal/barrier vector in the core registry. The recording time is diagnostic, **not** a remote event cutoff. A source without a proved current baseline is `switch_on_pending`, with any available dated evidence retained; late baseline arrival is not thereby classified as new mail. Opt-in alone does not establish pending replay boundaries. The separately reviewed internal engine supplies replay selection and conservative proof handling before these scoped reads; a source without birth evidence remains unknown.

Identical enable is idempotent, including after restart or disable: it never moves the switch-on boundary. A changed domain, owner, incarnation, generation or manifest cannot rebind retained state. Manifest changes stop readiness with `stale_generation`; status additionally reports `manifest_changed`, even for a manifest revision this runtime cannot serve. Re-enable refuses pending a separately reviewed reconciliation protocol, rather than resetting replay or reclaiming slots.

Disable only clears enablement. It retains the binding, boundary and subscription custody. Enable/disable use the same registry writer serialization and external placement fence as local subscription work. Scoped read and ack admission and release check this binding inside their own transactions; neither disable nor output loss undoes already committed read custody.

## Status and capacity

Status reports recorded enablement separately from current readiness. States include `no placement`, `remote owner`, `reverse-only`, `disabled`, `unavailable`, and `enabled at generation 1`. Refusal codes distinguish `no_placement`, `remote_owner`, `domain_mismatch`, `stale_generation`, pending placement and restore-fence mismatch. It reports `scoped_reads:true`, `client_exposed:true` only when the binding is ready, and `mcp_exposed:true` when ready. This means eligible for the PR7 MCP adapter, not that a listener is running or a hosted client tested. These are operator diagnostics; client discovery separately needs the request-local qualified descriptor and `mail:read`.

Status lists this door's grants without tokens or credentials: grant ID, scopes, ship restriction, lifecycle eligibility, and slots held in the configured domain. `eligible` means an active, nonexpired, nonrevoked `mail:read` grant bound to this door/resource; it does not assert current source coverage or that client tools are exposed. Counts include **all doors'** occupied slots in the domain: `slots_used`, `slots_free`, and `slots_limit:16`. Without a known domain, used/free are unknown (`null`), not a fabricated zero. Status does not initialize exposure/grant schema or retire readers.

**Revoking the owning grant is the only supported operator action to free a slot.** Engine admission retires terminal revoked/expired/missing authority transactionally. Status is observational: pause, disable, token rotation and abandonment never free a live slot, and automatic terminal expiry is not a manual capacity-management API. No close/reset/delete tool is introduced.

## PR5 scoped v2 reads

`desk.subscription.open`, `resume`, `pull`, `show`, `thread` and `ack` are admitted. Every call needs the actual current bearer credential, `mail:read`, the exact local door/resource/grant, an enabled local generation-1 binding and independent ship visibility. Domain, owner and source routing are never client inputs. Two grants/doors have separate readers; rotated tokens for one grant share progress. Legacy NULL-binding readers cannot acquire an operator binding in place through this surface. This refusal is publicly `unavailable` with `retry:operator_diagnosis` (internal `legacy_binding`); it must not invite a resume loop, reset or rebinding. Public reads never fall back to ordinary local mailbox copies, establish replay cuts, initiate source contacts or mutate legacy cursors, lifecycle, away or digest state.

Arguments are closed as specified in [§3.2](desk-exposure-plan.md#32-scoped-v2-commands-and-mcp-tools), with canonical UUID handles/origins, validated mail IDs, limits 1–50 and nonempty continuations of at most 256 bytes. Only these new commands limit the request ID's **serialized JSON** to 256 bytes; larger IDs are refused before engine admission and echoed as null. Other commands' ID behavior is unchanged. The maximum admitted request-ID size (including on a later continuation), runtime provenance and comma-inclusive wrapper overhead are reserved before engine page planning; the complete core response is measured against 192 KiB inside the writer transaction, before commit. PR7 separately measures duplicated MCP result framing (3 MiB); caller ID/HTTP framing is additional and bounded by ingress request limits.

Results use `desk.subscription.v1`, frozen source coverage and replay-selection diagnostics. Internal typed placement refusals retain distinct `no_placement`, `placement_pending`, `restore_guard`, `remote_owner` and `stale_generation` diagnoses; public failures use fixed, non-disclosing messages and preserve safe admitted coverage for engine refusals. Credentials, visibility and the immutable exposure binding are checked again before releasing protected output, including coverage. Lost open/page output retains durable custody and can be recovered through open/resume and retained snapshot start tokens. Reads never implicitly acknowledge.

## PR6 scoped acknowledgment

`desk.subscription.ack {subscription, ack_key, refs}` requires all three fields and the **same** actual bearer credential, grant/resource/visibility and local binding checks as reads, at admission and release. The subscription is a canonical UUID; the server-issued key and each ref are nonempty ASCII strings of at most 64 bytes; the array has 1–256 entries. The core sorts/deduplicates refs, validates the entire batch and commits one ledger outcome atomically. Forged, wrong-reader/kind/incarnation or mixed invalid batches cannot partially acknowledge. Domain, origin, cursor and generic `idempotency_key` inputs are refused; no second replay cache is used.

A page key binds one normalized batch. For a distinct or partial-page batch, obtain another server key through open/resume. Retry an uncertain ack only with its **identical key and batch** while retained. Identical replay never extends its committed receipt. Unknown or expired keys return `stale_or_unknown`, not fabricated success. A commit error or a credential/binding/visibility change after commit returns `outcome_unknown`: no protected data is released, but this is not evidence the ack failed. After receipt expiry, recover pending facts with fresh served refs rather than assuming the uncertain operation had no effect.

Success includes `acknowledged`, `operation_complete:true` and separately dated source coverage; it does not prove complete input coverage, request closure, dashboard acknowledgment or digest delivery. Automatic acks release represented carried custody and split reservations, permit eligibility-aware prefix compaction and retire acknowledged closure pins. Acknowledged open pins remain to carry later closures, subject to the separately disclosed bounded demotion policy. Explicit history ack uses its independent ledger and cannot advance automatic progress or release automatic pins/custody. Other grants/doors, selected-mailbox, v1 and dashboard state remain independent.

Unacknowledged reads eventually backpressure at the existing two-snapshot, 256-automatic-ref and independent 128-receipt-per-namespace bounds; there is no close/reset/eviction workaround. Ack frees represented custody, not necessarily snapshots or retained receipts before expiry. PR7 adds qualified capability discovery, six thin MCP tools, closed error/coverage rendering and bounded transport results. It adds no new authorization path or DDL. Remote owners, moves, migration and subscription presence/digests remain deferred.

The selected-mailbox hint remains **“unsupported read direction / incomplete feature”**. Enabling a desk does not repair that mailbox, add reverse reading, relocate a desk, or advertise aggregate subscriptions.
