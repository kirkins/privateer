# Design: a correction wakes the ship with the open follow-up

Status: approved as written by the captain on 2026-10-18, task `privateer-correction-wake`, and implemented on this branch. Read with [conversation and activation](conversation-and-activation.md) (wakes, batching and the routine filter) and [architecture](../architecture.md) (mail).

## Problem

An information-only notice waits for a ship's next real wake (`kind` notice, `urgency` low: it is `routine` in `src/activation.rs`). That is right for a heads-up and wrong for a correction.

The stow incident, as it happened: fitforge asked this ship a question (`baf32096`); this ship answered it and raised a request of its own (`e376ceec`) because of that answer; the machine was then stowed for an update, and fitforge sent a correction in the thread it had opened (`baf32096`), which it knows. The correction was a low notice, so nothing woke the ship that still had `e376ceec` open. The follow-up was still on the captain's desk with no reason left to be there.

Two rules came out of it. A notice is background, but a notice that corrects the thread of a request this mailbox has open is actionable; and anything about a request belongs in that request's thread, with a way to say which request it grows from.

## Design

**One structured link: `because_of`.** `mail.send` takes `because_of` on a request, naming the request this one grows from. It is the sender's own field, set where the request is raised, and it is not required to exist in the sending home: the parent may live in another home (as `baf32096` did). One level only: the wake travels one level, by construction, not further down a chain. Only a request carries it; a notice or decision does not.

**One definition of "background", in SQL.** `mail::ROUTINE_MAIL` is the single predicate every place that decides this uses, so wake eligibility, the wake page and `mail.pending`'s `actionable` count cannot drift:

```sql
kind='notice' AND urgency='low'
AND NOT (EXISTS(SELECT 1 FROM mail AS follow_up WHERE follow_up.because_of=mail.correlation
  AND follow_up.sender=mail.recipient AND follow_up.state='open'
  AND follow_up.kind IN ('request','decision'))
  OR EXISTS(SELECT 1 FROM mail AS req WHERE req.id=mail.correlation
  AND req.state='open' AND req.kind IN ('request','decision')
  AND (req.sender=mail.recipient OR req.recipient=mail.recipient)))
```

A message is background only if no follow-up **this mailbox sent** is still open in the thread the message carries, and no request correlated to it is still open with this mailbox as its sender or recipient. A low notice correlated to an open request wakes that request's sender or recipient, and a low notice correcting the thread of an open follow-up wakes the sender of that follow-up. The parent's state does not matter for a follow-up: the correction matters while the follow-up it settles is open, and not one moment longer. The old filter was written out in several places (`activation.rs` new_mail, `activation.rs` eligibility, `mail.rs` pending, `host/sleep.rs` alarm, `scheduling/backlog.rs` mail_pending, `permissions.rs` paused); all read the constant now.

**The wake names the follow-up.** `mail::OPEN_FOLLOW_UP` gives the oldest open follow-up the message concerns, and the wake text says so under that message, with what to do about it. A wake that says "a low notice arrived" leaves the ship to re-derive the connection; one that names `e376ceec` lets it withdraw or update it.

**A notice to a peer may name a thread its home never stored.** `mail.send` refused any correlation id it could not find locally, so a ship could not correct a thread that lives in another home — the case the rule exists for. A notice to a peer now carries an unknown correlation (a local typo is still refused); replies and requests still name something the sending home can judge, and the captain's reply over a gateway link still may name a request asked on another machine. `mail.receive` already skipped the check for notices.

**The instructions say so.** The generated `AGENTS.md` and the wake prompt both tell a first mate that anything about a request goes in that request's thread (answer, correction, "no longer needed"), and to set `because_of` when raising a request on behalf of another.

## What is not in it

- No transitive wake, no `because_of` on anything but a request.
- No priority for the correction over other mail: batching, the wake limit and the page budget are unchanged. A correction is ordinary mail that happens to be wake-worthy.
- No new acknowledgement or closure path. Withdrawing the follow-up is `mail.withdraw`, which the incident's replay ends with.
- Old peers keep a `because_of` envelope field they do not read. The wake happens in the home that raised the follow-up, so nothing depends on the other side understanding it.

## Tests

- `tests/activation.rs` `a_correction_in_the_thread_of_an_open_follow_up_wakes_the_ship_that_raised_it`: the stow replay. The machine is held, the question is asked and answered, the follow-up is raised `because_of` it and bound to the stow, and the correction is a low notice. The wake fires, the prompt names `e376ceec`, the ship withdraws it, and a later low notice in the same thread is quiet again.
- `tests/activation.rs` `a_correction_still_waits_for_the_batch_window_and_the_wake_budget`: the batch window and the hourly budget apply to it like any other mail.
- `tests/mail.rs`: the predicate's three cases (an open follow-up of this mailbox makes the notice actionable; a closed one does not; a thread with no follow-up of this mailbox never does), `because_of`'s refusals, the idempotency of the link, and a notice correcting a thread this home never stored.
- `tests/peers.rs` `a_correction_reaches_the_thread_its_sender_home_never_stored`: two homes. The parent request stays in one, the follow-up crosses with its link, and the other home's correction crosses back and is actionable there.