# Live voice client protocol, version 1

How a client talks to the live voice door: the bundled page in `page/` is the
first client, and anything else (a browser extension, a desktop app) can be
built from this document alone. The door's code is in `web.py` and `door.py`;
where this document and the code disagree, that is a bug.

A client does four things: it pairs once, starts a WebRTC voice session by
trading an SDP offer for an answer, polls the door's state every second while
it is open, and stops the session. Audio flows between the client and OpenAI
directly. The door only relays the SDP and puts its own words (notices,
read-backs, results) into the session.

## Versions

`GET /api/version` needs no token:

```json
{"protocol": 1, "door": "0.2.0", "features": []}
```

- `protocol` is this document's version. A client checks it first and refuses
  to go on (or falls back) when it is not a version it was written for.
- `door` is the plugin's own version (`manifest.json`), for bug reports only.
- `features` names additions made within version 1. An additive change (a new
  field, a new endpoint, a new error code) keeps `protocol: 1` and adds a
  name here, so a client can test for it. A client must ignore fields and
  features it does not know. A breaking change bumps `protocol`.

The version is only at `/api/version`; `/api/state` does not repeat it.

## Where the door is

The door listens on `127.0.0.1` only, on `port` from its config (8765 by
default). Its address is in the `url` file in the door's state directory
(by default `~/.local/state/privateer/doors/live-voice/state/url`). A client
uses `http://127.0.0.1:<port>` or `http://localhost:<port>` and nothing
else: the `Host` header must be one of those two with the door's port, or the
request is refused (`wrong_host`), whatever its Origin.

## Requests and replies

- Request bodies are JSON objects with `Content-Type: application/json`, at
  most 128 KiB. A POST with no body counts as `{}`.
- Every reply is JSON (`Content-Type: application/json`), and is never cached
  (`Cache-Control: no-store`).
- Every error is one shape, with an HTTP status from the table below:

  ```json
  {"error": "wrong pairing code", "code": "wrong_code"}
  ```

  `code` is stable within a protocol version; branch on it. `error` is a
  sentence for a person, safe to show, and may change. (The `error` field is
  the message so that clients written before this document, like the
  bundled page, keep showing it.)

### The token

Every endpoint except `/api/version` and `/api/pair` needs the token, sent as
a header on every call:

```
X-Privateer-Token: <token>
```

Never in the URL or a cookie. A missing, wrong or replaced token gets 401
`not_paired`: the client forgets its token and pairs again.

### Origin

