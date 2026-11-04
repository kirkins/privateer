# Initial desk placement (operator opt-in)

`machine captain` is the legacy local routing choice. The new agreement is a
separate, operator-only bootstrap, not a ship move, subscription, automatic desk
move, or owner transfer. Existing installations are not enrolled on upgrade.
Linked gateways share operator trust; core UUIDs are bookkeeping, not origin
authentication. No delegated command or door grant can invoke these verbs.

For the fictional one-way link `alpha -> beta`, with the desk on reached `beta`:

```text
machine.desk.propose {"domain":"survey-domain","owner":"beta","sources":["alpha","beta"]}
machine.desk.status {}
machine.desk.accept {"proposal_id":"<returned UUID>","manifest_hash":"<returned hash>"}
machine.desk.commit {}
```

These are ordinary Privateer JSON commands (`COMMAND --json JSON`, or `--file`
for a JSON file). Either participant may propose. Names are the link names,
including the local name established by `machine add --as`. A single-machine
installation uses `owner:"here", sources:["here"]`; `here` may explicitly name
an otherwise unnamed local core. Sources must be explicit. The optional
`max_source_age_ms` defaults to 900000. Placement alone establishes no source
barrier or freshness claim; intake must separately prove each source.

The coordinator writes one captain decision card showing the domain, owner,
source directions and exact manifest hash. An operator's own `captain.reply`
choosing `accept` or `reject` records confirmation where it is written. Importing
a reply as mail does not record approval. Explicit `machine.desk.accept` is the
terminal alternative; `machine.desk.reject {proposal_id,manifest_hash}` aborts
an uncertified proposal even if its decision card is lost. Both require the
exact binding. An aborted proposal cannot be accepted. Delegated replies,
including grants with `approval:answer`, never record placement approval.
No terminal or outbound connection is necessary on a
reached participant: proposal, card, confirmation, participant acceptances and
certificate travel on its collector's subsequent contacts.

Each participant checks its own pinned proposal, direct source/owner direction,
legacy state and exclusive enrollment reservation. The proposer coordinates if
it contacts all sources; otherwise the owner coordinates. An owner-only source
participates through the owner. There is no third-party mail relay and no newly
configured connection. A missing legal source/owner contact is rejected with
`no_legal_contact` before staging. Configure a direct source-to-owner link for
`push` or owner-to-source link for `collect`, then refresh contacts and retry.
Inventory is not acceptance.

## Durable records and wire format

`desk_placement_record` reserves the machine's captain desk (not just a name) to
one non-aborted proposal. The record contains:

- `proposal`: `proposal_id`, `manifest_hash`, `decision` mail ID and immutable
  `manifest` (`domain`, revision 1, source-age policy, owner core UUID, generation
  1, incarnation UUID, coordinator UUID and sorted sources).
- Sources pin core UUID, display name, ship membership and `intake_direction`:
  `local`, `push` or `collect`.
- Participant-local `local_role`, `intake_direction`, `approval`, acceptance set,
  state (`pending`, `active`, `aborted`) and immutable `certificate`.
- The certificate binds that exact proposal and the sorted acceptance set for
  every participant. No timeout elects a different owner.

The certificate, explicit `captain_desk` (`here` on the owner, peer name on a
source) and the coverage-domain source/member projection install in one registry
transaction. The owner's `collected` link stays intact. Routing consults the
agreement before legacy inference. Pending/conflicting/restore-unvalidated state
cannot serve as a desk. Legacy general captain collect/push checks the agreed
owner tuple; correlated-answer return paths remain separate. These legacy mail
transfers are **not** source-checkpoint evidence.

