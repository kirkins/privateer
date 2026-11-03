# Scoped v2 pull mail (phase 2.4 Parts A and partial B)

All four operations require `mail:read` and normal current-grant admission.
No arbitrary mailbox address or raw mail RPC is exposed. `ship` is an exact
active home name, local or on a directly reachable linked machine, subject to
the grant's ship restriction. This remains a ship-addressed API, not an aggregate
fleet inbox. Hidden
homes/messages produce the same external `not_found` envelope as missing ones.
Stored sender restrictions apply before paging and thread selection too.

- `inbox {ship, limit?:1..50, continuation?:TOKEN}` returns incoming captain
  mail in stable insertion order. Default page size is 25. Listing is not ack.
- `show {ship,id}` returns an exact ID, not a prefix. Captain incoming and
  outgoing conversation is visible; unrelated mailbox traffic is not.
- `thread {ship,id,limit?:1..50,continuation?:TOKEN}` returns the visible
  captain conversation sharing the selected message's correlation/root ID.
  This is a single correlation thread, not recursive arbitrary mailbox traversal.
- `ack {ship,refs:[REF,...]}` acknowledges up to 64 distinct refs from that
  grant's served incoming mail in that exact home. The entire batch succeeds
  or fails transactionally. A mail ID or claimed sequence is not an ack ref.
- `ack {refs:[REF,...]}` without `ship` acknowledges served grant-owned digest
  refs only. Digest generation still needs `captain:presence` plus `mail:read`;
  digest acknowledgment needs only `mail:read`.

Pages return `{items:[{mail,ref}],continuation,coverage}`; show returns
`{mail,ref,coverage}`. `ref:null` means historical/already acknowledged mail, an already carried copy
in another local home, or outgoing conversation: it cannot be acknowledged again. Ack returns an
`acknowledged` count, never a client-controlled cursor. All operations also
carry normal core-authored request provenance. Retrying a list/show before ack
reuses the same durable ref. Repeated copies with the same mail ID across
allowed active local homes are suppressed within this grant: the first carried
copy owns its ref. A different grant can still carry its own copy. Explicit
show/thread can inspect a copy but do not allocate another incoming ref for it.
At-least-once delivery is intentional; separate grants and v1 doors can each
observe the same message.

## Durable state and bounds

The local home's `metadata.door_reader_mailbox` is a random database-incarnation
ID. A local reader is `(client/grant, exact resource, mailbox)` with its own random
generation and monotonic `through_seq` in registry `door_reader_stream`.
A replacement database gets a new identity; copying the whole database is an
operator migration, not a desk-handoff protocol. Two currently allowed active
homes with the same incarnation fail closed, even when their cursors are zero.
Rows for absent/replaced/restricted identities are purged before window admission
or copy suppression, including their streams and continuations. Reappearance
therefore starts a fresh generation. No dashboard or v1 cursor is
consulted, initialized or advanced. Readers start at zero (available history).

