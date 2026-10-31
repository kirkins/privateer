# Typed reservation recovery: native permits and scoped gateway

This is **one exact resource hold**, not fleet repair. The scoped door/gateway
and MCP may apply an operator-issued permit, but can never issue one. There is
no route change, pane relaunch, retry, claim/lease/workspace cleanup or
nonterminal job settlement in this interface. The older local
operator `resource.recover {key,force}` command is unchanged; its broader
semantics are not this interface and must never be forwarded by a receiver.

## Operator workflow

All commands below require an existing home and ledger. Only normal native
ledger initialization installs the schema; observation, issuance, revocation
and execution never create/migrate either store, chmod files, repair journals
or change admission policy. Incompatible legacy stores refuse. Use the normal
upgrade workflow separately, not a recovery action to upgrade a store.

1. Obtain an exact native reference:

   ```sh
   privateer --home /srv/alpha/.privateer resource.recovery.reference --json '{"job_id":"job-alpha"}'
   ```

2. Create a request file containing the returned `reference`, the intended
   originating authority and a single idempotency key:

   ```json
   {
     "request": {
       "reference": "REPLACE WITH THE RETURNED OBJECT, NOT THIS STRING",
       "authority": {
         "grant": "grant-alpha",
         "resource": "resource-alpha",
         "grant_revision": "one",
         "resource_revision": "one",
         "visibility_revision": "one",
         "link_incarnation": null
       },
       "idempotency_key": "recovery-alpha"
     }
   }
   ```

   The authority fields describe the intended origin; they do **not** create a
   grant or replace authentication. Native local apply accepts only a null
   link incarnation. Terminal issuance may bind a non-null intended gateway
   incarnation; only the authenticated scoped resource gateway can apply it.
   The gateway independently checks current unrestricted `fleet:admin` at
   host and receiver, visibility/resource revision and link fences. Clients
   cannot supply authority or a link incarnation.

3. On the target, outside an agent/job session, run:

   ```sh
   privateer --home /srv/alpha/.privateer resource.recovery_permit.issue --file /srv/alpha/request.json
   ```

   Read the exact binding at the controlling terminal and type the fresh
   eight-character challenge. Neither stdin, `--yes`, a JSON confirmation, a
   standing approval nor an RPC issues a permit. The terminal is a trusted
   operator path under the same local-user trust boundary as native grant
   lifecycle commands; it is not a sandbox against arbitrary same-user code
   that can manufacture a PTY or change local state.

4. Execute with an argument file containing the **same request** and returned
   `approval_id`:

   ```sh
   privateer --home /srv/alpha/.privateer resource.recovery.apply --file /srv/alpha/apply.json
   ```

   Revoke unused authority with `resource.recovery_permit.revoke` and an
   `approval_id`. All new commands use the existing-store CLI entry, not the
   ordinary RPC dispatcher (whose startup can initialize homes).

## Scoped door and MCP workflow

`resource_recovery_reference {machine,ship,job_id}` requires `fleet:read` and
selected-ship visibility. It reads existing stores without reconciliation,
process control or migration, returning a closed opaque `reference` and the
caller's intended originating `authority`. Only a retained terminal job with
native evidence of process absence is actionable. Missing and hidden targets
share `recovery_target_stale`; a reference confers no authority. Home and ledger
identities are opaque random-origin/file-identity bindings, never paths.

The operator copies that reference and authority into the request above, chooses
the exact intended idempotency key, and issues the permit **on the target's
terminal**. The foreground check is advisory: a same-session process ignoring
SIGTTOU can take foreground. The human reading the exact binding and typing the
fresh challenge is the barrier within the same-user trust boundary.

`resource_recover {machine,ship,reference,approval_id,idempotency_key}` applies
the permit. It requires existing `fleet:admin` with **null/unrestricted ships**
at both host and receiver; named lists, including all current names, refuse.
Reading references separately needs `fleet:read`. There is no client/chat/voice
confirmation, permit-issuance tool or approval echo. Grants are not expanded.

Callable linked peers explicitly negotiate `resource_recover` on
`fleet.resource-action.v1` before the internal read-only `target` probe and
mutation. A configure-capable peer without recovery refuses `unsupported`;
reverse-only peers refuse without spool, recursion, route creation or operator
fallback. All three calls share the originating link-incarnation fence. The
receiver reads/applies only locally owned homes. The internal probe checks
current ledger/home/native incarnation, intended authority/permit and the exact
hold, returning a closed binding with the full reference for originating key
admission. Native execution repeats exact custody/evidence checks under its
locks. A matching retained historical result precedes permit/hold/job checks
in the probe as well, so deletion or permit expiry never prevents authorized
same-key replay. No probes or actions initialize/migrate stores.

