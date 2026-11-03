# Codex attended idle mail (proposal)

Status: PR A binding implementation approved; no delivery or live qualification.
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
for resume); concurrent launches/loaded threads in one cwd remain unbound when
ambiguous. A callback is a candidate, not launch proof. Hook-only binding is the
approved path: daemon listings verify its UUID, never select a binder. Never
pick the newest rollout, a thread name or `resume --last`. Fence stale callbacks
by launch process/boot identity and native UUID creation metadata; a fork needs
a new binding. Exact resume must recover only that UUID's receipt state in B/C.

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
Discover the rendezvous in the expected CODEX_HOME, verify its exact symlink
target, directory/socket ownership and modes, peer process identity, and fence
replacement. Upstream `uds/src/daemon_directory.rs` hardcodes the **physical**
root `/tmp/codex-daemon-<uid>/`, ignoring HOME, TMPDIR and CODEX_HOME. Containment
under the native home is therefore not proof; require the home-derived hash
path and peer credentials. Keep paths short (Unix sockets allow about 108 bytes).
The approved fixture exception permits only its own home-derived hash socket:
fail if it already exists before startup, verify peer PID/UID against the daemon
started by the fixture on every use, and remove only its socket/symlink/lock
after verified process exit, with an idempotent cleanup guard. Never inspect,
connect to, signal or remove other entries in that shared physical directory.

The approved spike proved distinct project hooks **per thread**, but found
SessionStart runs only at first turn entry. Hosted opt-in launches therefore
carry a positional initial bearings prompt, with no `-c`; that turn runs the
stable hook before becoming idle. Manual zero-turn panes, missing trust,
ambiguous binding and failed daemon proof stay honestly **UNBOUND**, without
mail. An app-server-created UUID followed by exact resume is a future
alternative, not an approved automatic binder.

PR A is opt-in via `PRIVATEER_CODEX_ATTENDED=1` on an attended native launch;
it preserves exec/process identity and route argv, creates a private per-project
launch record and merges only its stable SessionStart declaration into
`.codex/hooks.json`. It never changes hook trust. The callback checks the live
pane executable/PID/boot/start identity, hook metadata, exact loaded UUID/cwd,
home-derived socket, daemon peer identity, trusted per-thread declaration and
read-only queue capability. Current implementation is Linux-only and deliberately
fails closed on unsupported argv, paginated loaded-thread listings or more than
32 loaded threads. It emits `delivery_bind` only under current hosted custody.
Binding records explicitly say `delivery_enabled:false`; there is no mail poll,
queue mutation, hosted eligibility change or resume-policy change in A.

Paginated history may not materialize a JSONL rollout even inside SessionStart.
In that case A validates native metadata using `thread/read` and records that
source explicitly; a present rollout must match native session_meta. **This is
not receipt recovery qualification.** B must qualify a durable paginated-history
receipt anchor or refuse delivery for that history mode, rather than pretending
a nonexistent rollout supplies byte offsets.

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
under one private `/var/tmp` root, except the explicitly approved physical socket
layout above. Assert isolation and ownership before every connection. Never
read/copy auth files, use the machine daemon, change routes or kill unowned
processes. A throwaway CODEX_HOME `config.toml` may select a local mock model
provider without a daemon-excluding `-c`; no login or real model calls.

Tests by phase: A proves exact argv/socket connection, daemon death failing
closed, stable hook trust/refusal, distinct per-thread project hooks, stale and
ambiguous callbacks and exact resume-selector validation. B/C must qualify
native resume and fork behavior. B covers burst/low mail,
busy race and lag, unloaded persistent queue, receipt vs ack, crash/lost response,
client-ID reconciliation, saved-offset recovery beyond a tail window, truncation,
pinned uncertainty, pause/K=3 and stable busy edges. Use barriers/injected clocks,
not uptime assumptions, exact audit counts or sleep/retry wrappers. The real
Codex PTY/mock delivery fixture must prove promotion ordering and composer preservation;
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

### PR A feasibility spike (0.166.0, Linux/aarch64)

An isolated owned daemon and two real PTYs established the following; no model
prompt/login/call or delivery was made. This is not live mail qualification.

- Exact pane argv used `codex -C <project> --sandbox workspace-write --add-dir
  <private home> --no-alt-screen`, without daemon-excluding overrides. Neither
  PTY emitted the embedded-server warning. The owned daemon's peer-verified
  WebSocket exposed both exact loaded UUIDs and their distinct cwds; each
  `thread/queue/list` returned an empty queue.
- `hooks/list` for each **loaded thread** returned only that project's stable
  SessionStart declaration, enabled and trusted with its persisted declaration
  hash. Fixture-only trust simulated `/hooks` approval in its throwaway home.
  This proves per-thread discovery/trust, not callback execution.
- Neither startup callback ran before a first turn; `thread/read` reported idle
  with zero turns and `historyMode:paginated`. Its reported rollout path did not
  yet exist. Upstream `core/src/session/session.rs` queues a pending SessionStart
  source; `core/src/session/turn.rs::run_pending_session_start_hooks` executes it
  at turn entry. Startup binding cannot assume a hook or transcript exists for
  a never-prompted pane.
- The physical socket layout required the narrow exception above. The transport
  is **WebSocket JSON-RPC over Unix**, not newline JSON directly on the socket.
  The fixture daemon was stopped, its process exit verified, and only its owned
  socket/symlink/lock were cleaned. The machine daemon was not contacted.

Decision received: keep the stable hook as the only binder; require a positional
initial bearings prompt for hosted launches, leave zero-turn manual panes
unbound, and permit a fixture-only `continue:false` hook. A subsequent two-PTY
probe ran both distinct project callbacks and stopped at the hook before any
provider request. See upstream `core/src/session/session.rs:2028–2056` and
`core/src/session/turn.rs:342` at the pinned revision for the deferred hook.
Owned probe processes and temporary state have been cleaned.

The reproducible binding-only fixture is
[`scripts/codex-binding-spike.py`](../../scripts/codex-binding-spike.py); its
blocking hook is fixture-only, never installed by production. It tests actual
`delivery_bind`, distinct projects, untrusted/unbound behavior and daemon-death
refusal without mail delivery. Unit coverage lives in
[`tests/codex_attended_test.py`](../../tests/codex_attended_test.py), dispatched
by `tests/python_suites.rs`. Exact-UUID restart, draft preservation and durable
receipt behavior remain B/C acceptance work, not claims of this spike.
