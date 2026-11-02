# Codex attended idle mail (proposal)

Status: revised design only; awaiting first-mate approval before code or probes.
Goal: an explicitly launched Codex first-mate pane handles idle mail without
terminal injection. This qualifies attended mail, not full native autonomy.

## Binding and daemon boundary (PR A)

Add an optional `plugins/attended-codex` adapter supervised by the presence
wrapper in `src/cli.rs`. Keep mailbox/watchdog policy in Rust; do not modify
crew/job resume, process supervision, door readers or delegated-machine code.
Preserve native routes, permissions and credentials; never edit global config.

Bind `(Privateer home, ship, holder, launch generation, CODEX_HOME, thread UUID,
canonical cwd, transcript identity, verified daemon socket/process identity)`.
The existing [`session_started`](../../plugins/native_events.py) decoder handles
headless `thread.started`, not interactive terminal output. Prefer a **stable**
`SessionStart` callback declared in the ship folder's trusted `.codex` project
layer, explicitly installed without replacing existing hooks and trusted once
through `/hooks` (like Cursor project hooks). Hook trust hashes the declaration,
including command text: no launch nonce or changing path in that command.
Never auto-trust or bypass trust.

The daemon cannot rely on inherited per-pane environment. The stable callback
reads a private launch registry; validate its `session_id`, `cwd`,
`transcript_path`, `source` and native transcript `session_meta` against the
launch record. Validate ownership, non-symlink regular transcript, UUID and
workspace. Bind only an unambiguous matching launch (including the exact UUID
for resume); concurrent fresh launches in one cwd must remain unbound if their
metadata cannot distinguish them. A callback is a candidate, not launch proof.
An explicit local bind of an exact UUID is the alternative, with the same
transcript, launch and daemon checks. Never pick the newest rollout, a thread
name or `resume --last`. Fence stale callbacks/results by generation; a fork
needs a new binding. Exact resume recovers only that UUID's receipt state.

Upstream `tui/src/daemon_startup.rs::config_exclusion` excludes the shared daemon
for **any `-c` override except `features.*` and `tui.fullscreen_transcript`**, a
custom config loader, `--profile`, `--strict-config`, or
`--dangerously-bypass-hook-trust`. `--no-daemon` also refuses automatic mail.
In particular the existing route `web_search` option emits `-c web_search=…`;
reject that attended plan with a diagnostic, never remove the option or change
the route silently. Do not inject hook configuration through `-c`.

Before `wake.hosted` / `delivery_bind`, prove the pane's **exact argv** uses the
shared daemon: no `Running without the shared background server` warning,
verified connection to its socket, matching same-user daemon process identity,
and a read-only `thread/queue/list` capability probe for the exact bound UUID.
Use native socket discovery only within the expected home, verify directory
and socket ownership/modes and peer process identity, and fence socket/process
replacement. Private fixtures use a short socket path under their private
`/var/tmp` root (Unix socket paths are limited to about 108 bytes).

First approved spike: does this shared daemon actually apply project-layer
hooks **per thread**, including distinct project folders and resume? Use an
isolated real Codex PTY/daemon fixture, stable hook and no delivery. If hooks
are ignored/shared incorrectly, stop and request a decision before further
implementation; do not patch around this with daemon-excluding flags. Missing
trust, ambiguous binding or failed socket proof leaves a clear unbound state.

## Delivery and recovery (PR B)

Poll `ship.presence` / `mail.pending` on the existing five-second cadence, single
flight with bounded subprocess/RPC time and output. Recheck holder, generation,
UUID and daemon before admission. Coalesce into one outstanding notification:
at most six headers, no bodies, inbox paging argv, correlation/reply/ack guidance
and charter/deployment reminder, as in
[`attended/mail.mjs`](../../plugins/attended/mail.mjs). Include home, ship, UUID,
sequence range and a unique notification nonce in an exact receipt marker.
Low notices alone never start inference. Notification never performs `mail.ack`.

