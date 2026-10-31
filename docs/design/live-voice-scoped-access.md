# Per-device scoped live voice: approval contract

**Status: A1 approved on 2026-10-31; no deployment authorized.**
This is the contract-first part of fleet visibility PR5. Captain decision
**7cc62689, option a** approves the recommended policy in decisions 2–3 and 8:
private per-device server key plus one-time pairing, daily browser re-pairing,
30-day core key expiry, send-only mail in later A2, and separately reviewed reads
of one named ship's inbox in B. Independent review changes below are accepted.
Only **A1** is authorized for implementation; A2/B need separate approval.
Runtime spoken rendering remains PR6; aggregate mail exposure remains PR7 and
depends on the desk subscription line.

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
Defence in depth: trusted-v1 mode refuses a state directory containing scoped
state/binding markers or the scoped store, even with a v1 manifest/config. Do
not erase those markers to make downgrade succeed.

The scoped browser API is protocol **2**, independently of core protocol v2.
`/api/version` identifies protocol 2 and `access_mode:"scoped"`; trusted v1 keeps
its current exact reply. A scoped client never falls back to protocol 1. Document
operator-run registration examples using placeholders, not real installations.

### 2. Enrollment and distinct core-issued grants (decided 2026-10-31, 7cc62689)

**Captain approved:** captain-operated local enrollment, not OAuth/device-code issuance
in this PR. The captain first issues one local core grant per device using the
existing controlling-terminal `door client issue NAME --name alpha ...` flow.
The adapter cannot issue, edit, resume, extend or rotate a grant. Pairing alone
only proves possession of an enrollment code, never fleet authority.

A private config entry per device contains a stable opaque device UUID, a local
label such as `alpha`, the expected core grant UUID and a credential-file path.
Do not accept a grant UUID, device UUID, scopes, ships, credential-file path or
resource from browser/model fields as authority. At startup, pairing and session
start the adapter calls `capabilities` using that entry's bearer and verifies the returned
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
A code expires after ten minutes. After consumption or expiry, regenerate the
next one-use code into that device's private file, lazily or during a local
expiry sweep (including when a pair attempt discovers expiry), without
restarting the adapter. At most one outstanding code/device; there is no public
code-mint endpoint, and an HTTP response never reveals an unused code. Pairing
another device does not replace this one's token; re-pairing the same device
replaces only its previous page token and invalidates its old session.

### 3. Credential custody, pairing lifetime and redaction (decided 2026-10-31, 7cc62689)

**Captain approved:** the browser holds only its own local pairing token. The adapter
reads the core bearer from a captain-created private regular file; core keeps
only its existing token digest. Config contains references, never bearer values.
Require owner-only directories/files (0700/0600), reject symlinks, multiple hard
links, wrong ownership and nonregular/oversized credential files using opened
descriptors, and atomically persist adapter metadata. Maximum bearer file: 512
bytes, one local `pv_local_` credential; no environment/argv credential transport.
Do not add OAuth refresh, OS keyring dependencies, remote HTTP or TLS here.

Placeholder operator recipe, on the controlling terminal (not through a model
or door RPC): issue using `door client issue NAME --name alpha` with the chosen
scopes/ships and `--expires-ms EXPIRY_UNIX_MS` set 30 days ahead, then copy its
one-time credential into a private file using the following interaction:

```sh
umask 077
set -C  # refuse an existing credential file rather than following/overwriting it
mkdir -p /PRIVATE/DEVICE-DIRECTORY
chmod 700 /PRIVATE/DEVICE-DIRECTORY
cat > /PRIVATE/DEVICE-DIRECTORY/core-key
# Paste ONLY the issued credential as terminal input, then press Ctrl-D.
chmod 600 /PRIVATE/DEVICE-DIRECTORY/core-key
```

Replace placeholders locally. `cat` receives the secret on stdin, not as a
shell command, argument or environment value; the paste is not shell history.
Use a private non-recorded terminal, avoid scrollback/clipboard capture, and
clear the clipboard afterwards. Existing files must already satisfy the private
regular-file checks; do not follow an existing symlink. The adapter never runs
this recipe or issues a key. Configure only the file path and expected grant ID.

