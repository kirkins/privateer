"""Codex attended-mail queue runner, one bounded pass per supervisor tick.

The hosted supervisor runs `tick` as its own child while pane custody holds.
No native attachment claim: every invocation reserves a possibly-unattended
turn from the captain's grant (Rust, never refunded), including lost responses.
Lock order: SQLite/presence/RPC outside project flock; no RPC/wait under flock.
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
import uuid

sys.dont_write_bytecode = True
# The supervisor runs this script under `python3 -I -B`, which omits the script's
# own folder from sys.path; restore it before importing sibling modules.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from codex_binding import PATH_ENV, atomic, control, locked, require_fence
import codex_mail
from codex_receipts import (FINAL, MAX_LEDGER, MAX_SESSIONS, Ledger, anchor, checkpoint, digest,
                            encode, exact_uuid, marker, open_rollout, scan)
from codex_socket import Daemon, LIMIT, load, process_identity, safe

K = 3
PRE_SPAWN_FENCE_REFUSED = "codex-loaded-thread-fence-refused-before-invocation"
FENCE_CLEARS_WHEN = "a later pre-spawn fence passes, or a trusted rebind / hosted relaunch"
THREAD_FENCE_LOST = "codex-loaded-thread-fence-lost-after-invocation"
STICKY = ("codex-custody-lost-after-invocation", "codex-queue-cleanup-unproven", THREAD_FENCE_LOST)


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
    reason = ("custody poisoned: resume callback requires explicit hosted relaunch"
              if poison.get("reason") == "resume_callback_requires_relaunch" else "custody poisoned")
    require(poison.get("reason") is None and poison.get("generation") == record["generation"]
            and poison["token"] == current["token"], reason)
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


class LoadedThreadRefused(ValueError):
    def __init__(self, reason, facts):
        super().__init__(reason)
        self.facts = copy.deepcopy(facts)


def loaded_thread(daemon, record):
    """Single-thread-in-cwd custody, not an atomic native admission fence."""
    session = record["session"]
    loaded = daemon.call("thread/loaded/list", {"limit": 32})
    require(isinstance(loaded, dict), "invalid loaded-thread response")
    data = loaded.get("data")
    facts = {"loaded_count": len(data) if isinstance(data, list) else None,
             "threads_in_cwd": None, "bound_uuid_loaded": session in data if isinstance(data, list) else None,
             "cursor_state": "missing" if "nextCursor" not in loaded else (
                 "null" if loaded["nextCursor"] is None else "non-null"),
             "cwd_thread_is_bound": None}
    def fence(condition, reason):
        if not condition:
            raise LoadedThreadRefused(reason, facts)
    fence(facts["cursor_state"] == "null" and isinstance(data, list)
          and len(data) <= 32 and session in data, "thread not loaded")
    # Idle /new has no SessionStart callback yet: the old UUID can remain
    # loaded while the attended pane watches another thread. Only a trusted
    # callback may rebind; never redirect the offer to a different UUID.
    threads = [daemon.call("thread/read", {"threadId": identity})["thread"]
               for identity in loaded["data"]]
    require(all(isinstance(thread, dict) for thread in threads), "invalid thread/read response")
    matches = [thread for thread in threads if thread.get("cwd") == record["cwd"]]
    facts["threads_in_cwd"] = len(matches)
    facts["cwd_thread_is_bound"] = len(matches) == 1 and matches[0].get("id") == session
    fence(len(matches) == 1, "concurrent Codex threads in this cwd are ambiguous")
    fence(matches[0].get("id") == session, "loaded thread for this cwd is not the bound UUID")
    return matches[0]


def exited(child):
    """Leader exit WITHOUT reaping: the zombie keeps pinning its process group."""
    return os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None


def drain(child, timeout=3):
    """Own queue leader + observed descendants; never signal the daemon/pane.

    Record pidfds before signalling an escaped descendant. Sampling cannot prove
    an unobserved double-fork stopped; activation still needs native CLI boundary
    qualification. Output is /dev/null, not an unbounded pipe/log. The whole
    process group is killed on timeout AND after a normal exit, before reaping.
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
            if exited(child) or time.monotonic() >= deadline:
                break
            time.sleep(0.02)
    finally:
        try:
            # The unreaped leader (live or zombie) pins this Popen-owned
            # session/process group, so its ID cannot name another group.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # Group already empty except the zombie leader.
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
    require(result["declines"] <= K and type(result.get("human_paused")) is bool,
            "invalid runner budget/pause")
    # Local per-grant floor beside the Rust count: never decremented, so a
    # reset or replayed runtime answer cannot widen a grant here either.
    grants = result.setdefault("grants", {})
    require(isinstance(grants, dict) and all(
        isinstance(g, dict) and type(g.get("spent")) is int and type(g.get("budget")) is int
        and 0 <= g["spent"] <= g["budget"] for g in grants.values()), "invalid runner grant accounting")
    # Sticky diagnostics survive the transient native-evidence one. Older
    # ledgers stored a sticky name in "diagnostic"; move it, never drop it.
    sticky = result.setdefault("sticky", [])
    require(isinstance(sticky, list) and all(name in STICKY for name in sticky), "invalid runner diagnostics")
    if result.get("diagnostic") in STICKY:
        name = result.pop("diagnostic")
        if name not in sticky:
            sticky.append(name)
    refusal = result.get("pre_spawn_fence")
    if refusal is not None:
        # A corrupt optional diagnostic is absent, not an exception that kills
        # every tick/status. It never authorizes a spawn: dispatch fences anew.
        context = refusal.get("context") if isinstance(refusal, dict) else None
        facts = refusal.get("facts") if isinstance(refusal, dict) else None
        valid = (isinstance(context, dict)
                 and set(context) == {"generation", "callback_generation", "session", "holder"}
                 and all(isinstance(v, str) and v for v in context.values())
                 and isinstance(facts, dict)
                 and set(facts) == {"loaded_count", "threads_in_cwd", "bound_uuid_loaded",
                                    "cursor_state", "cwd_thread_is_bound"}
                 and all(facts[k] is None or type(facts[k]) is int and facts[k] >= 0
                         for k in ("loaded_count", "threads_in_cwd"))
                 and all(facts[k] is None or type(facts[k]) is bool
                         for k in ("bound_uuid_loaded", "cwd_thread_is_bound"))
                 and facts["cursor_state"] in ("missing", "null", "non-null")
                 and refusal.get("diagnostic") == PRE_SPAWN_FENCE_REFUSED
                 and refusal.get("clears_when") == FENCE_CLEARS_WHEN
                 and type(refusal.get("at_ms")) is int and refusal["at_ms"] >= 0)
        if not valid:
            result.pop("pre_spawn_fence", None)
    return result


