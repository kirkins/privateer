# Standing deployment grants

Status: **proposal; captain decision required before implementation**.
This note does not authorize a deployment or change an existing gate.

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
  deployment grant.
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

The captain must decide whether this is an acceptable alternative to the current
per-deployment rule. Until then existing approval behavior and generated text
remain unchanged. A nonmatch is a denial, not a prompt that silently grants more
scope. The ship may separately request an exact captain approval.

Production grants require an explicit `production: true` field. An environment's
production classification comes from captain configuration, not request text;
classification mismatch refuses. There is no implicit wildcard, environment
prefix matching, prose parser, or “all green repositories” rule.

## Data model (home-local and transactional)

Use a separate module and tables in the grantee's authoritative home database,
not an extension of fleet ship-management `grant.put`. One database owns grant,
consumption, audit and pending operation custody. Do not copy live authority to
a desk, accept a peer projection as authority, or transfer it with task custody.
Cross-home deployment authority is deferred.

`standing_deploy_grants`:

- `id`, `revision` (monotonic), `grantee` (one ship);
- `environment` (one exact identifier), `production`;
- structured `artifact_rule`: `kind: green_branch_commit`, canonical
  `repository_id`, full `branch_ref`, nonempty `required_checks`, and a
  captain-owned verifier configuration identity/digest;
- captain-owned deployment adapter identity/digest for this environment;
- `notify`, optional note and `expires_ms`;
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
and audit. Updating an active grant follows `grant.put` semantics; paused grants
must be resumed first, and a revoked id can never be reused or revived. All
fields are typed, size-bounded and reject unknown keys; no executable strings
inside an artifact rule.

## Proposed protocol and captain-only operations

Names below are proposed, not existing commands:

- `deploy_grant.put`: operator only; create or replace an active grant with
  expected revision on replacement.
- `deploy_grant.pause|resume|revoke`: operator only, expected revision; resume
  never clears revocation. Lifecycle changes and their audit share a transaction.
- `deploy_grant.list|show`: operator sees all, an agent sees only its own grants.
  List/show are bounded/paginated and expose effective lifecycle state.
- `deployment.start`: agent-owned open task, explicit grant id and exact
  artifact request; returns a durable operation id, **not a reusable bearer
  permission to run arbitrary shell**.
- `deployment.show|list`: owner-scoped status/evidence, bounded pagination.

Operator means the existing trusted captain control path. No captain name in
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
executable, endpoint, credentials, environment overrides or check names.

The verifier must establish all of the following from the configured forge and
artifact service, using bounded reads and refusing incomplete responses:

1. Canonical repository identity matches the rule, including forge identity;
   an owner/repository spelling alone is not globally unique.
2. The requested commit is exactly the configured branch tip at verification
   time (v1 deliberately does not accept arbitrary historical ancestry).
3. Every named required CI check is complete and successful on **that commit**
   from the configured trusted workflow/source. Missing, pending, failed,
   cancelled, skipped, untrusted or ambiguously duplicated checks refuse.
4. The exact digest/immutable locator names an artifact produced by the trusted
   build for that same commit. A green commit plus an unrelated local archive
   is not proof that the archive was built from it.

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
proof of correctness.

The branch/check service can change after its last read; there is no atomic
transaction across a forge and an upload service. Define the guarantee as green
branch tip **at the last verification**, pin the artifact immutably, and record
that evidence. Do not promise that the branch stays unchanged during upload.
If the artifact service cannot prove build provenance, the rule is unsupported
and the ship must use an exact captain approval instead.

## Consumption, process custody and audit

1. Validate the request, authenticate the requesting ship and check its owned
   open task and existing deployment charter action (`deploy_production` for
   production). The standing grant supplies captain consent, not that action.
   Check resource admission independently; no grant disables existing limits.
2. Verify artifact evidence as above, without starting the uploader.
3. Under the home write transaction, recheck all authority bindings. Atomically
   insert the immutable exact authorization/deployment row, pending intent,
   `deploy_grant.used` audit and optional captain-notice outbox item. Roll back
   everything if any durable write fails. Never return success first and log
   the use later.
4. The operator dispatcher claims pending intent once. Immediately before claim,
   recheck grant revision, pause/revocation/expiry, task/charter authority, config
   identities and fresh artifact evidence. A stale/revoked intent becomes denied
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
never credentials or unbounded remote output. Optional notices are durable,
retryable and deduplicated by deployment id; a notice delivery failure neither
loses the use record nor creates a second deployment. Use a bounded outbox with
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

## Scope and reviewable delivery slices

This is larger than adding a permissive branch to `delivery.check`. Proposed
split, each independently reviewable and fail-closed:

1. **This design and captain policy decision.** No Rust/generator changes.
2. **Core authority and custody.** Home-local grant lifecycle, revisioning,
   immutable deployment ledger, consumption, notices and dispatcher custody;
   fake trusted verifier/uploader at real subprocess boundaries in isolated
   tests. No bundled production environment; unsupported adapters refuse.
3. **Trusted artifact integrations.** Add commit/CI/artifact provenance support
   and an isolated uploader adapter for the required service, then qualify
   restart/race/idempotency behavior. Any real environment configuration and
   production pilot remain captain-authorized, separate from source delivery.

If slice 2 cannot include its verifier/dispatcher safety boundary in one
reviewable PR, split dormant storage/lifecycle from enabling consumption; never
ship enabled consumption backed by caller-supplied green booleans. Updating the
generated production rule belongs only with the approved, qualified authority
path, not with a dormant schema or this note. Existing exact approvals,
`delivery.check`, privileged commands and upgrade gates remain unchanged.

The later generated rule should say: production requires captain authorization
for the exact artifact and environment, either a per-deployment approval or a
matching captain-created standing deployment grant, with one durable consumed
authorization and artifact record per attempt. A prose standing instruction or
harness step approval is not a deployment grant.

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

## Decisions requested

1. Does the captain approve replacing a fresh per-artifact decision with a
   matching, explicitly created standing grant, while retaining exact-artifact
   authorization and audit for every attempt?
2. Accept the home-local, one-environment/one-repository green-branch rule and
   trusted artifact provenance requirement, with no caller-asserted CI fallback?
3. Accept the delivery split above and choose the first trusted service adapter
   separately? No production deployment is requested by this design.
