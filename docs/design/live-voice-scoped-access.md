# Per-device scoped live voice: approval contract

**Status: proposed; no implementation or deployment authorized by this document.**
This is the contract-first part of fleet visibility PR5. Approval must name the
numbered decisions and the implementation slice below. Recommendations are not
policy already in force. Runtime spoken rendering remains PR6; aggregate mail
exposure remains PR7 and depends on the desk subscription line.

## Existing behavior and constraints

Read against the complete `plugins/live-voice/` tree (including the transport,
pacing, request ledger, page and spike), `tests/live_voice_door_test.py`, its fake
app-server, [scoped v2](../door-scoped.md), [selected mail](../door-reader.md),
`src/door/{scoped,clients,delegated,idempotency}.rs`, client edits, registration,
and [fleet visibility](mcp-fleet-visibility.md).

Today `web.Pairing` has one persistent page token; pairing replaces it. `Door`
has one session, conversation, transcript, client-call map and notice queue.
Runtime calls have no device/auth context. Trusted v1 pushes captain mail and
accepts `delivered`; its hello supplies the default recipient and coverage.
Those are not scoped-v2 facilities. Replacing just the token check would leave
cross-client state and captain-authority paths intact.

Core `door client issue` creates the relevant grant (not `grant.put`, which is
ship-management authority). One UUID currently identifies both client and grant.
`capabilities {}` returns effective `scopes`, `ships`, and core provenance;
it does not expose expiry, pause state or the internal visibility revision.
Local bearer auth is exactly `{"token":"..."}`: adding even the internal
`resource` field refuses a local bearer. The core binds it to the registered
door and `door:NAME`. OAuth audiences are a different credential contract.

Core `ships {schema:"runtime.v1"}` rechecks the actual credential, grant identity
and visibility revision before releasing its potentially linked read. Legacy
ships/status, waiting, mail reads and ordinary send do not all have that same
release fence. An adapter must not confuse admission with continuing authority.
Core keyed send has 24-hour, bounded result protection, including uncertain
reservations; it is not an exactly-once distributed outbox. The scoped send
schema currently rejects nonempty payloads, including trusted voice's
`payload.answer.text` for a typed decision.

## Numbered decisions requiring approval

### 1. Compatibility and registration

**Recommend:** leave the existing `manifest.json`, trusted-v1 registration,
client protocol 1 and state files working without migration. Add a separate
`manifest.scoped.json` declaring `kind:"front-door-scoped", door_protocol:2`,
plus explicit `access_mode = "scoped"` configuration. Scoped registration uses
a new door name/state directory/port; the core writes its existing rollback-safe
guard manifest. Require agreement between config mode and hello version, and
verify pre-auth `protocol {}` returns version 2 before serving scoped clients.
Any mismatch, missing probe or malformed hello exits/refuses; never retry v1.
No auto-registration, implicit conversion, token import or manifest replacement.

The scoped browser API is protocol **2**, independently of core protocol v2.
`/api/version` identifies protocol 2 and `access_mode:"scoped"`; trusted v1 keeps
its current exact reply. A scoped client never falls back to protocol 1. Document
operator-run registration examples using placeholders, not real installations.

### 2. Enrollment and distinct core-issued grants (captain policy)

**Recommend:** captain-operated local enrollment, not OAuth/device-code issuance
in this PR. The captain first issues one local core grant per device using the
existing controlling-terminal `door client issue NAME --name alpha ...` flow.
The adapter cannot issue, edit, resume, extend or rotate a grant. Pairing alone
only proves possession of an enrollment code, never fleet authority.

A private config entry per device contains a stable opaque device UUID, a local
label such as `alpha`, the expected core grant UUID and a credential-file path.
Do not accept a grant UUID, device UUID, scopes, ships, credential-file path or
resource from browser/model fields as authority. At startup and pairing the
adapter calls `capabilities` using that entry's bearer and verifies the returned
provenance grant/client UUID and exact `door:NAME`. Reject duplicate device IDs,
labels, grant UUIDs or credential hashes; two devices cannot share one grant.
Missing/unreadable credentials disable only that enrollment; malformed or
ambiguous configuration fails startup closed. Enrollment capacity is 16 devices.