A request with no `Origin` header, or with the door's own origin
(`http://127.0.0.1:<port>` or `http://localhost:<port>`), is the bundled page
or a non-browser client. Any other `Origin` is refused with 403
`wrong_origin`, and a `Sec-Fetch-Site` other than `same-origin` or `none`
with 403 `cross_site`, unless the origin is listed in the door's
`allowed_origins` (see [From a browser extension](#from-a-browser-extension)).

## Endpoints

| Method and path | Token | Request body | 200 reply |
| --- | --- | --- | --- |
| `GET /api/version` | no | none | `{"protocol": 1, "door": "...", "features": []}` |
| `POST /api/pair` | no | `{"code": "<pairing code>"}` | `{"token": "<token>"}` |
| `POST /api/session/start` | yes | `{"offer": "<SDP offer>"}` | `{"answer": "<SDP answer>"}` |
| `POST /api/session/connected` | yes | `{}` | `{"ok": true}` |
| `POST /api/session/stop` | yes | `{}` | `{"ok": true}` |
| `GET /api/state` | yes | none | the state object below |

`GET /` and `GET /app.js` serve the bundled page; they are not part of the
protocol. `HEAD` works wherever `GET` does.

### Pairing

1. The door writes a random one-time pairing code (10 characters from
   `A-Z` and `2-9`, without `I`, `O`, `0` or `1`) to the `pairing-code`
   file, mode `0600`, in its state directory, each time it starts. The
   person using the client reads it there; the client never reads it by
   itself over the network.
2. The client posts it to `POST /api/pair`. Case and spaces do not matter.
3. On success the reply carries the token and the code is used up: the file
   is emptied and the same code never pairs again.
4. **One token at a time.** A new pairing replaces the token; every client
   holding the old one gets `not_paired` from then on. The token survives a
   door restart (it is kept, `0600`, in `session-token.json`). Pairing again
   needs a fresh code, which the door writes when it restarts.
5. A wrong code is 403 `wrong_code`. Five wrong codes within a minute are
   429 `rate_limited` for the rest of that minute, even with the right code,
   and the code is replaced (read the file again). The limit applies to every
   origin alike.

### Session start

The client builds an `RTCPeerConnection` and, before creating the offer:

- adds exactly **one audio track** (the microphone), sending and receiving;
- creates **one data channel labelled `oai-events`** (it carries the realtime
  events; the door does not read it);
- adds nothing else (no video, no second track or channel).

The offer must therefore have exactly two media sections, `m=audio` and
`m=application ... webrtc-datachannel`, each with an `a=mid`, both listed in
one `a=group:BUNDLE` line (the browser default). Wait for ICE gathering to
complete (or a few seconds) before sending it: there is no way to send
candidates later. At most 64 KiB.

`POST /api/session/start` with `{"offer": "<the offer's SDP text>"}`:

- 200 `{"answer": "<sdp>"}`: set it as the remote description (`type:
  "answer"`). Starting takes the door a few seconds (it starts Codex and asks
  OpenAI); allow at least 45 seconds before giving up.
- 400 `bad_offer`: the offer has the wrong shape; nothing was started.
- 409 `session_running`: a session already exists (any phase but `idle`).
  Stop it first, then start. There is one session at a time.
- 409 `stopped_while_starting`: a stop came in while this start ran.
- 502 `session_failed`: Codex or OpenAI refused or did not answer in time;
  the `error` sentence says why (credentials redacted), and `/api/state`
  `error` repeats it.

### Connected

When the peer connection's `connectionState` becomes `connected`, the client
posts `POST /api/session/connected` (`{}`). The phase becomes `live`, and only
then does the door put mail notices into the session. Posting it again is
harmless. 409 `no_session` when there is no session to connect.

### Stop

`POST /api/session/stop` (`{}`) ends the session: the door stops realtime and
Codex. It always answers 200 `{"ok": true}`, with or without a session. The
client also closes its peer connection and microphone.

## State: `GET /api/state`

```json
{
  "phase": "live",
  "live": true,
  "error": "",
  "queued": 2,
  "recipient": "liaison",
  "heartbeat_secs": 10,
  "draft": {"to": "ops", "text": "Ship the docs fix on Monday",
            "readback": "I will send ops: Ship the docs fix on Monday. Say send to ops to confirm."},
  "transcript": [
    {"role": "captain", "text": "Tell ops to ship the docs fix on Monday"},
    {"role": "door", "text": "I will send ops: ... Say send to ops to confirm."}
  ]
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `phase` | string | `idle`, `starting`, `answered` or `live` (below) |
| `live` | bool | true exactly when `phase` is `live` |
| `error` | string | why the last start failed or the last session ended; `""` when none. Cleared by the next start |
| `queued` | number | ship mail held at the door: not yet spoken, or older mail not yet counted |
| `recipient` | string | the ship a plain "tell ..." goes to when no ship is named; may be `""` |
| `heartbeat_secs` | number | a session whose client has not polled for this long is stopped |
| `draft` | object or null | the message waiting for the spoken confirmation: `to` (the ship it will go to), `text` (the captain's exact words), `readback` (the door's own read-back sentence). Show `to` and `readback`: they are what the door will do, whatever the voice said aloud |
| `transcript` | array | the current session's last 60 finished lines, oldest first, each `{"role", "text"}`. Kept after a stop until the next start |

Phases:

| `phase` | Means |
| --- | --- |
| `idle` | no session |
| `starting` | a start is running: the door is starting Codex and asking for an answer |
| `answered` | the answer went back; waiting for the client's `connected` |
| `live` | the client reported the connection up; notices flow |

Transcript roles:

| `role` | The line is |
| --- | --- |
| `captain` | a finished utterance of the captain's, as transcribed |
| `voice` | a finished reply of the realtime voice |
| `door` | the door's own words put into the session: a mail notice, a read-back, a send result, the backlog count line, a summary |

### The backlog count line

The door never reads a backlog aloud. Older mail (see the README) arrives as
one `door` line:

> You have 161 older messages from 7 ships. Say summary to hear who they are
> from, or skip.

("more older messages" for a later count in the same session). If the captain
says "summary" within `draft_ttl_secs`, a `door` line follows with one line per
ship. New mail beyond a few notices is offered as "And N more; say more to hear
them." These are spoken lines, not separate fields: a client shows them as
transcript lines like any other.

## The heartbeat, and what stops a session

`GET /api/state` with a valid token is the heartbeat. A client polls it about
every second while it is open (the bundled page does exactly that), and in any
case more often than `heartbeat_secs` (10 s by default). Anything that ends a
session leaves `phase` `idle`, and usually a reason in `error`:

| What | How |
| --- | --- |
| Stop | the client's `POST /api/session/stop` |
| A missed heartbeat | no state poll for `heartbeat_secs` while `answered` or `live` (a closed tab, a crashed browser, a sleeping laptop): the door stops the session itself, within about a second of the limit; `error` says the client stopped checking in |
| The client going away | a client that can tell (the page's `pagehide`) posts stop at once, best effort (`fetch(..., {keepalive: true})`); the heartbeat covers it when that is lost |
| The app-server closing | realtime closes or Codex exits; `error` says why |
| A door restart | the session ends with the door process; a restarted door is `idle` |
| A failed peer connection | the client sees `failed` or `closed` and posts stop |

Mail that arrives while no session is live waits, unacknowledged, for the
next one.

## The spoken confirmation rule

Nothing is sent to a ship on a click or a tool call. A message is the
captain's own last finished utterance, read back by the door, and is sent
only when the captain's *next* utterance is a plain confirmation that names
the recipient: "send to <ship>" (also "send it to <ship>", "yes send to
<ship>", "confirm send to <ship>"). A bare "send" or "yes" sends nothing and
the door asks for the name; naming another ship sends nothing; anything else
drops the draft; an unconfirmed draft expires after `draft_ttl_secs` (60 s).
There is no endpoint that sends, confirms or approves anything, and nothing
here ever approves: approvals happen only on the captain's phone. A client
shows `draft` so the captain can see what the door will send, and to whom.

## What a client never gets

The door never sends a client a Codex or ChatGPT credential, an API key, or
any token other than its own pairing token. The SDP answer is the only thing
from the OpenAI side a client receives, and the audio then flows between the
client and OpenAI on that negotiated connection. A client must not ask for,
store or forward such credentials, and needs none.

## From a browser extension

An extension's pages send `Origin: chrome-extension://<extension-id>` (or
`moz-extension://<uuid>` in Firefox), which the door refuses unless the
captain lists it in the door's config and restarts the door:

```toml
allowed_origins = ["chrome-extension://<extension-id>"]
```

Each entry is an exact origin, as the browser sends it: `scheme://host[:port]`,
no wildcards, no `"null"`, no path or trailing slash. Accepted schemes are
`chrome-extension`, `moz-extension` and `safari-web-extension` (no port), and
`http` or `https` on loopback only (`127.0.0.1`, `localhost`, `[::1]`, with
the port unless it is the default): any other web origin is a site the captain
does not control. A bad entry stops the door at start with a message saying
which.

For a listed origin, and only for it:

- `Sec-Fetch-Site` may be anything (`cross-site`, `same-site`, `none`).
- Replies carry `Access-Control-Allow-Origin: <that exact origin>` and
  `Vary: Origin`. Never `*`, and no `Access-Control-Allow-Credentials`: the
  token is a header, not a cookie.
- A CORS preflight (`OPTIONS` on an API path, with
  `Access-Control-Request-Method` naming that path's method) gets 204 with
  `Access-Control-Allow-Methods` (that method), `Access-Control-Allow-Headers:
  Content-Type, X-Privateer-Token` and `Access-Control-Max-Age: 600`, plus
  `Access-Control-Allow-Private-Network: true` when the preflight asks for it.
  A preflight from any other origin, or for the wrong method, gets 405
  `method_not_allowed` and no CORS headers.
- The Host check, the token and the pairing limit apply exactly as for any
  other client.

In the extension's manifest, grant the door's address, for example
`"host_permissions": ["http://127.0.0.1:8765/*"]`, and call the door from the
extension's own pages or service worker. A content script's requests carry the
web page's origin, not the extension's, and are refused.

**Response headers and cors fetches.** Every reply also carries
`Cross-Origin-Resource-Policy: same-origin`, `Cross-Origin-Opener-Policy:
same-origin` and a strict `Content-Security-Policy`. None of these block a
listed origin's `fetch`: the Fetch standard applies the
Cross-Origin-Resource-Policy check only to `no-cors` requests (an `<img>` or
`<script>` load, which stays blocked), and a cors-mode `fetch` is governed by
`Access-Control-Allow-Origin` instead; COOP applies only to documents opened
as top-level pages; and a response's CSP governs that response only when it is
rendered as a document. So they stay as they are.

## Error codes

| `code` | Status | When |
| --- | --- | --- |
| `wrong_host` | 403 | `Host` is not `127.0.0.1:<port>` or `localhost:<port>` |
| `wrong_origin` | 403 | `Origin` is neither the door's own nor listed |
| `cross_site` | 403 | `Sec-Fetch-Site` is cross-site or same-site without a listed origin |
| `not_paired` | 401 | the token is missing, wrong or replaced |
| `json_only` | 415 | a POST without `Content-Type: application/json` |
| `body_too_large` | 413 | a body over 128 KiB, or a bad `Content-Length` |
| `bad_body` | 400 | the body is not a JSON object |
| `not_found` | 404 | no such path |
| `method_not_allowed` | 405 | the path takes the other method; any `PUT`, `DELETE` or `PATCH`; a preflight not from a listed origin |
| `wrong_code` | 403 | the pairing code is wrong or used up |
| `rate_limited` | 429 | five wrong pairing codes within a minute |
| `bad_offer` | 400 | the offer is not one audio track and one data channel, bundled |
| `session_running` | 409 | start while a session exists |
| `stopped_while_starting` | 409 | stopped while this start ran |
| `no_session` | 409 | connected with no session |
| `session_failed` | 502 | Codex or OpenAI refused, failed or timed out |
| `internal` | 500 | a bug in the door; its log says more |
