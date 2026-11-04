# Standing deployment grants

Status: **captain-approved policy; staged implementation**.
The captain approved standing grants including production, with the production
notice-and-wait gate below. This note does not authorize any deployment or
change an existing executable gate. Slice 2a is dormant lifecycle only.

## Problem and current boundaries

A prose instruction such as “alpha may upload green main builds to beta” is
neither a machine-checkable scope nor an audit trail tying an upload to its
artifact. The proposed alternative is captain-created standing policy, evaluated
and consumed separately for every exact deployment attempt.

Relevant existing mechanisms:

- `src/grants.rs`: `grant.put` grants ship-management actions in the fleet
  registry. Only the operator writes, pauses, resumes or revokes; agents list
  their own grants. Uses are audited in the ship home, with optional captain
  notices. This is lifecycle precedent, not deployment authority. Its registry
  writes and home audit writes are not one transaction; do not copy that split
  for deployment consumption.
- `src/core.rs`: `approval.grant` creates task/artifact/environment/charter-bound
  rows in the home's `approvals` table. Consumption is single-use under the home
  write lock. `delivery.check` requires current/tested head equality and consumes
  an exact approval for production, but explicitly labels its checks as
  **caller-supplied**. Those booleans cannot prove eligibility for a standing
  deployment grant. `deploy_production` is listed among `core.rs` actions but
  is not enforced today: `delivery.check` authorizes only `merge_green`.
  Deployment action enforcement must be added, not assumed.
- `src/captain_approvals.rs`: `payload.approves` records a captain's own reply to
  an open, owned task request, or an approving decision option. It supplies
  time-limited harness step permission, not production authority. Production and
  privileged requests are explicitly excluded. A relay cannot create one.
- `src/releases/requests.rs`: upgrades atomically consume exact captain approval
  with durable intent, then let the operator dispatcher start the payload. This
  is a useful custody pattern, but upgrades have their own runtime-derived
  environment and must retain their existing approval requirements.
- The generated AGENTS rule in `src/cli.rs` currently says one exact-artifact
  approval per deployment and explicitly forbids standing substitutes. The root
  `AGENTS.md` and [conversation proposal](conversation-and-activation.md) describe
  the intended alternative. **Do not resolve this discrepancy by silently
  relaxing executable gates or generated instructions.**

## Proposed authority contract

A captain may give one named ship permission to deploy to **one exact environment**
under one structured artifact rule. A successful match replaces the need for a
new captain decision, never the need for an exact-artifact deployment record.
The grant does not widen the ship's charter, change a provider route, allow
privileged commands, approve an upgrade, or approve unrelated releases.

The captain approved this alternative, including production. Existing approval
behavior and generated text remain unchanged until a qualified consumption path
lands. A nonmatch is a denial, not a prompt that silently grants more
scope. The ship may separately request an exact captain approval.

Production grants require an explicit `production: true` field. An environment's
production classification comes from captain configuration, not request text;
classification mismatch refuses. There is no implicit wildcard, environment
prefix matching, prose parser, or “all green repositories” rule.

Roles are workflow controls under **one OS account, not a security boundary**.
Grants bound deployments only if upload credentials are reachable exclusively
through the captain-owned adapter; a ship with direct credentials can bypass
both these grants and today's exact approvals. Terminal consent does not defend
against a malicious same-user process that drops markers or fabricates a PTY.

Production requires a captain notice **then a wait window** specified in the
grant (for example, 30 minutes). The notice must name the exact artifact,
environment, grant revision, deployment id and earliest claim time, and explain
how to pause or revoke from the phone. Start the window only after confirmed
notice delivery; an unavailable notice path refuses rather than bypassing the
gate. Production grants require `notify:true`, `wait_window_ms` from **5 minutes
to 24 hours**, expiry at most **90 days from creation**, and `max_uses` from
**1 to 100**. Non-production expiry, when set, is at most **365 days from
creation**. Replacement does not reset the creation time. An unacknowledged notice is not affirmative
approval: the captain deliberately chose that no response allows deployment
after the window. Pause or revoke during the window must stop it: recheck all
authority at the end of the window under the claim transaction. Do not claim or
launch early. A grant revision change denies the queued attempt, rather than
silently restarting or shortening its window.

## Data model (home-local and transactional)

Use a separate module and tables in the grantee's authoritative home database,
not an extension of fleet ship-management `grant.put`. One database owns grant,
consumption, audit and pending operation custody. Do not copy live authority to
a desk, accept a peer projection as authority, or transfer it with task custody.
Cross-home deployment authority is deferred.

`standing_deploy_grants`:

- `id`, `revision` (monotonic), `grantee` (one ship hosted by this home, checked
  at put and again at consumption);
- `environment` (one exact identifier), `production`;
- structured `artifact_rule`: `kind: green_branch_commit`, canonical
  `repository_id`, full `branch_ref`, nonempty `required_checks`, and a
  captain-owned verifier configuration identity/digest;
- captain-owned deployment adapter identity/digest for this environment;
- `notify`, optional note and `expires_ms`, optional positive `max_uses` and
  `wait_window_ms` (notice, expiry, max uses and window mandatory for production);
- `created_ms`, `updated_ms`, `paused_ms`, irreversible `revoked_ms`;
- creator recorded as the authenticated operator, not a supplied `approver` name.

`deployments` is a durable per-attempt ledger:

- runtime-minted deployment id and requester-provided idempotency key, unique per
  requesting ship; owner, task id, effective charter hash;
- exact environment, repository identity, full commit object id, artifact digest
  and immutable artifact locator;
- authority source (`exact_approval` or `standing_grant`), approval id or grant
  id/revision plus a snapshot/digest of the matched rule;
- verifier identity/config digest, immutable CI run/check identities and
  conclusions, evidence digest, verification time;
- adapter config digest, runtime-owned request thread, pending/claimed/terminal
  state, claim identity, attempt outcome and timestamps.

A grant is reusable policy; each **use** is consumed by one deployment row. It
is not globally depleted after the first use. A derived exact authorization is
created and consumed in the same transaction as that deployment's intent; it
must never appear as an unused `approvals` row that generic `approval.consume`,
`delivery.check` or `upgrade.start` could redeem. A ledger row with the immutable
exact authorization is sufficient; avoid weakening assumptions of the existing
`approvals` table merely to share a table name.

