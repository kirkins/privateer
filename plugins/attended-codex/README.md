# Codex attended binding (experimental, exact-UUID resume only)

This is the binding boundary of the [attended-mail design](../../docs/design/codex-attended-mail.md).
ACT1 (Linux) makes a hosted Codex first mate resume the captain-set
`first_mate.codex_session` (an existing legacy-history UUID). Its supervisor runs
`codex_runner.py tick` every 5 s, and delivers mail through `codex queue --remote
… --thread <UUID>` **only under a captain grant** (`attended.codex.grant`, budget
1..1000, default 200 possibly-unattended turns, never refunded;
`attended.codex.revoke` poisons custody). No code grants delivery by itself.
It does not implement scheduled work, qualify native autonomy or claim ATTACHED.
The main attended-support table is unchanged until live qualification (PR C).

**Pre-spawn loaded-thread refusal.** An idle `/new`, an unloaded bound UUID,
ambiguous cwd/UUID ownership, an incomplete loaded-list page (missing or non-null
`nextCursor`), or more than 32 loaded threads withholds delivery **before a grant
reservation or invocation**. The local receipt ledger records
`runner.pre_spawn_fence`, named
`codex-loaded-thread-fence-refused-before-invocation`, with `at_ms`, launch/callback
context, and bounded facts: `loaded_count`, `threads_in_cwd`, `bound_uuid_loaded`,
`cursor_state` (`missing` / `null` / `non-null`), and `cwd_thread_is_bound`.
Unknown facts are null; incomplete/oversize pages are never followed. No message
bodies, cursor values or credentials are recorded. The last 16 state changes are
kept in `runner.pre_spawn_fence_events`; unchanged five-second refusals do not
add events or refresh the diagnostic's timestamp.

From the ship folder's own home, `attended.codex.status` includes this local
state under `runner` (and reports `runner.unavailable` if it cannot read the
adapter state). Its `pre_spawn_fence.clears_when` explains recovery: a later
passing pre-spawn fence, or a trusted rebind / hosted relaunch. Ticks recheck the
fence even while this diagnostic pauses mail; status itself is read-only and
never contacts Codex. A refusal leaves the offer prepared, never uncertain,
and never redirects it to another UUID. This is **non-sticky** and separate
from `codex-loaded-thread-fence-lost-after-invocation`: post-spawn custody loss
remains sticky, even after a later passing fence, rebind, or relaunch. Only the
explicit audited operator clear below can remove it; no native atomic-admission
race is closed.

The full runtime status lookup (including its local diagnostic adapter child)
now runs **before** the dispatch fence. `reserve()` takes only that prefetched
grant snapshot on this path; the Rust reservation still authorizes the current
grant and charges exactly once per nonce. Stale snapshots cannot authorize an
uncharged spawn. A failed refresh of an existing refusal (unreachable daemon or
malformed response) retains it and still publishes observation/receipt progress.

**Remaining non-atomic window:** after the loaded-list snapshot there are the
bounded `thread/read` calls, queue check, lifecycle scan/ledger writes, the Rust
reservation RPC (unless already pinned), presence RPC, lock acquisition, receipt
scan, durable invoking write and local custody checks before `Popen`. There is no
second loaded-thread RPC after charging: a refusal there could not honestly be
called unreserved, because Rust never refunds a turn. An idle `/new` in this
remaining window can still be detected only after spawn and become the existing
sticky `codex-loaded-thread-fence-lost-after-invocation` pause. The full status
child is no longer in that window; an idle `/new` during that lookup is caught
before reservation. This reduces the reviewed regression without claiming an
atomic Codex watched-thread admission fence.