Originating global pins bind operation, arguments, approval, full reference,
ledger/link and effective authority/visibility. They share the existing 24-hour,
256-per-grant/4096-global action bounds. Matching calls always reach the native
transaction to recover historical results; conflicting keys refuse. A new key
must never be used to escape uncertainty. Malformed post-dispatch replies,
transport loss, ambiguous commit or post-effect release refusal return
`outcome_unknown`; credential/resource/visibility checks still gate replay and
release. The acknowledgment is closed `resource.action.v1` (operation
`resource_recover`, selected machine/ship, stable native result ID, reference,
applied, outcome and authenticated originating provenance). Native ledger
results remain unchanged `resource.recovery.result.v1` records. Neither result
contains process identities, paths, native prose or unrelated state. The six recovery
refusals below are preserved as value-free door/MCP error codes.

Fixture map: `tests/fixtures/door_resource_recovery*.rs` exercises real supervised
doors, linked gateways and controlling-terminal issuance for scope/alias/escape
refusals, missing/expired/revoked/reused permits, unequal grants, changed
job/home/hold/ledger, live/unknown evidence, restart/replay after deletion,
unsupported peers, transport/malformed reply loss, link replacement and release
revocation. Denial database-byte comparisons flush fixture WAL edits first;
actions themselves do not checkpoint. Native recovery and originating
idempotency suites cover concurrent calls, PID reuse/descendants, rollback,
legacy nonmigration and retention saturation. MCP suites cover fixed schemas,
discovery, annotations and closed refusal rendering in both SDK eras. No live
account, voice approval, autonomous repair or production deployment is claimed.

A permit expires ten minutes after issuance. It binds the versioned operation
and exact deletion effect; native recovery incarnation; opaque ledger origin
and file identity; home origin, file identity and canonical location; locally
owned ship; task and job incarnation; reservation generation and exact record
fingerprint; intended originating grant/resource/visibility revisions;
request digest and idempotency-key hash. Its confirmation provenance, expiry,
revocation and consumed-result state are target-resident ledger facts.

## Custody, evidence and transaction

The recovery capability has its own durable native incarnation. Ledger
replacement is additionally fenced by the existing random action origin and
filesystem device/inode identity. Every reservation insert receives a fresh
random generation, including delete-and-reinsert of identical bytes. Updates
are fenced by the record fingerprint. There is no raw reservation-key input,
PID/path/force override, batch or generic action payload.

Resolve/open the existing home and configured ledger first. The lock order is
**home writer, then ledger writer**. Keep the home writer until the ledger
transaction has settled. Re-read the retained job without reconciliation and
compare the exact reservation under the ledger writer. Check terminal status,
exact task/job incarnation and local ship ownership; require elapsed native
start grace and recorded supervisor/payload identities in both copies.
Inspect their union, including both escaped-descendant lists, so lagging ledger
evidence cannot erase a live process. PID reuse is compared by start identity,
not PID alone. Payload process groups are checked even when their leader is
gone. Missing/malformed identities, inaccessible namespaces, inspection errors
and saturated descendant tracking preserve capacity. This uses native process
tracking evidence; it does not introduce an OS containment/cgroup guarantee.

Permit consumption, deletion of that reservation, durable result and the
separate recovery audit commit in one ledger transaction. Failure before
commit rolls everything back. No job row is updated, even for a terminal job;
no scheduler state is changed. Recheck home/configuration/ledger identities
before committing. Ambiguous commit reports `outcome_unknown`.

## Replay and retention

Recovery keys are global within the target ledger. A conflicting request,
approval or originating authority cannot reuse one. Matching calls serialize
and recover the original result **before looking for the deleted hold**.
Replay still checks current target identity and local ship ownership. Permit
expiry/revocation after successful consumption does not erase that historical
result. Lost replies must be retried with the original key, not a fresh key to
escape uncertainty. There is no exactly-once promise beyond result retention.

Results expire after 24 hours. Policy and recovery results share the existing
256-per-grant and 4096-global admission bounds; saturation refuses instead of
evicting an unexpired result. Permits have separate matching count bounds and
expire from storage 24 hours after their expiry. Issuance, consumption and
revocation are recorded in a separate count-bounded recovery audit (at most
4096 rows), which survives result/permit pruning. Recovery is never recorded
as policy configuration. Audit rows include bindings/provenance, before/after
hold state, result ID, key hash and time as applicable.

The closed recovery refusals are `recovery_target_stale`,
`recovery_not_eligible`, `recovery_process_alive`,
`recovery_identity_unknown`, `recovery_approval_required` and
`recovery_approval_stale`. Missing/hidden/changed targets share the first code.
Other refusals use `unsupported_arguments`, `unsupported`, `retry_capacity`,
`execution_failed` and `outcome_unknown`; process/SQLite prose is not returned
by the action dispatcher. Native terminal consent and operator-role errors
remain local CLI diagnostics, with no network surface.