The independent `desk_placement` frame has `version:1`, receiver name `you`, local
core `description`, bounded topology and `records`. It rides `fleet.link` and
`fleet.collect`, both ways. Dedicated operator-gateway verbs
`desk.placement.propose`, `.accept`, `.commit`, `.status` use
`{machine,desk_placement}` and return status plus the same frame. An accept frame
delivers approval; every receiver still checks and records its own acceptance.
Status/propose gateway verbs reject approval/certificate delivery; those use
accept/commit or the explicit collect frame. Polling status is not acceptance.
Each frame is
limited to 256 KiB, 16 sources/proposals/topology records and 256 ships per source.
The bounded registry audit uses `desk.placement.*`, not ship-move events.
Placement absorption is per-record and best-effort during directory and mail
exchange: invalid/conflicting records or frames emit `desk.placement.exchange_failed`
diagnostics without blocking ordinary mail. Failed facts remain at the sender
for retry on later contacts. Active/aborted replay does not recheck the bootstrap
ship inventory; subsequent ship creation does not break the link.

`machine.desk.status` and `.commit` retry callable contacts and return per-contact
errors. Reverse-only participants resume at the next collect. They do not open a
reverse route, recursively dispatch an operator command, or retry at another
owner. A certificate can exist at the coordinator while a participant is still
pending; retry exact records until they agree. Concurrent conflicting proposals
can leave a fenced availability gap; they never auto-select a winner. Reject a
pending card (or use `machine.desk.reject`) to release its reservation, then
reconcile contacts before retry. Replaying an aborted record cleans up an
interrupted removal of its matching uncertified guard.

## Legacy fences and recovery

Enrollment takes the independent registry-adjacent `desk-placement.lock`.
Presence, digest and v1 cursor writers hold its shared side for the duration of
the mutation; staging/acceptance/installation take the exclusive side. Existing
away periods and pending digests must be reconciled **before** enrollment; this
PR refuses unresolved imports instead of choosing a timestamp winner. Existing
legacy cursor, reader, away and digest rows are not rewritten or acknowledged.
Enrolled machines fence legacy presence/digest/v1 cursor writes with
`legacy_writer_fenced`. A trusted v1 supervisor retains a narrow control-card
channel: it emits the matching open placement decision with a `desk-placement:`
ref, and accepts its operator reply without ending away. A control receipt does
not advance any mail/digest cursor or count as consent; an unanswered card may
repeat after supervisor restart. This works even without an existing v1 cursor.
Normal mail and aggregate readers do not use that channel. Ordinary door sends
remain available: `back` is a read-only no-op when no away period exists, even
after enrollment. This does not un-fence the v1 inbox, presence or cursor writers.
The decision card states this door impact. Selected-mailbox v2
reader state remains independent and unchanged. Migration and generation-bound
aggregate writers are later PRs.

A synced, write-ahead `desk-placement-owner.json` beside the registry preserves
the exact enrollment/certificate across a registry-only restore. It is not a
replacement for backups, consensus, authentication or protection against an
operator starting a stale clone. Missing/mismatched registry records or guard
files fail closed. Do not delete the guard to re-bootstrap an owned desk. A
linked participant may replay the exact certified owner after a registry restore
or interrupted installation; a new generation-1 proposal is refused. There is
no forced recovery, timeout takeover or transfer in this PR. A single-machine
domain can explicitly replay its external write-ahead record with
`machine.desk.commit {proposal_id,manifest_hash}`, using the exact binding in
status's `recovery` field. A pending record stays pending; only the already
committed certificate can install an owner. A linked domain instead requires
participant certificate replay; a local accept cannot validate a restored owner.
The guard never downgrades a certificate while replaying local acceptance.

`subscriptions:false` and `coverage_complete:false` remain in placement status.
An active placement does not imply that source mail has been ingested, that any
reader exists, or that fleet coverage is complete. Intake/barriers and opt-in
subscriptions have their own independent evidence; owner transfer, presence
integration and cursor migration are not performed by these commands.

## Per-machine legacy reconciliation (operator runbook)

