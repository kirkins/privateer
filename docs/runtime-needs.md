# Runtime captain attention (v1)

`runtime.needs {"address":"captain","limit":40,"after":"<next_after>"}` is the
single local runtime query for captain attention. It is **operator-only**; it does
not make the captain mailbox readable to agents. An operator may invoke it on a
configured peer using the existing peer protocol. It performs no network calls,
acknowledgements, recovery, job reconciliation, migrations, or creation. The direct
CLI opens an existing database read-only (`Store::open_readonly` for consumers).
An absent home fails without creating it; an uninitialized/older schema returns
`available:false`, null origin, no items, and `unknown:["runtime_schema"]` rather
than migrating it or claiming it is quiet. Generic RPC servers/consumers still
own their normal Store startup lifecycle; the query itself never initializes a
Store. It uses a consistent deferred read transaction.

Response: `schema_version:1`, `available`, durable home `origin` UUID, `observed_ms`, `items`,
`next_after` (null at end), and `unknown` evidence categories. Limit is clamped to
1..100, ordered by source and source ID, not by unread order. Cursors are opaque
source keys, not snapshots: refresh from the first page to see earlier arrivals.
A page can be empty with a continuation if stale approval authority was filtered.
IDs are `<origin>:<source>:<source_id>`, stable across refresh and restart (also
when a home is moved). Copying an entire home copies its identity; clones must
not be presented as independent origins. Item IDs are not mutation permission.

Supported typed facts:

* **decision**: any open request/decision addressed to captain. `source_id` and
  `correlation` identify the request; `ship`, optional `task_id`, `created_ms`,
  `question` (max 2000 characters), `choices` (else empty/free text), and `version`
  accompany it. Reply with `mail.send` from captain to that ship, kind reply,
  correlation = source_id and a stable client-generated message ID. Existing
  recipient/open-state checks run inside the write transaction; retries with the
  same message ID/content are idempotent. Acknowledgement never resolves it.
* **approval**: only a request with `request_kind:"production_approval"`, to
  captain, local `task_id`, and payload `{artifact,environment,choices?}`. Sending
  captures the current charter hash; obsolete/revoked authority is omitted.
  `artifact`, `environment` and `version` are the exact grant binding, not parsed
  prose. Approve via existing `approval.grant` with `request_id`,
  `expected_version`, `task_id`, `artifact`, `environment`, `approver:"captain"`,
  and a stable explicit grant `id`. Deny with `approval.deny` and the same fields
  (no grant ID required). The checked response and grant/denial commit together.
  Denial creates no approval. States are `approved` / `denied`, with a correlated
  captain reply recording the outcome. Same grant ID/content or same denial is
  retryable; consumed grants cannot be replayed/reset. Changed binding, stale
  authority, withdrawn/terminal request, different outcome or non-operator caller
  fails. Plain replies/acknowledgements/agent prose NEVER create a grant. Existing
  `approval.consume`/`delivery.check` remain the exact-artifact, exact-environment
  consumption/deployment gate; this query/action does not deploy anything.
* **blocker**: explicit blocked tasks; jobs queued for >=60 seconds on nonterminal
  tasks; recorded orphaned jobs (even on terminal tasks, since payloads may remain).
  Includes typed `evidence`, task/ship, version, and job incarnation where relevant.
  These are last recorded runtime facts, not a live process observation. No generic
  unblock/cancel/"ok" mutation is offered: use existing guarded runtime commands
  after inspecting current evidence. Resolved facts disappear on refresh.

No text/unread heuristics. Unknown categories explicitly include live liveness,
wake backoff/hourly budget, dispatcher, Herdr, attended-mail pause, CI, provider
health, remote homes, and legacy unvalidated approval requests. No CI/service/credential integration is claimed. Missing
external evidence is not reported as healthy. The UI must not silently convert
`unknown` into an all-clear signal. These categories can be added as separately
reviewed authoritative adapters without changing item identity conventions.

## Requests ending without an answer

`mail.withdraw {"actor":"<sender>","id":"<request id>","reason":"..."}`
withdraws only the caller's own open request or decision. Agents cannot supply
another actor. Reason is mandatory, <=2000 bytes. Repeating the same withdrawal
is a no-op; a different reason or another terminal state is refused. A reply is
not invented. The original request retains its history and gains `state`,
`ended_by` (closure notice ID), `ended_ms`, `end_reason`. `mail.requests` accepts
`withdrawn`, `task_done`, `task_cancelled`, `approved`, `denied` alongside existing
filters. Replies continue to use `answered_by`/`answered_ms`.

`task.update` to done/cancelled atomically closes all requests linked to that
local task, including crew questions and earlier owners' linked questions after
custody transfer. This is a runtime consequence of the task's end, not permission
for a new owner to withdraw another sender's question. It changes no custody.
New requests cannot be linked to terminal tasks. Queries also omit legacy open
requests already linked to terminal tasks, without backfilling on read.

Closures are ordinary correlated low-urgency notices carrying reserved payload
`request_end:{version:1,state,reason}`. Local mail.send cannot forge this payload.
The receiver checks original sender/recipient and request kind. Updated peers
apply it transactionally and idempotently via the existing durable FIFO outbox,
flush and retry/backoff. Task closure enqueues without doing network I/O under
the task lock. Old peers accept and preserve the notice but do not automatically
apply its state until upgraded; no new wire command or reply is fabricated.
Task IDs remain local and do not travel. A valid reply already in flight wins
over closure when eventually delivered, so both homes converge to answered;
after observing closure a receiver cannot create a new reply. Both facts remain
in history. Restart/redelivery never reopen a closed request.