`door_reader_served` binds a random opaque ref to that reader, exact stable mail
ID, sequence and ack flag. There are at most **256 sparse records per grant**,
including acknowledged records behind unread gaps. Show/thread may allocate only
128, reserving inbox headroom so future-message acks cannot strand earlier mail.
When a page or show needs a slot the window cannot give, the reader forgets its
oldest **unacknowledged** served rows first (any mailbox of that grant, never a
row served by the same request). A forgotten ref is stale: an ack naming it
fails, the mail stays unacknowledged, and a fresh list serves it again under a
new ref. A client that never acknowledges, such as an MCP chat client, can
therefore exhaust a mailbox of any length, at least once (#490). Only a window
holding nothing but acknowledged rows behind unread gaps refuses, with the
explicit `reader_window_full` code.
An ack compacts only the contiguous prefix of authorized incoming IDs. Showing
and acknowledging a future message cannot consume an unseen earlier message.
Replayed/stale, invented, wrong-grant and wrong-home refs fail. Registry immediate
transactions serialize competing sessions and recover served refs/cursors after
restart. Cross-home deduplication uses outstanding served IDs and the original
home's immutable mail plus compact cursor as proof, not an unbounded ID/tombstone
ledger. Reads may compact facts **already acknowledged** in another home, but
never acknowledge a newly listed message. Duplicate-only pages still return an
explicit continuation when bounded scanning has more work. Operator removal of
original mail/home history removes that dedup proof; it is not a desk handoff.
Mail remains in its home; immutable IDs/order are the local read source,
not a distributed registry/home atomic transaction or a revocation recall claim.
Window backpressure is the explicit `reader_window_full` refusal; acknowledge or
list earlier inbox mail to proceed.

Continuation tokens are random, durable, bound to reader generation, operation,
thread root and a frozen upper sequence. Internal sequence keysets avoid offset
instability; the public API never trusts a supplied cursor. New arrivals appear
on a fresh list, not in an old continuation. Tokens expire after 24 hours; at most
32 recent tokens per grant survive. An evicted, expired, forged, wrong-reader or
wrong-generation token is the explicit `continuation_invalid` refusal, not an
empty-success page; restart at inbox without a continuation, using its durable
acknowledgment state.
Pages have a 512 KiB mail-content budget as well as the item bound and a bounded
candidate scan. The first item is always admitted even if larger than that budget,
so an oversized historical message cannot produce endless empty continuations.
Transport envelope limits still apply. Ack compaction streams the intervening authorized IDs (including
cross-home copies) without retaining that history in memory.

Digest delivery retains its existing independent `client/UUID` summary cursor.
`door_reader_digests` binds served digest refs to the same grant/resource;
acknowledgment and the digest delivered transition commit together. It does not
ack raw inbox IDs. Old pending summaries remain retrievable, oldest first, after
return or a new away period, so unread durable summaries cannot become orphaned
window entries. Scoped digest IDs use UUID suffixes rather than a pruned row count. See [scoped protocol](door-scoped.md) for digest retention and
existing trusted-plugin limitations.

## Linked reader wire and coverage (partial Part B)

Raw `inbox`, `show`, `thread` and ship-addressed `ack` route to the configured
machine whose cached directory holds that ship. Ambiguous routes fail closed.
A reverse-only link is **not** a reachable reader and is never queued as mail.
No collection, desk move, generic dispatch or captain-authority fallback occurs.
The authenticated core-to-core gateway has a versioned **sibling** command,
`fleet.reader.v1`, with the strict envelope:

```json
{"version":1,"origin":"ORIGIN_CORE_UUID","context":{"version":1,"client":"CLIENT_UUID","resource":"door:NAME","scopes":["mail:read"],"ships":null,"result_id":"RESULT_UUID"},"operation":"inbox","args":{"ship":"SHIP"}}
```

Only `inbox`, `show`, `thread`, and ship-addressed `ack` are accepted. The receiver
revalidates argument shape, mail scope and ship restrictions, uses the same local
reader, and never routes onward. No bearer token travels. `origin` is generated
and persisted by the originating core (`door_reader_identity`), not accepted from
door args. A remote reader's internal client key is `origin-core-UUID/client-UUID`;
the receiving registry supplies the serving machine namespace, exact resource,
mailbox incarnation and random stream generation. A response is
`{version:1,result:RESULT}`. `RESULT.origin` identifies the serving machine UUID,
mailbox and generation. Refs bind the generation in the served row as well as the
client/resource/mailbox; changing a stream generation invalidates its old refs
and continuation tokens. Acks return to the serving machine, where validation
and compaction commit together. Independent grants/origin cores have independent
cursors and refs, recovered after process restart.

Each raw mail result has `coverage.scope:"selected-mailbox"`, a `complete`
boolean, and `machines:[{machine,state,complete,retry}]` for the selected machine
(`here` for local). `other_machines:"not_queried"` explicitly prevents interpreting
one mailbox as fleet-wide coverage:

- `complete`: that selected mailbox was read/acked successfully.
- `unavailable`: no confirmed result; retry is `reconnect_then_list`,
  `reverse_only`, or `invalid_reader_response`. Empty items/null mail are **not**
  proof of an empty inbox. An ack has `acknowledged:0,outcome:"unconfirmed"`;
  a response lost after commit is not proof that the remote did not advance.
- `unavailable` with `retry:"reverse_only"` also carries a closed, value-free
  `coverage.hint`: this machine cannot read the other machine's mailbox directly
  because the link is reverse-only. Sends still deliver on collection. Read an
  already-held local copy or use a door on a machine with a callable link to the
  holder and a valid grant. This is not a transient retry; retrying on this
  machine cannot enable direct reads. There is no reverse-reader queue, relay
  or operator fallback, and local copies do not prove the source mailbox complete.
- `unsupported`: the old gateway explicitly refused the unknown sibling command;
  retry is `upgrade_peer`. There is no downgrade to ordinary operator mail.

A known remote refusal (scope, stale ref, missing mailbox) remains a non-disclosing
request refusal, not a fabricated complete inbox. An unavailable local home or
allowed history needed for local copy deduplication also fails the call.

## Coverage schema `coverage.v1`

Every scoped result that reports mail or waiting coverage carries one
`coverage` object with `schema:"coverage.v1"`, a `scope` and a boolean
`complete`. `complete` is relative to its scope; no scope is fleet-wide, and
it is separate from `continuation`, side effects and transport success.
A page with a continuation can have complete selected-mailbox coverage; that
never means the inbox is drained or every served ref is still retained. Paging
refusals (`continuation_invalid`, `reader_window_full`) remain non-disclosing
error envelopes, not fabricated empty pages or upgraded coverage.

| Scope | Results | Extra fields |
| --- | --- | --- |
| `selected-mailbox` | `inbox`, `show`, `thread`, ship-addressed `ack` | `machines` (one, as above), `other_machines:"not_queried"`, optional closed `hint` for `unavailable`/`reverse_only` |
| `local` | `waiting`; `report.read` (also `remote:"unavailable"`) | `other_machines:"not_queried"`, optional `reasons` when incomplete |
| `operation-only` | digest-only `ack` | `operation:"digest_ack"`, `mailboxes:"not_queried"` |
| `runtime-domain` | opt-in `ships`/`status` (`schema:"runtime.v1"`) | filtered per-machine/ship/facet `sources`, `inventory_complete`, `domain:null`, `state:"unknown_domain"`, `other_machines:"not_queried"`; always incomplete until a runtime domain is configured |
| `desk-domain` | operator-enabled local `desk.subscription.*` / `desk_subscription_*` | `domain`, `manifest_revision`, `max_source_age_ms` (known domains), `owner`, `snapshot`, `sources`, `other_machines`, optional `retry` |

Before this schema, digest acks used scope `desk-digest` and report reads
`local-active-homes`; both now use the scopes above, keeping their other
fields. **This is a client-visible change to the current scoped (v2) output**,
not an opt-in; clients matching those old scope strings must accept the new
versioned values. The adapter host stamps digest-ack coverage itself, so an older desk's
unversioned object is never relayed. Waiting keeps its `mail_delivery`
summary/warning, which stays topology wording, not proof. `captain.waiting`
(and therefore trusted v1 waiting) now gains an additive `coverage.v1` field;
legacy `status` is unchanged. Waiting is incomplete with non-identifying `reasons`
when a visible member home is missing (`missing_home`), the per-home scan
reaches 1000 asks (`request_scan_limit`), or the final list is cut to 100 asks
(`waiting_limit`). At exactly 1000 scanned asks, completeness is conservatively
false because the mail request API supplies no continuation.

`desk-domain` serves the operator-enabled local raw subset of
[door mail coverage](design/door-mail-coverage.md), under the
[approved reduced exposure contract](design/desk-exposure-plan.md). It does not
imply remote-owner support, handoff, migration, aggregate waiting or subscription
presence/digests. A coverage domain is an
operator-configured manifest: source cores, each with the ships whose homes it
holds, a revision, and `max_source_age_ms` for push-only freshness (default
900000, at most one day). The manifest and coverage leave the core filtered by
the admitted grant. Inventory needs `fleet:read`, coverage `mail:read`. Only
sources holding an allowed ship appear, listing only allowed ships; coverage
names sources by opaque handles, never ships or core IDs. A ship-restricted
grant sees `restricted:true` and no revision, since a hidden enrollment would
change it. An unknown domain, a domain whose every source is hidden, and a
grant without the needed scope all return the same `state:"unknown_domain"`, `domain:null`,
`complete:false` object. Desk-domain completeness is derived, never supplied: it
needs an active owner, a snapshot and every visible source `covered` through an
on-demand barrier or a push contact no older than the domain's operator-set
`max_source_age_ms`; a source's advertised bound cannot widen it. Local raw reads
use only dated intake evidence, never trigger source contacts, and disclose
missing/cached/unsupported/backpressured evidence independently of replay selection.

Source identity records give each origin mailbox `(core UUID, mailbox
incarnation UUID)` a stable opaque handle. Alias records map a copy's mail
identity to its root origin as ingestion carries it: never inferred from equal
sequences or bodies, a copy of a copy points at the root, and a copy that
already names another origin (or is itself an origin) is a conflict that
changes nothing and suppresses nothing.

The MCP adapter relays v1 coverage unchanged. When a core omits fields, it
renders `complete:false`, `other_machines:"unknown"` and scope `unreported` as
needed, with a note that the result is not a fleet-wide all-clear; an
unversioned `desk-domain` object is never shown as complete. A missing scope
always forces `complete:false`. When rendering fills missing or malformed fields,
it drops the `schema` label rather than claiming that repaired output validates
as `coverage.v1`. A valid v1 `operation-only` result has no
`other_machines` field and passes through unchanged.

**Linked machines share operator trust.** The machine link authenticates
access to a peer's operator gateway, nothing finer. A linked peer can assert any
reader origin, ack, alias or coverage, as it can already answer for the captain
at that gateway. These records and checks are consistency bookkeeping between
operator-trusted cores, not isolation of mutually untrusted machines.

## Local raw desk subscriptions (explicit opt-in)

See [operator opt-in](design/desk-opt-in.md) and the
[MCP tool contract, recovery and bounds](../plugins/mcp/README.md#operator-enabled-local-raw-desk-subscriptions).
The core advertises only the qualified `desk.subscription-capability.v1`
`local_raw` feature to current admitted `mail:read` readers with a ready enabled
binding. The adapter filters both discovery and guessed calls by that descriptor
and scope; it never caches authority, cursor progress or unattended polling.
All six `desk_subscription_open/resume/pull/show/thread/ack` tools are thin
one-command adapters through the same scoped credential/binding admission and
release. Reads reserve durable state (non-read-only/non-idempotent annotations).

Preserve `desk.subscription.v1` bodies and `coverage.v1` on admitted errors as
well as success. The adapter normalizes only closed errors, handles nested core
refusals as MCP errors, and adds fixed dated-coverage guidance. Missing/malformed
coverage cannot claim complete. `replay_selection.v1` and item lifecycle state
are independent of source completeness and raw ack. History is not “new/unread”;
empty partial pages are not an all-clear. Named-ship visibility remains filtered.

Open/resume recover standalone ack keys and retained snapshot start tokens, not
a client high-water. A page key binds one normalized batch; a distinct partial
batch needs a standalone key. Retry `outcome_unknown` only with **identical
ack_key and refs while retained**, including after adapter/core restart. After
expiry recover pending mail with fresh served refs without claiming the uncertain
old ack failed. No generic `idempotency_key`, automatic retry or second receipt
cache exists. Final core responses are bounded to 192 KiB before commit; the
separate duplicated MCP result is measured against 3 MiB including JSON escaping,
with caller ID/transport framing additional and ingress-bounded.

The selected-mailbox closed reverse-only core hint remains unchanged after desk
success; the adapter adds **“unsupported read direction / incomplete feature”**
as a separate `direction_notice`, preserving older peers' exact hint wording. Local waiting, selected-mailbox/v1/
dashboard/digest progress and other readers remain independent. Owner-local
legacy channels may coexist; source/pending/restored channels remain fenced.
This is a fixed generation-1 local raw milestone, not full roadmap completion.

## Desk-owned presence and summaries

Scoped `away` (status/start/back), `held`, `digest`, and digest-only `ack` resolve
the **existing** captain desk before execution. Explicit `machine captain`
placement wins; otherwise the existing sole collecting-machine rule applies.
Ambiguous or reverse-only owners refuse the request, never elect a local desk.
A remote owner is called through `fleet.delegated.v1`, allowlisted command
`desk.v1`, with `{origin:ORIGIN_CORE_UUID,operation,request:ARGS}` inside its args.
The existing admitted context travels separately in the delegated envelope.
The receiver revalidates operation, arguments, scopes, and liaison visibility
before presence writes, and executes locally only (no recursive routing).

The desk owns the away row, held classification, digest build/baseline and digest
ack transition. Adapter hosts create **no local away state** on link failure,
old-peer refusal or a successful remote call. Presence/digest/ack transport loss
is `outcome_unknown`, not permission to make a local fallback transition. The
response `desk:{machine,state:"complete"}` confirms execution at the selected
owner; it is not aggregate remote mailbox coverage. Sensitive away routing lists
remain filtered exactly as for local scoped requests.

Remote digest stream keys include origin-core UUID/client UUID, while stored
provenance retains the **original** client/grant UUID. Digest refs survive origin
supervisor restart and are validated at that same desk under `mail:read`;
generation still requires both `captain:presence` and `mail:read`. A summary ack
does not consume raw inbox mail. Local digest streams retain their Part A keys;
this is not a migration of existing pending summaries across a desk move.

**Remaining work:** digest/held collection reads the desk's local homes, including
already-collected mail; this increment does not aggregate an uncollected mailbox
on another machine into a digest. Cross-home deduplication is still within one
serving machine, not across origin machines; the same durable ID read from two
machines can still be carried twice by one grant. Fleet-wide aggregation, remote
per-v1-door push subscriptions and reverse-only readers are not delivered. Part C
still needs desk moves, generation handoff and migration preserving unread facts.
This partial increment does not claim the full section 5 subscription contract,
full MCP parity or native agent autonomy.