This is an explicit loss/history boundary, **not** provenance recovery, acknowledgment,
mail deletion or production rollout authorization. Work on each source separately
(for example fictional `alpha`, `beta`, `gamma`), using its local registry and
existing placement. Do not copy a digest between machines or change a provider
route. Read-only plans are agent-callable; apply/revoke/release/readmit and
`machine.desk.status` remain operator-only. No MCP mutation tools are provided.
Keep the plans and bounded human reasons in private operator records, not chat
coverage. Use `COMMAND --file /absolute/path/request.json` with the JSON bodies
below; replace placeholders with the returned local values, never shell-expand
untrusted JSON.

### Preconditions and rollout gate

- Back up the registry **and** retained homes plus the external placement guard.
  Confirm exact active placement/certificate, owner generation/incarnation and
  legal push/collect direction on every participant. Do not remove fences to
  make a plan succeed. No new connection or reverse route is needed.
- Allow ordinary bounded idle intake and legal contacts to publish imported
  homes and refresh source proof. Membership must be proved, homes imported or
  explicitly released, and all pinned unresolved/local history closed and read,
  with no pending receipt. Integrity gaps cannot be cut. A release is never
  evidence that an absent home's unread/open counts were zero.
- **Before any rollout**, run isolated old/new interop tests for both legal
  push and collect directions: no-cut strict v3 proof must remain decodable;
  older strict v3 decoders must discard the optional cut metadata sidecar
  without rejecting ordinary intake pages/mail or losing custody; new peers
  must round-trip cut digest, date and home counts, and withdraw the public
  flag on a post-revocation contact. Also exercise v2/missing/malformed proof,
  stale barriers, restart and downgrade/upgrade with the same retained data.
  Missing/unsupported proof must never yield complete coverage. The unit
  strict-decoder fixtures cover the field-level gate, not a deployed mixed
  binary fleet: run the full mixed-version subprocess gate against the exact
  candidate artifacts before authorizing rollout. Do not roll out merely
  because a scripted local pilot passed.

### 1. Plan, then apply release or readmission if needed (slice B)

For each retained home requiring an explicit decision:

```json
{"home":"/absolute/retained/alpha","ship":"alpha","operation":"release"}
```

Run `machine.desk.home.plan`. Review `plan.allowed`, census state, registry
revision and the returned digest. Release is allowed only for the exact retired
registration, with no active registration sharing that home, absent on disk and
not already released. Do not rename a live directory to make it absent.

Then run `machine.desk.home.release` with a **fresh** digest and a bounded reason:

```json
{"home":"/absolute/retained/alpha","ship":"alpha","digest":"<plan digest>","reason":"Explicitly accept this retired missing-home boundary"}
```

If a released home reappears, it is fenced as `reappeared`, not silently
re-enrolled. Inspect it, plan again using `operation:"readmit"`, then run
`machine.desk.home.readmit` with the same home/ship, that plan's digest and a
reason. Readmission requires that exact released ship/home and a readable,
retained home. It removes the live release, **not** the durable loss boundary
or old cached facts. Let intake re-import/reconcile before proceeding.
Skip these mutations for already imported homes; an unreadable or corrupt
present home must be repaired, not disguised as released.

### 2. Plan, then apply the legacy cut (slice C)

On the source itself, get its core UUID from the placement's source inventory.
Run `machine.desk.legacy.plan`:

```json
{"source":"<local source core UUID>"}
```

Review the returned digest and plan: exact placement binding, membership,
registry revision, loss epoch, per-home high-water/counts and pinned receipts.
The plan reads homes without opening an agent home or changing lifecycle.
It bounds work to 50 homes and 2000 pins/receipts. New mail, reopened/unread
pins or changed registry membership invalidate the boundary rather than being
silently folded into history. Then run `machine.desk.legacy.apply`:

```json
{"source":"<local source core UUID>","digest":"<fresh legacy plan digest>","reason":"Keep reviewed closed legacy observations outside authoritative desk mail"}
```

