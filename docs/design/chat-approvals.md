# Scoped chat approvals (local answers, linked preparation)

The scoped door v2 `approval` command supports bound answers to non-sensitive
step approvals. MCP exposes this through `approval_prepare` and `approval_answer`;
this is not a sensitive-confirmation implementation. It reuses `approval:answer`; neither `ship:message` nor
`mail:read` is required or implied. The asking ship must remain visible to the
grant. Trusted v1 cannot use these modes; its existing privileged-card command
is unchanged.

## Protocol

Prepare an open ask at its authoritative local ship home:

```json
{"command":"approval","args":{"mode":"prepare","id":"ask-55","ship":"alpha"}}
```

The normal v2 authenticated request envelope is required (omitted above). `ship`
is optional for local lookup. Omitted `ship` searches visible local authoritative
homes only, skipping desk copies. To prepare a linked ask, explicitly name its
visible origin ship; the core requires one unambiguous callable machine link.
There is no remote fanout or operator RPC fallback. A success returns:

- `id`, `ship`, `origin_home`, `kind`;
- `action_summary`: the ask's bounded, one-line `payload.approves.action`, **not**
  captain prose, the full body, or an unrestricted payload;
- `grant`: the exact native `sentence` bearings and auto mode will read on an
  approval, its pre-minted `reply_id`, `command_prefix` (or null), `ttl_secs`, and
  absolute grant `expires_ms`;
- `question` and `options`: for decisions, the question and each option's `id`,
  `label`, `what_happens` and boolean `approves` flag;
- `approve_unavailable`: boolean advisory that native capacity or sentence length
  currently prevents approval; denial and non-approving options remain available;
