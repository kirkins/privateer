# Scoped chat approval core (local origins)

The scoped door v2 `approval` command supports bound answers to non-sensitive
step approvals. This is core support, not an MCP tool or a sensitive-confirmation
implementation. It reuses `approval:answer`; neither `ship:message` nor
`mail:read` is required or implied. The asking ship must remain visible to the
grant. Trusted v1 cannot use these modes; its existing privileged-card command
is unchanged.

## Protocol

Prepare an open ask at its authoritative local ship home:

```json
{"command":"approval","args":{"mode":"prepare","id":"ask-55","ship":"alpha"}}
```

The normal v2 authenticated request envelope is required (omitted above). `ship`
is optional; lookup searches visible local ship homes only. A success returns:

- `id`, `ship`, `origin_home`, `kind`;
- `action_summary`: the ask's bounded, one-line `payload.approves.action`, **not**
  captain prose, the full body, or an unrestricted payload;
- `grant`: the exact native `sentence` bearings and auto mode will read on an
  approval, its pre-minted `reply_id`, `command_prefix` (or null), `ttl_secs`, and
  absolute grant `expires_ms`;
- `question` and `options`: for decisions, the question and each option's `id`,
  `label`, `what_happens` and boolean `approves` flag;
- `digest`, `version` (currently 2), `expires_ms` (the earlier of ten minutes
  after preparation and the grant's expiry).

The grant's TTL starts at preparation. A delayed answer grants only the remaining
lifetime, never a later expiry than the captain saw. The native reply identity,
expiry and sentence are fixed before consent, not reconstructed with a new clock
or reply ID when answered. Older version-1 challenges must be prepared again.

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
{"command":"approval","args":{"mode":"answer","id":"ask-55","ship":"alpha","digest":"<prepared digest>","version":2,"action":"approve","note":"Optional commentary"}}
```

`action` is `approve` or `deny`. A decision instead requires `option_id`, with no
`action`. Only an option marked `approves:true` records an approval. A selected
non-approving option or request denial closes the ask without granting anything.
`note` is bounded to 4096 bytes. Only its digest is retained in the door answer
audit; raw note text is discarded, never delivered as captain-attributed reply
content or passed to native yes/no or decision-text interpretation. An
approval-only grant therefore cannot use notes to message a ship. The note cannot
countermand or create the selected action. Free-text deciding inputs, unknown arguments, and mixed
option/action answers are refused.

## Commit and authority

The core re-resolves the actual bearer and effective grant, then holds the fleet
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
exactly one winner. A failure to record a requested approval rolls back rather
than reporting an approved-but-unrecorded answer. Native approval limits and
application timing remain in force; bearings and auto-mode use the same record
as a native card answer.

The reply and native approval result identify `answered_via:"chat-door"` and
carry core-owned client/grant/resource/result provenance. A companion native
approval provenance row retains that identity even after mail/history pruning;
`captain_approval.list/show` expose it. Native and door audits carry the same
provenance, without bearer tokens, notes or request argument dumps. A failure in
post-commit request auditing is `outcome_unknown`, not permission to retry as the
operator.

## Refusals and unchanged behavior

- Production, privileged/sudo, runtime tool/project permission, placement and
  other special typed asks require a separate trusted path. This phase accepts
  only ordinary requests/decisions declaring `payload.approves`, with no
  `request_kind`. Payload keys are allowlisted to `approves`, `decision`,
  `choices` (mail.send's legacy decision-label projection), and `about` (the
  runtime's topic reference). Any other payload key returns
  `approval_unavailable`, so future categories fail closed.
- Hidden origins are not searched. Lookup skips desk copies while searching
  other visible homes for the authoritative ask. Non-local origins or an ask
  found only as a desk copy return `approval_unavailable`; this phase performs
  no remote routing or SSH fallback.
- Changed, expired, consumed, wrong-version, wrong-grant, wrong-resource,
  wrong-home or mismatched challenges return value-free `approval_stale` (or a
  current credential/scope/visibility denial). Sensitive kinds return
  `trusted_confirmation_required` without content.
- Plain `send`, reply prose and liaison relays do not enter this bound path.
  Existing native rules for an authenticated, directly correlated non-sensitive
  captain reply are unchanged; a quote or relay is not new approval evidence.
- No grant, scope, provider route, standing deployment authority or privileged
  execution gate is changed. MCP tools and linked-origin routing are separate
  follow-up work; [#452 Q6/Q7](mcp-front-door.md#4-sensitive-approvals-new-runtime-work)
  remain gated on distinct trusted captain confirmation.
