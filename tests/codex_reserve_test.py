#!/usr/bin/env python3
"""Hermetic ACT2 session reservation tests: a stand-in daemon, no native Codex.

The real daemon path, a real `codex resume` attach and zero provider requests
are proved by scripts/codex-reserve-spike.py, not here.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
import codex_reserve as reserve
from codex_socket import process_identity


class ReserveTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="codex-reserve-", dir=os.environ.get("TMPDIR", "/var/tmp"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.cwd = self.root / "alpha"
        self.path = self.cwd / ".codex/privateer-attended/reservation.json"
        self.path.parent.mkdir(parents=True, mode=0o700)
        (self.cwd / ".codex").chmod(0o700)
        self.codex = self.root / "codex"
        (self.codex / "sessions").mkdir(parents=True)
        self.request = {"ship": "alpha", "cwd": str(self.cwd), "codex_home": str(self.codex),
                        "model": None, "name": "alpha first mate", "current": None}
        self.calls, self.threads = [], {}
        self.daemon = {"pid": 7, "identity": {"bootId": "alpha", "startTicks": 7}}
        self.turns, self.extra_record, self.history = [], None, "legacy"
        fixture = self

        class Stand:
            def __init__(self, home, timeout=3):
                self.pid, self.identity = fixture.daemon["pid"], fixture.daemon["identity"]
            def __enter__(self):
                return self
            def __exit__(self, *args):
                pass
            def call(self, method, params):
                fixture.calls.append(method)
                if method == "thread/start":
                    session = str(uuid.uuid4())
                    path = fixture.codex / f"sessions/rollout-{session}.jsonl"
                    fixture.threads[session] = path
                    thread = {"id": session, "sessionId": session, "historyMode": fixture.history,
                              "ephemeral": False, "cwd": params["cwd"], "turns": [], "path": str(path)}
                    return {"thread": thread, "model": params.get("model", "alpha-default"),
                            "modelProvider": "fixture", "reasoningEffort": None}
                if method == "thread/name/set":
                    path = fixture.threads[params["threadId"]]
                    with path.open("w") as out:
                        out.write(json.dumps({"type": "session_meta", "payload": {
                            "id": params["threadId"], "cwd": str(fixture.cwd), "history_mode": "legacy"}}) + "\n")
                        if fixture.extra_record:
                            out.write(json.dumps(fixture.extra_record) + "\n")
                    return {}
                if method == "thread/read":
                    session = params["threadId"]
                    return {"thread": {"id": session, "cwd": str(fixture.cwd), "historyMode": "legacy",
                                       "ephemeral": False, "turns": fixture.turns, "status": {"type": "idle"},
                                       "path": str(fixture.threads[session])}}
                raise AssertionError(method)
        stand = patch.object(reserve, "Reserver", Stand)
        stand.start()
        self.addCleanup(stand.stop)

    def saved(self):
        return json.loads(self.path.read_text())

    def dead_owner(self, **fields):
        # A real exited process: its identity can never match again.
        child = subprocess.Popen([sys.executable, "-c", "pass"])
        identity = process_identity(child.pid)
        child.wait()
        record = {"version": 1, "generation": str(uuid.uuid4()), "pid": child.pid, "process_identity": identity,
                  "started_ms": 1, "ship": "alpha", "cwd": str(self.cwd), "codex_home": str(self.codex),
                  "model": None, "kept": [], **fields}
        reserve.atomic(self.path, record)
        return record

    def test_reserves_durable_uuid_first_then_verifies_zero_turn_attachability(self):
        order = []
        original = reserve.update
        def spy(path, generation, **fields):
            order.append((fields.get("state"), list(self.calls)))
            return original(path, generation, **fields)
        with patch.object(reserve, "update", spy):
            result = reserve.create(self.path, {**self.request, "model": "alpha-model"})
        # The UUID is committed right after thread/start, before name/set or read.
        self.assertEqual(order[0], ("reserved", ["thread/start"]))
        self.assertEqual(order[1][0], "verified")
        self.assertEqual(self.calls, ["thread/start", "thread/name/set", "thread/read"])
        saved = self.saved()
        self.assertEqual((saved["state"], saved["session"]), ("verified", result["session"]))
        self.assertEqual(saved["daemon"], self.daemon)
        self.assertEqual(result["settings"]["requested_model"], "alpha-model")
        self.assertEqual(result["settings"]["model"], "alpha-model")
        self.assertEqual(result["rollout"]["path"], str(self.threads[result["session"]]))
        self.assertIsNone(result["adopted"])

    def test_lost_create_response_is_kept_as_an_orphan_never_found_by_cwd(self):
        old = self.dead_owner(state="reserving")
        result = reserve.create(self.path, self.request)
        saved = self.saved()
        self.assertEqual(len(saved["kept"]), 1)
        self.assertEqual(saved["kept"][0]["generation"], old["generation"])
        self.assertIsNone(saved["kept"][0]["session"])
        self.assertEqual(self.calls.count("thread/start"), 1)
        self.assertEqual(saved["session"], result["session"])

    def test_crash_after_the_uuid_commit_continues_with_that_exact_uuid(self):
        session = str(uuid.uuid4())
        self.threads[session] = self.codex / f"sessions/rollout-{session}.jsonl"
        old = self.dead_owner(state="reserved", session=session, daemon=self.daemon)
        result = reserve.create(self.path, self.request)
        self.assertEqual(result["session"], session)
        self.assertEqual(result["adopted"], old["generation"])
        self.assertNotIn("thread/start", self.calls)

    def test_an_assigned_or_failed_reservation_is_kept_and_a_new_one_created(self):
        for state, current in (("verified", True), ("failed", False)):
            session = str(uuid.uuid4())
            self.dead_owner(state=state, session=session, daemon=self.daemon)
            self.calls.clear()
            result = reserve.create(self.path, {**self.request, "current": session if current else None})
            self.assertNotEqual(result["session"], session)
            self.assertEqual(self.calls[0], "thread/start")
            self.assertEqual(self.saved()["kept"][-1]["session"], session)

    def test_a_live_owner_refuses_and_a_full_history_needs_the_operator(self):
        reserve.atomic(self.path, {"version": 1, "generation": str(uuid.uuid4()), "state": "reserving",
                                   "pid": os.getpid(), "process_identity": process_identity(os.getpid()),
                                   "ship": "alpha", "cwd": str(self.cwd), "codex_home": str(self.codex),
                                   "kept": []})
        with self.assertRaisesRegex(ValueError, "in progress"):
            reserve.create(self.path, self.request)
        self.assertEqual(self.calls, [])
        self.dead_owner(state="reserving", kept=[{}] * reserve.MAX_KEPT)
        with self.assertRaisesRegex(ValueError, "operator cleanup"):
            reserve.create(self.path, self.request)
        self.assertEqual(self.calls, [])
        # A different ship/home/cwd record is never reused or overwritten.
        self.dead_owner(state="reserving", ship="beta")
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            reserve.create(self.path, self.request)

    def test_daemon_replacement_and_unqualified_threads_refuse_with_the_uuid_kept(self):
        cases = [("daemon", "replaced"), ("turns", "zero-turn"), ("record", "zero-turn"),
                 ("history", "durable legacy")]
        for case, message in cases:
            self.path.unlink(missing_ok=True)
            self.turns, self.extra_record, self.history = [], None, "legacy"
            if case == "turns":
                self.turns = [{"id": "alpha-turn"}]
            elif case == "record":
                self.extra_record = {"type": "event_msg", "payload": {"type": "task_started"}}
            elif case == "history":
                self.history = "paginated"
            original = reserve.Reserver
            if case == "daemon":
                class Replaced(original):
                    starts = []
                    def __init__(inner, home, timeout=3):
                        super().__init__(home, timeout)
                        Replaced.starts.append(1)
                        if len(Replaced.starts) > 1:
                            inner.pid = 8
                patcher = patch.object(reserve, "Reserver", Replaced)
            else:
                patcher = patch.object(reserve, "Reserver", original)
            with patcher, self.assertRaisesRegex(ValueError, message):
                reserve.create(self.path, self.request)
            saved = self.saved()
            self.assertEqual(saved["state"], "reserved")
            self.assertIn(saved["session"], self.threads)

    def test_child_protocol_marks_failures_and_unsent_attempts(self):
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": str(self.root),
               "CODEX_HOME": str(self.codex)}
        script = ROOT / "plugins/attended-codex/codex_reserve.py"
        run = lambda request: subprocess.run(["/usr/bin/python3", "-I", "-B", str(script), "create", str(self.path)],
                                             cwd=self.cwd, env=env, input=json.dumps(request), text=True,
                                             capture_output=True, timeout=30)
        result = run({**self.request, "cwd": "/beta"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("cwd mismatch", json.loads(result.stdout)["error"])
        self.assertFalse(self.path.exists())
        # No daemon: nothing was sent, so nothing is kept as an orphan.
        result = run(self.request)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("error", json.loads(result.stdout))
        saved = self.saved()
        self.assertEqual(saved["state"], "unsent")
        result = run(self.request)
        self.assertEqual(self.saved()["kept"], [])


if __name__ == "__main__":
    unittest.main()
