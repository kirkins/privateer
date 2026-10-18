# Design: away digests

Status: proposal for the Opus design review; nothing here is implemented. Task `privateer-away-digest` (captain finding 14, notice d8789f3d). Read with [away mode](away.md) (what `/afk` does today), [one block, one ask](one-block-one-ask.md) (PR 230: attachments to an open ask) and [architecture](../architecture.md) (the away paragraph). Names marked *new* do not exist yet; every other name was found in the tree at fc4f831.

## Problem

The captain went jogging and Telegram filled with repeats: "When the captain is away (afk, or not reading Telegram), batch non-urgent notices into a periodic digest, and send only requests and urgent items immediately. Ships shouldn't spend inference re-telling the captain the same thing."

What the runtime does today (`src/away.rs`, `src/door.rs`):

- **Away is declared, fleet-wide, per machine.** `/afk`, `privateer afk` and the door's `away` request call `away::enter`, which writes a row in the `away` table of the fleet registry (`crate::fleet::registry`). The first ordinary captain message (`Session::send` in `src/door.rs` calls `away::back`), `/back` or `privateer back` ends it. Linked machines do not see it: nothing in `src/machines.rs` carries the away row.
- **While away the door carries almost nothing.** `Session::poll` skips every captain message unless `away::breakthrough` allows it: a ship's (not `crew`'s) `"urgency":"high"` mail, at most `BREAKTHROUGHS_PER_HOUR` (3) fleet-wide. Requests and decisions are held too. So the captain hears nothing for hours, then on return gets `away::back`'s records digest plus a liaison summary built from `away::held`.
- **Held mail is found by time, not by cursor.** `away::held` reads each home's captain mail between `since_ms` and `returned_ms`. The door's durable `door_cursor` is moved by `delivered` to `MAX(through_seq, seq)`, so a breakthrough at seq 10 moves it past held seq 5 to 9. That is harmless today only because `back()` advances every cursor anyway and the summary is time-based.
- **Repeats are not detected.** Mail ids are idempotent (`mail.send` returns `duplicate: true` for a repeated id) and the adapter dedupes by id (`SENT`), but two notices with the same words and different ids are both shown. The only text rule is the adapter's optional `suppress_noop_ack`. PR 230 collapses mail on one block ask's topic (`block_ask::topic`, `payload.attached_to`), which edits the existing card instead of posting; nothing else collapses.
- **Not reading Telegram cannot be observed.** The Bot API gives a bot no read receipts in a private chat: `getUpdates` (`poll_telegram` in `plugins/telegram/door.py`) delivers the captain's messages and button presses, never "seen". The only signal is the captain's own activity.

So the finding asks for two changes to away mode: let requests through at once, and replace hold-until-return with a periodic digest. It also asks for a third, wider one: stop showing the captain the same fact twice.

## Model

### How "away" is known

1. **Declared** (round 1): the existing away period. Unchanged entry and return.
2. **Quiet** (round 3, opt-in): the door records the time of the captain's last message, command or button press (*new* registry table `door_activity(door, ms)`, written by `Session::request` on each adapter request). With the door setting `digest_when_quiet_mins` (*new*, off by default), a captain silent that long gets digests as if away, with no liaison words, no bearings change and no return summary; their next action ends it. This is an inference, so it is off until the captain turns it on (open question 1).

Telegram read state is not used: there is none to read.

### What goes at once and what is held

One pure function decides, `away::sort(mail, period, told)` (*new*), returning `Immediate`, `Card`, `Digest(section)` or `Repeat(of)`. The door calls it at poll time and records its effect; `mail.send` calls it read-only to tell the sender (see How ships learn). The table is in order of precedence:

| captain mail while away | route |
|---|---|
| `payload.attached_to` set (PR 230), or a `request_end` closure notice | `Card`: the adapter edits the existing card at once, as `push`/`attach`/`resolve` do today. Card edits do not notify. Never a digest line, never a repeat |
| `kind` request or decision, any urgency, including `request_kind` `production_approval` and `needs_hands` asks | `Immediate`, with the header suffix "· while you're away" |
| high urgency from a ship (not `crew`), within the fleet budget (3 an hour, as today) and the per-ship budget (2 an hour, *new* `SHIP_BREAKTHROUGHS_PER_HOUR`) | `Immediate`, 🚨 as today (`breakthrough: true`) |
| high urgency over either budget | `Digest(urgent)`: first in the next digest, marked "over the hourly limit" |
| normal or low notice whose normalized text was already shown or digested (see Duplicates) | `Repeat(of)`: not shown, counted |
| reply (to a captain request), normal or low | `Digest(answers)` |
| normal notice | `Digest(updates)`, collapsed per ship and task |
| low notice, or anything from `crew` | `Digest(routine)`: counted per ship, not listed |

