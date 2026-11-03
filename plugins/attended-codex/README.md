# Codex attended binding (experimental, binding only)

This is PR A of the [attended-mail design](../../docs/design/codex-attended-mail.md).
It does **not** deliver mail, enable hosted eligibility, implement scheduled work,
or qualify native autonomy. The main attended-support table is unchanged.

Explicit opt-in: set `PRIVATEER_CODEX_ATTENDED=1` for a Privateer attended Codex
launch. Headless launches and other harnesses ignore it. The launcher preserves
native exec/process identity and route argv; it installs a stable SessionStart
command in the ship folder's `.codex/hooks.json`, preserving existing handlers.
The captain must trust that declaration through native `/hooks`; no trust state
or global configuration is changed. A release that changes the callback command
path requires native trust again; a new launch generation does not.

The per-project `.codex/privateer-attended/launch.json` is private runtime state,
not source to commit. It records the exact launch, process/boot identity, current
custody and binding diagnostic. Only one live launch per project is supported;
concurrent loaded conversations in that cwd fail unbound rather than guessing.
A stale record may be replaced after its process identity is proved gone.

SessionStart is deferred until first turn entry. Hosted opt-in launches without
a supplied prompt get a positional charter/inbox bearings prompt. A manual
zero-turn pane stays UNBOUND. Hooks and argv that exclude the shared daemon
(including effort/web-search `-c` overrides) cannot silently fall back. Missing
trust, custody, socket identity or experimental API support leaves mail disabled.
This first implementation requires Linux `/proc` and Unix peer credentials.

The callback verifies its exact UUID on the same home-derived daemon, its native
cwd/transcript metadata and trusted project declaration, then emits only the
existing hosted `delivery_bind` report. The socket uses WebSocket JSON-RPC over
Unix. Upstream puts its physical endpoint in a fixed `/tmp/codex-daemon-<uid>/`
directory, independent of CODEX_HOME/TMPDIR; the home rendezvous, expected hash,
private modes, peer PID/UID and boot/start identity must all agree. A dead,
replaced, unsupported or mismatched daemon is a refusal, never an embedded server.
The adapter cannot call queue/add or turn/start.

Paginated native history may have no JSONL rollout even inside SessionStart.
The binding records `metadata_source:daemon_thread_read` and does not pretend
that receipt/byte-offset recovery is qualified. Every record explicitly has
`delivery_enabled:false`. Delivery, reconciliation, restart and live mail tests
belong to later PRs; no real Codex login or model use is authorized by this code.

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
Privateer fleet/resource state, TMPDIR and XDG homes, with
`PRIVATEER_SERVICE=none` so ship creation cannot leave dispatchers behind. It
never copies auth or uses the machine daemon. Its only outside-root artifact is its own initially absent,
home-derived physical socket/lock; every connection verifies the owned daemon,
and teardown verifies payload exit before removing only those exact artifacts.
A separately trusted fixture-only `continue:false` hook stops the initial turn
before a provider request; production never installs that blocking handler.
A local mock provider catches unexpected requests, which fail the fixture.
Distinct project callbacks, untrusted/unbound behavior, actual `delivery_bind`
and daemon-death refusal are tested; this is not a live model/mail pilot.
