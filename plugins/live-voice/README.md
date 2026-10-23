# Live voice front door

Talk to your fleet out loud from a browser page on this machine. New ship
mail to you is read into the conversation, a few messages at a time; older
mail is only counted. You can tell a ship something, answer a ship's
question, or ask what the fleet is doing.

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
  (experimental API), `account/read` (only `chatgpt` goes on), `config/read`,
  `thread/start` (ephemeral, read-only sandbox, untrusted approvals, an empty
  scratch folder, no environment, your own Codex extras turned off, and the
  door's five tools), and `thread/realtime/start`
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
- **Your own Codex extras are off.** `environments: []` gives a delegated
  turn no shell or files, but it does not cover the MCP servers, plugins,
  apps and hooks in your own Codex config. The door reads Codex's effective
  config (`config/read`) and starts the thread with overrides that disable
  each MCP server and plugin it names (`enabled = false`; an empty
  `mcp_servers` table would merge and turn nothing off), set `apps._default`
  off and `web_search = "disabled"`, and turn off the `apps`, `plugins`,
  `remote_plugin`, `hooks`, `shell_tool`, `unified_exec`, `browser_use`,
  `browser_use_external`, `computer_use`, `multi_agent`, `memories`,
  `image_generation`, `tool_suggest` and `skill_mcp_dependency_install`
  features. If `config/read` fails, no session starts.
- Stop calls `thread/realtime/stop`, closes Codex's input, and ends its whole
  process group if it lingers. The page polls the door every second; a
  session whose page has not checked in for `heartbeat_secs` (10 s: a closed
  tab, a crashed browser) is stopped the same way, and closing the tab also
  asks the door to stop at once. Mail that arrives meanwhile waits for the
  next page.

## A backlog is never read aloud

A new door starts at the mail you have acknowledged elsewhere, which can be
far behind: on its first session it would otherwise read you hundreds of old
messages. Instead:

- **Older mail is counted, not read.** Mail is *older* when it was written
  before the door process started, or before the current session started
  and more than `backlog_age_secs` (10 minutes) ago, or when one ship's home
  has more than `gap_limit` (20) messages queued at the door. When a session
  goes live with older mail, the door waits a moment for it to stop arriving
  and says one line: "You have 161 older messages from 7 ships. Say summary
  to hear who they are from, or skip." Say "summary" to hear a line per ship
  (its count and the first words of its newest message, at most 8 lines);
  "skip", silence or anything else hears nothing more. Older mail that
  arrives later in the session gets its own count line.
- **A voice door acknowledges older mail once the count line is spoken**,
  whether or not you ask for the summary, in order per ship. You have that
  mail on your other channels (your phone, the ship itself), and you heard
  how much there was; it is not offered again here.
- **New mail is paced.** At most `max_notices_per_turn` (3) notices go into
  the session at once. Mail beyond that waits until the voice finishes
  speaking, or `pace_secs` (6 s), and is then offered: "And 4 more; say more
  to hear them." Say "more" for the next few. Mail waiting this way is not
  acknowledged until it is read.
- **Long mail is named, not read.** A notice longer than about 300
  characters becomes "ops sent a long message about <its first words>; it is
  on your phone."
- **Restarts do not repeat.** What was read or counted is recorded in
  `state/spoken.json` before it is acknowledged, so a restarted door neither
  reads nor counts it again. The file keeps each ship's acknowledged
  sequence and the keys of recently handled mail (at most 1000);
  `state/heard.json` keeps only the keys of the last 50 questions read aloud.

## Rules the door enforces in code

These hold whatever the voice model says:

- **Your exact words.** A message to a ship is your own transcribed speech,
  your last finished utterance, never the model's wording (no tool even takes
  message text). The door reads it back ("I will send ops: ... Say send to
  ops to confirm") and sends only when your *next* utterance is a plain
  confirmation that names the recipient: "send to ops", "send it to ops",
  "yes send to ops", "confirm send to ops". Naming the ship matters: the
  voice model speaks the read-back, so if it ever named a different ship than
  the door will use, your confirmation would name the one you heard and
  nothing would be sent. A bare "send" or "yes" sends nothing and the door
  asks for the ship's name; anything else drops the draft ("cancel" says so),
  and an unconfirmed draft expires after `draft_ttl_secs` (60 s). The page
  shows the door's own read-back, recipient included. The door's own words
  heard back never count as yours.
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
- **Mail is never lost.** Ship mail is queued, deduplicated by ship and mail
  id, and reported delivered only after it was put into a live session, as a
  notice or in a count line, in order per ship: Privateer moves a ship's
  delivery cursor to the highest message acknowledged, so nothing is
  acknowledged while an earlier message from that ship still waits. With no
  page connected it waits; restarting the door or the page sends nothing
  twice.
- **"Spoken" means put into the session.** A notice counts as spoken, and is
  acknowledged, once `appendText` (or `appendSpeech`) succeeds. If you talk
  over it (barge-in), or your playback is muted while you speak, you may not
  hear all of it. Ask "what is waiting on me?" to hear open questions again.

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
     Monday. Say send to liaison to confirm." The page shows the door's
     read-back. Say "send to liaison". Expect "Sent to liaison", and the
     liaison's inbox has those exact words.
   - Say just "send" to a read-back: the door asks you to name the ship.
     Say "send to" another ship: nothing is sent.
   - Say something, ask to send it, then say "cancel": nothing is sent.
   - When a ship mails you, the voice reads "<ship> says: ..." and only then
     is it marked delivered for this door.
   - On a first session with old mail waiting, expect one count line and
     nothing else; say "summary" or "skip".
   - A question from a ship can be answered: say the answer, then "answer
     that", then "send to <that ship>".
   - Mail asking for an approval is read as needing your phone; asking to
     answer it is refused.

6. **Stop** ends the session. **Mute** silences your microphone. Closing the
   tab ends the session too (at once, or within `heartbeat_secs`).

Logs: `~/.local/state/privateer/doors/live-voice/door.log` (the door writes
no transcripts or mail bodies there). A door that seems stuck dumps every
thread's stack there on `kill -USR1 <door pid>`.

## Settings

| Key | Default | Meaning |
| --- | --- | --- |
| `port` | 8765 | the page's port on 127.0.0.1 |
| `codex` | `codex` | the Codex CLI, a path or an argv list |
| `model`, `version` | `gpt-live-1-codex`, `v3` | realtime model and protocol |
| `draft_ttl_secs` | 60 | how long a read-back waits for "send to <ship>", and a count line for "summary" |
| `backlog_age_secs` | 600 | mail written before a session started and older than this is counted, not read |
| `gap_limit` | 20 | more than this many messages queued from one home are counted, not read |
| `max_notices_per_turn` | 3 | notices put into the session at once; the rest wait for "more" |
| `pace_secs` | 6 | how long a batch of notices waits for the voice to finish before offering the rest |
| `heartbeat_secs` | 10 | a session whose page has not checked in this long is stopped |
| `speak_with` | `text` | how the door's words enter the session: `text` (appendText) or `speech` (appendSpeech) |
| `speak_role` | `user` | the appendText role, `user` or `developer` |
| `voice_prompt` | unset | replaces Codex's own realtime voice prompt |
| `start_timeout_secs`, `rpc_timeout_secs`, `grace_secs` | 30, 20, 3 | timeouts |

## Limits

- Realtime in the app-server is **experimental** (written against codex-cli
  0.162.1). Methods, fields and the model can change in any Codex release.
- Not yet proven against the real service: that the voice delegates to Codex
  and that the delegated turn calls the door's tools; how the voice speaks the
  door's appended text (`speak_with` and `speak_role` are settings for that
  reason). The captain's hand test is the proof.
