# Milestone 1: independent peer runtime

Status: working first milestone, ready for review before a fleet trial. Built from scratch in Rust; no First Mate/Corsair shell tree or procedural policy was imported.

## Implemented

- Persistent charters and task ownership, accepted handoffs, bounded delegation, and revocation across descendants. Routine corrections preserve standing authority.
- Direct targeted peer messages over bounded local/SSH JSON RPC. The navigator is not a lock or authorization hop for department work.
- Durable event facts, one pending notification per task/issue, idempotent replay, and acknowledgment of only observed facts.
- Detached jobs, shared capacity across homes, disk admission, bounded logs/deadlines, cancellation, explicit retries, owned descendant cleanup, and conservative orphan recovery.
- Optional executable plugins, pinned provider selection across execution/retry, native Codex/Pi plans, and an optional gateway-constrained Pi plugin. Core contains no personal gateway/account/model conventions.
- Exact-artifact production approval records and current-head delivery evidence checks. Live forge delivery/deployment is not implemented by these checks.

## Evidence

The suite contains **42 integration tests**: 14 core, 13 real subprocess/job, 7 peer transport, 8 plugin cases. On 14 October 2026, the first 41 passed on macOS and Linux; the additional default-charter production approval regression passed locally. Formatting and Clippy with warnings denied passed on macOS. CI repeats formatting, Clippy, the complete test suite, and the scripted runtime pilot on Linux and macOS.

The three-role runtime pilot completed a real regression repair, meaningful tests, and a local Git commit after the navigator RPC session was killed. Direct department messages worked, repeated alerts retained 50 facts with one pending item, and another home was denied the occupied heavy-work slot.

A real two-host SSH canary exercised correlated RPC, accepted custody transfer, targeted inboxes, 20 facts with one pending issue, and remote charter revocation. Temporary test homes were removed. It did not establish cross-host charter inheritance or distributed consensus.

A real native Codex canary launched through the plugin and supervisor, repaired the fixture, passed independent tests, recorded a task-linked result, and completed with owned helpers cleaned up. Provider gateway tests exercised exact routing against fixtures; a separate private local preflight validated an existing Pi configuration without copying credentials or making a GLM inference request.

## Remaining before replacement of a working fleet

- Persistent conversational UX and automatic task/event activation of native department agents.
- Cross-host assignment and repository custody, reconnect/reconciliation behavior under real sustained work.
- Real forge adapters that obtain live required-check/current-head evidence and perform delivery.
- Qualification of each chosen harness/provider profile and its hooks with representative workloads.
- Reviewed migration preserving current work, notes, profiles, credentials, and rollback paths.

Charters are workflow controls under the current OS account. Plugins and harnesses are trusted executable code. Hard production credential isolation and containment of intentionally escaped processes require an enforced broker/platform boundary. Linux process identity uses kernel start ticks and the boot id, independent of the caller's timezone and locale. macOS uses second-resolution `ps lstart`, rendered in UTC and the C locale.

No live fleet replacement occurred during this milestone. Source publication is separate from production deployment.