Write a separate one-use random pairing code in each device's private state
subdirectory, never a global code that can select arbitrary configured grants.
The pair body contains only `code`; the adapter resolves it to an enrollment,
not a caller-chosen device/grant. Successful pairing requires a fresh core check,
consumes only that code and returns a random local page token. Five wrong
attempts per 60 seconds globally retain the existing brute-force defense; at
the threshold replace outstanding unused codes, not existing device tokens.
A code expires after ten minutes and is regenerated on adapter restart; no
public endpoint creates codes. Pairing another device does not replace this
one's token; re-pairing the same device replaces only its previous page token.

### 3. Credential custody, pairing lifetime and redaction (captain policy)

**Recommend:** the browser holds only its own local pairing token. The adapter
reads the core bearer from a captain-created private regular file; core keeps
only its existing token digest. Config contains references, never bearer values.
Require owner-only directories/files (0700/0600), reject symlinks, multiple hard
links, wrong ownership and nonregular/oversized credential files using opened
descriptors, and atomically persist adapter metadata. Maximum bearer file: 512
bytes, one local `pv_local_` credential; no environment/argv credential transport.
Do not add OAuth refresh, OS keyring dependencies, remote HTTP or TLS here.

Persist only a digest of the page token plus device/grant/binding identity,
issued/expiry timestamps and a hash of the configured credential. Return the raw
page token only once in `/api/pair`; use `X-Privateer-Token`, never URLs/cookies.
The page uses a distinct protocol-2 storage key, clears the code input and clears
credentials/UI/audio on authorization failure. No v1 localStorage-token import.
A copied token is the same logical device, not hardware attestation; theft needs
re-pairing to replace that page token, or core rotation/revocation. Existing loopback, exact-Origin/CORS, Host, JSON-only,
no-store, frame and CSP defenses remain mandatory.

Pair tokens expire in 24 hours, or sooner when the core credential stops working.
This is not an extension of grant expiry: every protected release reauthenticates.
Persisted time moving backwards invalidates pairing/recovery eligibility rather
than extending it; no minimum boot uptime is assumed. Re-pair after local expiry.
Known credentials must never enter prompts, notes, transcript/state data, logs,
errors, client results or runtime result bodies. Redact exact known secrets and
local/core token-shaped strings at external text boundaries, before truncation;
never print raw request/response/auth objects or exception repr containing them.
Only pairing's intentional one-time page-token return is an exception. This is
not a guarantee against a person voluntarily speaking an unrelated secret to
the external audio provider or against arbitrary unlabelled secrets in mail.

### 4. Expiry, pause, edit, rotate, revoke and restart

**Recommend:** use only the existing core lifecycle commands. The operator may
set/shorten grant expiry, pause, edit, rotate or revoke each independently. Pilot
documentation recommends an explicitly chosen finite expiry, not an invented
adapter-issued grant lifetime. Permanent grants require deliberate captain choice.

Invalid core auth fails closed immediately at the next request/release check;
while audio is active, a core capabilities check at most once per second also
stops idle revoked/expired/paused sessions. No successful-check TTL authorizes
later fleet output. Core outage/timeout is unavailable (503), not a reason to
use stored permissions or the trusted route. Revocation is not cancellation of
an admitted send, and stopping an app-server cannot retract already released
audio or model context.

Observed scope/ship changes terminate that device's session and purge its
volatile transcript/draft/calls/notices before any more release. Apply widening
only to a newly started session; no automatic scope acquisition. A restrictive
change during a runtime.v1 read is fenced by the core even if restored before
the adapter observes it. The capabilities API has no revision: **do not claim
that the adapter detects every restrict/restore or pause/resume between checks**.
Current admission and release checks, not a promised edit notification, are the
boundary. If policy requires invalidating every such historical session, approve
a separate core revision-token design before implementation; it is not in slice A.

