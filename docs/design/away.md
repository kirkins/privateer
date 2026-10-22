# Proposal: away mode

Status (2026-10-22): implemented in `src/away.rs` (`privateer afk` / `back`, [architecture](../architecture.md#away-mode)), with digests in [away digests](away-digest.md); this document is the design rationale and is not kept current.

## The captain's case

The captain talks to the fleet mostly on Telegram, and often from bed or on
the move. Today every message a ship writes to the captain arrives as it is
written, so a night away ends with a long scroll of updates, half of them
superseded by the morning. What the captain wants instead:

- say "I'm away" once, and have the fleet hold its messages;
- still be reached for a real emergency, marked so it stands out;
- on the first message back, get a short summary of what happened, what is
  done and what needs them, rather than the raw backlog.

## What First Mate's `/afk` taught

First Mate's `/afk` is worth borrowing from, and also worth simplifying:

- **Entry is the act.** Typing `/afk` records the away posture at once, with
  no confirmation step, because the captain may not look again.
- **The captain's words are the mandate.** `/afk merge #117 if CI goes green`
  is recorded verbatim, and the away session acts on it by judgment when the
  moment comes; anything unsure waits for the return.
- **Away never changes authority.** It changes how the captain is told, not
  who approves what, and destructive, irreversible or security-sensitive
  actions are never pre-authorized by away words.
- **The first ordinary message is the return**, ambiguity counts as a return,
  and a return brief is built from durable records.

First Mate had no phone channel, so it could only hold for the return. The
Telegram door is that channel, which makes emergencies reachable and the
return conversational.

## Proposal

### Going away

`/afk [words]` on Telegram (also `/afk` in a first mate's Pi session and
`privateer afk [words]`) records the captain as away, fleet-wide, in the
fleet registry: when it began, the words, and an optional expected return
(`/afk until 8am ...`). The door answers in one line ("Away from 23:40. I'll
hold everything except emergencies 🚨.") and, when there were words, restates
them in a plain sentence so a misreading can be corrected with another `/afk`.

The liaison receives the words as a message from the captain. They are its
standing instruction until the return: it acts on them within its existing
authority, and a decision they answer can be recorded on the asking ship's
request with `captain.relay`, citing that message (#120). Everything else
that needs the captain waits.

Every ship's bearings say "Captain away since 23:40 (back about 8:00): only
emergencies reach them", so first mates record waits and keep working instead
of stopping on a question.

### While away

The door holds the captain's mail: it stops pushing it to Telegram. Nothing
is lost, because the mail stays in each ship's home and the door's per-ship
cursor simply does not advance.

**Emergencies break through.** A message to the captain with `"urgency":
"high"` is delivered at once, prefixed 🚨 and naming the ship. To keep that
meaningful:

- The brief defines high urgency for the captain narrowly: production or
  data at risk, a security problem, money being spent unexpectedly, or a
  deadline within hours. Everything else is normal, whether or not the
  captain is away.
- At most a few breakthroughs per hour reach the phone (provisionally three);
  more are held and counted, so a flapping ship cannot ring all night.
- Runtime crew outcomes never break through; they go to first mates, not the
  captain.

Peeking does not end the away period: `/status`, `/waiting`, `/ships` and
`/held` (the held messages, listed) answer as usual and leave the captain
away.

### Coming back

The first ordinary message from the captain ends the away period, and so does
`/back`. Ambiguity counts as a return. The door then:

1. **Answers at once from records, with no model call:** "🌅 Welcome back.
   Away 7 h 12 m: 23 messages from 4 ships, 2 decisions waiting on you, 1
   emergency sent." The counts come from the held mail and `captain.waiting`.
2. **Asks the liaison for the summary.** It sends the liaison the held
   messages (bounded, oldest first, ids kept) and the captain's returning
   message. The liaison replies to the captain in plain words, in this order:
   what needs them (decisions first, each with its ship), what finished, what
   is still in progress, and then an answer to the message the captain came
   back with. Superseded updates collapse into their outcome.
3. **Marks the held mail delivered** once the summary has gone out, so it is
   not replayed. `/held` still lists anything the captain wants verbatim.

If the liaison cannot answer (its route is backing off, or it fails), the
records-only digest stands on its own and the held messages are offered with
`/held`.

### Authority

Away mode changes delivery, never authority. The liaison acts on the away
words only within its charter; a relayed answer still needs the captain's
actual words (the `/afk` message itself); production deployment approvals are
never relayed; and the away words lapse at the return.

## What it does not do

- No quiet hours on a timer. The captain says when they are away; a
  scheduled version can come later.
- No per-ship rules. One fleet-wide posture keeps it predictable.
- Nothing changes for a captain at a terminal: away mode is about what reaches
  the phone.

## Order of work

1. The away record, `/afk` and the return in the door, holding and
   breakthrough with 🚨 and its rate limit, `/held`, and the records-only
   return digest.
2. The liaison's return summary, and the away words delivered to the liaison.
3. The bearings line and the brief's definition of high urgency for the
   captain.
4. `/afk` in Pi sessions and `privateer afk|back` on the command line.

## Open questions

- Should breakthroughs be limited per hour, per night, or not at all?
  Recommended: three per hour, then held with a count, revisited after real
  nights.
- Should the return summary come from the liaison (a model turn, best prose)
  or from the records only (instant, no cost)? Recommended: both, the digest
  at once and the liaison's summary right after.
- Should `/afk` with an expected return send a morning summary even before the
  captain writes? Recommended: no; the captain's first message is the signal,
  so the summary arrives when they are ready to read it.
