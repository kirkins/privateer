"""Bounded voice-request history. Exact mail links only, never text guesses.

The door supplies persistence and holds its lock around these operations.
Expired/closed entries remain visible (until the oldest-first count cap), but
no longer match results. Answers stay linkable until the same seven-day limit.
"""

import sys

sys.dont_write_bytecode = True  # a release holds only its own files
import pacing

FILE = "requests.json"
LIMIT = 200
TTL_MS = 7 * 24 * 60 * 60 * 1000
LABEL_LIMIT = 60
STATES = ("waiting", "answered", "expired", "closed")
ENDED = ("withdrawn", "dismissed", "task_done", "task_cancelled")


def load(value, now: int) -> list:
    records = {}
    for item in value if isinstance(value, list) else []:
        if not isinstance(item, dict):
            continue
        if not all(isinstance(item.get(k), str) and item[k]
                   for k in ("id", "to", "label")):
            continue
        sent = item.get("sent_ms")
        if not isinstance(sent, int) or isinstance(sent, bool) or sent < 0 \
                or item.get("state") not in STATES:
            continue
        records[item["id"]] = {"id": item["id"], "to": item["to"],
                               "label": item["label"][:LABEL_LIMIT], "sent_ms": sent,
                               "state": item["state"]}
    kept = sorted(records.values(), key=lambda r: r["sent_ms"])[-LIMIT:]
    expire(kept, now)
    return kept


def expire(records: list, now: int) -> bool:
    changed = False
    for record in records:
        if record["state"] == "waiting" and now >= record["sent_ms"] + TTL_MS:
            record["state"] = "expired"
            changed = True
    return changed


def sent(records: list, ident: str, to: str, text: str, now: int) -> None:
    if any(r["id"] == ident for r in records):
        return  # an idempotent send does not reset its answer or age
    label = pacing.subject({"mail": {"body": text}})[:LABEL_LIMIT]
    records.append({"id": ident, "to": to, "label": label, "sent_ms": now,
                    "state": "waiting"})
    records.sort(key=lambda r: r["sent_ms"])
    del records[:-LIMIT]


def match(records: list, mail: dict, now: int) -> dict | None:
    # Privateer's serialized mail has correlation and because_of, not a
    # separate thread field. Never inspect payload, body or sender to guess.
    for field in ("correlation", "because_of"):
        ident = mail.get(field)
        if not isinstance(ident, str) or not ident:
            continue
        for record in records:
            if record["id"] == ident and record["state"] in ("waiting", "answered") \
                    and now < record["sent_ms"] + TTL_MS:
                return record
    return None


def observe(records: list, event: dict, now: int) -> bool:
    """Handled correlated mail (spoken OR counted) settles a request durably.

    A because_of link is a follow-up, not evidence of an answer.
    A snapshot of the original request can close it, if ever supplied by
    the runtime. A reply's own state is NOT the original request's state.
    """
    mail = pacing.mail_of(event)
    # The runtime currently streams captain mail, not request-lifecycle
    # snapshots. Keep this closure path for a future lifecycle feed; until
    # then a withdrawal elsewhere is invisible and the request expires.
    for record in records:
        if mail.get("id") == record["id"] and event.get("ship") == record["to"] \
                and mail.get("state") in ENDED and record["state"] != "closed":
            record["state"] = "closed"
            return True
    record = match(records, mail, now)
    if record is not None and mail.get("correlation") == record["id"] \
            and record["state"] == "waiting":
        record["state"] = "answered"
        return True
    return False


def answer_line(record: dict, sender: str, content: str, follow_up: bool = False) -> str:
    verb = ("has a follow-up on" if follow_up else
            "sent another reply to" if record["state"] == "answered" else "answered")
    return f"{sender[:60]} {verb} your question about {record['label']}: {content}"
