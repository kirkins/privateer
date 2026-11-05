"""Operator-only runtime-created exact Codex session (ACT2, Linux).

The fresh-launch reservation in docs/design/codex-attended-mail.md: native
`thread/start` on the peer-verified shared daemon, the returned UUID saved
durably BEFORE anything else, then native `thread/name/set`, which on 0.166
materializes the zero-turn legacy rollout (without it `codex resume <UUID>`
refuses with "no rollout found"). Finally ONLY that UUID is read back to
verify a durable, zero-turn, attachable thread. The Rust control then sets
first_mate.codex_session and relaunches `codex resume <UUID>`; only the
trusted resume callback can bind it. No turn, hook, model call, trust,
history-mode or config change; never a cwd/newest lookup; no retry loop.
"""
import json
import os
from pathlib import Path
import stat
import sys
import time
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from codex_binding import atomic, effort_word, locked
from codex_receipts import encode, exact_uuid, open_rollout
from codex_socket import Daemon, LIMIT, load, process_identity, safe

MAX_KEPT = 64  # Orphaned/failed/assigned reservations kept; past it, refuse.
SENT = []  # Set just before thread/start: after it, a native thread may exist.


class Reserver(Daemon):
    """The only mutating daemon client, confined to this operator control."""
    METHODS = Daemon.METHODS | {"thread/start", "thread/name/set"}


def now_ms():
    return time.time_ns() // 1_000_000


def require(condition, message):
    if not condition:
        raise ValueError(message)


def alive(owner):
    try:
        return process_identity(owner["pid"]) == owner["process_identity"]
    except (OSError, ValueError, KeyError, TypeError):
        return False


def read(path):
    if not path.exists() and not path.is_symlink():
        return None
    safe(path, private=True)
    return load(path)


def update(path, generation, **fields):
    """Commit one step for the reservation this process owns, or refuse."""
    with locked(path.parent):
        record = read(path)
        require(isinstance(record, dict) and record.get("generation") == generation
                and record.get("pid") == os.getpid(), "reservation superseded")
        record.update(fields)
        atomic(path, record)
        return record


def claim(path, request):
    """Step 1, under the project flock: one owner; earlier facts are kept."""
    with locked(path.parent):
        old = read(path)
        record = {"version": 1, "generation": str(uuid.uuid4()), "state": "reserving",
                  "pid": os.getpid(), "process_identity": process_identity(os.getpid()),
                  "started_ms": now_ms(), "ship": request["ship"], "cwd": request["cwd"],
                  "codex_home": request["codex_home"], "model": request["model"],
                  "effort": request.get("effort"), "kept": []}
        if old is not None:
            require(isinstance(old, dict) and old.get("version") == 1 and isinstance(old.get("kept"), list)
                    and all(old.get(k) == record[k] for k in ("ship", "cwd", "codex_home")),
                    "reservation record identity mismatch")
            require(old["state"] not in ("reserving", "reserved") or not alive(old),
                    "another Codex session reservation is in progress")
            record["kept"] = old["kept"]
            entry = {k: old.get(k) for k in ("generation", "state", "session", "started_ms", "error")}
            if old["state"] in ("reserved", "verified") and old.get("session") is not None:
                # A UUID was saved: continue with that EXACT UUID (verification
                # below), never create another one. This also covers a
                # same-commit retry after Rust committed it but before the
                # assigned marker was written.
                record.update(session=old["session"], state="reserved", daemon=old.get("daemon"),
                              adopted=old["generation"], start=old.get("start"))
            elif old["state"] != "unsent":
                # Lost create response (UUID unknown), failed, or already
                # assigned: keep the fact; never search for the thread by cwd.
                record["kept"] = [*record["kept"], {**entry, "kept_ms": now_ms()}]
                require(len(record["kept"]) <= MAX_KEPT, "reservation history full; operator cleanup required")
        atomic(path, record)
        return record


def assign(path, request):
    """Mark a verified reservation as committed by Rust; never mark another.

    Rust calls this only after its writer transaction set
    first_mate.codex_session. Repeating it for the same generation/session is
    idempotent, which keeps the crash-after-commit retry on that exact UUID.
    """
    with locked(path.parent):
        record = read(path)
        require(isinstance(record, dict) and record.get("version") == 1,
                "reservation record identity mismatch")
        require(record.get("generation") == request.get("generation"), "reservation generation changed")
        require(record.get("session") == request.get("session"), "reservation session changed")
        exact_uuid(record["session"])
        require(record.get("state") in ("verified", "assigned"), "reservation is not verified")
        if record.get("state") == "assigned":
            return {"assigned": True, "generation": record["generation"], "session": record["session"]}
        record.update(state="assigned", assigned_ms=now_ms())
        atomic(path, record)
        return {"assigned": True, "generation": record["generation"], "session": record["session"]}