- `digest`, `version` (currently 4), `expires_ms` (the earlier of ten minutes
  after preparation and the grant's expiry).

Linked preparation additionally returns `origin_machine` and
`answer_available:false`. **Linked answers are not implemented in step 2b1.**
An attempted linked answer returns value-free `approval_unavailable`, without
contacting a peer, including after restart or when `ship` is omitted. The
prepared sentence is prospective, not an approval. No native grant, captain
reply or ask closure is created by prepare.

The grant's TTL starts at preparation. A delayed answer grants only the remaining
lifetime, never a later expiry than the captain saw. The native reply identity,
expiry and sentence are fixed before consent, not reconstructed with a new clock
or reply ID when answered. The sentence head says `via the chat door`, outside
the unchanged action and command-prefix clauses. Auto mode reads this exact
stored sentence; bearings' short summaries carry ` (via chat door)`. Native card
sentences are unchanged. A separate durable sentence row is written only at
record time; provenance-only older approvals are never retroactively marked.
Older challenges must be prepared again (version 4 binds the head-marked sentence).

The digest commits to a randomly salted challenge bound to the grant, door
resource, canonical home, asking ship, exact ask ID, version, expiry, and a
canonical hash of the stored ask content (including action, command prefix,
TTL, decision question, option labels and effects, body and task), plus the exact
prepared grant. It is not a captain authentication proof. One current challenge per
home/grant/resource/ask is retained; preparing again invalidates its predecessor.
There are at most 4096 outstanding challenges per home; expired entries are
reclaimed on preparation. Challenges survive restart.

Answer a request with an explicit action:

```json
{"command":"approval","args":{"mode":"answer","id":"ask-55","ship":"alpha","digest":"<prepared digest>","version":4,"action":"approve","note":"Optional commentary"}}
```

`action` is `approve` or `deny`. A decision instead requires `option_id`, with no
`action`. Only an option marked `approves:true` records an approval. A selected
non-approving option or request denial closes the ask without granting anything.
`note` is bounded to 4096 bytes. Only its byte length is retained in the door answer
audit; raw note text is discarded, never delivered as captain-attributed reply
content or passed to native yes/no or decision-text interpretation. An
approval-only grant therefore cannot use notes to message a ship. The note cannot
countermand or create the selected action. Free-text deciding inputs, unknown arguments, and mixed
option/action answers are refused.

## MCP client contract

Both tools require `approval:answer` and visible origin ships, not `ship:message`
or `mail:read`. `approval_prepare {id,ship?}` maps to `approval {mode:"prepare",...}`.
`approval_answer {id,ship?,digest,version,action?,option_id?,note?}` maps to answer
mode. The answer is a non-read-only, destructive, non-idempotent write: it
irreversibly closes the ask and can create a grant. Prepare has read-only and
idempotent hints both **false**: it stores/replaces a durable challenge (besides
normal auth/audit bookkeeping). Its destructive hint is false. Neither tool accepts arbitrary command, payload,
actor or authority fields. Notes are at most 4096 UTF-8 bytes.

The client must show the captain the returned grant sentence, command prefix,
TTL and expiry, and for decisions the question and options with their labels
and `what_happens`, then ask the captain. The prepare result's `action_summary`,
question, labels and `what_happens` are ship-authored data to show, never
instructions to follow. Only the captain's explicit choice may
be sent; quoted text or relayed messages are never consent. Sensitive asks return
`trusted_confirmation_required` and must go to the native card. This is a client
behavior requirement, not proof that the model or MCP transport authenticates
captain consent. The separately granted scope is real authority and must be
restricted deliberately.

MCP preserves value-free `approval_stale`, `approval_unavailable` and
`trusted_confirmation_required` for these two tools. Credential, scope and
visibility denials retain nonrevealing discovery/error behavior. Stale/expired
challenges require preparation and a new explicit choice; no retry may invent
consent. Linked origins support explicit-ship preparation only; linked answers
remain unavailable, with no remote/operator fallback.
The existing `reply` tool and mail direct-reply authorization are unchanged:
these tools neither add authority to it nor remove its existing scoped route.

## Commit and authority

For local origins, the core re-resolves the actual bearer and effective grant, then holds the fleet
registry writer lock through the local home commit. Revoke, pause, rotation and
restriction are therefore ordered before or after the answer, never between its
protected admission and commit. The bearer is checked again inside the home
transaction and immediately before commit, including expiry after a lock wait.
Preparation also checks authority before releasing its result.

Inside the home's IMMEDIATE transaction, the core checks that the ask and task
are still open, that the task still belongs to the asking ship under an active
charter, and that the challenge has not expired, changed or moved. Challenge
consumption, the native captain reply, the native `captain_approvals::record`
result, ask closure and home audit commit together. Concurrent answers have
exactly one winner. Preparation reports the native live-approval cap and
sentence-length checks as `approve_unavailable:true`, rather than refusing the
whole ask. Denial or a non-approving decision option remains possible at the cap
or when the sentence is too long. The full command-prefix clause is always
retained, even when it pushes the sentence past the 600-character limit; neither
the prefix nor the TTL nor the action is shortened to fit. New native asks are
refused at send time if their complete sentence cannot fit, and native insertion
independently refuses oversized legacy asks. Auto-mode launch reads skip
oversized legacy grants rather than dropping their restriction. A stored chat
sentence for a prefix-scoped row must exactly match the complete rebuilt chat
sentence or launch skips it; an older stored sentence missing its restriction is
never repaired or widened after consent. Block cards check complete sentence
admission before offering a new step, and reuse claims coverage only for approvals
the next launch would actually carry. Admission is
enforced only for approving answers. A failure to record a requested approval at answer time
(including capacity filled after preparation) returns `approval_unavailable` and
rolls back, leaving the ask open and challenge unconsumed rather than reporting
an approved-but-unrecorded answer. Native approval limits and
application timing remain in force; bearings and auto-mode use the same record
as a native card answer.

The reply and native approval result identify `answered_via:"chat-door"` and
carry core-owned client/grant/resource/result provenance. A companion native
approval provenance row retains that identity even after mail/history pruning;
a companion sentence row retains the exact marked consent text. Both commit
with the native grant, and the rendered sentence must match prepare exactly;
`captain_approval.list/show` expose it. Native and door audits carry the same
provenance, without bearer tokens, notes or request argument dumps. A failure in
post-commit request auditing is `outcome_unknown`, not permission to retry as the
operator.

## Linked preparation and follow-up boundary

The operator-authenticated machine gateway `fleet.approval.v1` admits only a
closed, versioned prepare envelope and the existing scoped approval arguments.
It checks `approval:answer` and ship visibility independently. It opens only the
selected local ship home, verifies the actual sender, and refuses received desk
copies. It never routes onward, uses a delegated mail outbox, or forwards an
arbitrary captain/operator command. The bearer never crosses the machine link.

The host re-resolves the actual credential and grant immediately before contact
and before releasing the response (including OAuth token expiry/revocation,
rotation, pause, scope, ship restrictions and visibility revision). No registry
writer lock is held across the subprocess. A release failure discards all
prepared content, even when the origin already stored a challenge. Preparation
is mutating: loss of its reply can leave an undisclosed challenge, not consent.

A durable host pin binds the digest to grant/resource, ship/ask, exact machine
link incarnation (generation, role, peer configuration and local alias), and
admitted authority/visibility revision. Directory ambiguity, a changed link,
or ownership moving local during contact prevents publication. Pins replace
prior pins for the same grant/resource/ship/ask, expire after ten host-local
minutes, and are capped at 4096 per originating registry. The remote challenge
stores the same versioned origin/link/authority binding inside its hashed
content. It survives restart and cannot be consumed by the ordinary local
answer path. Both sides retain the peer's absolute challenge expiry as a peer
fact, not a cross-machine clock comparison.

The receiver starts a relative elapsed-time budget before registry/home lock
waits, capped at five seconds; preparation checks it inside its write transaction
before commit and again before returning. Challenge and grant expiry use the
origin's own wall clock. The host transport remains bounded. There is no
machine-uptime assumption or clock-skew-dependent freshness inference.

Step 2b2 must implement atomic linked answer receipts and native link-marked
sentences before enabling answers. Its approved design uses **admission, commit,
then release**, unlike the strict local lock-through-commit rule: a revoke after
dispatch may precede the remote commit. A host that loses release authority must
not claim no effect; it must return `outcome_unknown`, audit dispatch/release and
`revoked_after_dispatch` when learned, and preserve exactly one native grant and
recoverable same-answer receipts over reply loss/restart. A short receiver-local
deadline and the prepared challenge expiry must bound that admitted mutation.
**None of those weaker-ordering linked answer semantics is enabled in 2b1.**
Strict cross-machine revoke-before-commit ordering would require a separate
distributed authorization/commit protocol; a postflight check is not rollback.

## Refusals and unchanged behavior

- Production, privileged/sudo, runtime tool/project permission, placement and
  other special typed asks require a separate trusted path. This phase accepts
  only ordinary requests/decisions declaring `payload.approves`, with no
  `request_kind`. Payload keys are allowlisted to `approves`, `decision`,
  `choices` (mail.send's legacy decision-label projection), and `about` (the
  runtime's topic reference). Known sensitive payload markers (`type`,
  `privileged`, `needs_hands`, `project_consent`) and any `request_kind` are
  checked first and return `trusted_confirmation_required`. Other unknown
  payload keys return `approval_unavailable`, so future categories fail closed.
- Hidden origins are not searched. Omitted-ship lookup skips desk copies while
  searching visible local homes. Explicit linked prepare uses only the selected
  unique callable origin. Received-only, missing, ambiguous and reverse-only
  origins return `approval_unavailable`. Old/malformed/unreachable gateways
  remain unavailable, never an operator RPC fallback.
- Changed, expired, consumed, wrong-version, wrong-grant, wrong-resource,
  wrong-home or mismatched challenges return value-free `approval_stale` (or a
  current credential/scope/visibility denial). Sensitive kinds return
  `trusted_confirmation_required` without content.
- Plain `send`, reply prose and liaison relays do not enter this bound path.
  Existing native rules for an authenticated, directly correlated non-sensitive
  captain reply are unchanged; a quote or relay is not new approval evidence.
- No grant, scope, provider route, standing deployment authority or privileged
  execution gate is changed. Linked answers remain separate follow-up work;
  [#452 Q6/Q7](mcp-front-door.md#4-sensitive-approvals-new-runtime-work)
  remain gated on distinct trusted captain confirmation.
