# Telegram front door

Talk to your fleet from Telegram. Your messages go to your liaison (or any ship
you pick); every ship's mail to you arrives in the chat, and replying to a
message answers it. Nothing is relayed through a server: the door long-polls
Telegram from your machine, so it works behind NAT and hotel wifi.

```
📣 ops · notice
Nightly build is green again.

❓ ops · request · reply to answer
Deploy v2 to staging?
                                             yes, go ahead  ↰
                                                         👍
How is the API refactor going?
👍
   ↩️ liaison · reply
   Two PRs left; billing tests are flaky, I've asked ops.
```

This is a [front door](../../docs/architecture.md#front-doors): Privateer runs
it under `privateer door run telegram`, hands it the captain's mail from every
ship, and routes what you send to the right ship's home. The door keeps only
Telegram bookkeeping (which chat message carries which mail); the fleet's record
stays in Privateer.

## Set up

You need Python 3.11+ and `curl`.

1. Create a bot with [@BotFather](https://t.me/BotFather), or reuse one you
   already have, and send it a message.
2. Find your chat id: open `https://api.telegram.org/bot<TOKEN>/getUpdates` and
   read `message.chat.id`, or ask [@userinfobot](https://t.me/userinfobot). Your
   user id from the same place goes in `allowed_user_ids`.
3. Write a private config outside any repository, then register and start the
   door:

```sh
install -D -m 600 plugins/telegram/config.example.toml ~/.config/privateer/telegram.toml
$EDITOR ~/.config/privateer/telegram.toml      # bot_token, chat_id, allowed_user_ids
privateer door add telegram --config ~/.config/privateer/telegram.toml
privateer door start telegram
```

Privateer refuses a config file anyone else can read. To keep the token out of
the file, store it in the Secret Service and set `bot_token_secret` instead (see
`config.example.toml`).

On the first start the door skips anything already queued for the bot, so old
messages (from setting up the chat, or from a previous bridge on the same bot)
are not sent to a ship. Stop any other program polling the same bot first:
Telegram delivers each update to only one poller.

`privateer door status telegram` shows whether it is running; logs are in
`~/.local/state/privateer/doors/telegram/` (`door.log` from the supervisor and
the adapter's stderr, `state/telegram.log` from the adapter).

## Using it

- **A plain message** goes to the liaison as a request, so its answer comes back
  threaded under your message. Set `send_kind = "notice"` to just tell it.
  Which ship is "the liaison": the door's `--to SHIP` if you gave one, else the
  ship created as the liaison (`privateer init --liaison`), else a one-ship
  fleet's ship.
- **Reply to a message from a ship** to answer it. Replying to a request or
  decision sends your reply to it; replying to a notice or a reply sends a
  follow-up request to that ship. Replying to a message the door does not know
  (it remembers the last 2,000) is refused rather than guessed.
- **Decisions** whose payload lists `options` show buttons; tapping one answers.
- **`/to SHIP text`** sends one message to a ship. `/to SHIP` makes that ship the
  default for plain messages, `/to` shows buttons, and `/to default` resets.
- **`/status`** shows what each ship is doing, **`/waiting`** re-shows what still
  waits on you, and **`/ships`** lists the fleet. These read records and wake no
  one.
- **`/dismiss ID [reason]`** closes one waiting item without answering it: a
  request or decision ends as dismissed and its ship is told so, a notice is
  marked read. The first characters of the id, as `/waiting` shows them, are
  enough when they name one item.
- **Photos and files** are saved under the door's `state/inbox/` and the ship is
  told the path. **Voice notes** are transcribed when `[stt]` is configured,
  otherwise saved like files.
- **`/voice on`** speaks each message as a voice note after its text, through
  `[tts]`.

## Trust boundary

Only the configured `chat_id` is heard, and within it only `allowed_user_ids`
when set; everything else is logged and dropped. Anyone the door hears can mail
your ships as the captain, so in a group chat set `allowed_user_ids`. The bot
token is never put on a command line: it reaches curl in a private temporary
file and is redacted from logs.

## Coming from the Firstmate Telegram Bridge

This door carries over that bridge's Telegram plumbing, markdown rendering,
attachments, speech-to-text and text-to-speech, and its config keys for those
(`bot_token`, `bot_token_secret`, `chat_id`, `allowed_user_ids`, `proxy`,
`api_base`, `markdown`, `accept_files`, `max_file_mb`, `inbox_days`, `[stt]`,
`[tts]`). It does not read agent transcripts or terminal panes: ships tell you
things by mailing the captain, so you see what a ship chose to say rather than
every message it wrote mid-turn. Herdr-specific keys are ignored.