- Checked offline against codex-cli 0.162.1 (no network, a scratch Codex
  home, no login): `thread/start` accepts `environments: []` and the
  overrides above, and an MCP server in the Codex config is not started.
  Not checked offline: that the feature switches remove those tools from a
  delegated turn, and that `plugins.<id>.enabled = false` stops a plugin's
  own MCP servers (the `plugins` feature switch is the second guard).
- A captain utterance is the realtime `transcript/done` event with role
  `user`; if transcription mishears, the read-back shows it before anything is
  sent.
- At start the door ends a `codex app-server` left by a door that died
  outright, after `ps` shows the recorded process is still an app-server.

## Tests

`tests/live_voice_door_test.py` runs the door as a real subprocess (the test
plays the Privateer runtime), its page API over loopback HTTP, and a fake
`codex app-server` (`tests/fake_live_voice_app_server.py`) over real pipes. It
covers pairing, tokens, the Host and Origin checks and rate limits, the webrtc
sequence and its failures, the Codex config lockdown, a closed page ending its
session, mail spoken before it is delivered, in order per ship and
deduplicated by ship and id, a backlog counted in one line (summary, skip,
the age and gap rules, acknowledged in order), live mail paced per batch,
long mail in short form, bounded state files, the read-back and a
confirmation naming the recipient, each never-approve rule, handoffs,
declined server requests, and stop and restart without duplicate sends or
counts. It runs under
`cargo test --test python_suites`, or directly:

```sh
python3 tests/live_voice_door_test.py
```