Changes increment the revision. Preserve old rule snapshots in deployment rows
and audit. `max_uses` counts attempts across **all revisions of the id**;
replacement cannot refresh the budget. Updating an active grant follows
`grant.put` semantics; paused grants
must be resumed first, and a revoked id can never be reused or revived. All
fields are typed, size-bounded and reject unknown keys; no executable strings
inside an artifact rule.

## Proposed protocol and captain-only operations

Lifecycle names are slice 2a; deployment commands remain future work:

- `deploy_grant.put`: captain terminal only, explicit consent; create or replace
  an active grant with `expected_revision:0` on creation and the current revision
  on replacement. Every replacement requires consent, not only widening; show
  a field-by-field diff from the current revision before confirming.
- `deploy_grant.pause|resume|revoke`: operator only, expected revision; resume
  never clears revocation. Lifecycle changes and their audit share a transaction.
  Pause/revoke may originate on any authenticated captain surface (including the
  phone); agents and relays never acquire that authority. Slice 2a supplies the
  operator protocol, not new phone/dashboard integrations. A separately tested
  authenticated **phone pause and revoke path is a prerequisite slice before
  any production consumption**, not follow-up polish.
- `deploy_grant.list|show`: operator sees all, an agent sees only its own grants.
  List/show are bounded/paginated and expose effective lifecycle state. Show
  uses the same unknown-id error for nonexistent ids and another ship's ids.
- `deployment.start`: agent-owned open task, explicit grant id and exact
  artifact request; returns a durable operation id, **not a reusable bearer
  permission to run arbitrary shell**.
- `deployment.show|list`: owner-scoped status/evidence, bounded pagination.

Operator means the existing trusted captain control path. Create or widen only
from the captain terminal with consent, as `door client issue` does, never from
the phone door, dashboard bridge, RPC or `captain.relay`. The CLI is a separate
entry point; ordinary dispatch refuses `deploy_grant.put`, even for an operator. No captain name in
JSON, ordinary `mail.send`, `payload.approves`, prose note, peer request or crew
report creates or changes deployment authority. Do not expose these writes by
adding them to a general MCP dispatch or by treating MCP enrollment as consent;
separately confirmed delegated captain support is outside this first version.
Register all commands in their `ACCEPTED` lists and relevant authority checks.

Example rule: ship `alpha`, environment `beta`, repository `gamma`,
`refs/heads/main`, required check `test`. These are exact identifiers, not
illustrative wildcard syntax. Actual verifier endpoints and executable paths
belong in private captain configuration, not shipped examples or fixtures.

## How the artifact is proved

The requester supplies a full commit id and immutable artifact locator/digest,
not `checks_passed: true`, a movable tag, a short SHA, or a PR head as a proxy for
its later merge commit. The runtime invokes a captain-configured verifier with
bounded explicit argv and JSON stdin/stdout. The requester cannot select its
executable, endpoint, credentials, environment overrides or check names. The
configuration digest covers resolved executable bytes (not just a path or argv);
re-hash those bytes before each invocation. Require the verifier's output to
repeat the exact requested repository, commit, branch and check set and compare
all of them, rather than trusting a bare success.

The verifier must establish all of the following from the configured forge and
artifact service, using bounded reads and refusing incomplete responses:

1. Canonical repository identity matches the rule, including forge identity;
   an owner/repository spelling alone is not globally unique.
2. The requested commit is exactly the configured branch tip at verification
   time (v1 deliberately does not accept arbitrary historical ancestry).
3. Every named required CI check is complete and successful on **that commit**,
   from a pinned app, workflow path and `push` event. Refuse commit statuses
   outright and duplicate same-name checks, even if one is green. A repository
   token can self-post green statuses; `merge_green` can also alter workflow
   files. Pinning a workflow path alone therefore does not make its output
   trustworthy: the app/attestation verifier must validate the trusted build
   policy, not merely accept whatever workflow the ship merged. Missing,
   pending, failed, cancelled, skipped, untrusted or ambiguous checks refuse.
4. Establish one of two explicit provenance modes:
   - `ci_attestation`: a trusted CI build attestation tied to canonical repository,
     workflow and exact commit, covering the artifact's digest.
   - `runtime_build`: a runtime-owned adapter freshly checks out the verified
     commit, builds and uploads it. The ship supplies no artifact; needed for
     locally built TestFlight artifacts. Record the resulting artifact digest
     before upload. Build tools/recipe must be captain-owned and cannot leak
     upload credentials to ship-controlled build scripts.
   A green commit plus an unrelated local archive proves neither mode.
5. In attestation mode, at verification copy the bytes into runtime-owned
   content-addressed storage while hashing and compare with the attested digest.
   Upload only that copy, not the original locator. The adapter must re-hash what
   it streams if the storage cannot enforce immutability. A path/digest recorded
   before a later read does not prevent an artifact swap.

Existing PR-view adapters may supply implementation patterns, but a PR head's
checks are not evidence about a resulting main commit. A new commit/artifact
verification protocol and tests are necessary. Do not infer successful CI from
an empty list or a self-reported summary. A verifier failure is fail-closed and
cannot fall back to caller assertions.

Evidence reads happen outside SQLite locks. After verification, acquire the
home write transaction and recheck grant revision/lifecycle/expiry, verifier and
adapter config identities, task ownership/open state, charter and environment.
Discard evidence if any binding changed. Before claiming pending work the
operator dispatcher verifies fresh evidence again; if the branch or check state
changed, do not spawn. Configured time bounds prevent indefinitely reusing an
old successful read. Use injected clocks in tests; elapsed wall time is not
proof of correctness. Evidence is at most **five minutes old** at claim (and
must be refreshed after a production wait); future-dated evidence refuses.
Strict branch-tip matching deliberately refuses often on busy repositories.

The branch/check service can change after its last read; there is no atomic
transaction across a forge and an upload service. Define the guarantee as green
branch tip **at the last verification**, pin the artifact immutably, and record
that evidence. Do not promise that the branch stays unchanged during upload.
If the artifact service cannot prove build provenance, the rule is unsupported
and the ship must use an exact captain approval instead.

## Consumption, process custody and audit

1. Validate the request, authenticate the requesting ship and check its owned
   open task and deployment charter action (`deploy_nonproduction` for non-production,
   `deploy_production` for production). Add explicit enforcement in this path:
   today's `delivery.check` does not provide it. The standing grant supplies
   captain consent, not that action. Neither deployment action belongs to the
   default charter; the captain grants each explicitly per ship.
   Check resource admission independently; no grant disables existing limits.