Security problems and production at risk are not a separate kind: the brief (`src/cli.rs:273`) already reserves high urgency for them, and they go at once within the budget. A production approval is a request and is never budgeted. When the captain is not away, nothing changes except repeat suppression (open question 4).

### Digest cadence and shape

- **Cadence.** One digest per period while away, default 60 minutes, set with `/afk every 30m ...` (*new*, parsed by `away::parse` next to `until`; floor 10 minutes, ceiling 6 hours) and stored on the away row (*new* column `digest_every_secs`). The first is due one period after `since_ms`; each next one a period after the previous digest was built. A period with nothing held sends nothing. `/digest` (*new* door command) sends what is pending now and leaves the captain away, like `/held`.
- **Built from records, no model call.** The runtime builds structured lines; the adapter renders them. No ship and no liaison spends a turn on a digest.
- **One Telegram message, sent quietly** (`say(..., quiet=True)`, i.e. `disable_notification`), bounded to `TEXT_LIMIT` (3500) characters and 20 lines, each line's text cut at 120 characters. Overflow ends with "…and N more: /held".
- **Lines name an 8-character id prefix**, which `captain::locate` resolves (its `MIN_PREFIX` is 4), so `/dismiss ID` works on a line; the full id is the `id` of the matching `captain.feed` entry for the portal. Telegram cards do not show ids today, so the digest is the first place the captain sees them.

```
📋 Away digest 14:00–15:00 · 11 messages from 3 ships
🚨 Over the hourly limit
• deckhand 3f2a91c0 14:32: api health check failing on prod
Answers to you
• privateer 7bc1d2e4 14:10: PR 238 merged; CI green
Updates
• privateer · 3 updates · latest a91f03b2 14:50: PR 239 waiting on review
• corsair 0c44e1d9 14:41: nightly backup restored and verified
Routine: 4 low-urgency updates from 2 ships. Repeats not shown: 3.
/held for all of it · /dismiss ID closes one
```

Updates collapse per (ship, `task_id`): a ship's later notice on the same task supersedes the earlier ones in the digest, which shows the latest and a count. Mail without a task collapses per (ship, `correlation`) when it has one, else stands alone.

### Coming back: one catch-up, not a flood

`away::back` keeps its shape and its cursor move, and adds a `catch_up` value (*new*): the same builder run over everything not yet in a delivered digest. The adapter's `welcome` renders the welcome line and the catch-up as one message. An undelivered periodic digest is marked `superseded` and its items are in the catch-up, because its cursor never moved. The liaison summary request is sent only when the catch-up overflowed its 20 lines or something held is still open; otherwise the digests already said it (open question 3). Other registered doors get no catch-up; their cursors move as today.

### Duplicates

Three layers, in this order, and the first that matches wins:

