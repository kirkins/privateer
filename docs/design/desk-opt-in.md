# Per-door local desk opt-in (operator state only)

This is the opt-in/status slice of the [approved exposure contract](desk-exposure-plan.md#31-operator-opt-in-not-grant-authority). It adds **no client tools, scopes, capabilities or reader dispatch**. The existing subscription engine remains internal. An enabled binding is not a claim that the full roadmap or MCP exposure is complete.

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

First enable stores the domain, owner core, generation/incarnation, exact manifest revision/hash, approved replay policy, local switch-on recording time and a dated source-journal/barrier vector in the core registry. The recording time is diagnostic, **not** a remote event cutoff. A source without a proved current baseline is `switch_on_pending`, with any available dated evidence retained; late baseline arrival is not thereby classified as new mail. This PR does not implement replay selection or establish pending boundaries. That belongs to the next replay-eligibility slice, before client reads.

Identical enable is idempotent, including after restart or disable: it never moves the switch-on boundary. A changed domain, owner, incarnation, generation or manifest cannot rebind retained state. Manifest changes stop readiness with `stale_generation`; status additionally reports `manifest_changed`, even for a manifest revision this runtime cannot serve. Re-enable refuses pending a separately reviewed reconciliation protocol, rather than resetting replay or reclaiming slots.

Disable only clears enablement. It retains the binding, boundary and subscription custody. Enable/disable use the same registry writer serialization and external placement fence as local subscription work. Future client admission and release must check this binding inside their own transactions; this operator-only slice does not connect that serving path.

## Status and capacity

Status reports recorded enablement separately from current readiness. States include `no placement`, `remote owner`, `reverse-only`, `disabled`, `unavailable`, and `enabled at generation 1`. Refusal codes distinguish `no_placement`, `remote_owner`, `stale_generation`, pending placement and restore-fence mismatch. It reports `client_exposed:false` throughout this slice.

Status lists this door's grants without tokens or credentials: grant ID, scopes, ship restriction, lifecycle eligibility, and slots held in the configured domain. `eligible` means an active, nonexpired, nonrevoked `mail:read` grant bound to this door/resource; it does not assert current source coverage or that client tools are exposed. Counts include **all doors'** occupied slots in the domain: `slots_used`, `slots_free`, and `slots_limit:16`. Without a known domain, used/free are unknown (`null`), not a fabricated zero. Status does not initialize exposure/grant schema or retire readers.

**Revoking the owning grant is the only supported operator action to free a slot.** Engine admission retires terminal revoked/expired/missing authority transactionally. Status is observational: pause, disable, token rotation and abandonment never free a live slot, and automatic terminal expiry is not a manual capacity-management API. No close/reset/delete tool is introduced.

The selected-mailbox hint remains **“unsupported read direction / incomplete feature”**. Enabling a desk does not repair that mailbox, add reverse reading, relocate a desk, or advertise aggregate subscriptions.