Persist only a digest of the page token plus device/grant/binding identity,
issued/expiry timestamps and a hash of the configured credential. Return the raw
page token only once in `/api/pair`; use `X-Privateer-Token`, never URLs/cookies.
The page uses a distinct protocol-2 storage key, clears the code input and clears
credentials/UI/audio on authorization failure. No v1 localStorage-token import.
A copied token is the same logical device, not hardware attestation; theft needs
re-pairing to replace that page token, or core rotation/revocation. Existing
loopback, exact-Origin/CORS, Host, JSON-only, no-store, frame and CSP defenses
remain mandatory.

Captain decision 7cc62689: browser tokens expire in 24 hours (daily re-pairing),
and core keys are issued with 30-day expiry and rotated locally as needed.
Local pairing does not extend that expiry: every new fleet-data release
reauthenticates, with the already-released state exception in decision 4.
Persisted time moving backwards invalidates pairing/retry eligibility rather
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
set/shorten grant expiry, pause, edit, rotate or revoke each independently.
The approved captain policy is a 30-day core expiry, not an invented
adapter-issued lifetime; only captain issuance/edit changes that expiry.

Invalid core auth fails closed at the next admission/new-release check. Recheck
before each **new** release of fleet data, not on each browser heartbeat/state
poll. The same device's already-released session state may be re-served on valid
local pairing plus a live session, without a new core admission. Keep an explicit
released snapshot owned by device/binding/session; cached state is not permission
to append newly received data, re-use fleet text in a new model/voice output, or
revive a stopped session. Preserve original observation timestamps.

While a session is idle, check current capabilities about once per 30 seconds;
coalesce the idle check with recent real admission/release checks. There is no
1 Hz capability poll. Every core admission writes an audit row and the per-door
history is capped at 1,024 rows: one 1 Hz poll would displace roughly 17 minutes
of send/denial history. Browser polling must add no audit rows; idle checks have
a bounded 30-second cadence, not one check per device or frontend poll. Test
this audit-volume bound with an injectable clock and varied poll counts.

A failed current-authority check stops/purges that session. Core outage/timeout
is unavailable (503), not permission to release new fleet data using stored
permissions or the trusted route. No cached successful check authorizes a new
release. Revocation is not cancellation of an admitted send; stopping an
app-server cannot retract already released audio/model context. A device might
retain already-released state until the next check detects revocation (about
30 seconds when idle); the exception does not promise instant cached-data recall.

Observed scope/ship changes terminate that device's session and purge its
volatile transcript/draft/calls/notices before any more release. Apply widening
only to a newly started session; no automatic scope acquisition. A restrictive
change during a runtime.v1 read is fenced by the core even if restored before
the adapter observes it. The capabilities API has no revision: **do not claim
that the adapter detects every restrict/restore or pause/resume between checks**.
Current admission and release checks, not a promised edit notification, are the
boundary. If policy requires invalidating every such historical session, approve
a separate core revision-token design before implementation; neither A1 nor A2
includes one. Review accepts this limitation without a new core revision API.

Core rotation invalidates the old bearer. The captain replaces that device's
credential file; **no adapter restart is required**. At pairing and session
start, reopen and re-read the file, validate the opened descriptor's ownership,
permissions/type/link count/size and revalidate the new credential with the core.
A changed hash invalidates only that device's pairing/session binding. Re-pair;
no live session/draft or in-memory send retry survives rotation. Other devices
and their tokens are unaffected; no auth failure retries with a different grant.
Changing the grant UUID is new enrollment and cannot adopt another enrollment's
records. Restart restores local pairing bindings and value-free send records,
never a live app-server, unconfirmed draft, retry envelope or queued notice.
A2 restored send records report unknown outcome/check before resending, not
replay authorization. Trusted-v1 state (`spoken.json`, `heard.json`,
`requests.json`, session token) is untouched.

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
- A1 refuses send/confirm actions with a fixed unsupported response, even for
  an overpowered grant. A2 adds own-session draft/confirmation actions only;
  `/api/send/recover` does not exist in these slices. Restored own send records
  contain IDs/status and the warning to check before resending, never mail text.
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

