# Scoped live voice client protocol 2 (A1 + A2)

Opt-in companion to [trusted protocol 1](PROTOCOL.md), not a migration or fallback.
The [approved contract](../../docs/design/live-voice-scoped-access.md) governs policy.
A2 adds exact confirmed **new requests** with `ship:message`. Mail reads,
replies, decisions, option answers, mail notices, approvals, status/activity/work/
capacity tools, generic RPC and grant administration remain disabled. MCP is
unchanged. Do not infer an empty inbox or idle fleet from disabled features.

The protocol-1 Host/Origin/JSON/CORS/no-store/CSP, body limits, WebRTC offer
shape, ChatGPT-only login, Codex version/config lockdown and process-group
shutdown rules also apply. `/api/version` returns
`{protocol:2,access_mode:"scoped",door:"0.6.0",features:[...]}`. The bundled
protocol-2 script verifies this before loading its separate storage key
`privateer-voice-scoped-token`. Never import a v1 token or fall back to v1.

## Pairing and custody

The captain issues one distinct core grant per device in the terminal (30-day
expiry), and stores its bearer in that device's private server file. Enrollment
config carries only canonical device/grant UUIDs, a unique label and absolute
credential-file path. At most 16 devices. Directory/file custody is checked on
opened descriptors: owner UID, 0700/0600, no symlink path components, no multiple
hard links, no nonregular credential file, max 512 bytes. A credential file holds
one `pv_local_...` token. Malformed/duplicate enrollments refuse startup; a
missing/unreadable credential disables only its device. Configured grants must
match core provenance and exact `door:NAME`; local bearer auth never includes
an audience/resource field. No authority is minted by pairing.

Read a device's `state/scoped-devices/DEVICE_UUID/pairing-code` locally.
`POST /api/pair {code}` accepts no other fields and returns
`{token,device_id,expires_ms}` after fresh core validation. Codes are one-use,
expire in ten minutes and regenerate in that same private file after use/expiry
without restart. Five wrong attempts in 60 seconds globally replace unused
codes and rate-limit further pairing; they do not replace existing tokens.
Re-pairing replaces only that device's token/session. A browser token lasts 24
hours, travels only in `X-Privateer-Token`, never URLs/cookies. The persisted
binding contains only its digest, identities, credential hash and timestamps.
Re-pair daily. A copied token is the same logical device, not hardware identity.

Use existing core pause/resume/edit/expiry/rotate/revoke commands. Replace the
private credential file after core rotation and re-pair; no adapter restart is
needed. A changed credential invalidates only its own binding/session. Restart
may restore unexpired pairing bindings, never live sessions, text, drafts or
notices. Backwards wall time invalidates bindings rather than extending expiry.
Do not use shared `instructions_file` or `transcript_file` in scoped config.

## Session API

All protected calls authenticate the header token into an immutable server-owned
device/binding. Browser fields cannot choose a grant, bearer, resource or path.
Only one audio session is active. Another device sees its own idle state and a
generic busy response to start, never the owner/name/error/transcript.

- `GET /api/state` returns `{device_id,session_id:null|ID,phase,live,error,draft,
  send_history,outcome,transcript,requests:[],tool_calls,heartbeat_secs,
  mail_mode:"disabled",queued:0,warning,codex}`. Poll about once per second while open. This is the owner's
  heartbeat and already-released snapshot, not a new core admission. Idle state
  contains no other device's information. Stop purges volatile state.
- `POST /api/session/start {offer,client?,tools?,context?}` returns
  `{device_id,session_id,answer}`. Existing client/tools/context validation and
  bounds apply. The session ID is unpredictable and changes on each start.
- `POST /api/session/connected {session_id}` and
  `POST /api/session/stop {session_id}` return `{device_id,session_id,ok:true}`.
  A missing/foreign/old ID cannot act on a replacement, even for the same device.
- `POST /api/say {session_id,text}` queues an untrusted screen notice and returns
  `{device_id,session_id,queued:true,ahead}`; at most ten, one every two seconds.
- `POST /api/tool/result {session_id,id,output,error?,untrusted?}` answers only
  a pending call in this session. Returns `{device_id,session_id,ok:true}`.
  Scoped client output is **always** framed as untrusted (even with
  `untrusted:false`); marker neutralization and output/error bounds apply.
- `POST /api/draft/confirm {session_id,id,to}` names exactly the displayed owned
  draft. Any additional body/key/envelope field is refused. A missing, modified,
  foreign, expired or already-claimed draft cannot send. Returns the fixed-safe
  send result `{device_id,session_id,state,outcome,mail_id?,delivery_state?,result_id?,code?}`.
- `POST /api/draft/cancel {session_id,id}` discards an unattempted draft. An
  uncertain/in-flight send cannot be cancelled as if it never ran.
- The model sees `privateer_ships` with `fleet:read`; `privateer_send {to}` and
  `privateer_confirm {}` with `ship:message`; plus client tools. There is no
  default recipient: name an allowed ship exactly. Send only drafts the last
  transcribed utterance (max 4000 characters), filler/whitespace included. The
  readback names the recipient and exact text; the screen renders it literally.
  Model readbacks are framed as untrusted data. If marker neutralization or
  Unicode/control normalization would change the exact readback, drafting
  refuses and asks for the message again rather than silently changing it. Only a
  later whole captain utterance `send to SHIP` or exact screen click confirms.
  Bare yes/send, wrong-recipient, readback echoes, screen context/notices/tool
  result fragments and mail cannot authorize it. Saying the message again
  replaces an unattempted draft with a fresh ID/readback. `privateer_confirm`
  only reports backend truth; model calls never authorize a send.