Persist intent, nonce, rollout file identity and **byte offset** transactionally
before sending. Prefer `thread/queue/add` directly over the verified daemon's
Unix JSON-RPC transport, setting `client_user_message_id` to that nonce for exact
reconciliation. The CLI alternative is explicit argv
`[codex, queue, --remote, unix://<verified socket>, --thread, UUID, --message, BODY]`.
**Always pass `--remote`** on CLI delivery: without it Codex may silently create
an embedded server and accept unloaded threads when the daemon is gone. No
provider/model/permission overrides, embedded fallback or `turn/start` fallback.
The CLI generates a fresh client ID, so its marker must also be reconciled.

Queued items persist on unloaded threads. Before admitting new mail, reconcile
all of this adapter's queued markers/client IDs; never assume absence of a
loaded thread discarded its queue. Require the bound thread loaded on the
verified daemon for admission. Busy evidence comes from recognized native
rollout turn-start/end/abort events. If rollout writes lag, use validated daemon
thread lifecycle/status notifications and a supported read-only status snapshot
to re-establish state; capability/schema failure pauses admission as unknown,
not idle. Prove the exact event mapping in the fixture before enabling delivery.
Normally defer while busy; the idle-to-busy race is held by Codex's native queue,
never an interruption. Later arrivals remain durable for the next range.

Admission success is not presentation. Advance presentation only on promoted
native user input with the exact marker associated with its actual turn start,
or an explicit mailbox ack covering the range. Queue text/removal, assistant
prose and process exit are not receipts. Scan incrementally from the saved
intent offset, in bounded batches, never a last-N-records window. Persist the
scan checkpoint; validate rotation/replacement and pause on lost evidence.
Bound the session index (initially 128) and read batches (1 MiB); pin unresolved
intents and apply backpressure instead of evicting facts.

Timeout, disconnect or crash after intent means durable **uncertain**, not a
rejection or permission to resend. Reconcile through read-only
`thread/queue/list` on the verified socket (`client_user_message_id`), exact
queued markers, promoted receipts from the anchored offset and mailbox ack.
Queue presence means still pending; queue absence alone cannot prove rejection.
An explicit ack settling the uncertain range is the normal self-healing path;
never auto-ack to get unstuck. Uncertainty pauses delivery with `paused:true`,
`decline_reason:other` in `wake.hosted`, leaving decline count unchanged.

Add dispatched control **`attended.codex.reconcile`**, with an ACCEPTED entry for
`[ship, holder, session, notification_id]`. It validates current custody and
binding and re-runs the same evidence-based reconciliation; it cannot force
acceptance, discard an uncertain intent or blindly replay. Return evidence and
`pending`, `settled` or `uncertain`; unresolved uncertainty stays paused.
Human pause and K=3 exhaustion never auto-clear. Native errors/interruptions or
confirmed rejection pause; hosted sessions may retry a *proved unadmitted*
offer after one minute, at most three declines. Background mail cannot clear
pause. A captain prompt may recover an ordinary native-stop pause, not uncertain
admission. Cleanup fences generations and stops only owned probes, not the
shared daemon or an already admitted native turn.

## Runtime reporting and hosted qualification (PRs B/C)

Use [`wake.hosted`](../../src/wake_reason.rs) unchanged: validated
`delivery_bind`; `delivery` reports session, `accepted_through`, pause/reason,
declines capped at 3 and actual busy state with one stable `turn_started_ms` per
turn. Only promoted native mail receipts send `trigger:mail` with the range.
Queue admission and lease renewal are not progress. End/error evidence clears
busy; observation loss cannot fabricate a settled turn. Preserve K=3, the
five-minute blocked spell and the 30-minute busy bound in
[`host/delivery.rs`](../../src/host/delivery.rs). Report attention only from
native context/window evidence, never estimated usage.

Research baseline is codex-cli **0.166.0**, `rust-v0.166.0` (`6ff1e1c`), not a
version-only gate: the host can have a newer CLI and an older 0.162 daemon.
Probe the **daemon itself** with read-only `thread/queue/list`, validate schemas,
loaded UUID and hooks, and refuse unsupported/experimental-policy errors.
Never enqueue a capability probe. Surface **experimental queue API** in launch
and delivery diagnostics. Refuse ephemeral sessions for restart qualification;
`--no-daemon` has no automatic-mail fallback (a Stop hook cannot wake idle).

