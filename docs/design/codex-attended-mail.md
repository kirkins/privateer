# Codex attended idle mail (proposal)

Status: PR A merged; B0 binding hardening approved, exact-UUID resume only.
No delivery or live qualification. Following Opus's conditional go and the first
mate's approved split, B1 implements only the pre-lock poison prerequisite and
known-B0 dispatcher migration. B2 adds a dormant materialized receipt ledger
and opt-in queue-promoter fixture; B3 delivery remains unimplemented. The fresh
reservation decision below is design only, not implementation approval.
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
each implementation reviewable. B1 is merged; the current slice is B2 only and
stops after its gate/local commit. No intermediate enables `delivery_enabled`,
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
  B2 also pins the B1 body from PR 679. Only those exact bytes are migrated under
  prepare's lock after live-owner refusal. Native command/trust are unchanged; unknown bodies still refuse.
- **B2:** C2 native queue-promoter/loaded-without-TUI fixture, a dormant
  materialized receipt ledger and deterministic lost-response recovery tests.
  These are not B1 evidence. Native results and the C3 limitation are recorded
  below; B2 does not implement a queue runner or turn on delivery.
- **B3:** native queue child and under-lock invocation fence; poll/control/report,
  pause/K=3/stable-busy integration and the combined gate. Activation requires all
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

### B2 ledger and lost-response rule

`plugins/attended-codex/codex_receipts.py` is dormant: no polling, queue child,
report, acknowledgement, hosted eligibility or activation. Every mutation reloads
under the project lock and atomically fsyncs the ledger, capped at 8 MiB/128
sessions without eviction. One unresolved nonce across UUIDs blocks all new
intents; fork/relaunch cannot bypass it. Settled offer history and accepted/ack
watermarks survive explicit same-UUID restart. The bound materialized (`legacy`)
rollout must exist, match native metadata/cwd/UUID and dev/inode, and end at a
complete record. Forward scans consume at most 1 MiB, persist only complete
record boundaries, and pin metadata and a boundary hash as well as file identity.
This assumes native append-only history; it does not cryptographically verify
every earlier byte on every scan. Replacement/truncation at the checkpoint,
compaction/rollback or unsupported turn schema pauses; no anchor is reset.

Native 0.166 rollout `event_msg/task_started` opens a turn, exact
`event_msg/user_message` with the full saved body and native client ID records
presentation within that open turn, and matching `task_complete`/`turn_aborted`
closes it. Assistant prose, `response_item`, hook-stopped turn-start, queue echo
and queue disappearance are not receipts. The exact queue schema uses
`clientUserMessageId` and text `text_elements`. B3 must qualify any schema change,
not guess an alias. Deterministic tests cover these native shapes; stopped native
fixture turns deliberately cannot qualify accepted user-input receipts, because
the fixture blocks input before provider use. Successful promoted-input receipt
qualification remains a B3 acceptance requirement before activation.

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