2. Verify artifact evidence as above, without starting the uploader.
3. Under the home write transaction, recheck all authority bindings. Atomically
   insert the immutable exact authorization/deployment row, pending intent,
   `deploy_grant.used` audit and captain-notice outbox item (required for
   production). Enforce `max_uses` transactionally, counting attempts, not only
   successful uploads. Roll back
   everything if any durable write fails. Never return success first and log
   the use later.
4. The operator dispatcher claims pending intent once. Production stays pending
   until its confirmed notice delivery plus the grant's wait window. At the end
   of that window, under the claim, recheck grant revision, pause/revocation/
   expiry, task/charter authority, config identities and fresh artifact evidence.
   Also check a fleet-wide captain emergency pause at consumption and claim;
   refuse if its authoritative state cannot be read (a cached false is not
   authority). A stale/revoked intent becomes denied
   without spawning. Claim and its audit commit before process launch.
5. Run only the captain-configured environment adapter, with immutable verified
   artifact arguments and runtime-owned environment. Require the upload adapter
   to use the deployment id as its idempotency key when the service supports it.
   Record real subprocess identity/custody, bounded logs and one terminal result.
   An audit row proves authorization/intent, **not successful upload**.

Concurrent identical idempotency keys return the same operation/status, not a
second launch; the same key with changed inputs refuses. Different keys denote
distinct deployment attempts and consume distinct uses, even for the same
artifact. A failed launch or upload does not refund an authorization or delete
the attempt. A retry requires a new id/key and a fresh match.

On restart, pending intents may be revalidated; a claimed intent must not spawn
again merely because its wrapper disappeared. Recover through payload identity
and remote idempotency/status, or mark outcome unknown and require inspection.
Do not claim exactly-once external execution when the remote service lacks an
idempotency/status API. Preserve ambiguous attempts in the ledger.

Audit lifecycle writes, grant use, claim, refusal of queued work, completion and
unknown outcomes. Include exact artifact/environment and grant revision, but
never credentials or unbounded remote output. Notices are durable,
retryable and deduplicated by deployment id; a notice delivery failure neither
loses the use record nor creates a second deployment. For production, failure
also prevents starting the wait window and hence prevents claim. Use a bounded outbox with
backpressure, not unbounded best-effort spawning. Routine history pruning must
not erase the authoritative grant snapshots/deployment ledger; retention/export
policy needs explicit operator administration.

## Pause, revocation and races

Pause/revoke commits serialize with consumption and claim in the same database.
After pause/revoke returns, no new authorization or pending claim succeeds.
Revision changes invalidate previously checked evidence and unclaimed intents.
The captain can resume paused policy; revoked policy stays dead.

A claim that committed before pause/revoke may already have crossed the process
boundary. Revocation stops future claims, not time travel. Return/list those
in-flight operation ids for inspection. Cancellation must target a confirmed
payload identity and record uncertainty; killing a wrapper is never evidence
that an upload stopped. Do not silently requeue an in-flight attempt.

On `ship.move`, grant rows and history travel as dormant data with the home, not
as transferable deployment authority. Before consumption is enabled, bind grants
to a captain-configured installation identity and refuse them after a move until
captain re-consent pins destination verifier/adapter/config and emergency-pause
authority. Slice 2a has no consumption to transfer; do not change move code there.

## Scope and reviewable delivery slices

This is larger than adding a permissive branch to `delivery.check`. Proposed
split, each independently reviewable and fail-closed:

1. **This design and captain policy decision.** No Rust/generator changes.
2a. **Dormant lifecycle.** Home-local tables, captain-only lifecycle commands,
   revisioning, transactional audit and ACCEPTED entries only. Store and validate
   production notice/window/expiry/max-use fields; there is no consumption path.
2b. **Isolated consumption.** `deployment.start`, verifier subprocess protocol,
   immutable ledger, claim and dispatcher, with fake adapters only in tests.
   Independently safe: refuse without a captain-configured verifier and adapter,
   bundle neither, reject **all `production:true` consumption**, enforce charter
   actions including `deploy_production`, and copy verified artifact bytes to
   runtime storage. Include emergency pause, races and restart custody tests.
3. **Trusted artifact integrations.** Add the real attestation verifier and
   runtime-owned fresh-checkout build-and-upload adapter. A separate phone
   pause/revoke slice must land before production consumption. Production
   consumption comes after 2b, only with that phone path, the notice/wait gate
   and all production constraints.
   Any real configuration or pilot requires separate captain authorization.

Never ship enabled consumption backed by caller-supplied green booleans. Updating the
generated production rule belongs only with the approved, qualified authority
path, not with a dormant schema or this note. Existing exact approvals,
`delivery.check`, privileged commands and upgrade gates remain unchanged.

The later generated rule should say: production requires captain authorization
for the exact artifact and environment, either a per-deployment approval or a
matching captain-created standing deployment grant, with one durable consumed
authorization and artifact record per attempt. A prose standing instruction or
harness step approval is not a deployment grant.

## Slice 2b-i: dormant verifier and configuration contract

This separately reviewed prerequisite does **not** register `deployment.start`,
consume a grant, create an approval, or invoke an upload adapter. Its library
helper proves evidence and holds an artifact copy; that result is not deployment
authority. Resource admission, the ledger and the transactional claim belong to
2b-ii. Both slices bundle no verifier or adapter.

Captain configuration is file-based, never supplied by a deployment request:

- `$XDG_CONFIG_HOME/privateer/deployment-installation.json` (or the standard
  user config fallback): private JSON `{ "id": "alpha" }`, explicitly provisioned
  by the captain with a distinct id for each installation, not copied with a
  ship home, automatically minted or repaired when missing.
- `<home>/deployment/<environment>.json`: private typed JSON with
  `schema_version:1`, `installation_id`, the canonical absolute `home`, exact
  `environment`, `production`, `trusted_build_policy`, and `verifier` / `adapter`.
  Each tool has `id`, absolute resolved `executable`, fixed `args`, explicit
  absolute supporting `files`, and optional private `config`. No credentials,
  endpoint, executable or real environment is supplied by the repository.

