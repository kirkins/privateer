# Scoped live voice client protocol 2 (A1 + A2 + runtime/workspace reads)

Opt-in companion to [trusted protocol 1](PROTOCOL.md), not a migration or fallback.
The [approved contract](../../docs/design/live-voice-scoped-access.md) governs policy.
A2 adds exact confirmed **new requests** with `ship:message`. Mail reads,
replies, decisions, option answers, mail notices, approvals, work/capacity actions,
generic RPC and grant administration remain disabled. PR6a adds metadata-only
runtime status and sessions; PR6b adds selected recorded detail/capacity reads,
using existing `fleet:read`. MCP is unchanged. Do not infer an empty inbox or idle fleet from disabled features.
The read-only workspace adds microphone-free HTTP directory/runtime reads;
it introduces no action or browser-automation authority.

The protocol-1 Host/Origin/JSON/CORS/no-store/CSP, body limits, WebRTC offer
shape, ChatGPT-only login, Codex version/config lockdown and process-group
shutdown rules also apply. `/api/version` returns
`{protocol:2,access_mode:"scoped",door:"0.6.0",features:[...]}`. The bundled
protocol-2 script verifies this before loading its separate storage key
`privateer-voice-scoped-token`. Never import a v1 token or fall back to v1.
Clients use workspace routes only when protocol 2 advertises `workspace_reads`.
Trusted-v1 does not advertise that feature and returns 404 for these routes.
Workspace startup and reads do not resolve/probe Codex, start an app-server or
require microphone access. The Codex and ChatGPT checks apply when starting
voice; their failure does not close an independently owned workspace.

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
Re-pairing replaces only that device's token/session/workspace. A browser token lasts 24
hours, travels only in `X-Privateer-Token`, never URLs/cookies. The persisted
binding contains only its digest, identities, credential hash and timestamps.
Re-pair daily. A copied token is the same logical device, not hardware identity.

Use existing core pause/resume/edit/expiry/rotate/revoke commands. Replace the
private credential file after core rotation and re-pair; no adapter restart is
needed. A changed credential invalidates only its own binding/session/workspace.
Restart may restore unexpired pairing bindings, never workspaces, workspace
results, live sessions, text, drafts or notices. Backwards wall time invalidates
bindings rather than extending expiry.
Do not use shared `instructions_file` or `transcript_file` in scoped config.

## Read-only workspace API

All three routes require the paired device token in `X-Privateer-Token` and the
existing Host/Origin/JSON/CORS guards. Methods are POST; GET returns 405. Client
fields never select a grant, credential, path, schema, core command or route.
Bodies are closed at the top level: fields beyond those listed below are 400
`bad_body`. This API exposes only directory/runtime reads, independently of
audio, existing voice drafts and model/client tools.

| Route | Request body | Successful response |
| --- | --- | --- |
| `/api/workspace/open` | `{}` or `{client:{name?,screen?}}` | `{device_id,workspace_id,views:["directory","runtime"],expires_in_secs:900}`; `views:[]` without `fleet:read` |
| `/api/workspace/read` | `{workspace_id,view:"directory",args?:{}}` | `{device_id,workspace_id,view:"directory",data:{names,coverage}}` |
| `/api/workspace/read` | `{workspace_id,view:"runtime",args?:{machine?,ship?}}` | `{device_id,workspace_id,view:"runtime",data:{available:boolean,text:string}}` |
| `/api/workspace/close` | `{workspace_id}` | `{device_id,workspace_id,ok:true}` |

The optional `client` object uses the existing `client_of` description validator:
`name` is an optional valid name of at most 64 characters and `screen` an optional
boolean. Unknown fields inside that description are ignored, as for session
start. Explicit non-object values are 400 `bad_client`. It is not retained or
added to instructions. No extension capabilities, `client_*` tools, context,
microphone, SDP offer or active voice session are needed to open or read.

