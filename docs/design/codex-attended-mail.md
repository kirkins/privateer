# Codex attended idle mail (proposal)

Status: design only; first-mate approval required before implementation.
Goal: an explicitly launched Codex first-mate pane handles idle mail without
terminal injection. This is attended-mail qualification, not full autonomy.

## Boundary and identity

Add a small optional `plugins/attended-codex` adapter, launched and supervised
by the existing presence wrapper in `src/cli.rs`. Keep policy, mail and hosted
watchdog decisions in Rust. Do not modify crew/job resume, process supervision,
door readers or delegated-machine code. Preserve native model/provider,
permissions and credentials; never edit user Codex configuration.

Bind `(Privateer home, ship, holder, launch generation, CODEX_HOME, thread UUID,
canonical cwd, transcript identity)` before polling mail. The existing
[`session_started`](../../plugins/native_events.py) decoder handles headless
`thread.started`; interactive terminal output is not that stream. Use native
`SessionStart` hook input (`session_id`, `cwd`, `transcript_path`, `source`) to
record the interactive UUID instead. Install only launch-local hook configuration,
with an absolute callback/config path and a fresh launch nonce embedded in the
hook command: a shared daemon must not depend on inherited per-pane environment.
The callback writes an atomic, private binding for its wrapper; validate native
`session_meta`, UUID, workspace, ownership and regular non-symlink transcript
before use. Never select the newest transcript, a thread name, or `resume --last`.

Respect hook trust and managed restrictions: do not auto-trust hooks, replace
existing hooks or write global settings. Missing/untrusted startup hooks leave
a clear unbound diagnostic and no automatic mail. An explicit local bind may
supply the exact UUID, but must pass the same native/workspace validation.
Rebinding invalidates old callbacks and in-flight results; a fork is a new UUID.
Exact resume recovers that UUID's receipt state, not another conversation's.
Proving launch-local hook configuration and callback provenance on the shared
daemon is the first no-model implementation spike; stop if it requires a global
config change or cannot distinguish concurrent panes.

## Delivery, receipts and failure

Poll `ship.presence` and `mail.pending` on the existing five-second cadence,
single flight with bounded subprocess time/output. Recheck the exact holder and
binding before admission. Coalesce arrivals into one outstanding notification:
at most six headers, no bodies, inbox paging argv, correlation/reply/ack guidance,
and charter/deployment reminder, as in
[`attended/mail.mjs`](../../plugins/attended/mail.mjs). Include an exact receipt
marker with home, ship, UUID, sequence range and notification nonce. Low notices
alone never start inference; they remain available through the normal inbox.

Persist intent before calling explicit argv:
`[codex, queue, --thread, UUID, --message, BODY]`, using the launch's executable
and native home/local daemon. Do not pass `--resume`, remote options, provider,
model or permission overrides. Queueing must use the pane's shared daemon, never
an embedded replacement. Refuse unloaded/mismatched threads. A busy turn is not
interrupted: retain one coalesced pending range and normally defer admission
until idle; an idle-to-busy race is safely held by Codex's native queue. Later
arrivals remain durable for the next range. Queueing does not edit the composer;
draft preservation is a required live test, not an assumption of a blank draft.

A successful CLI response means admission only. Advance presentation only from
native promoted user input containing the exact marker, associated with its
actual turn start, or explicit mailbox acknowledgement. Never use queue text,
assistant prose, queue removal or process exit as presentation. Recover receipts
and turn state from a validated, bounded native rollout tail (initial bounds:
128 records / 1 MiB) plus a transactional per-session Privateer cache (128-session
index). Keep unconfirmed intent pinned rather than evicting it to meet the cap;
backpressure if no safe eviction exists. Rotate/reopen checks must preserve file
identity/provenance. Prove the native receipt/turn ordering in subprocess tests
before enabling automatic delivery; missing/truncated/unknown evidence pauses,
not an invented idle state. Notification never performs `mail.ack`.

Important: Codex 0.166.0's queue CLI generates a new client message UUID on each
invocation. Lost response, timeout or crash after intent therefore enters a
durable **uncertain** state. Reconcile the exact marker against native queue
records (read-only daemon query if needed) and promoted receipts; do not blindly
invoke the CLI again. If bounded evidence cannot settle it, remain paused and
require explicit local reconciliation/resume, warning about possible replay.
Human pause and K=3 exhaustion never auto-clear. Native errors/interruptions or
confirmed rejection pause; hosted sessions may retry a *proved unadmitted* offer
after one minute, at most three declines. Background mail cannot clear pause.
A new captain prompt can recover an ordinary native-stop pause, not uncertainty.
Cleanup fences generations and stops only owned probes; it never stops the
shared daemon or claims an admitted native turn was cancelled.

