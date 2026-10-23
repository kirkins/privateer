# Live voice front door

Talk to your fleet out loud from a browser page on this machine. Ship mail to
you is read into the conversation; you can tell a ship something, answer a
ship's question, or ask what the fleet is doing.

This is a [front door](../../docs/architecture.md#front-doors): Privateer runs
it under `privateer door run live-voice`, hands it the captain's mail from every
ship, and routes what you send to the right ship's home. It is opt-in: nothing
runs until you `door add` and `door start` it.

**Cost.** The voice runs on Codex's realtime voice through your own `codex
login` (Sign in with ChatGPT). Usage counts against your ChatGPT plan. The door
never uses an API key: it strips `OPENAI_API_KEY`, `CODEX_API_KEY` and
`AZURE_OPENAI_API_KEY` from Codex's environment and refuses to start a session
unless Codex reports a ChatGPT login. There is no other voice fallback.

This version is for **this machine only**: the page listens on `127.0.0.1`.
Phone access, TLS and a speech-to-text fallback are later work.

## How it works

- The page asks for your microphone, builds a WebRTC offer (one audio track
  and an `oai-events` data channel) and posts it to the door.
- The door starts the official `codex app-server` and runs: `initialize`
  (experimental API), `account/read` (only `chatgpt` goes on), `thread/start`
  (ephemeral, read-only sandbox, untrusted approvals, an empty scratch folder,
  no environment, and the door's five tools), and `thread/realtime/start`
  (`gpt-live-1-codex`, `v3`, audio out, the webrtc transport carrying the
  offer, `clientManagedHandoffs`). The answer arrives in
  `thread/realtime/sdp` and goes back to the page. Audio then flows between
  the page and OpenAI directly. The browser never sees a Codex or ChatGPT
  credential, only that answer.
- When the voice hands work to Codex, the delegated Codex turn sees the door's
  tools (`privateer_send`, `privateer_answer`, `privateer_status`,
  `privateer_waiting`, `privateer_ships`) and calls them through
  `item/tool/call`. The door speaks each result into the session itself. With
  client-managed handoffs, a delegated turn that used no door tool has its
  last answer appended back to the voice by the door (prefixed `[BACKEND]`).
  Every other server request (command, file or permission approvals, token
  refreshes, questions) is declined.
- Barge-in is the server's voice activity detection. There is no client
  interrupt, so the page mutes its own playback while you speak.
- Stop calls `thread/realtime/stop`, closes Codex's input, and ends its whole
  process group if it lingers.

## Rules the door enforces in code

These hold whatever the voice model says:

- **Your exact words.** A message to a ship is your own transcribed speech,
  your last finished utterance, never the model's wording (no tool even takes
  message text). The door reads it back ("I will send ops: ... Say send to
  confirm") and sends only when your *next* utterance is a plain confirmation:
  "send", "send it", "yes send", "confirm" and the like. Anything else drops
  it ("cancel" says so), and an unconfirmed draft expires after
  `draft_ttl_secs` (60 s). The door's own words heard back never count as yours.
- **Never approves.** The door sends only `send`, `ships`, `status` and
  `waiting` to Privateer, never `permissions` or `approval`. It never answers
  mail whose payload has `approves`, a decision with an option marked
  `"approves": true`, a production or privileged request, or a tool permission
  ask, and never answers with an option id: a decision gets your words as
  `payload.answer.text`. Such mail is read to you as "this needs your approval
  on your phone", and nothing is sent.
- **Who gets it.** A message goes to the door's default recipient (the
  liaison, or the ship given to `door add --to`) unless you name a ship from
  the fleet's list. An answer goes to the ship that asked, and only while its
  question is still waiting on you.
- **Mail is never lost.** Ship mail is queued, deduplicated by mail id, and
  reported delivered only after it was put into a live session. With no page
  connected it waits; restarting the door or the page sends nothing twice.

## Set up and test by hand

You need Python 3.11+, a browser with WebRTC, and the Codex CLI logged in with
ChatGPT (`codex login`; `codex login status` should say ChatGPT).

1. Optionally write a private config (every key has a default; see
   [config.example.toml](config.example.toml)):

   ```sh
   install -m 600 plugins/live-voice/config.example.toml ~/.config/privateer/live-voice.toml
   ```

2. Register and start the door:

   ```sh
   privateer door add live-voice [--config ~/.config/privateer/live-voice.toml] [--to SHIP]
   privateer door start live-voice
   privateer door status live-voice
   ```

3. Read the one-time pairing code. It is written only to a `0600` file in the
   door's state directory, next to the fleet registry (by default
   `~/.local/state/privateer/doors/live-voice/state/pairing-code`):

   ```sh
   cat ~/.local/state/privateer/doors/live-voice/state/pairing-code
   ```

   The page's address is in `state/url` beside it, normally
   `http://127.0.0.1:8765/`. Open it, type the code and press Pair. The code
   works once; restarting the door writes a new one. Five wrong codes within a
   minute lock pairing for that minute and replace the code. A new pairing
   replaces the earlier page's token.

4. Press **Start** and allow the microphone. Status goes to `answered`, then
   `live` once audio connects.

5. Try it:
   - "What is the fleet doing?" The voice should read the status from records.
   - "Tell the liaison the docs fix can wait until Monday." Expect the read-back
     "I will send liaison: Tell the liaison the docs fix can wait until
     Monday. Say send to confirm." The page shows the draft. Say "send". Expect
     "Sent to liaison", and the liaison's inbox has those exact words.
   - Say something, ask to send it, then say "cancel": nothing is sent.
   - When a ship mails you, the voice reads "<ship> says: ..." and only then
     is it marked delivered for this door.
   - A question from a ship can be answered: say the answer, then "answer
     that", then "send".
   - Mail asking for an approval is read as needing your phone; asking to
     answer it is refused.

6. **Stop** ends the session. **Mute** silences your microphone.

Logs: `~/.local/state/privateer/doors/live-voice/door.log` (the door writes
no transcripts or mail bodies there).

## Settings

| Key | Default | Meaning |
| --- | --- | --- |
| `port` | 8765 | the page's port on 127.0.0.1 |
| `codex` | `codex` | the Codex CLI, a path or an argv list |
| `model`, `version` | `gpt-live-1-codex`, `v3` | realtime model and protocol |
| `draft_ttl_secs` | 60 | how long a read-back waits for "send" |
| `speak_with` | `text` | how the door's words enter the session: `text` (appendText) or `speech` (appendSpeech) |
| `speak_role` | `user` | the appendText role, `user` or `developer` |
| `isolate_codex` | true | delegated Codex turns get no environment (`environments: []`) |
| `voice_prompt` | unset | replaces Codex's own realtime voice prompt |
| `start_timeout_secs`, `rpc_timeout_secs`, `grace_secs` | 30, 20, 3 | timeouts |

## Limits

- Realtime in the app-server is **experimental** (written against codex-cli
  0.162.1). Methods, fields and the model can change in any Codex release.
- Not yet proven against the real service: that the voice delegates to Codex
  and that the delegated turn calls the door's tools; how the voice speaks the
  door's appended text (`speak_with` and `speak_role` are settings for that
  reason); and whether `environments: []` is accepted (`isolate_codex =
  false` drops it). The captain's hand test is the proof.
- A captain utterance is the realtime `transcript/done` event with role
  `user`; if transcription mishears, the read-back shows it before anything is
  sent.
- The door ends a stale `codex app-server` left by a crash only on Linux,
  where it can check the recorded process is still one.

## Tests

`tests/live_voice_door_test.py` runs the door as a real subprocess (the test
plays the Privateer runtime), its page API over loopback HTTP, and a fake
`codex app-server` (`tests/fake_live_voice_app_server.py`) over real pipes. It
covers pairing, tokens, the Host and Origin checks and rate limits, the webrtc
sequence and its failures, mail spoken before it is delivered and deduplicated,
the read-back and confirmation, each never-approve rule, handoffs, declined
server requests, and stop and restart without duplicate sends. It runs under
`cargo test --test python_suites`, or directly:

```sh
python3 tests/live_voice_door_test.py
```