def diagnostic(a):
    return a["sticky"][0] if a["sticky"] else (
        PRE_SPAWN_FENCE_REFUSED if a.get("pre_spawn_fence") else a.get("diagnostic"))


def diagnostics_pause(a):
    return bool(a["sticky"]) or "diagnostic" in a or bool(a.get("pre_spawn_fence"))


def reserved(intent, record):
    """The captain grant's reservation for exactly this offer, or refuse."""
    r = intent.get("reservation")
    require(isinstance(r, dict) and r.get("session") == record["session"] == intent.get("session", record["session"])
            and r.get("nonce") == intent.get("nonce") and type(r.get("budget")) is int
            and type(r.get("spent")) is int and 1 <= r["spent"] <= r["budget"], "Codex delivery not granted")
    exact_uuid(r["grant_id"])
    return r


def progress(value):
    a = accounting(value)
    accepted = max((s.get("accepted_through", 0) for s in value["sessions"].values()), default=0)
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

    def ensure_session(self, record):
        """The bound UUID's watermarks, before any offer exists. Same bound."""
        session = exact_uuid(record["session"])
        def change(value):
            if session not in value["sessions"]:
                require(len(value["sessions"]) < MAX_SESSIONS, "receipt session index full; pinned facts not evicted")
                value["sessions"][session] = {"accepted_through": 0, "acknowledged_through": 0}
            return value["sessions"][session]
        return self.ledger.transaction(change)

    def reserve(self, record, *, grant=None):
        """Spend one grant turn for the prepared offer, before any flock.

        The Rust reservation is idempotent per nonce: a lost response retried
        next tick never spends twice. It is pinned in the intent and re-read
        under the lock; a revoke poisons custody, which the final fence reads.
        Dispatch supplies a grant-only snapshot fetched BEFORE its native fence,
        avoiding a full status adapter child in the fence-to-spawn window. The
        snapshot is not authority: Rust still validates the grant and nonce.
        """
        session = exact_uuid(record["session"])
        intent = self.ledger.read()["sessions"][session]["intent"]
        if intent.get("reservation"):
            return reserved(intent, record)
        require(intent["state"] == "prepared", "only a prepared offer reserves a turn")
        if grant is None:
            status = control(record, "attended.codex.status", {"ship": record["ship"]})
            grant = status.get("grant") if isinstance(status, dict) else None
        require(isinstance(grant, dict) and grant.get("session") == session, "Codex delivery not granted")
        try:
            result = control(record, "attended.codex.reserve", {
                "ship": record["ship"], "holder": record["holder"], "session": session,
                "grant_id": exact_uuid(grant["grant_id"]), "nonce": intent["nonce"]})
        except ValueError:
            if grant.get("remaining") == 0:
                self.ledger.transaction(lambda v: accounting(v).update(budget_exhausted=grant["grant_id"]))
            raise
        require(isinstance(result, dict) and result.get("reserved") is True and result.get("session") == session
                and result.get("nonce") == intent["nonce"] and result.get("grant_id") == grant["grant_id"],
                "invalid Codex turn reservation")
        reservation = {key: result[key] for key in ("grant_id", "session", "nonce", "budget", "spent")}
        def change(value):
            current = Ledger.intent(value, session, intent["nonce"])
            require(current["state"] == "prepared" and not current.get("reservation"), "offer changed while reserving")
            current["reservation"] = reservation
            accounting(value).pop("budget_exhausted", None)
            return reserved(current, record)
        return self.ledger.transaction(change)

    def publish(self, record, result):
        """wake.hosted for the current holder only: delivery state, and a mail
        wake for each promoted native receipt. Queue admission is not progress."""
        if not result.get("report_eligible"):
            return
        session = exact_uuid(record["session"])
        state = self.ledger.read()["sessions"][session]
        intent = state.get("intent", {})
        if intent.get("state") == "presented" and not intent.get("wake_reported"):
            reply = control(record, "wake.hosted", {
                "ship": record["ship"], "holder": record["holder"], "session": session, "trigger": "mail",
                "after_seq": intent["after_seq"], "through_seq": intent["through_seq"]})
            require(reply.get("recorded") is True, "hosted mail wake not recorded")
            def change(value):
                current = value["sessions"][session].get("intent", {})
                if current.get("nonce") == intent["nonce"]:
                    current["wake_reported"] = True
            self.ledger.transaction(change)
        # Without a truthful native busy start, withhold the report rather
        # than claim idle; the Rust watchdog still bounds blocked mail.
        if not result.get("busy_available"):
            return
        report = {"ship": record["ship"], "holder": record["holder"], "session": session, "trigger": "delivery",
                  "accepted_through": result["accepted_through"], "paused": result["paused"],
                  "decline_reason": result["decline_reason"] or "other", "decline_count": result["decline_count"],
                  "busy": result["busy"]}
        if result["busy"]:
            report["turn_started_ms"] = result["turn_started_ms"]
        reply = control(record, "wake.hosted", report)
        require(reply.get("recorded") is True, "hosted delivery report not recorded")

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
            grant = a["grants"].get(intent.get("reservation", {}).get("grant_id"), {})
            paused = (uncertain or state.get("native_stop", False) or a["human_paused"] or a["declines"] >= K
                      or diagnostics_pause(a) or "budget_exhausted" in a)
            eligible = False
            handles = None
            try:
                record = load(self.registry)
                require(THREAD_FENCE_LOST not in a["sticky"], "loaded-thread custody lost after invocation")
                require(expected is None or record == expected, "superseded observation")
                require(record == snapshot and record["session"] == session and record["state"] == "bound"
                        and record.get("hosted") is True, "stale report")
                presence_check(lease, record)
                # An outstanding offer belongs to its own launch; a settled
                # one from an earlier launch cannot block this launch's reports.
                if intent and intent.get("state") not in FINAL:
                    for key in ("generation", "callback_generation", "holder"):
                        require(record[key] == intent[key], "stale report generation")
                require_fence(self.registry, record)
                custody = load(self.registry.with_name("custody.json"))
                require(expected_custody is None or custody == expected_custody, "superseded custody")
                custody_check(self.registry, record, custody)
                require(intent.get("state") in FINAL or intent.get("custody_epoch", custody["epoch"]) == custody["epoch"],
                        "stale report epoch")
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
                    "grant_spent": grant.get("spent"), "grant_budget": grant.get("budget"),
                    "diagnostic": ("codex-uncertain-" + intent["nonce"]) if uncertain else (
                        diagnostic(a) or ("codex-turn-budget-exhausted" if "budget_exhausted" in a else None)),
                    "diagnostics": list(a["sticky"]),
                    "pre_spawn_fence": copy.deepcopy(a.get("pre_spawn_fence")),
                    "pause_reason": a.get("thread_fence_reason") or (
                        FENCE_CLEARS_WHEN if a.get("pre_spawn_fence") and not a["sticky"]
                        else intent.get("pause_reason"))}

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
            # A synthetic empty queue is safe only when the ack alone settles
            # the whole range; otherwise wait for real queue evidence below.
            if intent and acknowledged_through >= intent["through_seq"]:
                self.ledger.reconcile(session, intent["nonce"], {"data": [], "nextCursor": None},
                                      acknowledged_through)
        try:
            with Daemon(record["codex_home"]) as daemon:
                thread = daemon.call("thread/read", {"threadId": session})["thread"]
                require(isinstance(thread, dict) and thread.get("id") == session and thread.get("cwd") == record["cwd"]
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
                # Native status must agree with the rollout lifecycle: idle with
                # no open turn, or active with one. Anything else is unknown.
                open_turn = state["lifecycle"]["open_turn"]
                available = caught_up and (status is None or (status == {"type": "idle"} and not open_turn)
                                           or (isinstance(status, dict) and status.get("type") == "active"
                                               and bool(open_turn)))
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

    def _record_pre_spawn_fence(self, record, refusal=None):
        # Observations/RPCs precede flock. Never attach an old snapshot to a
        # replacement binding, and never modify the post-invocation sticky set.
        with locked(self.registry.parent):
            require(load(self.registry) == record, "binding changed during loaded-thread fence")
            value = self.ledger.read()
            a = accounting(value)
            previous = a.get("pre_spawn_fence")
            context = {key: record[key] for key in ("generation", "callback_generation", "session", "holder")}
            current = ({"diagnostic": PRE_SPAWN_FENCE_REFUSED, "facts": refusal.facts,
                        "context": context, "clears_when": FENCE_CLEARS_WHEN} if refusal else None)
            if (previous and {k: v for k, v in previous.items() if k != "at_ms"} == current
                    or previous is None and current is None):
                return  # One bounded ledger event per state change, not per tick.
            at_ms = time.time_ns() // 1_000_000
            if current:
                current["at_ms"] = at_ms
                a["pre_spawn_fence"] = current
            else:
                a.pop("pre_spawn_fence", None)
            events = a.setdefault("pre_spawn_fence_events", [])
            events.append(copy.deepcopy(current) if current else {
                "diagnostic": PRE_SPAWN_FENCE_REFUSED, "state": "cleared", "at_ms": at_ms})
            del events[:-16]
            save(self.ledger, value)

    def pre_spawn_thread(self, daemon, record):
        try:
            thread = loaded_thread(daemon, record)
        except LoadedThreadRefused as refusal:
            self._record_pre_spawn_fence(record, refusal)
            raise
        self._record_pre_spawn_fence(record)
        return thread

    def refresh_pre_spawn_fence(self, record):
        a = accounting(self.ledger.read())
        previous = a.get("pre_spawn_fence")
        if not previous:
            return False
        if previous["context"] != {key: record[key] for key in previous["context"]}:
            self._record_pre_spawn_fence(record)  # Trusted rebind/relaunch, not a sticky reset.
            return True
        with Daemon(record["codex_home"]) as daemon:
            self.pre_spawn_thread(daemon, record)
        return True

    def dispatch(self):
        """One dormant invocation. Production records always fail enabled check.

        No retry loop, environment override, activation control or scheduler.
        A test may supply a fixture-owned enabled registry; no code writes one.
        """
        record = load(self.registry)
        session = exact_uuid(record["session"])
        custody = load(self.registry.with_name("custody.json"))
        grant = {}  # Missing/invalid grant fails closed in reserve(), without another status RPC.
        intent = self.ledger.read()["sessions"][session]["intent"]
        if not intent.get("reservation"):
            status = control(record, "attended.codex.status", {"ship": record["ship"]})
            if isinstance(status, dict) and isinstance(status.get("grant"), dict):
                grant = status["grant"]
        with Daemon(record["codex_home"]) as daemon:
            # RPCs outside flock. Native queue serializes an idle->busy race.
            thread = self.pre_spawn_thread(daemon, record)
            require(thread.get("cwd") == record["cwd"]
                    and thread.get("historyMode") == "legacy" and not thread.get("ephemeral")
                    and thread.get("status") == {"type": "idle"}, "native thread not qualified idle")
            queue = daemon.call("thread/queue/list", {"threadId": session, "limit": 128})
            require(queue.get("data") == [] and queue.get("nextCursor") is None,
                    "pending or unsupported native queue")
            self._observe_lifecycle(record, thread["status"])
            self.reserve(record, grant=grant)  # A refused fence never spends a reservation.
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
                    and current.get("hosted") is True, "binding changed or disabled")
            presence_check(lease, current)
            binding = current["binding"]
            require(binding["session"] == current["session"], "binding UUID changed")
            require_fence(self.registry, current)
            custody_check(self.registry, current, custody)
            value = self.ledger.read()
            state = value["sessions"][current["session"]]
            intent = state["intent"]
            a = accounting(value)
            r = reserved(intent, current)
            g = a["grants"].setdefault(r["grant_id"], {"spent": 0, "budget": r["budget"]})
            g["budget"] = min(g["budget"], r["budget"])  # Never widened locally.
            require(g["spent"] < g["budget"], "Codex possibly-unattended turn budget exhausted")
            require(not a["human_paused"] and a["declines"] < K
                    and time.time_ns() // 1_000_000 >= a["retry_after_ms"]
                    and not a.get("observation_lost", True) and not diagnostics_pause(a),
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
            g["spent"] += 1
            save(self.ledger, value)  # Crash thereafter = uncertain, never refund.
            # After durable invoking, immediately at the real spawn boundary.
            require(load(self.registry) == current, "binding changed before spawn")
            daemon_check(daemon, current, custody)
            handles = Handles(custody)
            try:
                handles.check()
                presence_check(lease, current)  # No stale lease after slow fsync.
                # Both fences last: the poison read is the final step before Popen.
                require_fence(self.registry, current)
                custody_check(self.registry, current, custody)
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
            try:
                # Recheck after Popen and lock release: /new during presence,
                # scanning or fsync may already have made admission unwatched.
                # Do not require idle here: our child may have started a turn.
                # Remaining window: /new after the final loaded-list snapshot
                # (including during thread/read RPCs) can still precede native
                # admission/promotion undetected; Codex has no atomic
                # watched-thread admission fence.
                # The dispatch connection's single observation deadline may
                # be spent by the first fence, presence, scans, fsync and Popen.
                # Give this fence its own deadline, but never accept a new
                # daemon identity just because reconnecting succeeded.
                with Daemon(record["codex_home"]) as fence_daemon:
                    daemon_check(fence_daemon, record, custody)
                    loaded_thread(fence_daemon, record)
            except (OSError, ValueError, KeyError, TypeError) as error:
                # Persist before draining. Later receipt/ack evidence may settle
                # the offer, but neither it, unloading /new, nor restart may
                # resume mail or publish success for this unwatched attempt.
                with locked(self.registry.parent):
                    value = self.ledger.read()
                    a = accounting(value)
                    a["thread_fence_reason"] = "loaded-thread custody unproven after invocation: " + str(error)
                    if THREAD_FENCE_LOST not in a["sticky"]:
                        a["sticky"].append(THREAD_FENCE_LOST)
                    save(self.ledger, value)
            finally:
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
                a = accounting(value)
                for name, failed in (("codex-custody-lost-after-invocation", post_spawn_lost),
                                     ("codex-queue-cleanup-unproven", not cleaned)):
                    if failed and name not in a["sticky"]:
                        a["sticky"].append(name)  # Both kept; neither hides the other.
                save(self.ledger, value)
        return self.report(record["session"], record, custody)


def tick(path):
    """One supervisor pass. Every RPC is outside the project flock; one offer."""
    runner = Runner(path)
    record = load(runner.registry)
    if record.get("state") != "bound" or record.get("hosted") is not True:
        return {"tick": "unbound"}
    session = exact_uuid(record["session"])
    runner.ensure_session(record)
    presence_check(control(record, "ship.presence", {"ship": record["ship"]}), record)
    exhausted = accounting(runner.ledger.read()).get("budget_exhausted")
    if exhausted:
        # Only a new captain grant clears exhaustion; never time or mail.
        grant = control(record, "attended.codex.status", {"ship": record["ship"]}).get("grant") or {}
        if grant.get("session") == session and grant.get("grant_id") not in (None, exhausted):
            runner.ledger.transaction(lambda v: accounting(v).pop("budget_exhausted", None))
    state = runner.ledger.read()["sessions"][session]
    seen = max(state["accepted_through"], state["acknowledged_through"], state.get("suppressed_through", 0))
    pending = codex_mail.snapshot(control(record, "mail.pending", {"address": record["ship"], "after_seq": seen}),
                                  record["ship"])
    result = runner.observe(acknowledged_through=pending["acknowledged_through"])
    try:
        if runner.refresh_pre_spawn_fence(record):
            result = runner.report(session, record)
    except LoadedThreadRefused:
        runner.publish(record, runner.report(session, record))
        return {"tick": "held"}
    except (OSError, ValueError, KeyError, TypeError):
        # Failed refresh is not a passing fence. Keep the refusal, but publish
        # observation/receipt progress rather than losing all reports this tick.
        runner.publish(record, result)
        return {"tick": "held"}
    runner.publish(record, result)
    value = runner.ledger.read()
    intent = value["sessions"][session].get("intent")
    if intent is None or intent["state"] in FINAL:
        if intent is not None and intent["state"] == "presented" and not intent.get("wake_reported"):
            return {"tick": "mail wake unreported"}
        if (result["paused"] or result["busy"] or not result["report_eligible"]
                or pending["after_seq"] < result["accepted_through"] or not codex_mail.actionable(pending)):
            return {"tick": "idle"}
        grant = control(record, "attended.codex.status", {"ship": record["ship"]}).get("grant") or {}
        if grant.get("session") != session or not grant.get("remaining"):
            return {"tick": "not granted"}  # No offer, no nonce, nothing spent.
        nonce = str(uuid.uuid4())
        runner.ledger.prepare(record, codex_mail.body(record, nonce, pending),
                              pending["after_seq"], pending["through_seq"], nonce)
    elif intent["state"] != "prepared":
        return {"tick": "outstanding"}
    elif result["paused"] or result["busy"] or not result["report_eligible"]:
        return {"tick": "held"}  # A prepared offer waits; nothing is spent.
    try:
        result = runner.dispatch()
    except LoadedThreadRefused:
        runner.publish(record, runner.report(session, record))
        return {"tick": "held"}
    runner.publish(record, result)
    return {"tick": "invoked"}


OUTCOME = {"prepared": "pending", "queued": "pending", "invoking": "uncertain", "uncertain": "uncertain"}
LATE = ("Late native admission of this offer remains possible: resolution neither cancels "
        "a queued item or turn nor acknowledges mail.")


def bound_runner(path):
    """Refuse before any ledger access unless a hosted binding is current."""
    record = load(Path(path))
    require(isinstance(record, dict) and record.get("state") == "bound" and record.get("hosted") is True,
            "no current hosted Codex binding")
    return Runner(path)


def operator_check(runner, request, current):
    """The runtime's operator gate ran in Rust; this checks it is still current.

    Current bound hosted launch, the named holder, both fences, owned custody
    and the live lease. `current` also requires the binding's own session.
    """
    record = load(runner.registry)
    require(isinstance(request, dict) and record.get("state") == "bound" and record.get("hosted") is True,
            "no current hosted Codex binding")
    session, nonce = exact_uuid(request.get("session")), exact_uuid(request.get("notification_id"))
    require(request.get("ship") == record["ship"] and request.get("holder") == record["holder"]
            and (not current or session == record["session"]), "ship/holder/session is not the current binding")
    require_fence(runner.registry, record)
    custody_check(runner.registry, record, load(runner.registry.with_name("custody.json")))
    presence_check(control(record, "ship.presence", {"ship": record["ship"]}), record)
    intent = runner.ledger.read()["sessions"].get(session, {}).get("intent")
    require(isinstance(intent, dict) and intent.get("nonce") == nonce, "not this session's current offer")
    return record, session, nonce


def evidence_control(runner, record, session, nonce, current):
    """The tick's own evidence reduction for one offer, current or older.

    A current session reads its verified native thread/queue. An older UUID's
    pane is gone, so its only admissible queue page is empty (which can show an
    offer present, never absent); its rollout scan and the mailbox ack still
    run exactly as for the current session. Absence is never a rejection.
    """
    state = runner.ledger.read()["sessions"][session]
    seen = max(state["accepted_through"], state["acknowledged_through"], state.get("suppressed_through", 0))
    pending = codex_mail.snapshot(control(record, "mail.pending", {"address": record["ship"], "after_seq": seen}),
                                  record["ship"])
    if current:
        result = runner.observe(acknowledged_through=pending["acknowledged_through"])
    else:
        runner.ledger.reconcile(session, nonce, {"data": [], "nextCursor": None},
                                pending["acknowledged_through"])
        value = runner.ledger.read()["sessions"][session]
        intent = value["intent"]
        result = {"paused": intent["state"] in ("invoking", "uncertain") or "pause_reason" in intent,
                  "diagnostic": ("codex-uncertain-" + nonce)
                  if intent["state"] in ("invoking", "uncertain") else None}
    state = runner.ledger.read()["sessions"][session]
    intent = state["intent"]
    require(intent["nonce"] == nonce, "offer changed during reconcile")
    return {"outcome": OUTCOME.get(intent["state"], "settled"), "state": intent["state"],
            "paused": result["paused"], "diagnostic": result["diagnostic"],
            "accepted_through": state["accepted_through"], "acknowledged_through": state["acknowledged_through"],
            "evidence": {key: intent.get(key) for key in (
                "after_seq", "through_seq", "body_hash", "queue_id", "queue_client_id", "receipt",
                "scan_caught_up", "pause_reason", "possibly_unattended", "custody_epoch")}}


def reconcile_control(path, request):
    """attended.codex.reconcile: the tick's own evidence reduction, on demand.

    It cannot force acceptance, discard an uncertain offer or replay one; an
    unresolved offer stays paused.
    """
    runner = bound_runner(path)
    record, session, nonce = operator_check(runner, request, current=True)
    return evidence_control(runner, record, session, nonce, current=True)


def resolve_control(path, request):
    """attended.codex.resolve: evidence first, then the audited operator reducer.

    Works for the current session's offer and for an older UUID's outstanding
    one (its pane is gone), always under current custody. Nothing is deleted.
    """
    runner = bound_runner(path)
    record, session, nonce = operator_check(runner, request, current=False)
    evidence = evidence_control(runner, record, session, nonce, current=session == record["session"])
    if evidence["outcome"] == "settled":
        return {"resolved": False, "outcome": "settled", "state": evidence["state"], "evidence": evidence}
    reason = request.get("reason")
    result = runner.ledger.resolve_operator(session, nonce, request.get("operation"), reason,
                                            time.time_ns() // 1_000_000, caller_role="operator")
    return {"resolved": True, "outcome": "settled", "state": result["state"], "audit": result["audit"],
            "warning": LATE}


def status_control(path):
    """Local durable diagnostics only: no daemon, presence, reservation or reset."""
    runner = Runner(path)
    with locked(runner.registry.parent):
        record = load(runner.registry)
        value = runner.ledger.read()
        a = accounting(value)
        refusal = a.get("pre_spawn_fence")
        if refusal and refusal["context"] != {key: record.get(key) for key in refusal["context"]}:
            a.pop("pre_spawn_fence")  # Read-only view of a superseded diagnostic.
        state = value["sessions"].get(record.get("session"), {})
        intent = state.get("intent", {})
        uncertain = intent.get("state") in ("invoking", "uncertain")
        return {"diagnostic": ("codex-uncertain-" + intent["nonce"]) if uncertain else (
                    diagnostic(a) or ("codex-turn-budget-exhausted" if "budget_exhausted" in a else None)),
                "paused": uncertain or state.get("native_stop", False) or a["human_paused"]
                          or a["declines"] >= K or diagnostics_pause(a) or "budget_exhausted" in a,
                "diagnostics": list(a["sticky"]), "pre_spawn_fence": a.get("pre_spawn_fence"),
                "pause_reason": a.get("thread_fence_reason") or (
                    FENCE_CLEARS_WHEN if a.get("pre_spawn_fence") and not a["sticky"]
                    else intent.get("pause_reason"))}


def main(argv):
    if len(argv) == 3 and argv[1] == "tick":
        try:
            tick(argv[2])
        except Exception:  # Bounded pass: diagnostics live in the ledger, not the pane.
            return 1
        return 0
    if len(argv) == 3 and argv[1] in ("status", "reconcile", "resolve"):
        # The Rust operator control's child: a bounded JSON request on stdin,
        # one JSON answer on stdout. No diagnostic reaches a pane.
        try:
            request = json.loads(sys.stdin.read(LIMIT))
            if argv[1] == "status":
                result = status_control(argv[2])
            else:
                handler = reconcile_control if argv[1] == "reconcile" else resolve_control
                result = handler(argv[2], request)
            print(encode(result))
        except Exception as error:
            print(encode({"error": str(error)[:512] or type(error).__name__}))
            return 1
        return 0
    print("usage: codex_runner.py tick|status|reconcile|resolve <launch.json>", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
