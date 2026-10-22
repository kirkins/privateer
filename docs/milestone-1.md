# Milestone 1: independent peer runtime

> Historical record (noted 2026-10-22). This describes the runtime at the end of milestone 1 and is not kept current; [architecture](architecture.md) describes what exists today. Of the items listed as remaining below, standing grants for ship management (`src/grants.rs`), forge delivery through `pr.merge` (`src/review/pr.rs`, `plugins/forge.py`), cross-home assignment (`ship.move`, cross-ship waits) and front doors (`src/door.rs`, the Telegram plugin) have since shipped.

Status: durable peer runtime with working conversation, activation and crew machinery, under isolated qualification. Built from scratch in Rust; no First Mate/Corsair shell tree or procedural policy was imported. Scripted pilots establish runtime behavior; native fleet autonomy and production replacement require separate evidence.

## Implemented

- Persistent charters and task ownership, accepted handoffs, bounded delegation, and revocation across descendants. Routine corrections preserve standing authority.
- Direct targeted peer messages over bounded local/SSH JSON RPC. The navigator is not a lock or authorization hop for department work.
- Durable event facts, one pending notification per task/issue, idempotent replay, and acknowledgment of only observed facts.
- Detached jobs, shared capacity across homes, disk admission, bounded logs/deadlines, cancellation, explicit retries, owned descendant cleanup, and conservative orphan recovery.
- Typed ship/captain mailboxes, correlated requests and replies, durable delivery between homes, and a per-home resident dispatcher. First mates wake for actionable mail and crew outcomes; focused handoff notes, bearings and warm resume/fresh rotation keep context bounded.
- Runtime-recorded crew outcomes and explicit crew decision/reply reports, plus attended presence and native mail hooks/extensions for selected harness paths. Harness qualification remains specific to each profile and version.
- Optional executable harness/provider plugins and pinned provider selection across execution/retry. Core contains no personal gateway/account/model conventions.
- A local fleet registry and terminal dashboard with real fleet observations, captain inbox/reply flows, explicit ship management and guarded terminal boarding. Broader captain delivery policy and external front-door integrations remain design work.
- Exact-artifact production approval records and current-head delivery evidence checks. Live forge delivery/deployment is not implemented by these checks.

## Evidence

The integration suites exercise task custody, stale acknowledgments, real subprocess supervision/recovery, resource admission, transport/protocol bounds, provider-route enforcement, activation/context, crew reports, attended lifecycle contracts and dashboard state transitions. CI runs formatting, Clippy, the complete suite, the CLI build without dashboard features, isolated dashboard PTY smoke and the runtime pilot on Linux and macOS; Linux also repeats the suite in a crew environment. Consult the exact commit's CI result and retained pilot evidence rather than treating a historical test count as a current qualification.

The three-role runtime pilot completed a real regression repair, meaningful tests, and a local Git commit after the navigator RPC session was killed. Direct department messages worked, repeated alerts retained 50 facts with one pending item, and another home was denied the occupied heavy-work slot.

The [two-ship recovery pilot](recovery-pilot.md) adds fixture first mates, durable cross-home requests, a home-local accepted handoff, a crew decision/reply, a gated supervised repair while its dispatcher is killed, independent regression checks, restart delivery and duplicate/cleanup assertions. It retains command results and binary hashes. Both pilots use deterministic scripts; neither proves model-driven coordination or distributed task custody.

Earlier isolated canary evidence included real two-host SSH RPC, an accepted handoff of a task held by the remote home, targeted inboxes, repeated-event aggregation and remote charter revocation. Temporary test homes were removed. This did not transfer a task between homes or establish cross-host charter inheritance or distributed consensus; rerun the opt-in canary for the exact environment being qualified.

Earlier single-profile native canary evidence included a Codex launch through the plugin/supervisor, a fixture repair, independent tests, a task-linked result and owned-helper cleanup. Provider gateway fixtures exercise exact routing. Such evidence applies to the tested profile and version, and does not qualify every current adapter or a native two-ship fleet. The next [native trial plan](recovery-pilot.md#native-trial-preparation--pending-profile-selection) awaits explicit profile selection; this recovery fixture runs no inference.

## Remaining before replacement of a working fleet

- Qualification of the implemented conversational/activation paths with selected native profiles, interrupted real workloads and sustained use. Mail and crew outcomes drive current activation; arbitrary event-to-task orchestration is not implied.
- Cross-home assignment and repository custody protocols, plus reconnect/reconciliation behavior under real sustained work.
- Channels and captain-created standing grants, complete request-kind schemas, conversation loop limits, quiet hours and broader captain front-door/delivery policy.
- Real forge adapters that obtain live required-check/current-head evidence and perform delivery.
- Qualification of each chosen harness/provider profile and its hooks with representative workloads.
- Reviewed migration preserving current work, notes, profiles, credentials, and rollback paths.

Charters are workflow controls under the current OS account. Plugins and harnesses are trusted executable code. Hard production credential isolation and containment of intentionally escaped processes require an enforced broker/platform boundary. Linux process identity uses kernel start ticks and the boot id, independent of the caller's timezone and locale. macOS uses second-resolution `ps lstart`, rendered in UTC and the C locale.

No live fleet replacement occurred during this milestone. Source publication is separate from production deployment.
