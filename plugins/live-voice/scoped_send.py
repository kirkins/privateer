"""A2 send custody: replayable values live only in the owning Session.

The private, atomic ledger is deliberately insufficient to replay a send.
Unknown rows are never evicted to make room. No automatic retry or recovery.
Offline operator disposition requires inspection and an exact metadata digest.
"""
import json
import os
import re
import secrets
import sys
import threading
import uuid
from dataclasses import dataclass, field

sys.dont_write_bytecode = True  # immutable plugin releases, before sibling imports
from scoped_access import DAY_MS, digest, private_read, private_write
import rules
import web

UNKNOWN = ("Outcome unknown; check the conversation through an independently authorized "
           "surface before resending. No automatic replay or restart recovery.")
STATES = frozenset(("unknown", "accepted", "denied", "failed"))
FIELDS = frozenset(("device", "grant", "target", "mail_id", "key_hash", "state", "time_ms"))


def ship_name(value):
    # Never allow a name to become a prompt marker, path, shell or confirmation phrase.
    return isinstance(value, str) and re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}", value) is not None


def exact_readback(draft):
    from door import untrusted
    text = draft.readback()
    framed = untrusted(text)
    # Preserve the existing injection/marker defenses without silently reading
    # different words than the envelope. Unsafe normalization/marker cases must
    # be spoken again, not confirmed against a changed readback.
    content = framed.split("\n", 2)[2].rsplit("\n", 1)[0]
    if content != text:
        raise rules.Refused("Not drafted: those exact words cannot be safely read back; "
                            "say the message again without prompt markers or control text.")
    return framed


@dataclass(eq=False)
class Attempt:
    draft: object
    envelope: dict = field(repr=False)
    key: str = field(default_factory=lambda: secrets.token_urlsafe(32), repr=False)
    first_ms: int | None = None
    first_clock: float | None = None
    last_ms: int | None = None
    last_clock: float | None = None
    ready: bool = False
    busy: bool = False
    retry_blocked: bool = False
    state: str = "draft"
    outcome: str = "Not sent; explicit named-recipient confirmation is required."
    result: dict = field(default_factory=dict)
    confirmation: dict = field(default_factory=dict, repr=False)

    @classmethod
    def prepare(cls, draft):
        args = draft.args()
        rules.check_send(args, None)
        if (set(args) != {"id", "to", "kind", "body"} or args["kind"] != "request"
                or not ship_name(args["to"]) or not 0 < len(args["body"]) <= 4000):
            raise rules.Refused("Only a new request to one named ship is supported.")
        exact_readback(draft)
        return cls(draft, args.copy())

    def unchanged(self):
        return self.draft.target is None and self.draft.args() == self.envelope

    def args(self):
        return dict(self.envelope, idempotency_key=self.key)

    def retryable(self, now_ms, now_clock):
        if self.first_ms is None:
            return True
        valid = (not self.retry_blocked and self.last_ms <= now_ms < self.first_ms + DAY_MS
                 and self.last_clock <= now_clock < self.first_clock + DAY_MS / 1000)
        if valid:
            self.last_ms, self.last_clock = now_ms, now_clock
        else:
            self.retry_blocked = True
        return valid

    def record(self, binding):
        return dict(device=binding.device, grant=binding.grant, target=self.envelope["to"],
                    mail_id=self.envelope["id"], key_hash=digest(self.key), state=self.state,
                    time_ms=self.first_ms)

    def view(self):
        return dict(id=self.envelope["id"], to=self.envelope["to"],
                    readback=self.draft.readback(), state=self.state, outcome=self.outcome)