For every **new** protected fleet-data release check at admission and again
immediately before HTTP serialization, model/tool response or voice append/
handoff. Re-serving that device's already-released state uses decision 4's live
session/local-pairing exception, with no polling-driven core check. Resolve fresh `capabilities` with the same bearer; validate expected
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
failure prevents A2 send admission; do not proceed without the value-free
pre-admission record described in decision 10. A1 performs no sends.

### 7. Scoped commands and minimal first-slice surface

**Recommend the following complete allowlist; tool discovery is not authority.**
Even an overpowered grant cannot reach a command outside this list. No scope is
automatically added to an existing grant. Raw v2 dispatch and guessed tools
still run through the fixed map and current core authority.

| Runtime command / client action | Scope / exact use | Slice |
| --- | --- | --- |
| `protocol {}` | Unauthenticated version check only; no auth or fleet data | A1 |
| `capabilities {}` | Valid local bearer, no extra scope; expected grant/resource and current scope/ship check, never return another grant | A1 |
| `ships {schema:"runtime.v1"}` | `fleet:read`; release-fenced output reduced to authorized names and directory coverage only | A1 |
| Session start/connected/stop/state; screen notice/tools/context | Valid binding, own session; derived new fleet text additionally needs its original scopes/dependencies | A1 |
| `send {id,to,kind:"request",body,idempotency_key}` | `ship:message`, explicit allowed recipient, only a confirmed exact utterance, no payload/urgency/deadline overrides | A2 only |
| Draft create/confirm/cancel and in-session retry | Own session/draft, `ship:message`, same target; no caller-chosen body/key or restart recovery | A2 only |
| `inbox {ship,limit,continuation?}`, `show {ship,id}`, `thread {ship,id,limit,continuation?}` | `mail:read`, explicit selected mailbox; exact IDs, core refs/coverage retained, fresh release checks | B, separately approved |
| `send {id,to,kind:"reply",correlation,body,idempotency_key}` | `ship:message` plus adapter-required `mail:read` to revalidate exact stored ordinary ask; never approval-bearing mail or typed answer payload | B, separately approved |

**Confirmed by source inspection:** `src/door/runtime.rs::projection` builds
separate top-level `ships`, `coverage`, `machines`, `selection` and
`observed_totals`; each ship row has a separate `name` and optional runtime facts.
`src/door/coverage.rs::runtime_domain` emits facet-tagged sources. The adapter can
therefore construct a fresh DTO containing `names = ships[*].name` and only
`coverage.{schema,scope,complete,domain,state,inventory_complete,other_machines}`
plus sources whose `facet == "inventory"`. Retain each inventory source's original
state/freshness/provenance, never derive freshness from receipt time. Drop all
non-inventory sources and all other ship fields, runtime facts, totals, machines
and selection from the external/model response. No worker, job, needs or capacity
detail is required or released. Fail malformed projections closed; never pass
through the raw core result. Test with canary worker/capacity/detail fields.
This is an intentionally reduced voice-directory DTO, not an unmodified complete
runtime.v1 result; its coverage remains incomplete/unknown-domain, not all-clear.

In A1, `privateer_send`, `privateer_confirm` and drafting are refused and are not
offered as model tools. In A2 they retain their exact-confirmed-new-request
meaning. Explicit target names work with a message-only grant without a directory read:
check against capabilities and let the core validate/routinely refuse the target.
Omitted recipient is refused in scoped mode; never pick the first allowed ship
or use a v1 hello/default. Names outside current authority are nonrevealing.