**Audited sticky-pause recovery (#765).** From the ship folder's own home,
`attended.codex.status` lists every sticky name in `runner.diagnostics` and its
exact clear argv in `runner.clear_pause` (replace the reason placeholder).
The operator-only command is `attended.codex.clear_pause` with JSON fields
`ship`, `holder`, `session`, `diagnostic` (one exact sticky name), and `reason`
(nonblank, at most 512 UTF-8 bytes). For example, save this as a private JSON
file outside the checkout, replace the illustrative session and reason with
the values from status, then run the command as the operator:

```json
{"ship":"alpha","holder":"host:alpha","session":"11111111-1111-4111-8111-111111111111","diagnostic":"codex-loaded-thread-fence-lost-after-invocation","reason":"Closed the extra thread and verified the attended pane watches this UUID"}
```

```sh
privateer --home /path/to/alpha/.privateer attended.codex.clear_pause --file /path/to/clear-pause.json
```

Agents are refused, just as for grant/resolve. The command requires the exact
current hosted binding and holder, live presence, valid invalidation and custody
fences, live pane/supervisor identities, the same native daemon, exactly the
bound UUID in this cwd, qualified idle native state and a complete empty queue.
It refuses an invocation/cleanup still in progress or a concurrent runner or
binding change. Repair the underlying condition first; a clear is not an
acknowledgment that it is acceptable to deliver to an unwatched thread.

It clears only `codex-loaded-thread-fence-lost-after-invocation` or
`codex-custody-lost-after-invocation`, only when named and still active.
`runner.sticky_clear_audits` retains audit UUID, actor `operator`, OS uid,
diagnostic, reason, timestamp, current launch/callback/session/holder context,
custody epoch and the old loaded-thread reason. The audit and removal commit
atomically; full storage refuses instead of evicting audits. Rust also records
the ledger audit in the runtime audit log. SessionStart, relaunch, passing fences,
receipts and status never clear a sticky entry. Every uncleared sticky withholds
success reports, including after an old offer is positively settled.

Clearing does **not** settle, acknowledge, abandon or replay an offer, restore a
spent turn, reset declines, or bypass another pause. An uncertain attempt stays
uncertain and charged; use the separate reconcile/resolve controls when needed.
Delivery resumes only once all independent blockers are resolved, and only for
eligible newer mail. Native queue observations are not atomic admission proof.

**Known limit:** `codex-queue-cleanup-unproven` is shown with `supported:false`
and an explicit refusal. This command cannot clear it: older cleanup failures
saved no process identities, so an idle/empty Codex queue or hosted relaunch
cannot prove an escaped payload stopped. The operator must independently inspect
and confirm there is no stray Codex process for that ship. A supported audited
process-remediation reset needs follow-up work; this command remains fail-closed
and does not provide a force flag or silently discard that pause.

ACT2 adds captain-only controls, each run from the ship folder's own home:
`attended.codex.reconcile` and `attended.codex.resolve` (audited `abandon` or
`mark_presented` with a reason) for one uncertain offer, and
`attended.codex.session`, which creates a durable zero-turn legacy thread on the
verified shared daemon (`codex_reserve.py`: `thread/start`, durable UUID,
`thread/name/set`, exact verification), sets `first_mate.codex_session` and
relaunches the hosted pane onto `codex resume <UUID>`. A ship with
`first_mate.options.effort` was refused in ACT2; **ACT3a** qualifies the narrow
hosted exact-resume path below instead. A saved
`verified`/`reserved` UUID is adopted on a retry regardless of the request's
current session; the re-run zero-turn verification is what bounds re-adoption.

**Reservation history cleanup.** `reservation.json` keeps the live top-level
record plus up to 64 earlier reservations in `kept`. No code reads an individual
`kept` entry (claim only carries the list forward and reports its length), so
`kept` is history only and every entry in it, `assigned` included, is safe to
remove. `first_mate.codex_session` is what names the UUID a pane resumes. When a
control answers `reservation history full; operator cleanup required`, stop the
ship's hosted pane and no other control may be running, then edit
`$SHIP_FOLDER/.codex/privateer-attended/reservation.json` (mode 0600) by hand:

- replace the `kept` list with only its newest N entries (the list is oldest
  first, so keep the last N, N < 64), or simply set `"kept": []`;
- never remove or rewrite the top-level record, and keep its `pid`,
  `process_identity`, `generation`, `session`, `daemon`, `rollout` and `start`.

This drops only Privateer's duplicate history, not the native threads. Leave the
JSON valid and the file private; the next reservation then has room.

**ACT3a: per-ship model and effort at attach.** The hosted launch carries
`first_mate.options.model` as `--model` and effort as
`-c model_reasoning_effort="VALUE"` (`minimal`, `low`, `medium`, `high`, `xhigh`
or `none`; other values refuse with an explicit error). For effort,
prepare connects to the home-derived, peer/process-verified shared app-server and
inserts its own `--remote unix://<verified socket>` into the exact-UUID resume
argv. Explicit remote avoids Codex 0.166's *implicit* daemon exclusion for `-c`;
it does not change the provider, permissions, hook trust or global config.
Prepare records that daemon/socket identity. The trusted callback must see that
same daemon and read back every explicitly requested model/effort from the loaded
UUID's `thread/read` before binding. When settings were requested, any callback
failure (including timeout, unreachable daemon or unsupported schema) leaves
UNBOUND and stops the initial turn, rather than infer with defaults. No model/effort
mutation occurs in the callback or mail runner. Grants, budgets and custody
poison are unchanged.

Only this adapter-generated endpoint is allowed: caller-supplied `--remote`,
provider/config overrides (including `web_search`), non-hosted effort, fresh or
ambiguous/duplicate selectors still refuse. A missing daemon refuses before exec;
there is no embedded fallback. Unspecified settings keep native defaults. This
is settings retention qualification, **not** a live hosted-mail/autonomy pilot.

Reproduce with the installed executable (no package copies):

```sh
python3 -B scripts/codex-effort-spike.py --codex /absolute/path/to/codex \
  --root /var/tmp/act3a-probe
```

The one reusable, private root retains `evidence.json` and PTY/daemon logs. The
fixture starts the installed app-server directly, verifies its peer, attaches a
zero-turn exact-UUID TUI, tests idle `thread/settings/update`, checks model-only
`--model` on the implicit-daemon route, then attaches with the production
prepare-generated model/effort/remote flags (including flags before the UUID)
and reads both back.
It deliberately strips the bearings prompt (no turn or hook trust is exercised),
requires zero loopback provider requests, and always waits for owned process exit.
On CLI/app-server **0.166.0**, `--model ship-model` plus effort `high` read back
exactly after changing the loaded thread to `updated-model` / `medium`; provider
remained the fixture provider. Model-only implicit attach also read back exactly
(`implicit-model`, distinct from the prior thread model), so model-only mismatch
continues to stop the initial turn. Idle settings/update also worked, but is not used
as a potentially racy first-turn update. `thread/start` config effort alone is
not relied on across attach. No credentials, real inference or live pane is used.

## ACT3b: PTY qualification and remaining attachment gap

Reproduce against the installed binary and a **new**, private fixture root:

```sh
python3 -B scripts/codex-attended-pty-spike.py --codex /absolute/path/to/codex \
  --root /var/tmp/act3b-pty-probe
```

The fixture uses real PTYs and an owned, peer-verified daemon with isolated homes,
a fixture-only presence/report double and trusted stopping hooks. Exactly one
nonce-selected mail reaches its own loopback mock; there is no login, real
provider request, Rust hosted supervisor/grant, live pane or Herdr session.
Logs remain in the root. Native 0.166.0 evidence is checked into
`tests/fixtures/codex-attended-pty.json`; offline tests replay its receipt and
assert its negative attachment/navigation findings. See the design's ACT3b
section for barriers and scope.

- An unsent draft survives a queued mail turn and reaches its submit hook exactly
  once, only after explicit captain Enter.
- In both delivery-first and captain-submit-first races, exact captain/mail
  prompts reach their hooks once. Delivery-first preserves typing and native
  Tab-queued follow-up input; captain-first holds exactly one pending native
  mail until the captain turn ends. The one accepted mock mail has an exact
  queue-triggered turn/client receipt, recovered after reducer restart with
  accepted 1 / ack 0. Hook-stopped inputs are **not** presentation receipts.
- After both clients exit, a new production prepare-generated exact-UUID resume
  on the same daemon fires SessionStart and binds with explicit `ship-model` /
  `high` read back. An owned-daemon restart plus explicit relaunch also recovers
  the same UUID/watermark without replay. Dead PID state is not renewed custody.
- Idle `/new` fires no callback: the old binding remains bound until a new-UUID
  turn enters SessionStart, then becomes unbound with sticky custody poison
  `callback_uuid_changed`. This is not instantaneous navigation detection.

**Known upstream per-client identity gap:** an idle second TUI attaching the
same UUID fires no SessionStart, and its first submit emits only
UserPromptSubmit with no client/attachment identity. Neither is fenced; the
binding remains bound. Public idle/loaded fields cannot prove exclusive/current
TUI attachment. This negative evidence does not block the separately authorized
PR C pilot, but is not second-client safety qualification. Another client can
also change shared model/effort; attach read-back is not continuous enforcement.
Upstream needs authenticated per-TUI attach identity/epoch, ordered navigation
and detach events, a reconnect snapshot and conditional queue admission.

When a same-UUID **resume callback does occur after custody is already owned**,
the adapter conservatively poisons custody before taking the state lock:
`resume_callback_requires_relaunch`. The runner pauses with `custody poisoned:
resume callback requires explicit hosted relaunch`; later compact/rebind or a
runner restart cannot clear it. Only a new explicit hosted launch can do so.
Deterministic dispatcher/direct-handler/runner tests prove this fence, including
lock timeout. It does **not** detect the silent second-client case above.
Pane-exit/spawn and daemon-replacement races retain the existing pidfd/poison and
possibly-unattended budget boundary. No live mail/autonomy/deployment claim is
made; paginated installs still get no idle mail.

Explicit opt-in: set `PRIVATEER_CODEX_ATTENDED=1` for a Privateer attended Codex
launch **with `codex resume <exact UUID>`**. Fresh launches are refused before
exec: a stable project hook cannot prove which pane created a new thread when
another trusted same-folder TUI fires first. No newest-thread selection, UUID-age
heuristic or daemon listing may stand in for the UUID in the pane's own argv.
Headless launches and other harnesses ignore the opt-in.

The launcher preserves native exec/process identity and route argv. It installs
a stable SessionStart command running `/usr/bin/python3 -I -B` on
`.codex/privateer-attended/hook.py`, preserving unrelated handlers. This literal
system interpreter path does not move with version managers; launch is refused
if it is absent or not executable, with no interpreter fallback. Isolated Python
ignores PYTHONPATH, PYTHONHOME, user site packages and the script directory; only
the validated release-handler directory is added. This version-independent project
dispatcher reads
the current private launch record to select that launch's release handler. A
Privateer release change or new launch generation does not change the trusted
command. The dispatcher refuses unexpected existing contents. The captain must
trust the declaration through native `/hooks`; no trust state or global config
is changed. A declaration still invoking the old PR A `codex_binding.py` handler
with this registry **refuses the launch**, even outside SessionStart: that handler
could otherwise bind an unproven fresh thread. Those legacy release-handler
declarations are never silently removed or re-trusted: review and remove them
through native hook management before relaunch. Prepare replaces only this
project's earlier exact `INTERPRETER -I -B <this hook.py>` declarations with one
system-interpreter declaration, leaving unrelated declarations and trust untouched.
Changing from the launcher's interpreter to `/usr/bin/python3` changes the trust
hash: **review and re-trust the new declaration through `/hooks`** before binding.
An older non-isolated dispatcher also requires explicit review/removal before
reinstall; its command trust hash changes with `-I`. Unexpected dispatcher file
contents are never silently overwritten, except for explicit migrations of the
**exact known B0, B1, B2, B5a and ACT3a dispatcher bytes** (SHA-256 pinned in `codex_binding.py`,
fixtures in `tests/fixtures/codex-dispatcher-b{0,1,2,5a}.txt` and
`tests/fixtures/codex-dispatcher-act3a.txt`). Prepare upgrades that body atomically
only after refusing a live owner and while holding the project lock. A changed
B0 body is refused. The command/declaration and native trust remain untouched;
the new launch is UNBOUND and delivery-disabled, never grandfathered in.

The per-project `.codex/privateer-attended/launch.json` is private runtime state,
not source to commit. It records the exact launch, process/boot identity, current
custody and binding diagnostic. Only one live launch per project is supported;
concurrent loaded conversations in that cwd fail unbound rather than guessing.
A stale record may be replaced after its process identity is proved gone.

SessionStart is deferred until first turn entry. Hosted opt-in **resume** launches
without a supplied prompt get a positional charter/inbox bearings prompt. A manual
zero-turn resume pane stays UNBOUND. Fresh, fork and clear callbacks invalidate
any prior binding and are refused: relaunch explicitly with the desired UUID.
Compact callbacks must match both the launch UUID and its previous binding.

Prepare-time refusals (including unsupported argv, missing context, unsafe local
state and unqualified `-c` overrides that exclude the daemon) **refuse the
opted-in launch**, with a diagnostic. They do not merely disable mail while
letting the TUI launch. Callback-time failures instead leave the running pane
UNBOUND and mail disabled. Invalidation is persisted before daemon connection,
release-handler loading, callback validation or reporting, so a pruned, unreadable
or unsafe handler cannot preserve an older binding. Handler paths must be absolute,
regular owner-owned files named `codex_binding.py`, not group/world writable.
Short state transactions use bounded lock waits, never holding a lock over daemon
calls or reporting. **Before waiting for the lock**, every dispatcher/direct
callback first removes old `invalidate.json` and fsyncs the parent **before**
creating a temporary file, then atomically publishes a fresh private UUID token
and fsyncs the file and parent directory. An allocation/write/fsync/rename failure
therefore leaves no usable old token. Concurrent lstat/unlink FileNotFoundError
means another publisher already removed it; both paths still fsync and publish
a fresh token instead of aborting. Unsafe existing files still refuse.
Prepare also rotates the token before
replacing its launch record, fencing stale in-memory records. A timed-out callback or missing release thus
poisons the old binding even if it cannot update `launch.json`. Publication/storage
failure aborts the callback; an unreadable/missing/unsafe fence refuses future
admission. Each callback invalidation also advances a generation. Final binding
compares both generations and its own still-current request token under the lock:
an in-flight resume cannot replace a newer invalidation, including poison-only
invalidation whose lock wait timed out. A validated binding records
`fence_version:1` and `invalidation_token`; B0 bindings without them fail the
`require_fence` prerequisite even if they say bound/enabled. Only a new fully
validated callback restores proof, never elapsed time, a retry, or `last_binding`.
This is an availability fence, not cancellation of an already spawned request.
Future admission must check the token under the lock alongside custody, both
generations and exact UUID immediately before child creation; B1 has no such
child or delivery activation. The last good binding persists as `last_binding` solely for
compact validation across overlapping callbacks. It is not an active binding or
delivery authorization: invalidation removes `binding`, sets `state:unbound` and
keeps delivery disabled. A new launch does not inherit this continuity. This
implementation requires Linux `/proc` and Unix peer credentials.

The callback verifies its exact UUID on the same home-derived daemon, its native
cwd/transcript metadata and trusted project declaration, then emits only the
existing hosted `delivery_bind` report. Report subprocesses receive an allowlisted
launch-state environment, never the shared daemon's credentials or foreign actor.
The socket uses WebSocket JSON-RPC over Unix. Upstream puts its physical endpoint
in a fixed `/tmp/codex-daemon-<uid>/` directory, independent of CODEX_HOME/TMPDIR;
the home rendezvous, expected hash, private modes, peer PID/UID and boot/start
identity must all agree. A dead, replaced, unsupported or mismatched daemon is a
refusal, never an embedded server. The adapter cannot call queue/add or turn/start.

Paginated native history may have no JSONL rollout even inside SessionStart.
The binding records `metadata_source:daemon_thread_read` and does not pretend
that receipt/byte-offset recovery is qualified. **Paginated installations get no
idle mail.** No history-mode setting is changed to work around that limitation.
Every record explicitly has
`delivery_enabled:false`. B2 includes a **dormant**, bounded materialized-history
receipt ledger in `codex_receipts.py`, with transactional same-UUID recovery,
exact promoted-input vs queue/ack separation and one unresolved nonce.
B3a seeds mid-turn anchors (bounded lifecycle lookup at the saved offset), prunes
covered settled history to nonce digests while preserving watermarks/operator
audits, and skips unchanged poll writes. Current/unresolved evidence stays pinned;
the 8 MiB/128-session bound still applies backpressure, never fact eviction.
B4 qualifies the real 0.166.0 text-only `event_msg/user_message` shape using one
captain-approved loopback mock request. Its canonical UUID `client_id`, exact
message and empty `images/local_images/audio/local_audio/text_elements` arrays
are required; it has no turn ID, so only the anchored open native lifecycle with
`turn_attribution.turn_trigger:queue` can associate it. B5a requires that trigger,
including recovery from older checkpoints. Unknown/aliased/multimedia shapes pause
with a clear reason. Queue `clientUserMessageId` and promoted `client_id` are
separate canonical facts: equality has not been observed before promotion and
is not required. The exact saved body/nonce and queue-triggered lifecycle prove
the receipt. B5b also requires the attribution's turn_id to equal task_started's
turn_id, including when recovering old checkpoints. Queue item ID changes, not
only client-ID changes, pause. Native attachment remains unqualified. The dormant
Rust hosted-supervisor custody prerequisite is implemented in B5b: only the owner
of the unreaped child acquires a pidfd and records executable/start/boot identity.
A separate sticky custody poison fences pane exit, wrong-UUID callbacks, holder
changes and daemon/socket replacement; a new valid callback cannot repair it.
See the design for lock order and the B5c consumer's mandatory fresh pidfd checks
for both pane and supervisor. This does not make Codex hosted eligible or prove
current UI subscription. No runner, enable path or possibly-unattended K=3/budget
accounting is implemented yet. Activation remains blocked.

The fixed non-deleting admission-nonce filter has separate possible-reuse and
saturation errors with fill-ratio diagnostics (25% fill limit, about 4,700 unique
retirements per session). Saturation blocks inserts only into that session;
other sessions still check its filter for possible reuse. No rollover or
automatic UUID switching is provided. These are not exact-reuse claims. Future runners must
pause/surface the error, never resample or retry on a timer; operator capacity
review is required. False-positive rates accumulate across session filters
(roughly 39% across 128 near-capacity independent filters); no resampling means
these refusals also require operator recovery. Replay-preserving rollover or
capacity migration must address the project-wide rate before activation.
Positively never-spawned rejected nonces use a separate
counter and 256-entry exact window, not that admission filter. See the design
for capacity/migration details and the safety boundary of this exception.
It is not called by a mail runner. Lost CLI responses reconcile queue presence,
anchored native promotion or a covering runtime ack; **absence alone never proves
rejection or permits replay**. Evidence-free uncertainty pauses immediately and
may require operator/native recovery; there is no guaranteed negative-admission
lookup in this CLI. See the design's B2 recovery rule and activation limitations.
UUID reservation, delivery and live mail tests require later review; no real
Codex login or model use is authorized by this code.

## Tests

Hermetic Linux unit/socket/subprocess coverage (also in `tests/python_suites.rs`):

```sh
python3 -B tests/codex_attended_test.py
```

Opt-in native subprocess fixture, after an admitted build:

```sh
python3 -B scripts/codex-binding-spike.py \
  --binary target/debug/privateer --codex "$(command -v codex)"
```

The fixture creates a private `/var/tmp` root and isolates HOME, CODEX_HOME,
Privateer fleet/resource state, TMPDIR and XDG homes, with `PRIVATEER_SERVICE=none`
so ship creation cannot leave dispatchers behind. It never copies auth or uses
the machine daemon. Its only outside-root artifact is its own initially absent,
home-derived physical socket/lock; every connection verifies the owned daemon,
and teardown verifies payload exit before removing only those exact artifacts.
The fixture operator first starts/stops an ordinary TUI with only the trusted
fixture stopper to obtain a durable known UUID (zero-turn paginated threads
cannot be resumed); the attended pane then explicitly resumes that UUID.
Production never selects a UUID from listings. A separately trusted fixture-only
`continue:false` hook stops both seed and initial resume turns before a provider request;
production never installs that blocking handler. A local mock provider catches
unexpected requests, which fail the fixture. Distinct project callbacks,
untrusted/unbound behavior, exact-UUID binding, actual `delivery_bind` and
daemon-death refusal are tested; this is not a live model/mail pilot.

B2's separate queue-promoter fixture (after an admitted build):

```sh
python3 -B scripts/codex-queue-spike.py \
  --binary target/debug/privateer --codex "$(command -v codex)"
```

It inherits the same isolation/owned-daemon cleanup, creates only fixture legacy
threads, and compares an exact-resume TUI with a loaded thread that has never had
a TUI, plus that owned TUI's thread after the TUI exits (still loaded and idle).
Fixture-only SessionStart **and UserPromptSubmit** stoppers protect all but one
nonce-selected queued input: B4's captain-approved scope allows exactly **one
request to the fixture's own loopback mock**, not a real provider or login.
Retries are disabled, the one-use hook allowance rejects repeats, and the
fixture asserts exactly one POST to its own `127.0.0.1` port. It captures real
accepted native input, verifies its observed text-only client-ID shape and
lifecycle ordering, and recovers a durable anchored ledger receipt using a new
reducer instance (accepted range, no mailbox ack). Stopped turns are still not
presentation evidence. Delivery remains disabled. On Codex CLI/fixture daemon
**0.166.0**, the fixture proved **the daemon promotes a loaded thread even with no
TUI ever attached**. Public loaded/idle fields do not distinguish attachment,
including after TUI exit. Positive ATTACHED proof remains required, not a
loaded-only check (or separately qualified unattended-turn budget/reporting).
B5b implements only the dormant Privateer-owned hosted child/pidfd custody
prerequisite, with explicit exit/navigation races and an upstream attachment-identity
ask; it is not an attachment detector or permission to run mail. Its authenticated operator-only resolve control and surfacing of
the immediate bounded uncertainty diagnostic also remain B3; the dormant reducer
is not an exposed command. No user history setting is changed.