def create(path, request):
    record = claim(path, request)
    generation = record["generation"]
    if record.get("session") is None:
        with Reserver(request["codex_home"], timeout=10) as daemon:
            params = {"cwd": request["cwd"], "historyMode": "legacy", "ephemeral": False}
            if request["model"] is not None:
                params["model"] = request["model"]
            if request.get("effort") is not None:
                # No effort parameter on thread/start; the config key is it.
                params["config"] = {"model_reasoning_effort": request["effort"]}
            SENT.append(generation)
            result = daemon.call("thread/start", params)
            thread = result["thread"]
            # Durable before any validation, attach or callback can happen.
            record = update(path, generation, state="reserved", session=exact_uuid(thread["id"]),
                            reserved_ms=now_ms(), daemon={"pid": daemon.pid, "identity": daemon.identity},
                            start={key: result.get(key) for key in ("model", "modelProvider", "reasoningEffort")})
            require(thread.get("sessionId") == thread["id"] and thread.get("historyMode") == "legacy"
                    and thread.get("ephemeral") is False and thread.get("cwd") == request["cwd"]
                    and thread.get("turns") == []
                    and (request["model"] is None or result.get("model") == request["model"])
                    and (request.get("effort") is None or result.get("reasoningEffort") == request["effort"]),
                    "native thread/start did not return a durable legacy zero-turn thread")
    session = record["session"]
    with Reserver(request["codex_home"], timeout=10) as daemon:
        require(record.get("daemon") == {"pid": daemon.pid, "identity": daemon.identity},
                "Codex daemon replaced since thread/start; reservation left for operator review")
        daemon.call("thread/name/set", {"threadId": session, "name": request["name"]})
        thread = daemon.call("thread/read", {"threadId": session})["thread"]
    require(thread.get("id") == session and thread.get("cwd") == request["cwd"]
            and thread.get("historyMode") == "legacy" and thread.get("ephemeral") is False
            and thread.get("turns") == [] and thread.get("status") == {"type": "idle"}
            and isinstance(thread.get("path"), str), "reserved thread is not a durable idle zero-turn thread")
    stream, info, meta = open_rollout(thread["path"], session, request["cwd"])
    with stream:
        stream.seek(0)
        data = stream.read(LIMIT + 1)
    # Zero-turn: the materialized rollout holds session metadata only.
    lines = data.splitlines()
    require(len(data) <= LIMIT and data.endswith(b"\n")
            and all(json.loads(line).get("type") == "session_meta" for line in lines),
            "reserved rollout is not zero-turn session metadata")
    record = update(path, generation, state="verified", verified_ms=now_ms(),
                    rollout={"path": thread["path"], "device": info.st_dev, "inode": info.st_ino,
                             "size": info.st_size, "meta_hash": meta})
    return {"session": session, "state": "verified", "generation": generation,
            "adopted": record.get("adopted"), "daemon": record["daemon"], "rollout": record["rollout"],
            "settings": {"requested_model": request["model"], "requested_effort": request.get("effort"),
                         **(record.get("start") or {})},
            "kept": len(record["kept"])}


def main(argv):
    if len(argv) != 3 or argv[1] not in ("create", "assign"):
        print("usage: codex_reserve.py create|assign <reservation.json>", file=sys.stderr)
        return 2
    path = Path(argv[2])
    generation = None
    try:
        request = json.loads(sys.stdin.read(LIMIT))
        cwd = Path.cwd().resolve()
        require(isinstance(request, dict) and request.get("cwd") == str(cwd)
                and path == cwd / ".codex/privateer-attended/reservation.json", "reservation path/cwd mismatch")
        if argv[1] == "create":
            require(isinstance(request.get("ship"), str) and isinstance(request.get("name"), str)
                    and (request.get("model") is None or isinstance(request["model"], str))
                    and (request.get("effort") is None or effort_word(request["effort"]))
                    and (request.get("current") is None or exact_uuid(request["current"])),
                    "invalid reservation request")
            home = os.environ.get("CODEX_HOME") or str(Path(os.environ["HOME"]) / ".codex")
            request["codex_home"] = str(Path(home).resolve(strict=True))
            for directory in (cwd / ".codex", path.parent):
                directory.mkdir(mode=0o700, exist_ok=True)
                safe(directory, stat.S_ISDIR)
            print(encode(create(path, request)))
        else:
            require(isinstance(request.get("generation"), str) and isinstance(request.get("session"), str),
                    "invalid reservation assignment")
            print(encode(assign(path, request)))
        return 0
    except Exception as error:
        message = str(error)[:512] or type(error).__name__
        if argv[1] == "create":
            try:
                record = read(path)
                generation = record.get("generation") if isinstance(record, dict) else None
                if generation and record.get("pid") == os.getpid():
                    if record.get("state") == "reserved":
                        # The UUID stays recorded; the next create keeps it, not reuses it.
                        update(path, generation, state="failed", error=message)
                    elif record.get("state") == "reserving" and generation not in SENT:
                        # Nothing reached the daemon: no native fact to keep.
                        update(path, generation, state="unsent", error=message)
            except Exception:
                pass
        print(encode({"error": message}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