Core rotation invalidates the old bearer. The captain replaces the credential
file and restarts the adapter; the changed credential hash invalidates only that
device's pairing binding. Re-pair; no live session/draft survives rotation.
Confirmed send recovery records remain bound to the same device and grant, not
to the obsolete secret, and still require current authority. Changing the grant
UUID is new enrollment and cannot adopt old send/draft/history records. A
restart restores local pairing bindings and confirmed send records, never a
live app-server session, an unconfirmed draft or queued spoken notice. Trusted
v1 state (`spoken.json`, `heard.json`, `requests.json`, session token) is untouched.

### 5. Session concurrency and cross-device isolation

**Recommend:** one active audio session per adapter initially (current resource
behavior), but a separately authenticated logical state for every device.
Another device's start gets a generic busy refusal without owner/name/draft
metadata. It cannot stop, connect, confirm, cancel, poll or keep that session
alive. No implicit takeover; the owning device stops or loses its heartbeat.
Multiple concurrent app-servers are separate future resource/admission work.

All session-mutating requests include an unpredictable `session_id` returned
on start. A delayed stop/connected/click/tool result from an old session cannot
act on a replacement, even from the same paired device. The server derives the
device from the pairing token and checks `(device, binding generation, session
nonce)`; body identifiers only narrow that identity. A non-owner state poll
returns only its own idle state, never a global phase/count/error/transcript.
On ownership transfer erase the previous volatile state, not copy it.

Protocol-2 API shapes (the existing v1 shapes stay unchanged):

- Public `GET /api/version`: `{protocol:2, access_mode:"scoped", door, features}`;
  static files are public but contain no device/fleet state.
- `POST /api/pair {code}`: `{token, device_id, expires_ms}`. This is the sole
  credential-bearing response, available only to that code holder.
- Token-authenticated `GET /api/state`: `{device_id, session_id:null|ID, phase,
  live, error, draft, transcript, requests, tool_calls, heartbeat_secs,
  mail_mode:"disabled", queued:0, warning}`; arrays/draft/errors are the caller's
  only. An idle device has no other device's session information.
- `POST /api/session/start {offer,client?,tools?,context?}`:
  `{device_id, session_id, answer}`. Connected/stop requests carry `session_id`;
  successful replies carry the same `device_id, session_id` and `ok:true`.
- Draft confirm/cancel, tool result and say keep their documented fields, add
  mandatory `session_id`, and return own device/session identifiers with their
  documented outcome. No protected request body carries a token or core auth.
- `POST /api/send/recover {id}`: own durable operation only; returns
  `{device_id, id, message_id, delivery_state, outcome}` when known, never a
  foreign operation or a generic arbitrary-send facility.
- Protected errors keep `{code,error}` with fixed safe messages. Authentication
  failure is 401 `not_paired`; valid pairing but denied current authority is
  403 `access_denied`; unavailable core is 503 `unavailable`; foreign/stale
  session/draft/call/operation identifiers share 409 `stale_state`. Busy is
  409 `session_running`. Timeout/uncertain send is 502 `outcome_unknown`;
  retry capacity is 429 `retry_capacity` without losing the original intent.
  Existing shape/media/origin/rate errors remain documented. An authority error
  carries no device/grant/ship/draft details, and must clear the page's cached
  protected display and close its microphone/peer connection.

### 6. Request/result custody and release checks

**Recommend:** an explicit immutable context passed through API handlers and
runtime calls, not a thread-local or mutable `current_device`. Each waiter
contains device/grant/binding/session, operation, required scopes, authorized
ship dependencies and correlation ID. Runtime IDs are globally unique for the
process. A late response completes only its original waiter; no device can
retrieve another's result by guessing an ID. Timeout removes the waiter and
late data is discarded, never posted into the next session.