`workspace_id` is an unpredictable opaque string, not authority. Opening freezes
the full admitted scope/ship snapshot and replaces only this device's previous
workspace. A message-only or otherwise non-`fleet:read` device receives `views:[]`;
opening is still permitted, but either read is 403 `access_denied`. Device tokens
are still the same logical device if copied into another client.

Directory `args` must be absent or an empty object. It uses only
`ships {schema:"runtime.v1"}` and the same closed `{names,coverage}` projection
as voice: ship names and inventory source coverage, without worker sources,
job/capacity facts, totals, machine lists or arbitrary record fields. Coverage
remains explicitly unknown-domain/incomplete; an empty names list is not an
all-clear. Sources retain their original state, mode, provenance and timestamps.

Runtime selectors are optional handle strings, each 1–128 characters from
`A-Z`, `a-z`, `0-9`, `_`, `.`, `:`, `@` and `-`. No ship selects
`status {schema:"runtime.v1",machine?}`; a selected ship maps to
`ship_runtime {ship,machine?}`. Selectors only narrow authority. Extra selectors
such as `schema`, `command`, `include_text`, paths or generic arguments are
refused. `scoped_runtime.render` supplies fixed backend text under the
[PR6a rendering contract](#pr6a-spoken-runtime-status-and-sessions), including
coverage, original ages and **Mail disabled; not checked.** The page displays
that status report; this response is not the full structured runtime DTO.
It contains no model-authored summary, raw records, progress/question text,
mail, recovery controls or work/approval actions.

An unavailable, malformed or denied runtime observation returns HTTP 200 with
`data:{available:false,text:"Runtime unavailable; could not check. Mail disabled; not checked."}`
only if the fresh capability and ownership checks still succeed. Hidden/absent
targets use that same fixed response. It never becomes a checked idle/zero
answer. Successful rendering returns `available:true`; its coverage and counts
can still be incomplete, stale or unknown. Directory transport/projection
failures instead return fixed 503 `unavailable` (or 403 `access_denied` for a
core access denial). Arbitrary underlying diagnostics are not returned.

Each explicit read uses core credential admission for its allowed query, then
one fresh capability check and final local ownership check before releasing
either the result or an error. The expected provenance and entire scope/ship
snapshot must still match, including after a scope expansion. Device binding,
current workspace generation and lifetime are checked around that I/O. Closing,
replacing, re-pairing, rotating or restricting during a read prevents late data
from entering a replacement workspace. There is no trusted-v1/operator fallback.
As with voice, this orders release rather than making receipt atomic with
revocation, and cannot recall an observation already delivered. A change and
restoration wholly between checks remains subject to the existing adapter/core
revision limitations described below.

The adapter keeps one workspace and at most one in-flight read per configured
device (maximum 16 devices). The read reservation remains until its worker
unwinds, even after close/reopen; a replacement cannot bypass the limit. Other
devices keep independent capacity. A workspace expires at 900 seconds since
open or its latest admitted/completed read; expiry is also checked before
release, and a backwards monotonic clock invalidates it. Maintenance only
expires local state. `/api/state` neither reads nor refreshes a workspace and
does not extend its lifetime. The bundled page reads the directory after pair
or load, then reads on **Refresh observations** and explicit ship selection;
it does not add a periodic workspace query.

Starting/stopping voice does not close the workspace. Opening/replacing/closing
a workspace does not start/stop voice. The workspace and results are volatile:
door restart requires a new workspace and explicit fresh reads, even if a valid
page pairing binding restores. No workspace result is restored as a new reading.
An admitted read that raises an API error discards only its generation; a
successful `available:false` runtime response leaves it open. Invalid request
shapes and a busy refusal do not replace another workspace.

Errors retain `{code,error}` with fixed safe messages:

| HTTP | Code | Meaning |
| --- | --- | --- |
| 400 | `bad_body` / `bad_client` | Invalid workspace shape, view, selectors or optional client description |
| 401 | `not_paired` | Missing, expired or replaced device token |
| 403 | `access_denied` | Missing `fleet:read`, changed authority/provenance or invalidated binding |
| 409 | `stale_state` | Unknown, foreign, closed, expired or replaced workspace |
| 409 | `workspace_busy` | This device already has a read in flight, including an older generation |
| 503 | `unavailable` | Core capability/directory read unavailable or malformed |

Existing host/origin/media/body-limit errors apply unchanged. Clients discard
protected results on an authority/ownership failure and explicitly reopen after
a stale or failed admitted read. They do not silently retry under another
credential. A workspace read adds no mail acknowledgment, sending, scheduling,
job cancellation, recovery, configuration, approval or browser-control action.

## Session API

All protected calls authenticate the header token into an immutable server-owned
device/binding. Browser fields cannot choose a grant, bearer, resource or path.
Only one audio session is active. Another device sees its own idle state and a
generic busy response to start, never the owner/name/error/transcript.

- `GET /api/state` returns `{device_id,session_id:null|ID,phase,live,error,draft,
  send_history,outcome,transcript,requests:[],tool_calls,heartbeat_secs,
  mail_mode:"disabled",queued:0,warning,codex}`. Poll about once per second while open. This is the owner's
  heartbeat and already-released snapshot, not a new core admission or workspace
  read. Idle state contains no other device's information. Audio stop purges
  session state while leaving a separately owned workspace usable.
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
- The model sees directory-only `privateer_ships` and metadata-only
  `privateer_status {machine?,ship?}` and the five PR6b recorded read tools below
  with `fleet:read`; `privateer_send {to}` and
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
- Guessed answer/waiting/approval tools and non-tool app-server requests
  remain refused, even with an overpowered grant. Requests never carry reply
  correlation, option/approval payloads, custom urgency or deadlines.

Every new fleet-data output (HTTP/model result/voice append/transcript addition)
checks fresh capabilities, expected provenance, the entire session scope/ship
snapshot, and post-I/O device/binding/session ownership. Directory reads use
only `ships {schema:"runtime.v1"}` with its additional core release fence. The
new DTO is `{names,coverage}`: only inventory sources with original provenance,
state and timestamps survive; worker/job/capacity fields, totals, machines and
selection never pass through. Coverage remains unknown-domain/incomplete,
not a runtime activity or all-clear claim. Runtime reads follow the separate
fixed mapping and rendering contract below. Mail pushes are ignored without
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
shape/origin/media/rate errors remain. For audio calls, clear protected display,
stored page token, microphone and peer connection on 401/403/503; no fallback
credential. A workspace 503 clears the failed reading and requires reopening
that workspace; it does not invalidate the paired token by itself. Workspace
401/403 clears protected display and the stored token as usual.
Known credentials and token-shaped strings are redacted before truncation at
external text boundaries; pairing's one-time token response is intentional.
Never log raw auth/request/error objects or put a core key in a prompt.

## PR6a spoken runtime status and sessions

`privateer_status {machine?,ship?}` is offered only with existing `fleet:read`.
No ship selects `status {schema:"runtime.v1",machine?}`; a named ship selects
`ship_runtime {ship,machine?}`. Selectors only narrow existing authority. Extra
fields (including schema, command and include_text) are refused; no generic RPC,
new scopes, grants, routes, recovery, work or approval actions are added.
`read_only_runtime` is advertised in `/api/version` features. Trusted-v1
`door.py::status_text` and the directory-only `privateer_ships` remain unchanged.

`scoped_runtime.py` renders fixed backend words, not model-authored summaries or
record text. Runtime-domain/inventory completeness, requested/queried/reused/
cached/omitted machine lists, source states/modes, original observation/receipt
and age timestamps precede counts. It consumes core `observed_totals` with
completeness and contributing sources, plus each ship's `runtime.current/facts`;
it never recomputes census or freshness. Incomplete positive counts say
**at least N observed**; incomplete zeros say **could not check**, not checked
zeros. Counts separate checked first-mate/crew/errand workers, commands,
uncertain jobs, overlapping session categories, recorded job states and open task
backlog. Stored running jobs and backlog never establish execution. Recorded
harness/host-mode configuration is explicitly not liveness. Optional checked
session rows speak only role/state and allowlisted harness/host-mode; opaque
references, binding identity, paths, options and authored text are not spoken.
Omitted/unsupported detail never means no sessions.

Cache/carry is **last observed**, retains original timestamps and cannot establish
current working/counts. Reused observations keep their original timestamps and
reported age. A relative age beside the original time uses the rendering clock,
not a new observation; carried facts never become current. Missing ages stay
unknown. Non-covered source states take precedence over cache/carry wording.
Missing/unreadable/unsupported/timeout is **could not check**, never
idle/zero/free. Hidden, absent and removed targets share the nonrevealing
**Runtime unavailable; could not check** response; absence never proves retirement.
Malformed coverage fails closed. Cross-machine reverse-only coverage says
**unsupported read direction / incomplete feature**, not mailbox repair or an
empty inbox. Every runtime answer ends **Mail disabled; not checked.** There is
no mail read, acknowledgement, unsolicited notice or authored progress speech.

Speech is bounded to 7800 characters, below the transcript's 8000-character cap,
and at most twelve machine labels per list, twelve source observations, twelve
ships and twelve session rows per spoken ship. Multi-ship overviews leave session
rows out and say so; ask about a ship by name for rows. Whole metadata lines and
ship summaries are omitted with an explicit notice before counts, never silently
cut mid-sentence. The runtime coverage line and closing mail line are retained.
Totals are core-scoped observations, never recalculated
from those spoken rows. All new output uses the existing post-I/O capability and
session fence: restrictions/replacement during reads drop late results. Failed
reads also undergo that fence before the fixed unavailable answer; no trusted-v1
or operator fallback. State polls still only re-serve released snapshots. Fake
app-server and real-supervisor fixtures are not real-account/audio acceptance.
Selected recorded-job/work detail and capacity follow the PR6b contract below.

## PR6b spoken selected recorded detail and capacity

Existing `fleet:read` offers these fixed, read-only model tools. Their names map
one-to-one to the existing protocol-2 core commands, without the `privateer_`
prefix:

| Voice tool | Model arguments |
| --- | --- |
| `privateer_ship_jobs` | `ship`, optional `state`, `kind`, `limit`, `continuation` |
| `privateer_job_inspect` | `ship`, exact `reference` from a job read |
| `privateer_ship_work` | `ship`, optional `limit`, `continuation` |
| `privateer_machine_capacity` | `machine`, optional `ship`, `task_id` (task requires ship) |
| `privateer_runtime_needs` | `ship`, optional `limit`, `continuation` |

Limits are 1–50; no automatic paging. Jobs accept native states plus `completed`
and the four native job kinds. Detail refs contain exactly `origin`,
`incarnation`, `ship`, `task_id`, `id`; the adapter forwards these unchanged and
requires the returned detail to match. There is no raw-ID fallback. Opaque
continuations remain core-bound to authority, source, scan and filters; a stale
continuation/ref produces the same nonrevealing **Recorded detail unavailable;
could not check** response as hidden/absent targets, without a fallback/retry.
No new freshness policy, scope, grant, route or action is introduced.

Model arguments cannot supply `include_text` (even false), schemas, commands,
paths or routes. The backend forces `include_text:false` for job/work/needs
commands; capacity has no text option. **Progress or question text is withheld**,
including when the grant also has `mail:read`. Park-next, schedule reasons,
question choices, titles, argv, paths and arbitrary diagnostics are not spoken
or returned to the model. Metadata-only progress timestamps remain available.
Authored-text speech is a separate, undelivered follow-up.

`scoped_detail.py` validates coverage and renders original observation times,
unknown ages and age bases using an injectable rendering clock before recorded
counts. Counts name the authorized scan window and its completeness, not page
length or full history; retained job history explicitly leaves prior/pruned
history unknown. Queue/park/dependency waits, terminal outcomes and safe lineage
are metadata only. **Resumed is lineage, not success**; recorded state does not
establish live process identity. Schedule claims/offers/backlog are not workers.
Timestamp-bearing detail rows are whole units; omissions are stated before
counts. Speech stays within the same 7800-character budget and ends
**Mail disabled; not checked.** Unknown/unavailable is never idle/zero/free;
reverse-only says **unsupported read direction / incomplete feature**.

The result's first text item is backend speech. A separate, closed tool-only
metadata text item holds exact refs (including related job refs and optional
work generations), task/dependency IDs, need IDs, source/snapshot and continuation
for explicit follow-up reads. Job, task, mail and composite need IDs (including
embedded origin hashes) are never spoken; speech names kind, state and relationship
instead. These are not read aloud or copied into the backend speech transcript;
no authored fields or generic DTO passthrough is used. Refs for omitted speech
rows remain intact in that page's tool metadata. No action tool consumes them.

Capacity reads use core heavy-slot headroom, never Python admission arithmetic,
free-agent estimates or an admission promise. Host/charter caps distinguish
finite, known-unbounded and unknown policy. An unset host cap says **not configured;
runtime default**, using the policy's `max_heavy_jobs` source rather than treating
its null configured value as an explicitly unbounded cap. Holds remain conservative
even for uncertain payloads. Queue classifications are last admission observations, not
current diagnoses. Host CPU/memory/per-filesystem readings stay separate from
holds/headroom. Named-ship grants must select a ship; only selected holds/queue
are spoken; restricted queue rows must carry exactly the unattributed `capacity`
blocker. Shared policy, headroom, totals, readings and routes are withheld,
never reconstructed by subtraction. Null readings/headroom stay unknown. Needs
always say partial, including unknown aggregate mail coverage; reading cannot
answer or approve a need.

Every result, including failed reads and opaque metadata, uses the existing
post-I/O scope/provenance/device/session fence. Late results after restriction
or replacement are dropped. Core reads may write bounded registry continuation
handles and ordinary authorization bookkeeping, never home/work/mail/resource
state. `scoped_access.py`, `scoped_send.py` and trusted-v1 behavior are unchanged.
Focused renderer tests, adapter race tests and the real-supervisor linked-gateway
fixture cover the contract; they are not real-account/audio acceptance or a
deployment. Recovery/configure/work/approval actions remain unavailable.

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

The ledger admits at most 32 rows per device, with no shared row cap: a full
device cannot consume another device's send slots. Runtime reads are bounded at
576 KiB: the maximum 16 configured devices × 32 rows × 1 KiB, plus 64 KiB
headroom. Terminal metadata expires after 24 hours. Unresolved rows are never
automatically evicted: at that device's capacity, only its new sends refuse.
Re-pairing, rotation and configuration removal do not clear them. Retained rows
from removed devices may eventually exceed the file read bound and refuse
startup; offline disposition remains available beyond that bound.

For explicit private operator disposition, **stop the door and verify its adapter
has exited** before using the local `scoped_send.py` CLI. Keep it stopped throughout
inspection/disposition. The adapter holds an exclusive `flock` on the private
state directory's `scoped-state.lock` for its lifetime, even without a session.
Both CLI actions acquire that lock non-blocking before reading or writing; a
held lock refuses with **stop the voice door first**. Never unlink the lock file.
Offline maintenance permits file sizes and device counts beyond runtime bounds
and devices absent from configuration (it reads the full metadata file into
memory). It still validates private file custody, every row and the exact digest.
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
