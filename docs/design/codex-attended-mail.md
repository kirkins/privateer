# Codex attended idle mail (proposal)

Status: PR A merged; B0 binding hardening approved, exact-UUID resume only.
No delivery or live qualification. Following Opus's conditional go and the first
mate's approved split, B1 implements only the pre-lock poison prerequisite and
known-B0 dispatcher migration. B2 adds a dormant materialized receipt ledger
and opt-in queue-promoter fixture. B3a fixes its review findings; B3b fixes
rejected/abandoned history growth and adds negative attachment/receipt qualification.
B4 addresses nonce-capacity diagnostics, qualifies one accepted queue input against
an isolated loopback mock (captain-approved scope change), and proposes a
Privateer-owned attended-pane proof for review, **design only**.
B5a fixes the B4 review findings below. The first mate approved separate B5b
custody and B5c dormant-runner slices. B5b now implements the Rust custody
prerequisite and PR700 receipt carry-ins below. B5c adds a dormant, unconnected
queue consumer and conservative local accounting, described below. **ACT1**
(activation, Linux only, below) wires that runner into the hosted supervisor
behind a captain-only grant with an explicit possibly-unattended turn budget.
No code grants delivery by itself. **ACT2** (below) adds receipt-filter
rollover, operator reconcile/resolve controls and operator-created exact
sessions; real-PTY draft/race/second-client qualification is ACT3; live
qualification is PR C. The fresh reservation decision below was design only;
ACT2 implements its create-then-resume protocol as an operator control.
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
B0 uses a version-independent project-local `privateer-attended/hook.py`
dispatcher, invoked with isolated `/usr/bin/python3 -I -B`; refuse if that fixed
system interpreter is unavailable or older than Python 3.9 (checked by an isolated,
bounded prepare-time subprocess), never embed a version-managed interpreter.
The private launch record selects the current release handler. Prepare replaces
only this project's exact earlier isolated dispatcher declarations (any interpreter,
`-I -B`, this exact `hook.py`) with one declaration; unrelated hooks and trust are
untouched. Only trailing duplicate declarations may be removed: if removing one
would shift a later retained declaration in the same event, prepare refuses for
native operator review. This includes unrelated hooks in the same or a later group. Moving to the system interpreter changes the trust hash and requires
explicit native `/hooks` re-trust. Never auto-trust or bypass trust. Any existing
PR A versioned declaration naming this registry refuses prepare, pending explicit
operator review/removal, not automatic deletion or trust migration. The isolation
flag changes the command's trust hash; it must land before the stable hook is trusted.

The daemon cannot rely on inherited per-pane environment. The stable callback
reads a private launch registry; validate its `session_id`, `cwd`,
`transcript_path`, `source` and native transcript `session_meta` against the
launch record. Validate ownership, non-symlink regular transcript, UUID and
workspace. Bind only an unambiguous matching launch (including the exact UUID
for resume); concurrent launches/loaded threads in one cwd remain unbound when
ambiguous. A callback is a candidate, not launch proof. **B0 accepts only explicit
`codex resume <UUID>` launches** and resume/compact callbacks matching that
immutable launch UUID. A different trusted same-folder TUI can fire before the
pane has loaded its own thread: unique cwd, UUID creation time, hook trust and
native metadata cannot close this first-callback race. Refuse fresh launches and
startup/fork/clear callbacks rather than binding an unproven UUID. Daemon listings
verify an explicit UUID, never select a binder. Never pick the newest rollout, a
thread name or `resume --last`. Persist unbound/delivery-disabled state before
release-handler loading, callback validation or daemon connection; preserve the
last good binding in `last_binding` solely to validate compact continuity across
overlapping callbacks; delivery must never read it as an active binding. A new
launch discards that continuity. The dispatcher
invalidates even if the selected release is missing or unsafe. State locks cover
only short local transactions, with bounded waits, never daemon/report calls. An
invalidation advances the callback generation; the final bind compares it under
the same lock so an older in-flight resume cannot overwrite a newer invalidation. Exact resume must recover only that
UUID's receipt state in B/C. App-server UUID reservation followed by exact TUI
resume requires a separate design review before it can enable fresh attended
launches or delivery.

Upstream `tui/src/daemon_startup.rs::config_exclusion` excludes the shared daemon
for **any `-c` override except `features.*` and `tui.fullscreen_transcript`**, a
custom config loader, `--profile`, `--strict-config`, or
`--dangerously-bypass-hook-trust`. `--no-daemon` also refuses automatic mail.
**ACT3a exception:** hosted exact-UUID effort launches now use an adapter-generated,
peer-verified explicit `--remote` endpoint, not implicit daemon selection; see the
ACT3a section below. No caller-selected remote or general config override is allowed.
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
SessionStart runs only at first turn entry. Hosted opt-in **exact-resume** launches
therefore carry a positional initial bearings prompt, with no `-c`; that turn
runs the stable hook before becoming idle. Manual zero-turn resume panes,
missing trust, ambiguous binding and failed daemon proof stay honestly
**UNBOUND**, without mail. An app-server-created UUID followed by exact resume
is a future alternative, not an approved automatic binder.

Binding is opt-in via `PRIVATEER_CODEX_ATTENDED=1` on an attended native launch;
B0 additionally requires explicit exact-UUID resume. It preserves exec/process
identity and route argv, creates a private per-project launch record and merges
only its stable SessionStart declaration into `.codex/hooks.json`. It never
changes hook trust. Prepare-time refusals abort the opted-in launch with a public
reason; callback-time failures instead leave its running pane unbound. Report
subprocesses use only allowlisted launch-state paths and fixed role/actor/home,
never inherited daemon credentials, actor/job identity or loader overrides. The callback checks the live
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

## Fresh launch reservation decision (2026-11-03, design only)

### Required proof and options

A fresh launch needs a UUID fixed **before its first callback**, independently of
which same-folder pane happens to load or call back first. The proof must be a
private launcher-owned reservation result plus an exact native attach selector;
the callback only verifies that selector. Project hook trust authenticates a
command, not a pane. Neither UUID age, loaded cwd, rollout discovery, session
name, `resume --last`, nor a stopped seed turn discovered by cwd is a reservation.
The existing spike's fixture-only seed procedure is explicitly not a production
binder.

| Option | Proof / cost | Recommendation on 0.166 |
| --- | --- | --- |
| Ordinary fresh TUI, choose its first matching callback | No pane-to-UUID proof; same-folder first-callback race persists even if only one thread is loaded | Reject |
| `thread/start` on the verified shared daemon, save its returned UUID, then launch `codex resume <UUID>` | Causally exact UUID, no cwd selection; requires a durable, zero-turn, resumable thread and identical native settings at attach | Candidate only; not qualified for the default paginated mode |
| Seed a turn to make a reserved thread resumable | Risks inference before attended launch, persistent synthetic history and hook/policy changes; fixture stopper is not a native reservation API | Reject as a production workaround |
| Native durable reserve/create-and-attach API, or authenticated per-client launch token carried in SessionStart | Can establish causality without inference and survive a launcher crash; requires upstream support/qualification | Preferred route to fresh support |
| Existing durable session explicitly selected by UUID | Already has immutable argv proof; does not claim a fresh conversation | Keep B0 behavior for the next stage |

**Current decision:** no safe, qualified general fresh-attended protocol is
established on Codex 0.166. In particular, the existing fixture found that an
unprompted paginated thread had no durable history and could not be resumed;
`thread/start` returning a UUID is not proof of durable attachability. This is a
specific missing contract, not a claim that every history mode or future native
API is impossible. Keep fresh launch refusal. Do not quietly switch history
modes, seed a model turn, alter route/config, patch the TUI, or use a model call
to complete the reservation. A legacy-history create/resume experiment would
need separate isolated qualification and explicit route/settings equivalence;
it cannot silently become the default.

### Exact reservation protocol to qualify upstream

The needed native operation creates a non-ephemeral, durably resumable **empty**
thread on the peer-verified shared daemon without starting a turn. It returns
its UUID and a durable reservation identity tied to the requesting client. The
same native route, model, sandbox/approval policy, cwd and history contract must
be retained at attach; unknown or incompatible settings refuse, not default to
another route. An alternative upstream feature could let the launcher provide a
one-use UUID/token to native TUI creation, echoed in the trusted callback and
bound by the daemon to that client. A forgeable project environment value or a
new `-c` override is not that feature.

For create-then-resume, the proposed transaction is:

1. Under the project lock, claim a new launch generation, holder and process
   identity; save `reserving`, delivery disabled. Refuse any live owner. Release
   the lock for the verified-daemon create request. No mailbox admission yet.
2. Save the returned UUID/reservation receipt with that generation atomically
   and durably **before exec or any callback**; compare generation/holder again.
   Read only that UUID to verify durable empty-thread metadata and attachability.
   A crash/lost create response leaves an unknown/orphan reservation; never find
   it by cwd/newness, attach to a guess, or create/seed repeatedly on a timer.
   Native client-ID lookup/idempotency would allow exact reservation recovery;
   without it stop and leave the orphan alone for operator/native cleanup.
3. Exec the ordinary shared-daemon TUI with exact `resume <reserved UUID>` plus
   the original permitted native options and positional bearings prompt. A
   failed attach aborts rather than falling back to fresh. Preserve exec process
   identity as B0 does. Do not use TUI `--remote`, which B0 excludes.
4. Only a trusted resume callback for that immutable UUID can finish binding;
   recheck launch/callback generations, current custody, peer identity, loaded
   native metadata and history/receipt capabilities. The first bearings turn is
   an ordinary explicitly launched turn, not a hidden pre-attach seed. A callback
   for another UUID invalidates and refuses; it cannot consume the reservation.

This requires fixture proof of no pre-attach turn/provider request, zero-turn
restart/attach, same-folder competing callbacks, crashes on both sides of the
reservation commit, daemon replacement and exact settings retention. No such
proof is claimed here, and no reservation code is authorized by this document.

### Fork, clear, compact and restart

- **Fork / clear:** either changes the native conversation identity or uses an
  unproven callback source. Invalidate to unbound first, advance
  `callback_generation`, disable delivery and retain all old outstanding intents
  under their original UUID. Never migrate receipts, trust a fresh callback, or
  copy the parent's accepted watermark to a child. Relaunch with an explicitly
  selected durable child UUID, or use a separately qualified native reservation
  handoff. Pending queue items remain the old UUID's facts; no automatic removal,
  forwarding or inference on the old thread.
- **Compact:** same UUID only, matching the last verified binding, as B0 already
  requires. It advances callback generation and temporarily invalidates delivery.
  Re-enable only after full validation **and** receipt-anchor continuity. A
  compaction that prunes/revises receipt history pauses B until reconciled; it
  never resets the presented watermark or grants a fresh inference budget.
- **Restart:** explicit same UUID, new launch generation and current holder.
  Restore only that UUID's durable receipt ledger; an old generation cannot
  publish reports or send. Unresolved old offers block new ones until evidence
  settles them. Nothing in this design changes Codex-owned Rust resume paths.

## Delivery and recovery (PR B)