Sessions, utterances, drafts, send claims, outcomes, confirmation waiters,
transcript lines, turn handoffs, request records, client-tool calls, queued
screen notices and errors carry that same ownership. A screen click must match
device + session + draft ID + exact recipient. A client-tool result additionally
matches the pending call in that session; unknown, stale and foreign IDs have
the same generic refusal, without quoting another device's values.

For protected data check at admission and again immediately before HTTP
serialization, model/tool response, voice append/handoff, or retained-state
release. Resolve fresh `capabilities` with the same bearer; validate expected
provenance, required scopes, all ship dependencies, and consistency with the
session/admission authority snapshot. Recheck session/binding ownership under
the state lock after blocking I/O. On failure discard data and stop/purge the
session, not return a successful empty fleet answer. Do not refresh old data's
observation time. Shared/global client notes and transcript files are refused
in scoped config: they are not device-scoped protected storage.

Opaque model/voice text can blend multiple authorized inputs. Its dependency is
the whole session authority snapshot, not a guessed ship extracted from prose.
A detected authority change discards the whole session rather than attempting
to sanitize its history. A fresh final check is an ordering point, not an atomic
transaction with browser receipt or external provider audio; revocation after
release cannot recall bytes. No distributed atomic-revocation claim is made.

Bound request waiters to 32 and queued adapter events to 256; overflow refuses
new work or ends the affected session, never drops a durable send fact. Retain
existing transcript (60), utterance (20), client-call (8), notice (10), text and
body limits. Keep credential/auth failures value-free. A local state-store
failure prevents send admission; do not proceed without a durable intent.

### 7. Scoped commands and minimal first-slice surface

**Recommend the following complete allowlist; tool discovery is not authority.**
Even an overpowered grant cannot reach a command outside this list. No scope is
automatically added to an existing grant. Raw v2 dispatch and guessed tools
still run through the fixed map and current core authority.

| Runtime command / client action | Scope / exact use | Slice |
| --- | --- | --- |
| `protocol {}` | Unauthenticated version check only; no auth or fleet data | A |
| `capabilities {}` | Valid local bearer, no extra scope; expected grant/resource and current scope/ship check, never return another grant | A |
| `ships {schema:"runtime.v1"}` | `fleet:read`; use core release-fenced output, expose only authorized directory names and existing coverage qualification; no legacy scope prose/liaison/default/topology dump | A |
| `send {id,to,kind:"request",body,idempotency_key}` | `ship:message`, explicit allowed recipient, only a confirmed exact utterance, no payload/urgency/deadline overrides | A |
| Session start/connected/stop/state; screen notice/tools/context | Valid binding, own session; no additional fleet entitlement; all derived fleet text additionally needs its original scopes/dependencies | A |
| Draft create/confirm/cancel and send recovery | Own session/draft or own durable confirmed intent; `ship:message` and same allowed target; no ability to send caller-supplied body | A |
| `inbox {ship,limit,continuation?}`, `show {ship,id}`, `thread {ship,id,limit,continuation?}` | `mail:read`, explicit selected mailbox; exact IDs, core refs/coverage retained, fresh release checks | B, separately approved |
| `send {id,to,kind:"reply",correlation,body,idempotency_key}` | `ship:message` plus adapter-required `mail:read` to revalidate exact stored ordinary ask; never approval-bearing mail or typed answer payload | B, separately approved |

In A, `privateer_ships` reports only allowed names and a partial/unknown coverage
qualification; it does not interpret worker/capacity counts. `privateer_send`
and `privateer_confirm` retain their current meaning for new requests only.
Explicit target names work with a message-only grant without a directory read:
check against capabilities and let the core validate/routinely refuse the target.
Omitted recipient is refused in scoped mode; never pick the first allowed ship
or use a v1 hello/default. Names outside current authority are nonrevealing.

Do **not** offer/run `privateer_status`, `privateer_waiting` or
`privateer_answer` in A; attempted calls get a fixed unsupported refusal. Legacy
`waiting` exposes mail bodies under `fleet:read`; do not adopt that entitlement
for new scoped voice. Runtime status/job/work/needs/capacity rendering and its
coverage vocabulary belong to PR6, not a boolean-status migration hidden here.
An empty-scopes device may own its voice session but has no fleet tools/actions.