PR C alone adds hosted eligibility after binding/receipt/busy proof. Hosted
relaunch must use `codex resume <bound UUID>`, never a fresh session or picker;
request separate approval if this requires Codex-owned resume paths. Scheduled
work still displays the existing **no responsive hosted work adapter** fault;
work delivery is a follow-up, not mail parity. Scheduling, compaction, fleet
slash commands and follow-through are not claimed. No invented TUI footer or
`/privateer-mail` command. Update the README support table only after live proof.

The wrapper connects outside the tool sandbox. Linux `workspace-write` cannot
permit Unix socket connects by a per-path grant, including a Privateer MCP
broker; network access is needed. Test the selected sandbox's file/reply/ack
path without enabling networking or loosening permissions as a fallback. The
bundled Codex crew default is not a new attended grant.

## Implementation split and evidence

- **A:** revised binding, stable hook, socket verification, `delivery_bind` and
  daemon-proof fixture only; no queue delivery, hosted eligibility or README
  claim. Target at most about 800 production lines, one implementation commit.
  Stop for a decision if the per-thread hook spike fails.
- **B:** delivery state machine, exact reconciliation/control, anchored receipts,
  `wake.hosted` reporting and deterministic mock-provider subprocess tests.
- **C:** hosted eligibility, exact-UUID relaunch and separately authorized live
  pilot, then the evidence-backed README table.

All probes/tests isolate HOME, CODEX_HOME, PRIVATEER_FLEET,
PRIVATEER_RESOURCE_HOME, TMPDIR and XDG_STATE_HOME/XDG_DATA_HOME/XDG_CONFIG_HOME
under one private `/var/tmp` root. Assert socket isolation before launch. Never
read/copy auth files, use the machine daemon, change routes or kill unowned
processes. A throwaway CODEX_HOME `config.toml` may select a local mock model
provider without a daemon-excluding `-c`; no login or real model calls.

Tests by phase: A proves exact argv/socket connection, daemon death failing
closed, stable hook trust/refusal, distinct per-thread project hooks, stale and
ambiguous callbacks, explicit binding and resume. B covers burst/low mail,
busy race and lag, unloaded persistent queue, receipt vs ack, crash/lost response,
client-ID reconciliation, saved-offset recovery beyond a tail window, truncation,
pinned uncertainty, pause/K=3 and stable busy edges. Use barriers/injected clocks,
not uptime assumptions, exact audit counts or sleep/retry wrappers. The real
Codex PTY/mock fixture proves promotion ordering and composer preservation;
this is not live model qualification.

After implementation approval: one serial admitted heavy gate with
CARGO_BUILD_JOBS=1 and RUST_TEST_THREADS=1 (fmt, all-target/all-feature clippy,
lib, affected integration binaries, bytecode guard, build), then commit-bound
`verify.run`; park on each admitted job. MCP/door changes require separate scope
approval and their four extra suites. No heavy gate is claimed for this doc.

For C, request captain approval through the first mate for the exact isolated
artifact/environment and any real login/model use. Pilot: idle mail becomes a
real file/reply/ack turn, another arrival preserves a typed draft, busy mail
waits, and exact-UUID restart does not replay a delivered but deliberately
unacknowledged request. Record CLI/daemon versions, OS/arch, route evidence,
receipts, file/mail results and owned-process cleanup without credentials.

Evidence basis: repository contracts linked above and first-mate/Opus upstream
review at `6ff1e1c`: `tui/src/daemon_startup.rs`,
`hooks/src/engine/discovery.rs::hook_hash`, SessionStart schema,
`cli/src/queue_cmd.rs`, `ext/queue/src/service.rs`, and
`app-server/src/request_processors/thread_queue_processor.rs` under `codex-rs`.
No runtime probes, implementation, model calls or live qualification this round.
