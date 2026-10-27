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
`max_source_age_ms` defaults to 900000; this PR records that policy but implements
no source barriers or freshness claims.

The coordinator writes one captain decision card showing the domain, owner,
source directions and exact manifest hash. An operator's own `captain.reply`
choosing `accept` or `reject` records confirmation where it is written. Importing
a reply as mail does not record approval. Explicit `machine.desk.accept` is the
terminal alternative. No terminal or outbound connection is necessary on a
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
A status contact without approval never constitutes acceptance. Each frame is
limited to 256 KiB, 16 sources/proposals/topology records and 256 ships per source.
The bounded registry audit uses `desk.placement.*`, not ship-move events.

`machine.desk.status` and `.commit` retry callable contacts and return per-contact
errors. Reverse-only participants resume at the next collect. They do not open a
reverse route, recursively dispatch an operator command, or retry at another
owner. A certificate can exist at the coordinator while a participant is still
pending; retry exact records until they agree. Concurrent conflicting proposals
can leave a fenced availability gap; they never auto-select a winner. Reject a
pending card to release its reservation, then reconcile contacts before retry.

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
Normal mail and aggregate readers do not use that channel. Selected-mailbox v2
reader state remains independent and unchanged. Migration and generation-bound
aggregate writers are later PRs.

A synced, write-ahead `desk-placement-owner.json` beside the registry preserves
the exact enrollment/certificate across a registry-only restore. It is not a
replacement for backups, consensus, authentication or protection against an
operator starting a stale clone. Missing/mismatched registry records or guard
files fail closed. Do not delete the guard to re-bootstrap an owned desk. A
linked participant may replay the exact certified owner after a registry restore
or interrupted installation; a new generation-1 proposal is refused. There is
no forced recovery, timeout takeover or transfer in this PR. Single-machine
storage failures at the external-fence/registry boundary remain fail-closed and
need explicit exact-record recovery, not re-bootstrap.

`subscriptions:false` and `coverage_complete:false` remain in placement status.
An active placement does not imply that source mail has been ingested, that any
reader exists, or that fleet coverage is complete. Intake/barriers, subscriptions,
owner transfer, presence integration and cursor migration remain later work.