The environment configuration must match both installation id and canonical
home. Moving the home or changing the captain's installation file refuses until
captain reconfiguration and re-consent. Each tool binding digest covers this
context, build-policy identity, the complete typed tool configuration, executable
bytes, supporting-file bytes and optional configuration bytes. File arguments
must be declared supporting files. Shebang executables refuse: explicitly pin
both the resolved interpreter executable and its script instead. Use a
self-contained executable or explicitly pin every mutable supporting input; the runtime does not discover an interpreter's
import/dependency tree. Neither arbitrary interpreter code nor same-account
malicious file changes are a new security boundary. Require root/current-owned,
non-group/other-writable ancestors. The sole shared-directory exception is a
root-owned sticky directory (such as a system temporary directory); below it,
every ancestor must be current-owned and not group/other writable. Copy the
executable, supporting inputs and configuration into a fresh private tree,
preserving layout and rewriting pinned file arguments, then compare its binding
before execution. Run only this verified copy, not the original hashed path.
Rehash both the copy and original configuration after verification; a changed
binding invalidates the result. Inspecting
binding digests is a captain-only library operation, not a new RPC command.

The v1 verifier gets one bounded JSON stdin request with `schema_version:1`, the
full `artifact_rule`, captain-configured `trusted_build_policy`, full commit id
and requested artifact SHA-256. It gets a cleared environment, a fresh empty
working directory, fixed argv, umask `077`, inherited descriptors closed at exec,
and, when configured, `PRIVATEER_VERIFIER_CONFIG` pointing at the private copy.
It must emit exactly one bounded JSON line, then exit successfully. Stderr is
bounded and withheld. Timeout and group cleanup reserve the unreaped leader's
identity; no detached helpers are permitted. Cleanup of a process group is not
proof that arbitrary escaped processes stopped.

The response repeats repository, branch, exact commit, branch tip and artifact
SHA-256. It contains uniquely identified check runs (not commit statuses), each
repeating its name, commit, pinned app/workflow/event, completed status and success
conclusion; missing, duplicate or additional checks refuse. Its attestation
repeats the repository, commit, digest, app/workflow and configured build-policy
identity, with immutable attestation identity. The configured verifier is trusted
to validate the service's attestation and build policy; matching JSON is not a
cryptographic attestation implementation. `runtime_build` and all production
verification are deliberately unsupported in this slice.

Only regular local artifact files are accepted in v1. Bounded bytes are copied
while hashing into a private runtime directory, digest checked, then retained
read-only under a content-addressed name. An existing copy is rehashed rather
than trusted by its filename; no source locator is handed to a future adapter.
The future claim/adapter must rehash the copy it streams, since same-account
processes can change permissions or bytes. Evidence is timestamped by the runtime,
not by the verifier. The helper requires an owned open `task_id`, the explicit `deploy_nonproduction`
charter action, and remaining uses across all grant revisions before launch and
again before returning. It rechecks config, grant lifecycle/revision and
hosted grantee, but **only 2b-ii's writer transaction can consume
that evidence or authorize a claim**. Unreferenced content-addressed copies may
remain after a later refusal; retention/GC must never delete a ledger-owned copy.

## Slice 2b-ii-a: dormant transactional use ledger

The reviewed split keeps consumption APIs internal and **registers no deployment
command or dispatcher**. A sealed, non-deserializable proof can only be created
by the verifier; callers cannot submit a saved JSON result as authorization.
Inside one immediate home write transaction, the ledger rechecks the exact grant
snapshot, owned open task and charter digests, remaining cross-revision budget,
installation/home/tool configuration bindings and runtime evidence age (zero to
five minutes inclusive, never future-dated). It then inserts an append-only exact
use/authorization, a separate pending intent, and `deploy_grant.used` audit.
Failure of any write rolls back all three. These rows do not use `approvals`.

The captain must provision a private, installation-wide
`$XDG_CONFIG_HOME/privateer/deployment-pause.json` beside the installation file
(with the same standard config fallback). Its strict JSON is
`{"installation_id":"alpha","paused":false}`. Every new consumption reads this
authoritative file under the transaction; missing, unreadable, unsafe, malformed,
paused or mismatched installation state refuses. It is never created or repaired
by the runtime. A later claim must read it again, not use a cached false. This
file is the isolated installation's emergency gate, not the still-required
production phone pause/revoke implementation.

Idempotency keys are scoped to the authenticated grantee. Their canonical input
digest binds grant id, task, environment, commit and artifact SHA-256, not the
original source locator or a refreshed verification timestamp. The transaction
returns only the existing attempt/status for identical input, even if no further
uses remain; changed input refuses. Neither a new-use response nor a replay
includes an authorization payload. Stored authorization is audit history, not
permission to launch. Distinct keys consume distinct attempts. Repeated internal
calls still require a sealed proof and owner visibility. The future
entry point must resolve retries before expensive verification/admission, without
requiring an additional unused grant use to inspect an already recorded attempt.

At most 20 outstanding intents (`pending`, `claimed` or `started`) per home may
be reserved; claiming work cannot bypass this backpressure. Uses are immutable across
reopen, policy replacement and failed/unknown attempt states; no automatic refund
or history-prune path exists. The immutable record includes the exact policy,
artifact custody, evidence, configuration/installation binding and runtime-owned
request thread. This slice has no process launch, notice delivery, real adapter,
claim or restart dispatcher. Slice 2b-ii-b must add admission before reservation,
fresh claim-time verification tied to the **already consumed** use (not an extra
budget debit), authoritative pause at claim and ambiguous process custody before
registering `deployment.start`. Pending rows in this slice cannot execute.

## Slice 2b-ii-b1: dormant claim and fail-closed reconciliation

This approved further split still registers **no deployment commands** and adds
no admission bridge, activation hook, adapter supervisor or deployment execution.
It fixes the 2b-ii-a review findings: status-only replay, common verifier/adapter
private-copy preparation with explicit tool/digest arguments, source validation
before copy-target creation, and a bounded descriptor-close fallback (refuse
above 65,536 descriptors when the native close-range operation is unavailable).

An internal operator-only claim rereads the immutable use and freshly runs the
verifier against its retained artifact, never against caller retry arguments.
The sealed recorded-use type permits checking the already consumed attempt:
count all revisions and require `used <= max_uses`, rather than charging another
use or requiring one spare use. The stored grant snapshot/revision and original
task/charter, installation, configuration, artifact and commit bindings must all
match. Changed branch/check evidence refuses even when the original evidence was
green; original evidence is never treated as fresh permission.

