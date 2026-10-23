# Live voice spike: Codex realtime login probe

An experimental go/no-go probe for a voice front door. It answers one
question: can a **managed Codex login** (Sign in with ChatGPT, done by Codex
itself) start a realtime voice session through the official `codex
app-server` and get audio back?

This is not a door and not a plugin yet: there is no manifest, nothing
registers it, and Privateer never runs it. It is a single script you run by
hand.

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
   `outputModality: "audio"`, websocket transport (audio comes back over the
   same JSON-RPC pipe), no startup context, client-managed handoffs.
5. Waits for `thread/realtime/started`, then sends one text turn with
   `thread/realtime/appendText` ("Say the word ready.").
6. Collects realtime events for up to `--timeout` seconds (default 30),
   ending early once audio has flowed and the assistant transcript is done,
   or after `--settle` quiet seconds following audio.
7. `thread/realtime/stop`, then closes stdin, waits, sends SIGTERM, then
   SIGKILL to the app-server's whole process group, and checks the group is
   gone.

What it never does: read, copy or print Codex's credential files or any
token; set or pass `OPENAI_API_KEY` (it removes `OPENAI_API_KEY`,
`CODEX_API_KEY` and `AZURE_OPENAI_API_KEY` from the child's environment); call
an OpenAI endpoint itself; approve anything (every server request, such as a
command approval, is declined, and any Codex turn the session delegates to is
interrupted). The app-server's stderr is discarded. Server error text is
truncated and token-shaped strings are redacted before printing. The account
email is never printed.

## Run it

```sh
python3 plugins/live-voice/spike/probe.py
python3 plugins/live-voice/spike/probe.py --codex /path/to/codex --save-audio /tmp/ready.wav
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--codex PATH` | `codex` on PATH | the Codex CLI to run as `codex app-server` |
| `--timeout S` | 30 | seconds to wait for the spoken answer |
| `--rpc-timeout S` | 20 | seconds to wait for each JSON-RPC response |
| `--settle S` | 3 | quiet seconds after audio that end the answer |
| `--grace S` | 5 | seconds per shutdown stage before escalating |
| `--model`, `--version` | `gpt-live-1-codex`, `v3` | realtime model and protocol version |
| `--prompt TEXT` | `Say the word ready.` | the text turn |
| `--append text\|speech` | `text` | send the prompt with `appendText` or `appendSpeech` |
| `--save-audio PATH` | off | write received audio as a 16-bit PCM WAV |

It needs nothing but a Codex CLI that is already logged in with ChatGPT
(`codex login`). It uses that subscription's realtime allowance.

## Read the verdict

The probe prints one JSON object on stdout:

```json
{"verdict": "go", "reason": "realtime session started and returned audio",
 "codex_version": "codex-cli 0.162.1", "auth_mode": "chatgpt", "plan_type": "plus",
 "steps": [{"method": "initialize", "ok": true, "ms": 40}, "..."],
 "audio_bytes": 96000, "audio_format": [{"sample_rate": 24000, "channels": 1, "chunks": 50}],
 "transcript": "Ready.", "events_seen": {"thread/realtime/outputAudio/delta": 50, "...": 1},
 "realtime_item_types": {}, "realtime_errors": [],
 "shutdown": "clean", "process_group_gone": true}
```

- **`go`**: realtime started and audio came back. Exit status 0.
- **`no-go`**: the server refused: `thread/realtime/start` or the text turn
  answered with an error (look at the failing step's `error.code` and
  `error.message`, for example a 401 `no_matching_rule`), or the session
  reported `thread/realtime/error` / `closed` before any audio
  (`realtime_errors`). Exit status 0. A refusal is a valid answer; the probe
  never retries another way.
- **`error`**: inconclusive. The login was refused by the probe itself
  (`auth_mode` is not `chatgpt`), Codex was not found, a step timed out, or
  the session started but no audio came within `--timeout`. Exit status 1.

`events_seen` counts every notification by method (server requests appear as
`request:<method>`), `realtime_item_types` counts the `type` of realtime items,
and `audio_format` lists each sample rate and channel count seen. Audio
content is never printed.

## Limits

- Realtime in the app-server is **experimental**: methods, fields and the
  `gpt-live-1-codex` model can change or disappear in any Codex release. The
  probe was written against the protocol generated by `codex app-server
  generate-json-schema --experimental` for codex-cli 0.162.1.
- A `go` from one run shows the path works for this login, this plan and this
  Codex version today, nothing more. Subscription usage limits apply.
- `--save-audio` assumes 16-bit PCM (the session's `audio/pcm` format) and
  writes only chunks of the first sample rate and channel count seen.
- Process-group cleanup is POSIX. A helper that leaves the group
  (`setsid`) is not swept.

## Tests

`tests/live_voice_probe_test.py` runs the probe against a fake app-server
(`tests/fake_codex_app_server.py`) over real pipes: the go path, a 401 refusal,
a session that errors and closes, the API key refusal (realtime never called),
and a silent server that ignores EOF and SIGTERM and must be killed. It runs
under `cargo test --test python_suites`, or directly:

```sh
python3 tests/live_voice_probe_test.py
```