Never `delivered`, legacy pushed mail, `held`, `digest`, `ack`, away/back,
report/file transfer, approval/permissions, stow/upgrade, work/resource actions,
session screens/typing, grant administration, generic RPC, provider changes,
internal desk/subscription commands or operator fallbacks in either slice.
There are no new MCP tools, scopes, presets or MCP permission changes.

### 8. Scoped mail behavior (captain policy)

**Recommend: disable all unsolicited mail notices in A and B pending PR7.**
No polling every ship to emulate a subscription, no push/`delivered` fallback,
no dashboard/v1 cursor import, and no direct internal subscription/database
access. Ignore/refuse an unexpected pushed mail event without retaining,
speaking or acknowledging its body. `/api/state` explicitly says
`mail_mode:"disabled"` in A, with queued mail zero and a fixed warning that
mail was not checked; zero is not an empty-inbox/all-clear assertion.
New requests can be recorded as sent/queued/unknown, never marked answered from
unobserved mail. Do not load trusted-v1 request/result correlation history.

B may add **user-requested selected-mailbox** reads only, with
`mail_mode:"selected-pull"`, `coverage.scope:"selected-mailbox"`, original
continuation and other-machines qualification. Results/refs/continuations remain
owned by the device's grant, selected mailbox and operation; neither another
device nor another session can consume its local pending read. Do not ack merely
because speech append succeeded; initially expose no ack action at all. Core
served-window/continuation errors remain explicit, not empty inboxes. A
correlated result is visible only after an authorized selected read, exact link
matching and device-owned request matching; same ship/body never guesses a link.
No aggregate waiting, fleet mail completeness, remote-only unsolicited result
notification or mail lifecycle feed is promised by PR5.

### 9. Exact speech, confirmations and replies

**Recommend:** keep `rules.Conversation` as the hard confirmation gate, with
per-session custody: full last transcribed utterance, filler included, exact
read-back; a later whole confirmation naming that exact ship or a screen click
naming that exact owned draft. Bare yes/send, wrong ship, expired/stale draft,
model-authored body, screen/mail/tool output or echoed read-back never confirms.
Replacement utterance gets a fresh draft/key and read-back; an unconfirmed draft
is never persisted or replayed. A click/spoken race has one claimant.

Preserve approval-shape/option refusal even if the grant has `approval:answer`,
backend truth vs model claims, marker neutralization, untrusted-content frames,
screen/mail fragment echo windows in both text/speech modes, bounded handoffs
and refusal of app-server shell/file/permission/token-refresh requests. Preserve
Codex version checks, managed ChatGPT-only login and config lockdown, with no
provider/account fallback or credential files exposed to the model/browser.

For B, re-read the exact ask in the selected mailbox immediately before sending;
require still open, expected sender/recipient/ID and same nonapproval kind/content.
Core again classifies its stored ask in the write transaction. Refuse all typed
decision answers for now: omitting their required payload or using an option ID
is not equivalent to trusted voice's text answer. Supporting that payload needs
a separately approved core argument/security change, not silent degradation.
A carried ask cannot change mailbox/route automatically to work around a refusal.

### 10. Keyed-send durability, uncertainty and recovery

**Recommend:** mandatory stable mail ID **and** idempotency key for every scoped
send. Before the first runtime write, transactionally persist the confirmed
intent in a private adapter SQLite store: device/grant/resource, original
scopes/ships snapshot, target, exact body (max 4000 characters), key, mail ID,
confirmation/session identity, first-attempt time and outcome category. Never
persist the bearer. Persisting full confirmed text is a new privacy cost, stated
in configuration/documentation; without this record do not send.