After external reads, the claim's home writer transaction independently rechecks
all current authority, evidence age, the retained-file identity/metadata seal
(described below) and the authoritative emergency pause (missing/unreadable/unsafe/malformed refuses). Only `pending`
can transition to `claimed`. Claim and its audit, including refreshed evidence
identities and timestamp, commit together. Only that new committed transition
returns a non-deserializable claim token with a runtime-minted claim id and the
dispatcher's process identity. A competing claim or replay returns status with
no token; a historical `claimed` status is not permission to launch. Refusal
atomically records `denied` with a bounded generic reason, not raw verifier
stderr. Failed durable writes roll back the transition and audit together. No
use is refunded, deleted, or refreshed.

Dormant operator reconciliation examines at most 20 in-flight intents per pass.
A live recorded dispatcher is left alone. An absent, malformed or unverifiable
dispatcher identity makes `claimed`/`started` become `unknown` with an audit,
retaining the original claim identity, immutable use and artifact evidence.
Pending work remains pending. This is **not proof a payload stopped**: nothing
is killed, marked successful, refunded or replayed; unknown work needs manual
payload/remote-outcome inspection. The real payload identity/supervisor boundary
belongs to the next slice and must not infer completion from wrapper exit.

## Slice 2b-ii-b2a: sealed claim and off-lock retained-artifact hash

This review-fix-only slice still registers **no deployment commands** and adds
no admission, adapter execution or activation hook. Claim fields are private to
the claim module, with read-only accessors; only `claim_with` constructs a token.
The dormant executor transition atomically moves `claimed` to `started` only
when both the current state and exact `claim_identity` match, with its audit in
the same transaction. A stale token, repeated start or failed audit cannot
advance the intent. This transition alone launches nothing and proves no payload.

Verification (including claim reverification) refuses to run within a home
transaction. It hashes retained bytes before acquiring the home writer lock,
checks digest and size against custody, and retains an open file plus its
size/device/inode/mtime/ctime/mode/owner/link metadata. Keeping the inode open
prevents deletion/recreation from reusing its identity. Under the lifecycle
writer lock, recheck performs only file identity/metadata comparisons for the
artifact, including reopening the private custody path to ensure it still names
that inode. It does **not** rehash up to 512 MiB while blocking lifecycle writers.
The subsequent b2b review fix applies the same held-inode seal to installation
and environment configuration, verifier/adapter executables, supporting files
and private tool configuration. All hashes run before the transaction; under the
lock, original paths, ancestor permissions and inode/size/mtime/ctime/mode/owner/
link metadata are rechecked without rehashing tool bytes. The fail-closed bounded
pause reread stays in the transaction. A deterministic hash-boundary regression
acquires a second home writer immediately at **every** input hash, covering
consumption and claim.

This metadata seal is not hard same-account isolation or upload permission: the
later uploader must hash the actual stream against the consumed artifact digest.
Changed bytes/metadata or replacement files refuse; arbitrary JSON cannot supply
a sealed artifact proof. Evidence age is still measured from the verifier reply,
not extended by an off-lock hash.

The approved remaining split is 2b-ii-b2b, a dormant narrow jobs-owned native
host-admission bridge with tests **before** any use reservation, then 2b-ii-b2c,
the bounded pinned-adapter supervisor with durable payload identity and the
operator activation dispatcher. Only the complete reviewed safe path may
register start/show/list and enable nonproduction. Do not repurpose ordinary
`job.start`, weaken its action requirements, or change resume paths. The later
executor must require a fresh claim token, rehash what it streams and never spawn
merely because a status replay says `claimed`. Ambiguous handoffs stay unknown,
never auto-replayed or inferred stopped from wrapper exit. Production remains
refused; nonproduction expiry remains optional by the captain's recorded decision.

## Slice 2b-ii-b2b: dormant jobs-owned admission bridge

The bridge lives in `jobs::deployment_admission`, alongside the native host
reservation implementation. It is internal and dormant: **no command is
registered, no grant use is consumed, and no adapter or process is launched**.
The private claim injection seam is now test-only; production `claim()` is the
only token entry point. `mark_started` consumes its claim token by value.

An operator must hold a current open task whose owner is a hosted local ship,
with the dedicated `deploy_nonproduction` charter action. Admission refuses an
existing home transaction: the eventual dispatcher must reserve capacity before
entering grant consumption. It requires explicit validated CPU, memory and
incremental disk estimates and a 1–3600 second requested bound, further capped
by the current charter's `job_timeout_secs` (default 900 seconds; zero refuses).
The native shared host
reservation path applies fleet stow, resource floors, queue fairness, concurrent
heavy limits and charter limits. This slice is fail-fast, not a capacity wait
that holds deployment authority; refusal creates no queue entry or grant use.
Ordinary `job.start` action restrictions and all resume paths remain unchanged.

A successful reservation records a fresh deployment-prefixed id/incarnation,
local task/ship/charter, dispatcher process identity, heavy demand, disk accounting
and timeout in the host ledger. A nonserializable process-local token is returned
only after the home authority lock and host reservation both commit. This is not
a cross-database atomic transaction: a failed home commit explicitly compensates
only the exact unlaunched reservation, while a crash retains the durable hold.
There is deliberately no Drop-based release. Explicit pre-launch compensation
compares the entire original reservation, refusing changed custody/state/payload.
No token can be reconstructed from a reservation status or historical JSON.

Restart/open/read never releases capacity just because the dispatcher is gone.
The captain-only manual `resource.recover` path remains available, with the
pre-handoff recovery qualification below; missing payload identity requires an
explicit force after inspection, not a claim that wrapper death proves payload
absence. No automatic deployment replay
or inference about a remote outcome is added. Later b2c must consume admission
and claim tokens in a durable handoff, supervise the real payload identity,
recheck authority and pause, and retain ambiguous capacity/custody before any
nonproduction activation. Production remains refused.

Metadata seals are not same-account isolation. A same-size in-place rewrite
restoring mtime is detected by ctime when the filesystem distinguishes it;
coarse ctime granularity can miss same-tick rewrites. The later executor must
still hash its private prepared tools and the actual artifact upload stream.
Tests use fixed resource observations through a private observation seam, real
SQLite writer races and reopen/recovery, never a real deployment target.

## Slice 2b-ii-b2c-a: review fixes before an executor

This review-fix-only slice still registers **no deployment commands** and adds
no adapter, activation hook or deployment execution. The remaining approved
split is b2c-b (dormant bounded adapter supervisor and durable admission/claim
handoff, fake adapters only), then b2c-c (activation/restart dispatcher and
start/show/list, nonproduction only after the complete safe path is reviewed).
Production stays refused throughout.

