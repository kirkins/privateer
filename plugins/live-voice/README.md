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

**Other clients.** The bundled page is one client of the door's API. Any
other (a browser extension, a desktop app) can be built from
[PROTOCOL.md](PROTOCOL.md), the live voice client protocol, version 1:
pairing, the session calls, the state a client polls, the heartbeat and every
error code. A client checks `GET /api/version` first. A browser extension's
origin must be listed in `allowed_origins`; nothing in the door is specific to
any one client. A client may describe itself at session start (`{"name",
"screen"}`: a client with a screen gets one generic line in the voice
instructions), and a client with a screen may confirm or cancel the waiting
draft by its id (`/api/draft/confirm`, `/api/draft/cancel`).

## How it works

- The page asks for your microphone, builds a WebRTC offer (one audio track
  and an `oai-events` data channel) and posts it to the door.
- The door starts the official `codex app-server` and runs: `initialize`
  (experimental API), `account/read` (only `chatgpt` goes on), `config/read`,
  `thread/start` (ephemeral, read-only sandbox, untrusted approvals, an empty
  scratch folder, no environment, your own Codex extras turned off, the
  door's six tools and its instructions), and `thread/realtime/start`
  (`gpt-live-1-codex`, `v3`, audio out, the webrtc transport carrying the
  offer, `clientManagedHandoffs`). The answer arrives in
  `thread/realtime/sdp` and goes back to the page. Audio then flows between
  the page and OpenAI directly. The browser never sees a Codex or ChatGPT
  credential, only that answer.
- When the voice hands work to Codex, the delegated Codex turn sees the door's
  tools (`privateer_send`, `privateer_answer`, `privateer_confirm`,
  `privateer_status`, `privateer_waiting`, `privateer_ships`) and calls them
  through `item/tool/call`. Each result goes back to that turn as the tool's
  output, and reaches the voice when the turn ends (below). The door's
  instructions to the turn (the
  `INSTRUCTIONS` in `door.py`) describe the draft, read-back and confirm
  cycle, say that "send to <ship>" is the door's to handle, and that
  `[Privateer door]` lines are authoritative.
- Each line of the door's reaches the voice once, by one of two ways. A line
  a tool call produced (a read-back, a status answer, what `privateer_confirm`
  reports) is the turn's result: with client-managed handoffs, the door hands
  it to the voice when the turn ends, prefixed `[BACKEND]`, the result Codex's
  own voice prompt waits on and treats as authoritative, and never the turn's
  retelling. A line the door produces on its own (the outcome of a spoken
  confirmation or a click, the message said again read back, an expiry, mail
  notices) goes into the session at once as a `[Privateer door]` line, since
  no turn may be waiting for it. A turn whose line the door already said gets
  a `[BACKEND]` line saying so, without the words again. A turn that used no
  door tool is handed its last answer, unless that answer talks of sending,
  which such a turn cannot know: then what the door knows now ("Not sent yet:
  ...", or an earlier send told as earlier, never as a new one). Every other
  server request (command, file or permission approvals, token refreshes,
  questions) is declined.
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
  your whole last finished utterance, filler included, never the model's
  wording (no tool even takes message text). The door reads it back and says
  that it goes as said: "I will send ops exactly: 'Okay, so tell ops the
  deploy moves to Friday'. Say send to ops to confirm, or say it again." It
  sends only when your *next* utterance is a plain confirmation that names
  the recipient: "send to ops", "send it to ops", "yes send to ops",
  "confirm send to ops". Naming the ship matters: the voice model speaks the
  read-back, so if it ever named a different ship than the door will use,
  your confirmation would name the one you heard and nothing would be sent.
  Say the message again, without the filler, and those words become the
  draft and are read back (any utterance of three words or more with no
  "send", "yes", "confirm" or "cancel" in it). A bare "send" or "yes" sends
  nothing and the door asks for the ship's name; anything else drops the
  draft ("cancel" says so), and an unconfirmed draft expires after
  `draft_ttl_secs` (60 s). The page shows the door's own read-back,
  recipient included, with **Send** and **Cancel** buttons that name that
  exact draft. The door's own words heard back never count as yours.
- **The true outcome.** Whatever confirms a draft (your words, a click, or
  both), it sends at most once, and the door says the outcome itself: "Sent
  to ops, id 3f9a2c." or "Not sent: ...". When the voice calls
  `privateer_confirm`, the door returns that same outcome; if your "send to
  ops" has not been transcribed yet, it waits for it (`confirm_wait_secs`,
  4 s), then answers "Not confirmed: I did not hear you say send to ops."
  The tool only reports: it cannot confirm for you. A confirmation with
  nothing waiting gets "Nothing is waiting to send, so nothing was sent." or
  "Already sent to ops, id 3f9a2c; not sent again."
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
     "I will send liaison exactly: 'Tell the liaison the docs fix can wait
     until Monday'. Say send to liaison to confirm, or say it again." The
     page shows the door's read-back. Say "send to liaison". Expect "Sent to
     liaison, id <six characters>", and the liaison's inbox has those exact
     words. Asked "did it send?", the voice should say the same.
   - Say a message with filler, hear it read back whole, then say it again
     without the filler: the new words are read back and are what is sent.
   - Press **Send to liaison** on the page instead of speaking: the same
     outcome is said into the session.
   - Say just "send" to a read-back: the door asks you to name the ship.
     Say "send to" another ship: nothing is sent.
   - Say something, ask to send it, then say "cancel" (or "actually, forget
     it"): nothing is sent.
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

**A transcript for hand tests.** Off by default. Set `transcript_file` (an
absolute path, or one starting with `~`) and the door appends each finished
line, your utterances, its own lines and the voice's replies, to that file as
JSON lines (`{"time", "role", "text"}`), creating it with mode `0600` (and
setting `0600` on a file that is already there). Only those lines are
written: never audio, SDP, tokens or the door's credentials, and token-shaped
text in an utterance is redacted. It holds what you said, so delete it after
the test.

**Your own notes for the voice.** Set `instructions_file` (for example
`"~/.config/privateer/live-voice-instructions.md"`) to add personal context to
the instructions the door gives Codex. It is read at each session start, at
most 16 KiB, and appended under its own heading after the door's instructions
and safety lines, which it cannot remove or replace (the rules above are
enforced in code either way). A missing, oversized or unreadable file is one
line in door.log, and the session starts without it; its contents are never
logged.

## Settings

| Key | Default | Meaning |
| --- | --- | --- |
| `port` | 8765 | the page's port on 127.0.0.1 |
| `allowed_origins` | `[]` | exact origins besides the door's own page that may call the API, e.g. `"chrome-extension://<extension-id>"`; see [PROTOCOL.md](PROTOCOL.md#from-a-browser-extension) |
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
| `confirm_wait_secs` | 4 | how long `privateer_confirm` waits for your "send to <ship>" to be transcribed |
| `transcript_file` | unset | opt-in transcript for hand tests: JSON lines, mode `0600`; see above |
| `instructions_file` | unset | your own notes appended to the door's instructions at each session start; see above |
| `start_timeout_secs`, `rpc_timeout_secs`, `grace_secs` | 30, 20, 3 | timeouts |

## Limits

- Realtime in the app-server is **experimental** (written against codex-cli
  0.162.1). Methods, fields and the model can change in any Codex release.
- Not yet proven against the real service: that the voice delegates to Codex
  and that the delegated turn calls the door's tools; how the voice speaks the
  door's appended text (`speak_with` and `speak_role` are settings for that
  reason). The captain's hand test is the proof.
- The door's `INSTRUCTIONS` reach the delegated Codex turn (`thread/start`
  `developerInstructions`). The realtime voice itself runs on Codex's own
  voice prompt (unless `voice_prompt` replaces it), which tells it to
  delegate actions and to treat `[BACKEND]` messages as authoritative; that
  is why a tool's result is handed back as `[BACKEND]`. A tool's line
  therefore reaches the voice only when its turn ends; a turn that never
  ends leaves it unsaid (the page's draft card still shows a read-back). The door
  cannot stop the voice from improvising aloud; it makes sure every word it
  supplies is the true outcome.
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
covers pairing, tokens, the Host and Origin checks and rate limits, client
protocol v1 (allowed origins and their CORS preflight, the version endpoint,
every error code in its documented shape, and a second minimal client written
only from PROTOCOL.md that pairs, starts, polls and stops), the webrtc
sequence and its failures, the Codex config lockdown, a closed page ending its
session, mail spoken before it is delivered, in order per ship and
deduplicated by ship and id, a backlog counted in one line (summary, skip,
the age and gap rules, acknowledged in order), live mail paced per batch,
long mail in short form, bounded state files, the exact read-back and a
confirmation naming the recipient, `privateer_confirm` reporting the true
outcome (sent once, not confirmed, the wrong ship, an expired draft, a
confirmation transcribed after the call, a message said again, a cancel in
other words), each door line reaching the voice once, handoffs carrying the
door's words (an earlier send told as earlier), the draft endpoints (a stale
click sends nothing, a click and the spoken confirmation together send once,
a confirm waiting on a click's send in flight, the runtime's id), the transcript
file, the client description and `instructions_file`, each never-approve
rule, declined server requests, and stop and restart without duplicate sends
or counts. It runs under
`cargo test --test python_suites`, or directly:

```sh
python3 tests/live_voice_door_test.py
```
