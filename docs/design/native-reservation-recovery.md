# Native typed reservation recovery (PR A)

This is **one exact resource hold**, not fleet repair. There is no door,
gateway, MCP tool, route change, pane relaunch, retry, claim/lease/workspace
cleanup or nonterminal job settlement in this interface. The older local
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
   grant or replace authentication. PR A accepts only local operator calls and
   a null link incarnation. No linked receiver exists in this PR. PR B must
   independently authenticate current unrestricted `fleet:admin`, visibility,
   resource and link incarnation before it can use a native seam. A non-null
   client-selected link is currently `unsupported`.

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