`resource.recover` now handles deployment-prefixed reservations separately. An
operator may explicitly release this home's unchanged `deployment-reserved`
pre-handoff hold after the start grace period and manual inspection with
`force:true`, even while its admitting process lives. That process is not yet
a payload supervisor and can outlive a dropped token or failed compensation.
Foreign-home holds, changed state/custody, any payload/escaped identity and
associated job records refuse; force never overrides those checks. No process
is killed and no remote outcome is inferred. The response explicitly says
`payload_absence_proven:false`. Exact-record compare-and-delete serializes with
a future handoff; a winning recovery invalidates any still-held token. The
future executor MUST compare the original reservation before handing off and
never launch from a hold recovery already removed.

The host ledger owns the hold and the recovery audit: deletion and a bounded
`resource_recovery_audit` record commit in **one host transaction**, including
the full pre-handoff reservation, operator provenance and inspection override.
Audit/delete failure retains the hold; there is no success-then-best-effort-log
gap or cross-database atomicity claim. No grant use is refunded or attempt
replayed. Pending or ambiguous deployment intent custody is unchanged.

Admission's private test seam now substitutes only fixed resource observations;
live code and tests share every native reserve argument, including floors,
charter limits and queue fairness. Requested timeout is capped by the current
charter. Tool executability and the shebang prohibition are checked on the held
inode that supplied the executable hash, not on an independent earlier path
open. The verified-private-copy execution requirement remains unchanged.

## Slice 2b-ii-b2c-b1: dormant consuming custody handoff

The approved b2c-b split delivers the two b2c-a review nits first, then the
bounded adapter supervisor as b2c-b2. This slice adds **no adapter execution,
command registration, activation or restart wiring**. Production and
nonproduction deployment entry points remain unavailable.

`resource.recover` routes by the reservation's `deployment-` **state prefix**
alone. The deployment recovery path independently requires a deployment-prefixed
job id, canonical key and unchanged pre-handoff custody. A malformed
ordinary-id/deployment-state row must refuse there, never fall through to the
ordinary job recovery path. Legacy ordinary-state jobs retain ordinary recovery
even when their historical caller-chosen id has a deployment prefix.

`Admitted::handoff(self)` consumes the process-local admission token and
compare-and-swaps the **entire original host reservation** under the host writer
lock to `deployment-handoff`. Only a committed transition yields a new private,
nonserializable `Handoff` token. Recovery and handoff therefore serialize: if
recovery removes the hold first, no handoff token can be returned; if handoff
wins, pre-handoff recovery refuses. A changed record or failed write returns no
handoff token and never releases capacity. The b2c-b2a refinement below returns
unchanged admission custody only on a failed BEGIN before any write. Dropping
either token retains its hold.
No generic spawn closure or reservation-to-token reconstruction is exposed.

This transition records host capacity custody only: it neither consumes a grant
nor authorizes upload, changes deployment intent state, or proves any payload
exists. It is not a cross-database transaction. A crash after the host commit
but before receiving the token leaves an ambiguous handoff for later inspection,
never automatic replay or pre-handoff compensation. The subsequent supervisor
must make its sole spawn path consume `Handoff`, recompare its exact durable
record and bind a fresh sealed claim before giving an adapter authority.

## Slice 2b-ii-b2c-b2a: contract and pre-launch custody refinements

This reviewable slice records the supervisor contract and addresses PR 691's
remaining local nits. It adds no spawn method, adapter execution, command
registration, activation dispatcher or restart wiring. The consuming spawn,
cgroup supervisor, claim bindings and six fake-adapter fixtures are deferred to
b2c-b2b. Both production and nonproduction activation remain unavailable.

`Admitted::handoff` now returns a typed error with the unchanged admission token
when `BEGIN IMMEDIATE` fails before any write. The caller can explicitly retry
handoff or compensate using `release_unlaunched`; dropping the error/token keeps
capacity reserved. This token is not a handoff or upload authority. Recovery can
still invalidate it, and every later handoff still compares the full original
record. Once the writer lock is acquired, any failed CAS, update or commit
returns no reusable token; rollback is attempted and custody remains subject to
inspection. No grant use is refunded, and ambiguous handoffs are never replayed.

Stale post-handoff deployment reservations (including unknown future deployment
states) now report `outcome:unknown`, `inspection_required:true` and `recover:null`.
They never suggest the pre-handoff-only `resource.recover force:true` path.
Observation retains the record and capacity. Ordinary reservations and unchanged
pre-handoff deployment holds retain their existing recovery suggestions.

### b2c-b2b-i: PR 694 refinements before a child gate

The approved three-way split keeps this slice to the updated contract below and
the dashboard inspection-only rendering regression. Slice b2c-b2b-ii will add the
Linux qualification/child-gate module with real-kernel probe and refusal tests;
b2c-b2b-iii will add the consuming spawn, exact claim/hold bindings, bounded
supervisor and six fake adapters. All three remain dormant: no dispatcher,
command registration, restart hook or activation. No containment implementation
or real-kernel qualification is claimed by this contract/UI slice.

### b2c-b2b-ii: dormant Linux qualification and authority-free gate

This slice implements the qualification primitive, not the adapter supervisor.
It accepts only an explicitly trusted isolated delegation, pins the directory
without following symlinks, checks its cgroup2 mount identity and `nsdelegate`,
creates and holds a private empty boundary, verifies finite `pids.max`, and
clones a disposable authority-free child into that boundary with a pidfd and
new cgroup namespace. The child uses only allocation-free libc/syscalls, a
fresh private cwd, umask 077, closed inherited descriptors, empty capabilities
and no-new-privileges. It verifies its namespace root and that its namespace
differs from the parent's, opens the ancestor migration file itself, reports
the result and remains gated until killed. The parent verifies membership,
exercises `cgroup.kill` on the live gated child, proves emptiness and bounds
pidfd rescue/reaping by `CLOCK_BOOTTIME`. No adapter is executed; the explicit
cleared exec environment and held-inode executable policy remain obligations
of iii's sole consuming spawn, not capabilities of this authority-free probe.

An inconclusive cleanup returns bounded refusal plus retained cgroup/process
custody; it never removes that boundary. A successful qualification returns
an empty held boundary, not a launch token or persistent permission. Dropping
it does not discharge custody or remove the boundary. There is no caller,
durable ledger recording, activation or automatic restart path in this slice;
iii must bind and record these facts before any authority release and repeat
the probe against the exact held launch boundary. Ordinary CI asserts refusal
on an unqualified directory; a positive test requires the explicitly supplied
`PRIVATEER_TEST_CGROUP_DELEGATION` isolated real-kernel delegation and fails
rather than substitutes fake files if it cannot qualify. No real-kernel escape
containment result is claimed when that opt-in is absent.

