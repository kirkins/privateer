# Design: away digests

Status: round 2, revised after the Opus design review, and implemented on this branch (quiet mode stays deferred). Task `privateer-away-digest` (captain finding 14, notice d8789f3d). Read with [away mode](away.md) (what `/afk` does today), [one block, one ask](one-block-one-ask.md) (PR 230: attachments to an open ask) and [architecture](../architecture.md) (the away paragraph). Names marked *new* do not exist yet; every other name was found in the tree at fc4f831.

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

Round 2 of the design, after the Opus design review (pinned `design_review` on `task:privateer-away-digest`). The captain's answers to the review's open questions are taken as given below; [Decisions](#decisions) lists them.

### How "away" is known

Away stays **declared only**: `/afk`, `privateer afk` or the door's `away` request start it, and the first ordinary captain message, `/back` or `privateer back` ends it, as today. Inferring away from silence ("quiet mode") is deferred: Telegram gives a bot no read receipts in a private chat, and an inference that is wrong silences the phone.

### What goes at once and what is held

One pure function, `away::route(mail, period, settings)` (*new*), decides each captain message's route at poll time; the door records the decisions that budgets depend on. In order of precedence:

| captain mail while away | route |
|---|---|
| `payload.attached_to` set (PR 230) and not high urgency and not a reply, or a `request_end` closure notice | `Card`: sent at once; the adapter edits the existing card. If the adapter does not know the card, it shows the mail as a normal quiet message instead of only logging it |
| `kind` request or decision, any urgency, including `production_approval` and `needs_hands` asks | `Immediate` on **every** door, header suffix "· while you're away". After `loud_requests_per_hour` (3) from one ship in the last hour, further ones still go at once but without a sound (`"quiet": true`) |
| high urgency from a ship (not `crew`), within the fleet budget (`breakthroughs_per_hour`, 3) and the per-ship budget (`ship_breakthroughs_per_hour`, 2) | `Immediate`, 🚨, `breakthrough: true` (as today). High urgency is never reduced to a card edit: attached high-urgency mail takes this row, not `Card` |
| high urgency over either budget | `Overflow`: recorded on the away row (`overflow`, with id and time) **before** anything else, so after a restart and an hour passing the same message cannot also break through; it leads the next digest |
| anything else (normal and low notices, replies, `crew` mail) | `Digest` |

"Ship" for the budgets is the home the mail is held in (the event's `ship`); a liaison relaying for a ship spends the liaison's budget. A door whose manifest lacks `"digests": true` gets every `Card`, `Immediate` and breakthrough at once, and holds the rest for the return as today: only the digest event is gated by the flag.

### Digest cadence and shape

- **Cadence.** Due `digest_every_mins` (60) after the later of the away start and the period's last digest; `/afk every 30m ...` (*new*, parsed by `away::parse`; floor 10 minutes, ceiling 6 hours) stores a per-period value on the away row (`digest_every_secs`). With any overflow item held, it is due `urgent_due_mins` (15) after the first overflow since the last digest, whichever is sooner. A period with nothing held sends nothing. `/digest` (door request `digest`) builds one now and leaves the captain away.
- **Sound.** A digest is sent silently (`disable_notification`) unless it has an urgent section; a digest with over-the-limit urgent items always makes a sound.
- **Built from records, no model call.** The runtime builds the lines; the adapter joins them into one message. No ship and no liaison spends a turn on a digest.
- **Bounded.** At most `digest_lines` (20) item lines, each cut at `line_chars` (120); overflow ends with "…and N more: /held".
- **Lines name an 8-character id prefix**, which `captain::locate` resolves, so `/dismiss ID` works on a line.

```
📋 Away digest 14:00–15:00 · 11 messages from 3 ships
🚨 Over the hourly limit
• deckhand 3f2a91c0 14:32: api health check failing on prod
Answers to you
• privateer 7bc1d2e4 14:10: PR 238 merged; CI green
Updates
• privateer · 3 updates · latest a91f03b2 14:50: PR 239 waiting on review
• corsair 0c44e1d9 14:41: nightly backup restored and verified ×3
Routine: 4 low-urgency updates from 2 ships.
/held for all of it · /dismiss ID closes one
```

Updates collapse per (sender, `task_id`), else per (sender, `correlation`), else stand alone: the latest text is shown with the count of updates. Repeats of the same text on that key show as "×N" on the line instead of being dropped.

### Coming back: one catch-up, retried until delivered

`away::back` ends the period, marks the period's pending digests `superseded`, and for each door with digests builds a **catch-up** (*new*): an `away_digest` row of kind `catch_up` with its own id (`catchup-<door>-<since_ms>`), sent and acknowledged like any digest, so a failed Telegram call is retried by the adapter's `flush` and re-emitted by the next session. Its lines are the welcome line, everything held and not yet in a delivered digest, and **every open request still waiting, by line** (ship, id prefix, words), because `back()` moves every cursor past immediate requests that may not have been delivered yet. `privateer back` prints the same catch-up text. Then every door's cursor moves past the held mail, as today.

The liaison's summary is requested only when the catch-up overflowed its lines, or when no registered door takes digests (today's behaviour). Open requests are already listed in the catch-up.

### Duplicates and repeats

1. **Same id**: `mail.send` idempotency and the adapter's `SENT` list, as today. The digest builder also skips a second copy of one message id held in two homes (a desk copy), as `captain.feed` does.
2. **Same topic (PR 230)**: attachments go to the card (`Card` above), except high urgency.
3. **Same words, on one key.** The key is the sender plus the mail's `task_id`, else its `correlation`, else nothing; the text is the body normalized (lowercase, whitespace collapsed, trailing punctuation dropped; digits kept). A message is a **repeat** only when its text equals the *last* thing told on its key, so a state that flips back ("prod up", "prod down", "prod up") or recurs after a change is news each time, and generic bodies ("Done.") from different ships never collide. Cross-ship matching (any sender, same text) applies only to bodies of at least `cross_ship_chars` (200) characters, which catches a liaison relaying a ship's long notice verbatim. Paraphrases are not caught. The *new* registry table `captain_told(key, hash, id, ms, repeat)` keeps the last telling per key for `repeat_hours` (24), pruned at write; the decision for a message id is stable, so two doors agree.

Requests, decisions and high urgency are never treated as repeats. Suppression applies **only while away**, and even then a repeat is not dropped: it is counted "×N" on its digest line. When the captain is present, a repeat is still delivered, but quietly and marked "(again)". The durable mail is never changed.

### How ships learn not to re-tell

- **The send reply.** For captain mail while away, `mail.send` adds a `delivery` field and a note (after the PR 230 attachment note, which takes precedence): "The captain is away; this goes in the next digest. Do not resend." / for a repeat whose earlier copy is still held: "The captain will get this in the digest already (as ID); do not resend." / for a repeat already told: "The captain already has this (ID at 14:32); do not resend." / for high urgency over the limit: "Urgent messages are over the hourly limit; this leads the next digest, due within 15 minutes." The note is a preview: the door decides at poll time.
- **Bearings.** "Captain away since 23:40 (back about 8:00): requests and high urgency reach them at once; notices go in a digest. Tell each outcome once."
- **Brief.** After the urgency rule in `src/cli.rs`: "A notice the captain already has is not shown again, and the send reply says so; do not resend it."
- **Nothing is mailed back.** No notice tells a ship its line went out in a digest.

### Settings in one place

`away::Settings` holds every default: `digest_every_mins` 60, `digest_quiet` true, `digest_lines` 20, `line_chars` 120, `urgent_due_mins` 15, `breakthroughs_per_hour` 3, `ship_breakthroughs_per_hour` 2, `loud_requests_per_hour` 3, `repeat_hours` 24, `cross_ship_chars` 200. An optional `away.json` beside the fleet registry overrides any of them; `/afk every` overrides the period for one away period.

### State and restart safety

| fact | where | written |
|---|---|---|
| away period, words, breakthroughs (each with `ship`), `overflow`, `requests`, `digest_every_secs` | `away` row (registry) | `enter`, `route`, `back` |
| held items | the captain mail rows in each home, never copied | by `mail.send` as today |
| a digest, a catch-up, or a door's away-start baseline | *new* registry table `away_digest(id, door, since_ms, kind, built_ms, through JSON, lines JSON, loud, state)`; `state` is `baseline`, `pending`, `delivered` or `superseded` | built after `poll`; acknowledged by `delivered` |
| last telling per key | *new* registry table `captain_told` | when a door routes a message |

- **The digest cursor lives only in the registry.** For one door, one away period and one home it is the largest `through[ship]` over that door's `baseline` and `delivered` rows of the period. Nothing is written to any home when a digest is acknowledged, so one unreadable home cannot leave a digest pending: the builder skips that home (its cursor stays where it was) and the next digest picks it up.
- **Lower bound.** On its first poll in an away period a door records a `baseline` row whose `through` is its `door_cursor` in each home. The first digest therefore excludes mail delivered before the away period, and a second away period does not repeat mail delivered while the captain was present. Mail that arrived during the period while the door was down has a higher sequence than the cursor and is included.
- **A digest is built once and re-sent verbatim.** A door builds the next digest only when none of its own for the period is `pending`. A session emits each pending row (digest or catch-up) once; a restarted session emits it again with the same id, and the adapter's `SENT` list dedupes it.
- **Breakthroughs do not hide held mail.** Immediate mail moves `door_cursor`; the digest has its own cursor, so a breakthrough at seq 10 cannot hide held seq 5 to 9.
- **Due time is computed each poll**, so there is no timer to lose; a suspended laptop sends one late digest.
- **At least once.** Between a successful Telegram send and the adapter's `SENT` write there is a short window in which a crash sends the digest again, exactly as for mail today. Nothing is lost; rarely, something is shown twice.
- **Protocol.** A *new* runtime-to-door event `{"type":"digest","ref":"digest:ID","id":ID,"kind":"digest|catch_up","lines":[...],"loud":BOOL}`, acknowledged with the existing `delivered`. Only an adapter whose manifest says `"digests": true` is sent one.

## Failure modes

- **A digest is never sent** (Telegram failing, adapter down): the row stays `pending` and is retried; no newer digest piles up behind it, and the next one covers everything since.
- **The captain misses an urgent item**: requests always go at once; high urgency over the budget leads a loud digest within 15 minutes.
- **A ship marks everything high**: the per-ship budget (2 an hour) keeps one ship from spending the fleet's three; the rest lead the next digest.
- **A ship floods requests**: they still go at once; after 3 in an hour from one ship they go silently.
- **Return races a pending digest**: if the adapter sends a digest just after `back()` superseded it, the captain sees a few lines twice, once. Accepted.
- **A repeat hides news**: it cannot; a repeat is counted on its line while away and delivered quietly while present.
- **Linked machines**: their ships get no away note, bearings line or `delivery` field; their captain mail is still routed by the door here.

## Changes by file

- `src/away.rs`: `Settings`, `route`, per-ship and overflow budgets, the loud-request budget, `away_digest` and `captain_told`, the builder, `catch_up` in `back`, `every` in `parse`, the bearings line, the send preview.
- `src/door.rs`: route mail through `away::route`; baseline, build and emit digests; `digest:` refs in `delivered`; the `digests` manifest field; the `digest` request; `again` for a repeat while present.
- `src/mail.rs`: the `delivery` preview and note in the captain branch of `mail.send`.
- `src/cli.rs`: the brief sentence.
- `plugins/telegram/door.py`, `manifest.json`: render `digest` events, quiet requests and "(again)"; unknown-card attachments as quiet messages; high-urgency attachments as messages; `/digest`; `"digests": true`.

## Test plan

In isolated homes, through commands and durable state.

1. `src/away.rs` unit tests: normalization; repeat keys (flip-flop, generic bodies from different ships, long bodies across ships); digest bounds and collapse.
2. `tests/door.rs`, with the fake adapter:
   - requests go at once and notices are digested; a door without the flag still gets requests at once;
   - an urgent digest is loud and due early;
   - an over-limit item is not also broken through after a restart;
   - the first digest excludes pre-away mail, and a second away period does not repeat mail delivered while present;
   - one unreadable home does not stop digests;
   - a pending digest is resent after a restart and acknowledged once; a failed catch-up is resent;
   - desk copies are deduplicated;
   - a repeat while present is delivered quietly marked again.
3. `tests/mail.rs`: the digest and repeat notes while away; none when present.
4. `tests/telegram_door.py`: a digest is one quiet `sendMessage` (loud when urgent); an unknown-card attachment is rendered as a quiet message; a high-urgency attachment is a message.
5. `tests/context.rs`: the bearings line while away.

## Decisions

1. Away is declared only; quiet mode is deferred.
2. Requests always go at once; after 3 an hour from one ship, further ones go at once but silently.
3. The liaison's return summary is requested only when the catch-up overflowed.
4. Repeat suppression only while away; when present, a repeat is delivered quietly marked "(again)".
5. Paraphrases are not caught.
6. Defaults: 60 minutes, silent, 20 lines; urgent digests loud and early. All in `away::Settings`.