- Guessed answer/status/waiting/approval tools and non-tool app-server requests
  remain refused, even with an overpowered grant. Requests never carry reply
  correlation, option/approval payloads, custom urgency or deadlines.

Every new fleet-data output (HTTP/model result/voice append/transcript addition)
checks fresh capabilities, expected provenance, the entire session scope/ship
snapshot, and post-I/O device/binding/session ownership. Directory reads use
only `ships {schema:"runtime.v1"}` with its additional core release fence. The
new DTO is `{names,coverage}`: only inventory sources with original provenance,
state and timestamps survive; worker/job/capacity fields, totals, machines and
selection never pass through. Coverage remains unknown-domain/incomplete,
not a runtime activity or all-clear claim. Mail pushes are ignored without
retaining, speaking or acknowledging them; no `delivered` fallback.

An idle session rechecks authority about once every 30 seconds, coalesced with
real checks. State polls add **no core audit admissions**. Already-released
state may remain visible until that check detects revocation; a new output
never uses cached authority. A changed scope/ship snapshot stops/purges the
session; widening needs a new start. The API has no authority revision token:
a restrict/restore or pause/resume wholly between checks is not detectable by
the adapter (the runtime.v1 core read still has its own revision fence).
Revocation cannot recall audio/context already released or cancel an admitted
operation. A final check orders release, not an atomic transaction with receipt.
Waiters (32), events (256), transcript (60), utterances (20), client calls (8)
and notices (10) are bounded; overload refuses work or stops the adapter.

Errors keep `{code,error}` with fixed safe messages. Missing/expired/replaced
page token: 401 `not_paired`; denied core authority: 403 `access_denied`; core
outage/malformed response/timeout: 503 `unavailable`; foreign/stale session or
call: 409 `stale_state`; busy: 409 `session_running`; disabled draft action:
400 `unsupported`; startup: 502 `session_failed` or `codex_version`. Existing
shape/origin/media/rate errors remain. Clear protected display, stored page
token, microphone and peer connection on 401/403/503; no fallback credential.
Known credentials and token-shaped strings are redacted before truncation at
external text boundaries; pairing's one-time token response is intentional.
Never log raw auth/request/error objects or put a core key in a prompt.

## A2 keyed send custody and uncertainty

Only `send {id,to,kind:"request",body,idempotency_key}` reaches scoped core v2.
The server creates the mail ID/key and freezes the exact draft envelope in its
owning session's memory, never in browser/model arguments. Speech/click races
have one claimant. Core independently admits the grant, scope, resource and
recipient on every attempt. One fresh release fence covers each send result's
voice append, transcript copy and HTTP snapshot; polling adds no admissions.
Revoke-after-admission semantics are unchanged: a final fence may hide a result
but cannot undo already-admitted mail.

Before admission the adapter atomically fsyncs private `scoped-sends.json` with
only device UUID, grant UUID, target, mail ID, key **hash**, state and first-attempt
wall timestamp. No message body, transcript, raw key, bearer, core result or
arbitrary diagnostic is persisted. A failed prewrite refuses admission. Failed
post-admission persistence, pipe loss, timeout, malformed response or uncertain
core outcome is **outcome unknown; check before resending**, never "not sent".
Queued/held mail is not delivered or completed work; unknown delivery is labeled.
Core refusal/capacity codes are fixed/value-free. A refusal of an uncertain retry
does not erase the earlier uncertain fact.

An explicit same-draft live-session retry reuses the exact original envelope,
mail ID and key. No loops, fresh-key fallback or scope expansion. Retry ends at
24 hours from the first attempt, or backwards wall/monotonic time. Uncertainty
blocks replacement/cancellation into a fresh-key send in that session. The core
provides bounded 24-hour keyed protection, not indefinite exactly-once delivery.

Restart, session loss, rotation or re-pair discards all replayable values. Only
the same device/grant's authorized metadata is restored; the page says **outcome
unknown; check the conversation through an independently authorized surface
before resending**. There is no `/api/send/recover`, body storage, automatic replay
or cross-restart exactly-once claim. A newly confirmed message after independent
inspection is a new send, not recovery; duplicates/paraphrases cannot be detected
without stored text. A different grant/device cannot adopt historical metadata.

The ledger holds at most 32 rows per device, with no shared row cap: a full
device cannot consume another device's send slots. Terminal metadata expires
after 24 hours. Unresolved rows are never automatically evicted: at that device's
capacity, only its new sends refuse. Re-pairing/rotation does not clear them.

For explicit private operator disposition, **stop the door and verify its adapter
has exited** before using the local `scoped_send.py` CLI. Keep it stopped throughout
inspection/disposition; the CLI does not coordinate writes with a running adapter.
Run `python3 -B /CHECKOUT/plugins/live-voice/scoped_send.py --state-dir /PRIVATE/state
--device DEVICE_UUID --show-unknown` (one argv command). It shows only that device's
value-free unknown records and a `checked` digest. Check each listed mail ID in
its named ship's conversation through an independently authorized surface. Do not
infer failure from timeout, missing results or absence in this ledger. If safe to
discard the local uncertainty records, run the same command with
`--clear-unknown CHECKED_DIGEST` instead of `--show-unknown`. A changed unknown
record set refuses; inspect again. This clears **only** the selected device's
unknown rows, leaves terminal/other-device records intact, and never resends,
claims delivery or removes mail from core. Restart the door afterward to discard
cached history. Any subsequently confirmed message is a new send and can duplicate
old mail; clearing metadata does not extend core keyed protection.

No model/browser ledger-delete or recovery endpoint exists. Corrupt/unsafe
metadata custody fails closed. These examples do not authorize production use.
