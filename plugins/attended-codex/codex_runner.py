"""Dormant queue consumer. Not imported by a launcher; no CLI or enable path.

No native attachment claim. Every invocation reserves a possibly-unattended
turn, including lost responses. Runtime controls/report dispatch are later work.
Lock order: SQLite/presence outside project flock; no RPC/wait under flock.
"""
import copy
import json
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import time

sys.dont_write_bytecode = True
from codex_binding import PATH_ENV, atomic, control, locked, require_fence
from codex_receipts import (FINAL, MAX_LEDGER, Ledger, anchor, checkpoint, digest,
                            encode, exact_uuid, marker, open_rollout, scan)
from codex_socket import Daemon, LIMIT, load, process_identity, safe

K = 3
TURN_BUDGET = 3  # Cumulative per project, never reset by ack/rebind/restart.


def require(condition, message):
    if not condition:
        raise ValueError(message)


def executable(pid):
    path = Path(f"/proc/{pid}/exe")
    info = path.stat()
    return {"path": str(path.readlink()), "device": info.st_dev, "inode": info.st_ino}


def live(fd):
    poll = select.poll()
    poll.register(fd, select.POLLIN)
    return not poll.poll(0)


class Handles:
    """Independently acquired pidfds, not a cached supervisor Boolean."""
    def __init__(self, custody):
        self.fds = []
        try:
            for prefix in ("", "supervisor_"):
                pid = custody[prefix + "pid"]
                require(type(pid) is int and pid > 0, "invalid custody PID")
                fd = os.pidfd_open(pid, 0)
                self.fds.append(fd)
                identity = custody["process_identity" if not prefix else "supervisor_identity"]
                require(process_identity(pid) == identity
                        and executable(pid) == custody[prefix + "executable"]
                        and live(fd), "pane/supervisor identity changed or exited")
        except BaseException:
            self.close()
            raise

    def check(self):
        require(all(live(fd) for fd in self.fds), "pane/supervisor exited")

    def close(self):
        for fd in self.fds:
            os.close(fd)
        self.fds = []


def custody_check(registry, record, expected):
    safe(registry.with_name("custody.json"), private=True)
    current = load(registry.with_name("custody.json"))
    require(current == expected and current["version"] == 1 and current["state"] == "owned",
            "custody replaced or revoked")
    poison_path = registry.with_name("custody-poison.json")
    safe(poison_path, private=True)
    poison = load(poison_path)
    require(poison.get("reason") is None and poison.get("generation") == record["generation"]
            and poison["token"] == current["token"], "custody poisoned")
    exact_uuid(current["token"])
    exact_uuid(current["epoch"])
    for key in ("generation", "session", "holder", "ship", "home", "cwd", "codex_home", "pid", "process_identity"):
        require(current[key] == record[key], "custody/launch context mismatch")
    require(current["executable"]["path"] == record["executable"], "custody executable mismatch")
    return current


def native_check(record, custody):
    native = custody["daemon"]
    pidfile = load(Path(record["codex_home"]) / "app-server-daemon/daemon.pid")
    info = safe(native["socket"], stat.S_ISSOCK, private=True)
    require(pidfile["pid"] == native["pid"] and pidfile["processIdentity"] == native["identity"]
            and process_identity(native["pid"]) == native["identity"]
            and executable(native["pid"]) == native["executable"]
            and str(Path(native["rendezvous"]).readlink()) == native["socket"]
            and (info.st_dev, info.st_ino) == (native["device"], native["inode"]), "native daemon replaced")


def daemon_check(daemon, record, custody):
    native_check(record, custody)
    daemon.check()  # Local process/socket/rendezvous checks only; no RPC.
    binding, native = record["binding"], custody["daemon"]
    require(daemon.pid == binding["daemon_pid"] == native["pid"]
            and daemon.identity == binding["daemon_identity"] == native["identity"]
            and str(daemon.target) == binding["socket"] == native["socket"]
            and str(daemon.rendezvous) == binding["rendezvous"] == native["rendezvous"]
            and executable(daemon.pid) == native["executable"], "daemon custody changed")
    info = safe(daemon.target, stat.S_ISSOCK, private=True)
    require((info.st_dev, info.st_ino) == (native["device"], native["inode"]), "daemon socket replaced")


