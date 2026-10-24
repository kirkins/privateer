# Scoped v2 local pull mail (phase 2.4 Part A)

All four operations require `mail:read` and normal current-grant admission.
No arbitrary mailbox address or raw mail RPC is exposed. `ship` is an exact
**local** active home name, subject to the grant's ship restriction. Hidden
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
`{mail,ref,coverage}`. `ref:null` means historical/already acknowledged mail or
outgoing conversation: it cannot be acknowledged again. Ack returns an
`acknowledged` count, never a client-controlled cursor. All operations also
carry normal core-authored request provenance. Retrying a list/show before ack
reuses the same durable ref, so the adapter deduplicates by mailbox and mail ID.
At-least-once delivery is intentional; separate grants and v1 doors can each
observe the same message.

## Durable state and bounds

The local home's `metadata.door_reader_mailbox` is a random database-incarnation
ID. A reader is `(client/grant, exact resource, mailbox)` with its own random
generation and monotonic `through_seq` in registry `door_reader_stream`.
A replacement database gets a new identity; copying the whole database is an
operator migration, not a desk-handoff protocol. No dashboard or v1 cursor is
consulted, initialized or advanced. Readers start at zero (available history).

`door_reader_served` binds a random opaque ref to that reader, exact stable mail
ID, sequence and ack flag. There are at most **256 sparse records per grant**,
including acknowledged records behind unread gaps. Show/thread may allocate only
128, reserving inbox headroom so future-message acks cannot strand earlier mail.
An ack compacts only the contiguous prefix of authorized incoming IDs. Showing
and acknowledging a future message cannot consume an unseen earlier message.
Replayed/stale, invented, wrong-grant and wrong-home refs fail. Registry immediate
transactions serialize competing sessions and recover served refs/cursors after
restart. Mail remains in its home; immutable IDs/order are the local read source,
not a distributed registry/home atomic transaction or a revocation recall claim.
Backpressure is a normal non-disclosing refusal; ack earlier inbox mail to proceed.

Continuation tokens are random, durable, bound to reader generation, operation,
thread root and a frozen upper sequence. Internal sequence keysets avoid offset
instability; the public API never trusts a supplied cursor. New arrivals appear
on a fresh list, not in an old continuation. Tokens expire after 24 hours; at most
32 recent tokens per grant survive. Token eviction/expiry is a refusal, not an
empty-success page; restart at inbox using its durable acknowledgment state.
Pages have a 512 KiB mail-content budget as well as the item bound.

Digest delivery retains its existing independent `client/UUID` summary cursor.
`door_reader_digests` binds served digest refs to the same grant/resource;
acknowledgment and the digest delivered transition commit together. It does not
ack raw inbox IDs. See [scoped protocol](door-scoped.md) for digest retention and
existing trusted-plugin limitations.

## Coverage and deferred work

Every reader result states `coverage.scope:"local-mailbox",complete:true` for
that one local home and separately
`remote:{complete:false,state:"unavailable",retry:"remote_subscriptions_not_implemented"}`.
This operation does not query a linked machine, collect mail or move the captain
desk. Mail previously collected into this home is readable like other local mail.
An unavailable local home fails the call; it is not silently skipped. Reverse-only
and reachable links alike have no reader subscription in this increment.

Part B still needs the authenticated core-to-core subscription protocol, desk-owned
away/digest classification, origin mailboxes and explicit link failure/retry
coverage. Part C needs desk moves and generation handoff. No claim of cross-machine
subscription completeness, full MCP parity or native agent autonomy is made.