Poll `ship.presence` / `mail.pending` on the existing five-second cadence, single
flight with bounded subprocess/RPC time and output. Recheck holder, generation,
UUID and daemon before admission. Immediately before **each** `codex queue` call
(or direct queue/add), re-read under the binding lock and require unchanged launch
`generation`, `callback_generation`, `state:bound`, `delivery_enabled:true`,
`record.session` and exact `binding.session`. The UUID must match the intent and
argv byte-for-byte after canonical UUID validation. Also compare live pane
PID/boot/start/executable identity, holder, CODEX_HOME and daemon/socket identity.
A stored binding snapshot or `last_binding` alone is not authorization. Fence the
invocation against callback invalidation as specified below; stale proof
refuses/pauses rather than sending to an old UUID. Coalesce into one outstanding notification:
at most six headers, no bodies, inbox paging argv, correlation/reply/ack guidance
and charter/deployment reminder, as in
[`attended/mail.mjs`](../../plugins/attended/mail.mjs). Include home, ship, UUID,
sequence range and a unique notification nonce in an exact receipt marker.
Low notices alone never start inference. Notification never performs `mail.ack`.

Persist intent, nonce, rollout file identity and **byte offset** transactionally
before sending. Two transports are possible: `thread/queue/add` directly over the
verified daemon's Unix JSON-RPC transport can set `client_user_message_id` to that nonce for exact
reconciliation. The proposed first implementation instead uses the task's native
CLI boundary, explicit argv
`[codex, queue, --remote, unix://<verified socket>, --thread, UUID, --message, BODY]`.
**Always pass `--remote`** on CLI delivery: without it Codex may silently create
an embedded server and accept unloaded threads when the daemon is gone. No
provider/model/permission overrides, embedded fallback or `turn/start` fallback.
Never pass `--resume`: queue delivery must not load or run an unattended old
thread. The CLI generates a fresh client ID, so its exact marker must also be
reconciled. A successful English stdout line is not a presentation receipt.

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

### Admission fence and irreversible boundary (review proposal)

The lock protects **the invocation**, not a distributed transaction with Codex.
The implementation must make that narrower guarantee explicit:

1. Outside the lock, validate custody with `ship.presence`, daemon peer/process
   and rendezvous, exact loaded thread, recognized idle state and trusted binding.
   Reconcile all earlier intents/own queued markers before any new offer. Persist
   a private intent with nonce, exact body hash/range, UUID, both generations,
   custody and receipt anchor. No uncertain predecessor may be bypassed.
2. Prepare a bounded argv-only child launch with an allowlisted environment,
   explicit `--remote unix://<verified socket>`, absolute recorded Codex binary,
   and no model/provider/config/permission flags. Under the binding lock, reopen
   the registry and perform **all four required checks**: launch generation,
   callback_generation, bound state and exact session UUID. Require delivery
   enabled and all other intent/identity fields unchanged. Check current local
   pause/intent state; a retry must repeat the entire check, not reuse a token.
3. Still holding that lock, durably mark this attempt `invoking` (a crash from
   here is conservatively uncertain) and **create the queue subprocess before
   releasing the lock**. There may be no awaited RPC, timer, unlocked scheduling
   gap or deferred worker spawn between the final read and process creation.
   Exec failure proved before any native request is a non-admission; a crash
   after `invoking` without that proof stays uncertain. A worker which performs
   the actual spawn must itself hold the lock and repeat the read.
4. Release promptly; never wait for the CLI, daemon or `wake.hosted` under the
   hook state lock. Bound child lifetime and output, drain/wait for the actual
   child and verify owned descendant cleanup; killing a wrapper is not evidence
   its native request was unaccepted. Timeout/lost result is uncertain. Before
   committing a result/report, recheck both generations/custody/UUID again.
   Stale results can update the old UUID's evidence ledger, never re-enable the
   current binding or report progress for its replacement.

Thus an invalidation committed first prevents the invocation; an invocation
ordered first may be admitted later by Codex even after invalidation. Local
locks cannot withdraw a request already sent or fence another native client.
Never describe a post-spawn check or child kill as cancellation. Preserve the
old intent and reconcile it; do not requeue it in a fork/clear replacement. If
review requires **no remote acceptance after invalidation**, 0.166 queue/add
needs an upstream conditional admission token/cancel-before-promote contract;
that stronger guarantee is not available from this design. Similarly a callback
is deferred until turn entry, so it is not instantaneous notice of a TUI clear.
The proposed guarantee is exact-UUID admission authorized at invocation, not
proof that an idle pane has never navigated. Qualification must exercise those
races and refuse any stronger UI-continuity claim.

A slow filesystem/spawn can exceed the dispatcher's one-second lock wait. B must
not enable delivery while callback lock timeout can silently retain a usable old
binding. **B1 prerequisite (implemented; delivery still disabled):** before trying the state lock, every
dispatcher invocation first removes the old private invalidation-request file
and fsyncs its parent, **before allocating a temporary file**, then atomically
publishes a fresh token in a separate regular file (fsync file/parent, same
ownership and symlink checks as the registry). Failure before rename therefore
leaves no token and refuses old snapshots, not a still-valid old token. Prepare
also rotates this poison before replacing the launch record. The old binding records its last validated request
token. Every admission checks that the request token still equals the binding's
validated token under the lock immediately before spawn. Missing/mismatched
request files refuse. A callback that times out has already poisoned future
admission; it must not restore/clear its request on exit. Validation may publish
a bound record only if its own request token is still current, as well as both
generations matching. Overlapping callbacks cannot clear a newer poison. A
crash after request publication leaves delivery paused, safely recoverable only
by a new fully validated callback. Preserve old intents throughout.

The request token is an additional sticky availability fence, not atomic native
cancellation: a request published after the final read can race with an already
committing spawn. The state-lock invalidation and spawn remain the ordered local
authorization boundary described above. Storage failure publishing the token
must refuse the callback; an unreadable fence must refuse admission. Absolute
correctness under a filesystem that cannot persist either poison or invalidation
is not claimed. Test lock-timeout, publication/rename crash and overlapping-token
barriers explicitly, and review dispatcher upgrade/migration (B0 deliberately
refuses an unexpected dispatcher body) before activation. B1 supplies these tests
and the exact-known-B0 upgrade described below; it does not activate delivery.
This was not fixed by the earlier step-1 patch.
An alternative is an admission service with ordered callback/admission messages
and no blocking native subprocess in its critical section; it is substantially
more machinery and still cannot retract an already sent queue/add. Recommend
the poison-token addition plus short spawn-only locking, subject to review.
A bounded wait alone is not a correctness proof.

### Body, custody and receipt state

