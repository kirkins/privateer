"""How much mail the live voice door lets into a session, and in what words.

A voice door must never read a backlog aloud. Mail is backlog when it was
written before the door process started, or before the current session
started and is older than `backlog_age_secs`, or when one ship has more than
`gap_limit` messages queued at this door. Backlog is never spoken item by
item: the captain hears one count line and may ask for a summary, a line per
ship. Live mail goes in at most `max_notices_per_turn` notices per batch; the
rest waits and is offered as "and N more". No notice runs past
`NOTICE_SPOKEN_LIMIT` characters; longer mail is named, not read.

Plain functions over the runtime's mail events; the door holds the state.
"""

import json
import re

DEFAULT_BACKLOG_AGE = 600
DEFAULT_MAX_NOTICES = 3
DEFAULT_GAP_LIMIT = 20
DEFAULT_PACE = 6
NOTICE_SPOKEN_LIMIT = 300
SUMMARY_LINES = 8
SUBJECT_WORDS = 6
SUBJECT_CHARS = 60

SUMMARY_WORDS = frozenset({"summary", "summarise", "summarize", "summaries"})
MORE_WORDS = frozenset({"more"})
# A keyword counts only as a short utterance of its own ("summary please",
# "say more"), not inside a message the captain is dictating.
KEYWORD_UTTERANCE = 4


def mail_of(event: dict) -> dict:
    mail = event.get("mail")
    return mail if isinstance(mail, dict) else {}


def sender(event: dict) -> str:
    """Who wrote it. Mail collected from another machine sits in the
    captain's home here, so the event's ship is not always the writer."""
    return str(mail_of(event).get("from") or event.get("ship") or "a ship")


def created_ms(event: dict) -> int | None:
    value = mail_of(event).get("created_ms")
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def ref_seq(event: dict) -> int | None:
    """The mail's sequence in its home, from the runtime's `SHIP:SEQ` ref."""
    _, _, seq = str(event.get("ref") or "").rpartition(":")
    return int(seq) if seq.isdigit() else None


def mail_key(ship, mail_id) -> str:
    """Mail ids are unique within a ship's home, not across the fleet."""
    return json.dumps([str(ship or ""), str(mail_id or "")])


def event_key(event: dict) -> str:
    return mail_key(event.get("ship"), mail_of(event).get("id"))


def old(event: dict, door_ms: int, session_ms: int | None, age_ms: int, now_ms: int) -> bool:
    """Backlog by age: written before this door started, or before the
    session (now, with none) began and older than the age limit. Mail
    without a creation time is taken as new."""
    created = created_ms(event)
    if created is None:
        return False
    if created < door_ms:
        return True
    start = now_ms if session_ms is None else session_ms
    return created < start and now_ms - created > age_ms


def subject(event: dict) -> str:
    """The first few words of a mail: its report's title, else its body."""
    mail = mail_of(event)
    payload = mail.get("payload") if isinstance(mail.get("payload"), dict) else {}
    report = payload.get("report")
    text = report.get("title") if isinstance(report, dict) and report.get("title") else \
        mail.get("body")
    words = str(text or "").split()
    cut = " ".join(words[:SUBJECT_WORDS])
    if len(cut) > SUBJECT_CHARS:
        cut = cut[:SUBJECT_CHARS - 1].rstrip()
    cut = cut.rstrip(" .,;:!?")
    return cut or "nothing written"


def plural(n: int, word: str) -> str:
    return f"{n} {word}" if n == 1 else f"{n} {word}s"


def count_line(events: list) -> str:
    """The one line the captain hears for a backlog."""
    ships = {sender(e) for e in events}
    return (f"You have {plural(len(events), 'older message')} from "
            f"{plural(len(ships), 'ship')}. Say summary to hear who they are from, or skip.")


def summary_lines(events: list) -> list:
    """At most SUMMARY_LINES lines: a ship's count and its newest subject,
    busiest ship first; past the limit the rest share the last line."""
    by_ship: dict = {}
    add_summary(by_ship, events)
    return format_summary(by_ship)


def add_summary(by_ship: dict, events: list) -> None:
    """Fold a later page into a summary without retaining whole mail bodies."""
    for event in events:
        name = sender(event)
        count, _ = by_ship.get(name, (0, None))
        # Events arrive oldest first per home, so the last one seen is newest.
        by_ship[name] = (count + 1, subject(event))


def format_summary(by_ship: dict) -> list:
    """Render accumulated counts and newest subjects, at most SUMMARY_LINES."""
    ranked = sorted(by_ship.items(), key=lambda item: (-item[1][0], item[0]))
    shown = ranked if len(ranked) <= SUMMARY_LINES else ranked[:SUMMARY_LINES - 1]
    lines = [f"{name}: {plural(count, 'message')}, the newest about {newest}."
             for name, (count, newest) in shown]
    rest = ranked[len(shown):]
    if rest:
        total = sum(count for _, (count, _) in rest)
        lines.append(f"And {plural(len(rest), 'other ship')}: {plural(total, 'message')}.")
    return lines


def spoken_notice(event: dict, full: str, approving: bool) -> str:
    """The notice as spoken: as written when short, else named, not read."""
    if len(full) <= NOTICE_SPOKEN_LIMIT:
        return full
    where = "it needs your approval on your phone" if approving else "it is on your phone"
    return f"{sender(event)[:60]} sent a long message about {subject(event)}; {where}."


def more_line(count: int) -> str:
    return f"And {count} more; say more to hear them."


def keyword(text: str, words: frozenset) -> bool:
    """A short utterance of its own that says one of `words`."""
    said = re.sub(r"[^a-z]+", " ", str(text).lower().replace("'", "").replace("’", "")).split()
    return 0 < len(said) <= KEYWORD_UTTERANCE and bool(words & set(said)) \
        and not {"no", "not", "dont"} & set(said)