class Ledger:
    def __init__(self, state):
        self.path = os.path.join(state, "scoped-sends.json")
        self.lock = threading.RLock()
        try:
            self.rows = json.loads(private_read(self.path, 256 * 1024))
        except FileNotFoundError:
            self.rows = []
        if not isinstance(self.rows, list):
            raise ValueError("invalid send metadata")
        seen, counts = set(), {}
        for row in self.rows:
            if (not isinstance(row, dict) or set(row) != FIELDS
                    or any(not isinstance(row[k], str) for k in FIELDS - {"time_ms"})
                    or row["state"] not in STATES or not ship_name(row["target"])
                    or not re.fullmatch(r"voice-[a-f0-9]{24}", row["mail_id"])
                    or not re.fullmatch(r"[a-f0-9]{64}", row["key_hash"])
                    or type(row["time_ms"]) is not int or row["time_ms"] < 0):
                raise ValueError("invalid send metadata")
            for key in ("device", "grant"):
                if str(uuid.UUID(row[key])) != row[key]:
                    raise ValueError("invalid send identity")
            if row["mail_id"] in seen:
                raise ValueError("duplicate send metadata")
            seen.add(row["mail_id"])
            counts[row["device"]] = counts.get(row["device"], 0) + 1
        if any(count > 32 for count in counts.values()):
            raise ValueError("send metadata capacity exceeded")

    def put(self, record, now_ms):
        with self.lock:
            rows = [r for r in self.rows if not (r["state"] != "unknown"
                    and now_ms >= r["time_ms"] + DAY_MS)]
            previous = next((r for r in rows if r["mail_id"] == record["mail_id"]), None)
            if previous is None and sum(
                    r["device"] == record["device"] for r in rows) >= 32:
                raise web.ApiError(409, "send_capacity", "Send metadata full; nothing new was sent.")
            rows = [r for r in rows if r["mail_id"] != record["mail_id"]] + [record.copy()]
            # Swap memory only after durable rename/fsync. Failed post-admission
            # transitions leave the prewritten unknown fact on disk.
            private_write(self.path, json.dumps(rows, separators=(",", ":")))
            self.rows = rows

    def prune(self, now_ms):
        with self.lock:
            rows = [r for r in self.rows if r["state"] == "unknown"
                    or now_ms < r["time_ms"] + DAY_MS]
            if rows != self.rows:
                private_write(self.path, json.dumps(rows, separators=(",", ":")))
                self.rows = rows

    def inspect_unknown(self, device):
        if str(uuid.UUID(device)) != device:
            raise ValueError("invalid device identity")
        with self.lock:
            rows = [r.copy() for r in self.rows if r["device"] == device and r["state"] == "unknown"]
            checked = digest(json.dumps(rows, sort_keys=True, separators=(",", ":")))
            return dict(device=device, unknown=rows, checked=checked)

    def clear_unknown(self, device, checked):
        """Offline only: discard inspected metadata, never replay or claim delivery."""
        with self.lock:
            inspection = self.inspect_unknown(device)
            if not secrets.compare_digest(inspection["checked"], checked):
                raise ValueError("send metadata changed; inspect again")
            rows = [r for r in self.rows if r["device"] != device or r["state"] != "unknown"]
            private_write(self.path, json.dumps(rows, separators=(",", ":")))
            self.rows = rows
            return dict(device=device, cleared=len(inspection["unknown"]), replayed=False)

    def history(self, binding):
        with self.lock:
            return [dict(r, state="unknown", outcome=UNKNOWN) for r in self.rows
                    if r["device"] == binding.device and r["grant"] == binding.grant]


def send_result(raw, binding, door, attempt):
    """Value-free, conservative projection. Never forward a raw send result/mail body."""
    provenance = raw.get("provenance")
    if (not isinstance(provenance, dict) or provenance.get("client") != binding.grant
            or provenance.get("grant") != binding.grant
            or provenance.get("resource") != "door:" + door
            or raw.get("message_id") != attempt.envelope["id"]
            or raw.get("ship") != attempt.envelope["to"]
            or raw.get("delivery_state") not in ("delivered", "queued", "held", "failed", "unknown")
            or not isinstance(provenance.get("result_id"), str)
            or not re.fullmatch(r"[a-fA-F0-9-]{36}", provenance["result_id"])):
        return None
    return dict(mail_id=raw["message_id"], delivery_state=raw["delivery_state"],
                result_id=provenance["result_id"])


def main():
    import argparse
    parser = argparse.ArgumentParser(description="Offline scoped-send metadata disposition; stop the door first.")
    parser.add_argument("--state-dir", required=True, help="absolute private door state directory")
    parser.add_argument("--device", required=True, help="canonical device UUID")
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--show-unknown", action="store_true")
    action.add_argument("--clear-unknown", metavar="CHECKED", help="digest from --show-unknown after independent inspection")
    args = parser.parse_args()
    try:
        ledger = Ledger(args.state_dir)
        result = (ledger.inspect_unknown(args.device) if args.show_unknown
                  else ledger.clear_unknown(args.device, args.clear_unknown))
    except (OSError, ValueError, TypeError):
        parser.exit(1, "Scoped send metadata unavailable or changed; stop the door and inspect again.\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