## Runtime reporting and compatibility

Use the existing [`wake.hosted`](../../src/wake_reason.rs) contract, unchanged:
`delivery_bind` after validated binding; `delivery` carries session,
`accepted_through`, pause/reason, decline count capped at 3, and actual busy state
with one stable `turn_started_ms` per turn. Only a promoted native receipt sends
`trigger: mail` with its range. Queue admission and lease renewal are not progress.
Native terminal/error records clear busy; observation loss cannot fabricate a
settled turn. Preserve the existing watchdog's K=3, five-minute blocked spell and
30-minute busy bound in [`host/delivery.rs`](../../src/host/delivery.rs), with no
new timeout policy. Report attention only when native context/window evidence
exists, never estimate usage. Add Codex to hosted eligibility only after these
binding, receipt and busy reports are proven. Scheduling, compaction, fleet slash
commands and follow-through parity are not claimed by this mail adapter.

Baseline/minimum: codex-cli **0.166.0**, upstream `rust-v0.166.0` (`6ff1e1c`).
Check `queue --help` flags, shared-daemon availability, exact loaded thread and
recognized hook/rollout schemas; version alone cannot qualify an older daemon.
Read-only queue capability detection should precede delivery where available;
unsupported method/experimental-policy errors fail closed and report unsupported.
Do not enqueue a probe into a real conversation. Surface **experimental queue
API** at launch and in delivery diagnostics/docs; no silent `turn/start` fallback.
Recommend refusing `--no-daemon` for automatic mail (manual inbox remains usable).
A Stop hook cannot wake an already-idle pane and its timeout/trust dependency is
not an equivalent fallback. Reject ephemeral sessions for restart qualification.
Keep diagnostics in wrapper/CLI surfaces; do not promise unsupported TUI footers
or `/privateer-mail` registration. Any new dispatched controls need ACCEPTED lists.

The wrapper runs outside Codex's tool sandbox; queue/socket observation must not
require a model tool to connect. Linux `workspace-write` still cannot gain Unix
socket access through a path grant alone, including access to a Privateer MCP
broker. Test the selected sandbox's actual file/reply/ack path, report refusals,
and never enable networking or loosen sandbox/permissions as an adapter fallback.
The existing bundled Codex crew sandbox default is not a new attended grant.

## Evidence and acceptance

All fixtures/probes use one private directory under `/var/tmp` containing HOME,
CODEX_HOME, PRIVATEER_FLEET, PRIVATEER_RESOURCE_HOME, TMPDIR and all XDG homes;
fixture daemon socket discovery must resolve inside it. Never read auth files,
copy credentials, contact the machine daemon or change provider routes.

1. Unit/state-machine and real Privateer mailbox tests: burst/low notices,
   single flight, busy race, stale holder/generation, bind/fork/resume, native
   receipt versus ack, crash-before/after-queue, lost response, queue rejection,
   pinned uncertain intent, bounded recovery, pause/K=3 and stable busy edges.
   Use injected clocks/barriers, not sleeps, exact audit counts or uptime.
2. A real Codex subprocess/PTY fixture, with throwaway home and owned daemon,
   verifies hook binding/trust refusal, CLI parsing, shared-daemon queueing,
   busy-to-idle promotion, native receipt ordering, exact resume and teardown.
   Use a local deterministic protocol server without login/real model use where
   needed; distinguish this from a live model qualification. Assert socket
   isolation before launch, and never kill an unowned daemon.
3. After implementation approval, run the assigned serial admitted heavy gate
   (fmt, all-target/all-feature clippy, lib, affected integration binaries,
   bytecode guard, build), then commit-bound `verify.run`. Do not touch MCP/door
   paths unless separately approved; if touched, add all four required MCP suites.
4. Separately request captain authorization through the first mate for the exact
   isolated artifact/environment and any real login/model use. Pilot: idle mail
   causes a real file/reply/ack turn, another arrival preserves a typed draft,
   busy mail waits, and exact-UUID restart does not replay a delivered but
   deliberately unacknowledged request. Record version, OS/arch, route evidence,
   receipts, mailbox/file results and owned-process cleanup without credentials.
   Update the attended support table only with evidence actually obtained.

Design evidence: repository contracts above and read-only upstream inspection at
`6ff1e1c`: `hooks/schema/generated/session-start.command.input.schema.json`,
`cli/src/queue_cmd.rs`, `tui/src/session_queue_commands.rs`, and
`app-server/src/request_processors/thread_queue_processor.rs` under `codex-rs`.
No runtime tests, model calls, implementation or live qualification in this round.