Do **not** offer/run `privateer_status`, `privateer_waiting` or
`privateer_answer` in A1/A2; attempted calls get a fixed unsupported refusal. Legacy
`waiting` exposes mail bodies under `fleet:read`; do not adopt that entitlement
for new scoped voice. Runtime status/job/work/needs/capacity rendering and its
coverage vocabulary belong to PR6, not a boolean-status migration hidden here.
An empty-scopes device may own its voice session but has no fleet tools/actions.

Never `delivered`, legacy pushed mail, `held`, `digest`, `ack`, away/back,
report/file transfer, approval/permissions, stow/upgrade, work/resource actions,
session screens/typing, grant administration, generic RPC, provider changes,
internal desk/subscription commands or operator fallbacks in any slice.
There are no new MCP tools, scopes, presets or MCP permission changes.

### 8. Scoped mail behavior (decided 2026-10-31, 7cc62689)

**Captain approved: send-only mail in A2; no inbox reads in A1/A2.**
A1 has no send capability either. Later B needs separate review for one named
ship's selected inbox, not aggregate visibility. **Disable all unsolicited mail
notices in A1, A2 and B pending PR7.**
No polling every ship to emulate a subscription, no push/`delivered` fallback,
no dashboard/v1 cursor import, and no direct internal subscription/database
access. Ignore/refuse an unexpected pushed mail event without retaining,
speaking or acknowledging its body. `/api/state` explicitly says
`mail_mode:"disabled"` in A1/A2, with queued mail zero and a fixed warning that
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

**Recommend for A2 (no drafting/sending in A1):** keep `rules.Conversation` as the
hard confirmation gate, with
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

### 10. A2 keyed sends, value-free records and unknown outcomes

**Engineering review choice:** A1 has no send path. In A2 give every confirmed
draft one stable mail ID and idempotency key; retain its exact envelope, body
(max 4000 characters), key, first-attempt time and confirmation identity **only
in memory for the owning live session**. Retries of that draft reuse those IDs,
key and exact arguments. The model/browser never chooses a key, body or envelope.
A click/spoken race has one claimant. No unkeyed or fresh-key fallback.

Before runtime admission, transactionally write only a value-free private
metadata record: device UUID, grant UUID, target, mail ID, **key hash**, state
and first-attempt timestamp. No body, transcript, raw key, bearer, raw core
result, arbitrary error or replayable envelope on disk. "Value-free" here means
no message content or credentials; target/IDs remain private authorized metadata.
If this write fails, refuse to send. Persist state transitions atomically; a
failure after runtime admission leaves the prewritten record unknown, not
"not sent". Bound to 32 records/device and 256 total. Remove terminal metadata
after 24 hours, keep unresolved records until explicit operator disposition,
and refuse new admissions at capacity rather than evict unresolved facts.

Timeout, pipe loss, `outcome_unknown`, uncertain execution/audit/save or malformed
success says **outcome unknown; check before resending**. In a still-live session,
retain known core provenance/message IDs and `delivery_state`; queued/held/unknown
is not delivered or completed work. Core denial/capacity errors remain typed and
value-free. An explicit in-session retry checks current authority and uses only
the original envelope, within 24 hours measured from its first attempt. No retry
loop; backwards/unknown clock continuity disables retry. Pending reservations
may stay unknown: keyed retry does not promise to reconstruct a lost outcome.
Do not turn an uncertain draft into a new key while that session still owns it.

After restart/session loss/re-pair/rotation, the replayable envelope is gone.
Restore only that same device/grant's metadata and display **outcome unknown;
check the conversation through an independently authorized surface before
resending**. Never replay a record or treat its key hash as a usable key.
There is no `/api/send/recover`, no persistent full-text storage and no automatic
send on startup. A separately confirmed new message after manual inspection is
not recovery or an exactly-once guarantee; without stored text the adapter cannot
recognize every paraphrase/duplicate across restarts. Do not claim that it can.
Changing a grant/device cannot adopt historical records. The core's bounded
24-hour protection, transaction boundaries and revoke-after-admission semantics
are unchanged. Persistent body/restart recovery needs its own later review.

### 11. Small PR split and the first approved slice

**A1 approved 2026-10-31 under captain decision 7cc62689 and this revised contract:**

