# Door mail coverage and desk-generation handoff

## Status and recommendation

Design for [#452 phase 2.4](mcp-front-door.md#9-phased-delivery-plan-prs-and-evidence), part C. **This document changes no runtime behavior.** It extends the required subscription contract in [MCP front door §5](mcp-front-door.md#5-cross-machine-mail-away-routing-and-duplicate-policy); the older “current behavior after #454” subsection is not the complete v2 implementation baseline.

Recommend **desk-owned subscriptions**, served by the desk core to an authenticated door-host core. Keep selected-mailbox reads as a separate surface. Add explicit source coverage and a fenced, generation-changing desk handoff. Do not move the desk when starting a door, infer reachability from a ship directory, or turn a failed remote read into a successful local read. Initially, reverse-only readers report unavailable coverage rather than introducing a second remote execution queue.

Examples use machines `alpha`, `beta` and `gamma`, and ships `survey` and `relay`; these are fictional. “Fleet” below means an explicit configured coverage domain, filtered by the admitted grant, not discovery of arbitrary machines. “Owner” means the core authorized to mutate the desk stream, not an adapter, grant or ship.

## 1. Today's model: storage, direction and visible gaps

### Mail and links

- A ship's captain request and its open/closed state originate in that ship's home. Copies can exist in a local captain home and on another machine; a copy's mailbox sequence is not the original's sequence. `captain::waiting_visible()` enumerates local active homes and filters both holding home and sender under delegated visibility. See [captain.rs](../../src/captain.rs), `fleet`, `waiting_visible`, `held_here`, and `with_local_asks`.
- A configured link is **one-way callable**: `alpha -> beta` means `alpha` records role `collects` and can call `beta`'s gateway; `beta` records `collected` and cannot initiate a call back. The request and response both carry data, but that does not create a reverse connection. The directory is cached inventory, not a callable route. See [machine-link architecture](../architecture.md#linked-machines), and [machines.rs](../../src/machines.rs), `Machine`, `collect`, gateway `fleet.collect`/`fleet.collected`.
- `alpha` periodically collects pending ship mail and eligible captain mail from `beta`, delivers by stable mail ID, then acknowledges accepted/refused deliveries. A crash before acknowledgment can repeat a copy. Captain mail normally starts at link creation; older captain history is not automatically copied. `captain_destination()` puts a correlated answer in a local home holding its ask, otherwise in `captain_home()` (liaison, else first ship). Unknown destinations can wait and eventually produce a visible refusal.
- By default the linking machine collects captain mail from the reached machine. `machine captain beta` on `alpha` instead records a local `captain_desk(machine,set_ms)` choice. `collect()` stops taking `beta`'s captain mail; `push_captain()` sends captain mail since that choice **plus still-open older requests/decisions**, then closures, using `fleet.captain`. Accepted/refused records prevent repeated transfer; unreadable homes are skipped and retried. Closed older asks and older notices are not a complete history handoff. See `set_desk`, `push_captain`, `push_messages`, `push_closures`, and `mail::awaiting_captain` in [mail.rs](../../src/mail.rs).
- There is no general captain-mail relay through a third machine. A source must be linked and placed toward its desk itself. `holds_desk()` also infers a desk from a `collected` link when no explicit placement exists; several such links can be ambiguous. A local placement command is not agreement by both machines: setting `beta` on `alpha` can leave `beta` inferring that `alpha` owns its desk. `desk_here()` and scoped `desk.v1` then refuse execution. No durable shared generation or transfer certificate resolves this today.

### CLI and door reads are different

[Captain dispatch](../../src/captain.rs), `dispatch`, routes the operator's `captain.waiting` and `captain.feed` to an explicitly remote desk; waiting adds local-only asks. Replies/dismissals stay local when `held_here()` finds the item. As changed for #481, `captain.send` to a locally hosted ship stays in that ship's home even when the desk is elsewhere (the desk may not reach back). Other sends use the desk's routes. Consequently the remote desk feed is **not** a record of every local outgoing send. Gateway captain operations execute against their own homes, never recursively forward. See also the captain-desk paragraph in [architecture.md](../architecture.md).

[Trusted v1 doors](../../src/door.rs) poll local homes, including copies already collected there. `cursor()` persists a per-door/per-home `door_cursor`, initialized from the captain's acknowledged position; `Session::carried` bounds session acknowledgments. Digest acknowledgments require an emitted digest. Door acknowledgments neither acknowledge the dashboard nor another door. `Session::waiting()` calls the local helper, not the desk-routing CLI wrapper. `door_mail_delivery()` in [machines.rs](../../src/machines.rs) only reports topology; it contacts nobody and cannot collect or repair coverage.

The newer scoped v2 baseline is more capable, but not a fleet subscription:

| Surface | Actual coverage and state |
| --- | --- |
| `inbox`, `show`, `thread`, mailbox `ack` | [reader_link.rs](../../src/door/reader_link.rs) selects one local or directory-routed linked mailbox. [machines/reader.rs](../../src/machines/reader.rs) calls `fleet.reader.v1` only through a `collects` link. Reverse-only, unsupported and failed peers return incomplete selected-mailbox coverage; there is no reverse reader queue. |
| Local reader state | [reader.rs](../../src/door/reader.rs) persists machine identity, mailbox incarnation UUID, per-client/resource/mailbox generation and contiguous cursor, sparse served refs, and snapshot-bound continuation tokens. Transactions validate a whole ack batch before changes. Only actually served refs can advance an authorized contiguous prefix. |
| Copy suppression | `copies`, `copy_known` and `compact` prove copies from allowed active **local** homes and their reader cursors/served rows. This is not cross-machine reader deduplication. Grant/client identity on a peer is namespaced by the originating core UUID via `reader_context()`. |
| `waiting`, `status` | [delegated.rs](../../src/door/delegated.rs) reads visible local homes and returns conservative `mail_delivery` wording, hiding operator-only topology. No remote waiting aggregation. |
| Scoped away, held, digest and digest ack | [desk.rs](../../src/door/desk.rs) uses `delegated_desk_call()` and `fleet.delegated.v1` / `desk.v1`. It refuses ambiguous, reverse-only or failed desk routes instead of mutating local fallback state. A receiving gateway verifies `desk_here()` and executes locally only. |
| Digest inputs | [digest.rs](../../src/digest.rs), `collect_visible`/`build_visible`/`delegated`, uses the desk's local stores and registry; already collected copies can participate. It does not query uncollected remote homes. A successful desk operation or digest ack is not evidence those inputs cover the fleet. |

Current reader bounds include a 256-record sparse window, 32 continuation records per client/resource, 24-hour continuation expiry and a 512 KiB page-body budget; tool limits cap pages at 50 items. These are existing implementation facts, not proposed unbounded history retention.

[plugins/mcp/tools.py](../../plugins/mcp/tools.py), `MAIL_COVERAGE`, `mail_inbox` and `fleet_waiting`, explicitly says selected-mailbox coverage is not fleet coverage and waiting is not a fleet-wide all-clear (#476). Preserve that wording until stronger behavior actually ships. The diagnostic `subscriptions:false` does not mean linked selected-mailbox reads are impossible.

### Concrete non-desk cases

For `alpha -> beta`, a door on `beta` while the desk is `alpha` sees captain mail held on `beta`, but cannot pull `alpha`'s desk or collect from `alpha`. A scoped send may queue for reverse collection; that is not a reader transport. A door on `alpha` with an agreed desk on `beta` can read a selected `beta` mailbox and route scoped presence there, but still lacks an aggregate subscription to all desk-held and source-held mail. In either direction it can miss uncollected remote notices, older history left behind at a placement change, and local-only outgoing sends on another machine. Neither inventory nor successful sends closes these gaps.

## 2. Options

| | (a) Desk-owned subscriptions | (b) Door-side collection |
| --- | --- | --- |
| Correctness | One owner for reader delivery, held classification, digests and catch-up. Complete only if source coverage is proved; desk location alone is insufficient. | Can inspect reachable sources that the desk has not collected. Must merge copies, closures, cursor namespaces and concurrent placement changes itself. Merely pulling the desk's current homes has the same input gaps as (a). |
| Failure modes | Desk outage blocks fresh subscription data/presence. Source outages leave explicit gaps. Lost responses replay durable refs. Handoff must fence the old owner. | Door can show partial reachable mail during desk outage, but cannot invent authoritative away/digest state. Different doors can silently disagree unless each reports an identical coverage model. Restart needs a durable merge journal per host. |
| Complexity | Shared desk journal, source checkpoints, per-door/grant readers, delegated RPC, handoff and coverage accounting. Reuses a single presence owner. | Distributed per-host merge/deduplication and source cursors, plus desk routing anyway for presence. More copies of sensitive mail and more recovery states. A full solution converges toward a desk service plus host caches. |
| Link needs | Door host must reach desk for synchronous pull; sources must reach desk or desk reach sources for ingestion. | Host must reach every source it claims to cover, or receive a separately designed reverse transport. Reaching only one peer never proves a whole domain. |

**Reverse-only impossibility:** if only `alpha -> beta` exists, `beta` cannot synchronously read `alpha`, whether `alpha` is desk or source. It may receive data in the response/request of `alpha`'s next poll, but only if a new protocol explicitly carries it. Configuring a route, tunnel, desk move or operator fallback is not a reader's authority.

A future bounded reverse request/reply spool could let `alpha` poll requests queued on `beta`, execute at `alpha`, and return results on another contact. That is asynchronous, needs fresh grant admission before each protected page, durable correlation/expiry/cancel semantics and separate tests. Existing reverse-send collection cannot be repurposed as proof of these properties. **Defer this transport**: the initial design reports `reverse_only` with retry/remediation, while allowing already received data to be displayed as dated partial data. If `alpha` does not poll, neither option can promise fresh data on `beta`.

## 3. Recommended protocol

### 3.1 Authority and identities

Implement a versioned core-to-core `fleet.door-mail.v1` envelope, negotiated independently of `fleet.reader.v1` and `desk.v1`. Names below are proposed operations, not today's API. The plugin supplies only its normal permitted request arguments; it cannot supply peers, owner identities, raw SQL, paths, grants or provenance.

A protected envelope binds protocol version, authenticated initiating core ID, request/idempotency ID, destination desk ID and expected generation, registered door identity/resource, reader identity, operation, arguments and core-resolved delegated context. The receiver validates the configured link peer **and** the claimed origin binding; a caller-supplied UUID is not authentication. No gateway follows an arbitrary forwarded route. A generation mismatch yields a checked owner hint, never an automatic operator retry.

A v2 reader key is `(origin_core, registered_door, resource, grant_id)`; tokens of the same grant share it. Different grants and doors remain independent even for one application. Trusted v1 readers use their registered trust record and door identity, not a forged v2 grant or external Full Captain assertion. Keep authoritative provenance separate from internal namespaced storage keys, as [DelegatedContext](../../src/door/scoped.rs) does today.

| Operation | Authority and bounds |
| --- | --- |
| `coverage` / `open` / `resume` | `mail:read`; derives subscription/reader identity from admitted context. Returns only allowed coverage, creates or resumes bounded reader state, never acknowledges mail or changes placement. Fleet inventory/waiting metadata requires `fleet:read`; if it includes mail bodies, also require `mail:read`. |
| `pull` / `show` / `thread` | `mail:read`; filter both conversation participants and source visibility under the current ship restriction, including desk copies. No arbitrary-address mailbox access. Returns opaque refs and continuation, not implicit acknowledgment. |
| `ack` | `mail:read`; only refs carried to this reader, owner generation and subscription incarnation. No dashboard ack, request closure or presence change. |
| `held`, `digest`, `digest_ack`, `presence` | Preserve existing command scope table: held and digest ack require `mail:read`; explicit digest generation requires `mail:read` plus `captain:presence`; away status requires `fleet:read`; start/back requires `captain:presence` and existing target checks. All execute only at the recorded desk owner. |
| `source.page` / `source.commit` / `source.status` | Internal authenticated linked-core replication, not exposed to grants or adapters. Only the linked source's own captain conversations, closures and source checkpoints for the configured desk domain; no operator command dispatch. Core replication is distinct from permission to reveal replicated content to a door. |
| `handoff.prepare` / `handoff.page` / `handoff.seal` / `handoff.activate` / `handoff.status` | Operator-authorized placement transfer bound to old/new core IDs, domain, expected generation and transfer ID. Only configured participants may stage/activate it; never implicitly authorized by `mail:read`, subscription startup or delegated presence. A future scoped placement API needs explicit `fleet:admin` mapping and existing owner policy, not access to these internal verbs. |

The grant-owning host rechecks the current grant on **every** protected request, including resume, continuation, ack and delivery to the adapter. Remote admission uses a bounded, request-bound delegated snapshot, not a stored subscription's initial scopes. A grant restriction invalidates visibility/continuation state and prevents old unread refs from being newly exposed. Never send bearer credentials over the machine link. The desk checks operation scopes/ships again and records admitted provenance.

Define admission ordering explicitly: revocation committed before host admission denies that request; revocation during an already admitted remote operation cannot retract data/side effects admitted there. Recheck at host release can discard an arriving page after revocation, but cannot undo its served rows. Do not prefetch further pages or run unattended subscriptions using cached grant authority. A future reverse transport must renew this admission, not replay expired snapshots indefinitely.

### 3.2 Source journal and coverage barriers

A desk subscription needs more than reading whichever copies happen to be local. Maintain a durable desk intake journal referencing stable **origin** `(core UUID, mailbox incarnation, mail ID)` and source order, with local aliases for collection/desk copies. Preserve correlation, open/closed state and source checkpoint evidence. Existing mail IDs remain externally useful; never use a home path, ship rename or a copied `seq` as global identity. Copy provenance must be carried by new ingestion, not guessed from equal sequences or identical bodies. Conflicting origins/content for one purported copy produce an integrity gap, not silent suppression.

Source pages have bounded bytes/items, an immutable upper checkpoint and ordered events for mail and closures. Commit source intake and its checkpoint together before reporting acceptance; a failed receipt is safe to replay. Local mail remains the source of truth for its request lifecycle. The journal is a projection, not permission to answer an ask or manufacture approval evidence. Use idempotent import intents when source home and registry cannot commit atomically; never claim a distributed SQL transaction.

Initialize each source with a bounded snapshot of retained captain history, including **all unread mail and open requests regardless of age**, then replay changes from a recorded barrier. Include local-only outgoing captain sends and closures so #481's routing stays unchanged while its observational gap can close. A source that cannot supply required retained facts reports `history_gap`/`unreadable`; a legacy desk push's timestamp cutoff is not a sufficient baseline. Do not reroute or re-offer ship mail merely to rebuild reader coverage.

Record the configured domain/source manifest revision, membership and checkpoint vector. Retired or unreachable sources with unread/open facts cannot disappear just because a new directory omits them. Bootstrap from all authorized local homes and known linked sources; transitive or unregistered fleet members remain explicitly outside the domain, never silently claimed observed. Topology conflicts and unknown membership prevent a fleet-wide completeness claim.

For a fresh aggregate read, take a barrier at each required source, fetch through it, and report a **vector snapshot**, not a globally simultaneous instant. Source ingestion can ride the already legal collect/push direction, but needs new versioned checkpoint evidence; an old peer's successful collect is insufficient. A reverse-only source can become covered through its own authenticated polling/carry if it participates in this ingestion protocol. Until that barrier arrives, cached data is partial. No timeout promotes a stale checkpoint to fresh coverage.

### 3.3 Durable delivery, acks and duplicates

At the desk persist subscriptions, reader identity, generation/incarnation, source checkpoint vector, contiguous per-origin acknowledged prefixes, bounded sparse served/acked refs, paging snapshots and independent digest/catch-up baselines. Persist the host's subscription identity and last known owner; host caches are not cursor authority. Reserve served refs transactionally **before** releasing a page. Restart resumes these records; lost output leaves harmless refs that are re-carried, not acknowledged. Resume never trusts a client-provided high-water mark.

Ack behavior:

- Bind random opaque refs to reader, owner generation, subscription incarnation, origin/mail identity and kind (raw mail versus digest). Validate the entire batch, including visibility and generation, before any mutation. Client sequences cannot advance a cursor.
- An ack lost before commit changes nothing. An ack committed with its response lost may be retried with the same bounded idempotency key and identical ref batch, returning its recorded outcome. After receipt retention expires, return `stale_or_unknown`, not fabricated success. Existing selected-mailbox ack behavior need not be silently changed.
- A forged, future, wrong-reader, wrong-origin or wrong-kind ref fails closed. Sparse acknowledgments compact only a proved contiguous prefix of eligible mail; no unseen gap can be skipped by `show` or thread reads. Concurrent acks have one transactional outcome.
- A read never closes an open request. Dashboard acknowledgment, raw delivery acknowledgment and digest delivery are distinct facts. A digest cannot globally acknowledge its input mail or suppress another reader.

At-least-once delivery is intentional. Deduplicate repeated origin/copy identities within one reader, retaining aliases and closure facts; never deduplicate by rendered text. Two doors or two grants can each receive the same mail. Bounded sparse records and cursor/checkpoint proofs replace an unbounded delivered-ID set. If retention removes a proof needed to suppress a copy, safe redelivery or an explicit coverage gap is preferable to treating unseen mail as read. Do not prune unread, open or pinned pending delivery facts to free a queue; stop intake/page admission and report backpressure.

### 3.4 Desk moves: fenced generation handoff

The current `captain_desk.set_ms` is not an ownership generation. Introduce a domain ID, owner core ID, monotonic generation plus unguessable incarnation, and a durable transfer record. Incrementing a local integer on two disconnected machines is **not** election. No automatic takeover on timeout, registry restore or link failure.

Use a cooperative transfer from old owner `alpha` at generation `g` to `beta` at `g+1`:

1. **Prepare.** An operator submits `(transfer_id, domain, expected_owner=alpha, expected_generation=g, target=beta)`. The old owner serializes transfers and verifies the target/call direction and capacity. Target records `staging`, never serves it. Placement ambiguity must be resolved explicitly before bootstrap; one local `machine captain` update cannot certify agreement.
2. **Copy.** Page an authenticated manifest and snapshot to the target: source journal/aliases, unread/open/closure facts, source barriers, reader cursors, pending deliveries, away period and routing counters, digest/catch-up rows and baselines. Hash and count chunks; repeat by transfer ID. No target grant/token duplication is required: reader keys still refer to their grant-owning host. Live intake can continue with a bounded delta log; apply backpressure if it fills.
3. **Seal and fence.** After target confirms durable staging, old owner takes its owner-write lock, drains admitted owner mutations, fences all reader acks/pulls/presence/digest builds, and records the final delta/barriers and sealed manifest. Imports from multiple stores need replayable intents; seal cannot succeed while any required intent is unresolved. The final ownership transaction records an **irrevocable successor certificate** binding domain, `g`, `g+1`, both core IDs, transfer ID and sealed manifest. The old owner is now permanently fenced for `g`; it only serves transfer status and a verified redirect. It cannot unilaterally roll back this certificate.
4. **Activate.** Target verifies the certificate through the authenticated configured relationship, durably installs the final state, and atomically changes `staging` to active owner `g+1`. It cannot become active from a prepare message or elapsed timeout. The source certificate designates the successor even during the availability gap before activation: there is one recorded successor and possibly no serving owner, never two serving owners. Both records reference one transfer, not rival elections.
5. **Resume.** Door hosts verify the new owner through configured links/status; they must have a callable route to resume. Sources learn the generation and reconcile barriers; old routes return a hint or explicit stale placement, not recursive forwarding. Until all required sources reconcile, results remain partial. Old pending deliveries are reissued with **new refs** at `g+1`; already acknowledged prefixes and request state are preserved, not reset to the dashboard cursor.

Every owner mutation checks the active generation inside the same transaction as its cursor/presence change. An ack committed before sealing is included in the final delta. An ack racing after fencing is refused without mutation. **Never translate an old ref into a new-generation ack**, even if it names the same mail and reader. Pending mail/digests can therefore repeat after a move; they cannot disappear because an old ack arrived at either desk. Any retained receipt for an ack committed before sealing is explicitly a historical `g` outcome, not admission or acknowledgment at `g+1`. Intake arriving after the sealed barrier stays unacknowledged at its source until the successor durably accepts it; the old owner must not report acceptance and discard it.

Recovery: restart before certification resumes staging or allows an explicit abort under the old owner's lock; after certification only forward recovery/activation of that exact successor is allowed. A target receipt lost on the wire is recovered by `handoff.status`. A target crash after activation cannot make the old owner serve again. Simultaneous moves, moves back and delayed old activation messages compare expected generation/transfer ID and fail stale. A second transfer starts only from the active successor.

This protocol sacrifices availability during a partition. It cannot safely force a move if the old owner is permanently lost: operator recovery needs verifiable fencing of that installation and a separately reviewed recovery procedure. Restoring an unfenced old registry backup must not reactivate it; bootstrap/restore needs current ownership validation, otherwise it stays read-only/unavailable. A local certificate file is not protection against an operator deliberately running a stale clone. Do not claim consensus or partition-tolerant failover.

### 3.5 Away, digest and paging

Only the active desk classifies held mail and mutates authoritative away state, breakthrough budgets, digest builds and catch-up decisions for these subscriptions. Adapter hosts relay; failed routes cannot create local away forks. Scoped sends preserve today's rule that they do not end away, including failed sends; presence changes remain explicit. Trusted local v1 behavior stays unchanged until a deliberate migration.

Digest inputs come from the intake projection and carry its coverage vector. If a source is missing, a useful partial digest may still be delivered with visible omissions, but its baseline advances **only** through represented source checkpoints. Preserve counts, IDs/correlation, deadlines, open requests, closures and omitted-source facts when combining notifications. Pending summaries are transferred intact across desk moves, with new-generation ack refs; no second builder runs at the old owner. Request content remains filtered for each grant, including old pending summaries after restriction. Returning from away updates that owner's subscription catch-ups once; it must not consume independent MCP raw inbox cursors as if they were trusted v1 push cursors.

Use stable keyset paging with per-source upper checkpoints and a bounded merge snapshot. Continuations bind reader, visibility revision, owner generation, operation/thread anchor, domain manifest revision and snapshot; expire them explicitly. Limit items, serialized response bytes (including metadata), scanned rows, source fan-out, in-flight refs, pending snapshots, retry/idempotency receipts and transfer chunks. Empty pages may have continuations when work limits are hit. Reject an oversized item with a structured gap/handle policy, never silently drop it. Window pressure stops new serving without discarding pending unread facts. Avoid holding a registry transaction over network calls.

### 3.6 Coverage on every result

All new mail, waiting, presence/digest, ack, resume and handoff responses, including empty and failed ones, carry typed coverage or a non-disclosing refusal envelope. Keep `coverage.complete` separate from `continuation`/`more`, side-effect outcome and transport success. A complete page is not a drained inbox; an accepted send is not completed work.

Proposed fields (names subject to implementation review):

```json
{
  "coverage": {
    "scope": "desk-domain",
    "complete": false,
    "domain": "opaque-domain-id",
    "manifest_revision": 8,
    "owner": {"id": "opaque-core-id", "generation": 4, "state": "active"},
    "snapshot": "opaque-vector-handle",
    "sources": [
      {"id": "visible-source-a", "state": "covered", "through": "opaque-checkpoint"},
      {"id": "visible-source-b", "state": "reverse_only", "through": null}
    ],
    "other_machines": "not_queried",
    "retry": {"action": "wait_for_source_contact", "after_ms": 30000}
  }
}
```

Required semantics:

- Scope distinguishes `selected-mailbox`, `desk-domain`, `local` and `operation-only`. Selected-mailbox `complete:true` never means fleet-wide. Ack/presence success may have operation-only completeness while input coverage remains partial; include those separately rather than upgrading the whole result.
- `complete:true` for desk-domain data means every required **authorized** source in the fixed manifest is reconciled through the advertised fresh barrier, no required home/page failed, and no unresolved ownership, retention or integrity gap exists. It is only complete as of that vector. No claim about later mail or unknown machines. Only a validated manifest of the whole configured fleet permits the additional user-facing “fleet-wide as of snapshot” wording.
- Distinguish `covered`, `cached`, `unavailable`, `reverse_only`, `unsupported`, `history_gap`, `unreadable`, `backpressure`, `handoff_pending`, `stale_generation` and `ambiguous_owner`. Report checkpoint age/last successful contact, observation time and retry action; do not compare remote clocks as proof of freshness. Network failures after a write can be `outcome_unknown`; coverage cannot imply that write failed or succeeded.
- The local `mail_delivery.summary/warning` diagnostic remains topology information, not live proof. Tool rendering must display omissions, not summarize a partial empty waiting result as “nothing needs you.”
- Ship restrictions apply to coverage too: hidden topology, counts, home paths and source names must not leak. Use opaque allowed source handles and an unexplained incomplete/unknown-domain marker where necessary. If the host cannot establish authorized completeness without hidden metadata, return `complete:false`, not an invented all-clear. Precise operator diagnostics stay in bounded internal audit.

## 4. Migration and local-only compatibility

1. Add schema/version negotiation without deleting or rewriting existing `door_cursor`, `door_reader_*`, away or digest rows. Old peers remain explicit `unsupported`/partial. Existing selected-mailbox v2 operations keep their coverage scope and acknowledgment contract.
2. Bootstrap an owner/domain only from explicit unambiguous placement and peer agreement. Preserve existing registry machine UUIDs, mailbox incarnation UUIDs and grant IDs; source identity migration adds aliases rather than renumbering stored mail. No automatic desk movement on upgrade.
3. Opt a door into the new subscription at a recorded local/source barrier. Import its proved per-home acknowledged prefixes and outstanding unread/open facts transactionally via replayable migration state. Never translate a sequence from one mailbox into another's. A remote selected-mailbox cursor migrates only with authenticated evidence from its holder, not the adapter's claimed position. If old copy provenance cannot be proved, replay retained unread/open facts conservatively and report any history gap.
4. Keep a v2 grant's existing reader progress independent from v1 and dashboard progress. Do not initialize it from the dashboard on upgrade. Local v1 cursors retain their existing first-registration rule. Fence the migrated door's old push writer before new subscription serving; use a new subscription incarnation and reissue outstanding refs. After migration, late old-session acks cannot write either migrated cursor.
5. Import one authoritative away period, routing counters, pending digests and per-reader baselines from the agreed owner. Conflicting active local periods require operator reconciliation, never timestamp-winner merging. Do not mark pending digest rows delivered merely because the import completed.
6. A single-machine installation needs no link, transfer or remote grant store. Existing non-opted-in v1 doors, scoped selected-mailbox reads, local sends, dashboard unread state and grant lifecycle stay unchanged. New local desk-domain subscriptions run the same bounded serving/ack logic without RPC; migration replay is permitted, mail loss is not. Downgrade cannot safely reactivate migrated old cursor writers: retain the fence and require explicit compatible recovery.

## 5. Test plan and acceptance evidence

Use isolated registries and homes with fake providers, two-machine **local transport invoking real gateway subprocesses**, plus a real adapter under the door supervisor. Reuse [linked reader fixtures](../../tests/fixtures/door_reader_linked.rs), [linked desk fixtures](../../tests/fixtures/door_desk_linked.rs), [reader-copy fixtures](../../tests/fixtures/door_reader_copies.rs) and [MCP tests](../../tests/mcp_door_test.py). Run each transport case with desk/door roles reversed; do not simulate a reverse route by accidentally installing a second callable link.

| Area | Required assertions |
| --- | --- |
| Source visibility | Uncollected remote notice, open older request, pre-move unread notice, local-only #481 outgoing send, closure and desk copy. Assert the exact missing IDs before ingestion and inclusion after a proved barrier, without duplicate lifecycle effects. Unknown/transitive membership and unreadable home prevent fleet all-clear. |
| Directions/reverse-only | `alpha -> beta`, door on each side, desk on each side, explicit and inferred placement; unsupported legacy peers and inconsistent placement. Reverse-only reader returns partial/unavailable with no local fallback, secret topology leak or presence fork. Source carry can close an ingestion gap only after its barrier arrives. |
| Grant boundary | Observer/Operator/Full Captain and two ship-restricted grants; same application with independent grants, rotated tokens of one grant, two doors, forged origin/provenance and unconfigured peer. Restrict/revoke before admission, during RPC and before page release; resume/continuation cannot use old authority. No grant gains replication or handoff verbs. |
| Restart/reconnect | Kill host, adapter and desk gateway before served commit, after commit/before output, after ack commit/before receipt, and while intake awaits source receipt. Durable refs/cursors recover; lost pages reappear; no dashboard or other-reader cursor moves. |
| Lost/forged acks | Unserved/future/wrong origin, mailbox, kind, reader, grant, subscription and generation; mixed valid/invalid batch changes nothing. Duplicate ref, duplicate batch/idempotency replay, sparse far-future ack and concurrent acks never skip unseen mail. Expired receipt returns unknown/stale honestly. |
| Copies and paging | Same origin via source and desk, overlapping pages, outgoing/incoming correlation, equal IDs with conflicting provenance; duplicate within reader suppressed when proved, different doors still receive. At item/byte/scan/ref limits, empty continuation, stable snapshot under concurrent mail, expiry and reconnect, no gap is skipped or queue silently evicted. |
| Desk transfer | Fault injection at every prepare/copy/seal/certificate/install/activation commit and response boundary. Assert exactly one active owner or a fenced availability gap, byte/manifest verification, preserved unread/open/closure facts, pending digest and acknowledged prefixes. Retry transfer and simultaneous/return moves are idempotent or rejected stale. |
| Cross-desk stale ack | Carry mail and digest at `g`; ack one before fence and leave one in flight. Activate `g+1`, offer the latter with a new ref. Send old refs to **both desks**, including mixed new/old batches and delayed retry receipts: neither new cursor nor new digest state advances. An ack committed before fence is included in transferred progress. |
| Partition/recovery | Disconnect during transfer, old owner unavailable, target restart, old backup restored, stale redirect/activation replay. No timeout takeover or unilateral unfence. Incomplete coverage persists until placement/source reconciliation; reconnect converges without acknowledging missing mail. |
| Away/digest | One authoritative period/budget/build/catch-up across hosts, partial-source digest leaves missing-source baseline untouched, return and move races do not lose held/open facts. Scoped valid/failed sends never end away. Two readers remain independent. |
| Migration/regression | Upgrade seeded local v1/v2 cursors, sparse served refs and pending digests; repeat interrupted import and refuse unsafe downgrade. Legacy local-only behavior and #481 sends remain unchanged; no credentials in DB fixtures, logs or public docs. |

Evidence must record exact commands, source/owner states and assertions. These are **planned tests**, not results delivered by this docs-only change. Captain-run hosted-client/account checks in [the front-door checklist](mcp-front-door.md#10-captain-run-real-client-checklist-q5) remain separate; local transport is not proof of Internet connectivity, full native agent autonomy or production readiness.

## 6. Small implementation PRs

Each PR preserves existing interfaces until an explicit opt-in; partial stages must not advertise fleet subscriptions as complete.

1. **Coverage/identity contract:** versioned result schema, filtered domain manifest and source identity/alias records; keep today's selected-mailbox coverage distinct. Tests: schema validation, privacy restrictions, unknown domain, local/legacy responses, MCP omission rendering.
2. **Source intake and barriers:** bounded snapshot/delta/checkpoint operations on legal collect/push paths, durable copy provenance, outgoing/closure projection, retention/backpressure. Tests: two directions, old unread/open facts, #481 local sends, duplicate imports, restart between intake and receipt, skipped homes and unsupported peers.
3. **Desk-owned pull readers:** delegated open/resume/pull/show/thread/ack, per-door/grant durable refs and source-vector paging. Tests: real subprocess host/desk reads, two grants/doors, revoke races, sparse/lost/forged acks, byte/window limits and cross-machine copy suppression. Reverse-only remains explicitly unavailable.
4. **Ownership generation and handoff:** operator-only prepare/stage/seal/certificate/activate/status, owner fencing, registry-restore guard and verified placement agreement. Tests: every crash boundary, conflicting/return transfers, link loss, unread/open preservation and mail/digest stale refs at both desks. Seed digest-shaped state without enabling a second digest builder.
5. **Away/digest ownership integration:** project held inputs with coverage, transfer pending summaries and budgets, generation-bound digest refs, catch-up exactly once per owner transition. Tests: partial inputs, restart, return/move races and independent reader baselines. Preserve v1 policy until opt-in.
6. **Migration and end-to-end exposure:** explicit cursor import/fence, adapter capability negotiation, aggregate waiting/mail tools with honest rendering, local-only regression and setup/troubleshooting docs. Tests: real supervisor restart/migration/reconnect, reverse-only/unsupported/partition matrix and no silent fallback. Only then update capability claims.

A reverse-reader spool is a **separate optional PR series**, with its own authority refresh and asynchronous custody design; it is not smuggled into reverse-send support. If implementation review finds a PR too broad, split its schema/protocol and behavioral activation, keeping advertised coverage conservative between them.

## 7. Short open questions

1. **Reverse-only delivery:** accept explicit partial/unavailable coverage for this phase (recommended), or prioritize a separately designed asynchronous reverse-reader transport before declaring phase 2.4 complete? No new route is implied by either choice.
2. **Coverage domain UX:** should the first aggregate subscription require explicit selection/confirmation of its configured machine domain (recommended), or use an operator-defined default domain? Unknown/unregistered sources must remain outside any fleet-wide claim either way.
3. **Unrecoverable owner:** is cooperative handoff with no automatic failover acceptable (recommended initial contract)? Forced recovery after permanent desk loss needs a separately authorized fencing/restore procedure, not a reader-level override.