def drain(child, timeout=3):
    """Own queue leader + observed descendants; never signal the daemon/pane.

    Record pidfds before signalling an escaped descendant. Sampling cannot prove
    an unobserved double-fork stopped; activation still needs native CLI boundary
    qualification. Output is /dev/null, not an unbounded pipe/log.
    """
    tracked = {}
    overflow = False
    deadline = time.monotonic() + timeout
    try:
        while True:
            parents = {child.pid, *(pid for pid, fd in tracked.items() if live(fd))}
            processes = []
            for entry in Path("/proc").iterdir():
                if not entry.name.isdigit():
                    continue
                try:
                    fields = (entry / "stat").read_text().rsplit(") ", 1)[1].split()
                    processes.append((int(entry.name), int(fields[1]), int(fields[19])))
                except (OSError, ValueError, IndexError):
                    continue
            for _ in range(8):
                added = False
                for pid, parent, start in processes:
                    if parent not in parents or pid in parents or pid in tracked:
                        continue
                    if len(tracked) >= 64:
                        overflow = True
                        deadline = 0
                        break
                    try:
                        fd = os.pidfd_open(pid, 0)
                    except ProcessLookupError:
                        continue
                    # A dead descendant's recycled numeric PID must not make
                    # an unrelated process our child. Pin start time and verify
                    # its parent again while the known parent is still live.
                    try:
                        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
                        owned = (int(fields[1]) == parent and int(fields[19]) == start
                                 and (parent == child.pid or live(tracked[parent])))
                    except (OSError, ValueError, IndexError):
                        owned = False
                    if not owned:
                        os.close(fd)
                        continue
                    tracked[pid] = fd
                    parents.add(pid)
                    added = True
                if not added:
                    break
            if child.poll() is not None or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
    finally:
        try:
            if child.poll() is None:
                # Unreaped leader pins this Popen-owned session/process group.
                os.killpg(child.pid, signal.SIGKILL)
            for fd in tracked.values():
                if live(fd):
                    try:
                        signal.pidfd_send_signal(fd, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            child.wait(timeout=1)
            until = time.monotonic() + 1
            while any(live(fd) for fd in tracked.values()) and time.monotonic() < until:
                time.sleep(0.01)
            cleaned = not overflow and all(not live(fd) for fd in tracked.values())
        finally:
            for fd in tracked.values():
                os.close(fd)
    return cleaned


def save(ledger, value):
    Ledger.compact(value)
    require(len(json.dumps(value).encode()) <= MAX_LEDGER, "receipt ledger full")
    atomic(ledger.path, value)


def presence_check(presence, record):
    require(isinstance(presence, dict) and presence.get("attended") is True
            and presence.get("lease", {}).get("holder") == record["holder"]
            and type(presence["lease"].get("expires_ms")) is int
            and presence["lease"]["expires_ms"] > time.time_ns() // 1_000_000,
            "hosted presence changed or expired")


def accounting(value):
    result = value.setdefault("runner", {"possibly_unattended_spent": 0, "declines": 0,
                                         "accepted_through": 0, "retry_after_ms": 0,
                                         "human_paused": False})
    require(isinstance(result, dict), "invalid runner accounting")
    for key in ("possibly_unattended_spent", "declines", "accepted_through", "retry_after_ms"):
        require(type(result.get(key)) is int and result[key] >= 0, "invalid runner accounting")
    require(result["possibly_unattended_spent"] <= TURN_BUDGET and result["declines"] <= K
            and type(result.get("human_paused")) is bool, "invalid runner budget/pause")
    return result


def progress(value):
    a = accounting(value)
    accepted = max(s.get("accepted_through", 0) for s in value["sessions"].values())
    if accepted > a["accepted_through"]:
        a.update(accepted_through=accepted, declines=0, retry_after_ms=0)


def lifecycle(state, record):
    """Independent durable forward cursor, even after a mail intent settles.

    scan() validates lifecycle/schema and saved file boundaries. The second pass
    reads exactly that validated batch to recover native started_at, not poll time.
    An initially open turn without its timestamp remains busy/unknown, never idle.
    """
    if "lifecycle" not in state:
        a = anchor(record["binding"], record["cwd"])
        state["lifecycle"] = {"scan": a, "body": None, "state": "observing",
                              "open_turn": a["open_turn"], "open_turn_trigger": a["open_turn_trigger"],
                              "lifecycle_version": 2, "turn_started_ms": None}
    cursor = state["lifecycle"]
    old = cursor["scan"]["offset"]
    # None cannot match a qualified native text-only user message.
    caught_up = scan(cursor, record["session"], record["cwd"])
    a = cursor["scan"]
    stream, info, meta = open_rollout(a["path"], record["session"], record["cwd"])
    with stream:
        require((info.st_dev, info.st_ino) == (a["device"], a["inode"])
                and meta == a["meta_hash"] and checkpoint(stream, a["offset"]) == a["boundary_hash"],
                "busy history changed")
        stream.seek(old)
        for line in stream.read(a["offset"] - old).splitlines():
            value = json.loads(line)
            if value.get("type") != "event_msg":
                continue
            event = value["payload"]
            if event.get("type") == "task_started":
                started = event.get("started_at")
                require(type(started) is int and started > 0, "native busy start timestamp unavailable")
                cursor["turn_started_ms"] = started * 1000
            elif event.get("type") in ("task_complete", "turn_aborted"):
                cursor["turn_started_ms"] = None
                if event.get("type") == "turn_aborted":
                    state["native_stop"] = True
                    state["last_abort"] = event["turn_id"]
                    state["native_abort_count"] = state.get("native_abort_count", 0) + 1
    return caught_up


class Runner:
    def __init__(self, registry):
        self.registry = Path(registry)
        record = load(self.registry)
        self.ledger = Ledger(self.registry, record["home"], record["ship"])

    def report(self, session, expected=None, expected_custody=None):
        """Dormant report data, never wake.hosted or a fabricated mail wake."""
        lease = None
        snapshot = None
        try:
            snapshot = load(self.registry)
            lease = control(snapshot, "ship.presence", {"ship": snapshot["ship"]})
        except (OSError, ValueError, KeyError, TypeError):
            pass
        with locked(self.registry.parent):
            value = self.ledger.read()
            state = value["sessions"][exact_uuid(session)]
            a = accounting(value)
            intent = state.get("intent", {})
            cursor = state.get("lifecycle", {})
            uncertain = intent.get("state") in ("invoking", "uncertain")
            paused = (uncertain or state.get("native_stop", False) or a["human_paused"] or a["declines"] >= K
                      or a["possibly_unattended_spent"] >= TURN_BUDGET or "diagnostic" in a)
            eligible = False
            handles = None
            try:
                record = load(self.registry)
                require(expected is None or record == expected, "superseded observation")
                require(record == snapshot and record["session"] == session and record["state"] == "bound"
                        and record["delivery_enabled"] is True, "stale report")
                presence_check(lease, record)
                for key in ("generation", "callback_generation", "holder"):
                    require(record[key] == intent[key], "stale report generation")
                require_fence(self.registry, record)
                custody = load(self.registry.with_name("custody.json"))
                require(expected_custody is None or custody == expected_custody, "superseded custody")
                custody_check(self.registry, record, custody)
                require(intent.get("custody_epoch", custody["epoch"]) == custody["epoch"], "stale report epoch")
                handles = Handles(custody)
                native_check(record, custody)
                eligible = True
            except (OSError, ValueError, KeyError, TypeError):
                pass
            finally:
                if handles is not None:
                    handles.close()
            return {"session": session, "report_eligible": eligible,
                    "accepted_through": state["accepted_through"],
                    "paused": paused or not eligible, "decline_reason": "other" if paused or not eligible else None,
                    "decline_count": a["declines"], "busy": bool(cursor.get("open_turn")),
                    "turn_started_ms": cursor.get("turn_started_ms"),
                    "busy_available": "lifecycle" in state and not a.get("observation_lost", True)
                                      and (not cursor.get("open_turn") or cursor.get("turn_started_ms") is not None),
                    "possibly_unattended_spent": a["possibly_unattended_spent"],
                    "possibly_unattended_budget": TURN_BUDGET,
                    "diagnostic": ("codex-uncertain-" + intent["nonce"]) if uncertain else a.get("diagnostic")}

    def observe(self, acknowledged_through=0):
        """Read-only native evidence; no queue mutation and no enable path."""
        record = load(self.registry)
        session = exact_uuid(record["session"])
        require(type(acknowledged_through) is int and acknowledged_through >= 0,
                "invalid runtime acknowledgement")
        # Runtime ack is independent positive evidence, even if the daemon is
        # gone. No queue absence below is used as proof of non-admission.
        if acknowledged_through:
            value = self.ledger.read()
            intent = value["sessions"].get(session, {}).get("intent")
            if intent:
                self.ledger.reconcile(session, intent["nonce"], {"data": [], "nextCursor": None},
                                      acknowledged_through)
        try:
            with Daemon(record["codex_home"]) as daemon:
                thread = daemon.call("thread/read", {"threadId": session})["thread"]
                require(thread.get("id") == session and thread.get("cwd") == record["cwd"]
                        and isinstance(thread.get("status"), dict), "observer thread/status mismatch")
                queue = daemon.call("thread/queue/list", {"threadId": session, "limit": 128})
                value = self.ledger.read()
                intent = value["sessions"].get(session, {}).get("intent")
                if intent:
                    self.ledger.reconcile(session, intent["nonce"], queue, acknowledged_through)
                self._observe_lifecycle(record, thread.get("status"))
        except (OSError, ValueError, KeyError, TypeError):
            with locked(self.registry.parent):
                value = self.ledger.read()
                a = accounting(value)
                a.update(observation_lost=True, diagnostic="codex-native-evidence-unavailable")
                progress(value)
                save(self.ledger, value)
        return self.report(session, record)

    def _observe_lifecycle(self, record, status=None):
        with locked(self.registry.parent):
            value = self.ledger.read()
            state = value["sessions"][record["session"]]
            a = accounting(value)
            candidate = copy.deepcopy(state)
            try:
                caught_up = lifecycle(candidate, record)
                aborts = candidate.get("native_abort_count", 0) - state.get("native_abort_count", 0)
                a["declines"] = min(K, a["declines"] + aborts)
                state.update(candidate)  # Commit bounded forward progress too.
                available = caught_up and (status is None or (
                    status == {"type": "idle"} and not state["lifecycle"]["open_turn"]))
                a["observation_lost"] = not available
                if available and a.get("diagnostic") == "codex-native-evidence-unavailable":
                    a.pop("diagnostic", None)
                if not available:
                    a["diagnostic"] = "codex-native-evidence-unavailable"
            except (OSError, ValueError, KeyError, TypeError):
                # Retain the last known busy ID/start; disconnection is not idle.
                a["observation_lost"] = True
                a["diagnostic"] = "codex-native-evidence-unavailable"
            progress(value)
            save(self.ledger, value)

    def dispatch(self):
        """One dormant invocation. Production records always fail enabled check.

        No retry loop, environment override, activation control or scheduler.
        A test may supply a fixture-owned enabled registry; no code writes one.
        """
        record = load(self.registry)
        require(record.get("delivery_enabled") is True, "Codex delivery disabled")
        session = exact_uuid(record["session"])
        custody = load(self.registry.with_name("custody.json"))
        with Daemon(record["codex_home"]) as daemon:
            # Every RPC precedes flock. Native queue serializes an idle->busy race.
            loaded = daemon.call("thread/loaded/list", {"limit": 32})
            require(loaded.get("nextCursor") is None and isinstance(loaded.get("data"), list)
                    and len(loaded["data"]) <= 32 and session in loaded["data"], "thread not loaded")
            thread = daemon.call("thread/read", {"threadId": session})["thread"]
            require(thread.get("id") == session and thread.get("cwd") == record["cwd"]
                    and thread.get("historyMode") == "legacy" and not thread.get("ephemeral")
                    and thread.get("status") == {"type": "idle"}, "native thread not qualified idle")
            queue = daemon.call("thread/queue/list", {"threadId": session, "limit": 128})
            require(queue.get("data") == [] and queue.get("nextCursor") is None,
                    "pending or unsupported native queue")
            self._observe_lifecycle(record, thread["status"])
            return self._spawn(record, custody, daemon)

    def _spawn(self, record, custody, daemon):
        # The runtime owns presence; this RPC precedes flock. Poison serializes
        # holder replacement, and the checked expiry cannot be renewed by mail.
        lease = control(record, "ship.presence", {"ship": record["ship"]})
        child = None
        post_spawn_lost = False
        with locked(self.registry.parent):
            safe(self.registry, private=True)
            current = load(self.registry)
            require(current == record and current.get("state") == "bound"
                    and current.get("hosted") is True and current.get("delivery_enabled") is True,
                    "binding changed or disabled")
            presence_check(lease, current)
            binding = current["binding"]
            require(binding["session"] == current["session"], "binding UUID changed")
            require_fence(self.registry, current)
            custody_check(self.registry, current, custody)
            value = self.ledger.read()
            state = value["sessions"][current["session"]]
            intent = state["intent"]
            a = accounting(value)
            require(not a["human_paused"] and a["declines"] < K
                    and a["possibly_unattended_spent"] < TURN_BUDGET
                    and time.time_ns() // 1_000_000 >= a["retry_after_ms"]
                    and not a.get("observation_lost", True) and "diagnostic" not in a,
                    "runner paused, unavailable or budget exhausted")
            require(intent["state"] == "prepared" and not state["lifecycle"]["open_turn"]
                    and not state.get("native_stop", False), "outstanding attempt, native stop or native busy")
            for key in ("generation", "callback_generation", "holder", "cwd"):
                require(intent[key] == current[key], "intent launch/callback/custody changed")
            require(digest(intent["body"].encode()) == intent["body_hash"]
                    and intent["body"].split("\n", 1)[0] == encode(marker(
                        current["home"], current["ship"], current["session"], intent["nonce"],
                        intent["after_seq"], intent["through_seq"])), "intent body/UUID changed")
            require(all(s.get("intent", {}).get("state") in FINAL for sid, s in value["sessions"].items()
                        if sid != current["session"] and s.get("intent")), "unsettled predecessor")
            # Fresh scan closes any rollout lag since the unlocked observation.
            require(scan(intent, current["session"], current["cwd"]) and not intent.get("open_turn")
                    and intent["state"] == "prepared", "receipt evidence not idle/caught up")
            env = {"PATH": os.defpath, "LANG": "C.UTF-8"}
            env.update({k: v for k, v in current["path_env"].items() if k in PATH_ENV})
            require(env.get("CODEX_HOME", str(Path(env["HOME"]) / ".codex")) == current["codex_home"],
                    "queue native home mismatch")
            argv = [current["executable"], "queue", "--remote", "unix://" + binding["socket"],
                    "--thread", exact_uuid(current["session"]), "--message", intent["body"]]
            require(Path(argv[0]).is_absolute(), "queue executable not absolute")
            intent.update(state="invoking", custody_epoch=custody["epoch"], possibly_unattended=True)
            a["possibly_unattended_spent"] += 1
            save(self.ledger, value)  # Crash thereafter = uncertain, never refund.
            # After durable invoking, immediately at the real spawn boundary.
            require(load(self.registry) == current, "binding changed before spawn")
            require_fence(self.registry, current)
            custody_check(self.registry, current, custody)
            daemon_check(daemon, current, custody)
            handles = Handles(custody)
            try:
                handles.check()
                presence_check(lease, current)  # No stale lease after slow fsync.
                try:
                    child = subprocess.Popen(argv, cwd=current["cwd"], env=env, stdin=subprocess.DEVNULL,
                                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                             start_new_session=True)
                except OSError:
                    # Only Popen exec failure, never CLI exit or identity loss.
                    intent["state"] = "rejected"
                    a["declines"] = min(K, a["declines"] + 1)
                    a["retry_after_ms"] = time.time_ns() // 1_000_000 + 60_000
                    save(self.ledger, value)
                    raise
                # A late exit/poison cannot cancel native admission. Preserve the
                # old intent, charge it, and withhold replacement-custody reports.
                try:
                    handles.check()
                    require(load(self.registry) == current, "binding changed after spawn")
                    require_fence(self.registry, current)
                    custody_check(self.registry, current, custody)
                    daemon_check(daemon, current, custody)
                except (OSError, ValueError, KeyError, TypeError):
                    post_spawn_lost = True
            finally:
                handles.close()
        # No wait/RPC/report under project flock. English CLI output is discarded.
        cleaned = False
        try:
            cleaned = drain(child)
        finally:
            # Nonzero, timeout, success prose, and clean exit are all uncertain.
            # A later observer reconciles native evidence; no resend here.
            with locked(self.registry.parent):
                value = self.ledger.read()
                state = value["sessions"][record["session"]]
                # An operator may have settled the old offer while the child
                # ran. Never apply its result to a replacement intent.
                for saved in [state.get("intent", {}), *state.get("history", [])]:
                    if saved.get("nonce") == intent["nonce"] and saved["state"] == "invoking":
                        saved["state"] = "uncertain"
                if post_spawn_lost:
                    accounting(value)["diagnostic"] = "codex-custody-lost-after-invocation"
                if not cleaned:
                    accounting(value)["diagnostic"] = "codex-queue-cleanup-unproven"
                save(self.ledger, value)
        return self.report(record["session"], record, custody)
