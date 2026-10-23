# Live voice spike: Codex realtime login probe

An experimental go/no-go probe for a voice front door. It answers one
question: can a **managed Codex login** (Sign in with ChatGPT, done by Codex
itself) start a realtime voice session through the official `codex
app-server` and get audio back?

This is not a door and not a plugin yet: there is no manifest, nothing
registers it, and Privateer never runs it. It is a single script you run by
hand.

It has two transports:

- **`webrtc`** (the default) answers the login question. Codex 0.162.1 opens
  a WebRTC realtime call with the managed ChatGPT login, the way the Codex
  terminal app does, so a `go` here means the backend accepted this login
  and answered the offer. The probe connects no media, so a webrtc `go`
  proves **auth, not audio**.
- **`websocket`** is kept for comparison. It carries audio back over the
  app-server's JSON-RPC, but Codex 0.162.1 runs websocket realtime only with
  API key auth. With a ChatGPT login it fails with `realtime conversation
  requires API key auth`, which the probe reports as `no-go`.

## What it does

`probe.py` (Python 3, standard library only) spawns `codex app-server` on
stdio and speaks its JSON-RPC:

1. `initialize` with `experimentalApi: true` (realtime is experimental), then
   `initialized`.
2. `account/read`. Only a `chatgpt` account goes further. An API key login
   (`apiKey`), no login, or any other mode is **refused**: the probe stops
   without starting a thread or calling realtime, because an API key bills
   per use.
3. `thread/start`: ephemeral, `read-only` sandbox, `untrusted` approvals, in
   an empty scratch directory.
4. `thread/realtime/start` with `model: "gpt-live-1-codex"`, `version: "v3"`,
   `outputModality: "audio"`, no startup context and client-managed handoffs
   (the parameters the Codex terminal app sends), plus the transport:
   - webrtc: `{"type": "webrtc", "sdp": <offer>}`. The probe builds a static
     offer shaped like the Codex voice host's: one audio m-line (opus
     48000/2, `sendrecv`, `rtcp-mux`), then the application m-line of the
     `oai-events` data channel (`UDP/DTLS/SCTP webrtc-datachannel`), bundled.
     Each m-line has `a=mid` and `a=setup:actpass`. The ICE ufrag and password
     come from `secrets`, and the `sha-256` fingerprint is of random bytes.
     It carries no ICE candidates, and no socket is ever opened.
   - websocket: `{"type": "websocket"}`.
5. webrtc: waits up to `--timeout` seconds (default 30) for the backend's
   answer, `thread/realtime/sdp`, or for `thread/realtime/error` / `closed`.
   It keeps only the answer's size and m-line kinds, never its text, which
   can carry session identifiers. No text turn is sent, because there is no
   media path to speak over.

   websocket: waits for `thread/realtime/started`, sends one text turn with
   `thread/realtime/appendText` ("Say the word ready."), then collects
   realtime events for up to `--timeout` seconds. It stops early once audio
   has flowed and the assistant transcript is done, or after `--settle`
   quiet seconds following audio.
6. `thread/realtime/stop`, then closes stdin, waits, sends SIGTERM, then
   SIGKILL to the app-server's whole process group, and checks the group is
   gone.