The model/browser never chooses keys/bodies or modifies an existing intent.
Timeout, pipe loss, core `outcome_unknown`, uncertain execution/audit/save or
malformed success says **outcome unknown**, not "not sent". A durable known
result records the original core provenance/result/message IDs and
`delivery_state`. Queued/held/unknown is not delivered or completed work; do not
reuse v1's unconditional `Sent` wording for these states. Core denial/capacity
errors remain typed, value-free, and never trigger an unkeyed/new-key retry.

A retry/recovery uses exactly the original envelope/key/mail ID after fresh
current credential/authority checks. No retry loop, no automatic send at restart.
Add `POST /api/send/recover {id:OPERATION_ID}` for that device's explicit screen
recovery, including after re-pair/restart. It may finish a previously confirmed
intent that never reached the core; it cannot confirm an unconfirmed draft.
A pending core reservation may continue returning unknown: recovery is not a
promise to reconstruct a lost outcome. UI/model cannot convert uncertainty into
a fresh draft of the same confirmed operation; tell the person to inspect the
conversation through an independently authorized surface when necessary.

Permit recovery only within the core's 24-hour protection window, conservatively
measured from the first local attempt (not last retry). Backwards/unknown clock
continuity disables recovery until inspected. After that window an unresolved
intent stays blocked/unknown and is never retried automatically, even with the
same key; do not claim exactly-once beyond retention. Keep compact unknown
records rather than evicting them to admit new sends. A device/grant change
cannot adopt/re-key another's operation. Rotation of the same grant may recover
only with equal current scope/ship authority; restriction refuses old recovery.

Bound to 32 intents/device and 256 total, including unresolved records. Expire
terminal full-text records after 24 hours; preserve unresolved bounded records
for explicit operator disposition, or refuse new sends at capacity. A persistence
failure after runtime admission leaves the prewritten intent unknown; never
claim no mail was sent. The core registry/home/network transaction boundaries
and revoke-after-admission semantics are unchanged.

### 11. Small PR split and the first approved slice

**Recommend approve A only after reviewing decisions 1–10:**

- **A — isolated scoped caller and new requests:** opt-in scoped manifest/config,
  per-device local grant enrollment, protocol-2 page/API, strict authenticated
  runtime envelopes, device/session state/release fences, minimal authorized
  directory listing, exact new-request drafting/confirmation, durable keyed-send
  recovery and explicit disabled-mail behavior. Existing trusted-v1 behavior is
  covered by regression tests, not migrated. No core production source change.
- **B — selected-mailbox pull and ordinary replies:** separately approve the
  selected read UX, new fixed tool schemas and revalidated ordinary reply path;
  still no unsolicited notices or typed decision answer payload. Add device-owned
  result correlation only from these authorized reads. B requires its own tests
  and review, not blanket authorization from approving A.
- **PR6 / PR7:** runtime renderers and externally exposed desk-mail integration
  remain on their own dependency/review lines. Neither ships by implication here.

If A's implementation needs a new core field/command or grows beyond the above
single security outcome, stop and propose a narrower A1/A2 before writing that
change. In particular, no authority-revision API, typed answer payload or source
subscription exposure is preapproved. No production registration/deployment or
real-account acceptance is part of any slice in this contract.

### 12. Files and ownership boundaries

**Recommend:** focused helpers under `plugins/live-voice/` for enrollment/binding,
scoped transport and confirmed-send storage; targeted changes to `door.py`,
`web.py`, `rules.py`, page assets, explicit scoped manifest/config example,
README and PROTOCOL. Keep v1 code paths narrow and explicit rather than adding
conditional auth to the global state object and assuming isolation.

Extend `tests/live_voice_door_test.py`, the fake app-server as needed, and a
focused real-supervisor fixture included by `tests/door.rs`. Reuse core grant
APIs and fixture issuance helpers; tests may arrange private fixture grants but
production code never reads/writes the core registry itself. No change to
`src/crew.rs`, job resume paths, `src/workspace/process.rs`, `src/door/reader.rs`
or `src/machines/delegated.rs`. No route or resource-policy edits. Add/update an
ACCEPTED inventory if a separately approved core dispatched command ever changes.

