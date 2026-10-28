# MCP front door

## 1. Status, goal and non-goals

This is the phase 1 contract for [#452](https://github.com/kirkins/privateer/issues/452), not a claim that MCP, delegated door grants or OAuth already exist. The captain's decisions recorded in the #452 comments of 2026-10-24 UTC (the captain's review is headed 2026-10-23, local time) take precedence over the earlier design recommendations. Sections below distinguish current behavior, required new runtime work and unverified interoperability details.

Build an **opt-in, bundled front-door plugin** through which an external MCP client uses the same captain-facing runtime and mailboxes as other doors. An explicitly authorized **Full Captain** client has parity with a trusted captain front door, subject to the runtime's mandatory gates. There is no permanent MCP-only capability ceiling. Narrower clients receive independently revocable grants enforced by the core, not merely filtered tool lists.

This is not a rewrite of fleet orchestration, a new agent role, or Privateer's harness-side use of MCP. A third-party assistant is a client, not a ship or a privileged internal agent. This work does not require an external identity provider, expose the fleet by default, give crews the captain's accounts, authorize production deployment, change provider routes, or make same-user processes a hard isolation boundary. Development and demonstrations remain isolated from live installations.

The implementation baseline is [the door runtime](../../src/door.rs), [machine routing](../../src/machines.rs), [CLI dispatch](../../src/main.rs), [core dispatch](../../src/lib.rs), [captain operations](../../src/captain.rs), and the [Telegram](../../plugins/telegram/README.md) and [live-voice](../../plugins/live-voice/README.md) adapters. Existing standing [ship-to-ship grants](../../src/grants.rs) are not external-client credentials: use distinct records and names rather than silently reinterpreting them.

## 2. Captain decisions

The numbering here is the original written Q1–Q11, **not** the reordered verbal questions. Sources are the [questions](https://github.com/kirkins/privateer/issues/452#issuecomment-6377783232), [first captain review](https://github.com/kirkins/privateer/issues/452#issuecomment-6377902708), [follow-up](https://github.com/kirkins/privateer/issues/452#issuecomment-6377969933), and [closing record](https://github.com/kirkins/privateer/issues/452#issuecomment-6378297869). All eleven are answered; none remains an implicit approval.

| Question | Binding answer | Consequence |
| --- | --- | --- |
| Q1: priority | **(b), after #417 voice**, with shared core foundations allowed in parallel where they do not derail voice | Phase 1 and phase 2 may run alongside remaining voice items; this is not permission to skip their gates. |
| Q2: Python dependencies | **(a), official Python `mcp` SDK in a hash-locked per-plugin environment**, using `uv` or pip | No system-wide installation, vendored platform wheel collection, or hand-written replacement framing as the default. |
| Q3: initial isolation | **(a), trusted plugin initially**, documented plainly; tighten OS/process isolation in hardening | No day-one Landlock or root setup requirement. Evaluate Linux Landlock later; a separate OS user is optional stronger isolation, not an approved setup action. |
| Q4: remote reachability | **Loopback/private by default; remote is a first-class opt-in** with HTTPS, explicit `public_url`, authentication and permissions | Support managed tunnels and operator reverse proxies without requiring any particular service or identity provider. Not merely a trial-only feature. |
| Q5: real clients | **Captain-run account-specific checklist** | Crews exercise local/test clients; the captain personally tests ChatGPT/claude.ai with their accounts and configured HTTPS endpoint. Never share those credentials. |
| Q6: production deployment approvals | **(b), Full Captain with confirmation mode** | A client may initiate a deployment, but final authority requires a separate trusted captain confirmation bound to the exact artifact and environment, plus existing gates. This is new runtime work. |
| Q7: privileged/root approvals | **(c), only with confirmation mode** | Full Captain may request/relay the exact operation, but needs separate trusted captain confirmation and existing terminal hash/sudo gates. MCP alone never executes as root. A narrower client needs an explicit suitably scoped policy and strong confirmation; nothing is inferred from messaging rights. |
| Q8: sends and away mode | **Do not end away mode unless the grant includes `captain:presence`** | Scoped sends without that scope leave presence and held-mail cursors unchanged. |
| Q9: older MCP clients | **Modern protocol and compatible 2025-era clients from the start** | Exercise and document both paths; do not ship modern-only and defer compatibility until someone complains. |
| Q10: OIDC verification | **(a), Rust core, reviewed OIDC crate behind a Cargo feature** | The plugin cannot assert that an ID token was verified or decide who the captain is. |
| Q11: v1 `file.put` scope | **Restrict to each door's own state directory in a separate small PR** | Already present in the baseline through [#456](https://github.com/kirkins/privateer/pull/456); preserve it, rather than claiming it is future MCP-only protection. |

Additional agreed requirements:

- Standalone self-hosted authentication works without Authelia, Authentik, Keycloak or any hosted account. OIDC is an optional login backend, not the fleet permission authority.
- Require **explicit owner approval for every new client by default**. Policy-driven automatic enrollment is future work, not an enabled default.
- Start with a **minimal safe grant**, then explicitly expand per client and per ship. An available preset is not an automatic grant; Full Captain requires deliberate selection.
- Prefer nonrevealing **not-found-style** results for denied/unapproved tool or capability discovery where suitable, while retaining precise internal reason codes. Do not replace required OAuth authentication challenges with misleading success or break protocol discovery.
- No anonymous access, including public read-only fleet access. Public protocol/auth metadata is not fleet access.
- Keep useful authentication/audit events, with redaction, bounded retention and useful safe client identifiers. Never log tokens, login codes, passwords or complete authorization headers.
- First run clearly displays **local-only** until remote access is deliberately configured.
- Encrypted local configuration is an acceptable initial secrets baseline only with sound key custody, permissions and redaction. Encryption with a freely readable adjacent key is not isolation. Secrets-manager integration may follow.
- Ordinary useful audit logs are sufficient initially; preserve structured events so tamper-evidence can be added later.
- Cross-machine door delivery was folded into #452 in the captain's 2026-10-24 handoff: subscribe where the captain desk lives, with durable remote per-door cursors and validated acknowledgments. Section 5 specifies the ownership and duplicate policy this needs.

## 3. Architecture and enforcement contract

### Components and protocol versions

```text
external MCP client -- HTTPS / loopback HTTP --> MCP plugin
captain browser ----- login / consent pages --> MCP plugin
                                                | scoped door v2 (JSON lines)
                                                v
                              core auth state + client grants + authorization
                                                |
                          existing captain commands, mail and linked machines
```

The plugin handles HTTP, MCP framing, tool schemas and rendering login/consent pages. It relays authorization operations to the core. It does **not** issue tokens, store the authoritative grants, approve enrollment, verify captain identity on its own, or implement orchestration. It is disabled until deliberately registered and started through the existing door lifecycle.

Add **scoped door protocol v2 beside trusted v1**. Telegram and live-voice remain v1 with their current trust semantics; remote-voice migration is separate. Registry-controlled door trust, chosen by the operator at registration, determines authority. A scoped door cannot downgrade to v1 or acquire captain trust by changing `hello`, its manifest at runtime, or request arguments. Registration must prevent the bundled MCP adapter from being accidentally admitted as an implicitly trusted captain door. Restart must retain that choice.

A proposed v2 request envelope is illustrative, not an implemented API:

```json
{"type":"request","id":"r1","command":"send","auth":{"token":"<access-token>"},"args":{"to":"<ship>","body":"<message>"}}
```

For **every scoped request**, the core must:

1. Resolve the opaque token through its own hash lookup. Reject missing, unknown, expired, revoked, wrong-door or wrong-resource tokens and paused, expired or revoked grants. Identity comes **only from token lookup**, never client-supplied `captain`, `from`, `operator`, `grant` or liaison claims.
2. Compute required capabilities from a single core command-and-arguments table, including implicit writes. Unknown commands and unsupported argument variants fail closed. Check the current grant, target ships, resource audience and any confirmation requirement, not a cached permission snapshot embedded in a token.
3. Carry an immutable delegated context through actual privileged execution, secondary calls, mailbox replies and remote gateways. Merely checking a wrapper then calling a generic operator dispatch with arbitrary arguments is insufficient. Reject reserved identity/provenance fields and write authoritative ones in the runtime.
4. Resolve approval-bearing replies against the stored request, not the client's claimed request kind. Only an authorized approval path may create an operator approval marker. Liaison delegation cannot drop the grant, add scopes or transform mail into approval evidence.
5. Execute through existing runtime commands and record stable result IDs and redacted audit events. Check grant validity and consume one-shot authorizations atomically with the protected state change wherever they share storage. Define remote admission/revocation ordering explicitly; a transport failure is never authorization to retry as an operator.

Only a fixed allowlist of public metadata and bounded login/authorization bootstrap operations can run before token authorization. They return no fleet data and do not create a grant without owner approval. Do not allow unauthenticated `oauth.*` or `meta.*` by wildcard.

Revocation applies at the next protected admission and must also stop new data from long-lived mail subscriptions. It cannot retract data already delivered or undo an admitted side effect. Long-running operations need explicit custody/status/cancellation semantics; revoking a client is not proof that its subprocess stopped.

### Core state

| Record | Required bindings and behavior |
| --- | --- |
| Client/application | Stable client ID, registration method, validated redirect URIs, display metadata and registration history. Registration alone gives no grant. Treat names/logos as untrusted display data. |
| Grant | Authenticated captain identity, specific application and connection identity, door, exact resource/audience, expanded scopes/preset, ship restrictions, confirmation policy, optional expiry, lifecycle timestamps and last use. Multiple tokens for one grant do not create independent readers. |
| Access token | Cryptographically random opaque value; persist only its hash and grant/audience/lifetime binding. Re-read effective grant on use. No client-minted JWT assertion of authority. |
| Refresh token | Hashed, grant-bound token family; rotate transactionally on every use, detect reuse and revoke the affected family. Concurrent refresh behavior must be tested. |
| Authorization code | Hashed, short-lived, single-use code bound to client, exact redirect, resource, requested/approved scopes and PKCE S256 challenge. Consume transactionally on successful exchange. |
| Login transaction | Expiring one-time standalone login credential or core-verified OIDC result, anti-CSRF state and consent binding. It is not a reusable fleet bearer credential. |
| Confirmation | One-shot, expiring core record bound to grant, exact action and its immutable inputs; trusted captain authentication and consumption evidence, not client narration. |
| Delivery state | Durable per-door subscription and per-grant reader state, carried references, desk generation and bounded paging; see section 5. |
| Audit | Grant/token/login/request/confirmation/delivery events with safe IDs, allow/deny reason, timestamp and origin machine; no raw secrets or unrestricted argument dumps. |

Use reviewed libraries and OS randomness, not new cryptographic primitives. Exact token lifetimes, login-code length/expiry, limits and table/API names require implementation review; draft values are not captain decisions. Grant list/show/restrict/pause/revoke/rotate operations must be available through an authenticated owner surface without disrupting other doors. These client grants must remain distinct from standing ship-management or deployment grants.

### Presets

Proposed presets expand to the scope table in section 6:

- **Observer:** `fleet:read`, `mail:read`, limited to explicitly selected visibility. Mail may be sensitive; this is not synonymous with the minimal first-run grant.
- **Operator:** Observer plus `ship:message`, `work:manage`, `file:transfer`; no approval/admin/presence authority by implication.
- **Full Captain:** all approved captain-facing capabilities, including `approval:answer`, `fleet:admin`, `captain:presence`, with mandatory sensitive confirmations intact.
- **Custom:** explicit scopes, ship restrictions and optional stricter confirmation policy.

Default enrollment should request only the minimum visibility needed, not silently select Observer fleet-wide. Ship restrictions must apply to reads, mail, file results, work targets and liaison routes as well as direct sends. A scope is an upper bound, never permission to bypass the underlying charter or runtime gate. Full Captain is a parity promise, not a wildcard to call internal RPCs or mint new owner-approved clients without consent.

## 4. Sensitive approvals: new runtime work

**Q6 (b) and Q7 (c) supersede the draft recommendations.** They authorize designing confirmation support, not enabling it in current doors or treating an MCP tool call as the captain's approval.

### Production

A deliberately granted Full Captain client may conversationally initiate a production deployment. The core must hold the sensitive action pending **separate trusted confirmation authenticated as the real captain**, bound to the exact artifact and target environment. Display those immutable values on a trusted surface independent of the assistant's asserted consent. A changed artifact, changed environment, stale request, changed authority or expired confirmation requires a new decision. Record approver and consumption against the actual deployment.

Keep all existing production checks, including task/charter authority, review/delivery requirements and exact approval matching. Existing captain-created standing deployment grants remain explicit, separately recorded policy; MCP enrollment must not create or expand them or silently substitute them for this confirmation flow. MCP alone never approves a production deployment.

### Privileged actions

Full Captain may request/relay an exact privileged operation, but it needs a separate trusted captain confirmation of that **exact privileged request**, including the existing machine/program hash/argv/cwd binding and expiry. Preserve terminal hash confirmation, `sudo` and existing one-shot consumption/revalidation. MCP provides neither `privileged.run` nor an equivalent root-execution tunnel. A narrower client requires explicitly authorized suitably scoped policy and the same strong confirmation, not merely `ship:message`.

Today the door `approval` command answers only a locally staged privileged request with the checked hash prefix. It does not answer production approvals. `privileged.run` is terminal-only and refused through dispatch/RPC/gateways/doors. These existing paths do not implement the new MCP confirmation requirement. See [privileged actions](privileged-action.md) and [delivery and deployment](../architecture.md#delivery-and-deployment).

For both categories, an MCP elicitation, browser dialog in an untrusted client, model-generated “yes,” echoed hash, or liaison message is **not** trusted confirmation. The core must bind, expire, revalidate and consume confirmation once; replay, cross-grant confirmation and races with revocation must fail. Which independent trusted surface supplies this proof is an open implementation item, not a reason to weaken it. Optional per-grant confirmation for other writes may reuse this mechanism.

## 5. Cross-machine mail, away routing and duplicate policy

### Current behavior after #454

[Door delivery coverage](../../src/machines.rs) is diagnostic only. `door_mail_delivery()` does not contact a peer, collect mail, move the captain desk or create a route. Its `desk_machines`, `reaches`, `reverse_only`, `summary` and `warning` appear in `hello`/`status`, with a warning at door start.

The v1 supervisor polls **local active ship homes**, including captain mail already collected from linked machines. A locally sent request's correlated replies and notices can return when this machine reaches the responder and collection runs. Reverse-only links cannot be collected from here. Uncorrelated notices and ship requests follow captain-desk placement; mail held only at another machine's desk is not visible to this local door. Sending to a linked ship and `file.put` can route remotely, but that is not complete remote inbox coverage. `ships` includes linked inventory; `status` reads local homes, and door `waiting` calls the local `captain::waiting()` helper rather than the desk-routing dispatch wrapper. Do not infer fleet-wide observations from those results.

Each v1 door has a durable cursor **per local home**, separate from the captain's dashboard read cursor. A new door starts from the captain's acknowledged position. The session tracks emitted mail's carried high-water mark per ship and accepts acknowledgments only within that bound; digest acknowledgments require an emitted digest ID. This is at-least-once delivery, with adapter mail-ID deduplication. One door's acknowledgment does not mark the dashboard read or suppress a different door.

Current away routing and digest construction use the local registry and local mail stores. Returning from away advances registered door cursors past held mail and creates catch-ups. An ordinary v1 `send` invokes `away::back()` **before** recipient/message validation, so even a failed send attempt can have presence side effects. None of this is a durable remote subscription protocol.

### Required subscription contract

A door must be able to **subscribe at the machine holding the captain desk**, even when its adapter runs elsewhere. Implement this as core-to-core communication over authenticated configured machine links, not arbitrary plugin-supplied remote authority. Desk placement must not change just because a door starts.

- The desk core owns authoritative away state/routing decisions, digest baselines/builds, held-mail classification and catch-up decisions for desk subscriptions. The adapter host relays events, not a second independently computed digest. Cross-machine presence writes go through that owner under `captain:presence`; link failure cannot silently create divergent local presence state.
- The desk persists a per-door remote subscription cursor for each origin mailbox, plus per-grant MCP reader state. Cursor identity includes the originating machine/mailbox and a desk/subscription generation so unrelated sequence numbers cannot collide. A single plugin-global cursor must not let one client consume another's mail.
- An acknowledgment is bound to the registered door, authorized reader, subscription generation and **mail actually carried** on that stream. Reject forged, future, wrong-origin, wrong-grant and stale-generation refs. Persist delivery and ack transitions transactionally and monotonically; retain enough bounded carried state to validate after reconnect without trusting a claimed cursor.
- Pull/list/show/thread operations page by stable IDs with explicit continuation. Listing and acknowledgment are separate: a read/list call must not acknowledge unseen mail. At-least-once delivery plus stable-ID deduplication is the guarantee, not exactly-once observation. Maintain bounded in-flight windows and backpressure.
- On link failure return incomplete coverage and retry state, never success with a silent missing inbox. Restart must recover durable cursors. Desk moves require a generation/handoff protocol with one recorded owner and preservation of unread/open facts; stale acknowledgments cannot advance the new desk's stream.
- **Across doors, duplicates are intentional:** independently authorized doors may show the same mail, and each owns its own delivery acknowledgment. Never globally suppress another door because one door or MCP grant read it. Deduplicate repeated copies within a reader by durable mail identity, preserving origin and correlation. Digest/catch-up accounting is per subscription and must preserve durable facts when combining notifications.

This is new runtime work under #452, not behavior delivered by [#454](https://github.com/kirkins/privateer/pull/454). Test both link directions, collection, desk changes, restart, lost acknowledgments, duplicate mail and stale generations. [Door mail coverage and desk-generation handoff](door-mail-coverage.md) specifies the phase 2.4 design, including the newer v2 baseline, proposed wire operations, reverse-only limits and migration of existing state; do not advertise full cross-machine coverage before that work passes.

## 6. Captain operation inventory and scope mapping

This catalogs all **13 current door request commands**, their argument-dependent authority and delivery events. Names are runtime commands, not final MCP tool names. Scopes are a proposed taxonomy to implement centrally; they do not change today's trusted v1 boundary.

| Current operation | Core path / effect today | Proposed scope and hidden effects |
| --- | --- | --- |
| `ships` / `fleet_ships` | Unchanged legacy directory; explicit `schema:"runtime.v1"` safe metadata projection retains missing homes and nullable local failures | `fleet:read`; allowed `machine`/`ship` core handles only with opt-in. Runtime-domain `coverage.v1`; V2 reads callable links via dedicated `fleet.runtime.v1`, labels reuse/cache/carry and filters at receiver and host. No home/work/presence/mail writes; bounded process-memory observation cache plus existing registry auth/audit bookkeeping. [V1 contract/tests](mcp-fleet-visibility.md#v1-delivered-contract-and-evidence), [V2 contract/tests](mcp-fleet-visibility.md#v2-delivered-contract-and-evidence). V3 adds optional closed checked worker/session counts through the same readers, without host mutations or screens; [census evidence/limitations](mcp-fleet-visibility.md#v3-delivered-contract-and-evidence). |
| `ship_jobs`, `job_inspect`, `ship_work` | V4 fixed selected-ship job history/detail and scheduled/unscheduled backlog reads; safe origin/incarnation-bound refs, bounded pages/count coverage | `fleet:read`; authored progress/park-next/schedule reason needs explicit `include_text:true` plus `mail:read`. Receiver validates `fleet.inspection.v1` and local ownership; host rechecks credential/generation before release. No argv/env/cwd/log/process/binding fields, reconciliation, enqueue/defer/claim, mail/presence or home writes. Registry-only bounded continuation and auth/audit bookkeeping are explicit; Linux/macOS continuations are boot-bound across gateway subprocesses. Counts are retained/scan-window facts, never workers or complete prior history. [V4 contract and fixtures](mcp-fleet-visibility.md#v4-delivered-contract-and-evidence). |
| `status` / `fleet_status` | `{}` preserves local legacy output; `schema:"runtime.v1"` opts into closed metadata DTO, nullable errors, tri-state working and per-facet runtime coverage | `fleet:read`; selectors narrow allowed directory handles, never grant authority. V1 never asserts positive activity; false needs all fresh local negative checks, otherwise unknown. No authored text, argv, cwd, process/binding or operator JSON. V2 adds recorded owned job kinds/states/wait kinds and full nonterminal totals from linked cores; expanded-state counts remain partial when truncated. Cached/carried facts never enter current totals; release rechecks the admitted credential/authority. No work/presence/mail writes, only bounded process-memory cache and registry auth/audit. [V2 contract and exact fixtures](mcp-fleet-visibility.md#v2-delivered-contract-and-evidence). V3 now supplies affirmative payload/busy-turn evidence, deduplicated worker counts, distinct command/viewer/session counts and explicit uncertainty. Live supervisors/stored running alone still cannot assert true; fresh zero activity also needs ledger/dispatcher evidence. Nullable crew-pane job incarnation is recorded only on existing native writes, never migrated by reads. [V3 contract/tests](mcp-fleet-visibility.md#v3-delivered-contract-and-evidence). |
| `waiting` | Local `captain::waiting()` helper | `fleet:read`. Outstanding asks/decisions may contain sensitive text; enforce visibility, do not claim remote desk completeness. |
| `held` | `away::held()` | `mail:read` because it exposes held mail, not just aggregate state. Reading leaves the captain away. |
| `away {action:status}` | Read current away state | `fleet:read`; no presence change. |
| `away {action:start\|back}` | Enter/leave away mode | `captain:presence`. Writes routing/catch-ups and may advance all relevant door cursors. |
| `digest` | Set session flag to build/send an away digest if supported | `captain:presence` **and** `mail:read`. Leaves captain away, but builds durable digest rows and can cause delivery; not a pure read. |
| Runtime `mail` event / `delivered` | Push mail; delivery ack advances this door's per-home cursor | `mail:read` on v2 subscription/pull and ack, bound to the reader. Cursor advance is a write, not dashboard read acknowledgment. |
| Runtime `digest` event / `delivered` | Render/retry digest; ack its durable row | `mail:read`, same reader binding; independent of explicitly requesting digest generation. |
| `send`, kind `request` or `notice` | `mail.send` as captain, local home or linked gateway; audited `door.send` | `ship:message`. Assigning a first mate work is a request, not a second orchestration system. Today sends end away; scoped sends must not without `captain:presence`. |
| `send`, kind `reply`, with correlation | `mail.send` captain reply | `ship:message`, plus `approval:answer` for approval-bearing replies. Today's operator marker can authorize tool-permission/captain-approval decisions; classify from stored request and payload. Requirement for scoped sends: production and privileged requests must never be answerable as ordinary replies. Today `captain.reply`, `mail.send` and `mail.deliver` refuse ordinary replies to both production and privileged approval requests, including the v1 door `send` path through `mail.send`; preserve those stored-request checks in scoped execution. |
| `dismiss` | `captain.dismiss` via dispatch | `work:manage`. Ends an ask/decision or marks a notice read; refuses production/privileged approval kinds. May route to the desk when not held locally. |
| `file.put {to,name,content_file}` | Scoped local source file to receiving home, including linked transfer | `file:transfer`. Reads a local file and writes remote/local file state; returns a file ID, not completed work. #456 already restricts sources to this door's state directory. |
| `approval {id,action,hash}` | `privileged::door_answer`; grant/deny a locally staged privileged request; both need a hash prefix of at least `CONFIRM_CHARS` characters | `approval:answer` plus mandatory trusted confirmation for scoped MCP. No production approval and no root execution. Never interpret a model-echoed hash as captain proof. |
| `permissions {}` | `permissions::across_machines` read | `fleet:read`; report partial remote failures accurately. |
| `permissions {mode}` | Set standard/yolo across linked machines | `fleet:admin`. Changes first-mate tool enforcement; no implication of deployment or root authority. |
| `stow {action:status}` | `stow.status` via dispatch | `fleet:read`; coverage depends on machine/all arguments and links. |
| `stow {action:start\|resume}` | `stow.start`/`stow.resume` | `fleet:admin`. Checkpoints/stops or resumes fleet work; retain machine-specific authorization and custody. |
| `upgrade {action:plan}` | Linked upgrade plan, fresh confirmation | `fleet:admin`, **not** read-only: overwrites a single five-minute confirmation slot in the current door session. |
| `upgrade {action:confirm}` | Consume session confirmation, re-plan/equality-check targets, dispatch `upgrade.start` | `fleet:admin`, existing gates plus any applicable sensitive confirmation. New v2 confirmation must bind grant, exact plan/artifact/targets and expiry; one client cannot consume another's slot. |

The extra `captain:presence` class is justified by fleet-wide delivery effects: an assistant continuing work while its captain is away must not announce a return. This mapping deliberately classifies `held` as `mail:read`, not the old draft's broad `fleet:read`.

### Parity additions: not implemented door commands

| Needed surface | Existing foundation / gap | Proposed scope and requirement |
| --- | --- | --- |
| Pull inbox, show message, thread, explicit ack | v1 is push-only; underlying `mail.inbox`/`mail.show` exist | `mail:read`; new v2 reader API, bounded paging, independent cursors and coverage. Do not expose raw arbitrary-address mailbox RPC. |
| Activity and recent wakes | `captain.feed`, `wake.list`/CLI | `fleet:read` metadata, authorized content separately scoped; see [runtime inventory and redaction](mcp-fleet-visibility.md). Transcript/screen access needs explicit review, not assumed under summary reads. |
| Live fleet workers, sessions, jobs, scheduling and capacity | `fleet.snapshot`, `work.status`, `job.list/status`, `ship.bearings`, `resource.status`, `runtime.needs`, checked host metadata | [Fleet visibility and runtime parity](mcp-fleet-visibility.md) V1–V4 deliver opt-in local/linked safe recorded metadata plus checked worker/session census, per-facet coverage and dated reuse/cache/carry through existing ships/status tools, and selected-ship job history/detail and structured scheduled/unscheduled backlog through `ship_jobs`, `job_inspect`, `ship_work`. Capacity and typed blockers remain V5; actions and aggregate mail remain V6/V7. Metadata `fleet:read`; authored progress/park-next/schedule reason text additionally needs opt-in and `mail:read`, never implicit grant expansion. |
| Backlog and ranking | `task.backlog`, `task.rank`/CLI | Reads `fleet:read`; rank/other custody-preserving work changes `work:manage`. [Guarded action ledger](mcp-fleet-visibility.md#4-action-parity-ledger-not-a-new-orchestration-path): preserve ownership, enqueue/claim checks and resource admission; backlog is not a worker count. |
| Consistent reply/annotation routing | CLI uses `captain.reply`; doors currently send mail directly | `ship:message`, plus `approval:answer` where applicable; preserve prefix lookup, attached asks and refusal of sensitive approval kinds. |
| Files/reports to captain | `report.get` and stored file metadata exist; no captain door download command | `file:transfer` for bounded file retrieval, `mail:read` for authorized report content. Define safe handles, never arbitrary host paths. |
| Production/privileged initiation and confirmation status | Existing task/mail, approval/delivery and privileged staging machinery | Explicit scoped initiation; final protected answer under `approval:answer` **and** section 4. No `privileged.run` exposure. |
| Ship/fleet administration, client-access management | CLI/core ship/project/machine/door/grant/capability/route surfaces | `fleet:admin` only for explicitly mapped captain operations, with existing policy and owner-consent requirements. Inventory individual commands and tests before exposure; no raw dispatch escape hatch. |

Maintain a parity ledger covering each applicable captain-facing command, tool name, target policy, hidden writes and test. Deferred surfaces are tracked implementation gaps, **not** a decision to impose an MCP-only ceiling. Conversational `ask_liaison` is just a scoped send to the normal recipient: door default, otherwise one liaison, otherwise the sole local ship. It grants no additional authority. If the liaison cannot preserve/enforce delegated policy for a requested operation, deny that path rather than laundering an Operator request into captain administration.

For files, #456 checks a regular single-link source, a non-symlink state directory and the resolved source path; it rejects escapes and symlinks pointing outside. It is an accidental-path guard, **not hostile-plugin containment**: the source is reopened downstream and the same-user plugin owns the directory. Scoped uploads/downloads need bounded handles or descriptor-safe handoff and race tests, not an unqualified claim that canonicalizing a path solves TOCTOU.

## 7. Threat model

Assets include captain authority, fleet mail/files and source, approval evidence, routing/away state, client credentials, consent state, audit history and availability. Actors are the captain/owner, differently scoped external clients (including prompt-injected assistants), ships/liaisons, the trusted but network-facing plugin, local users, network attackers, and any TLS-terminating tunnel/proxy or optional identity provider.

Trust boundaries are client-to-HTTP endpoint, browser-to-login/consent, plugin-to-core stdio, core-to-stores/execution, core-to-linked-machine gateways, and optional core-to-OIDC issuer. Each boundary must preserve provenance rather than accepting an asserted captain identity.

| Threat | Required mitigation | Residual risk / test focus |
| --- | --- | --- |
| Guessed tools, raw commands or forged identity exceed grant | Core command/argument allowlist, token lookup, target checks, immutable delegated context, fail closed | Test every entry point, alias, reserved payload and missing context, not just `tools/list`. |
| Prompt injection misuses legitimate authority | Minimal grants, ship restrictions, explicit expansion, sensitive confirmation, audit/revoke | A client can still abuse everything its real grant permits; Full Captain intentionally has broad authority. |
| Approval laundering through replies/liaison | Resolve stored ask; gate operator markers; propagate provenance; reject sensitive relays | Agent behavior is not a permission check. Test fan-out, attached asks and remote replies. |
| Compromised same-user plugin bypasses door entirely | State trusted-plugin limit; later OS/process confinement or separate user/broker | **High initial residual risk:** it can read local fleet state, invoke the CLI or steal tokens. Core checks do not contain it. |
| Token theft/replay or cross-client confusion | HTTPS, hashes at rest, short access lifetime, audience/door binding, rotation/reuse detection, targeted revocation | Plugin sees bearer tokens in transit and can replay those it has seen; hashing does not prevent that. |
| Consent phishing, malicious display metadata, forged client “yes” | Trusted owner bootstrap/confirmation, bound consent summary, exact redirects, escape rendered data, CSRF defenses | Plugin serves pages and is initially trusted. A polished consent page alone cannot prove owner action. |
| Code exchange/refresh race or stale grant | Transactional code use/rotation and current-grant checks at execution | Test replay, concurrent refresh/revoke, restart mid-exchange and remote admissions; no stale authorization cache. |
| DNS rebinding, browser CSRF or local port probing | Loopback default, Host/Origin checks, tokens on all fleet access, state/nonce and form CSRF controls | Loopback is reachable by other local processes; it is not authentication. |
| SSRF from client metadata, discovery or OIDC URLs | Reviewed fetching policy, HTTPS, destination validation on resolution/redirect, byte/time/redirect limits | Private-network OIDC needs an explicit trusted configuration exception, not attacker-selected private destinations. |
| Redirect/issuer/audience mix-up | Exact registered redirects, PKCE S256, issuer response binding, exact resource audience checks | Test wrong issuer/resource, code substitution and unregistered redirects; no token passthrough. |
| Files leak host secrets or race path checks | State-scoped upload, size quotas, safe handles, target checks and descriptor-safe reads | #456 is not a sandbox; test symlink/hard-link/rename races and remote transfer failure. |
| One client consumes another's mail or upgrade plan | Independent cursors, carried-ref validation, grant-bound plans and one-shot confirmations | Test reconnect, stale acks, desk moves, duplicated doors and simultaneous confirmations. |
| Remote outage gives false completion | Coverage/error fields, stable mail IDs and async result correlation | “Accepted/sent” is not “delivered” or “task completed”; cancellation is not proof a payload stopped. |
| Resource exhaustion and authentication brute force | Bounded queues, bodies, streams, logs and histories; per-IP/client/grant/core login limits; supervisor backoff | Public endpoints remain DoS targets. Limits must apply before expensive parsing/fetching as well as inside core. |
| Secrets in logs/results or tampered audit | Redaction, safe IDs, bounded retention, private permissions, encryption/key custody | Mail itself may contain sensitive material. Same-user audit is mutable; tamper-evidence is later work. |
| Accidental exposure or untrusted proxy headers | Explicit `public_url`, deliberate remote setup, exact proxy trust, no automatic forwarded-header URL discovery | TLS terminators may see content/tokens; the captain chooses that trust. |

**Trusted-plugin limit:** initial phases run the adapter as the same OS user as Privateer. Plugin-local checks, tool hiding and core scopes protect external client authority through the intended channel, not against that plugin taking over its account. Hardening must test actual subprocess boundaries and filesystem/CLI escape attempts. Landlock is a Linux hardening candidate, not portable complete isolation; a separate OS identity or constrained broker is stronger and requires explicit operator setup. Do not claim either is already implemented.

## 8. Transport, authentication and networking

### MCP versions

Use the official Python `mcp` SDK in a **hash-locked per-plugin venv**, with pinned transitive dependencies and reproducible `uv`/pip bootstrap instructions supplied with the plugin. Select and test a release supporting both required protocol eras; availability of that exact SDK release is unverified here. Avoid installing packages into system Python. Optional stdio may later bridge to the same scoped core; it must not become an unauthenticated operator channel.

The [2026-07-28 Streamable HTTP specification](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/basic/transports/streamable-http.mdx) uses request-scoped POSTs with JSON or SSE responses, not protocol sessions or a GET event stream. Request metadata must match the negotiated version and message. Pull mail uses explicit core reader handles, not protocol-session state. Closing a stream does not unsend already accepted mail.

Serve compatible **2025-era clients from the first plugin release** (Q9), with their initialization/session and optional GET-stream behavior handled separately by the SDK. Do not impose modern-only GET rejection on that compatibility path. Select the exact supported 2025 revisions using client evidence and document unsupported versions and fallback errors. Do not claim a particular vendor/version works without running it.

MCP results are structured and carry stable ship/mail/request IDs, pagination and honest states. A send acknowledgment means accepted/sent; later correlated mail/status reports the outcome. Persist bounded idempotency records so a lost HTTP response and retry do not mail twice; mismatched key/arguments must fail. Filter tool discovery by effective grant, but recheck in core on every invocation. Tool annotations are usability hints, not authorization. Elicitation is interaction, not sensitive approval proof.

### Standalone OAuth and optional OIDC

Privateer supplies a built-in authorization server state machine in the core. The plugin exposes endpoints/pages as a transport relay. The setup flow is: enable door, see local-only status, select standalone login or optional OIDC, configure remote access if wanted, connect a client, authenticate, review that specific client and requested permissions, explicitly grant, then use tools.

Standalone bootstrap should use an expiring, single-use login credential issued through the trusted owner CLI or an explicitly supported trusted door, entered into the login page. Do not ask a captain to approve an attacker-supplied pairing code without context. The core binds the login transaction, application, redirect, resource and final consent. An ordinary client token cannot enroll another client or approve its own expansion. Notify the owner of new/changed grants and provide list/inspect/restrict/pause/revoke/rotate, showing application, captain identity, scopes/ships, last use, expiry and activity.

Required OAuth contract, following the [MCP authorization specification](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/basic/authorization/index.mdx):

- OAuth 2.1 Authorization Code with **PKCE S256**; no implicit/password grant. Codes are short-lived and single-use. Enforce exact redirect matching, client binding and transaction/CSRF protections.
- RFC 9728 protected-resource metadata, advertised through well-known discovery and `WWW-Authenticate` on 401; authorization-server metadata (RFC 8414) with exact issuer and supported PKCE methods. Emit/advertise the RFC 9207 issuer response parameter. Public metadata exposes no fleet inventory.
- RFC 8707 `resource` binding at authorization and token exchange; core checks the exact intended audience and door on every protected request. Never pass access or refresh tokens to ships, agent prompts, tool results or upstream services.
- Rotating refresh tokens with transactional reuse detection and token-family revocation; independently revocable grants; bounded redacted failed-login/authentication audit and rate limits enforced in the core as well as at HTTP ingress.
- Client registration follows real client requirements: pre-registration, Client ID Metadata Documents (CIMD), and compatible Dynamic Client Registration (DCR) where needed. Registration is not owner consent. The modern spec recommends CIMD and retains DCR for compatibility; **actual SDK and target-client registration behavior is unverified** until traced. Do not advertise unsupported registration metadata.
- Invalid/missing credentials get appropriate OAuth challenges. For an authenticated client's hidden/denied tool, prefer a nonrevealing not-found-style result where suitable; internally retain precise denial reasons. Scope step-up must request explicit owner approval and follow protocol challenge requirements without dumping a list of private capabilities.

OIDC is a later authentication backend behind a **Cargo feature**. The Rust core, using a reviewed OIDC/JOSE crate, verifies issuer, signature/algorithm/key, audience/authorized party, expiry and nonce/state binding and maps configured issuer/subject to the captain. Plugin-provided decoded claims are not evidence. Discovery/key rotation must preserve that trust. OIDC credentials are not fleet access tokens, and changing providers does not change client grants or scopes. Choose the crate and network-fetch ownership during implementation; no external provider is required for default setup.

### Networking and limits

Bind to loopback by default; first-run UI and logs say local-only. Remote operation is a supported opt-in with explicit HTTPS `public_url` as the resource/metadata base, using either a managed tunnel or operator reverse proxy. No specific tunnel or identity provider is required. Private-network access is valid for reachable clients; hosted connectors generally need a publicly reachable HTTPS endpoint and cannot simply connect to the captain's loopback/private network. Account/plan restrictions remain client-specific and unverified.

Keep the plugin listener on loopback behind the proxy/tunnel where practical. Deliberate non-loopback/private binding needs explicit configuration and equivalent TLS/network controls, not an accidental default. Validate Host and any supplied Origin against exact configured values, rejecting invalid Origins with 403; absence of Origin on a nonbrowser client is not authentication. Trust forwarded headers only from explicitly configured proxy peers, not every loopback process. Never derive issuer/audience from attacker-controlled forwarding headers. Ensure proxies support request-scoped SSE without buffering and configure timeouts and body limits. Apply bounded per-source and per-grant admission/rate limits, including login, registration and metadata fetches; specify operational values with tests, not unlimited defaults.

## 9. Phased delivery plan: PRs and evidence

This is a sequencing plan, not a claim of completed implementation. Each PR preserves relevant v1 door and harness-side MCP behavior. Use isolated fleets, fake providers and **real subprocess adapters under the supervisor** where process boundaries matter. Splitting a row further for reviewability is expected; do not compress security gates to meet the earlier rough 13–16 PR estimate.

| Phase / PR | Deliverable | Required evidence before advancing |
| --- | --- | --- |
| 1.1 | This design and architecture link; docs only | Whitespace, relative links, private-data scan and review against Q1–Q11. No runtime capability claim. |
| 2.1 | Registry-controlled scoped v2 channel and centralized command/argument scope table beside v1 | Real subprocess: missing/bad token and unknown commands fail closed; downgrade/spoofing denied; Telegram/live-voice v1 regression tests. |
| 2.2 | Distinct client grants, hashed local tokens, CLI lifecycle and bounded audit | Restart recovery; expiry/pause/revoke/rotate; next-request enforcement; concurrent unequal grants; no raw secrets in DB/logs. Local issuance requires owner consent, not OAuth bypass. |
| 2.3 | Delegated provenance through execution/gateways/liaison; presence, reply markers, grant-bound upgrade plans and safe file handoff | Approval-bearing Operator replies grant nothing; forged payload/relay denied; no away exit without presence, including failed sends; cross-client confirmations denied; file escape/race tests. |
| 2.4 | Desk-owned cross-machine subscriptions, remote per-door cursors and per-grant pull mail ([coverage and handoff design](door-mail-coverage.md)) | Two-machine local-transport fixtures, both link directions; restart/reconnect, lost/stale/forged acks, paging, duplicates, desk moves, away/digest single ownership, honest coverage errors. |
| 2.5 | Parity reads/work/file retrieval and explicit administration ledger; table-driven bypass suite. [Fleet visibility/runtime parity](mcp-fleet-visibility.md) adds the 2026-10-27 inventory, coverage contract and small PR sequence, including 2.4c dependencies. | Reads filtered by visibility; custody/admission intact; no arbitrary dispatch/path access; every current operation tested against each preset and raw v2 requests. Multi-machine active/idle/reverse-only ships, remote-only captain mail, separate worker/session/job/task/slot counts; track unfinished parity explicitly. |
| 2.6 | Trusted confirmation runtime for Q6/Q7 (may split by approval category) | Exact artifact/environment or privileged-request binding; distinct captain proof; replay/expiry/revoke/race tests; changed input reapproval; normal delivery checks, terminal hash/sudo restrictions unchanged. Must precede exposing sensitive MCP answers. |
| 3.1 | Opt-in MCP plugin, locked venv, loopback HTTP, both protocol eras, read tools using owner-issued scoped tokens | SDK client conformance for modern/2025 paths, malformed metadata, Host/Origin, auth challenges and unknown versions; fresh bootstrap and reproducible dependency install. |
| 3.2 | 3.2a structured writes/liaison and durable idempotency; 3.2b bounded `send_file` and local captain `get_report` ([design](mcp-file-transfer.md)); admin tools remain | Preset matrix, denied guessing/raw args, duplicate/restart writes, send-vs-completion, grant edits and descriptor-safe uploads. File downloads moved to a later phase: first design ship-authored core-owned publication and immutable captain-mail references, then bounded grant-bound handles/download races. Q6/Q7 gated on 2.6, never emulated by prompts. |
| 4.1 | Core standalone login, AS clients/codes/PKCE/token/refresh/revocation state machine | Wrong verifier/redirect/client/resource, single-use races, refresh reuse, failed-login limits, restart mid-flow, grant consent and expansion. |
| 4.2 | Plugin metadata, login/consent/OAuth endpoints and remote setup | RFC discovery traces, issuer binding, CSRF/redirect attacks, sanitized pages, registration/CIMD SSRF guards, two different client implementations; explicit local-only versus remote UI. |
| 4.3 | Registration compatibility required by actual clients, including DCR if needed | Recorded client/version/registration flows, negative onboarding tests, bounded registration state; do not claim untested interoperability. May combine with 4.2 if small. |
| 4.4 | Optional core-verified OIDC Cargo feature | Local test issuer: bad issuer/signature/audience/nonce/expiry, key rotation, unavailable provider, disabled-feature build; standalone still works and grants remain unchanged. |
| 5.1 | Process/OS hardening; evaluate Landlock and optional separate-user/broker deployment | Real subprocess attempts to read fleet registry/secrets or invoke generic operator CLI denied under the chosen profile; honest Linux/macOS/support limits. Owner setup is explicit, never automatic root work. |
| 5.2 | Adversarial input/resource/recovery/proxy hardening | Bounded bodies/queues/logs/streams, auth brute force and fetch limits, concurrent revoke/refresh/send, restart mid-request, revoked streaming reader, proxy-header abuse, partial remote failure; no wrapper-stop claims. |
| 5.3 | Setup/troubleshooting, compatibility matrix, parity closure and captain-run demonstration | Fresh standalone install; local and remote recipes; account-specific checklist below and the final [ChatGPT text/voice runtime-parity check](mcp-fleet-visibility.md#captain-run-chatgpt-check-last); current gaps stated. Tamper-evident audit remains a separately scoped follow-up. |

After phase 3, local clients can use only the capabilities actually completed and tested with owner-issued tokens. Remote hosted clients require phase 4 authentication plus deliberate HTTPS reachability; no public pilot is implied by a loopback test. Full Captain acceptance is not complete until the applicable parity ledger is closed and sensitive confirmation paths pass.

[#417 item 5](https://github.com/kirkins/privateer/issues/417), remote voice clients with individually revocable credentials, **reuses the core client-grant system** rather than creating another shared-token authority. Pair devices separately, preserve voice's existing read-back/confirmation restrictions, and test independent revoke/rotation. Moving voice to scoped v2 is a distinct migration; this design does not silently alter today's v1 adapter.

## 10. Captain-run real-client checklist (Q5)

Crews supply exact tested versions/configuration and a disposable fleet. The captain keeps all personal account sessions and credentials. Record client/build/date, MCP revision, registration method, endpoint mode, observed result and outstanding caveats; redact screenshots and traces.

1. On a fresh isolated install, enable the door and verify **local-only**, no identity-provider prerequisite and no anonymous fleet reads. Reproduce the hash-locked plugin environment.
2. Exercise local Claude Code and Codex (or record an unsupported tested version), including 2025 compatibility and modern request-scoped tests using a suitable implementation. Keep SDK fixture conformance separate from evidence for a named real client.
3. Deliberately configure HTTPS `public_url` via the captain's chosen tunnel or reverse proxy. Verify TLS, Origin/Host behavior, metadata URLs, issuer/resource binding and no unintended alternate listener. This configures the pilot endpoint, not a production deployment approval.
4. Personally connect ChatGPT and claude.ai where the account/plan supports custom MCP. Record actual discovery, redirect and registration behavior; a client/account limitation is a limitation, not a passing test. Engineering acceptance needs at least two real client implementations; do not disguise simulated clients as account tests.
5. Approve a minimal client, then explicitly expand it. Check displayed client identity, requested ships/scopes, expiry, last use and activity. Confirm a different/new client still requires owner approval.
6. Run Observer and Operator concurrently: reads/mail, ordinary conversation, work assignment/status, scoped files, replies/dismissal. Try hidden tools, raw approval/admin arguments, disallowed ships and liaison escalation. Confirm denies are nonrevealing externally and precise/redacted internally.
7. Grant **Full Captain explicitly** and exercise representative status, conversation, work, mail, file and allowed fleet-admin paths. For production/privileged requests use nonproduction fixtures: verify separate trusted confirmation, exact binding and denial of client-only/self-approval. No real deployment or root action is authorized by this checklist.
8. Revoke/restrict one grant while another is active; confirm prompt enforcement and unaffected peers. Exercise refresh/rotation, expired tokens, restart/reconnect, paging and lost-response retry without duplicate sends.
9. From a door not colocated with the captain desk, verify remote subscription coverage, both link directions, mail/digest delivery, away-preserving sends, duplicate-door policy, lost/stale acknowledgments and recovery after desk/link changes.
10. Disable/revoke the pilot, check bounded redacted audit and cleanup, and record failures/gaps honestly. Preserve existing Telegram/live-voice behavior and do not change a provider route to make a test pass.

## 11. Open implementation items and unverified facts

No Q1–Q11 decision remains open. The following engineering details still need evidence or a reviewed contract refinement:

- Exact supported 2025 revisions, SDK version with dual-era support, Python/platform requirements and locked dependency set. This document does not claim vendor compatibility from product names or an SDK example alone.
- Real ChatGPT/claude.ai/Claude Code/Codex registration, transport, OAuth step-up, redirect and plan/account restrictions. Pre-registration/CIMD/DCR selection must be verified rather than copied from the old draft.
- Exact trusted confirmation surface and captain authentication proof for Q6/Q7; confirm-record storage, expiration, revocation ordering and remote atomic admission. Until built, sensitive MCP answers stay unavailable, not silently weaker.
- Full per-command parity ledger for captain administration, work mutations and bounded file download/report handling. [Fleet visibility/runtime parity](mcp-fleet-visibility.md) now inventories native workers/jobs/capacity, proposed scoped reads/actions and their remaining gaps; it adds no runtime behavior. Missing surfaces are implementation gaps to close, not permanent exclusions.
- Desk subscription wire commands, authenticated cross-machine delegated context, generation/handoff and cursor migration, retention bounds and away-state migration. Section 5 fixes the intended owner/duplicate policy but does not pretend the distributed implementation already exists.
- Core authorization table/API/schema names, token/code lifetimes, rate/size/queue/retention limits and transaction boundaries; connection-versus-device enrollment UX. These must be explicit and tested in their PRs.
- Reviewed Rust OIDC crate, feature/dependency footprint, trusted discovery/key-fetch placement and private-issuer configuration policy. Verification itself is fixed in the core by Q10.
- Hardening profile, platform coverage and whether an optional separate user/broker is warranted. No claim that Landlock alone isolates all same-user authority or that ordinary audit is tamper-proof.

Public source provenance matters: the four #452 comments supply the numbered decisions; cross-machine inclusion is also recorded in the captain's task handoff. The earlier private draft is background only and is not a repository dependency. Its modern-only launch, production prohibition, root-without-separate-confirmation recommendation and administrative ceiling are **not** this contract. Current code also supersedes its unrestricted-file and remote-status descriptions. Implementation PRs must report tested evidence and remaining limitations, and obtain explicit artifact/environment-bound authorization before any production deployment.
