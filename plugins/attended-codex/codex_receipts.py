"""Dormant B2 receipt ledger. No transport, mailbox ack, reporting or activation.

Callers supply read-only evidence from the verified exact-thread daemon and the
runtime's mailbox cursor. This module never interprets CLI prose or absence as
non-admission. B3 must separately authorize every spawn under the binding lock.
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import uuid

sys.dont_write_bytecode = True
from codex_binding import atomic, locked
from codex_socket import LIMIT, safe

MAX_LEDGER = 8 * LIMIT
MAX_SESSIONS = 128
MAX_NONCE_DIGESTS = 256
NONCE_FILTER_BITS = 65536
NONCE_FILTER_MAX_FILL = 0.25
FINAL = {"presented", "acked", "rejected", "abandoned", "operator_presented"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def nonce_bits(nonce_hash):
    if (not isinstance(nonce_hash, str) or len(nonce_hash) != 64
            or any(c not in "0123456789abcdef" for c in nonce_hash)):
        raise ValueError("invalid receipt nonce digest")
    return [int(nonce_hash[i:i + 8], 16) % NONCE_FILTER_BITS for i in range(0, 32, 8)]


def nonce_filter(state):
    value = state.get("nonce_filter", "0" * (NONCE_FILTER_BITS // 4))
    if (not isinstance(value, str) or len(value) != NONCE_FILTER_BITS // 4
            or any(c not in "0123456789abcdef" for c in value)):
        raise ValueError("invalid receipt nonce filter")
    return int(value, 16)


def remember_nonces(state, additions):
    # A non-deleting Bloom filter bounds storage without forgetting reuse.
    # False positives refuse safely; saturation is backpressure, not eviction.
    hashes = list(dict.fromkeys([*state.get("nonce_digests", []), *additions]))
    if not hashes and "nonce_filter" not in state:
        return
    bits = nonce_filter(state)
    for nonce_hash in hashes:
        for bit in nonce_bits(nonce_hash):
            bits |= 1 << bit
    state["nonce_filter"] = format(bits, f"0{NONCE_FILTER_BITS // 4}x")
    state["nonce_digests"] = hashes[-MAX_NONCE_DIGESTS:]


class NonceFilterClosed(ValueError):
    """Capacity/collision backpressure, not positive proof of nonce reuse."""
    def __init__(self, kind, session, fill):
        self.diagnostic = {"kind": kind, "session": session, "fill_ratio": fill,
                           "filter_bits": NONCE_FILTER_BITS, "max_fill_ratio": NONCE_FILTER_MAX_FILL,
                           "next": "Pause offers; do not resample nonces or retry on a timer. Operator capacity review required."}
        super().__init__(encode(self.diagnostic))


def seen_nonce(state, nonce_hash):
    # Compatibility helper for offline inspection; a positive filter test is
    # only possible reuse, NEVER the exact-reuse diagnostic used by prepare.
    return (nonce_hash in state.get("nonce_digests", [])
            or nonce_hash in state.get("rejected_nonce_digests", [])
            or ("nonce_filter" in state and all(nonce_filter(state) & (1 << bit)
                                               for bit in nonce_bits(nonce_hash))))


def check_nonce(sessions, nonce, target_session):
    nonce_hash = digest(nonce.encode())
    for state in sessions.values():
        if (nonce_hash in state.get("nonce_digests", [])
                or nonce_hash in state.get("rejected_nonce_digests", [])
                or any(nonce == offer.get("nonce")
                       for offer in [state.get("intent", {}), *state.get("history", [])])):
            raise ValueError("receipt nonce reused")
    for session, state in sessions.items():
        bits = nonce_filter(state)
        fill = bin(bits).count("1") / NONCE_FILTER_BITS
        if session == target_session and fill >= NONCE_FILTER_MAX_FILL:
            raise NonceFilterClosed("nonce_filter_saturated", session, fill)
        if all(bits & (1 << bit) for bit in nonce_bits(nonce_hash)):
            raise NonceFilterClosed("nonce_filter_possible_reuse", session, fill)


def exact_uuid(value):
    if not isinstance(value, str) or str(uuid.UUID(value)) != value:
        raise ValueError("receipt requires canonical exact UUID")
    return value


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def marker(home, ship, session, nonce, after_seq, through_seq):
    return {"type": "privateer-mail", "version": 1, "home": home, "ship": ship,
            "session": exact_uuid(session), "notification_id": exact_uuid(nonce),
            "after_seq": after_seq, "through_seq": through_seq}


def open_rollout(path, session, cwd):
    """No symlink, ownership or file-identity substitution between stat and open."""
    path = Path(path)
    if not path.is_absolute() or path.resolve() != path:
        raise ValueError("unsafe materialized receipt path")
    info = safe(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    stream = os.fdopen(fd, "rb")
    try:
        current = os.fstat(fd)
        if not stat.S_ISREG(current.st_mode) or (info.st_dev, info.st_ino) != (current.st_dev, current.st_ino):
            raise ValueError("receipt rollout replaced during open")
        line = stream.readline(LIMIT + 1)
        meta = json.loads(line)
        payload = meta.get("payload", {})
        if (len(line) > LIMIT or not line.endswith(b"\n") or meta.get("type") != "session_meta"
                or payload.get("id") != session or payload.get("cwd") != cwd
                or payload.get("history_mode", "legacy") != "legacy"):
            raise ValueError("materialized receipt session metadata mismatch")
        return stream, current, digest(line)
    except BaseException:
        stream.close()
        raise


def checkpoint(stream, offset):
    # Pin the committed boundary as well as dev/inode. Detect truncate/regrow at
    # that boundary; native rollouts must remain append-only, not mutable logs.
    stream.seek(max(0, offset - 4096))
    return digest(stream.read(min(offset, 4096)))


def open_turn_at(stream, offset):
    """Bounded initial lifecycle lookup, NOT a receipt tail scan.

    Receipts still scan forward from their durable anchor. To seed the open-turn
    state at that boundary, walk backwards to the nearest native lifecycle event.
    If it is beyond 1 MiB, refuse rather than guess idle or ignore a terminal.
    """
    position, pending, consumed = offset, b"", 0
    while position and consumed < LIMIT:
        size = min(position, 65536, LIMIT - consumed)
        position -= size
        stream.seek(position)
        pending = stream.read(size) + pending
        consumed += size
        lines = pending.split(b"\n")
        pending = lines.pop(0) if position else b""
        for line in reversed(lines):
            if not line:
                continue
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ValueError("invalid native anchor record")
            if value.get("type") != "event_msg":
                continue
            event = value.get("payload")
            if not isinstance(event, dict):
                raise ValueError("invalid native anchor event")
            kind = event.get("type")
            if kind in ("task_started", "task_complete", "turn_aborted"):
                turn = event.get("turn_id")
                if not isinstance(turn, str) or not turn:
                    raise ValueError("native anchor lifecycle lacks turn ID")
                attribution = event.get("turn_attribution")
                trigger = attribution.get("turn_trigger") if isinstance(attribution, dict) else None
                return (turn, trigger) if kind == "task_started" else (None, None)
            if kind in ("context_compacted", "thread_rolled_back", "turn_started", "turn_complete"):
                raise ValueError("native anchor lifecycle/schema needs reconciliation")
    if position:
        raise ValueError("native open-turn anchor exceeds lookup budget")
    return None, None


def anchor(binding, cwd):
    if binding.get("history_mode") != "legacy":
        raise ValueError("idle mail requires materialized legacy history; paginated/unknown installs get no idle mail")
    path, session = binding["transcript"], exact_uuid(binding["session"])
    stream, info, meta = open_rollout(path, session, cwd)
    with stream:
        offset = info.st_size
        if not offset:
            raise ValueError("empty receipt rollout")
        stream.seek(offset - 1)
        if stream.read(1) != b"\n":
            raise ValueError("receipt anchor has an incomplete native record")
        if binding.get("transcript_identity") != {"device": info.st_dev, "inode": info.st_ino}:
            raise ValueError("binding rollout identity changed")
        turn, trigger = open_turn_at(stream, offset)
        return {"path": path, "device": info.st_dev, "inode": info.st_ino,
                "offset": offset, "meta_hash": meta, "boundary_hash": checkpoint(stream, offset),
                "open_turn": turn, "open_turn_trigger": trigger}


class NativeInputUnqualified(ValueError):
    def __init__(self):
        super().__init__("native text-only user_message schema unqualified; idle mail paused")


def native_client(event):
    """Observed 0.166.0 text-only queue input, B4 loopback fixture.

    No turn_id is persisted on this event: association comes ONLY from the
    anchored lifecycle. Refuse aliases/multimedia/unknown shape, not guesses.
    """
    arrays = {"images", "local_images", "audio", "local_audio", "text_elements"}
    if (set(event) != {"type", "client_id", "message", *arrays}
            or event.get("type") != "user_message" or not isinstance(event.get("message"), str)
            or any(event.get(key) != [] for key in arrays)):
        raise NativeInputUnqualified()
    try:
        return exact_uuid(event.get("client_id"))
    except ValueError:
        raise NativeInputUnqualified() from None


def scan(intent, session, cwd):
    """One <=1 MiB forward batch, complete-record checkpoints, no last-N tail.

    task_started/task_complete are the native v1 rollout spellings on 0.166.
    A user_message has no turn_id; associate it only with the unambiguous open
    queue-triggered task_started in this same anchored stream. Queue and
    promoted client IDs are separate facts; equality is unqualified.
    Assistant/response_item text and queue removal are never presentation evidence.
    """
    a = intent["scan"]
    stream, info, meta = open_rollout(a["path"], session, cwd)
    with stream:
        if ((info.st_dev, info.st_ino) != (a["device"], a["inode"])
                or info.st_size < a["offset"] or meta != a["meta_hash"]
                or checkpoint(stream, a["offset"]) != a["boundary_hash"]):
            raise ValueError("receipt anchor lost: replacement, truncation or rewrite")
        # Older checkpoints lack trigger provenance. Recover BOTH facts at
        # the validated scan boundary, never infer queue origin from the body.
        if "open_turn" not in intent or "open_turn_trigger" not in intent:
            intent["open_turn"], intent["open_turn_trigger"] = open_turn_at(stream, a["offset"])
        stream.seek(a["offset"])
        data = stream.read(LIMIT)
        complete = data.rfind(b"\n") + 1
        if not complete and len(data) == LIMIT:
            raise ValueError("native receipt record exceeds scan budget")
        for line in data[:complete].splitlines():
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ValueError("invalid native receipt record")
            if value.get("type") != "event_msg":
                continue
            event = value.get("payload")
            if not isinstance(event, dict):
                raise ValueError("invalid native receipt event")
            kind = event.get("type")
            if kind == "task_started":
                turn = event.get("turn_id")
                if not isinstance(turn, str) or not turn or intent.get("open_turn"):
                    raise ValueError("ambiguous native turn association")
                intent["open_turn"] = turn
                attribution = event.get("turn_attribution")
                intent["open_turn_trigger"] = (attribution.get("turn_trigger")
                                               if isinstance(attribution, dict) else None)
            elif kind in ("task_complete", "turn_aborted"):
                if event.get("turn_id") != intent.get("open_turn"):
                    raise ValueError("native terminal turn mismatch")
                intent["open_turn"] = None
                intent["open_turn_trigger"] = None
            elif kind == "user_message" and event.get("message") == intent["body"]:
                turn = intent.get("open_turn")
                client = native_client(event)
                if not turn:
                    raise ValueError("native mail input lacks open-turn association")
                if intent.get("open_turn_trigger") != "queue":
                    raise ValueError("native mail input lacks queue-triggered turn association")
                receipt = {"turn_id": turn, "client_id": client}
                if intent.get("receipt") not in (None, receipt):
                    raise ValueError("duplicate native mail promotion")
                intent.update(receipt=receipt, client_id=client, state="presented")
            elif kind in ("context_compacted", "thread_rolled_back", "turn_started", "turn_complete"):
                raise ValueError("receipt history continuity/schema needs reconciliation")
        a["offset"] += complete
        a["boundary_hash"] = checkpoint(stream, a["offset"])
        return a["offset"] == info.st_size


def queued_match(page, intent):
    if (not isinstance(page, dict) or not isinstance(page.get("data"), list)
            or "nextCursor" not in page):
        raise ValueError("unsupported native queue evidence")
    # A bounded page can establish presence, never absence. The B3 observer may
    # enumerate further pages; no caller can turn nextCursor into rejection.
    if len(page["data"]) > 128:
        raise ValueError("native queue evidence exceeds budget")
    matches = []
    for item in page["data"]:
        if not isinstance(item, dict) or not isinstance(item.get("input"), list):
            raise ValueError("invalid native queued submission")
        if item["input"] == [{"type": "text", "text": intent["body"], "text_elements": []}]:
            if not isinstance(item.get("id"), str) or not isinstance(item.get("clientUserMessageId"), str):
                raise ValueError("native queued submission lacks IDs")
            matches.append(item)
    if len(matches) > 1:
        raise ValueError("duplicate queued nonce")
    return matches[0] if matches else None


class Ledger:
    """Bounded project ledger; every mutation reloads and atomically commits.

    Watermarks and nonce digests survive explicit same-UUID restart. Historical
    settled bodies are pruned; operator audits survive. Recent exact nonce
    digests plus a bounded non-deleting filter refuse possible admission-nonce
    reuse, even after eviction from the exact window. Never-spawned rejections
    use a separate counter/recent exact window. Filter capacity/collisions and
    pinned facts apply explicit backpressure.
    The project directory and lock are shared with binding transactions.
    """
    def __init__(self, registry, home, ship):
        self.path = Path(registry).with_name("receipts.json")
        self.home, self.ship = home, ship

    def read(self):
        if not self.path.exists() and not self.path.is_symlink():
            return {"version": 1, "home": self.home, "ship": self.ship, "sessions": {}}
        safe(self.path, private=True)
        with self.path.open("rb") as stream:
            data = stream.read(MAX_LEDGER + 1)
        if len(data) > MAX_LEDGER:
            raise ValueError("receipt ledger exceeds storage bound")
        value = json.loads(data)
        if (not isinstance(value, dict) or value.get("version") != 1
                or value.get("home") != self.home or value.get("ship") != self.ship
                or not isinstance(value.get("sessions"), dict) or len(value["sessions"]) > MAX_SESSIONS):
            raise ValueError("receipt ledger identity/schema mismatch")
        return value

    @staticmethod
    def compact(value):
        for state in value["sessions"].values():
            kept, hashes = [], []
            for offer in state.get("history", []):
                settled = (offer["state"] == "rejected"
                           or (offer["state"] == "abandoned"
                               and offer["through_seq"] <= state.get("suppressed_through", 0))
                           or (offer["state"] in FINAL
                               and offer["through_seq"] <= state["accepted_through"]))
                if settled:
                    nonce_hash = digest(exact_uuid(offer["nonce"]).encode())
                    if offer["state"] == "rejected":
                        # Proven exec failure: no native request ever existed.
                        # Do not consume the non-deleting admission filter.
                        state["rejected_count"] = state.get("rejected_count", 0) + 1
                        state["rejected_nonce_digests"] = [
                            *state.get("rejected_nonce_digests", []), nonce_hash][-MAX_NONCE_DIGESTS:]
                    else:
                        hashes.append(nonce_hash)
                    # Retain the exact range and body commitment, also when
                    # migrating older audits that predate these audit fields.
                    if "operator_resolution" in offer:
                        audit = {**offer["operator_resolution"], **{key: offer[key]
                                 for key in ("after_seq", "through_seq", "body_hash")}}
                        state.setdefault("operator_audits", []).append(audit)
                else:
                    kept.append(offer)
            remember_nonces(state, hashes)
            if "history" in state:
                state["history"] = kept

    def transaction(self, change):
        with locked(self.path.parent):
            value = self.read()
            before = json.dumps(value)
            result = change(value)
            self.compact(value)
            # Measure atomic's exact default encoding; don't rewrite/fsync on
            # unchanged polls, but persist checkpoint/ack/diagnostic changes.
            after = json.dumps(value)
            if len(after.encode()) > MAX_LEDGER:
                raise ValueError("receipt ledger full; pinned facts not evicted")
            if after != before:
                atomic(self.path, value)
            return copy.deepcopy(result)

    def prepare(self, record, body, after_seq, through_seq, nonce):
        session = exact_uuid(record["binding"]["session"])
        expected = marker(self.home, self.ship, session, nonce, after_seq, through_seq)
        if (record["home"] != self.home or record["ship"] != self.ship or record["session"] != session
                or not isinstance(after_seq, int) or not isinstance(through_seq, int)
                or not 0 <= after_seq < through_seq or len(body.encode()) > LIMIT // 2
                or body.split("\n", 1)[0] != encode(expected)):
            raise ValueError("receipt intent identity/range/body mismatch")
        def change(value):
            sessions = value["sessions"]
            if any(s.get("intent", {}).get("state") not in FINAL for s in sessions.values() if s.get("intent")):
                raise ValueError("one outstanding durable nonce; reconcile predecessor first")
            if session not in sessions:
                if len(sessions) >= MAX_SESSIONS:
                    raise ValueError("receipt session index full; pinned facts not evicted")
                sessions[session] = {"accepted_through": 0, "acknowledged_through": 0}
            state = sessions[session]
            if after_seq < max(state["accepted_through"], state.get("suppressed_through", 0)):
                raise ValueError("receipt range would replay accepted mail or operator-abandoned offer")
            check_nonce(sessions, nonce, session)
            if state.get("intent"):
                state.setdefault("history", []).append(state["intent"])
            intent = {"state": "prepared", "nonce": nonce, "body": body, "body_hash": digest(body.encode()),
                      "after_seq": after_seq, "through_seq": through_seq, "generation": record["generation"],
                      "callback_generation": record["callback_generation"], "holder": record["holder"],
                      "cwd": record["cwd"], "scan": anchor(record["binding"], record["cwd"])}
            state["intent"] = intent
            return intent
        return self.transaction(change)

    def invoking(self, session, nonce):
        """B3 calls while holding its authorization lock; not nested in this API.

        This standalone dormant API takes the lock itself. The runner must use
        a single combined transaction+spawn, not call this then spawn unlocked.
        """
        def change(value):
            intent = self.intent(value, session, nonce)
            if intent["state"] != "prepared":
                raise ValueError("receipt intent already attempted")
            intent["state"] = "invoking"
            return intent
        return self.transaction(change)

    @staticmethod
    def intent(value, session, nonce):
        intent = value["sessions"][exact_uuid(session)]["intent"]
        if intent["nonce"] != exact_uuid(nonce) or digest(intent["body"].encode()) != intent["body_hash"]:
            raise ValueError("receipt intent nonce/body mismatch")
        return intent

    def reconcile(self, session, nonce, queue, acknowledged_through=0, pre_request_failure=False):
        """Lost response is evidence-only. No CLI exit/status/time heuristic.

        pre_request_failure may ONLY mean Popen exec failed before a child
        existed/request could be sent. Not a timeout, stderr or CLI nonzero.
        """
        def change(value):
            state = value["sessions"][exact_uuid(session)]
            intent = self.intent(value, session, nonce)
            if type(acknowledged_through) is not int or acknowledged_through < 0:
                raise ValueError("invalid mailbox acknowledgement evidence")
            state["acknowledged_through"] = max(state["acknowledged_through"], acknowledged_through)
            if acknowledged_through >= intent["through_seq"] and intent["state"] != "presented":
                intent["state"] = "acked"
            elif intent["state"] not in FINAL:
                candidate = copy.deepcopy(intent)
                try:
                    # B4 stored queue IDs in client_id too. Preserve that fact
                    # separately before scan records the native promotion ID.
                    if candidate.get("queue_id") and "queue_client_id" not in candidate:
                        candidate["queue_client_id"] = exact_uuid(candidate.pop("client_id"))
                    caught_up = scan(candidate, session, candidate["cwd"])
                    match = queued_match(queue, candidate)
                    if match:
                        client = exact_uuid(match["clientUserMessageId"])
                        if candidate.get("queue_client_id") not in (None, client):
                            raise ValueError("queued mail client ID changed")
                        candidate.update(queue_client_id=client, queue_id=match["id"])
                        if candidate["state"] != "presented":
                            candidate["state"] = "queued"
                    elif pre_request_failure and candidate["state"] in ("prepared", "invoking"):
                        candidate["state"] = "rejected"
                    elif candidate["state"] not in ("prepared", "presented"):
                        candidate["state"] = "uncertain"
                    candidate["scan_caught_up"] = caught_up
                    candidate.pop("pause_reason", None)
                    intent.clear()
                    intent.update(candidate)
                except (OSError, ValueError, KeyError, TypeError) as error:
                    # Commit only the last good checkpoint; never reset an
                    # anchor or turn missing evidence into rejection.
                    if intent["state"] != "prepared":
                        intent["state"] = "uncertain"
                    intent["pause_reason"] = (str(error) if isinstance(error, NativeInputUnqualified)
                                              else "native receipt evidence unavailable or discontinuous")
            if intent["state"] in ("presented", "acked", "operator_presented"):
                intent.pop("pause_reason", None)
                state["accepted_through"] = max(state["accepted_through"], intent["through_seq"])
            diagnostic = None
            if intent["state"] in ("invoking", "uncertain"):
                # A single bounded, stable-ID diagnostic is available NOW, not
                # after a timer. B3 must surface/coalesce it by ID on every
                # observation until positive evidence or operator resolution.
                diagnostic = {"id": "codex-uncertain-" + nonce, "kind": "admission_uncertain",
                              "session": session, "notification_id": nonce,
                              "after_seq": intent["after_seq"], "through_seq": intent["through_seq"],
                              "next": "Reconcile exact native evidence or covering mailbox ack; operator may resolve. Never replay on a timer."}
            return {"state": intent["state"], "accepted_through": state["accepted_through"],
                    "acknowledged_through": state["acknowledged_through"],
                    "paused": intent["state"] in ("invoking", "uncertain") or "pause_reason" in intent,
                    "diagnostic": diagnostic, "evidence": copy.deepcopy(intent)}
        return self.transaction(change)

    def resolve_operator(self, session, nonce, operation, reason, at_ms, *, caller_role):
        """Dormant reducer for B3's OPERATOR-ONLY audited resolve control.

        caller_role MUST come from the runtime's authenticated Caller, never
        request JSON, environment or mail. There is no dispatched control yet;
        this guard is not a substitute for B3's native operator-only gate.
        Resolution cannot retract a native queue item/turn or acknowledge mail.
        """
        if caller_role != "operator":
            raise PermissionError("Codex receipt resolution is operator-only")
        if (operation not in ("abandon", "mark_presented") or not isinstance(reason, str)
                or not reason.strip() or len(reason.encode()) > 512
                or type(at_ms) is not int or at_ms < 0):
            raise ValueError("invalid operator receipt resolution")
        def change(value):
            state = value["sessions"][exact_uuid(session)]
            intent = self.intent(value, session, nonce)
            if intent["state"] not in ("uncertain", "invoking", "queued"):
                raise ValueError("receipt intent does not require operator resolution")
            audit = {"id": str(uuid.uuid4()), "actor": "operator", "operation": operation,
                     "reason": reason, "at_ms": at_ms, "session": session, "notification_id": nonce,
                     **{key: intent[key] for key in ("after_seq", "through_seq", "body_hash")}}
            intent["operator_resolution"] = audit
            intent.pop("pause_reason", None)
            if operation == "abandon":
                intent["state"] = "abandoned"
                # Suppress automatic reoffers of this range without inventing
                # acceptance. Mail remains durable/unacked; late admission is
                # possible and the operator is explicitly accepting that risk.
                state["suppressed_through"] = max(state.get("suppressed_through", 0), intent["through_seq"])
            else:
                intent["state"] = "operator_presented"
                state["accepted_through"] = max(state["accepted_through"], intent["through_seq"])
            return {"state": intent["state"], "audit": audit}
        return self.transaction(change)