### 13. Required acceptance evidence

**Recommend:** deterministic barriers, injectable clocks and real subprocess
boundaries, not longer sleeps/retry wrappers or exact audit-row counts.

| Behavior | A evidence required | Additional B evidence |
| --- | --- | --- |
| Distinct unequal grants | Read-only `alpha`; message-only `beta`; different ship restrictions, no shared-grant enrollment, empty-scope/overpowered-grant fixed-map refusals | Mail-only vs mail+message and hidden stored sender |
| Lifecycle | Independently revoke/pause/expire/rotate/edit; restart and re-pair; old bearer/page/session IDs fail, other device unaffected | Stale continuation/served results cannot cross grants |
| Door/resource binding | Real supervisor, another door's bearer, local bearer with forbidden audience, expected-grant mismatch; malformed/old probe and v2-to-v1 downgrade refusal | Same checks on selected linked reads |
| Isolation | Foreign state/error/heartbeat, guessed draft and send IDs, stop/connect, client-tool result, notices, delayed events/turns/handoffs and old-session actions | Foreign mail/ref/correlation/continuation |
| Release races | Barrier-held runtime.v1 read followed by grant restriction/revoke; discard before HTTP/model/voice output; blocked append plus session replacement; timeout/late response ownership | Held selected read/revalidated ask while restricted/changed |
| Send recovery | Click/spoken race; accepted send then lost response; core pending unknown; adapter crash/restart; same key/mail ID and exactly one mail, no fresh-key or unkeyed fallback; queued vs delivered truth | Same tests for ordinary reply, changed/closed/approval ask refuses |
| Safety/privacy | Exact filler-inclusive read-back, named recipient, stale/expired/cancel/replace, screen/tool/mail-echo/injection and approval refusal; secret canaries absent from prompts/logs/state/results except private credential custody and intentional pairing response | Selected mail remains framed data, never a confirmation; typed decision refused |
| Disabled mail | Unexpected pushed mail never spoken/queued/acked; no `delivered`, dashboard cursor or internal subscription access; honest disabled warning | Selected-mailbox incomplete/unavailable/unsupported output, no implicit ack/poll-all |

Run the standing **one managed serial heavy gate**, against the new binary,
with `CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1`, an honest approximately 1 GiB
estimate and `wait_for_capacity:true`; park on it and end the turn. Use a
private TMPDIR under `/var/tmp`, remove it afterwards, never lower floors.
Gate: `cargo fmt --check`; clippy locked/all targets/all features with warnings
denied; locked lib tests; affected integration binaries (at least `door` and
`python_suites` when those fixtures change); bytecode guard in `upgrade_fetch`;
locked build. Run voice door/probe suites explicitly and applicable door/MCP
suites against that built artifact (`PRIVATEER_TEST_BINARY`). If core door/MCP
paths change with separate approval, also bootstrap a task-private MCP venv and
run all four standing MCP Python suites. Report every step's actual outcome,
final local commit and limitations, including any fixture/platform not exercised.
No tests or acceptance are claimed merely by listing them here.

### 14. Security and deployment claims

**Recommend:** retain the existing trust model explicitly. This constrains
external devices through a trusted same-user adapter, not a malicious plugin,
local operator or configured linked operator. Bearer pairing is not hardware
identity or voice biometric authentication. Revocation prevents new admissions
and subsequent checked releases; it cannot unsend mail, retract model context,
recall audio or prove an external media stream stopped instantaneously. Check
owned app-server process boundaries on shutdown; wrapper exit is not evidence
that every payload/descendant stopped.

No new MCP permissions/tools, fleet authority, provider route, production access,
aggregate mail subscription, dashboard cursor migration, session screen/typing,
native autonomous-agent acceptance or real-account voice claim. Captain-run
acceptance and any deployment require separate exact-artifact/environment
approval. The first mate owns delivery/review/PR publication; crew commits locally
and reports tested evidence only.