- **A1 — isolated read-only scoped caller:** scoped manifest/config, private
  per-device enrollment/pairing and hot credential reload, protocol-2 API/page,
  device/session isolation and release fences, capabilities, directory reduced
  to names + inventory coverage, disabled mail, and `privateer_send`/confirmation
  refused. Trusted v1 regression coverage and scoped-state downgrade refusal.
  No sending, send ledger, core production change or implicit A2 authority.
- **A2 — exact confirmed keyed new requests:** separately approved after A1;
  device-owned exact drafting/confirmation, same-draft in-memory keyed retries,
  value-free persisted unknown-outcome metadata, no restart recovery/full text.
  Mail stays send-only. Requires its own tests/review and explicit go-ahead.
- **B — selected-mailbox pull and ordinary replies:** separately review one named
  ship's inbox UX, fixed tools and revalidated ordinary reply path. Still no
  unsolicited mail or typed decision payload. Device-owned result correlation
  derives only from authorized selected reads. Neither A1 nor A2 approves B.
- **PR6 / PR7:** runtime renderers and externally exposed desk-mail integration
  remain on their own dependency/review lines. Neither ships by implication here.
- **Later recovery proposal:** persisted confirmed message text and restart-send
  recovery are deferred, not an unmentioned extension of A2.

If a slice needs a new core field/command or exceeds its one security outcome,
stop and propose a narrower slice before writing that change. No authority-
revision API, typed answer payload or subscription exposure is preapproved.
No production registration/deployment or real-account acceptance is included.

### 12. Files and ownership boundaries

**Recommend:** focused helpers under `plugins/live-voice/` for enrollment/binding,
scoped transport and (A2 only) value-free send metadata; targeted changes to `door.py`,
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

A1 evidence excludes all actual send paths; A2-specific rows below apply only
when A2 is separately approved. The A1 message-only device must demonstrate
send refusal (not a successful send), and scoped draft IDs are absent/refused. Existing v1 send safety remains regression-tested.

| Behavior | A1/A2 evidence required | Additional B evidence |
| --- | --- | --- |
| Distinct unequal grants | Read-only `alpha`; message-only `beta`; different ship restrictions, no shared-grant enrollment, empty-scope/overpowered-grant fixed-map refusals | Mail-only vs mail+message and hidden stored sender |
| Lifecycle | Independently revoke/pause/expire/rotate/edit; expired code regenerated and credential file replaced/re-paired without restart; old bearer/page/session IDs fail, other device unaffected; restart does not revive a session | Stale continuation/served results cannot cross grants |
| Door/resource binding | Real supervisor, another door's bearer, local bearer with forbidden audience, expected-grant mismatch; malformed/old probe and v2-to-v1 downgrade refusal | Same checks on selected linked reads |
| Isolation | Foreign state/error/heartbeat, guessed draft and send IDs, stop/connect, client-tool result, notices, delayed events/turns/handoffs and old-session actions | Foreign mail/ref/correlation/continuation |
| Release races | Barrier-held runtime.v1 read followed by grant restriction/revoke; discard before HTTP/model/voice output; blocked append plus session replacement; timeout/late response ownership | Held selected read/revalidated ask while restricted/changed |
| A2 send uncertainty | Click/spoken race; accepted send/lost response; pending unknown; same-draft in-session retry uses same key/mail ID and yields one mail; crash/restart/rotation never replay, only unknown/check-before-resend metadata; no body/raw key persisted; queued vs delivered truth | Same tests for ordinary reply, changed/closed/approval ask refuses |
| Poll audit volume | Injectable clock: frequent state/heartbeat polling does not add core audit admissions; idle checks bounded to about one per 30 s per active adapter session, preserving send/denial evidence within retained history | Same when re-serving already-released selected results |
| Reduced directory / no downgrade | Worker/capacity/runtime/totals canaries absent from response/model input, inventory coverage preserved; A1 refuses send despite powerful grant; v1 refuses scoped-marked state directory | Same refusal of noninventory details |
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