Apply re-plans under the registry transaction and requires the exact digest.
Only one active cut per source is allowed; revoke before replacing it. Legacy
observations stay retained and are never projected as authoritative desk mail.

### 3. Verify proof, not just the operator decision (slice D)

Run existing `machine.desk.status` on the owner after ordinary idle work and
legal source contact. It can drive existing placement contacts, but does not
force source proof or bypass admission. In `source_status`, inspect:

- `legacy_cut.digest`, `cut_at_ms` (source operator decision time), hashed
  per-home `homes[].count`, and `released_homes`. These are frozen cut counts;
  the separate existing `homes` diagnostics contain dated unread/open counts.
  Old cuts whose bounded apply audit expired before upgrade report a null
  decision time, never a fabricated timestamp. Proof from a pre-diagnostics
  peer can report a digest with null home details/date until refreshed. New cut times are durable even
  after audit trimming.
- `legacy_cut.observed_at` is desk-measured: now for a local cut, or receipt
  time of its matching valid remote barrier. A retained remote vector cannot borrow
  a newer unmatched checkpoint's time or keep a public flag after proof withdrawal. No remote read is opened for status.
- `legacy_cut.proof_holds` is derived from the same completion evaluation as
  source `complete`, including current binding, loss/copy checks, local
  revalidation and desk-clock freshness. False means the cut is recorded but
  does **not** establish current coverage. Check source `state` and `reason`.

Public subscription coverage and existing desk MCP results expose only
`history:"legacy_cut"` with `history_as_of` (desk observation milliseconds).
This means history before the boundary is **legacy, closed**, not proved
provenance. No cut digest, home counts, paths or names enter that flag. No cut
means both fields are omitted. Neither the flag, empty output nor continuation
completion is an all-clear; scope, source barriers and coverage still matter.

### Refusals, revocation and rollback

- `digest mismatch` / `home changed`: state raced the plan. Inspect a new plan;
  never retry a stale digest or edit database rows to force acceptance.
- `proved membership`, `imported or released homes`, `legacy home bound`:
  finish enrollment/reconciliation or explicitly decide the retained-home
  boundary. Unknown membership cannot be waived by a cut.
- `closed and read`, `not pending`, reopened/unread or newer unresolved mail:
  resolve the actual lifecycle through authorized workflows, then re-plan.
- `integrity_gap`: repair/investigate conflicting identity/content; it cannot
  be waived. New loss epochs after a cut independently withdraw proof.
- `pin/receipt bound`, `backpressure`, status budget exhaustion: incomplete,
  not empty. Allow bounded work/retry; do not raise a proof claim manually.
- Wrong source/binding, `stale_generation`, missing guard or unsupported proof:
  restore exact placement and supported evidence. Do not elect another owner.
- `reappeared` / retention refused: use a new readmission plan for the exact
  released home; present/unreadable homes are not releasable.

To withdraw, run `machine.desk.legacy.revoke` locally:

```json
{"source":"<source core UUID>","reason":"Withdraw the reviewed history boundary"}
```

Revocation remains possible after binding changes. It atomically removes the
active cut/pins/date, retains audit and mail, and removes the local public flag
immediately. The owner's remote flag changes at its next valid legal contact;
there is no instantaneous cross-machine revocation promise. Until then it is
only a dated observation. Re-plan before any later apply.

Downgrade/rollback is fail-closed: do not delete schema, guard, loss boundaries,
mail or retained diagnostics to satisfy an older binary. Optional metadata is
omitted entirely without a cut; old strict proof parsers may reject a cut's
new optional diagnostics and mark coverage incomplete, while ordinary strict
intake frames/mail continue. Old/missing proof cannot create eligibility.
Upgrade/reconcile again to regain proof. Revocation does not recover missing
homes or erase loss, and readmission does not erase the historical release.
Backups must preserve these independent durable facts together.