Use the exact semantics of [`attended/mail.mjs`](../../plugins/attended/mail.mjs)
and the [native-mod contract](../../plugins/README.md#attended-native-sessions):
one burst, at most six validated headers, no message bodies, no fabricated tasks.
The text says first mate of the ship, count and through-sequence, **notification
not acknowledgement**, explicit inbox paging argv with `address`, `after_seq` and
`limit:40`, correlation/reply guidance, acknowledge only handled mail, no-action
instructions apply only to their own message, and charter/deployment rules still
apply. Render mailbox header data as JSON data, never as shell instructions.
Low-only mail does not start inference; later arrivals stay in the mailbox for
the next snapshot. No terminal input, composer mutation, interruption,
`turn/start` or `turn/steer` fallback is permitted. The native queue must preserve
a typed draft and serialize any idle-to-busy race; prove both with real PTYs.

Prefix the body with a versioned, machine-parseable JSON marker containing
`type:privateer-mail`, `version:1`, home, ship, exact session UUID,
`notification_id` (random nonce), `after_seq` and `through_seq`. Store the exact
encoded body/hash and count in the intent. The marker is correlation evidence,
not a bearer authorization or an acknowledgement command. Only the bound native
**user-input** record with this exact payload, native thread identity and its
actual promoted turn association counts; a quoted marker in assistant/tool/mail
text, queue-list echo, UI preview, or removal event does not. Validate the real
0.166 event schema and turn ordering in the fixture, not substring matching.

| Durable intent state | Evidence / next action |
| --- | --- |
| `prepared` | Anchor and body committed, no attempt; revalidate before invoking |
| `invoking` / `uncertain` | Native acceptance may have happened; reconcile, never blindly resend |
| `queued` | Own exact marker found in queue/list or structured add result; wait, no presentation watermark |
| `presented` | Exact native promoted user input **and** actual turn-start association; persist receipt/watermark before reporting |
| `acked` | Runtime `acknowledged_through` covers range; settle independently, no invented native turn/wake |
| `rejected` | Positive proof the offer was not admitted; bounded retry policy only |

One outstanding offer per binding/ship is a hard bound, not one offer per poll.
Partial acknowledgements can advance the known ack cursor but cannot erase the
unsettled suffix. Queue absence, a CLI nonzero exit, socket close, elapsed time,
process exit or success prose alone is not proof of rejection/presentation.
Explicit ack can settle uncertainty without a native receipt; the adapter never
issues `mail.ack`, never infers handling/task completion, and never fabricates a
mail wake from an ack alone. Persist receipts and the accepted watermark
transactionally before retiring the intent. Old receipts survive restart and
compaction even when deliberately unacknowledged in the mailbox.

### History-mode choice and transport trade-offs

**Recommend refuse delivery for `historyMode:paginated` in initial B.** Binding
metadata may still be verified, but no enabled mail admission or hosted
qualification is implied. On 0.166 the reported rollout can be absent; assigning
byte offset zero to a nonexistent file is not a durable receipt anchor. An
unknown history mode also refuses. Do not change the user's history mode to make
the adapter work.

For supported materialized history, require an existing native session_meta
matching UUID/cwd; save device/inode plus committed byte offset before invoking.
Scan bounded forward batches (1 MiB) with complete-record checkpoints, never a
last-N window. Pin every unresolved anchor and receipt; truncation, replacement,
missing history or an unvalidated schema pauses rather than resets. Bound the
session index to 128 and ledger to 8 MiB per project initially; backpressure
when pinned facts fill either bound, never evict unresolved evidence. A new
launch recovers the same UUID's ledger, never another thread's.

A future paginated anchor must be a native, restart-stable `(thread UUID,
history revision, ordinal/cursor)` read through a supported public API, with
proof that the cursor cannot skip promotion/turn events, retention across
compaction/restart, and explicit gap/revision handling. Persist intent and scan
checkpoints against that anchor before sending; enumerate bounded pages until
the intent is settled, without treating an empty/partial page as absence. Native
revision remapping must be provable; otherwise pause. A Privateer ledger or
transient event subscription alone cannot prove an unseen native promotion after
crash. Do not read Codex's private SQLite database as an undocumented substitute.
This is an alternative acceptance project, not an asserted 0.166 capability.

| Delivery option | Advantages | Costs / decision |
| --- | --- | --- |
| CLI `codex queue --remote … --thread <UUID> --message <BODY>` | Native subprocess boundary requested by this task; preserves thread settings; explicit endpoint prevents embedded fallback | Fresh native client ID, so reconcile exact body/nonce; output is not receipt. Recommend for initial B |
| Direct verified `thread/queue/add` | Caller sets stable `client_user_message_id`; structured results and stronger exact queue lookup | Own experimental protocol/schema compatibility and same admission race. Possible later simplification after separate review; do not assume ID makes add idempotent |
| Transient events / last-N transcript tail | Easy/cheap | Cannot recover lost response or old promotion beyond retention; reject as sole anchor |
| Durable paginated native cursor | Supports default history without fabricated rollout | Requires qualified retention/revision API; defer and refuse mode until proved |

### Pause, busy and watchdog reporting (issue #616 contract)

Keep transport availability, native busy state, presentation and mailbox handling
as separate facts. Normally do not invoke queue while busy. Unknown status,
lifecycle gaps or lagging history is **not idle**: reconcile a supported read-only
status snapshot with ordered native events before admitting again. A native
start fixes one `turn_started_ms` for its turn ID; persist and reuse that timestamp
across every poll/reconnect/restart. Only confirmed terminal end/error/abort
clears it. Do not invent a new start at each heartbeat or mark idle merely because
the observer disconnected. If no truthful busy snapshot exists, withhold an idle
report and expose unavailable diagnostics; the existing watchdog still has its
blocked-mail bound. If the turn was known busy, preserve that fact/timestamp
until terminal or reconciled native evidence proves otherwise.

Hosted reports are fenced by current holder, session and both generations:

- `delivery_bind`: validated binder only, no mail progress or wake-budget spend.
- `delivery`: exact session, receipt/ack-backed `accepted_through`, local pause,
  `decline_reason`, `decline_count` in `[0,3]`, actual `busy`, and stable
  `turn_started_ms` when busy. Queue admission never advances accepted progress.
- `mail`: exact range only after promoted native receipt/real turn association.
  Captain-origin turns use `captain`, not an inferred mail wake. Ack-only
  settlement updates diagnostic progress without inventing a native turn.

Persist decline count and reason. A proven native refusal/non-admission or
native interruption/error pauses; hosted automatic retry is no sooner than one
minute, only for a **proved unadmitted** offer and only while count `<3`.
Increment once per proved decline, not per poll/reconciliation failure. A turn
that was already presented but later failed is never a reason to replay the same
range; handling/ack remains the mate's responsibility. At **K=3**, stay paused; manual pause and exhaustion never auto-clear. Actual
receipt/ack progress resets the no-progress decline spell; an explicit authorized
resume/captain prompt can recover an ordinary native-stop pause, but cannot
erase uncertainty. Background messages, lease renewal, rebind/compact or passage
of time cannot clear uncertainty/human pause/exhaustion. Report uncertain
admission as paused/reason `other` with unchanged decline count. No automatic
resend when the minute expires if the prior offer may have been admitted.

Do not change Rust watchdog policy: three declines can alert immediately;
otherwise actionable mail blocked for five minutes alerts once per no-progress
spell/custody. A real busy turn holds the blocked timer, but a continuous turn
has its own 30-minute bound. Leases, queue success and rebinds must not reset
those clocks; a replacement adapter must not manufacture a fresh busy edge.

### Review recommendation and qualification boundary

Recommend **(1)** retain explicit durable exact-UUID resume, fresh/fork/clear
unbound; **(2)** qualify CLI queue on verified materialized-history bindings only,
with the invocation fence and durable uncertainty/receipt state above;
**(3)** refuse paginated delivery until a native durable anchor is qualified;
**(4)** keep hosted eligibility, exact-resume runtime integration and live
pilot in C. Default paginated installations will therefore still have no idle
mail; that honest limitation is preferable to replay or a guessed binder.

Before B code, request independent Opus review of reservation feasibility,
invocation-vs-remote-acceptance semantics and the invalidation-timeout fence.
Then qualify deterministic invalidation barriers before/after spawn, same-folder
callbacks, custody/generation replacement, busy race, native draft preservation,
daemon disappearance with no embedded fallback, unknown schema, lost response,
unloaded persistent queues, restart beyond a transcript tail, compaction/gaps,
partial/complete ack, low-only bursts and K=3/stable busy timestamps. Use only a
fixture-owned daemon/throwaway state and local mock provider. None of this is a
claim of delivery implementation, native autonomy, live login/model evidence or
permission to touch the running machine daemon.

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

### Approved B sub-split (2026-11-03)

The first mate authorized separate **B1**, **B2**, and later **B3** crews to keep
each implementation reviewable. B1 and B2 are merged. On 2026-11-03 the first
mate approved splitting B3 again: the current slice is **B3a**, B2 review fixes
only, and stops after its gate/local commit. No intermediate enables `delivery_enabled`,
and the main support table stays unchanged.

- **B1:** shared dispatcher-recognition helper; durable, self-contained dispatcher
  poison before lock acquisition; explicit exact-known-B0 body upgrade; migration,
  lock-timeout, overlapping-token and publication/rename crash tests. New bindings
  record `fence_version:1` and the validated `invalidation_token`.
  `require_fence` refuses B0/missing/unsafe/mismatched tokens; it is a prerequisite,
  not full admission authorization. File fsync plus parent fsync makes publication
  and registry/dispatcher replacement durable. A callback whose publication fails
  aborts, and a poisoned binding requires a fully validated new callback.
  The pinned B0 digest covers the dispatcher shipped in PR 675 at `01f1e5d7`;
  B2 also pins the B1 body from PR 679; B3a pins the B2 body from PR 683.
  Only those exact bytes are migrated under
  prepare's lock after live-owner refusal. Native command/trust are unchanged; unknown bodies still refuse.
- **B2:** C2 native queue-promoter/loaded-without-TUI fixture, a dormant
  materialized receipt ledger and deterministic lost-response recovery tests.
  These are not B1 evidence. Native results and the C3 limitation are recorded
  below; B2 does not implement a queue runner or turn on delivery.
- **B3a:** B2 review fixes: tolerate concurrent poison removal, seed mid-turn
  anchors, compact covered settled history to nonce digests (preserving audits),
  avoid unchanged ledger writes, and refuse non-object JSON as ValueError.
  No runner/control or activation.
- **B3b:** compact rejected/abandoned offers, bound the nonce index without
  forgetting reuse, retain range/hash in operator audits; probe native ATTACHED
  and real user-message/client-ID schema qualification. The fixture records
  negative qualification, not a discriminator or accepted-input receipt. No
  runner is added: neither prerequisite is established at zero provider requests.
  A future native queue child/under-lock fence requires both proofs first.
- **B3c:** authenticated controls, poll/report, pause/K=3/stable-busy integration
  and the combined activation gate. Activation requires all
  C1-C4, including the B2 native evidence; merely passing B1's poison check cannot
  enable delivery. **Paginated installs get no idle mail**; no history setting is
  changed (C4).

B1 deterministic tests execute the real dispatcher in owned subprocesses, hold
its state lock through timeout, crash before/after poison rename and after parent
fsync, and check that an older validating callback cannot clear newer poison even
when no newer callback generation committed. They cover exact B0 migration versus
near-match refusal, unchanged native trust, old-binding refusal, missing/unsafe
fence and persistence failure. Existing native binding-only qualification also
checks the stored token under lock; it does not enqueue or establish C2/C3.

### B3a review fixes (delivery disabled)

Both the self-contained dispatcher and `publish_request` now treat
`FileNotFoundError` from the poison lstat/unlink as already removed by another
publisher. Unsafe existing files still refuse. Parent fsync and a fresh token
publication still happen, so a competing deletion cannot abort a callback before
it publishes its own poison. Deterministic interleavings exercise both syscall
boundaries in the direct API and actual isolated dispatcher subprocess. Only
the exact pinned B0/B1/B2 dispatcher bytes upgrade; near matches refuse without
changing command/trust.

Anchoring during an already open turn records that native turn ID. A bounded
backwards lifecycle lookup (at most 1 MiB, complete native records, 64 KiB reads)
finds the nearest start/terminal at the saved anchor offset; no lifecycle within
the budget refuses rather than guessing idle. This is only the initial lifecycle
seed, **not a tail-based receipt scan**: receipts still scan forward from their
committed offset. B2 anchors missing this seed recover it from that validated
boundary, not today's EOF. A matching later terminal closes the old turn, so
subsequent promotion evidence can settle normally; mismatched terminals and
unknown/compacted history still pause. Long active-turn histories with no
lifecycle in the lookup budget remain unavailable pending reconciliation; there
is no unbounded scan under the binding lock.

B3a removes historical FINAL offers covered by `accepted_through` on transaction
commit, keeping SHA-256 nonce digests to reject reuse across restart and UUIDs.
B3b extends this to rejected and suppressed abandoned offers as described below.
Watermarks, the current intent and all uncovered/pinned offers survive; operator
resolution audits are retained separately even after body compaction. B3b bounds digests with a non-deleting filter; audits
remain subject to the overall 8 MiB/128-session storage bounds, not an
infinite-retention claim. Exhaustion applies
backpressure and never evicts facts. Migrating B2 history uses the same atomic
transaction and rolls back on write failure. Unchanged reconciliation skips
replacement/fsync; advancing scan/ack/state still commits. Non-object ledger
JSON refuses as ValueError before member access.

B3a deterministic coverage includes a mid-turn anchor and B2-anchor recovery,
matching completion/abort followed by later receipt, mismatched terminal refusal,
lookup bounds/block boundaries, repeated large settled bodies beyond the old
8 MiB cumulative limit, nonce-reuse refusal, audit preservation, migration write
failure, and unchanged-poll write suppression. The real user-message schema and
ATTACHED qualification remain B3b blockers. No history setting changes; **paginated
installs get no idle mail**. No queue runner or public control is added here.

### B3b review fixes and qualification stop (delivery disabled)

A pre-request `rejected` offer is positively unadmitted and can be compacted
without advancing acceptance. An `abandoned` offer can be compacted when covered
by `suppressed_through`, also without inventing acceptance or acknowledgement.
Both retire from history on the next prepare/transaction, preserving the latest
intent in full. Covered accepted FINAL history still compacts as before;
unresolved and uncovered offers remain pinned. Operator audits now retain
`after_seq`, `through_seq` and `body_hash`, including when migrating older full
offers. Previously compacted B3a audits cannot retroactively recover fields whose
source bodies were already discarded.

Each session retains at most 256 recent exact SHA-256 nonce digests plus a fixed
65,536-bit, four-index, non-deleting Bloom filter. All historical digests migrate
into that filter before the exact window is trimmed. This is **not** an eviction
of replay protection: every retired nonce, even across UUIDs/restart and outside
the exact window, is still refused. A filter false positive also refuses a new
nonce safely; saturation is eventual backpressure, not permission to reuse.
The filter costs 16 KiB as hex JSON per session. Unresolved facts, operator audits
and the entire ledger remain bounded by the existing 8 MiB limit; operator audit
history is intentionally not evicted, so infinite operator resolutions are not
an infinite-retention claim. Deterministic tests exceed the old cumulative-body
cap with both rejected and abandoned offers, migrate old digest lists and audits,
check evicted/cross-UUID/restarted nonce refusal, saturation/corruption refusal,
and preserve unresolved evidence and existing atomic-write rollback coverage.

`scripts/codex-queue-spike.py` now records exact `thread/read` fields, status,
source/originator and `thread/loaded/list` membership for an owned exact-resume
real TUI, a loaded thread that never had a TUI, and the former after its owned
PTY exits. It explicitly returns `attached_discriminator_qualified:false` and
`native_user_message_qualified:false`, with separate blocker reasons. These are
negative qualification results, not a production attachment detector. The native
thread's creation source/originator is not current attachment, and idle plus
loaded cannot distinguish an attended TUI from another app-server subscriber.
The probe never turns these fields into permission to spawn.

The fixture retains trusted SessionStart/UserPromptSubmit stops, waits for native
terminal rollout events, and verifies that neither queued probe produces an
accepted `event_msg/user_message` with its body. These hooks protect the zero
provider-request constraint **by stopping before input acceptance**. Removing a
stop or racing interrupt after allowing a hook is not a qualified barrier against
provider requests. The synthetic receipt shape must therefore remain unqualified;
no guessed alias, synthetic rollout or source-only schema assertion substitutes
for a real accepted native record.

Upstream 0.166.0 (`rust-v0.166.0`) corroborates the limits: public `Thread` has
runtime status and creation source/originator, not TUI connection identity;
`app-server/src/thread_state.rs` keeps live connection/subscriber state internally
(and app-server clients are not proof of a TUI). Persisted thread "attachments"
are data associations, not live TUI attachments. `core/src/session/turn.rs` calls
`run_pending_session_start_hooks` and then `run_hooks_and_record_inputs`; a
stopping UserPromptSubmit branch skips `record_pending_input`.
`protocol/src/protocol.rs::UserMessageEvent` declares optional `client_id`, but
that source declaration alone does not qualify an actual promoted queue rollout.

**Stop here:** no positive public exact-pane ATTACHED proof and no zero-request
accepted native user-message/client-ID record are established by this fixture.
B3b therefore adds **no dormant runner**. Delivery remains false everywhere;
B3c activation and captain-authorized PR C are blocked pending those proofs or
an independently approved upstream contract/qualification change. No login,
real model call or production route/history change is permitted. **Paginated
installs get no idle mail.** Gate outputs must report the native observations
and zero provider requests; a passing negative probe does not mean activation
passed.

### B4: filter capacity and one-request fixture

B3b's zero-request result above is historical. The captain approved exactly one
queued turn per isolated fixture run reaching **only the fixture's own loopback mock provider** for B4.
No real model, login, auth.json, production route/history edit or live daemon is
permitted. All other turns still stop at trusted hooks. The nonce-selected
UserPromptSubmit allowance is one-use (exclusive file creation); the mock returns
a terminal canned response, with provider/stream retries disabled in the
throwaway config. The fixture asserts exactly one request to its loopback port,
and records the actual native events before checking the receipt contract.
Neither this request nor a native accepted-input receipt qualifies attachment or
live mail. Delivery remains disabled; no runner is authorized in B4.

**Observed evidence:** exact-head serial admitted gate
`1e1b983a-90d6-48e6-a43b-f6867ae624da` at
`b2e3e7789b25fb8e9e04d6e14b82ff5482026954` passed all eight steps
(Codex Rust units 2, Python 86, plugins 36, attended 24, bytecode guard 1,
build, binding fixture and queue fixture). The binding fixture
made zero requests; the queue fixture made exactly one POST to its own
`127.0.0.1:<fixture port>/v1/responses`. Both CLI and owned daemon were 0.166.0.
The real rollout recorded, in order:

1. `event_msg/task_started` with `turn_id` and
   `turn_attribution.turn_trigger:"queue"`.
2. `event_msg/user_message` with a **canonical UUID `client_id`**, exact queued
   `message`, and empty `images`, `local_images`, `audio`, `local_audio`,
   `text_elements` arrays. It has **no `turn_id`**.
3. `event_msg/task_complete` with the same native turn ID (with an intervening
   token-count event for the canned mock response).

The observed lifecycle/input/terminal payloads (token-count omitted) are pinned
in `tests/fixtures/codex-queue-accepted.jsonl`; only fixture-generated values, no
credentials or personal names. `native_client` requires that exact text-only
input shape and canonical client UUID. A matching body with missing/aliased
fields, multimedia, extra turn ID or unknown shape pauses with a clear
`native text-only user_message schema unqualified` reason, not a guessed alias.
Turn association still comes only from the anchored open lifecycle. The new
native probe also prepares a real ledger anchor before the one request and
reconstructs the reducer to prove receipt recovery without CLI output; it
asserts accepted progress with **zero acknowledgement**. This replaces the old
synthetic input contract; it does not qualify paginated receipts or attachment.
A repeat of the permitted hook input is explicitly refused by the one-use guard.

The non-deleting nonce filter now has **separate closed diagnostics**, not a
blanket claim that every match proves nonce reuse:

- Exact retained nonce/digest match: `receipt nonce reused`.
- Filter-only match: `nonce_filter_possible_reuse` (could be an evicted real
  nonce OR a false positive; do not assert which).
- Fill >= 25% in the **session being prepared**: `nonce_filter_saturated`, even
  for a nonce whose bits do not match. Other sessions still participate in exact
  and possible-reuse checks, but their capacity alone does not block this session.

Both filter errors include the responsible session, fill ratio, 65,536-bit
capacity and maximum fill. They close admission transactionally without changing
the ledger or acceptance/ack cursors. With four independent indices this fixed
8 KiB bitset (16 KiB hex JSON) reaches 25% fill at roughly 4,700 distinct admitted
retirements per session; false-positive probability there is roughly 0.39%.
This is an estimate, not an insert-count guarantee. Existing more-filled B3b
filters are preserved and refuse new inserts into their own session, never
cleared automatically; negative cross-session membership checks still work. Old digest-only
ledgers migrate conservatively, including unknown historical rejection digests;
we cannot subtract them from an existing non-deleting filter safely.

Positively pre-request `rejected` offers no longer consume admission-filter
capacity. Compaction counts them separately in `rejected_count`, retains the last
256 exact rejection digests and drops their full bodies. Eviction of these exact
digests is safe ONLY because the trusted rejection evidence proves no child or
native request ever existed; there is no possible old native admission to
correlate. Their old nonce may eventually be accepted as fresh, unlike any nonce
that might have reached Codex. Prepared/uncertain/queued/presented/acked/operator
resolved nonces do NOT get this exemption. The latest intent remains pinned.
This intentionally supersedes B3b's all-retired-nonces-forever filter promise for
never-spawned failures only. Existing uncertain facts and audits are unchanged.

**Future runner backoff contract:** either filter diagnostic pauses new offers
immediately and surfaces/coalesces a stable diagnostic per project/session/kind;
it does not spend a native decline, manufacture progress, regenerate random
nonces until one passes, retry every five seconds, switch session, clear the
filter, or wait for the watchdog to explain the stall. Reconciliation/real ack
of an existing offer remains possible. Resume requires explicit operator capacity
review (a separately designed durable replay-preserving ledger rollover or
capacity migration), not timer/captain chatter/rebind. No such rollover or runner
is implemented here. Rejected-exec failures retain the existing one-minute/K=3
retry contract; exemption from filter capacity is not permission to spin.

### B5a: B4 review fixes (delivery disabled)

The first mate approved this limited slice on 2026-11-04. Saturation now refuses
prepare only for the target session; a different session's saturated filter
still checks possible reuse and retains all watermarks. This is not rollover or
permission for a runner to switch UUIDs to evade capacity. No rollover exists;
explicit operator capacity review is still required for the full session.

The fixture has **not** observed a queue item's `clientUserMessageId` before
promotion, so equality with the promoted native `user_message.client_id` is
**unqualified and no longer required**. Queue and promoted client IDs must each
be canonical UUIDs and are stored as separate facts (`queue_client_id` versus
receipt `client_id`). A queue ID changing across observations still pauses.
B4 queued checkpoints migrate their saved queue ID separately before scanning;
no fact is lost when promotion supplies a different ID. Exact saved body/nonce
plus the anchored native **queue-triggered** open turn is the receipt proof,
not presumed ID equality. A matching mail body under a missing/unknown/non-queue
trigger refuses. Busy lifecycle tracking can still observe ordinary captain
turns; it does not turn them into mail receipts. Older checkpoints lacking
trigger provenance recover it with the bounded lifecycle lookup at their saved
scan offset, not today's EOF. A lookup gap pauses, never guesses queue origin.

The HTTP mock handler records each validation violation and answers 500 rather
than relying on assertions swallowed by the server thread. Main-thread checks
still fail the fixture on any violation. Deterministic tests exercise malformed
and excess requests as well as the one allowed request without provider access.

B5b will implement Rust hosted-supervisor custody/poison and barrier tests; B5c
will implement the dormant runner. **Every future delivered turn must be counted
and reported as possibly unattended against K=3 and the budget.** Hosted-pane
custody is the authorization boundary; no public native attachment discriminator
is claimed or required as a substitute. Neither future slice may add an enable
path here. Before activation, filter rollover, remaining native queue
qualification and all documented barriers still need qualification/review.
Delivery remains false, no Rust supervision path changes, no live qualification.
**Paginated installs get no idle mail**, and no history-mode setting changes.

### B5b: dormant Rust custody prerequisite (delivery disabled)

`src/host/codex_custody.rs` is called only by the opted-in Codex hosted-pane
supervisor in `src/cli.rs` / `src/host/sleep.rs`. It does not make Codex hosted
eligible, start queue children, report delivered turns, or provide an enable
path. The actual unreaped `Child` is required to acquire a Linux pidfd. The
supervisor waits for the native wrapper to exec and the trusted callback to
bind before publishing `custody.json`: a wrapper executable cannot qualify as
the expected Codex payload. The record pins pane PID/boot/start, executable
path/device/inode, launch generation, exact UUID/home/cwd/CODEX_HOME, holder,
random custody epoch, supervisor PID/boot/start/executable, and daemon
PID/boot/start/executable plus rendezvous target and socket device/inode.
No public native attachment discriminator is claimed.

**Shared fence protocol.** `prepare` alone initializes `custody-poison.json`
with a new launch generation/token; neither callback nor supervisor can repair
that token. The supervisor copies it into its owned record under the same
project `flock` as Python. The file must still name that generation, with no
revocation reason. Publication never refreshes the token, so a concurrent
pre-lock revocation cannot be cleared by a late custody writer. Missing,
unsafe or mismatched files refuse proof. The existing `invalidate.json` fence
remains independently required for binding/callback generations. The stable
hook command/trust declaration is unchanged; only the exact pinned B5a
(and older explicitly known) dispatcher bodies upgrade. Near matches refuse.

Wrong-UUID callbacks publish custody poison **before trying the project lock**
in both direct and self-contained dispatcher paths, even when lock acquisition
times out or the selected release is missing. The dispatcher passes its bounded
saved callback input to the handler instead of reading stdin twice. A callback
observing a changed daemon also poisons custody. Rust independently checks the
pinned daemon process, executable, daemon.pid, socket inode and rendezvous on
each local supervision pass (250 ms); it does not connect, start or signal the
daemon. Pane exit is checked using pidfd readiness before reaping, and intended
stops retire custody before waiting for the child. Unhandled supervision errors
retire through Drop. A supervisor crash cannot run Drop: its recorded process
identity/liveness must therefore also be checked by B5c; **the durable owned
state alone is never authority**, and restart cannot reconstruct wait custody
from an old registry PID.

Presence replacement, release and handover synchronously poison local custody
inside the SQLite writer transaction. Fixed order is **SQLite writer (when
needed) -> project flock**; poison removal/file+parent fsync/publication comes
before flock. There is no SQLite acquisition, daemon/report RPC, child wait or
reap under flock. A lock timeout may leave the old record saying `owned`, but its
old token is already unusable. Once poison is durable, flock or diagnostic
`custody.json` read/rewrite failures are logged and **do not abort presence**:
attend, leave and handover still commit. Only poison persistence failure aborts
revocation. A stale `owned` record is not authorization without its matching
unrevoked token. Same-holder live renewal and stale leave do not
rotate custody. Projects without launch/custody state are untouched. A failed
transaction may conservatively leave custody revoked; it never rolls poison
back or discards receipts. Revocation failures do not abandon a running native
child. Filesystem persistence failure remains the same explicit limitation as
B1; a consumer must refuse missing/unreadable proof.

**B5c must** reread custody, both poison files, current holder/epoch, both binding
generations and exact intent/UUID under the project fence immediately before
spawn, acquire its own pidfds for pane and supervisor, and independently verify
start time and executable device/inode plus daemon identity. It must not trust
a Python Boolean, stored `owned` state, cached snapshot or pidfd acquisition
alone. B5c implements a dormant consumer below; activation is not qualified. Exit after
the final check may still lead to a late admitted turn; every future delivered
turn is **possibly unattended**, counted/reported against K=3 and the budget.
There is no cancellation or instantaneous UI-navigation guarantee.

PR700 receipt carry-ins: a queue trigger now requires
`turn_attribution.turn_id == task_started.turn_id`, both in forward scanning
and bounded anchor recovery. New intents seed their live lifecycle from the
anchor's saved fields rather than recomputing them; older checkpoints are
versioned and revalidated at their saved offset. Queue item **id and
clientUserMessageId** must both stay unchanged across queue observations;
promotion client_id remains a separate fact with no equality assumption.
False-positive refusals accumulate across many nearly-full session filters:
with n independent filters at 0.39% each the rough union probability is
`1 - (1 - 0.0039)^n` (about 39% at 128), not 0.39% project-wide. Independence
is an approximation, not a guarantee. Since the runner must not resample a
refused nonce, even false positives need explicit operator recovery. Before
activation, design replay-preserving rollover/capacity migration with that
project-wide rate in mind; no filter reset/rollover or silent UUID switching is
implemented here.

Deterministic coverage uses actual owned subprocesses, pidfds and a shared
flock: exit before reap, executable/PID/start/UUID/generation substitution,
wrapper-vs-payload, daemon/socket replacement, poison-before-lock timeout,
initial publication after poison, sticky supervision loss, holder replacement,
handover/release and harmless stale leave. Python covers wrong-UUID pre-lock
poison in the real dispatcher and direct path, daemon change, exact B5a migration,
attribution-ID mismatch in scan/legacy recovery, lifecycle seeding and queue-ID
change. Round-2 regression coverage holds the real project flock through its
timeout during attend, leave and handover, then reopens SQLite to verify each
presence change committed while the stale `owned` record remains poisoned. A
malformed custody record exercises the same post-poison best-effort boundary;
an unsafe poison path instead proves all three presence changes roll back.
Home-context comparison resolves both paths canonically, accepting a symlink
alias of the same home while refusing a different home. Store's existing
symlink-policy checks are unchanged. The non-Linux pidfd build fix is retained.
These are custody/reducer tests, **not runner spawn-barrier evidence**.
Real supervisor-crash recovery, before/after final-spawn barriers, lost queue
responses, same-UUID second client, idle navigation, PTY draft preservation,
K=3/busy accounting and live qualification remain B5c/C activation gates.
**Paginated installs get no idle mail**; no history-mode setting changes.

### B5c: dormant queue consumer and conservative accounting (delivery disabled)

`plugins/attended-codex/codex_runner.py` is an **unconnected library**, not a
poller, launcher, scheduled worker, dispatched control or hosted-eligibility
change. It consumes a ledger-prepared offer; body/header selection remains with
the future runtime integration. No production caller imports it and no code
writes `delivery_enabled:true`. The enabled records in the deterministic suite
are fixture-owned data, not an activation mechanism. Production bindings remain
`delivery_enabled:false`. Native attachment is not asserted: every potentially
delivered queue turn is counted and reported as **possibly unattended**.

Invocation follows this durable state machine:

1. Reconcile earlier offers using `observe`: read-only queue evidence, anchored
   promoted input and the trusted runtime ack cursor. There is no replay on
   queue absence, CLI prose/nonzero/exit/timeout or elapsed time. An outstanding
   `invoking`/`uncertain` offer immediately exposes stable diagnostic
   `codex-uncertain-<nonce>`, with unchanged decline count; no five-minute wait.
   Exact queued evidence remains pending, exact promoted input advances receipt
   progress, covering real ack settles independently, including after daemon
   death (ack reduction precedes connection). That progress resets the decline
   spell but never refunds the turn budget. The runner issues no ack.
2. `dispatch` requires native **legacy** history, exact UUID/cwd, loaded-list
   membership without pagination, an idle native snapshot, and an empty,
   non-paginated native queue. All daemon RPC and `ship.presence` calls precede
   the project flock. The final fence rechecks the lease's holder and expiry;
   synchronous Rust poison orders holder replacement with that fence. It never
   acquires SQLite while holding flock.
3. Under the shared flock, reopen the entire launch and match the snapshot,
   bound/hosted/enabled state, both generations, exact intent/session/body marker,
   custody epoch/holder/context, `invalidate.json` and `custody-poison.json`.
   Compare local daemon PID/boot/start/executable, daemon.pid, rendezvous and
   socket device/inode against both binding and Rust custody. A fresh bounded
   receipt scan must be caught up and idle; missing/schema/gap evidence refuses.
4. Atomically save `invoking`, custody epoch, `possibly_unattended:true` and a
   conservative budget reservation **in the same receipts.json transaction**.
   Re-read launch and both fences after that fsync. Immediately before actual
   `Popen`, independently acquire **both pane and supervisor pidfds**, check
   boot/start/executable path/device/inode and exit readiness, and recheck lease
   expiry. Actual spawn occurs under flock, with no RPC, unlocked deferred
   spawn or wait between authorization and creation. Argv is exactly the recorded
   absolute executable plus `queue --remote unix://<verified socket> --thread
   <exact UUID> --message <saved body>`. No resume, embedded fallback, config,
   provider, model, permission or history override is added. Only allowlisted
   launch state paths, fixed PATH and locale reach the child.
5. Check custody/liveness/fences again after spawn, then unlock before all waiting.
   A late exit/poison is **not cancellation**: keep the old intent and charge it.
   Discard English output to `/dev/null`. Bound the owned leader to three seconds,
   track up to 64 observed descendants with pidfds, and kill/wait the actual
   leader and observed escaped descendants by owned identities on cleanup.
   Failed cleanup pauses immediately. A sub-20-ms unobserved double-fork can
   escape sampling; stopping every possible payload is **not claimed**, and
   real native queue descendant qualification remains an activation gate.
6. Only `Popen` exec failure positively records `rejected`; count that decline
   once and enforce the one-minute retry floor. Identity/fence failure after
   durable `invoking` stays conservatively uncertain, even if this process knows
   it never called Popen. Every successful spawn, including CLI nonzero/timeout,
   remains uncertain until the normal evidence reducer settles it. A result for
   an operator-settled old nonce cannot overwrite a replacement intent. Report
   objects include `report_eligible:false` for changed custody/generation/fence,
   dead pane/supervisor/daemon or expired presence; they are not sent to Rust.

**Two separate conservative bounds** are persisted at project scope, so rebind,
UUID switching and restart cannot reset them. Native declines are capped at
**K=3**; only actual receipt/ack progress resets that no-progress spell. A native
abort pauses once per observed turn and does not replay a presented range.
Human/native-stop pauses do not clear on background mail, renewal or time; no
resume control is introduced. Independently, the dormant implementation has a
hard **three-reservation possibly-unattended turn budget**. Each `invoking`
transaction spends one, even a later positively proved exec failure or pre-spawn
fence failure; this intentionally over-counts rather than refunding a crash or
late admission. Receipt/ack progress, a new session, rebind and restart never
refund that budget. There is no reset/refill/configuration API. Report data
includes spent count and the fixed cap. This is conservative local accounting,
**not integration with the runtime wake/model budget ledger**; authenticated
report dispatch and runtime budget reservation must be designed/tested before
activation. Merely returning a report object does not spend/report a real Rust
wake. No native turn is fabricated from queue success or ack-only settlement.

A separate durable lifecycle cursor continues after an intent settles. It uses
bounded complete-record forward scans and the native `task_started.started_at`
seconds timestamp (the pinned 0.166 fixture shape), never heartbeat/reconnect
wall time. Preserve the same open turn ID/start across polls, restart and
observation loss; only matching completion/abort clears it. If initially
anchored mid-turn without its timestamp, busy is known but unavailable for
truthful timestamp reporting; refuse admission, never invent a fresh start.
Unknown snapshot/schema, lag or history discontinuity exposes unavailable
state while preserving known busy facts. Bounded catch-up commits its forward
checkpoint; it cannot get stuck re-reading the first 1 MiB forever. Existing
Rust five-minute blocked and thirty-minute continuous-busy policy is unchanged.

B5b carry-ins: `Custody::revoke` sets its sticky in-memory flag only **after**
poison succeeds, so failed poison persistence is retried by later revoke/retire/
Drop. Tests replace the poison path with an unsafe directory, restore it, and
prove the second attempt revokes. Before best-effort custody.json update, poison
contains a durable `record_update:not_confirmed` warning. On update failure,
Rust publishes a **new revoked poison token** containing bounded `record_error`
and `record_update:failed` rather than mutating a stale read token or merely
printing stderr. A diagnostic publication failure is logged but cannot roll back
presence after the original durable revocation; a missing token still refuses.
The non-Linux test lives outside the Linux-only custody module and pins Linux
refusal plus no-custody holder change leaving presence intact. It is not Linux
pidfd evidence and has not been executed on macOS in this Linux gate.

`tests/codex_runner_test.py` is included by `tests/codex_attended_test.py`. It
uses isolated state and actual owned processes with real pidfds, shared flocks
and Popen boundaries, not a running Codex or a provider. It covers generation /
poison before spawn (including after the invoking commit), late poison/exit
with a real spawned child and charged uncertainty, supervisor crash with the
pane alive, independent start/executable substitution and FD cleanup, commit
failure, expired/replaced holder, no enabled/B0-fence bypass, unsupported status,
unloaded/pending queue and daemon replacement, exact argv/environment isolation,
no RPC/wait under flock, observed escaped-descendant cleanup, lost response,
receipt vs ack (including covering ack after daemon death), restart, K=3,
permanent cumulative budget, native abort/human
pause and stable busy/catch-up. The native C2 fixture is unchanged: it still
qualifies daemon-side promotion without a TUI and exactly one captain-authorized
loopback mock request, **not attachment or live operation of this runner**.

Before activation: replay-preserving filter rollover/capacity migration; remaining
queue/client-ID qualification (no equality assumption); real native runner/PTY
composer, same-UUID second-client and idle-navigation barriers; authenticated
runtime pause/reconcile/operator controls and budget/report integration; plus
captain-authorized PR C live qualification. No real login/model/provider use,
production deployment, hosted-eligibility change or Codex-owned Rust edit is
part of B5c. **Paginated installs get no idle mail.** No history mode is changed.

### ACT1: hosted activation behind a captain grant (Linux)

The first mate approved the activation split on 2026-11-04 (ACT1 here, then
ACT2, ACT3 and PR C) and budget option (B). Hosted-pane custody is the
attendance boundary (captain decision 1fc59021), never ATTACHED: every queue
turn is charged as **possibly unattended**.

**Session source.** The captain sets `first_mate.codex_session` to an exact,
existing native session UUID (canonical lowercase; only with `bundled:codex`).
It must be a durable **legacy-history** session: `thread/read` reports
`historyMode:"legacy"`, not ephemeral, cwd equal to the ship folder, and a
materialized rollout whose `session_meta` names that UUID and cwd. Paginated or
unknown history binds metadata only and **gets no idle mail** (C4); no history
setting is changed. Fresh, fork and clear stay refused. Hosted Codex is
refused without this setting, and on non-Linux hosts (Custody::new refuses).
Runtime-created exact sessions (Operator-only `thread/start` on the verified
daemon, zero-turn, recorded durably) are ACT2.

**Flow.** `start --hosted` for harness `codex` places `resume <UUID>` first in
argv (the binding grammar requires it) and sets `PRIVATEER_CODEX_ATTENDED=1` in
the child environment; `codex_binding.prepare` is unchanged (exact UUID, shared
daemon, no daemon-excluding `-c`, bearings prompt), and exec keeps the PID.
Sleep, relaunch, refresh and upgrade rerun `replan`, which rereads the setting,
so the same UUID resumes under a new launch generation and recovers that UUID's
ledger; the launch is recorded as continued. After the trusted resume callback
binds, the supervisor publishes custody (B5b) and, while each 250 ms pass
validates it, starts at most one **runner tick** every 5 s:
`/usr/bin/python3 -I -B codex_runner.py tick <launch.json>` as its own child
(so the runner can pidfd the supervisor), with an empty environment except
fixed PATH/LANG, output to `/dev/null`, its own process group and a 45 s cap.
Retire (sleep, relaunch, supersede, exit) poisons first, then lets an
in-flight pass finish within its cap before killing and reaping it; a killed
pass's queue child is outside its process group, so that cleanup is the
runner's own bounded drain, not a claim that every payload stopped.

**One tick**, all RPC before the project flock: `ship.presence` (current hosted
lease); `mail.pending` after max(accepted, acknowledged, suppressed), validated
as `attended/mail.mjs` validates it; `observe` with the runtime ack cursor;
`publish`; then at most one offer. `publish` sends `wake.hosted` only when the
report is eligible: `trigger:mail` with the exact range once per promoted native
receipt (recorded in the intent before the next offer may be prepared), and a
`delivery` report with receipt/ack-backed `accepted_through`, pause/reason,
declines and busy. It is withheld when no truthful busy start exists. An
uncertain offer is reported `paused`, reason `other`, on the first pass (C3);
there is no resend. A new offer needs an idle, unpaused, eligible report, actionable
(not low-only) mail, and a current grant naming this UUID with turns left. Its
body is the `mail.mjs` text with at most six headers as compact JSON, no message
bodies, and the v1 receipt marker as its first line. `Ledger.prepare`, then
`reserve`, then the existing fenced `dispatch`. A prepared offer waits, without
spending, while busy or paused.

**Grant and budget (Rust, `src/host/codex_delivery.rs`).**
`attended.codex.grant {ship, session, budget}` and `attended.codex.revoke
{ship}` are operator-only commands (OPERATOR_COMMANDS; the runtime Caller, never
a role in the JSON). A grant must name the ship's `codex_session`; its budget
is 1..1000, default 200. `attended.codex.status {ship}` is readable by the
operator or that ship's own agent. `attended.codex.reserve {ship, holder,
session, grant_id, nonce}` belongs to the ship's agent with the current
unslept hosted lease. It spends one turn per nonce, idempotently, so a lost
response retried with the same nonce never spends twice. The count is
persisted per grant in SQLite and is never refunded by ack, rebind, restart,
relaunch or a new lease. Only a new captain grant refills it, with a new grant
id; the old id stops reserving. At or below 10% remaining (at least 1), one
captain notice per grant says a refill is near. Exhaustion pauses with
diagnostic `codex-turn-budget-exhausted` until a new grant id appears, never on
time or mail. The runner pins the reservation in the intent and requires it
under the lock (replacing the launch record's `delivery_enabled`, which binding
still writes `false`). `receipts.json` keeps a second, never-decremented
per-grant floor, so a reset or replayed runtime answer cannot widen a grant.
The local `possibly_unattended_spent` total is reported. K=3 declines are unchanged.

**Revoke ordering.** Revoke, or a grant for a different session, deletes the
grant and publishes custody poison (`delivery_disabled`) inside the SQLite
writer transaction, in the fixed order SQLite writer, then poison, then flock. A
poison failure aborts the revoke, so an unfenced disable is never reported. The
runner reads that poison as its last step before `Popen`, so a revoke committed
first prevents the spawn. A spawn ordered first may still be admitted by Codex,
as the admission-fence section says. Poison is sticky for the launch: delivery
resumes after revoke plus re-grant only at the next pane launch.

**PR 710 carry-ins.** Both fences are read after the final pidfd check, as the
last step before Popen. The ack-only synthetic empty queue is used only when the
ack covers the whole range. `progress` is zero on an empty ledger. Sticky
diagnostics (`codex-custody-lost-after-invocation`,
`codex-queue-cleanup-unproven`) are kept together in a list and are not hidden
by the transient native-evidence diagnostic; older single-diagnostic ledgers
migrate. Exec-failure rejections assert no refund. Native status `active` with
an open rollout turn now counts as consistent busy evidence (previously every
busy turn was "unavailable", so no busy report could be truthful). This
`active` shape comes from the 0.166 app-server schema but has not been observed
in the fixture, which is an ACT3/PR C qualification item.

**Tests (deterministic, no provider).** The Rust unit tests cover
operator-only grant and revoke, the agent's own status, session and budget
bounds, per-nonce idempotent spend, no refund across a new lease, refill only
by a new grant id, holder/grant refusal, revoke poisoning before commit with
rollback on a poison failure, one low-budget notice, and a tick child with a
clean environment and a single pass in flight. The Python runner tests cover a
tick end to end, through the real fenced `Popen` of a stand-in queue binary:
exact marker and body, immediate paused/other uncertainty, then a native
promoted receipt giving exactly one mail wake and `accepted_through` 3. They
also cover low-only and ungranted mail making no offer, busy holding a
prepared offer without spending, exhaustion until a new grant, the local floor
against a reset runtime, unbound/unhosted no-ops, withheld busy-less delivery
reports, mail snapshot validation, and all the carry-ins. The native C2
fixtures are unchanged. **Limitations:** no real Codex PTY yet (ACT3: draft
preservation, idle-to-busy race, same-UUID second client, idle navigation); no
live login or model use (PR C); no filter rollover (ACT2; the filter refuses
at 25% fill, about 4,700 retirements per session); macOS needs kqueue
`EVFILT_PROC`/`NOTE_EXIT`, `proc_pidinfo` start time plus `kern.boottime`,
`proc_pidpath` with device/inode, `LOCAL_PEERPID` and the Mac daemon directory
layout, and stays refused. A restarted pane with an old uncertain offer from a
previous generation withholds reports (the watchdog bounds it) until evidence,
ack or operator resolution settles that offer.

### ACT2: rollover, operator controls and created sessions (Linux)

**Filter rollover** (`codex_receipts.py`). The probabilistic nonce filter is
now checked only for the session being prepared; other sessions get exact
checks (recent digest window, intents, history) and a corrupt filter anywhere
still fails closed. This is safe because every filtered nonce belongs to an
offer covered by its own session's accepted/suppressed watermark, and the
receipt body carries that session UUID and range: a reused nonce can never
match a new body. It removes B5b's compounding false-positive refusals (about
39% at 128 sessions). At prepare, when every intent is final and the target
has no pinned history, a filter at 5% fill (about 840 retirements; false
positives about 6e-6 per prepare) starts a new generation for the **same**
session: an empty filter, `filter_generation`+1, an archive entry (generation,
fill, SHA-256 of the sealed filter, watermarks, time) and `filter_floor` =
max(accepted, suppressed), which prepare enforces. Watermarks, the exact
256-digest window, counts, audits and the ledger are never deleted. Older
saturated filters roll the same way. After 64 archived generations, or while
pinned, saturation refuses as before. A filter-only hit below 5% still refuses
(operator review); nonces are never resampled.

**Operator controls** (`src/host/codex_delivery.rs`, OPERATOR_COMMANDS):
`attended.codex.reconcile {ship, holder, session, notification_id}` and
`attended.codex.resolve {… , operation: abandon|mark_presented, reason}` (1..512
bytes). Each runs `/usr/bin/python3 -I -B codex_runner.py reconcile|resolve
<launch.json>` from the ship folder's own home: a clean environment, its own
process group, the 45 s tick cap, a JSON request on stdin and one bounded JSON
answer. The runner requires the current bound hosted launch, the named holder,
both fences, owned custody and the live lease, and the offer as that session's
current intent. Reconcile runs the tick's own evidence reduction (queue, anchored
receipt, mailbox ack) and answers `pending`, `settled` or `uncertain` with the
evidence; it cannot force acceptance, discard or replay, and uncertainty stays
paused. Resolve reconciles first, so evidence wins (`resolved:false`); otherwise
`Ledger.resolve_operator` records actor, operation, reason and time with the
state change, and Rust audits it. It also reaches an older UUID's outstanding
offer (whose pane is gone) under current custody. The answer warns that late
native admission remains possible; nothing cancels a queued item or acks mail.

**Created sessions** (`attended.codex.session {ship}`, operator only). Fixture
evidence on 0.166.0: a zero-turn legacy `thread/start` writes no rollout, and
`codex resume <UUID>` refuses with `no rollout found`. Native
`thread/name/set` on that UUID materializes `session_meta` (legacy, id, cwd)
with no turn, hook or provider request, after which an exact `codex resume
<UUID>` TUI attaches idle with zero turns. `codex_reserve.py` implements the
reservation protocol with that step: under the project flock it claims
`reservation.json` (generation, owner PID/identity, `reserving`) and refuses a
live owner; it unlocks for the peer-verified daemon's `thread/start {cwd,
historyMode: legacy, ephemeral: false, model?}`; under the lock it saves the
UUID and daemon identity before anything else; then `thread/name/set`, and
`thread/read` of only that UUID must show legacy, not ephemeral, cwd, no turns,
idle, and a rollout of session metadata only (device/inode recorded). A dead
owner without a UUID (a lost create response) is kept as an orphan, never
found by cwd; one with a committed UUID continues with that exact UUID. Failed,
assigned and orphaned reservations are kept (64, then the README's documented
manual cleanup: `kept` is history whose entries no code reads, so every entry
including `assigned` may be trimmed, and only the top-level current record must
never be removed or edited); an attempt that never reached the daemon is not. A
changed daemon refuses. Rust
then, in one writer transaction, revokes and poisons a grant naming another
session, sets `first_mate.codex_session` and audits, then relaunches a hosted
pane (`host::upgrade::relaunch`) onto `codex resume <UUID>`, or leaves it for the
next launch. Only the trusted resume callback can bind, as for any session; a
new grant is still the captain's.

**Model and effort (ACT2 historical limitation; superseded by ACT3a below).** `thread/start` takes `model`; effort only through
`config.model_reasoning_effort` (there is no effort parameter, though
`thread/settings/update` has one). Both show on `thread/read`, but at attach the
TUI appends `event_msg/thread_settings_applied` with its own config: the
fixture saw the model reset to the config default and effort `null`. Neither is
retained. The control passes `first_mate.options.model` to `thread/start`
(recorded), and the pane keeps its daemon-compatible `--model`. A ship with
`options.effort` is refused before `thread/start`: the only effort flag is `-c`,
which excludes the shared daemon, and the captain's config is never changed.

**Nits.** One `Ticker` literal. The tick child and the queue child are now
killed as whole process groups after a normal exit as well as on timeout:
exit is detected with `waitid(WNOWAIT)`, so the unreaped leader still pins the
group ID until the kill, then it is reaped.

**Tests.** Python: target-only filter checks with 127 other matching or
saturated sessions, rollover with watermarks/window/floor/archive, no rollover
while pinned or with a full archive; reconcile uncertain→settled only on
evidence, current binding/custody/offer refusals, audited resolve, evidence
over resolution, old-UUID resolve, the child JSON protocol; reservation order
(UUID durable before name/set), lost response, crash after commit, live owner,
full history, identity mismatch, daemon replacement, non-legacy/turns/records,
unsent attempts; the group kill. Rust: operator-only controls and argument
bounds, the real runner child's refusal, effort/manifest refusal, assign
(poisoned grant for another session), the tick group kill and a bound-launch
real-runner pass. `scripts/codex-reserve-spike.py` runs a fixture-owned 0.166
daemon: a bare thread/start is not attachable; both crash sides; the built
binary's control; the exact-resume TUI attach with zero turns and zero provider
requests, and the settings the attach applied.

**Limitations / ACT3.** `thread/name/set` as the materializing step is an
observed 0.166 behaviour, not a documented reservation API; a future Codex may
change it, and the fixture fails loudly if a bare thread/start becomes
attachable. Same-folder competing callbacks, the bearings turn binding a
created session, model/route equivalence at attach, real-PTY draft
preservation, idle-to-busy race, same-UUID second client and idle navigation
remain ACT3. Sticky runner diagnostics still have no clear path. The control
uses the CODEX_HOME of the runtime invoking it; a pane with another home fails
closed at binding. macOS stays refused.

### ACT3a: model and effort at hosted exact-UUID attach

Mechanism (b) is qualified on installed CLI/app-server **0.166.0** without a
provider turn. `scripts/codex-effort-spike.py` uses one reusable private
`/var/tmp/act3a-probe`, the installed binary directly as an owned app-server
(no package/cache copies), a loopback-only mock and no auth. Every connection
verifies the owned peer/process/home-derived socket; finally waits for both TUI
and daemon exit and removes only its initially absent socket/symlink/lock.
Read-back JSON and logs remain in that root. It is not the managed-daemon
installer or a live Herdr/hosted pane.

Evidence / mechanism comparison:

- (a) After an idle zero-turn exact-UUID TUI attach, public
  `thread/settings/update {threadId, model:"updated-model", effort:"medium"}`
  returned `{}`; a separate `thread/read` reported that model and
  `reasoningEffort:"medium"`, idle, zero turns. This proves idle updates, **not**
  hook-time ordering before the initial bearings turn; production does not use it.
- (b) `codex resume <UUID> --remote unix://<verified socket> --model ship-model
  -c model_reasoning_effort="high"` then read back `model:"ship-model"`,
  `reasoningEffort:"high"`, same fixture `modelProvider`, idle, zero turns and
  zero provider requests. A separate step-1 probe also proved `-c model="ship-model"`;
  production keeps the existing `--model` route flag. This explicit remote path
  bypasses only Codex's *implicit* daemon-selection exclusion, with no embedded
  fallback or provider/permission change.
- Model-only implicit discovery: `codex resume --model implicit-model <UUID>`
  (no effort, config override or explicit remote) read back `model:"implicit-model"`
  on the same owned daemon, replacing `updated-model`. Provider remained the
  fixture provider, idle with zero turns and zero provider requests. The fixture
  PID record includes native legacy timestamp and boot/start identity observation
  fields; no managed installation or package copy is created. Thus model-only
  read-back is qualified too, and its mismatch remains a stopping error.
- (c) `thread/start {model, config:{model_reasoning_effort:"high"}, ...}` returned
  the requested settings. Its retention under implicit resume is still not a
  contract; no speculative alternative field or private database is used.

Implementation: Rust and Python allowlist effort to `minimal`, `low`, `medium`,
`high`, `xhigh`, `none` (with a clear control error for other values), passing it to
reservation config and includes it in hosted exact-session launch argv. Prepare
accepts only that effort override in addition to the existing feature/fullscreen
allowlist. It refuses caller-supplied remote/provider/web-search overrides,
non-hosted effort and ambiguous duplicates. For effort it discovers and verifies
the home-derived shared endpoint **before exec**, inserts its own `--remote`
immediately after `resume` (also preserving flags before the UUID), and records
PID/boot/start/socket identity. Value flags cannot consume flag-like values. The callback checks the same proof, then
reads the loaded exact UUID's configured model/effort before publishing a binding.
When settings were requested, every callback failure stops the initial turn and
leaves UNBOUND, including settings mismatch, unreachable daemon, timeout and
unsupported schema; no callback/runner settings mutation is introduced. These
paths remain refusals, never silent defaults. Explicit model without effort is
also read back; unspecified settings remain native defaults.

The fixture deliberately removes the bearings prompt to prove settings with
zero turns and no hook trust; deterministic tests separately cover prepare,
callback read-back, settings/daemon mismatch, initial-turn stop and refusal of
unsupported combinations. This does **not** qualify draft/race/second-client
behavior, ongoing settings enforcement after another native client changes a
thread, or live model/mail behavior (PR C). Exact-UUID selection, grants,
possibly-unattended budgets, custody poison and provider routes are unchanged.

### B4: Privateer-owned attended-pane proof (reviewed design; implementation split above)

0.166 loaded/idle is not ATTACHED. A future alternative can restrict attended
mail to **Privateer's own hosted, supervised exact-resume pane**, rather than
claiming a public Codex attachment detector. Manual terminals, merely loaded
threads, app-server clients and stale launch records never qualify. This is a
narrower local authorization proof, not proof that the daemon's queue promoter
needs a TUI. It cannot enable delivery until independently reviewed and tested.

**Recorded identity and ownership.** The one hosted supervisor owns the actual
native child and its wait handle. After exec, require the absolute executable
plus device/inode, PID, boot ID and `/proc` start time, and hold a Linux pidfd for
that exact process (PID reuse cannot substitute). The native launch wrapper must
exec, not leave a wrapper whose exit could hide a live payload. Inspect argv for
immutable canonical `resume <UUID>` and the already approved shared-daemon route;
never select by cwd, latest session or process name. The private launch record
binds home/ship, hosted holder/custody epoch, launch generation, CODEX_HOME,
canonical cwd, UUID, and this child identity. A trusted SessionStart **resume**
callback must validate that tuple, exact loaded UUID, daemon peer/socket identity,
project hook trust and transcript metadata, and commit callback generation plus
its durable poison token. A callback alone cannot create ownership or authorize
a different PID/UUID; fresh/fork/clear remain unbound.

**Liveness acquisition and observation.** The supervisor acquires the pidfd while
it still owns the unreaped child, verifies boot/start/executable again, and
retains both pidfd and wait custody until exit has been consumed and durably
recorded. Never reacquire by PID from an old registry snapshot alone. Observe
pidfd readiness plus `waitid`/`waitpid` on the actual child; terminal/PTTY closure
and killing a wrapper are not process-exit proof. On supervisor restart, missing
wait custody is unqualified: keep delivery off and require an explicit hosted
relaunch/rebind, not a `kill(pid, 0)` reconstruction. A successfully acquired pidfd
pins identity, **not liveness forever**. Executable replacement, daemon replacement,
supervision loss, holder change or child exit immediately revoke local eligibility.

**Final spawn authorization.** Outside the lock obtain authenticated custody,
read-only native idle/receipt evidence and a prepared bounded argv. The runner
must share a serialized local authorization path with the hosted supervisor's
custody/exit transitions (not a delayed asynchronous Boolean cached by Python).
Under the project fence lock, reread launch and callback generations, bound and
enabled flags, poison, exact UUID/intent, live holder/epoch, and the supervisor's
owned handle/child identity; test pidfd exit readiness again immediately before
creating the native queue child. Missing proof refuses. Save `invoking` durably
and spawn under that same authorization, then unlock before any wait/RPC/report.
Exit processing must publish poison before waiting for the lock, durably mark
unbound/disabled and preserve old intents. Use a fixed lock order and bounded
transactions; never hold the hook lock across supervisor RPC or child waiting.
How Rust custody and the Python spawn path share this local boundary is an
implementation prerequisite, not solved by trusting an environment variable.

**Exit races and irreversible delivery.** If exit is observable before the final
check, refuse; if observed after `invoking` but before a proved exec failure,
keep the old UUID's offer uncertain and immediately pause/report. Check handle,
generations/custody again after spawn and before reporting; an exited/replaced
pane may not renew or report for its successor. Reconcile old native evidence or
covering mailbox ack; never migrate/replay into a replacement UUID. Cleanup only
the owned queue child with bounded wait/descendant identity checks; do not signal
the shared daemon. A pane can die in the kernel **between the last liveness check
and spawn/admission**. Neither a pidfd nor a state lock can atomically prevent
that or retract an admitted queue item. In this mid-delivery case "fail closed"
means no further offers, truthful uncertainty and immediate diagnostics, **not**
a claim that no late unattended turn occurred. A request ordered before exit
may still cost one nudge on the old thread. If review requires zero post-exit
model turns, this local design is insufficient; it needs conditional native
admission/cancel-before-promote, or separately approved unattended-turn budget
accounting. Do not call a child kill cancellation or fabricate an ack.

**UI continuity limitation.** Privateer can prove its hosted exact-resume child
is alive and was bound by a trusted callback. It cannot currently prove that the
child still displays that UUID: SessionStart is deferred to turn entry and an
idle TUI may navigate/clear before a new callback. A second client can also
resume the same UUID; project hooks are not per-pane authentication. No PID,
loaded/idle status, source/originator or hook trust hash closes those gaps.
Therefore this proposal proves an owned-live-pane authorization at invocation,
not arbitrary current TUI subscription. Opus must explicitly accept that weaker
attended boundary or require upstream proof; **no activation is implied**.

**Upstream ask.** Provide an authenticated public TUI attachment identity:
daemon-issued client/attachment ID bound to an OS peer/connection and exact
thread UUID, with a launcher nonce echoed in SessionStart, current attach epoch,
ordered attach/detach/navigation events and a read-only restart/reconnect
snapshot. Queue admission should accept the expected attachment ID/epoch and
refuse if detached before promotion; specify durable admission lookup by stable
nonce and cancellation boundaries. Generic app-server subscribers and creation
origin are not TUI attachment identities. No custom config/route flags, private
SQLite inspection or patched Codex TUI substitute for that API.

Before any implementation: deterministic barriers for exit before/after final
check/spawn, PID reuse, exec identity change, wrapper-vs-payload, supervisor crash,
custody replacement, poison lock timeout, same-UUID second client, idle navigation,
daemon replacement and lost response; real PTY/owned subprocess boundary tests.
Record late-admission/turn budget honestly. This design edits none of the
Codex-owned Rust files and implements no supervisor, runner, control or activation.
**Paginated installs get no idle mail**, and no history-mode setting is changed.

### B2 ledger and lost-response rule

`plugins/attended-codex/codex_receipts.py` is dormant: no polling, queue child,
report, acknowledgement, hosted eligibility or activation. Every mutation reloads
under the project lock and atomically fsyncs the ledger, capped at 8 MiB/128
sessions without evicting unresolved evidence. One unresolved nonce across UUIDs
blocks all new intents; fork/relaunch cannot bypass it. Accepted/ack watermarks
and settled nonce digests survive explicit same-UUID restart. B3a removes
historical settled offer bodies below the accepted watermark; operator audits
remain durable, as does the current intent's complete evidence. The bound materialized (`legacy`)
rollout must exist, match native metadata/cwd/UUID and dev/inode, and end at a
complete record. Forward scans consume at most 1 MiB, persist only complete
record boundaries, and pin metadata and a boundary hash as well as file identity.
This assumes native append-only history; it does not cryptographically verify
every earlier byte on every scan. Replacement/truncation at the checkpoint,
compaction/rollback or unsupported turn schema pauses; no anchor is reset.

Native 0.166 rollout `event_msg/task_started` opens a turn; a mail receipt
requires its `turn_attribution.turn_trigger` to be exactly `queue`. Exact
`event_msg/user_message` with the full saved body and native client ID records
presentation within that queue-triggered open turn, and matching `task_complete`/`turn_aborted`
closes it. B4 now qualifies the text-only user-message/client-ID shape against
real 0.166.0 records from its one-request loopback fixture, as recorded above;
unsupported shapes still refuse with a clear reason. Assistant prose,
`response_item`, hook-stopped turn-start, queue echo and queue disappearance are
not receipts. The exact queue schema uses `clientUserMessageId` and text
`text_elements`. Future changes must qualify schemas, not guess aliases.
Stopped native fixture turns still cannot qualify accepted user-input receipts;
the B4 scope change is what allowed that missing evidence. Positive ATTACHED
proof, a reviewed runner and the remaining integration/activation gates are
still absent; receipt qualification alone does not enable delivery.

After a lost CLI result, on the very next reconciliation:

1. An exact queue body/nonce plus native client ID establishes `queued`, **not**
   presentation or acknowledgement. This remains one pinned outstanding offer.
2. An exact anchored native user-input/turn association establishes `presented`
   and commits accepted progress before any future reporting. This also works
   after restart, beyond a tail window and across bounded scan batches.
3. A runtime `acknowledged_through` covering the range settles `acked` without
   fabricating a receipt or turn. Partial ack preserves the unresolved suffix.
4. A durable `prepared` offer has not been attempted; a runner may reject an
   `invoking` offer only on positive local exec-failure proof **before any child
   or request existed**. CLI nonzero/stderr/success prose, timeout, disconnect,
   process death and queue absence alone are never that proof.
5. With no positive evidence the offer stays `uncertain` and paused, unchanged
   decline count. B3 must surface this immediately through reconcile/diagnostic
   reporting rather than waiting for the five-minute watchdog. It must never
   reoffer merely because time elapsed. There is no qualified CLI API to prove
   negative admission in this case; an exact admission-result/idempotency API
   upstream would be needed for guaranteed automatic recovery. This limitation
   is explicit, not a claim that every lost response self-heals. The first mate
   accepted this C3 boundary on 2026-11-03, subject to immediate diagnostic,
   automatic later evidence/ack recovery and audited operator-only resolution;
   independent review is still required and delivery remains off.

`Ledger.invoking` is a dormant test/state API, **not** an admission fence: B3 must
combine intent-state commit and actual child creation in its own under-lock final
authorization transaction. Calling it and then spawning unlocked is forbidden.
Pre-request-failure evidence and ack cursors are trusted caller inputs from that
runner/runtime, never commands inferred from mail text.

The returned `diagnostic` is immediately available in uncertain/invoking state,
is bounded below 1 KiB, contains no mail body, and has stable ID
`codex-uncertain-<notification UUID>`. B3 must surface it to the first mate at the
first reconciliation, coalescing by that ID (one outstanding offer), not enqueue
one alert per poll or wait for the watchdog. Later exact native evidence or
covering runtime ack automatically clears it without an operator step.

B3's explicit **operator-only** control contract is `attended.codex.resolve`
with exact `ship`, `holder`, `session`, `notification_id`, `operation` (`abandon`
or `mark_presented`) and a nonempty bounded `reason`. It must validate the native
runtime Caller is operator (never a role supplied in request JSON/environment),
require current custody/identity, and audit the exact resolution. It cannot be
called by crew/first mates, inferred from mail, or used as a blind replay or ack.
B2 supplies the dormant `Ledger.resolve_operator` reducer and deterministic
role-refusal/stale-nonce/transactional-audit tests, **not** the dispatched native
control; B3 must add and integration-test its authenticated operator-only gate
before activation. Its trusted `caller_role` parameter is not itself runtime
authentication. The ledger transaction stores operator actor, operation, reason,
time, exact session and nonce with the state transition. `abandon` suppresses
future automatic reoffers through that range without claiming acceptance or
acking mail. It does not cancel an admitted native item/turn. `mark_presented`
records distinct `operator_presented` provenance, not a fabricated native receipt
or mail turn. Neither invents a mailbox acknowledgement. Source-side accepted
progress must retain this distinction in B3 reports. The operator must be told
that late native admission remains possible when resolving uncertain offers.

`python3 -B scripts/codex-queue-spike.py --binary <built-privateer> --codex <codex>`
is the separate opt-in C2 fixture. It uses only fixture-owned legacy thread
creation on its peer-verified daemon and an exact-resume real TUI. A
fixture-only UserPromptSubmit hook (in addition to SessionStart) stops **every**
queued turn before provider requests; SessionStart alone would not protect later
turns. It compares the attached pane with a loaded thread that has never had a
TUI, using queue CLI `--remote`/`--thread` with no `--resume` or turn/start. Native
queue turn provenance plus stopped hook/terminal evidence identifies promotion;
English CLI output is discarded. No production history configuration is changed.
The initial 0.166 probe also refused exact TUI resume of a zero-turn legacy
`thread/start` UUID with `no rollout found`; merely choosing legacy mode is not
a fresh-reservation fix. The fixture therefore queues one hook-stopped seed turn
on that **returned exact UUID**, then exact-resumes it (no cwd/newest selection).
This fixture-only seed is not a production reservation or approved fresh binder.
**C2 observed result (Codex CLI and fixture daemon 0.166.0, Linux/aarch64):**
the daemon promoted the no-TUI case. The fixture kept the thread loaded through
its app-server client, with no TUI ever attached; only the exact `codex queue`
invocation supplied input. It observed exactly one `turn/started`, then stopped
completion, an empty queue, and a materialized native
`task_started.turn_attribution.turn_trigger:queue`. The exact-resume attached TUI
also reached UserPromptSubmit with the exact queued probe text. Both the new
queue fixture and the original binding fixture recorded **zero provider
requests**, and verified owned-daemon process exit/cleanup. This proves daemon-side
promotion even without a TUI, not detached inference safety. **B3 must require a
positively proven ATTACHED pane**, not merely loaded, before enabling delivery
(or separately implement and qualify explicit unattended-turn budget/reporting).
A stale binding or mere loaded listing is not attachment proof. Stop hooks block
before native user-input acceptance, so this native result is not a presentation
receipt or model-turn qualification.

Evidence: serial admitted job `ea12d1ff-2e15-46f8-bda4-71100c7b0fd6` at
`d5ce6b57e7824fa18573c6b6d65c755519d7ff2f` passed all eight steps: build,
queue-promoter fixture, Codex Rust units (2), Python binding/receipt tests (68),
plugins (36), attended (24), bytecode guard (1), original native binding fixture.
The job reserved one CPU, 1 GiB memory and 1 GiB incremental disk, waited for
capacity, and used the task-private state isolation above plus pinned Rust caches.
No Rust files changed, so fmt/clippy were not required. Earlier gate attempts
caught the missing module bytecode guard and the zero-turn legacy resume failure;
both were fixed before this complete passing gate. This is B2 evidence only:
C1 migration/fence and C3 recovery are tested, C2 requires attached-only future
admission, C4 remains **no idle mail for paginated installs**. No queue runner,
public resolve control, successful native user-input receipts, typed-draft
preservation, K=3/busy/watchdog integration, live model/mail trial or production
deployment is claimed. Those remaining activation requirements belong to B3/C.


- **A:** revised binding, stable hook, socket verification, `delivery_bind` and
  daemon-proof fixture only; no queue delivery, hosted eligibility or README
  claim. Target at most about 800 production lines, one implementation commit.
  Stop for a decision if the per-thread hook spike fails.
- **B0:** reset before validation, stable project dispatcher, public launch-refusal
  diagnostics and allowlisted reports; explicit exact-UUID resume only, rejecting
  fresh/fork/clear identity changes. Separate gate/PR before queue delivery.
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

For future B implementation after approval: one serial admitted heavy gate with
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

Step-1 follow-up evidence (2026-11-03): commit `04be30c66dc02b3b0027f14e395563c6cad3f46f`
fixes positional-trust duplicate removal and requires system Python >=3.9.
Admitted serial heavy job `f86ebaa9-f130-4c54-8697-263e4e28c7ee` exited 0:
`cargo test --locked --lib codex` (2 tests),
`python3 -B tests/codex_attended_test.py` (39), and
`cargo test --locked --test plugins` (36). It used the task-private state-path
isolation above, one CPU, 1 GiB memory and 1 GiB incremental disk reservation.
This is a plugin follow-up gate, not the full Rust gate or a new native-daemon
probe. The subsequent reservation/delivery text is design-only, based on the
existing fixture and reviewed upstream source; no new live/native probe was run.

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
