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

Observation has a cooperative 250ms deadline, installed **before metadata/schema
reads**. A SQLite progress hook stops at 2,000 callbacks (every 1,000 VM
instructions). Independently, a shared 4,096-checkpoint budget covers source
phases, page items and every authoritative task/ancestor read, checking the same
deadline. This also bounds repeated small SQL statements whose individual VM
runs never reach a progress callback; the callback count alone is not a strict
2,000,000-instruction bound across such statements. Cyclic ancestry is rejected
with a per-authority-read visited set, without repairing the graph. Authority is
read through the same effective-charter traversal used by production grants.
Lock waits are capped at 25ms; scoped hooks/timeout are removed/restored on every
exit, leaving normal commands' write-wait policy unchanged. Source JSON/text is
preflighted with a 64KiB per-field bound before materialization; task references
and other small authority fields have a 160-byte bound.
An exhausted budget, lock failure, oversized or invalid source returns
`available:false`, no partial items/cursor, and an explicit unknown category (or
refuses the observation). It MUST NOT be rendered as quiet/healthy. The result
LIMIT bounds the page, not the scan; a large home may be unavailable even with
`limit:1`. These are cooperative bounds, not a hard real-time guarantee: SQLite
callbacks cannot preempt blocked OS/filesystem I/O, database open, or an individual
native operation. No background recovery or new live adapters are performed.

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
  Explicit terminal/cancelled authority is also non-actionable. Corrupt charter
  JSON, an invalid active flag, missing ancestry or a cycle instead makes the
  observation unavailable, never an available empty/partial page.
  `artifact`, `environment` and `version` are the exact grant binding, not parsed
  prose. Approve via existing `approval.grant` with `request_id`,
  `expected_version`, `task_id`, `artifact`, `environment`, `approver:"captain"`,
  and a stable explicit grant `id`. Deny with `approval.deny` and the same fields
  (no grant ID required). If either correlation field is supplied, **both must
  be nonempty strings**; null, numeric, malformed or incomplete pairs fail before
  any mutation. Correlation is refused on consumption/unsupported commands.
  Legacy grants with BOTH fields omitted remain supported. The checked response
  and grant/denial commit together under the authority write lock.
  Denial creates no approval. States are `approved` / `denied`, with a correlated
  captain reply recording the outcome. Same grant ID/content or same denial is
  retryable; consumed grants cannot be replayed/reset. Changed binding, stale
  authority, withdrawn/terminal request, different outcome or non-operator caller
  fails. Plain replies/acknowledgements/agent prose NEVER create a grant. Existing
  `approval.consume`/`delivery.check` remain the exact-artifact, exact-environment
  consumption/deployment gate; this query/action does not deploy anything.
* **blocker**: jobs queued for >=60 seconds on nonterminal
  tasks; recorded orphaned jobs (even on terminal tasks, since payloads may remain).
  Includes typed `evidence`, task/ship, version, and job incarnation where relevant.
  These are last recorded runtime facts, not a live process observation. No generic
  unblock/cancel/"ok" mutation is offered: use existing guarded runtime commands
  after inspecting current evidence. Resolved facts disappear on refresh.
  Generic `task.status:"blocked"` is NOT a captain-action fact: intentional
  dependency scheduling stays visible in Fleet, not Needs you. No separately
  reviewed typed task captain-attention evidence exists yet; task captain
  actionability is explicitly unknown. The query does not infer it from titles,
  status, mail prose, or unread counts.

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

Runtime closure and approval-answer IDs use fresh UUIDs checked for local
collision under the same write lock, not predictable hashes of public request
IDs. Ordinary client IDs (including old `end-<hash>` and
`approval-answer-<hash>` forms) remain legitimate history, never overwritten or
accepted as proof of an outcome. The request's committed `ended_by`/`answered_by`
link and recorded state/content are authoritative for retries, including legacy
closed requests; no old history is renamed. Outbox retries resend the SAME
committed notice ID, not a newly allocated one. Allocation retries are bounded.

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