### b2c-b2b-iii-a: private authority-free exec gate and PR 705 fixes

The approved follow-up split keeps iii-a below roughly 800 production lines:
fix the two PR 705 probe findings and add a private, authority-free gated exec
primitive on the held `Qualified` boundary. No caller outside the Linux module
can release this gate. Its opt-in tests execute only a native fake fixture;
there is no adapter entry point, claim binding, deployment metadata, artifact
stream, command registration or activation in this slice. iii-b will add the
sole consuming `Handoff` spawn, exact fresh `Claim` and durable cross-ledger
custody binding, sealed adapter integration, bounded streaming supervisor and
six fake adapter fixtures. Neither slice enables deployments.

A missing ancestor `cgroup.procs` must never qualify: report the child's open
errno separately and require open success before accepting **write** `ENOENT`.
Every post-clone path attempts both `cgroup.kill` and pidfd SIGKILL before
propagating a clock/observation error. A failed cleanup clock read permits a
second BOOTTIME read solely to bound rescue by the separate cleanup window;
it must never reuse a potentially distant runtime deadline. If the clock
remains unreadable, only a nonblocking reap is permitted (never an unbounded
wait). Unresolved reaping/emptiness retains custody. A successful reap does not
convert a clock failure into qualification.

The private gate repeats qualification on the exact held boundary and repeats
the child's root, namespace, credential and child-open migration checks at the
actual exec gate. Before release it verifies process identity and membership.
A failed check or dropped gate kills via both cgroup and pidfd with bounded
cleanup. Inconclusive cleanup retains the boundary and process identity for
inspection. The child has no authority while gated. It uses an empty environment
except a fixed protocol version, a fresh private cwd, umask 077, null standard
streams, empty capabilities and no-new-privileges. Only its explicit gate/ack
pipes and held native executable survive setup; all close at exec. Execution
uses `execveat(AT_EMPTY_PATH)` on the held inode, never a pathname or interpreter.
The private primitive does not itself establish the executable's sealed digest;
that is iii-b's mandatory pinned-adapter check before it may create any gate.

Gate acknowledgement is not durable deployment permission. In iii-b only the
consuming spawn may release it, after recording exact claim/hold/cgroup/process
custody. A failed or ambiguous acknowledgement never permits replay. The
primitive records no deployment outcome or capacity release; `Qualified` and
all inconclusive cleanup retain local custody. Positive tests require the
explicit isolated real-kernel delegation, otherwise they assert refusal.

### b2c-b2b child-gate contract (supervisor not implemented here)

Prepare and verify the private pinned adapter before giving it deployment
metadata or artifact bytes. The adapter inherits the verifier's launch contract:
clear the environment, set only fixed protocol variables, use a fresh private
working directory, set umask 077, and close inherited descriptors at exec
(including home/host-ledger connections and retained artifact descriptors).
Execute only a verified private copy made from the held executable inode, never
a reopened configured path. Pin and verify all interpreter/script/config inputs
before launch; no shebang or ambient PATH resolution may substitute another
executable. No credentials or deployment metadata belong in argv or ambient env.

The sole spawn method consumes `Handoff` and a fresh, nonserializable `Claim`.
It must compare the entire durable host hold again and bind its canonical key,
incarnation, claim identity and immutable deployment use, checking the same home,
ship, task and admitted timeout. A historical JSON status cannot recreate either
token. Start a gated child with no deployment authority, commit its real process
identity and exact claim/hold binding before releasing authority bytes, and leave
the handoff ambiguous if either ledger write or the gate acknowledgement fails.
Home and host commits are not atomic: any partial commit retains capacity and
requires inspection, not replay. No spawn closure may bypass these checks.

Hash the actual artifact stream delivered to the adapter and hash its observed
stdout/stderr streams incrementally. Bound protocol metadata, artifact bytes,
combined stdout/stderr, retained diagnostics and per-iteration IO work; a child
that writes continuously must not starve deadline checks. Exceeding any IO or
protocol bound kills the attempt cgroup and records the outcome **locally** as
failed (or unknown if cleanup cannot prove emptiness), never success. This says
nothing about whether a remote deployment completed. Use nonblocking IO
and the admitted hard deadline measured on Linux `CLOCK_BOOTTIME`, never
`Store::now_ms` wall time. Suspend **counts** toward both runtime and the separate
bounded cleanup deadline; `std::time::Instant`/`CLOCK_MONOTONIC` alone is not
sufficient because it stops during suspend on Linux. Clock changes must neither
extend nor shorten runtime. No unbounded wait/reap or external `ps` is permitted
on either path. Protocol/stream mismatch or partial delivery never implies a
successful remote deployment.

**Positive containment choice:** Linux cgroup v2, a dedicated per-attempt cgroup
with kernel-enforced membership inherited across fork, double-fork, setsid and
reparenting. The concrete qualifying mechanism is a trusted, pinned cgroup2
mount with `nsdelegate`, and `clone3(CLONE_INTO_CGROUP | CLONE_NEWCGROUP)` using
the held attempt-directory fd. The child is born in that cgroup, with a new
cgroup namespace rooted there, **before** executing the adapter. The kernel floor
is Linux 5.16 (open-time namespace/credential permission checks for
CVE-2021-4197); `cgroup.kill` needs 5.14 and `CLONE_INTO_CGROUP` needs 5.7.
A version string is not a substitute for probing the required operations.
This implementation conservatively refuses older kernels, including possible
stable backports, rather than infer that the security fix is present.
An unprivileged launcher also needs an appropriately qualified user namespace
for `CLONE_NEWCGROUP`; this slice does not implement a user-namespace launcher
and refuses when the current credentials cannot perform the exact clone, not
a weaker join-after-fork path.
The adapter must have no privilege to change the enclosing cgroup mount or
namespace boundary: drop capabilities and set no-new-privileges before exec.
No inherited namespace/mount/control fds may cross exec.

