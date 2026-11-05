#!/usr/bin/env python3
"""Dormant runner boundary tests: owned processes only, no Codex/model calls."""
import copy
import fcntl
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "plugins/attended-codex"))
import codex_binding as binding
import codex_receipts as receipts
import codex_mail
import codex_runner as runner
from codex_socket import load, process_identity


@unittest.skipUnless(sys.platform == "linux" and hasattr(os, "pidfd_open"), "Linux custody only")
class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="codex-runner-", dir=os.environ.get("TMPDIR", "/var/tmp"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.directory = self.root / ".codex/privateer-attended"
        self.directory.mkdir(parents=True, mode=0o700)
        self.registry = self.directory / "launch.json"
        self.env = {}
        for name in binding.PATH_ENV:
            if name == "PRIVATEER_SERVICE":
                continue
            path = self.root / name.lower()
            path.mkdir()
            self.env[name] = str(path)
        environment = patch.dict(os.environ, self.env)
        environment.start()
        self.addCleanup(environment.stop)
        self.pane = self.process()
        self.supervisor = self.process()
        self.daemon_process = self.process()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.root / "socket"))
        os.chmod(self.root / "socket", 0o600)
        self.addCleanup(self.listener.close)
        (self.root / "rendezvous").symlink_to(self.root / "socket")
        codex = Path(self.env["CODEX_HOME"])
        (codex / "app-server-daemon").mkdir(mode=0o700)
        binding.atomic(codex / "app-server-daemon/daemon.pid", {
            "pid": self.daemon_process.pid, "processIdentity": process_identity(self.daemon_process.pid)})
        self.session = str(uuid.uuid4())
        self.loaded_sessions = [self.session]
        self.other_threads = {}
        self.status = {"type": "idle"}
        token, custody_token = str(uuid.uuid4()), str(uuid.uuid4())
        self.rollout = codex / "rollout.jsonl"
        self.rollout.write_text(json.dumps({"type": "session_meta", "payload": {
            "id": self.session, "cwd": str(self.root), "history_mode": "legacy"}}) + "\n")
        info = self.rollout.stat()
        self.record = {"generation": str(uuid.uuid4()), "callback_generation": str(uuid.uuid4()),
            "session": self.session, "holder": "host:alpha", "ship": "alpha", "home": self.env["HOME"],
            "cwd": str(self.root), "codex_home": str(codex), "pid": self.pane.pid,
            "process_identity": process_identity(self.pane.pid), "executable": runner.executable(self.pane.pid)["path"],
            "state": "bound", "hosted": True, "delivery_enabled": False, "path_env": self.env,
            "control": "/opt/privateer",
            "binding": {"session": self.session, "fence_version": 1, "invalidation_token": token,
                "history_mode": "legacy", "transcript": str(self.rollout),
                "transcript_identity": {"device": info.st_dev, "inode": info.st_ino},
                "daemon_pid": self.daemon_process.pid, "daemon_identity": process_identity(self.daemon_process.pid),
                "socket": str(self.root / "socket"), "rendezvous": str(self.root / "rendezvous")}}
        info = (self.root / "socket").stat()
        self.custody = {**{k: self.record[k] for k in ("generation", "session", "holder", "ship", "home", "cwd",
            "codex_home", "pid", "process_identity")}, "version": 1, "state": "owned", "token": custody_token,
            "epoch": str(uuid.uuid4()), "executable": runner.executable(self.pane.pid),
            "supervisor_pid": self.supervisor.pid, "supervisor_identity": process_identity(self.supervisor.pid),
            "supervisor_executable": runner.executable(self.supervisor.pid),
            "daemon": {"pid": self.daemon_process.pid, "identity": process_identity(self.daemon_process.pid),
                "executable": runner.executable(self.daemon_process.pid), "device": info.st_dev, "inode": info.st_ino,
                "socket": str(self.root / "socket"), "rendezvous": str(self.root / "rendezvous")}}
        binding.atomic(self.registry, self.record)
        binding.atomic(self.directory / "custody.json", self.custody)
        binding.atomic(self.directory / "invalidate.json", {"token": token})
        binding.atomic(self.directory / "custody-poison.json", {"token": custody_token, "generation": self.record["generation"]})
        self.runner = runner.Runner(self.registry)
        self.prepare()
        # A native-boundary stand-in, invoked as the exact recorded executable
        # plus argv `queue ...`; never touches a provider, daemon, or auth file.
        (self.root / "queue").write_text("import json, os, sys\nfrom pathlib import Path\n"
            "Path('invoked.json').write_text(json.dumps({'argv':sys.argv, 'env':dict(os.environ)}))\n")
        fixture = self
        class FakeDaemon:
            def __init__(self, home):
                self.pid = fixture.daemon_process.pid
                self.identity = process_identity(self.pid)
                self.target = fixture.root / "socket"
                self.rendezvous = fixture.root / "rendezvous"
            def __enter__(self):
                return self
            def __exit__(self, *args):
                pass
            def check(self):
                runner.native_check(fixture.record, fixture.custody)
            def call(self, method, params):
                with binding.locked(fixture.directory):
                    pass  # Both loaded-thread fences must run outside flock.
                if method == "thread/loaded/list":
                    return {"data": list(fixture.loaded_sessions), "nextCursor": None}
                if method == "thread/read":
                    thread = fixture.other_threads.get(params["threadId"], {
                        "id": fixture.session, "cwd": str(fixture.root), "historyMode": "legacy",
                        "status": fixture.status})
                    return {"thread": copy.deepcopy(thread)}
                if method == "thread/queue/list":
                    return {"data": [], "nextCursor": None}
                raise AssertionError(method)
        self.daemon_class = FakeDaemon
        daemon_patch = patch.object(runner, "Daemon", FakeDaemon)
        daemon_patch.start()
        self.addCleanup(daemon_patch.stop)
        # A stand-in for the Rust grant: per-nonce idempotent, never refunded.
        self.grant = {"grant_id": str(uuid.uuid4()), "session": self.session, "budget": 200, "spent": 0}
        self.reservations = {}
        self.calls = []
        self.pending = {"address": "alpha", "after_seq": 0, "acknowledged_through": 0, "count": 0,
                        "through_seq": 0, "actionable": 0, "headers": [], "attention": None}
        def control(record, command, args):
            self.calls.append((command, copy.deepcopy(args)))
            with binding.locked(self.directory):
                pass  # Runtime RPC must NEVER run under the project fence.
            if command == "ship.presence":
                return {'attended': True, 'lease': {'holder': 'host:alpha', 'expires_ms': 10**18}}
            if command == "attended.codex.status":
                grant = self.grant and {**self.grant, "remaining": self.grant["budget"] - self.grant["spent"]}
                return {"ship": "alpha", "grant": grant}
            if command == "attended.codex.reserve":
                if not self.grant or args["grant_id"] != self.grant["grant_id"]:
                    raise ValueError("Privateer binding report refused")
                if args["nonce"] not in self.reservations:
                    if self.grant["spent"] >= self.grant["budget"]:
                        raise ValueError("Privateer binding report refused")
                    self.grant["spent"] += 1
                    self.reservations[args["nonce"]] = self.grant["spent"]
                return {"reserved": True, "grant_id": self.grant["grant_id"], "session": args["session"],
                        "nonce": args["nonce"], "budget": self.grant["budget"],
                        "spent": self.grant["spent"], "remaining": self.grant["budget"] - self.grant["spent"]}
            if command == "mail.pending":
                return copy.deepcopy(self.pending)
            if command == "wake.hosted":
                return {"recorded": True}
            raise AssertionError(command)
        control_patch = patch.object(runner, 'control', side_effect=control)
        control_patch.start()
        self.addCleanup(control_patch.stop)
        self.runner.reserve(self.record)

    def process(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(90)"],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def cleanup():
            if child.poll() is None:
                child.kill()
            child.wait()
        self.addCleanup(cleanup)
        return child

    def prepare(self, after=0, reserve=True):
        self.nonce = str(uuid.uuid4())
        self.body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, self.nonce, after, after + 1)) + "\nNotification, not acknowledgment."
        intent = self.runner.ledger.prepare(self.record, self.body, after, after + 1, self.nonce)
        if reserve and hasattr(self, "grant"):
            self.runner.reserve(self.record)
        return intent

    def mutate(self, change):
        return self.runner.ledger.transaction(change)

    def event(self, kind, **fields):
        with self.rollout.open("a") as stream:
            stream.write(json.dumps({"type": "event_msg", "payload": {"type": kind, **fields}}) + "\n")

    def start(self, turn="turn", at=123):
        self.event("task_started", turn_id=turn, started_at=at,
                   turn_attribution={"turn_id": turn, "turn_trigger": "queue"})

    def promoted(self):
        self.start()
        self.event("user_message", message=self.body, client_id=str(uuid.uuid4()),
                   images=[], local_images=[], audio=[], local_audio=[], text_elements=[])
        self.event("task_complete", turn_id="turn")

    def assert_not_invoked(self):
        self.assertFalse((self.root / "invoked.json").exists())

    def test_idle_new_refuses_dispatch_then_first_turn_poison_holds(self):
        new_session = str(uuid.uuid4())
        self.loaded_sessions.append(new_session)
        self.other_threads[new_session] = {"id": new_session, "cwd": self.record["cwd"],
                                          "historyMode": "legacy", "status": {"type": "idle"}}
        original = self.registry.read_bytes()
        ledger = self.runner.ledger.read()
        poison_path = self.directory / "custody-poison.json"
        poison = poison_path.read_bytes()
        # Idle /new has not fired a callback: binding and custody still look
        # current, but the pane is no longer watching the bound UUID.
        with patch.object(runner.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "concurrent Codex threads in this cwd"):
                self.runner.dispatch()
            spawn.assert_not_called()  # Neither the old nor the new UUID.
        self.assert_not_invoked()
        self.assertEqual(self.registry.read_bytes(), original)
        self.assertEqual(poison_path.read_bytes(), poison)
        self.assertEqual(self.runner.ledger.read(), ledger)

        # The first turn in the new UUID emits the trusted SessionStart. It
        # must revoke old custody, never silently rebind or transfer the offer.
        event = {"session_id": new_session, "cwd": self.record["cwd"],
                 "hook_event_name": "SessionStart", "source": "startup"}
        with patch.object(binding, "Daemon", return_value=self.daemon_class(None)):
            with self.assertRaisesRegex(ValueError, "fresh/fork/clear"):
                binding.bind(self.registry, event)
        self.assertEqual(load(poison_path)["reason"], "callback_uuid_changed")
        self.assertEqual(load(self.registry)["state"], "unbound")
        self.assertEqual(load(self.registry)["session"], self.session)
        with self.assertRaisesRegex(ValueError, "custody poisoned"):
            runner.custody_check(self.registry, self.record, self.custody)
        # Even if the new thread unloads, callback invalidation blocks an old
        # offer, including after a runner restart.
        self.loaded_sessions = [self.session]
        for consumer in (self.runner, runner.Runner(self.registry)):
            with self.assertRaisesRegex(ValueError, "binding changed or disabled"):
                consumer.dispatch()
        self.assertEqual(set(self.runner.ledger.read()["sessions"]), {self.session})
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "prepared")
        self.assert_not_invoked()

    def test_only_loaded_thread_in_cwd_must_be_the_bound_uuid(self):
        new_session = str(uuid.uuid4())
        self.loaded_sessions.append(new_session)
        self.other_threads[self.session] = {"id": self.session, "cwd": str(self.root / "elsewhere")}
        self.other_threads[new_session] = {"id": new_session, "cwd": self.record["cwd"],
                                          "historyMode": "legacy", "status": {"type": "idle"}}
        with patch.object(runner.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(ValueError, "loaded thread for this cwd is not the bound UUID"):
                self.runner.dispatch()
            spawn.assert_not_called()
        self.assertEqual(load(self.registry)["session"], self.session)
        self.assert_not_invoked()

    def test_loaded_list_requires_explicit_end_of_bounded_page(self):
        for reply in ({"data": [self.session]},
                      {"data": [self.session], "nextCursor": "more"},
                      {"data": [self.session] + [str(uuid.uuid4()) for _ in range(32)],
                       "nextCursor": None}):
            with self.subTest(reply=reply):
                original = self.daemon_class.call
                def call(daemon, method, params):
                    if method == "thread/loaded/list":
                        return reply
                    return original(daemon, method, params)
                ledger = self.runner.ledger.read()
                with patch.object(self.daemon_class, "call", new=call), patch.object(runner.subprocess, "Popen") as spawn:
                    with self.assertRaisesRegex(ValueError, "thread not loaded"):
                        self.runner.dispatch()
                    spawn.assert_not_called()
                self.assertEqual(self.runner.ledger.read(), ledger)
                self.assert_not_invoked()

    def test_new_between_first_fence_and_spawn_sticks_across_recovery(self):
        new_session = str(uuid.uuid4())
        original = runner.control
        def control(record, command, args):
            if command == "ship.presence":
                # Dispatch's first loaded-list fence already passed; /new has
                # no callback until the user submits a turn in the new UUID.
                self.loaded_sessions.append(new_session)
                self.other_threads[new_session] = {"id": new_session, "cwd": self.record["cwd"]}
            return original(record, command, args)
        original_drain = runner.drain
        def drain(child):
            # A crash or delay in cleanup must not lose the detected custody gap.
            a = self.runner.ledger.read()["runner"]
            self.assertIn(runner.THREAD_FENCE_LOST, a["sticky"])
            self.assertIn("concurrent Codex threads in this cwd", a["thread_fence_reason"])
            return original_drain(child)
        with patch.object(runner, "control", side_effect=control), patch.object(runner, "drain", side_effect=drain):
            result = self.runner.dispatch()
        self.assertTrue((self.root / "invoked.json").exists())
        invoked = json.loads((self.root / "invoked.json").read_text())["argv"]
        self.assertEqual(invoked[invoked.index("--thread") + 1], self.session)
        self.assertEqual(load(self.registry), self.record)  # Never silently rebind.
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "uncertain")
        self.assertEqual(result["possibly_unattended_spent"], 1)
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        self.assertIn(runner.THREAD_FENCE_LOST, result["diagnostics"])
        self.assertIn("concurrent Codex threads in this cwd", result["pause_reason"])
        self.runner.publish(self.record, result)
        self.assertFalse(any(command == "wake.hosted" for command, _ in self.calls))

        # Unloading the new UUID and restarting do not erase the sticky pause.
        self.loaded_sessions = [self.session]
        restarted = runner.Runner(self.registry)
        result = restarted.observe()
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        # Preserve positive native facts without reporting watched delivery or
        # automatically resuming mail when the old offer becomes settled.
        self.promoted()
        result = restarted.observe()
        self.assertEqual(result["accepted_through"], 1)
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        self.assertIn("concurrent Codex threads in this cwd", result["pause_reason"])
        restarted.publish(self.record, result)
        self.assertFalse(any(command == "wake.hosted" for command, _ in self.calls))
        with patch.object(runner.subprocess, "Popen") as spawn:
            self.assertEqual(runner.tick(self.registry), {"tick": "idle"})
            self.prepare(1)
            with self.assertRaisesRegex(ValueError, "runner paused"):
                restarted.dispatch()
            spawn.assert_not_called()
        self.assertEqual(restarted.report(self.session)["possibly_unattended_spent"], 1)

    def test_post_spawn_fence_refuses_changed_cwd_uuid(self):
        new_session = str(uuid.uuid4())
        original = subprocess.Popen
        def spawn(*args, **kwargs):
            self.loaded_sessions.append(new_session)
            self.other_threads[self.session] = {"id": self.session, "cwd": str(self.root / "elsewhere")}
            self.other_threads[new_session] = {"id": new_session, "cwd": self.record["cwd"]}
            return original(*args, **kwargs)
        with patch.object(runner.subprocess, "Popen", side_effect=spawn):
            result = self.runner.dispatch()
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        self.assertIn("loaded thread for this cwd is not the bound UUID", result["pause_reason"])

    def test_post_spawn_fence_failure_still_drains_child(self):
        original_spawn = subprocess.Popen
        original_call = self.daemon_class.call
        children = []
        def spawn(*args, **kwargs):
            child = original_spawn(*args, **kwargs)
            children.append(child)
            return child
        def call(daemon, method, params):
            # Failure is specifically in the second loaded-list fence.
            if children and method == "thread/loaded/list":
                raise ConnectionError("fixture post-spawn disconnect")
            return original_call(daemon, method, params)
        with patch.object(runner.subprocess, "Popen", side_effect=spawn), patch.object(self.daemon_class, "call", new=call):
            result = self.runner.dispatch()
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode)
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        self.assertIn("fixture post-spawn disconnect", result["pause_reason"])
        self.assertEqual(result["possibly_unattended_spent"], 1)

    def test_post_spawn_fence_allows_own_turn_to_be_active(self):
        original = subprocess.Popen
        def spawn(*args, **kwargs):
            self.status = {"type": "active"}
            return original(*args, **kwargs)
        with patch.object(runner.subprocess, "Popen", side_effect=spawn):
            result = self.runner.dispatch()
        self.assertTrue(result["report_eligible"])
        self.assertNotIn(runner.THREAD_FENCE_LOST, result["diagnostics"])
        self.assertIsNone(result["pause_reason"])

    def test_loaded_threads_in_other_cwds_do_not_block_bound_dispatch(self):
        other_session = str(uuid.uuid4())
        self.loaded_sessions.append(other_session)
        self.other_threads[other_session] = {"id": other_session, "cwd": str(self.root / "elsewhere")}
        self.runner.dispatch()
        invoked = json.loads((self.root / "invoked.json").read_text())["argv"]
        self.assertEqual(invoked[invoked.index("--thread") + 1], self.session)

    def test_no_grant_reservation_and_b0_fence_cannot_deliver(self):
        # launch.json's delivery_enabled is not authority; only the pinned
        # captain-grant reservation for this exact offer is.
        self.runner.ledger.reconcile(self.session, self.nonce, {"data": [], "nextCursor": None}, 1)
        self.prepare(1, reserve=False)
        with self.assertRaisesRegex(ValueError, "not granted"):
            self.runner.dispatch()
        self.assert_not_invoked()
        with self.assertRaisesRegex(ValueError, "not granted"):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()
        self.runner.reserve(self.record)
        self.record["binding"].pop("fence_version")
        binding.atomic(self.registry, self.record)
        with self.assertRaisesRegex(ValueError, "fence"):
            self.runner.dispatch()
        self.assert_not_invoked()

    def test_real_spawn_under_flock_exact_argv_allowlisted_env_and_uncertain_result(self):
        original = subprocess.Popen
        def spawn(argv, **kwargs):
            fd = os.open(self.directory / "lock", os.O_RDWR)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                os.close(fd)
            value = self.runner.ledger.read()
            self.assertEqual(value["sessions"][self.session]["intent"]["state"], "invoking")
            self.assertEqual(value["runner"]["possibly_unattended_spent"], 1)
            child = original(argv, **kwargs)
            wait = child.wait
            def unlocked_wait(*args, **kw):
                with binding.locked(self.directory):
                    pass
                return wait(*args, **kw)
            child.wait = unlocked_wait
            return child
        with patch.dict(os.environ, {"OPENAI_API_KEY": "fixture-not-a-credential", "LD_PRELOAD": "bad"}), patch.object(runner.subprocess, "Popen", side_effect=spawn):
            result = self.runner.dispatch()
        invoked = json.loads((self.root / "invoked.json").read_text())
        self.assertEqual(invoked["argv"], ["queue", "--remote", "unix://" + str(self.root / "socket"),
                                          "--thread", self.session, "--message", self.body])
        self.assertNotIn("OPENAI_API_KEY", invoked["env"])
        self.assertNotIn("LD_PRELOAD", invoked["env"])
        self.assertEqual(result["possibly_unattended_spent"], 1)
        self.assertTrue(result["report_eligible"])
        self.assertTrue(result["paused"])
        self.assertEqual(result["diagnostic"], "codex-uncertain-" + self.nonce)
        self.assertEqual(result["accepted_through"], 0)
        with self.assertRaises(ValueError):
            self.runner.dispatch()

    def test_lost_response_immediate_queue_then_promotion_recovery_without_ack(self):
        self.runner.dispatch()
        queue = {"data": [{"id": "queue-item", "clientUserMessageId": str(uuid.uuid4()),
                          "input": [{"type": "text", "text": self.body, "text_elements": []}]}], "nextCursor": None}
        result = self.runner.ledger.reconcile(self.session, self.nonce, queue)
        self.assertEqual(result["state"], "queued")
        self.assertEqual(result["accepted_through"], 0)
        self.promoted()
        restarted = runner.Runner(self.registry)
        result = restarted.observe()
        self.assertEqual(result["accepted_through"], 1)
        self.assertEqual(result["possibly_unattended_spent"], 1)
        self.assertEqual(restarted.ledger.read()["sessions"][self.session]["acknowledged_through"], 0)

    def test_lost_response_absence_nonzero_and_elapsed_time_never_prove_rejection(self):
        (self.root / "queue").write_text("raise SystemExit(7)\n")
        result = self.runner.dispatch()
        self.assertEqual(result["decline_count"], 0)
        with patch.object(runner.time, "time_ns", return_value=10**30):
            result = self.runner.observe()
            self.assertTrue(result["paused"])
            self.assertEqual(result["diagnostic"], "codex-uncertain-" + self.nonce)
            with self.assertRaises(ValueError):
                self.runner.dispatch()
        self.assertEqual(result["accepted_through"], 0)

    def test_partial_and_covering_ack_never_refund_possibly_unattended_budget(self):
        self.runner.dispatch()
        result = self.runner.observe(acknowledged_through=0)
        self.assertTrue(result["paused"])
        result = self.runner.observe(acknowledged_through=1)
        self.assertEqual(result["accepted_through"], 1)
        self.assertEqual(result["possibly_unattended_spent"], 1)
        self.assertFalse(result["busy"])

    def test_covering_runtime_ack_settles_even_after_daemon_death(self):
        self.runner.dispatch()
        self.mutate(lambda v: v['runner'].update(declines=3))
        self.daemon_process.kill()
        self.daemon_process.wait()
        result = self.runner.observe(acknowledged_through=1)
        self.assertEqual(result['accepted_through'], 1)
        self.assertEqual(result['decline_count'], 0)
        self.assertEqual(result['possibly_unattended_spent'], 1)
        self.assertNotEqual(result['diagnostic'], 'codex-uncertain-' + self.nonce)
        state = self.runner.ledger.read()['sessions'][self.session]
        self.assertEqual(state['intent']['state'], 'acked')
        self.assertEqual(state['acknowledged_through'], 1)
        self.assertFalse(result['report_eligible'])

    def test_three_invocations_exhaust_budget_even_with_real_progress_and_restart(self):
        self.grant["budget"] = 3
        for i in range(3):
            self.runner.dispatch()
            self.promoted()
            result = self.runner.observe()
            self.assertEqual(result["possibly_unattended_spent"], i + 1)
            self.assertEqual(result["accepted_through"], i + 1)
            if i < 2:
                self.prepare(i + 1)
            self.runner = runner.Runner(self.registry)
        with self.assertRaisesRegex(ValueError, "refused"):
            self.prepare(3)
        with self.assertRaisesRegex(ValueError, "not granted"):
            self.runner.dispatch()
        self.assertEqual(self.runner.report(self.session)["diagnostic"], "codex-turn-budget-exhausted")
        self.assertTrue(self.runner.report(self.session)["paused"])
        self.assert_grant_floor_holds_against_a_reset_runtime()

    def assert_grant_floor_holds_against_a_reset_runtime(self):
        # A runtime that forgot its count cannot widen the grant locally.
        self.grant["spent"] = 0
        self.runner.ledger.transaction(lambda v: runner.accounting(v).pop("budget_exhausted", None))
        self.runner.reserve(self.record)
        with self.assertRaisesRegex(ValueError, "budget exhausted"):
            self.runner.dispatch()

    def test_exec_failure_only_rejection_retry_minute_and_k3_bound(self):
        for i in range(3):
            with patch.object(runner.subprocess, "Popen", side_effect=FileNotFoundError("fixture exec failed")):
                with self.assertRaises(FileNotFoundError):
                    self.runner.dispatch()
            value = self.runner.ledger.read()
            self.assertEqual(value["sessions"][self.session]["intent"]["state"], "rejected")
            self.assertEqual(value["runner"]["declines"], i + 1)
            # Exec failure never refunds the reserved possibly-unattended turn.
            self.assertEqual(value["runner"]["possibly_unattended_spent"], i + 1)
            if i < 2:
                self.prepare()
                with self.assertRaisesRegex(ValueError, "paused"):
                    self.runner.dispatch()
                self.mutate(lambda v: v["runner"].update(retry_after_ms=0))
        self.prepare()
        with self.assertRaises(ValueError):
            self.runner.dispatch()
        self.assert_not_invoked()

    def test_busy_start_is_native_and_stable_across_poll_restart_observation_loss(self):
        self.runner._observe_lifecycle(self.record)
        self.start(at=456)
        self.runner._observe_lifecycle(self.record)
        first = self.runner.report(self.session)
        self.assertTrue(first["busy"])
        self.assertEqual(first["turn_started_ms"], 456000)
        self.runner = runner.Runner(self.registry)
        self.runner._observe_lifecycle(self.record)
        self.assertEqual(self.runner.report(self.session)["turn_started_ms"], 456000)
        with patch.object(runner, "lifecycle", side_effect=ValueError("fixture gap")):
            self.runner._observe_lifecycle(self.record)
        result = self.runner.report(self.session)
        self.assertTrue(result["busy"])
        self.assertFalse(result["busy_available"])
        self.assertEqual(result["turn_started_ms"], 456000)
        self.event("turn_aborted", turn_id="turn")
        self.runner._observe_lifecycle(self.record)
        result = self.runner.report(self.session)
        self.assertFalse(result["busy"])
        self.assertIsNone(result["turn_started_ms"])

    def test_busy_unknown_paginated_and_unloaded_refuse(self):
        self.runner._observe_lifecycle(self.record)
        self.start()
        with self.assertRaisesRegex(ValueError, "busy|unavailable"):
            self.runner.dispatch()
        self.assert_not_invoked()
        self.record["binding"]["history_mode"] = "paginated"
        self.runner._observe_lifecycle(self.record)
        # Changing history settings is never an adapter remedy.
        with self.assertRaises(ValueError):
            runner.lifecycle({}, self.record)

    def test_poison_and_identity_substitution_barriers_before_spawn(self):
        self.runner._observe_lifecycle(self.record)
        for field in ("pid", "process_identity", "supervisor_identity", "executable", "supervisor_executable"):
            altered = copy.deepcopy(self.custody)
            if field == "pid":
                altered[field] = self.supervisor.pid
            elif "identity" in field:
                altered[field]["startTicks"] += 1
            else:
                altered[field]["inode"] += 1
            binding.atomic(self.directory / "custody.json", altered)
            with self.assertRaises(ValueError):
                self.runner._spawn(self.record, self.custody, self.daemon_class(None))
            self.assert_not_invoked()
        binding.atomic(self.directory / "custody.json", self.custody)
        binding.publish_request(self.registry, "custody-poison.json", reason="barrier")
        with self.assertRaisesRegex(ValueError, "poison"):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()

    def test_resume_callback_poison_survives_binding_recovery_and_runner_restart(self):
        self.runner._observe_lifecycle(self.record)
        binding.invalidate(self.registry, {"session_id": self.session, "source": "resume"})
        poison_path = self.directory / "custody-poison.json"
        poison = poison_path.read_bytes()
        self.assertEqual(load(poison_path)["reason"], "resume_callback_requires_relaunch")
        # Even a successful later binder can renew only the binding fence,
        # not the separately sticky custody capability.
        recovered = load(self.registry)
        recovered.update(state="bound", binding=copy.deepcopy(self.record["binding"]))
        recovered["binding"]["invalidation_token"] = binding.request_token(self.registry)
        binding.atomic(self.registry, recovered)
        for consumer in (self.runner, runner.Runner(self.registry)):
            with self.assertRaisesRegex(ValueError, "resume callback requires explicit hosted relaunch"):
                consumer._spawn(recovered, self.custody, self.daemon_class(None))
            self.assert_not_invoked()
            self.assertEqual(poison_path.read_bytes(), poison)
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "prepared")

    def test_exit_before_final_check_refuses_pane(self):
        self.runner._observe_lifecycle(self.record)
        self.pane.kill()
        self.pane.wait()
        with self.assertRaises((ValueError, ProcessLookupError, FileNotFoundError)):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()

    def test_supervisor_crash_with_live_pane_refuses_independently(self):
        self.runner._observe_lifecycle(self.record)
        self.supervisor.kill()
        self.supervisor.wait()
        self.assertIsNone(self.pane.poll())
        with self.assertRaises((ValueError, ProcessLookupError, FileNotFoundError)):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()

    def test_late_supervisor_exit_after_check_still_charges_and_never_claims_cancellation(self):
        original = subprocess.Popen
        def spawn(*args, **kwargs):
            self.supervisor.kill()
            self.supervisor.wait()
            return original(*args, **kwargs)
        with patch.object(runner.subprocess, "Popen", side_effect=spawn):
            result = self.runner.dispatch()
        self.assertTrue((self.root / "invoked.json").exists())
        self.assertEqual(result["possibly_unattended_spent"], 1)
        self.assertTrue(result["paused"])
        self.assertFalse(result["report_eligible"])
        self.assertEqual(result["accepted_through"], 0)
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "uncertain")

    def test_callback_generation_and_invalidation_barriers_and_stale_reports(self):
        self.runner._observe_lifecycle(self.record)
        changed = copy.deepcopy(self.record)
        changed["callback_generation"] = str(uuid.uuid4())
        binding.atomic(self.registry, changed)
        with self.assertRaisesRegex(ValueError, "changed"):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assertFalse(self.runner.report(self.session)["report_eligible"])
        binding.atomic(self.registry, self.record)
        binding.publish_request(self.registry)
        with self.assertRaisesRegex(ValueError, "fence"):
            self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()

    def test_independent_pidfd_checks_detect_start_and_executable_substitution(self):
        before = len(list(Path('/proc/self/fd').iterdir()))
        for field in ('process_identity', 'supervisor_identity', 'executable', 'supervisor_executable'):
            altered = copy.deepcopy(self.custody)
            altered[field]['startTicks' if 'identity' in field else 'inode'] += 1
            with self.assertRaisesRegex(ValueError, 'identity'):
                runner.Handles(altered)
        self.assertEqual(len(list(Path('/proc/self/fd').iterdir())), before)

    def test_prelock_poison_after_invoking_commit_refuses_actual_spawn(self):
        self.runner._observe_lifecycle(self.record)
        original = runner.save
        def save(ledger, value):
            original(ledger, value)
            if value['sessions'][self.session]['intent']['state'] == 'invoking':
                binding.publish_request(self.registry)
        with patch.object(runner, 'save', side_effect=save):
            with self.assertRaisesRegex(ValueError, 'fence'):
                self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()
        result = self.runner.report(self.session)
        self.assertEqual(result['possibly_unattended_spent'], 1)
        self.assertEqual(result['diagnostic'], 'codex-uncertain-' + self.nonce)
        self.assertFalse(result['report_eligible'])

    def test_late_callback_poison_is_not_remote_cancellation(self):
        original = subprocess.Popen
        def spawn(*args, **kwargs):
            binding.publish_request(self.registry)
            return original(*args, **kwargs)
        with patch.object(runner.subprocess, 'Popen', side_effect=spawn):
            result = self.runner.dispatch()
        self.assertTrue((self.root / 'invoked.json').exists())
        self.assertFalse(result['report_eligible'])
        self.assertTrue(result['paused'])
        self.assertEqual(result['possibly_unattended_spent'], 1)

    def test_daemon_replacement_pending_queue_unloaded_and_unknown_status_refuse(self):
        for method, response in [
            ('thread/loaded/list', {'data': [], 'nextCursor': None}),
            ('thread/read', {'thread': {'id': self.session, 'cwd': str(self.root), 'historyMode': 'legacy', 'status': {'type': 'unknown'}}}),
            ('thread/queue/list', {'data': [{'id': 'another-item'}], 'nextCursor': None})]:
            original = self.daemon_class.call
            def call(daemon, name, args):
                return response if name == method else original(daemon, name, args)
            with patch.object(self.daemon_class, 'call', new=call):
                with self.assertRaises(ValueError):
                    self.runner.dispatch()
            self.assert_not_invoked()
        binding.atomic(Path(self.record['codex_home']) / 'app-server-daemon/daemon.pid', {'pid': self.pane.pid})
        with self.assertRaises(ValueError):
            self.runner.dispatch()
        self.assert_not_invoked()

    def test_disconnect_preserves_busy_timestamp_and_marks_observation_unavailable(self):
        self.runner._observe_lifecycle(self.record)
        self.start(at=789)
        self.runner._observe_lifecycle(self.record)
        with patch.object(self.daemon_class, 'call', side_effect=ConnectionError('fixture disconnect')):
            result = self.runner.observe()
        self.assertTrue(result['busy'])
        self.assertFalse(result['busy_available'])
        self.assertEqual(result['turn_started_ms'], 789000)

    def test_native_abort_pauses_once_and_background_progress_does_not_resume(self):
        self.runner._observe_lifecycle(self.record)
        self.start()
        self.event('turn_aborted', turn_id='turn')
        self.runner._observe_lifecycle(self.record)
        self.assertEqual(self.runner.report(self.session)['decline_count'], 1)
        self.runner._observe_lifecycle(self.record)
        self.assertEqual(self.runner.report(self.session)['decline_count'], 1)
        self.runner.observe(acknowledged_through=1)
        self.assertTrue(self.runner.report(self.session)['paused'])
        self.prepare(1)
        with self.assertRaisesRegex(ValueError, 'native stop'):
            self.runner.dispatch()
        self.assert_not_invoked()

    def test_timeout_and_observed_escaped_descendant_cleanup_remain_uncertain(self):
        (self.root / 'queue').write_text("import subprocess, sys, time\nfrom pathlib import Path\n"
            "p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(90)'], start_new_session=True)\n"
            "Path('descendant.pid').write_text(str(p.pid))\ntime.sleep(90)\n")
        original = runner.drain
        with patch.object(runner, 'drain', side_effect=lambda child: original(child, timeout=0.3)):
            result = self.runner.dispatch()
        pid = int((self.root / 'descendant.pid').read_text())
        try:
            fd = os.pidfd_open(pid, 0)
        except ProcessLookupError:
            pass
        else:
            try:
                self.assertFalse(runner.live(fd))
            finally:
                os.close(fd)
        self.assertEqual(result['diagnostic'], 'codex-uncertain-' + self.nonce)
        self.assertEqual(result['decline_count'], 0)

    def test_normal_exit_still_kills_the_queue_childs_whole_process_group(self):
        # The leader forks a same-group member and exits at once: it is
        # reparented, so descendant sampling may never see it. Killing the
        # group while the zombie leader pins it must still stop it.
        (self.root / 'queue').write_text("import os, sys, time\nfrom pathlib import Path\n"
            "pid = os.fork()\n"
            "if pid == 0:\n    time.sleep(90)\n    os._exit(0)\n"
            "Path('member.pid').write_text(str(pid))\n")
        result = self.runner.dispatch()
        pid = int((self.root / 'member.pid').read_text())
        try:
            fd = os.pidfd_open(pid, 0)
        except ProcessLookupError:
            pass
        else:
            try:
                deadline = time.monotonic() + 2
                while runner.live(fd) and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertFalse(runner.live(fd))
            finally:
                os.close(fd)
        self.assertEqual(result['diagnostic'], 'codex-uncertain-' + self.nonce)

    def control_request(self, **fields):
        return {"ship": "alpha", "holder": "host:alpha", "session": self.session,
                "notification_id": self.nonce, **fields}

    def test_operator_reconcile_reports_uncertain_then_settles_only_on_evidence(self):
        self.runner.dispatch()
        result = runner.reconcile_control(self.registry, self.control_request())
        self.assertEqual((result["outcome"], result["state"]), ("uncertain", "uncertain"))
        self.assertTrue(result["paused"])
        self.assertEqual(result["diagnostic"], "codex-uncertain-" + self.nonce)
        self.assertTrue(result["evidence"]["possibly_unattended"])
        # Elapsed time and repeated reconciles never settle or replay it.
        again = runner.reconcile_control(self.registry, self.control_request())
        self.assertEqual(again["outcome"], "uncertain")
        self.assertEqual([c for c, _ in self.calls].count("attended.codex.reserve"), 1)
        self.promoted()
        result = runner.reconcile_control(self.registry, self.control_request())
        self.assertEqual((result["outcome"], result["state"]), ("settled", "presented"))
        self.assertEqual(result["accepted_through"], 1)

    def test_operator_controls_require_current_binding_custody_and_offer(self):
        self.runner.dispatch()
        for wrong in ({"holder": "host:beta"}, {"session": str(uuid.uuid4())}, {"ship": "beta"},
                      {"notification_id": str(uuid.uuid4())}, {"notification_id": "alpha"}):
            for handler in (runner.reconcile_control, runner.resolve_control):
                with self.assertRaises(ValueError):
                    handler(self.registry, self.control_request(operation="abandon", reason="alpha", **wrong))
        binding.publish_request(self.registry, "custody-poison.json", generation=self.record["generation"],
                                reason="holder_changed")
        with self.assertRaisesRegex(ValueError, "poisoned"):
            runner.resolve_control(self.registry, self.control_request(operation="abandon", reason="alpha"))
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "uncertain")

    def test_operator_resolve_audits_reason_keeps_evidence_and_warns_late_admission(self):
        self.runner.dispatch()
        for bad in ({"operation": "ack", "reason": "alpha"}, {"operation": "abandon", "reason": " "},
                    {"operation": "abandon", "reason": "x" * 513}):
            with self.assertRaises(ValueError):
                runner.resolve_control(self.registry, self.control_request(**bad))
        result = runner.resolve_control(self.registry, self.control_request(
            operation="abandon", reason="alpha queue drained by hand"))
        self.assertTrue(result["resolved"])
        self.assertEqual(result["state"], "abandoned")
        self.assertIn("Late native admission", result["warning"])
        audit = result["audit"]
        self.assertEqual((audit["actor"], audit["operation"], audit["reason"]),
                         ("operator", "abandon", "alpha queue drained by hand"))
        state = self.runner.ledger.read()["sessions"][self.session]
        self.assertEqual(state["intent"]["operator_resolution"], audit)
        self.assertEqual(state["intent"]["body"], self.body)  # Evidence kept.
        self.assertEqual((state["suppressed_through"], state["accepted_through"]), (1, 0))
        # A settled offer is not resolved twice; the first audit stands.
        again = runner.resolve_control(self.registry, self.control_request(operation="mark_presented", reason="beta"))
        self.assertEqual((again["resolved"], again["state"]), (False, "abandoned"))
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["operator_resolution"], audit)

    def old_uuid_intent(self, body=None, scan=None, session=None):
        old, old_nonce = session or str(uuid.uuid4()), str(uuid.uuid4())
        original = copy.deepcopy(self.runner.ledger.read()["sessions"][self.session]["intent"])
        body = body or original["body"]
        def change(value):
            value["sessions"][old] = {"accepted_through": 0, "acknowledged_through": 0, "intent": {
                **original, "nonce": old_nonce, "state": "uncertain", "body": body,
                "body_hash": receipts.digest(body.encode())}}
            if scan is not None:
                value["sessions"][old]["intent"]["scan"] = scan
                value["sessions"][old]["intent"].update(
                    open_turn=scan["open_turn"], open_turn_trigger=scan["open_turn_trigger"])
        self.mutate(change)
        return old, old_nonce

    def test_old_uuid_resolve_runs_mailbox_ack_evidence(self):
        self.runner.dispatch()
        old, old_nonce = self.old_uuid_intent()
        self.pending.update(acknowledged_through=1, through_seq=1)
        result = runner.resolve_control(self.registry, self.control_request(
            session=old, notification_id=old_nonce, operation="abandon", reason="alpha"))
        # Evidence wins: the covering ack settles the offer, so it is kept.
        self.assertFalse(result["resolved"])
        self.assertEqual((result["outcome"], result["state"]), ("settled", "acked"))
        state = self.runner.ledger.read()["sessions"][old]
        self.assertEqual((state["intent"]["state"], state["accepted_through"]), ("acked", 1))

    def test_old_uuid_resolve_runs_the_rollout_scan_on_an_empty_queue_page(self):
        self.runner.dispatch()
        old, old_nonce = str(uuid.uuid4()), str(uuid.uuid4())
        old_rollout = self.root / "old-rollout.jsonl"
        old_rollout.write_text(json.dumps({"type": "session_meta", "payload": {
            "id": old, "cwd": str(self.root), "history_mode": "legacy"}}) + "\n")
        info = old_rollout.stat()
        scan = receipts.anchor({"history_mode": "legacy", "transcript": str(old_rollout), "session": old,
                                "transcript_identity": {"device": info.st_dev, "inode": info.st_ino}},
                               str(self.root))
        old, old_nonce = self.old_uuid_intent(body=self.body, scan=scan, session=old)
        with old_rollout.open("a") as stream:
            for event in (
                    {"type": "task_started", "turn_id": "old-turn", "started_at": 123,
                     "turn_attribution": {"turn_id": "old-turn", "turn_trigger": "queue"}},
                    {"type": "user_message", "message": self.body, "client_id": str(uuid.uuid4()),
                     "images": [], "local_images": [], "audio": [], "local_audio": [], "text_elements": []},
                    {"type": "task_complete", "turn_id": "old-turn"}):
                stream.write(json.dumps({"type": "event_msg", "payload": event}) + "\n")
        result = runner.resolve_control(self.registry, self.control_request(
            session=old, notification_id=old_nonce, operation="abandon", reason="alpha"))
        self.assertFalse(result["resolved"])
        self.assertEqual((result["outcome"], result["state"]), ("settled", "presented"))
        self.assertEqual(self.runner.ledger.read()["sessions"][old]["accepted_through"], 1)

    def test_operator_resolve_prefers_native_evidence_and_reaches_an_old_uuid(self):
        self.runner.dispatch()
        self.promoted()
        result = runner.resolve_control(self.registry, self.control_request(
            operation="abandon", reason="alpha"))
        self.assertFalse(result["resolved"])
        self.assertEqual((result["outcome"], result["state"]), ("settled", "presented"))
        # An older UUID's outstanding offer blocks every new one; the operator
        # can resolve it under the CURRENT custody even though its pane is gone.
        old = str(uuid.uuid4())
        old_nonce = str(uuid.uuid4())
        def change(value):
            value["sessions"][old] = {"accepted_through": 0, "acknowledged_through": 0, "intent": {
                **value["sessions"][self.session]["intent"], "nonce": old_nonce, "state": "uncertain",
                "body": "alpha old", "body_hash": receipts.digest(b"alpha old")}}
        self.mutate(change)
        with self.assertRaisesRegex(ValueError, "current binding"):
            runner.reconcile_control(self.registry, self.control_request(session=old, notification_id=old_nonce))
        result = runner.resolve_control(self.registry, self.control_request(
            session=old, notification_id=old_nonce, operation="mark_presented", reason="alpha saw it"))
        self.assertEqual(result["state"], "operator_presented")
        self.assertEqual(self.runner.ledger.read()["sessions"][old]["accepted_through"], 1)

    def test_control_child_protocol_is_one_json_answer(self):
        import io
        for request, code in ((self.control_request(), 0), ({"ship": "beta"}, 1)):
            out = io.StringIO()
            with patch.object(sys, "stdin", io.StringIO(json.dumps(request))), patch.object(sys, "stdout", out):
                self.assertEqual(runner.main(["codex_runner.py", "reconcile", str(self.registry)]), code)
            answer = json.loads(out.getvalue())
            self.assertIn("error" if code else "outcome", answer)

    def test_invoking_commit_failure_never_creates_child(self):
        self.runner._observe_lifecycle(self.record)
        with patch.object(runner, 'save', side_effect=OSError('fixture fsync failure')):
            with self.assertRaises(OSError):
                self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()
        self.assertEqual(self.runner.ledger.read()['sessions'][self.session]['intent']['state'], 'prepared')

    def test_holder_change_and_lease_expiry_refuse_before_spawn(self):
        for presence in [
            {'attended': False, 'lease': {'holder': 'host:alpha', 'expires_ms': 10**18}},
            {'attended': True, 'lease': {'holder': 'host:other', 'expires_ms': 10**18}},
            {'attended': True, 'lease': {'holder': 'host:alpha', 'expires_ms': 1}}]:
            with patch.object(runner, 'control', return_value=presence):
                with self.assertRaisesRegex(ValueError, 'presence'):
                    self.runner.dispatch()
                self.assertFalse(self.runner.report(self.session)['report_eligible'])
            self.assert_not_invoked()

    def test_large_busy_scan_makes_bounded_forward_progress_without_reset(self):
        self.runner._observe_lifecycle(self.record)
        before = self.runner.ledger.read()['sessions'][self.session]['lifecycle']['scan']['offset']
        with self.rollout.open('a') as stream:
            for _ in range(1200):
                stream.write(json.dumps({'type': 'response_item', 'payload': {'text': 'x' * 1000}}) + '\n')
        self.runner._observe_lifecycle(self.record)
        mid = self.runner.ledger.read()['sessions'][self.session]['lifecycle']['scan']['offset']
        self.assertGreater(mid, before)
        self.assertLess(mid - before, runner.LIMIT + 1)
        self.assertFalse(self.runner.report(self.session)['busy_available'])
        self.runner._observe_lifecycle(self.record)
        self.assertTrue(self.runner.report(self.session)['busy_available'])

    def test_three_aborts_in_one_observation_are_three_declines_not_one_poll(self):
        self.runner._observe_lifecycle(self.record)
        for i in range(3):
            self.start(turn=f'turn-{i}')
            self.event('turn_aborted', turn_id=f'turn-{i}')
        self.runner._observe_lifecycle(self.record)
        self.assertEqual(self.runner.report(self.session)['decline_count'], 3)
        self.runner._observe_lifecycle(self.record)
        self.assertEqual(self.runner.report(self.session)['decline_count'], 3)

    def test_corrupt_negative_budget_refuses_instead_of_granting_more_turns(self):
        self.mutate(lambda v: runner.accounting(v).update(possibly_unattended_spent=-1))
        with self.assertRaisesRegex(ValueError, 'accounting'):
            self.runner.dispatch()
        self.assert_not_invoked()

    def test_old_result_cannot_mutate_operator_settled_or_newer_callback_intent(self):
        original = runner.drain
        old_nonce = self.nonce
        def drain(child):
            cleaned = original(child)
            self.runner.ledger.resolve_operator(self.session, old_nonce, 'mark_presented',
                'fixture-only audit', 1234, caller_role='operator')
            self.record['callback_generation'] = str(uuid.uuid4())
            binding.atomic(self.registry, self.record)
            self.prepare(1)
            return cleaned
        with patch.object(runner, 'drain', side_effect=drain):
            result = self.runner.dispatch()
        self.assertFalse(result['report_eligible'])
        value = self.runner.ledger.read()
        self.assertEqual(value['sessions'][self.session]['intent']['nonce'], self.nonce)
        self.assertEqual(value['sessions'][self.session]['intent']['state'], 'prepared')
        self.assertEqual(value['sessions'][self.session]['accepted_through'], 1)
        self.assertEqual(value['runner']['possibly_unattended_spent'], 1)

    def test_human_pause_does_not_clear_on_ack_or_observation(self):
        self.mutate(lambda v: runner.accounting(v).update(human_paused=True))
        self.runner.observe(acknowledged_through=1)
        with self.assertRaises(ValueError):
            self.runner.dispatch()
        self.assertTrue(self.runner.report(self.session)["paused"])
        self.assert_not_invoked()


    def test_poison_published_during_final_pidfd_check_still_refuses_spawn(self):
        # PR 710 carry-in: the poison reads follow handles.check(), so a revoke
        # landing while the pidfds are checked still stops the Popen.
        self.runner._observe_lifecycle(self.record)
        original = runner.Handles.check
        def check(handles):
            original(handles)
            binding.publish_request(self.registry, "custody-poison.json", reason="barrier")
        with patch.object(runner.Handles, "check", new=check):
            with self.assertRaisesRegex(ValueError, "poison"):
                self.runner._spawn(self.record, self.custody, self.daemon_class(None))
        self.assert_not_invoked()
        self.assertEqual(self.runner.report(self.session)["diagnostic"], "codex-uncertain-" + self.nonce)

    def test_partial_ack_does_not_use_a_synthetic_empty_queue(self):
        self.runner.ledger.reconcile(self.session, self.nonce, {"data": [], "nextCursor": None}, 1)
        self.prepare(4)  # Range (4, 5].
        self.runner.ledger.invoking(self.session, self.nonce)
        seen = []
        original = self.runner.ledger.reconcile
        def reconcile(session, nonce, queue, acknowledged_through=0, **kw):
            seen.append((queue, acknowledged_through))
            return original(session, nonce, queue, acknowledged_through, **kw)
        with patch.object(self.runner.ledger, "reconcile", side_effect=reconcile), \
                patch.object(self.daemon_class, "call", side_effect=ConnectionError("fixture daemon gone")):
            self.runner.observe(acknowledged_through=4)
        self.assertEqual(seen, [])  # Partial ack: no synthetic queue absence.
        with patch.object(self.runner.ledger, "reconcile", side_effect=reconcile), \
                patch.object(self.daemon_class, "call", side_effect=ConnectionError("fixture daemon gone")):
            self.runner.observe(acknowledged_through=5)
        self.assertEqual(seen, [({"data": [], "nextCursor": None}, 5)])
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "acked")

    def test_progress_on_an_empty_ledger_is_zero(self):
        value = {"sessions": {}}
        runner.progress(value)
        self.assertEqual(value["runner"]["accepted_through"], 0)

    def test_custody_loss_and_cleanup_failure_keep_both_sticky_diagnostics(self):
        original, drain = subprocess.Popen, runner.drain
        def spawn(*args, **kwargs):
            binding.publish_request(self.registry)
            child = original(*args, **kwargs)
            # The injected cleanup failure must not itself abandon an actual
            # subprocess. Keep ownership and reap it after the mock is removed.
            self.addCleanup(lambda: self.assertTrue(drain(child)))
            return child
        with patch.object(runner.subprocess, "Popen", side_effect=spawn), \
                patch.object(runner, "drain", return_value=False):
            result = self.runner.dispatch()
        self.assertEqual(result["diagnostics"], ["codex-custody-lost-after-invocation",
                                                 "codex-queue-cleanup-unproven"])
        # A later native-evidence gap never hides either sticky diagnostic.
        with patch.object(self.daemon_class, "call", side_effect=ConnectionError("fixture disconnect")):
            result = self.runner.observe()
        self.assertEqual(result["diagnostics"], ["codex-custody-lost-after-invocation",
                                                 "codex-queue-cleanup-unproven"])
        self.assertTrue(result["paused"])

    def test_legacy_single_sticky_diagnostic_migrates_without_loss(self):
        def legacy(value):
            value["runner"] = {"possibly_unattended_spent": 0, "declines": 0, "accepted_through": 0,
                               "retry_after_ms": 0, "human_paused": False,
                               "diagnostic": "codex-queue-cleanup-unproven"}
        self.mutate(legacy)
        result = self.runner.report(self.session)
        self.assertEqual(result["diagnostics"], ["codex-queue-cleanup-unproven"])
        self.assertEqual(result["diagnostic"], "codex-queue-cleanup-unproven")
        self.assertTrue(result["paused"])

    # ---- Activation: the supervisor's tick, grant budget and mail body ----

    def settle_setup_offer(self):
        self.runner.ledger.reconcile(self.session, self.nonce, {"data": [], "nextCursor": None}, 1)
        self.pending.update(after_seq=1, acknowledged_through=1, through_seq=1)

    def mail(self, through, actionable=1):
        after = self.pending["after_seq"]
        headers = [{"seq": seq, "id": f"m{seq}", "from": "captain", "kind": "request", "urgency": "normal",
                    "task_id": None} for seq in range(through, after, -1)][:6]
        self.pending.update(count=through - after, actionable=actionable, through_seq=through, headers=headers)

    def commands(self, name):
        return [args for command, args in self.calls if command == name]

    def test_tick_offers_once_reports_receipt_as_mail_wake_and_settles(self):
        self.settle_setup_offer()
        self.mail(3)
        self.calls.clear()
        self.assertEqual(runner.tick(self.registry), {"tick": "invoked"})
        invoked = json.loads((self.root / "invoked.json").read_text())
        message = invoked["argv"][-1]
        intent = self.runner.ledger.read()["sessions"][self.session]["intent"]
        self.assertEqual(message.split("\n", 1)[0], receipts.encode(receipts.marker(
            self.record["home"], "alpha", self.session, intent["nonce"], 1, 3)))
        self.assertIn("This is a notification, not an acknowledgement.", message)
        self.assertIn('\\"after_seq\\":1,\\"limit\\":40', message)
        self.assertEqual(len(self.commands("attended.codex.reserve")), 1)
        # The lost CLI result is uncertain at once (C3), reported paused/other.
        delivery = [a for a in self.commands("wake.hosted") if a["trigger"] == "delivery"][-1]
        self.assertEqual((delivery["paused"], delivery["decline_reason"], delivery["accepted_through"]),
                         (True, "other", 1))
        self.assertFalse(any(a["trigger"] == "mail" for a in self.commands("wake.hosted")))
        # The native promoted input is the receipt: one mail wake for (1, 3].
        self.body = message
        self.promoted()
        self.calls.clear()
        self.assertEqual(runner.tick(self.registry), {"tick": "idle"})
        wakes = [a for a in self.commands("wake.hosted") if a["trigger"] == "mail"]
        self.assertEqual([(w["after_seq"], w["through_seq"], w["session"]) for w in wakes], [(1, 3, self.session)])
        delivery = [a for a in self.commands("wake.hosted") if a["trigger"] == "delivery"][-1]
        self.assertEqual((delivery["paused"], delivery["accepted_through"], delivery["busy"]), (False, 3, False))
        self.calls.clear()
        self.pending.update(after_seq=3, count=0, actionable=0, through_seq=3, headers=[])
        runner.tick(self.registry)
        self.assertFalse(any(a["trigger"] == "mail" for a in self.commands("wake.hosted")))
        self.assertEqual(self.grant["spent"], 2)  # setUp's offer plus this one; no refund.

    def test_tick_never_offers_low_only_mail_or_without_a_grant(self):
        self.settle_setup_offer()
        self.mail(3, actionable=0)
        self.assertEqual(runner.tick(self.registry), {"tick": "idle"})
        self.mail(3)
        self.grant = None
        self.calls.clear()
        self.assertEqual(runner.tick(self.registry), {"tick": "not granted"})
        self.assertEqual(self.commands("attended.codex.reserve"), [])
        self.assertEqual(self.runner.ledger.read()["sessions"][self.session]["intent"]["state"], "acked")
        self.assert_not_invoked()

    def test_tick_holds_a_prepared_offer_while_busy_without_spending(self):
        self.settle_setup_offer()
        self.prepare(1, reserve=False)
        self.runner._observe_lifecycle(self.record)
        self.start(turn="captain-turn")
        self.status = {"type": "active", "activeFlags": []}
        self.mail(2)
        self.calls.clear()
        self.assertEqual(runner.tick(self.registry), {"tick": "held"})
        self.assertEqual(self.commands("attended.codex.reserve"), [])
        self.assert_not_invoked()
        delivery = [a for a in self.commands("wake.hosted") if a["trigger"] == "delivery"][-1]
        self.assertEqual((delivery["busy"], delivery["turn_started_ms"]), (True, 123000))

    def test_tick_budget_exhaustion_pauses_until_a_new_captain_grant(self):
        self.settle_setup_offer()
        self.grant["budget"] = 1  # setUp already spent it.
        self.mail(2)
        self.assertEqual(runner.tick(self.registry), {"tick": "not granted"})
        self.prepare(1, reserve=False)
        with self.assertRaisesRegex(ValueError, "refused"):
            runner.tick(self.registry)
        result = self.runner.report(self.session)
        self.assertTrue(result["paused"])
        self.assertEqual(result["diagnostic"], "codex-turn-budget-exhausted")
        self.assertEqual(runner.tick(self.registry), {"tick": "held"})  # Time and mail clear nothing.
        self.grant = {"grant_id": str(uuid.uuid4()), "session": self.session, "budget": 5, "spent": 0}
        self.assertEqual(runner.tick(self.registry), {"tick": "invoked"})
        self.assertEqual(self.grant["spent"], 1)

    def test_tick_unbound_or_unhosted_does_nothing(self):
        for change in ({"state": "unbound"}, {"hosted": False}):
            binding.atomic(self.registry, {**self.record, **change})
            self.calls.clear()
            self.assertEqual(runner.tick(self.registry), {"tick": "unbound"})
            self.assertEqual(self.calls, [])
        self.assertEqual(runner.main(["codex_runner.py"]), 2)

    def test_publish_withholds_delivery_without_a_truthful_busy_start(self):
        self.calls.clear()
        self.runner.publish(self.record, {"report_eligible": True, "busy_available": False})
        self.assertEqual(self.commands("wake.hosted"), [])
        self.runner.publish(self.record, {"report_eligible": False, "busy_available": True})
        self.assertEqual(self.commands("wake.hosted"), [])

    def test_mail_snapshot_validation_and_body_carries_only_headers(self):
        good = {"address": "alpha", "after_seq": 2, "acknowledged_through": 2, "count": 1, "through_seq": 3,
                "actionable": 1, "headers": [{"seq": 3, "id": "m3"}], "attention": None}
        codex_mail.snapshot(good, "alpha")
        for bad in ({"address": "beta"}, {"count": -1}, {"actionable": 2}, {"through_seq": 1},
                    {"headers": [{"seq": 9}]}, {"headers": [{"seq": 3}] * 7, "count": 7},
                    {"attention": 3}, {"after_seq": True}):
            with self.assertRaises(ValueError):
                codex_mail.snapshot({**good, **bad}, "alpha")
        self.assertFalse(codex_mail.actionable({**good, "actionable": 0}))
        record = {**self.record, "control": "/opt/privateer"}
        text = codex_mail.body(record, self.nonce, good)
        head, rest = text.split("\n", 1)
        self.assertEqual(json.loads(head), receipts.marker(self.record["home"], "alpha", self.session,
                                                           self.nonce, 2, 3))
        self.assertIn('["/opt/privateer","--home"', rest)
        self.assertIn('Headers: [{"seq":3,"id":"m3"}]', rest)

if __name__ == "__main__":
    unittest.main()
