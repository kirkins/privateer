"""Codex attended-mail notification body: the attended/mail.mjs text, no bodies.

One burst, at most six validated headers, rendered as JSON data. The first line
is the exact versioned receipt marker; it correlates, it never authorizes or
acknowledges. Low-only mail never starts a turn.
"""
import json
import sys

COMPACT = (",", ":")  # JSON.stringify's encoding, as attended/mail.mjs writes it.

sys.dont_write_bytecode = True
from codex_receipts import encode, marker


def snapshot(value, ship):
    """Validate mail.pending exactly as attended/mail.mjs does."""
    numbers = ("count", "actionable", "through_seq", "after_seq", "acknowledged_through")
    if (not isinstance(value, dict) or value.get("address") != ship
            or not all(type(value.get(key)) is int and value[key] >= 0 for key in numbers)
            or value["through_seq"] < value["after_seq"] or value["actionable"] > value["count"]
            or not isinstance(value.get("headers"), list)
            or len(value["headers"]) > min(6, value["count"])):
        raise ValueError("invalid mailbox snapshot")
    for header in value["headers"]:
        if (not isinstance(header, dict) or type(header.get("seq")) is not int
                or not value["after_seq"] < header["seq"] <= value["through_seq"]):
            raise ValueError("invalid mailbox snapshot header")
    if value.get("attention") is not None and not isinstance(value["attention"], str):
        raise ValueError("invalid mailbox snapshot attention")
    return value


def actionable(value):
    return value["count"] > 0 and value["actionable"] > 0 and value["through_seq"] > value["after_seq"]


def body(record, nonce, value):
    ship, home = record["ship"], record["home"]
    inbox = [record["control"], "--home", home, "mail.inbox", "--json",
             json.dumps({"address": ship, "after_seq": value["after_seq"], "limit": 40}, separators=COMPACT)]
    head = encode(marker(home, ship, record["session"], nonce, value["after_seq"], value["through_seq"]))
    text = (f"You are the first mate of {ship}. {value['count']} new mailbox arrivals through seq "
            f"{value['through_seq']}. This is a notification, not an acknowledgement. "
            f"Read message bodies with this argv: {json.dumps(inbox, separators=COMPACT)}. "
            "Page the inbox as needed, handle requests and reply with their correlation IDs; "
            "acknowledge only handled mail. A no-action instruction in one message applies only to "
            "that message, not to other mail in this or later wakes. Existing charter and "
            f"production-deployment approval rules still apply. Headers: {json.dumps(value['headers'], separators=COMPACT)}")
    if value.get("attention"):
        text += " " + value["attention"]
    return head + "\n" + text