Qualification runs an authority-free disposable child under the **same** launch
credentials, namespaces and fd policy as the adapter. Verify the mount's cgroup2
identity and `nsdelegate` option, the child's actual cgroup namespace root and
membership, and kernel rejection (`ENOENT`) of a probe moving that child through
an outside-namespace ancestor `cgroup.procs` file **opened by the probe child**
using `openat` on a deliberately supplied ancestor `O_PATH` directory fd. Never
supply a supervisor-opened migration file: the kernel uses the opener's namespace
and credentials, not those of the writer. The supervisor first checks that the
ancestor file can be opened for writing. The child's own open must also succeed;
only the subsequent write's `ENOENT` is namespace-delegation proof. Both the
child's directory and migration fds close before adapter exec; a DAC-only `EACCES` refusal does not prove namespace
containment. Source evidence: Linux v5.16
[`cgroup_file_open`, `__cgroup_procs_write`, `cgroup_procs_write_permission`](https://github.com/torvalds/linux/blob/v5.16/kernel/cgroup/cgroup.c)
respectively retain `current->nsproxy->cgroup_ns`, use `of->file->f_cred` and
`ctx->ns`, and return `-ENOENT` for a source/destination outside that namespace
when `nsdelegate` is enabled. See also the
[cgroup-v2 namespace delegation rules](https://docs.kernel.org/admin-guide/cgroup-v2.html#delegation-containment). Also exercise `cgroup.kill`
and bounded `populated 0` observation on the held probe cgroup. An unexpectedly
successful migration fails qualification; retain the probe's pidfd for bounded
termination even outside the probe cgroup. Probe children receive no metadata,
artifact bytes or credentials. Repeat qualification against the held attempt
boundary before any real adapter launch; never trust a cached pathname or a
boolean supplied by configuration.

Set and verify a finite `pids.max` before creating either probe or adapter
children so a fork storm cannot outrun cleanup. Missing controllers, rejected
clone flags, unavailable namespace privileges, or any inconclusive probe refuse
activation **before executing the adapter**, with a bounded reason recorded.
Merely having a writable `cgroup.procs` path, kernel version, or same-account
workflow convention is not qualification. Unsupported platforms refuse as well.
Never fall back to process-group signals, descendant sampling or "wrapper
disappeared" as proof.

Before releasing authority, durably record the cgroup identity together with the
payload's kernel process identity and the exact claim/hold binding. Cleanup
writes `cgroup.kill` and proves `populated 0` via `cgroup.events` on that same held
cgroup within its cleanup deadline. Failure to kill, read, match identity or
prove emptiness retains capacity and records an unknown outcome, even if the
leader exited successfully. A kill-and-empty proof establishes **only absence of
processes in the attempt cgroup**; it never proves whole-host payload absence,
remote rollback or remote outcome. An escaped or unprovable payload is durably
recorded, never automatically re-run, and never released by pre-handoff recovery.
A supervisor crash leaves its persisted cgroup custody for explicit later
inspection; no restart launch hook is added here.

**Explicit pinned-adapter trust assumption:** this slice does not isolate all IPC
channels to already-running outside processes. The reviewed pinned adapter must
not delegate local work to an outside service (for example a user service manager,
D-Bus activation, a pre-existing terminal server, or an at/cron daemon). Cgroup
namespace containment does not prevent those handoffs. Every recorded outcome,
including success with proven cgroup emptiness, must carry the containment scope
`attempt_cgroup_only` and the limitation `outside_ipc_delegation_not_excluded`.
These are durable custody facts, not optional diagnostics. A known or suspected
outside handoff records unknown outcome and retains capacity for inspection;
kill-and-empty cannot discharge that custody. A malicious pinned adapter or
violated IPC trust assumption is outside this slice's local absence guarantee;
stronger sandboxing and qualification are required before claiming otherwise.

Fake subprocess fixtures must cover success, failure, hard-deadline hang,
oversized output, identity mismatch before authority, and an immediate
reparent/double-fork child. Containment tests must exercise the real kernel
boundary on a qualified isolated delegation; fake cgroup files or parent-child
sampling are not evidence of escape containment. Positive escape tests run only
where a real, qualified delegated cgroup-v2 subtree is available. Elsewhere,
including CI and macOS, the test must assert activation refusal and record that
the environment is unqualified, not substitute fake containment.

PR 691/694 follow-ups also require stale `deployment-handoff` reservations to be
presented as "inspection required, outcome <recorded outcome>, capacity retained"
(or "outcome unknown" only when none is recorded), including
in the dashboard's stuck-capacity detail. Preserve their bounded reason/outcome,
show no blank command and offer no recovery instruction for inspection-only
holds, even alongside ordinary actionable reservations. Never turn them into
`resource.recover force:true` suggestions. A host writer-lock failure before any
handoff write should return pre-handoff custody to its caller for explicit
compensation or retry; failures at/after a write or commit remain conservatively
ambiguous and must not return a reusable admission token.

## Required validation before enabling

- Operator-only create/edit/pause/resume/revoke, including direct dispatch and
  agent/crew/peer/scoped-client denial; owner-only reads and task handoff denial.
- Exact environment/repository/ref/digest/check matching; production marking;
  missing, skipped, stale, malformed and untrusted evidence; verifier argv and
  environment injection; artifact/commit substitution and merge-head mismatch.
- Grant revision/expiry/pause/revoke, charter revocation and task changes racing
  verification, consumption and claim; transaction rollback and two consumers
  racing the same idempotency key; independent valid attempts both recorded.
- Restart with pending, claimed, started and unknown payloads; failing spawn,
  surviving payload after wrapper exit, immutable adapter/config bindings,
  admission failure, bounded notices and retry deduplication.
- Audit facts survive optional notification failure and recovery. Assert relevant
  rows/identities, never global exact audit counts or uptime/sleep assumptions.
- Existing exact-artifact approvals, harness `approves`, delivery, upgrades,
  privileged operations and provider routing retain their current behavior.

Run the assignment's admitted serial fmt/clippy/lib/integration/bytecode/build
and verification gates for code slices; add the MCP gates if those paths change.
A design-only slice has no runtime behavior to test. Fake adapters establish the
local protocol/custody boundary, not real service interoperability, production
permission, hard same-user isolation or native fleet autonomy.

## Recorded decision and remaining qualification

The captain approved standing substitutes including production, retaining exact
artifact records, and chose notice then a grant-defined wait window with phone
pause/revoke, required expiry and max uses. There is no remaining production-gate
choice in this design. Trusted service integration, notification receipt semantics
and an isolated real-environment pilot still need separate qualification and
captain authorization. No production deployment is requested here.