What it never does: read, copy or print Codex's credential files or any
token; set or pass `OPENAI_API_KEY` (it removes `OPENAI_API_KEY`,
`CODEX_API_KEY` and `AZURE_OPENAI_API_KEY` from the child's environment); call
an OpenAI endpoint itself; print SDP text; approve anything (every server request, such as a
command approval, is declined, and any Codex turn the session delegates to is
interrupted). The app-server's stderr is discarded. Server error text is
truncated and token-shaped strings are redacted before printing. The account
email is never printed.

## Run it

```sh
python3 plugins/live-voice/spike/probe.py
python3 plugins/live-voice/spike/probe.py --codex /path/to/codex
python3 plugins/live-voice/spike/probe.py --transport websocket --save-audio /tmp/ready.wav
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--codex PATH` | `codex` on PATH | the Codex CLI to run as `codex app-server` |
| `--transport webrtc\|websocket` | `webrtc` | webrtc waits for the SDP answer (auth); websocket sends a text turn and waits for audio |
| `--timeout S` | 30 | seconds to wait for the SDP answer or the spoken answer |
| `--rpc-timeout S` | 20 | seconds to wait for each JSON-RPC response |
| `--settle S` | 3 | websocket: quiet seconds after audio that end the answer |
| `--grace S` | 5 | seconds per shutdown stage before escalating |
| `--model`, `--version` | `gpt-live-1-codex`, `v3` | realtime model and protocol version |
| `--prompt TEXT` | `Say the word ready.` | websocket: the text turn |
| `--append text\|speech` | `text` | websocket: send the prompt with `appendText` or `appendSpeech` |
| `--save-audio PATH` | off | websocket: write received audio as a 16-bit PCM WAV |

It needs nothing but a Codex CLI that is already logged in with ChatGPT
(`codex login`). It uses that subscription's realtime allowance.

## Read the verdict

The probe prints one JSON object on stdout. A webrtc go:

```json
{"verdict": "go",
 "reason": "the backend answered the WebRTC offer: the login is accepted for realtime (auth, not audio)",
 "transport": "webrtc",
 "go_means": "auth only: the backend answered the WebRTC offer for this login; no media was connected, so audio is not proven",
 "codex_version": "codex-cli 0.162.1", "auth_mode": "chatgpt", "plan_type": "plus",
 "steps": [{"method": "initialize", "ok": true, "ms": 40}, "..."],
 "answer_sdp_bytes": 1450, "answer_media": ["audio", "application"],
 "audio_bytes": 0, "audio_format": [], "transcript": "",
 "events_seen": {"thread/realtime/sdp": 1, "thread/realtime/started": 1, "...": 1},
 "realtime_item_types": {}, "realtime_errors": [],
 "shutdown": "clean", "process_group_gone": true}
```

A websocket go:

```json
{"verdict": "go", "reason": "realtime session started and returned audio",
 "transport": "websocket", "go_means": "audio: ...",
 "codex_version": "codex-cli 0.162.1", "auth_mode": "chatgpt", "plan_type": "plus",
 "steps": [{"method": "initialize", "ok": true, "ms": 40}, "..."],
 "audio_bytes": 96000, "audio_format": [{"sample_rate": 24000, "channels": 1, "chunks": 50}],
 "transcript": "Ready.", "events_seen": {"thread/realtime/outputAudio/delta": 50, "...": 1},
 "realtime_item_types": {}, "realtime_errors": [],
 "shutdown": "clean", "process_group_gone": true}
```

- **`go`**, exit status 0. webrtc: a `thread/realtime/sdp` answer arrived,
  so the backend accepted the login and answered the offer. That proves
  auth, not audio (`go_means` says so in the output). websocket: realtime
  started and audio came back.
- **`no-go`**, exit status 0: the server refused. Either
  `thread/realtime/start` (or the websocket text turn) answered with an
  error, so look at the failing step's `error.code` and `error.message`,
  for example a 401 `no_matching_rule`. Or the session reported
  `thread/realtime/error` / `closed` before the answer or any audio, so look
  at `realtime_errors`. A refusal is a valid answer; the probe never retries
  another way.
- **`error`**, exit status 1: inconclusive. The probe itself refused the
  login (`auth_mode` is not `chatgpt`), Codex was not found, a step timed
  out, or the session started but no answer (webrtc) or audio (websocket)
  came within `--timeout`.

`events_seen` counts every notification by method (server requests appear as
`request:<method>`), `realtime_item_types` counts the `type` of realtime items,
and `audio_format` lists each sample rate and channel count seen. On webrtc,
`answer_sdp_bytes` and `answer_media` (the answer's m-line kinds) describe the
answer. Audio content and SDP text are never printed.

## Limits

- Realtime in the app-server is **experimental**: methods, fields and the
  `gpt-live-1-codex` model can change or disappear in any Codex release. The
  probe was written against the protocol generated by `codex app-server
  generate-json-schema --experimental` for codex-cli 0.162.1.
- A `go` from one run shows the path works for this login, this plan and this
  Codex version today, nothing more. Subscription usage limits apply.
- A webrtc `go` proves auth only. The answer is never applied, no ICE or DTLS
  runs, and no audio flows. Proving audio needs a real WebRTC peer (a browser
  or the Codex voice host), which is the next PR's job.
- The static offer carries no ICE candidates, while the Codex voice host
  gathers its candidates before sending. A backend that insists on a
  candidate or a real DTLS fingerprint could reject the offer. That would
  show up as a `no-go` whose error names the SDP or offer, not the login,
  and should be read as inconclusive about auth.
- `--save-audio` assumes 16-bit PCM (the session's `audio/pcm` format) and
  writes only chunks of the first sample rate and channel count seen.
- Process-group cleanup is POSIX. A helper that leaves the group
  (`setsid`) is not swept.

## Tests

`tests/live_voice_probe_test.py` runs the probe against a fake app-server
(`tests/fake_codex_app_server.py`) over real pipes. webrtc: a go on the SDP
answer (the offer the fake receives must parse as SDP with exactly one audio
m-line, and the answer text must not reach the output), an `API key auth`
failure (no-go), and no answer (error). websocket: the go path, a 401
refusal, a session that errors and closes, and the `API key auth` failure.
It also covers the API key refusal (realtime never called), fresh ICE
credentials per offer, and a silent server that ignores EOF and SIGTERM and
must be killed. It runs under `cargo test --test python_suites`, or
directly:

```sh
python3 tests/live_voice_probe_test.py
```
