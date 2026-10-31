# Scoped live voice client protocol 2 (A1)

Opt-in companion to [trusted protocol 1](PROTOCOL.md), not a migration or fallback.
The [approved contract](../../docs/design/live-voice-scoped-access.md) governs policy.
This slice is read-only: no drafts, sends, mail reads, mail notices, approvals,
status/activity/work/capacity tools, generic RPC or grant administration. MCP is
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

- `GET /api/state` returns `{device_id,session_id:null|ID,phase,live,error,draft:null,
  transcript,requests:[],tool_calls,heartbeat_secs,mail_mode:"disabled",queued:0,
  warning,codex}`. Poll about once per second while open. This is the owner's
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
- Draft confirm/cancel endpoints require the own session but always refuse A1.
  No draft or recovery/send endpoint is provided. The model sees only
  `privateer_ships` when its grant has `fleet:read`, plus the client's tools.
  Guessed send/confirm/answer/status/waiting tools get a fixed unsupported reply,
  even with an overpowered grant. Speech, clicks or injected screen text cannot
  send or approve. Other app-server requests are declined.

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
