# Codex attended binding (experimental, exact-UUID resume only)

This is the binding boundary of the [attended-mail design](../../docs/design/codex-attended-mail.md).
It does **not** deliver mail, enable hosted eligibility, implement scheduled work,
or qualify native autonomy. The main attended-support table is unchanged.

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
**exact known B0, B1 and B2 dispatcher bytes** (SHA-256 pinned in `codex_binding.py`,
fixtures in `tests/fixtures/codex-dispatcher-b{0,1,2}.txt`). Prepare upgrades that body atomically
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
state and effort/web-search `-c` overrides that exclude the daemon) **refuse the
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
are required; it has no turn ID, so only the anchored open native lifecycle can
associate it. Unknown/aliased/multimedia shapes pause with a clear reason.
Positive ATTACHED proof is still unqualified and activation remains blocked.

The fixed non-deleting admission-nonce filter has separate possible-reuse and
saturation errors with fill-ratio diagnostics (25% fill limit, about 4,700 unique
retirements per session). These are not exact-reuse claims. Future runners must
pause/surface the error, never resample or retry on a timer; operator capacity
review is required. Positively never-spawned rejected nonces use a separate
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
The proposed Privateer-owned hosted child/pidfd proof is **design only**, with
explicit exit/navigation races and an upstream attachment-identity ask; it is
not an implemented attachment detector or permission to run mail. Its authenticated operator-only resolve control and surfacing of
the immediate bounded uncertainty diagnostic also remain B3; the dormant reducer
is not an exposed command. No user history setting is changed.
