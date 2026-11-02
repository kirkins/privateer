# Per-door local desk opt-in (operator state only)

This documents operator state and the PR5 scoped-read slice of the [approved exposure contract](desk-exposure-plan.md#31-operator-opt-in-not-grant-authority). PR5 adds **scoped v2 read commands only**, with no new scopes, MCP tools or capability advertisement. An enabled binding is not a claim that the full roadmap or MCP exposure is complete.

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

Disable only clears enablement. It retains the binding, boundary and subscription custody. Enable/disable use the same registry writer serialization and external placement fence as local subscription work. Scoped read admission and release check this binding inside their own transactions; neither disable nor output loss undoes already committed read custody.

## Status and capacity

Status reports recorded enablement separately from current readiness. States include `no placement`, `remote owner`, `reverse-only`, `disabled`, `unavailable`, and `enabled at generation 1`. Refusal codes distinguish `no_placement`, `remote_owner`, `domain_mismatch`, `stale_generation`, pending placement and restore-fence mismatch. It reports `scoped_reads:true`, `client_exposed:true` only when the binding is ready, and `mcp_exposed:false`. These are operator diagnostics, not client capability advertisement.

Status lists this door's grants without tokens or credentials: grant ID, scopes, ship restriction, lifecycle eligibility, and slots held in the configured domain. `eligible` means an active, nonexpired, nonrevoked `mail:read` grant bound to this door/resource; it does not assert current source coverage or that client tools are exposed. Counts include **all doors'** occupied slots in the domain: `slots_used`, `slots_free`, and `slots_limit:16`. Without a known domain, used/free are unknown (`null`), not a fabricated zero. Status does not initialize exposure/grant schema or retire readers.

**Revoking the owning grant is the only supported operator action to free a slot.** Engine admission retires terminal revoked/expired/missing authority transactionally. Status is observational: pause, disable, token rotation and abandonment never free a live slot, and automatic terminal expiry is not a manual capacity-management API. No close/reset/delete tool is introduced.

## PR5 scoped v2 reads (intermediate, not an operable public release)

Only `desk.subscription.open`, `resume`, `pull`, `show` and `thread` are admitted. Every call needs the actual current bearer credential, `mail:read`, the exact local door/resource/grant, an enabled local generation-1 binding and independent ship visibility. Domain, owner and source routing are never client inputs. Two grants/doors have separate readers; rotated tokens for one grant share progress. Legacy NULL-binding readers cannot acquire an operator binding in place through this surface. Public reads never fall back to ordinary local mailbox copies, establish replay cuts, initiate source contacts or mutate legacy cursors, lifecycle, away or digest state.

Arguments are closed as specified in [§3.2](desk-exposure-plan.md#32-scoped-v2-commands-and-mcp-tools), with canonical UUID handles/origins, validated mail IDs, limits 1–50 and nonempty continuations of at most 256 bytes. Only these new commands limit the request ID's **serialized JSON** to 256 bytes; larger IDs are refused before engine admission and echoed as null. Other commands' ID behavior is unchanged. The actual request ID, runtime provenance and wrapper overhead are reserved before engine page planning; the complete core response is measured against 192 KiB inside the writer transaction, before commit. MCP framing remains separate later work, not a tested transport claim here.

Results use `desk.subscription.v1`, frozen source coverage and replay-selection diagnostics. Internal typed placement refusals retain distinct `no_placement`, `placement_pending`, `restore_guard`, `remote_owner` and `stale_generation` diagnoses; public failures use fixed, non-disclosing messages and preserve safe admitted coverage for engine refusals. Credentials, visibility and the immutable exposure binding are checked again before releasing protected output, including coverage. Lost open/page output retains durable custody and can be recovered through open/resume and retained snapshot start tokens. Reads never implicitly acknowledge.

**No scoped ack exists until PR6.** Unacknowledged reads eventually backpressure at the existing two-snapshot, 256-ref and 128-receipt bounds; there is no close/reset/eviction workaround. This intermediate slice is not a usable MCP release. PR7 separately gates qualified capability discovery, tools, adapter error/coverage rendering and transport framing. Remote owners, moves, migration and subscription presence/digests remain deferred.

The selected-mailbox hint remains **“unsupported read direction / incomplete feature”**. Enabling a desk does not repair that mailbox, add reverse reading, relocate a desk, or advertise aggregate subscriptions.
