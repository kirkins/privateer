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
a stable SessionStart command pointing to `.codex/privateer-attended/hook.py`,
preserving existing handlers. This version-independent project dispatcher reads
the current private launch record to select that launch's release handler. A
Privateer release change or new launch generation does not change the trusted
command. The dispatcher refuses unexpected existing contents. The captain must
trust the declaration through native `/hooks`; no trust state or global config
is changed. Existing PR A versioned hook declarations are not silently removed
or re-trusted: review and remove those old declarations through native hook
management before trusting the new stable dispatcher once.

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
callback validation or reporting, so failed callbacks cannot preserve an older
binding. This implementation requires Linux `/proc` and Unix peer credentials.

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
that receipt/byte-offset recovery is qualified. Every record explicitly has
`delivery_enabled:false`. UUID reservation, delivery, reconciliation, restart
receipt recovery and live mail tests require later review; no real Codex login
or model use is authorized by this code.

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
The fixture operator first starts/stops an ordinary zero-turn TUI to obtain a
known UUID; the attended pane then explicitly resumes that UUID. Production
never selects a UUID from listings. A separately trusted fixture-only
`continue:false` hook stops the initial resume turn before a provider request;
production never installs that blocking handler. A local mock provider catches
unexpected requests, which fail the fixture. Distinct project callbacks,
untrusted/unbound behavior, exact-UUID binding, actual `delivery_bind` and
daemon-death refusal are tested; this is not a live model/mail pilot.