1. **Same id**: already handled (`mail.send` idempotency, the adapter's `SENT` list).
2. **Same topic (PR 230)**: `block_ask::topic` and `open_primary` attach mail about an open block ask to it at send time. `away::sort` routes it as `Card` before any other rule, so an attachment is never also a digest line or a repeat, and the card's "Also:" list stays the one place it is told. If the adapter does not know the card (`attach` logs "whose card is not known here"), the digest counts it on its ask's line ("+2 attached to 5d1e…").
3. **Same words**: for low and normal notices only, the runtime hashes the body normalized (lowercase, whitespace collapsed, trailing punctuation dropped; digits kept, so "3 tests failing" and "4 tests failing" differ). The *new* registry table `captain_told(hash PRIMARY KEY, first_id, ship, first_ms, last_ms, repeats)` remembers what was shown or digested in the last 24 hours, from any ship, so a liaison relaying a ship's notice verbatim is caught. Rows older than 24 hours are pruned at write. Requests, decisions, replies and high urgency are never suppressed by text.

A repeat is still stored as mail in its home, unacknowledged, and `/held` lists it marked "repeat of ID": the durable fact is kept; only the telling is dropped. Paraphrased re-telling is not caught by a hash; the brief and the send note below are what reduce it (open question 5).

### How ships learn not to re-tell

- **The send reply.** `mail.send` to the captain already returns a `note` for an attachment ("Your message was added to … Do not resend."). For captain mail while the sender's machine has an away period, it adds a `delivery` field (*new*) from `away::sort`, with one of these notes: "The captain is away; this goes in the digest at 15:00. Do not resend." / "The captain already has this from deckhand as 3f2a91c0 at 14:32; it was not shown again. Do not resend." / "Urgent messages are over the hourly limit; this leads the next digest." The note is a preview: the door decides at poll time, and the budgets can move in between, so the note says "goes in the digest" and never promises "was sent".
- **Bearings.** `away::bearings_line` becomes: "Captain away since 23:40 (back about 8:00): requests and high urgency reach them at once; notices go in an hourly digest. Tell each outcome once."
- **Brief.** One sentence added after the urgency rule at `src/cli.rs:273`: "A notice the captain already has is not shown again, and the send reply says so; do not resend it."
- **Nothing is mailed back.** No notice tells a ship its line went out in a digest: waking a ship to say "delivered" spends the inference the finding asks to save.

### State and restart safety

| fact | where | written |
|---|---|---|
| away period, words, breakthroughs | `away` row (existing) + *new* `digest_every_secs` | `enter`, `breakthrough`, `back` |
| per-ship breakthrough counts | the same `breakthroughs` JSON, each entry gains `ship` | `breakthrough` |
| held items | the captain mail rows in each home, never copied | by `mail.send` as today |
| what each door has digested | *new* table `door_digest_cursor(door, through_seq)` in each home, beside `door_cursor` | only when the adapter acknowledges the digest |
| a built digest | *new* registry table `away_digest(id, door, period_ms, n, built_ms, through JSON, lines JSON, state, delivered_ms)`; `state` is `pending`, `delivered` or `superseded` | built after `poll` in the same tick |
| shown texts | *new* `captain_told` | when a door shows or digests a notice |

Rules that make a restart safe:

- **A digest is built once and re-sent verbatim.** Its id is `digest-<door>-<since_ms>-<n>`. A door builds the next digest only when none of its own is `pending`. After a supervisor or adapter restart the pending row is emitted again with the same id, and the adapter's `SENT` dedupe keeps the captain from seeing it twice.
- **The digest has its own cursor.** Immediate mail moves `door_cursor`; a digest moves `door_digest_cursor` per home to its recorded `through`. A breakthrough at seq 10 therefore no longer hides held seq 5 to 9 from the digest, which is the gap described under Problem.
- **Acknowledgement is one transaction per home** (cursor move), then the registry row is marked `delivered`; a crash between them re-sends a digest whose cursors already moved, and the adapter dedupes it by id.
- **Due time is computed, not timed.** "Due" is `max(since_ms, last built_ms) + digest_every_secs <= now`, evaluated each poll, so there is no timer to lose; a suspended laptop sends one late digest, not several.
- **Protocol.** A *new* runtime-to-door event `{"type":"digest","ref":"digest:<id>","id":ID,"lines":[...],"counts":{...}}`, acknowledged with the existing `delivered` (`Session::delivered` gains a `digest:` branch before its `ship:seq` parse). Only an adapter whose manifest says `"digests": true` (*new* `Manifest` field) is sent one; any other door keeps today's hold-until-return, because the current `door.py` logs and ignores unknown event types and would leave the digest pending forever.

## Failure modes

- **A digest is never sent** (Telegram failing, adapter down): the row stays `pending`, nothing advances, the adapter's `flush` retries every `RETRY_SECONDS` as for mail. No newer digest piles up behind it; the next one, once it goes, covers everything since. `privateer away` and `/status` add "digest pending since 15:00" (*new*). With the door not running at all, nothing reaches the phone, as today.
- **The captain misses an urgent item**: requests now always go at once; a high item over budget leads the next digest, at most one period late, where today it waited for the return. A ship that labels a real emergency `normal` is at most one period late too; the brief's urgency rule is unchanged.
- **A ship marks everything high**: the per-ship budget (2 an hour) keeps one ship from spending the fleet's three; its overflow leads the digest with a count ("deckhand: 7 urgent this hour, 2 sent"), so the captain sees the habit and can correct it.
- **A ship floods requests**: requests are not budgeted, by the captain's words. PR 230 already folds a blocked task's asks into one card. A per-ship request cap is open question 2.
- **Return races a pending digest**: if the adapter sends the pending digest just after `back()` marked it superseded, the captain sees a few lines twice, once. Accepted; the alternative is to block the captain's message on the adapter.
- **A repeat hides news**: only exact normalized text within 24 hours is suppressed, never asks or urgent mail, and `/held` still lists it.
- **Linked machines**: their ships get no away note, bearings line or `delivery` field, because the away row lives in this machine's registry. Their captain mail still reaches the captain's home (`machines::captain_home`) and is sorted there by the door, so delivery is right; only the sender's preview is missing.

## Changes by file

- `src/away.rs`: `sort` and its `Route`; `digest_every_secs` and `every` in `parse`; per-ship breakthrough budget; the digest builder (sections, collapse, bounds); `catch_up` in `back`; `captain_told` and normalization; the `bearings_line` wording; `away_digest` table beside `away`.
- `src/door.rs`: route held mail through `away::sort`; build and emit the `digest` event after `poll`; `digest:` refs in `delivered`; `door_digest_cursor` beside `cursor`; `door_activity` (round 3); the `digests` manifest field; a `digest` request for `/digest`.
- `src/mail.rs`: the read-only `delivery` preview and note in the captain branch of `mail.send`, after the attachment rule.
- `src/cli.rs:273`: the brief sentence.
- `plugins/telegram/door.py` and `manifest.json`: render a `digest` event as one quiet message and acknowledge it; render `catch_up` in `welcome`; `/digest`; the request header suffix while away; `"digests": true`; help text for `/afk every`.
- `docs/architecture.md` (away paragraph) and `docs/design/away.md` status.

## Test plan

In isolated homes (HOME, PRIVATEER_FLEET, XDG), through commands and durable state.

1. `src/away.rs` unit tests: `sort` over the precedence table (attachment beats everything; request at any urgency is immediate; high over the fleet or ship budget is digested; repeat only for low and normal notices); normalization (digits kept); digest bounds (20 lines, 3500 characters, overflow line); collapse per ship and task; due-time math with an injected clock.
2. `tests/door.rs`, extending the fake-adapter pattern of `away_holds_mail_lets_three_emergencies_through_and_summarizes_on_return`:
   - `away_sends_requests_at_once_and_digests_notices` (*new*): a request and a decision arrive while away; normal, low and reply mail does not, and `/digest` (the `digest` request) emits one `digest` event with the right sections and ids.
   - `a_pending_digest_is_resent_after_a_restart_and_acknowledged_once` (*new*): kill the adapter before `delivered`; the restarted session emits the same id; one acknowledgement moves `door_digest_cursor`.
   - `a_breakthrough_does_not_hide_earlier_held_mail_from_the_digest` (*new*): high at seq 10 after normal at 5 to 9.
   - `return_sends_one_catch_up_and_supersedes_the_pending_digest` (*new*).
   - `a_door_without_digests_keeps_holding_until_return` (*new*): manifest without the flag.
   - `one_ship_cannot_spend_the_fleets_urgent_budget` (*new*).
3. `tests/mail.rs`: `mail.send` to the captain while away returns the digest, repeat and over-limit notes; an attachment keeps its PR 230 note and gets no digest note; no note when not away.
4. `tests/telegram_door.py` (run by `the_telegram_door_works_end_to_end_against_a_fake_bot_api`): a digest is one `sendMessage` with `disable_notification`; a failing fake Telegram retries the same text and acknowledges once; card edits for attachments still happen while away; the catch-up renders in the welcome message.
5. `tests/context.rs`: the new bearings line while away.

Restart recovery, the per-ship race and the real adapter subprocess are covered by items 2 and 4; quiet-mode inference (round 3) gets its own door test when built.

## Rounds

1. `sort`, requests at once, the periodic digest with its cursor, table, protocol event and Telegram rendering, `/digest`, `/afk every`, the catch-up, the per-ship budget.
2. Repeat suppression (`captain_told`), the `delivery` send note, the bearings and brief wording.
3. Quiet mode, if the captain wants it.

Each round is committed after `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and the affected suites (`--test-threads=1`) pass as supervised heavy jobs.

## Open questions for the captain

1. **"Not reading Telegram."** Telegram tells a bot nothing about reading. Should silence count as away (quiet mode after, say, 45 minutes without a message or button press), or should away stay something you declare? Recommended: declared only for now; quiet mode as an opt-in after a week of digests.
2. **Requests while away.** You said requests go at once. Should a ship be limited (say three requests an hour while you are away, the rest in the digest), or never? Recommended: no limit at first; PR 230 already folds a blocked task's asks into one card.
3. **The liaison's summary on return.** With hourly digests, is the liaison's plain-words summary still worth its model turn? Recommended: only when the catch-up overflowed or something held is still open.
4. **Repeats when you are not away.** Suppress identical notices within 24 hours always, or only while away? Recommended: always; an exact repeat is never news.
5. **Paraphrased repeats.** Exact text only, or also a ship telling the same task outcome twice in different words (one notice per task status change)? Recommended: exact text now; revisit with the digest's collapse counts as evidence.
6. **Digest defaults.** Every 60 minutes, sent without a notification sound, 20 lines. Confirm, or name your own.
