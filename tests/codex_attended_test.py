#!/usr/bin/env python3
"""Hermetic binding tests. No native Codex, provider, auth or live Privateer state."""
import base64
from concurrent.futures import ThreadPoolExecutor
import copy
import hashlib
import importlib.util
import io
import json
import multiprocessing
import os
from pathlib import Path
import shlex
import socket
import struct
import sys
import subprocess
import tempfile
import threading
from types import SimpleNamespace
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
# Hook, dispatcher and runner processes are real interpreter starts; a loaded
# shared runner can exceed 5 s before the child returns. These are liveness
# bounds, not latency assertions.
SUBPROCESS_TIMEOUT = 30
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
import codex_binding as binding
import codex_socket
import codex_receipts as receipts


def uuid7(ms):
    return str(uuid.UUID(int=(ms << 80) | (7 << 76) | (2 << 62) | 1))


class FakeDaemon:
    pid = 1
    identity = {"bootId": "alpha", "startTicks": 1}
    target = Path("/alpha/socket")
    rendezvous = Path("/alpha/rendezvous")
    socket_identity = (7, 9)

    def __init__(self, record, event):
        session = event["session_id"]
        self.responses = {
            "thread/loaded/list": {"data": [session], "nextCursor": None},
            "thread/read": {"thread": {"id": session, "sessionId": session, "cwd": record["cwd"],
                                      "path": event["transcript_path"], "historyMode": "paginated"}},
            "thread/queue/list": {"data": [], "nextCursor": None},
            "hooks/list": {"data": [{"hooks": [{"command": record["hook_command"], "source": "project",
                                                "trustStatus": "trusted", "enabled": True}]}]},
        }

    def call(self, method, args):
        return copy.deepcopy(self.responses[method])

    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass


def socket_server(path, ready, home, bad_handshake):
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(path)
        os.chmod(path, 0o600)
        listener.listen(1)
        ready.send(True)
        with listener.accept()[0] as client:
            client.settimeout(5)
            header = b""
            while not header.endswith(b"\r\n\r\n"):
                header += client.recv(1)
            key = next(line.split(b":", 1)[1].strip() for line in header.split(b"\r\n") if line.startswith(b"Sec-WebSocket-Key:"))
            accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
            if bad_handshake:
                accept = b"wrong"
            client.sendall(b"HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
            def read(n):
                value = b""
                while len(value) < n:
                    chunk = client.recv(n - len(value))
                    if not chunk:
                        raise EOFError()
                    value += chunk
                return value
            try:
                while True:
                    header = read(2)
                    size = header[1] & 127
                    if size == 126:
                        size = struct.unpack("!H", read(2))[0]
                    mask = read(4)
                    request = json.loads(bytes(b ^ mask[i % 4] for i, b in enumerate(read(size))))
                    if "id" not in request:
                        continue
                    result = {"codexHome": home} if request["method"] == "initialize" else {"data": [], "nextCursor": None}
                    payload = json.dumps({"id": request["id"], "result": result}).encode()
                    client.sendall(bytes([0x81, len(payload)]) + payload)
            except (EOFError, ConnectionResetError, BrokenPipeError):
                pass


class BindingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="alpha-codex-unit-", dir=os.environ.get("TMPDIR", "/var/tmp"))
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.cwd = self.root / "alpha"
        self.cwd.mkdir()
        for name in ["home", "codex", "resources", "tmp", "state", "data", "config"]:
            (self.root / name).mkdir()
        self.env = {"HOME": str(self.root / "home"), "CODEX_HOME": str(self.root / "codex"),
                    "PRIVATEER_FLEET": str(self.root / "fleet.sqlite"),
                    "PRIVATEER_RESOURCE_HOME": str(self.root / "resources"),
                    "TMPDIR": str(self.root / "tmp"), "XDG_STATE_HOME": str(self.root / "state"),
                    "XDG_DATA_HOME": str(self.root / "data"), "XDG_CONFIG_HOME": str(self.root / "config"),
                    "PRIVATEER_HOME": str(self.root / "home"), "PRIVATEER_CONTROL_CLI": "/alpha/privateer",
                    "PRIVATEER_ROLE": "agent", "PRIVATEER_ACTOR": "alpha",
                    "PRIVATEER_PRESENCE_HOLDER": "host:alpha", "PRIVATEER_HOSTED": "1"}
        environment = patch.dict(os.environ, self.env)
        environment.start()
        self.addCleanup(environment.stop)
        self.old_cwd = Path.cwd()
        os.chdir(self.cwd)
        self.addCleanup(os.chdir, self.old_cwd)
        self.session = str(uuid.uuid4())
        self.argv = [sys.executable, "resume", self.session, "-C", str(self.cwd), "--sandbox", "workspace-write"]
        binding.prepare(self.argv, self.env)
        self.registry = self.cwd / ".codex/privateer-attended/launch.json"
        self.record = codex_socket.load(self.registry)
        self.event = {"session_id": self.session, "cwd": str(self.cwd),
                      "hook_event_name": "SessionStart", "source": "resume"}
        self.event["transcript_path"] = str(self.root / "codex/sessions/alpha.jsonl")
        self.daemon = FakeDaemon(self.record, self.event)

    def test_hosted_bearings_are_positional_and_routes_are_unchanged(self):
        self.assertEqual(self.record["argv"][:len(self.argv)], self.argv)
        self.assertEqual(self.record["argv"][len(self.argv)], "--")
        self.assertIn("first mate of alpha", self.record["argv"][-1])
        self.assertFalse(self.record["delivery_enabled"])
        self.assertEqual(self.record["state"], "unbound")
        self.assertNotIn(self.record["generation"], self.record["hook_command"])

    def test_exclusions_and_unknown_plans_fail_closed(self):
        for args in [["--no-daemon"], ["--profile", "alpha"], ["--strict-config"],
                     ["--dangerously-bypass-hook-trust"], ["--remote", "unix:///alpha"],
                     ["-c", 'web_search="disabled"'], ["-c", 'model_reasoning_effort="high"'],
                     ["--config=web_search=live"], ["-cweb_search=live"], ["--ignore-user-config"]]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                binding.plan(["codex", *args], self.cwd)
        self.assertEqual(binding.plan(["codex", "-c", "features.hooks=true"], self.cwd), (None, False))
        self.assertEqual(binding.plan(["codex", "-c", "tui.fullscreen_transcript=true"], self.cwd), (None, False))

    def test_effort_uses_only_a_verified_daemon_and_reads_both_settings_back(self):
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        argv = [*self.argv, "--model", "ship-model", "-c", 'model_reasoning_effort="high"']
        with patch.object(binding, "Daemon", return_value=self.daemon):
            planned = binding.prepare(argv, self.env)
        self.assertEqual(planned[1:5], ["resume", "--remote", "unix:///alpha/socket", self.session])
        record = codex_socket.load(self.registry)
        self.assertEqual(record["requested_settings"], {"model": "ship-model", "effort": "high"})
        self.assertEqual(record["attach_daemon"], binding.daemon_proof(self.daemon))
        thread = self.daemon.responses["thread/read"]["thread"]
        thread.update(model="ship-model", reasoningEffort="high")
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control", side_effect=[
                    {"attended": True, "lease": {"holder": "host:alpha"}}, {"recorded": True}]):
            result = binding.bind(self.registry, self.event)
        self.assertEqual(result["settings"], record["requested_settings"])
        self.assertEqual(codex_socket.load(self.registry)["state"], "bound")
        for field, bad in [("model", "wrong-model"), ("reasoningEffort", "low"), ("reasoningEffort", None)]:
            with self.subTest(field=field, bad=bad):
                thread.update(model="ship-model", reasoningEffort="high")
                thread[field] = bad
                with patch.object(binding, "Daemon", return_value=self.daemon), \
                        patch.object(binding, "control", return_value={
                            "attended": True, "lease": {"holder": "host:alpha"}}) as report, \
                        self.assertRaises(binding.SettingsError):
                    binding.bind(self.registry, self.event)
                self.assertEqual([call.args[1:] for call in report.call_args_list],
                                 [("ship.presence", {"ship": "alpha"})])
                saved = codex_socket.load(self.registry)
                self.assertEqual(saved["state"], "unbound")
                self.assertFalse(saved["delivery_enabled"])
        thread.update(model="ship-model", reasoningEffort="high")
        for key, value in [("pid", 99), ("socket_identity", [7, 10])]:
            changed = copy.deepcopy(record)
            changed["attach_daemon"][key] = value
            with self.assertRaisesRegex(ValueError, "daemon changed"):
                binding.validate(changed, self.event, self.daemon)

    def test_model_only_keeps_implicit_route_and_requires_exact_read_back(self):
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        argv = [*self.argv, "--model", "ship-model"]
        planned = binding.prepare(argv, self.env)
        self.assertEqual(planned[:len(argv)], argv)
        self.assertNotIn("--remote", planned)
        record = codex_socket.load(self.registry)
        self.assertIsNone(record["attach_daemon"])
        self.assertEqual(record["requested_settings"], {"model": "ship-model"})
        thread = self.daemon.responses["thread/read"]["thread"]
        thread["model"] = "ship-model"
        self.assertEqual(binding.validate(record, self.event, self.daemon)["settings"],
                         {"model": "ship-model"})
        for model in ["wrong-model", None]:
            thread["model"] = model
            with self.subTest(model=model), self.assertRaises(binding.SettingsError):
                binding.validate(record, self.event, self.daemon)

    def test_remote_insertion_preserves_flags_before_resume_uuid(self):
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        argv = [sys.executable, "resume", "--model", "ship-model", self.session,
                "-c", 'model_reasoning_effort="high"']
        with patch.object(binding, "Daemon", return_value=self.daemon):
            planned = binding.prepare(argv, self.env)
        self.assertEqual(planned[:len(argv) + 2],
                         argv[:2] + ["--remote", "unix:///alpha/socket"] + argv[2:])
        self.assertEqual(codex_socket.load(self.registry)["requested_settings"],
                         {"model": "ship-model", "effort": "high"})

    def test_value_flags_cannot_consume_another_flag(self):
        for flag in ["-C", "--cd", "--sandbox", "-s", "--add-dir", "--model", "-m",
                     "--ask-for-approval", "-a", "--enable", "--disable"]:
            for value in ["--model", "-m", ""]:
                with self.subTest(flag=flag, value=value), \
                        self.assertRaisesRegex(ValueError, "option value"):
                    binding.plan([*self.argv, flag, value], self.cwd)

    def test_effort_accepts_only_supported_values(self):
        for value in ["minimal", "low", "medium", "high", "xhigh", "none"]:
            self.assertEqual(binding.parse_plan(
                [*self.argv, "-c", "model_reasoning_effort=" + json.dumps(value)], self.cwd)[2],
                {"effort": value})
        for value in ["unsupported", "auto", "High", True, None, []]:
            self.assertFalse(binding.effort_word(value))
            with self.subTest(value=value), self.assertRaises(ValueError):
                binding.plan([*self.argv, "-c", "model_reasoning_effort=" + json.dumps(value)], self.cwd)

    def test_unproven_effort_paths_refuse_before_prepare_modifies_state(self):
        argv = [*self.argv, "-c", 'model_reasoning_effort="high"']
        before = self.registry.read_bytes()
        with patch.object(binding, "Daemon", side_effect=ValueError("daemon unavailable")), \
                self.assertRaisesRegex(ValueError, "unavailable"):
            binding.prepare(argv, self.env)
        self.assertEqual(self.registry.read_bytes(), before)
        with self.assertRaisesRegex(ValueError, "hosted exact-UUID"):
            binding.prepare(argv, {**self.env, "PRIVATEER_HOSTED": "0"})
        for extra in [["--remote", "unix:///other"], ["-c", 'web_search="disabled"'],
                      ["-c", 'model_reasoning_effort="low"'], ["-c", 'model_provider="other"']]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                binding.plan([*argv, *extra], self.cwd)
        for value in ['"high low"', '"high"\\nmodel_provider="other"', 'true', '""']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                binding.plan([*self.argv, "-c", "model_reasoning_effort=" + value], self.cwd)

    def run_hook(self, event=None):
        event = self.event if event is None else event
        with patch.object(binding.sys, "argv", ["hook", "--registry", str(self.registry)]), \
                patch.object(binding.sys, "stdin", SimpleNamespace(
                    buffer=io.BytesIO(json.dumps(event).encode()))), \
                patch.object(binding.sys, "stdout", new_callable=io.StringIO) as output:
            binding.main()
        return json.loads(output.getvalue())

    def settings_launch(self):
        self.record["requested_settings"] = {"model": "ship-model", "effort": "high"}
        self.record["attach_daemon"] = binding.daemon_proof(self.daemon)
        binding.atomic(self.registry, self.record)

    def test_settings_mismatch_hook_stops_the_initial_turn(self):
        self.settings_launch()
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control", return_value={
                    "attended": True, "lease": {"holder": "host:alpha"}}) as report:
            result = self.run_hook()
        self.assertIs(result["continue"], False)
        self.assertIn("does not match the requested setting", result["stopReason"])
        self.assertEqual([call.args[1] for call in report.call_args_list], ["ship.presence"])
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_foreign_uuid_settings_callback_is_unbound_without_stopping(self):
        self.settings_launch()
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control") as report:
            result = self.run_hook({**self.event, "session_id": str(uuid.uuid4())})
        self.assertNotIn("continue", result)
        self.assertNotIn("stopReason", result)
        self.assertIn("UNBOUND", result["systemMessage"])
        report.assert_not_called()
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_exited_pane_settings_callback_is_unbound_without_stopping(self):
        # Use a real exited holder PID rather than only a stale mocked identity.
        with subprocess.Popen([sys.executable, "-c", "pass"]) as pane:
            pane.wait(timeout=SUBPROCESS_TIMEOUT)
        self.record["pid"] = pane.pid
        self.settings_launch()
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control") as report:
            result = self.run_hook()
        self.assertNotIn("continue", result)
        self.assertNotIn("stopReason", result)
        self.assertIn("identity is stale", result["systemMessage"])
        report.assert_not_called()
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_replaced_attach_daemon_stops_verified_settings_launch(self):
        self.settings_launch()
        self.record["attach_daemon"]["pid"] += 1
        binding.atomic(self.registry, self.record)
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control", return_value={
                    "attended": True, "lease": {"holder": "host:alpha"}}) as report:
            result = self.run_hook()
        self.assertIs(result["continue"], False)
        self.assertIn("attach daemon changed", result["stopReason"])
        self.assertEqual([call.args[1] for call in report.call_args_list], ["ship.presence"])
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_superseded_settings_callback_does_not_stop_newer_turn(self):
        self.settings_launch()
        self.daemon.responses["thread/read"]["thread"].update(model="ship-model", reasoningEffort="high")
        for newer in ["callback", "launch", "poison"]:
            def report(record, command, args):
                if command == "ship.presence":
                    return {"attended": True, "lease": {"holder": "host:alpha"}}
                current = codex_socket.load(self.registry)
                if newer == "poison":
                    binding.publish_request(self.registry)
                elif newer == "launch":
                    current["generation"] = str(uuid.uuid4())
                else:
                    current["callback_generation"] = str(uuid.uuid4())
                binding.atomic(self.registry, current)
                return {"recorded": True}
            with self.subTest(newer=newer), patch.object(binding, "Daemon", return_value=self.daemon), \
                    patch.object(binding, "control", side_effect=report):
                result = self.run_hook()
            self.assertIn("superseded", result["systemMessage"])
            self.assertNotIn("continue", result)
            self.assertNotIn("stopReason", result)
            self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_wrong_presence_settings_callback_does_not_stop(self):
        self.settings_launch()
        for changed in ["holder", "unattended"]:
            self.record["attach_daemon"] = binding.daemon_proof(self.daemon)
            binding.atomic(self.registry, self.record)
            presence = {"attended": changed != "unattended", "lease": {
                "holder": "other-holder" if changed == "holder" else "host:alpha"}}
            with self.subTest(changed=changed), \
                    patch.object(binding, "Daemon", return_value=self.daemon), \
                    patch.object(binding, "control", return_value=presence):
                result = self.run_hook()
                self.assertNotIn("continue", result)
                self.assertNotIn("stopReason", result)
                self.assertIn("UNBOUND", result["systemMessage"])

    def test_callback_failure_stops_only_after_settings_launch_custody_is_proven(self):
        for settings in [{"model": "ship-model"}, {"effort": "high"}, {}]:
            self.record["requested_settings"] = settings
            for error in [ValueError("unsupported schema"), TimeoutError("daemon timed out"),
                          OSError("daemon unreachable"), KeyError("schema field")]:
                self.record.update(state="bound", delivery_enabled=False)
                binding.atomic(self.registry, self.record)
                with self.subTest(settings=settings, error=error), \
                        patch.object(binding, "Daemon", return_value=self.daemon), \
                        patch.object(binding, "control", return_value={
                            "attended": True, "lease": {"holder": "host:alpha"}}), \
                        patch.object(self.daemon, "call", side_effect=error):
                    result = self.run_hook()
                if settings:
                    self.assertIs(result["continue"], False)
                    self.assertTrue(result["stopReason"])
                else:
                    self.assertNotIn("continue", result)
                self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_unverified_callback_failure_never_stops_a_turn(self):
        self.settings_launch()
        for error in [ValueError("daemon unavailable"), OSError("daemon unreachable")]:
            with self.subTest(error=error), \
                    patch.object(binding, "Daemon", side_effect=error):
                result = self.run_hook()
            self.assertNotIn("continue", result)
            self.assertNotIn("stopReason", result)
            self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_resume_is_exact_and_prompt_is_not_parsed_as_flags(self):
        session = str(uuid.uuid4())
        self.assertEqual(binding.plan(["codex", "resume", session, "--", "--no-daemon"], self.cwd), (session, True))
        for args in [["resume"], ["resume", "--last"], ["resume", "alpha"], ["resume", session, "a", "b"]]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                binding.plan(["codex", *args], self.cwd)

    def test_live_same_cwd_is_refused_and_stale_relaunch_keeps_command(self):
        with self.assertRaises(ValueError):
            binding.prepare(self.argv, self.env)
        self.record["last_binding"] = {"session": self.session}
        self.record["process_identity"]["bootId"] = "previous-boot"
        binding.atomic(self.registry, self.record)
        binding.prepare(self.argv, self.env)
        fresh = codex_socket.load(self.registry)
        self.assertNotEqual(fresh["generation"], self.record["generation"])
        self.assertNotIn("last_binding", fresh)
        self.assertEqual(fresh["hook_command"], self.record["hook_command"])
        self.assertEqual(len(codex_socket.load(self.cwd / ".codex/hooks.json")["hooks"]["SessionStart"]), 1)

    def test_hook_command_is_stable_across_privateer_releases(self):
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        with patch.object(binding, "__file__", str(self.root / "release-beta/codex_binding.py")):
            binding.prepare(self.argv, self.env)
        fresh = codex_socket.load(self.registry)
        self.assertNotEqual(fresh["handler"], self.record["handler"])
        self.assertEqual(fresh["hook_command"], self.record["hook_command"])
        self.assertEqual(len(codex_socket.load(self.cwd / ".codex/hooks.json")["hooks"]["SessionStart"]), 1)

    def test_system_interpreter_is_stable_and_unavailable_interpreter_refuses(self):
        command = self.record["hook_command"]
        self.assertEqual(shlex.split(command)[0], "/usr/bin/python3")
        with patch.object(binding.sys, "executable", "/alpha/python/next/bin/python3"):
            self.assertEqual(binding.hook_command(self.registry), command)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        hooks = self.cwd / ".codex/hooks.json"
        before = hooks.read_bytes()
        for available in (False, True):
            with self.subTest(available=available), \
                    patch.object(binding.Path, "is_file", return_value=available), \
                    patch.object(binding.os, "access", return_value=False), \
                    self.assertRaisesRegex(ValueError, "requires executable /usr/bin/python3"):
                binding.prepare(self.argv, self.env)
            self.assertEqual(hooks.read_bytes(), before)
            self.assertEqual(codex_socket.load(self.registry), self.record)

    def test_prepare_refuses_old_or_unproven_system_python_before_state_changes(self):
        hooks = self.cwd / ".codex/hooks.json"
        before = hooks.read_bytes()
        dispatcher = self.registry.with_name("hook.py").read_bytes()
        for outcome in (subprocess.CompletedProcess([], 1),
                        subprocess.TimeoutExpired([], 3), OSError("unavailable")):
            kwargs = ({"side_effect": outcome} if isinstance(outcome, Exception)
                      else {"return_value": outcome})
            with self.subTest(outcome=outcome), patch.object(binding.subprocess, "run", **kwargs) as probe, \
                    self.assertRaisesRegex(ValueError, r"/usr/bin/python3 >= 3.9"):
                binding.prepare(self.argv, self.env)
            self.assertEqual(probe.call_args.args[0][:4], ["/usr/bin/python3", "-I", "-B", "-c"])
            self.assertEqual(probe.call_args.kwargs["timeout"], 3)
            self.assertEqual(hooks.read_bytes(), before)
            self.assertEqual(self.registry.with_name("hook.py").read_bytes(), dispatcher)
            self.assertEqual(codex_socket.load(self.registry), self.record)

    def test_duplicate_dispatcher_cannot_shift_unrelated_hook_trust_positions(self):
        hooks = self.cwd / ".codex/hooks.json"
        own = {"type": "command", "command": self.record["hook_command"]}
        unrelated = {"type": "command", "command": "/alpha/other.py"}
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        trust = self.root / "codex/hooks.state.json"
        trust.write_text('{"alpha":"unchanged trust"}')
        for later_group in (False, True):
            groups = [{"hooks": [own, own]}]
            if later_group:
                groups.append({"matcher": "resume", "hooks": [unrelated]})
            else:
                groups[0]["hooks"].append(unrelated)
            value = {"hooks": {"SessionStart": groups}}
            binding.atomic(hooks, value)
            before = hooks.read_bytes()
            with self.subTest(later_group=later_group), \
                    self.assertRaisesRegex(ValueError, "trust positions must not shift"):
                binding.prepare(self.argv, self.env)
            self.assertEqual(hooks.read_bytes(), before)
            self.assertEqual(codex_socket.load(self.registry), self.record)
            self.assertEqual(trust.read_text(), '{"alpha":"unchanged trust"}')
            # Also leave the caller's in-memory declaration tree untouched.
            original = copy.deepcopy(value)
            with self.assertRaises(ValueError):
                binding.project_hooks(value, self.registry, self.cwd)
            self.assertEqual(value, original)

    def test_prepare_replaces_only_own_isolated_dispatcher_entries_without_trusting(self):
        hooks = self.cwd / ".codex/hooks.json"
        own = str(self.registry.with_name("hook.py"))
        other = str(self.root / "beta/hook.py")
        unrelated = [{"type": "command", "command": "true"},
                     {"type": "command", "command": shlex.join(["/alpha/python3", "-I", "-B", other])},
                     {"type": "command", "command": shlex.join(["/alpha/python3", "-I", "-B", own, "extra"])}]
        old = lambda python: {"type": "command", "command": shlex.join([python, "-I", "-B", own])}
        value = {"description": "alpha", "hooks": {
            "SessionStart": [{"matcher": "resume", "hooks": [old("/alpha/python3"), *unrelated]},
                             {"hooks": [old("/beta/python3"), old("/usr/bin/python3")]}],
            "Stop": [{"hooks": [{"type": "command", "command": "false"}]}]}}
        binding.atomic(hooks, value)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        trust = self.root / "codex/hooks.state.json"
        trust.write_text('{"alpha":"unchanged trust"}')
        binding.prepare(self.argv, self.env)
        fresh = codex_socket.load(hooks)
        self.assertEqual(fresh, {**value, "hooks": {**value["hooks"], "SessionStart": [
            {"matcher": "resume", "hooks": [
                {"type": "command", "command": self.record["hook_command"]}, *unrelated]}]}})
        self.assertEqual(trust.read_text(), '{"alpha":"unchanged trust"}')

    def test_prepare_preserves_current_dispatcher_position_and_declaration_metadata(self):
        hooks = self.cwd / ".codex/hooks.json"
        value = codex_socket.load(hooks)
        value["hooks"]["SessionStart"][0]["matcher"] = "resume"
        value["hooks"]["SessionStart"][0]["hooks"][0]["timeout"] = 9
        value["hooks"]["SessionStart"].append(
            {"hooks": [{"type": "command", "command": "/alpha/stop.py"}]})
        binding.atomic(hooks, value)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        binding.prepare(self.argv, self.env)
        self.assertEqual(codex_socket.load(hooks), value)

    def test_dispatcher_uses_current_record_not_its_original_release(self):
        handler = self.root / "release-beta/codex_binding.py"
        handler.parent.mkdir()
        handler.write_text("import json,sys\nprint(json.dumps({'callback':json.load(sys.stdin),'argv':sys.argv[1:]}))\n")
        self.record["handler"] = str(handler)
        binding.atomic(self.registry, self.record)
        result = subprocess.run(shlex.split(self.record["hook_command"]),
                                input=json.dumps(self.event), text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        value = json.loads(result.stdout)
        self.assertEqual(value["callback"], self.event)
        self.assertEqual(value["argv"], ["--registry", str(self.registry)])
        self.registry.chmod(0o644)
        result = subprocess.run(shlex.split(self.record["hook_command"]),
                                input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("callback", result.stdout)

    def test_dispatcher_isolated_python_and_invalidates_before_loading_handler(self):
        handler = self.root / "release-beta/codex_binding.py"
        handler.parent.mkdir()
        handler.write_text("import json,os,pathlib,sys\n"
            "value=json.loads(pathlib.Path(sys.argv[2]).read_text())\n"
            "print(json.dumps({'record':value,'previous':_DISPATCH_CONTEXT[0],"
            "'token':_DISPATCH_CONTEXT[1],'path':sys.path,'isolated':sys.flags.isolated}))\n")
        record = {**self.record, "state": "bound", "delivery_enabled": True,
                  "binding": {"session": self.session}, "handler": str(handler)}
        binding.atomic(self.registry, record)
        poison = self.root / "poison"
        poison.mkdir()
        (poison / "sitecustomize.py").write_text("raise RuntimeError('untrusted PYTHONPATH')\n")
        # Without -I, PYTHONHOME prevents Python startup entirely. Also exclude
        # hook.py's writable script directory, not just PYTHONPATH/user site.
        env = {**os.environ, "PYTHONPATH": str(poison), "PYTHONHOME": str(poison)}
        self.assertEqual(shlex.split(self.record["hook_command"])[1:3], ["-I", "-B"])
        result = subprocess.run(shlex.split(self.record["hook_command"]), env=env,
                                input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertEqual(result.returncode, 0, result.stderr)
        value = json.loads(result.stdout)
        self.assertEqual(value["isolated"], 1)
        self.assertNotIn(str(poison), value["path"])
        self.assertNotIn(str(self.registry.parent), value["path"])
        self.assertEqual(value["path"][0], str(handler.parent))
        self.assertEqual(value["previous"], record)
        self.assertEqual(value["record"]["callback_generation"], value["token"])
        self.assertEqual(value["record"]["state"], "unbound")
        self.assertFalse(value["record"]["delivery_enabled"])
        self.assertNotIn("binding", value["record"])
        self.assertEqual(value["record"]["last_binding"], record["binding"])

    def test_missing_or_unsafe_handler_cannot_preserve_binding(self):
        directory = self.root / "release-beta"
        directory.mkdir()
        handler = directory / "codex_binding.py"
        other = directory / "other.py"
        other.write_text("raise AssertionError('unsafe handler was run')\n")
        for kind in ("missing", "directory", "symlink", "writable", "wrong_name"):
            with self.subTest(kind=kind):
                target = handler
                if kind == "directory":
                    handler.mkdir()
                elif kind == "symlink":
                    handler.symlink_to(other)
                elif kind == "writable":
                    handler.write_text(other.read_text())
                    handler.chmod(0o666)
                elif kind == "wrong_name":
                    target = other
                binding.atomic(self.registry, {**self.record, "state": "bound", "delivery_enabled": True,
                    "binding": {"session": self.session}, "handler": str(target)})
                result = subprocess.run(shlex.split(self.record["hook_command"]),
                                        input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("unsafe handler was run", result.stderr)
                saved = codex_socket.load(self.registry)
                self.assertEqual(saved["state"], "unbound")
                self.assertFalse(saved["delivery_enabled"])
                self.assertNotIn("binding", saved)
                self.assertEqual(saved["last_binding"], {"session": self.session})
                # A second dispatcher invocation must not erase continuity even
                # though its predecessor already removed the active binding.
                result = subprocess.run(shlex.split(self.record["hook_command"]),
                                        input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(codex_socket.load(self.registry)["last_binding"], saved["last_binding"])
                if kind == "directory":
                    handler.rmdir()
                elif kind in ("symlink", "writable"):
                    handler.unlink()

    def test_legacy_release_hook_for_this_registry_refuses_without_changing_hooks(self):
        hooks = self.cwd / ".codex/hooks.json"
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        for event in ("SessionStart", "Stop"):
            for option in (["--registry", str(self.registry)], ["--registry=" + str(self.registry)]):
                value = {"hooks": {event: [{"hooks": [{"type": "command", "command": shlex.join(
                    [sys.executable, "-B", "/alpha/old-release/codex_binding.py", *option])}]}]}}
                binding.atomic(hooks, value)
                before = hooks.read_bytes()
                with self.subTest(event=event, option=option), self.assertRaisesRegex(ValueError, "legacy codex_binding.py hook"):
                    binding.prepare(self.argv, self.env)
                self.assertEqual(hooks.read_bytes(), before)
                self.assertEqual(codex_socket.load(self.registry), self.record)
        env = {**os.environ, **self.env, "PRIVATEER_CODEX_ATTENDED": "1"}
        result = subprocess.run([sys.executable, "-B", str(ROOT / "plugins/native_launch.py"),
            "--attended-policy", '{"mode":"standard"}', "codex", "codex", "resume", self.session],
            env=env, text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Codex attended launch refused: legacy codex_binding.py hook", result.stderr)
        self.assertEqual(hooks.read_bytes(), before)

    def test_malformed_hook_shapes_are_value_errors_and_visible_at_launch(self):
        hooks = self.cwd / ".codex/hooks.json"
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        values = [None, [], {"hooks": []}, {"hooks": {"Stop": {}}},
                  {"hooks": {"SessionStart": [None]}}, {"hooks": {"SessionStart": [{}]}},
                  {"hooks": {"SessionStart": [{"hooks": [None]}]}},
                  {"hooks": {"SessionStart": [{"hooks": [{"command": 7}]}]}}]
        for value in values:
            binding.atomic(hooks, value)
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "invalid Codex project hook"):
                binding.prepare(self.argv, self.env)
        env = {**os.environ, **self.env, "PRIVATEER_CODEX_ATTENDED": "1"}
        result = subprocess.run([sys.executable, "-B", str(ROOT / "plugins/native_launch.py"),
            "--attended-policy", '{"mode":"standard"}', "codex", "codex", "resume", self.session],
            env=env, text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Codex attended launch refused: invalid Codex project hook", result.stderr)

    def test_unknown_dispatcher_is_not_overwritten(self):
        path = self.registry.with_name("hook.py")
        path.write_text("# captain owned\n")
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        with self.assertRaisesRegex(ValueError, "dispatcher differs"):
            binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_text(), "# captain owned\n")

    def bind_current(self):
        def control(record, command, args):
            return ({"attended": True, "lease": {"holder": record["holder"]}}
                    if command == "ship.presence" else {"recorded": True})
        with patch.object(binding, "Daemon", return_value=self.daemon), patch.object(binding, "control", control):
            binding.bind(self.registry, self.event)
        return codex_socket.load(self.registry)

    def test_exact_b0_dispatcher_migration_preserves_command_trust_and_facts(self):
        path = self.registry.with_name("hook.py")
        b0 = (ROOT / "tests/fixtures/codex-dispatcher-b0.txt").read_bytes()
        self.assertEqual(hashlib.sha256(b0).hexdigest(), binding.B0_DISPATCHER_SHA256)
        path.write_bytes(b0)
        hooks = self.cwd / ".codex/hooks.json"
        before = hooks.read_bytes()
        trust = self.root / "codex/hooks.state.json"
        trust.write_text('{"alpha":"unchanged"}')
        # Even an old bound/enabled record is never fence-qualified.
        old = {**self.record, "state": "bound", "delivery_enabled": True,
               "binding": {"session": self.session}}
        binding.atomic(self.registry, old)
        with self.assertRaisesRegex(ValueError, "fence missing"):
            binding.require_fence(self.registry, old)
        with self.assertRaisesRegex(ValueError, "another Codex launch"):
            binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_bytes(), b0)
        old["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, old)
        # Simulated pending facts live beside, not in the per-launch registry.
        ledger = self.registry.with_name("receipts.json")
        ledger.write_text('{"alpha":"unresolved nonce"}')
        binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_text(), binding.DISPATCHER)
        self.assertEqual(hooks.read_bytes(), before)
        self.assertEqual(trust.read_text(), '{"alpha":"unchanged"}')
        self.assertEqual(ledger.read_text(), '{"alpha":"unresolved nonce"}')
        self.assertFalse(codex_socket.load(self.registry)["delivery_enabled"])
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")
        # Prepare rotates poison but does not manufacture validated binding.
        self.assertTrue(self.registry.with_name("invalidate.json").exists())
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, codex_socket.load(self.registry))
        restored = self.bind_current()
        binding.require_fence(self.registry, restored)
        self.assertFalse(restored["delivery_enabled"])
        self.assertEqual(ledger.read_text(), '{"alpha":"unresolved nonce"}')

    def test_exact_b5a_dispatcher_upgrade_preserves_native_trust(self):
        path = self.registry.with_name("hook.py")
        body = (ROOT / "tests/fixtures/codex-dispatcher-b5a.txt").read_bytes()
        self.assertEqual(hashlib.sha256(body).hexdigest(), binding.B5A_DISPATCHER_SHA256)
        hooks = (self.cwd / ".codex/hooks.json").read_bytes()
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        path.write_bytes(body + b"\n")
        with self.assertRaisesRegex(ValueError, "dispatcher differs"):
            binding.prepare(self.argv, self.env)
        path.write_bytes(body)
        binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_text(), binding.DISPATCHER)
        self.assertEqual((self.cwd / ".codex/hooks.json").read_bytes(), hooks)
        self.assertFalse(codex_socket.load(self.registry)["delivery_enabled"])

    def test_near_b0_body_is_not_migrated(self):
        path = self.registry.with_name("hook.py")
        body = (ROOT / "tests/fixtures/codex-dispatcher-b0.txt").read_bytes() + b"\n"
        path.write_bytes(body)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        with self.assertRaisesRegex(ValueError, "dispatcher differs"):
            binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_bytes(), body)

    def test_bound_fence_requires_version_token_and_private_regular_file(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        with binding.locked(self.registry.parent):
            binding.require_fence(self.registry, bound)
        self.assertEqual(bound["binding"]["invalidation_token"], binding.request_token(self.registry))
        self.assertFalse(bound["delivery_enabled"])
        for missing in ("fence_version", "invalidation_token"):
            old = copy.deepcopy(bound)
            del old["binding"][missing]
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                binding.require_fence(self.registry, old)
        original = path.read_bytes()
        path.chmod(0o644)
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)
        path.unlink()
        with self.assertRaises(OSError):
            binding.require_fence(self.registry, bound)
        other = self.root / "other-token.json"
        other.write_bytes(original)
        other.chmod(0o600)
        path.symlink_to(other)
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)
        with self.assertRaises(ValueError):
            binding.publish_request(self.registry)
        path.unlink()
        binding.atomic(path, {"token": "not-a-uuid"})
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)

    def test_wrong_uuid_callback_poison_precedes_direct_and_dispatcher_lock(self):
        bound = self.bind_current()
        original = self.registry.read_bytes()
        event = {**self.event, "session_id": str(uuid.uuid4())}
        for native_dispatcher in (False, True):
            with self.subTest(native_dispatcher=native_dispatcher):
                before = codex_socket.load(self.registry.with_name("custody-poison.json"))["token"]
                with binding.locked(self.registry.parent):
                    if native_dispatcher:
                        result = subprocess.run(shlex.split(bound["hook_command"]), input=json.dumps(event),
                                                text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("state lock timed out", result.stderr)
                    else:
                        with self.assertRaises(TimeoutError):
                            binding.bind(self.registry, event)
                poison = codex_socket.load(self.registry.with_name("custody-poison.json"))
                self.assertNotEqual(poison["token"], before)
                self.assertEqual(poison["reason"], "callback_uuid_changed")
                self.assertEqual(self.registry.read_bytes(), original)
        # A later valid callback cannot repair the supervisor custody token.
        poison = self.registry.with_name("custody-poison.json").read_bytes()
        self.bind_current()
        self.assertEqual(self.registry.with_name("custody-poison.json").read_bytes(), poison)

    def test_daemon_change_callback_revokes_custody_without_enabling_delivery(self):
        self.bind_current()
        self.daemon.pid = 2
        try:
            record = self.bind_current()
            poison = codex_socket.load(self.registry.with_name("custody-poison.json"))
            self.assertEqual(poison["reason"], "daemon_changed")
            self.assertFalse(record["delivery_enabled"])
        finally:
            self.daemon.pid = 1

    def test_direct_lock_timeout_leaves_sticky_poison_and_durable_facts(self):
        bound = self.bind_current()
        original = self.registry.read_bytes()
        with patch.object(binding.fcntl, "flock", side_effect=BlockingIOError()), \
                patch.object(binding.time, "monotonic", side_effect=[0, 2]), self.assertRaises(TimeoutError):
            binding.invalidate(self.registry)
        self.assertEqual(self.registry.read_bytes(), original)
        self.assertNotEqual(binding.request_token(self.registry), bound["binding"]["invalidation_token"])
        with binding.locked(self.registry.parent), self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)
        # A full newer callback, not time or a lock becoming free, restores proof.
        restored = self.bind_current()
        binding.require_fence(self.registry, restored)
        self.assertFalse(restored["delivery_enabled"])

    def test_real_dispatcher_lock_timeout_poisons_before_missing_handler(self):
        bound = self.bind_current()
        bound["handler"] = str(self.root / "missing/codex_binding.py")
        binding.atomic(self.registry, bound)
        original = self.registry.read_bytes()
        with binding.locked(self.registry.parent):
            result = subprocess.run(shlex.split(bound["hook_command"]), input="{}",
                                    text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("state lock timed out", result.stderr)
            self.assertEqual(self.registry.read_bytes(), original)
            with self.assertRaises(ValueError):
                binding.require_fence(self.registry, bound)

    def test_poison_only_overlap_blocks_old_callback_even_without_generation_change(self):
        entered, release = threading.Event(), threading.Event()
        def control(record, command, args):
            if command == "ship.presence":
                entered.set()
                if not release.wait(SUBPROCESS_TIMEOUT):
                    raise TimeoutError("test did not release callback")
                return {"attended": True, "lease": {"holder": record["holder"]}}
            return {"recorded": True}
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control", control), ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(binding.bind, self.registry, self.event)
            try:
                self.assertTrue(entered.wait(5))
                before = codex_socket.load(self.registry)
                request = binding.publish_request(self.registry)
                self.assertEqual(codex_socket.load(self.registry), before)
            finally:
                release.set()
            with self.assertRaisesRegex(ValueError, "superseded"):
                future.result(timeout=5)
        self.assertEqual(binding.request_token(self.registry), request)
        self.assertEqual(codex_socket.load(self.registry), before)
        self.assertFalse(before["delivery_enabled"])

    def test_crash_at_poison_rename_and_directory_sync_never_restores_token(self):
        # Execute the actual dispatcher with fault injection in an owned process.
        # Before rename: missing token; after rename: fresh token. Neither lets
        # an old binding survive, even before registry invalidation/handler entry.
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        for stage in ("before_rename", "after_rename", "after_directory_sync"):
            with self.subTest(stage=stage):
                binding.atomic(path, {"token": bound["binding"]["invalidation_token"]})
                original = self.registry.read_bytes()
                harness = """import os,pathlib,sys
hook,stage=sys.argv[1:]
real_replace,real_fsync=os.replace,os.fsync
def replace(src,dst):
    if pathlib.Path(dst).name == 'invalidate.json':
        if stage == 'before_rename': os._exit(73)
        real_replace(src,dst)
        if stage == 'after_rename': os._exit(73)
    else: real_replace(src,dst)
def fsync(fd):
    real_fsync(fd)
    import stat
    if stage == 'after_directory_sync' and stat.S_ISDIR(os.fstat(fd).st_mode): os._exit(73)
os.replace,os.fsync=replace,fsync
exec(compile(pathlib.Path(hook).read_text(),hook,'exec'),{'__file__':hook})
"""
                result = subprocess.run(["/usr/bin/python3", "-I", "-B", "-c", harness,
                    str(self.registry.with_name("hook.py")), stage],
                    input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                self.assertEqual(result.returncode, 73, result.stderr)
                self.assertEqual(self.registry.read_bytes(), original)
                with self.assertRaises((ValueError, FileNotFoundError)):
                    binding.require_fence(self.registry, bound)

    def test_real_dispatcher_storage_failures_remove_old_fence(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        for stage in ("allocation", "write_sync", "rename"):
            with self.subTest(stage=stage):
                binding.atomic(path, {"token": bound["binding"]["invalidation_token"]})
                harness = """import os,pathlib,sys,tempfile,stat
hook,stage=sys.argv[1:]
real_mkstemp,real_fsync,real_replace=tempfile.mkstemp,os.fsync,os.replace
def mkstemp(*a,**kw):
    if stage == 'allocation': raise OSError('fixture ENOSPC')
    return real_mkstemp(*a,**kw)
def fsync(fd):
    if stage == 'write_sync' and stat.S_ISREG(os.fstat(fd).st_mode): raise OSError('fixture write failure')
    return real_fsync(fd)
def replace(src,dst):
    if stage == 'rename': raise OSError('fixture rename failure')
    return real_replace(src,dst)
tempfile.mkstemp,os.fsync,os.replace=mkstemp,fsync,replace
exec(compile(pathlib.Path(hook).read_text(),hook,'exec'),{'__file__':hook})
"""
                result = subprocess.run(["/usr/bin/python3", "-I", "-B", "-c", harness,
                    str(self.registry.with_name("hook.py")), stage],
                    input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("fixture", result.stderr)
                with self.assertRaises(FileNotFoundError):
                    binding.require_fence(self.registry, bound)
                self.assertEqual(codex_socket.load(self.registry), bound)

    def test_dispatcher_unsafe_poison_cannot_load_handler(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        original = self.registry.read_bytes()
        path.chmod(0o644)
        result = subprocess.run(shlex.split(bound["hook_command"]), input="{}",
                                text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe Codex invalidation request", result.stderr)
        self.assertEqual(self.registry.read_bytes(), original)
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)

    def test_poison_sync_failure_aborts_before_lock_or_handler(self):
        bound = self.bind_current()
        original = self.registry.read_bytes()
        real_sync = binding.sync_directory
        def fail_sync(directory):
            real_sync(directory)
            raise OSError("fixture persistence failure")
        with patch.object(binding, "sync_directory", side_effect=fail_sync), \
                patch.object(binding, "locked") as lock, self.assertRaises(OSError):
            binding.invalidate(self.registry)
        lock.assert_not_called()
        self.assertEqual(self.registry.read_bytes(), original)
        with self.assertRaises((ValueError, FileNotFoundError)):
            binding.require_fence(self.registry, bound)

    def test_prepare_rotates_poison_before_launch_record_even_on_write_failure(self):
        bound = self.bind_current()
        bound["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, bound)
        real_atomic = binding.atomic
        def atomic(path, value, **kwargs):
            if path == self.registry:
                raise OSError("fixture registry write failure")
            return real_atomic(path, value, **kwargs)
        with patch.object(binding, "atomic", side_effect=atomic), self.assertRaises(OSError):
            binding.prepare(self.argv, self.env)
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)
        binding.prepare(self.argv, self.env)
        with self.assertRaises(ValueError):
            binding.require_fence(self.registry, bound)
        self.assertFalse(codex_socket.load(self.registry)["delivery_enabled"])

    def test_poison_pre_rename_failures_remove_previous_capability(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        for function in ("mkstemp", "fsync", "replace"):
            with self.subTest(function=function):
                binding.atomic(path, {"token": bound["binding"]["invalidation_token"]})
                module = binding.tempfile if function == "mkstemp" else binding.os
                with patch.object(module, function, side_effect=OSError("fixture storage failure")), \
                        self.assertRaises(OSError):
                    binding.publish_request(self.registry)
                with self.assertRaises((ValueError, FileNotFoundError)):
                    binding.require_fence(self.registry, bound)

    def test_interleaved_poison_deletes_do_not_abort_publication(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        for stage in ("lstat", "unlink"):
            with self.subTest(stage=stage):
                binding.atomic(path, {"token": bound["binding"]["invalidation_token"]})
                real = getattr(Path, stage)
                deleted = False
                def interleave(current, *args, **kwargs):
                    nonlocal deleted
                    if current == path and not deleted:
                        deleted = True
                        # A second publisher removed this capability between
                        # our existence/stat observation and the actual syscall.
                        os.unlink(path)
                    return real(current, *args, **kwargs)
                with patch.object(Path, stage, interleave):
                    request = binding.publish_request(self.registry)
                self.assertTrue(deleted)
                self.assertEqual(binding.request_token(self.registry), request)
                with self.assertRaises(ValueError):
                    binding.require_fence(self.registry, bound)

    def test_real_dispatcher_interleaved_delete_still_publishes_and_invalidates(self):
        bound = self.bind_current()
        path = self.registry.with_name("invalidate.json")
        bound["handler"] = str(self.root / "missing/codex_binding.py")
        for stage in ("lstat", "unlink"):
            with self.subTest(stage=stage):
                binding.atomic(self.registry, bound)
                binding.atomic(path, {"token": bound["binding"]["invalidation_token"]})
                harness = """import os,pathlib,sys
hook,stage=sys.argv[1:]
real_stat,real_unlink=pathlib.Path.lstat,os.unlink
deleted=False
def interleave(path):
    global deleted
    if pathlib.Path(path).name == 'invalidate.json' and not deleted:
        deleted=True
        real_unlink(path)
def lstat(path,*a,**kw):
    if stage == 'lstat': interleave(path)
    return real_stat(path,*a,**kw)
def unlink(path,*a,**kw):
    if stage == 'unlink': interleave(path)
    return real_unlink(path,*a,**kw)
pathlib.Path.lstat,os.unlink=lstat,unlink
exec(compile(pathlib.Path(hook).read_text(),hook,'exec'),{'__file__':hook})
"""
                result = subprocess.run(["/usr/bin/python3", "-I", "-B", "-c", harness,
                    str(self.registry.with_name("hook.py")), stage],
                    input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("missing/codex_binding.py", result.stderr)
                self.assertNotEqual(binding.request_token(self.registry), bound["binding"]["invalidation_token"])
                current = codex_socket.load(self.registry)
                self.assertEqual(current["state"], "unbound")
                self.assertFalse(current["delivery_enabled"])
                with self.assertRaises(ValueError):
                    binding.require_fence(self.registry, bound)

    def test_exact_b2_dispatcher_migrates_but_near_match_refuses(self):
        path = self.registry.with_name("hook.py")
        body = (ROOT / "tests/fixtures/codex-dispatcher-b2.txt").read_bytes()
        self.assertEqual(hashlib.sha256(body).hexdigest(), binding.B2_DISPATCHER_SHA256)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        hooks = (self.cwd / ".codex/hooks.json").read_bytes()
        path.write_bytes(body + b"\n")
        with self.assertRaisesRegex(ValueError, "dispatcher differs"):
            binding.prepare(self.argv, self.env)
        path.write_bytes(body)
        binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_text(), binding.DISPATCHER)
        self.assertEqual((self.cwd / ".codex/hooks.json").read_bytes(), hooks)
        self.assertFalse(codex_socket.load(self.registry)["delivery_enabled"])

    def test_exact_b1_dispatcher_migrates_but_near_match_refuses(self):
        path = self.registry.with_name("hook.py")
        body = (ROOT / "tests/fixtures/codex-dispatcher-b1.txt").read_bytes()
        self.assertEqual(hashlib.sha256(body).hexdigest(), binding.B1_DISPATCHER_SHA256)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        path.write_bytes(body + b"\n")
        with self.assertRaisesRegex(ValueError, "dispatcher differs"):
            binding.prepare(self.argv, self.env)
        path.write_bytes(body)
        binding.prepare(self.argv, self.env)
        self.assertEqual(path.read_text(), binding.DISPATCHER)

    def test_prepare_refusal_is_visible_at_native_launch_boundary(self):
        env = {**os.environ, **self.env, "PRIVATEER_CODEX_ATTENDED": "1"}
        result = subprocess.run([sys.executable, "-B", str(ROOT / "plugins/native_launch.py"),
            "--attended-policy", '{"mode":"standard"}', "codex", "codex", "--no-daemon"],
            env=env, text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Codex attended launch refused:", result.stderr)
        self.assertIn("requires the shared daemon", result.stderr)
        self.assertNotIn("Native harness launch failed", result.stderr)

    def test_fresh_plan_is_refused_before_installing_state(self):
        with self.assertRaisesRegex(ValueError, "fresh pane identity is unproven"):
            binding.prepare([sys.executable, "--", "bearings"], self.env)
        self.assertEqual(codex_socket.load(self.registry), self.record)

    def test_unproven_sources_cannot_win_same_folder_first_callback_race(self):
        for source in ("startup", "fork", "clear"):
            # Simulate the other pane firing first: it is the sole loaded
            # thread in our cwd, with fresh UUID7, native metadata and trust.
            event = {**self.event, "source": source, "session_id": uuid7(self.record["started_ms"] + 1)}
            daemon = FakeDaemon(self.record, event)
            for state in ("unbound", "bound"):
                record = {**self.record, "state": state, "binding": {"session": self.session}}
                binding.atomic(self.registry, record)
                with self.subTest(source=source, state=state), patch.object(binding, "Daemon", return_value=daemon), \
                        patch.object(binding, "control") as control, self.assertRaisesRegex(ValueError, "identity is unproven"):
                    binding.bind(self.registry, event)
                control.assert_not_called()
                saved = codex_socket.load(self.registry)
                self.assertEqual(saved["state"], "unbound")
                self.assertNotIn("binding", saved)
                self.assertFalse(saved["delivery_enabled"])

    def test_wrong_resume_uuid_cannot_bind_or_replace_exact_launch_uuid(self):
        event = {**self.event, "session_id": str(uuid.uuid4())}
        daemon = FakeDaemon(self.record, event)
        with patch.object(binding, "Daemon", return_value=daemon), self.assertRaisesRegex(ValueError, "UUID does not match"):
            binding.bind(self.registry, event)
        saved = codex_socket.load(self.registry)
        self.assertEqual(saved["state"], "unbound")
        self.assertEqual(saved["session"], self.session)

    def test_project_hook_merge_preserves_existing_handlers(self):
        hooks = self.cwd / ".codex/hooks.json"
        value = codex_socket.load(hooks)
        value["description"] = "alpha"
        value["hooks"]["Stop"] = [{"hooks": [{"type": "command", "command": "true"}]}]
        binding.atomic(hooks, value)
        self.record["process_identity"]["bootId"] = "stale"
        binding.atomic(self.registry, self.record)
        binding.prepare(self.argv, self.env)
        self.assertEqual(codex_socket.load(hooks), value)

    def test_paginated_metadata_binds_without_inventing_rollout_evidence(self):
        value = binding.validate(self.record, self.event, self.daemon)
        self.assertEqual(value["session"], self.event["session_id"])
        self.assertEqual(value["metadata_source"], "daemon_thread_read")
        self.assertEqual(value["history_mode"], "paginated")
        self.assertFalse(Path(value["transcript"]).exists())

    def test_stale_wrong_cwd_old_session_or_unloaded_callback_is_refused(self):
        for key, value in [("cwd", "/beta"), ("source", "bogus"), ("session_id", uuid7(1))]:
            event = {**self.event, key: value}
            with self.subTest(key=key), self.assertRaises(ValueError):
                binding.validate(self.record, event, self.daemon)
        self.daemon.responses["thread/loaded/list"]["data"] = []
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)

    def test_ambiguous_cwd_and_pagination_refuse_binding(self):
        self.daemon.responses["thread/loaded/list"]["data"].append(str(uuid.uuid4()))
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            binding.validate(self.record, self.event, self.daemon)
        self.daemon.responses["thread/loaded/list"]["nextCursor"] = "more"
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)

    def test_unknown_queue_schema_and_untrusted_hook_refuse_binding(self):
        self.daemon.responses["thread/queue/list"] = {"data": []}
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)
        self.daemon.responses["thread/queue/list"]["nextCursor"] = None
        self.daemon.responses["hooks/list"]["data"][0]["hooks"][0]["trustStatus"] = "untrusted"
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)

    def test_transcript_mismatch_and_symlink_are_refused(self):
        self.daemon.responses["thread/read"]["thread"]["path"] = "/beta"
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)
        self.daemon.responses["thread/read"]["thread"]["path"] = self.event["transcript_path"]
        transcript = Path(self.event["transcript_path"])
        transcript.parent.mkdir()
        transcript.symlink_to(self.root / "home")
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)

    def test_binding_reports_only_after_owned_presence_and_keeps_generation(self):
        calls = []
        def control(record, command, args):
            calls.append((command, args))
            return {"attended": True, "lease": {"holder": record["holder"]}} if command == "ship.presence" else {"recorded": True}
        with patch.object(binding, "Daemon", return_value=self.daemon), patch.object(binding, "control", control):
            binding.bind(self.registry, self.event)
        bound = codex_socket.load(self.registry)
        self.assertEqual(bound["state"], "bound")
        self.assertEqual(bound["generation"], self.record["generation"])
        self.assertEqual(calls[-1][1]["trigger"], "delivery_bind")
        self.assertFalse(bound["delivery_enabled"])
        with patch.object(binding, "Daemon", return_value=self.daemon), patch.object(binding, "control", return_value={"attended": False}):
            with self.assertRaises(ValueError):
                binding.bind(self.registry, self.event)
        self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_failed_callback_invalidates_binding_before_validation_or_connection(self):
        for failure in ("metadata", "daemon"):
            with self.subTest(failure=failure):
                record = {**self.record, "state": "bound", "delivery_enabled": True,
                          "binding": {"session": self.event["session_id"]}}
                binding.atomic(self.registry, record)
                bad = {**self.event, "cwd": "/beta"}
                daemon = (patch.object(binding, "Daemon", side_effect=ValueError("offline"))
                          if failure == "daemon" else patch.object(binding, "Daemon", return_value=self.daemon))
                with daemon, patch.object(binding, "control") as control, self.assertRaises(ValueError):
                    binding.bind(self.registry, bad)
                control.assert_not_called()
                saved = codex_socket.load(self.registry)
                self.assertEqual(saved["state"], "unbound")
                self.assertFalse(saved["delivery_enabled"])
                self.assertNotIn("binding", saved)

    def test_new_invalidation_wins_over_in_flight_resume_without_timing_assumptions(self):
        for source in ("clear", "pruned_handler"):
            with self.subTest(source=source):
                binding.atomic(self.registry, {**self.record, "state": "bound", "delivery_enabled": True,
                                              "binding": {"session": self.session}})
                entered = threading.Event()
                release = threading.Event()
                def control(record, command, args):
                    if command == "ship.presence":
                        entered.set()
                        if not release.wait(SUBPROCESS_TIMEOUT):
                            raise TimeoutError("test did not release report")
                        return {"attended": True, "lease": {"holder": record["holder"]}}
                    return {"recorded": True}
                with patch.object(binding, "Daemon", return_value=self.daemon), \
                        patch.object(binding, "control", control), ThreadPoolExecutor(max_workers=1) as executor:
                    future = executor.submit(binding.bind, self.registry, self.event)
                    try:
                        self.assertTrue(entered.wait(5), "resume did not enter report")
                        if source == "clear":
                            with self.assertRaisesRegex(ValueError, "identity is unproven"):
                                binding.bind(self.registry, {**self.event, "source": source})
                        else:
                            record = codex_socket.load(self.registry)
                            record["handler"] = str(self.root / "missing/codex_binding.py")
                            binding.atomic(self.registry, record)
                            result = subprocess.run(shlex.split(self.record["hook_command"]),
                                input="{}", text=True, capture_output=True, timeout=SUBPROCESS_TIMEOUT)
                            self.assertNotEqual(result.returncode, 0)
                        invalidated = codex_socket.load(self.registry)
                    finally:
                        release.set()
                    with self.assertRaisesRegex(ValueError, "superseded"):
                        future.result(timeout=5)
                self.assertEqual(codex_socket.load(self.registry), invalidated)
                self.assertEqual(invalidated["state"], "unbound")
                self.assertFalse(invalidated["delivery_enabled"])
                self.assertNotIn("binding", invalidated)

    def test_state_lock_wait_is_bounded_and_does_not_drop_brief_contention(self):
        real_flock = binding.fcntl.flock
        calls = []
        def flock(fd, flags):
            calls.append(flags)
            if len(calls) == 1:
                raise BlockingIOError()
            return real_flock(fd, flags)
        with patch.object(binding.fcntl, "flock", side_effect=flock), patch.object(binding.time, "sleep"):
            binding.invalidate(self.registry)
        self.assertEqual(len(calls), 2)
        with patch.object(binding.fcntl, "flock", side_effect=BlockingIOError()), \
                patch.object(binding.time, "monotonic", side_effect=[0, 2]), self.assertRaises(TimeoutError):
            binding.invalidate(self.registry)

    def test_malformed_and_oversized_callbacks_also_invalidate_old_binding(self):
        for event in (b"{", b"x" * (codex_socket.LIMIT + 1)):
            binding.atomic(self.registry, {**self.record, "state": "bound", "binding": {"session": self.session}})
            with patch.object(binding, "Daemon") as daemon, self.assertRaises(ValueError):
                binding.bind(self.registry, event)
            daemon.assert_not_called()
            self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_compact_can_validate_previous_binding_after_invalidation(self):
        record = {**self.record, "state": "bound", "binding": {"session": self.event["session_id"]}}
        binding.atomic(self.registry, record)
        def control(record, command, args):
            self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")
            return {"attended": True, "lease": {"holder": record["holder"]}} if command == "ship.presence" else {"recorded": True}
        with patch.object(binding, "Daemon", return_value=self.daemon), patch.object(binding, "control", control):
            binding.bind(self.registry, {**self.event, "source": "compact"})
        self.assertEqual(codex_socket.load(self.registry)["state"], "bound")

    def test_overlapping_callbacks_keep_compact_continuity_without_timing_assumptions(self):
        good = {"session": self.session}
        binding.atomic(self.registry, {**self.record, "state": "bound", "binding": good,
                                      "delivery_enabled": True})
        entered = threading.Event()
        release = threading.Event()
        first_thread = []
        def control(record, command, args):
            if command == "ship.presence":
                saved = codex_socket.load(self.registry)
                self.assertEqual(saved["state"], "unbound")
                self.assertFalse(saved["delivery_enabled"])
                self.assertNotIn("binding", saved)
                self.assertEqual(saved["last_binding"], good)
                if threading.get_ident() == first_thread[0]:
                    entered.set()
                    if not release.wait(SUBPROCESS_TIMEOUT):
                        raise TimeoutError("test did not release earlier callback")
                return {"attended": True, "lease": {"holder": record["holder"]}}
            return {"recorded": True}
        def resume():
            first_thread.append(threading.get_ident())
            return binding.bind(self.registry, self.event)
        with patch.object(binding, "Daemon", return_value=self.daemon), \
                patch.object(binding, "control", control), ThreadPoolExecutor(max_workers=1) as executor:
            future = executor.submit(resume)
            try:
                self.assertTrue(entered.wait(5), "earlier callback did not reach report")
                # This newer callback sees only last_binding, not binding. It
                # completes validation while the older callback is still paused.
                compact = binding.bind(self.registry, {**self.event, "source": "compact"})
                bound = codex_socket.load(self.registry)
            finally:
                release.set()
            with self.assertRaisesRegex(ValueError, "superseded"):
                future.result(timeout=5)
        self.assertEqual(codex_socket.load(self.registry), bound)
        self.assertEqual(bound["state"], "bound")
        self.assertEqual(bound["binding"], compact)
        self.assertEqual(bound["last_binding"], compact)
        self.assertFalse(bound["delivery_enabled"])

    def test_compact_without_good_binding_or_with_wrong_last_uuid_is_refused(self):
        for last in (None, {"session": str(uuid.uuid4())}):
            record = dict(self.record)
            if last is not None:
                record["last_binding"] = last
            binding.atomic(self.registry, record)
            with patch.object(binding, "Daemon", return_value=self.daemon), \
                    self.assertRaisesRegex(ValueError, "no matching binding"):
                binding.bind(self.registry, {**self.event, "source": "compact"})
            self.assertEqual(codex_socket.load(self.registry)["state"], "unbound")

    def test_existing_rollout_metadata_is_checked(self):
        transcript = Path(self.event["transcript_path"])
        transcript.parent.mkdir()
        transcript.write_text(json.dumps({"type": "session_meta", "payload": {
            "id": self.event["session_id"], "cwd": self.record["cwd"]}}) + "\n")
        self.assertIsNotNone(binding.validate(self.record, self.event, self.daemon)["transcript_identity"])
        transcript.write_text(json.dumps({"type": "session_meta", "payload": {"id": "beta", "cwd": self.record["cwd"]}}) + "\n")
        with self.assertRaises(ValueError):
            binding.validate(self.record, self.event, self.daemon)

    def test_pid_reuse_and_unsafe_files_fail(self):
        record = copy.deepcopy(self.record)
        record["process_identity"]["startTicks"] += 1
        self.assertFalse(binding.live(record))
        self.registry.chmod(0o666)
        with self.assertRaises(ValueError):
            codex_socket.load(self.registry)

    def test_client_cannot_mutate_or_autostart_a_daemon(self):
        client = object.__new__(codex_socket.Daemon)
        for method in ["thread/queue/add", "turn/start", "thread/start"]:
            with self.assertRaises(ValueError):
                client.call(method, {})
        with self.assertRaises(OSError):
            codex_socket.Daemon(self.root / "codex")

    def test_real_websocket_peer_identity_and_dead_daemon_fail_closed(self):
        directory = self.root / "sockets"
        directory.mkdir(mode=0o700)
        target = directory / "socket"
        rendezvous = directory / "alias"
        rendezvous.symlink_to(target)
        daemon_dir = self.root / "codex/app-server-daemon"
        daemon_dir.mkdir()
        parent, child = multiprocessing.Pipe()
        server = multiprocessing.get_context("fork").Process(target=socket_server,
            args=(str(target), child, str(self.root / "codex"), False))
        server.start()
        try:
            self.assertTrue(parent.poll(5))
            self.assertTrue(parent.recv())
            binding.atomic(daemon_dir / "daemon.pid", {"pid": server.pid,
                "processIdentity": codex_socket.process_identity(server.pid)})
            with patch.object(codex_socket, "socket_paths", return_value=(rendezvous, target)):
                with codex_socket.Daemon(self.root / "codex") as daemon:
                    self.assertEqual(daemon.pid, server.pid)
                    self.assertEqual(daemon.call("thread/queue/list", {"threadId": "alpha"}),
                                     {"data": [], "nextCursor": None})
                server.join(5)
                self.assertFalse(server.is_alive())
                with self.assertRaises((ValueError, OSError)):
                    codex_socket.Daemon(self.root / "codex")
        finally:
            if server.is_alive():
                server.terminate()
            server.join(5)
            parent.close()
            child.close()

    def test_control_report_is_bounded_and_carries_launch_state(self):
        script = self.root / "control"
        script.write_text("#!/usr/bin/env python3\nimport json,os\nprint(json.dumps({'home':os.environ['HOME'],'actor':os.environ['PRIVATEER_ACTOR']}))\n")
        script.chmod(0o700)
        self.record["control"] = str(script)
        self.assertEqual(binding.control(self.record, "ship.presence", {}),
                         {"home": self.env["HOME"], "actor": "alpha"})
        script.write_text("#!/usr/bin/env python3\nimport json,os\nprint(json.dumps(dict(os.environ)))\n")
        self.record["path_env"]["PRIVATEER_DAEMON_SECRET"] = "must-not-leak"
        self.record["path_env"]["PRIVATEER_SERVICE"] = "none"
        with patch.dict(os.environ, {"PRIVATEER_DAEMON_SECRET": "must-not-leak", "PRIVATEER_JOB": "foreign-job",
                                   "PRIVATEER_ACTOR": "beta", "PYTHONPATH": "/beta", "LD_PRELOAD": "/beta"}):
            reported = binding.control(self.record, "ship.presence", {})
        self.assertNotIn("PRIVATEER_DAEMON_SECRET", reported)
        self.assertNotIn("PRIVATEER_JOB", reported)
        self.assertNotIn("PYTHONPATH", reported)
        self.assertNotIn("LD_PRELOAD", reported)
        self.assertEqual(reported["PRIVATEER_ACTOR"], "alpha")
        self.assertEqual(reported["PRIVATEER_SERVICE"], "none")
        self.assertEqual(reported["PRIVATEER_FLEET"], self.env["PRIVATEER_FLEET"])
        script.write_text("#!/usr/bin/env python3\nprint('x' * (2 << 20))\n")
        with self.assertRaises(ValueError):
            binding.control(self.record, "ship.presence", {})


class ReceiptTests(unittest.TestCase):
    # Captured from the real 0.166.0 B4 loopback probe, not invented fields.
    native_events = [json.loads(line)["payload"] for line in
                     (ROOT / "tests/fixtures/codex-queue-accepted.jsonl").read_text().splitlines()]
    client_id = native_events[1]["client_id"]
    setUp = BindingTests.setUp
    bind_current = BindingTests.bind_current

    def intent(self, after=0, through=4, session=None, padding=0):
        record = self.bind_current()
        if session:
            record["session"] = record["binding"]["session"] = session
        self.transcript = self.root / "codex/sessions/alpha.jsonl"
        self.transcript.parent.mkdir(exist_ok=True)
        if not self.transcript.exists():
            self.transcript.write_text(json.dumps({"type": "session_meta", "payload": {
                "id": record["session"], "cwd": record["cwd"], "history_mode": "legacy"}}) + "\n")
        info = self.transcript.stat()
        record["binding"].update(history_mode="legacy", transcript_identity={"device": info.st_dev, "inode": info.st_ino})
        self.ledger = receipts.Ledger(self.registry, record["home"], record["ship"])
        self.nonce = str(uuid.uuid4())
        self.body = receipts.encode(receipts.marker(record["home"], "alpha", record["session"], self.nonce, after, through)) + "\nNotification not acknowledgement; inbox headers only." + "x" * padding
        self.offer_record = record
        return self.ledger.prepare(record, self.body, after, through, self.nonce)

    def append(self, *events):
        with self.transcript.open("a") as stream:
            for event in events:
                stream.write(json.dumps({"type": "event_msg", "payload": event}) + "\n")

    def reconcile(self, queue=None, ack=0, failure=False):
        return self.ledger.reconcile(self.session, self.nonce,
            queue if queue is not None else {"data": [], "nextCursor": None}, ack, failure)

    def native_input(self, body=None):
        return {**copy.deepcopy(self.native_events[1]), "message": self.body if body is None else body}

    def promote(self):
        self.append({"type": "task_started", "turn_id": "alpha-turn", "started_at": 123,
                     "turn_attribution": {"turn_trigger": "queue", "turn_id": "alpha-turn"}},
                    self.native_input(),
                    {"type": "task_complete", "turn_id": "alpha-turn"})

    def test_captured_native_queue_record_settles_anchored_receipt(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        events = copy.deepcopy(self.native_events)
        events[1]["message"] = self.body  # only substitute the per-test mail body
        self.append(*events)
        result = self.reconcile()
        self.assertEqual(result["state"], "presented")
        self.assertEqual(result["evidence"]["receipt"], {"turn_id": events[0]["turn_id"],
                                                        "client_id": events[1]["client_id"]})
        self.assertNotIn("turn_id", events[1])
        self.assertEqual(result["accepted_through"], 4)
        self.assertEqual(result["acknowledged_through"], 0)

    def test_unqualified_native_input_shape_pauses_with_clear_reason(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        original = self.transcript.read_bytes()
        good = self.native_input()
        missing = {key: value for key, value in good.items() if key != "images"}
        alias = {key: value for key, value in good.items() if key != "client_id"}
        alias["clientId"] = self.client_id
        for event in (missing, alias, {**good, "client_id": None}, {**good, "client_id": "alpha"},
                      {**good, "turn_id": "alpha"}, {**good, "audio": ["alpha"]},
                      {**good, "text_elements": None}):
            with self.subTest(event=event):
                self.transcript.write_bytes(original)
                self.append({"type": "task_started", "turn_id": "alpha-turn"}, event)
                result = self.reconcile()
                self.assertTrue(result["paused"])
                self.assertEqual(result["state"], "uncertain")
                self.assertIn("user_message schema unqualified", result["evidence"]["pause_reason"])
                self.assertEqual(result["accepted_through"], 0)
        self.transcript.write_bytes(original)
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_lost_response_recovers_from_anchored_promotion_without_cli_receipt(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.promote()
        result = self.reconcile()
        self.assertEqual(result["state"], "presented")
        self.assertEqual(result["accepted_through"], 4)
        self.assertEqual(result["acknowledged_through"], 0)
        self.assertEqual(result["evidence"]["receipt"], {"turn_id": "alpha-turn", "client_id": self.client_id})
        # New process and same UUID: durable receipt despite unacknowledged mail.
        self.ledger = receipts.Ledger(self.registry, self.record["home"], "alpha")
        self.assertEqual(self.reconcile()["accepted_through"], 4)
        with self.assertRaisesRegex(ValueError, "replay"):
            self.intent()

    def test_queue_presence_is_admission_not_presentation_or_ack(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        queue = {"data": [{"id": "alpha-queue", "clientUserMessageId": self.client_id, "input": [
            {"type": "text", "text": self.body, "text_elements": []}]}], "nextCursor": "more"}
        result = self.reconcile(queue)
        self.assertEqual(result["state"], "queued")
        self.assertEqual(result["accepted_through"], 0)
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_queue_and_promotion_ids_are_independent_including_b4_recovery(self):
        for legacy in (False, True):
            with self.subTest(legacy=legacy):
                self.intent(after=4 if legacy else 0, through=8 if legacy else 4)
                self.ledger.invoking(self.session, self.nonce)
                queue_client = str(uuid.uuid4())
                queue = {"data": [{"id": "alpha-queue", "clientUserMessageId": queue_client, "input": [
                    {"type": "text", "text": self.body, "text_elements": []}]}], "nextCursor": None}
                self.assertEqual(self.reconcile(queue)["evidence"]["queue_client_id"], queue_client)
                if legacy:
                    value = self.ledger.read()
                    offer = value["sessions"][self.session]["intent"]
                    offer["client_id"] = offer.pop("queue_client_id")
                    binding.atomic(self.ledger.path, value)
                    migrated = self.reconcile(queue)["evidence"]
                    self.assertEqual(migrated["queue_client_id"], queue_client)
                    self.assertNotIn("client_id", migrated)
                # Persisted fact still guards against queue-to-queue ID changes.
                changed = copy.deepcopy(queue)
                changed["data"][0]["clientUserMessageId"] = str(uuid.uuid4())
                self.assertEqual(self.reconcile(changed)["state"], "uncertain")
                self.promote()
                self.ledger = receipts.Ledger(self.registry, self.record["home"], "alpha")
                result = self.reconcile(queue)
                self.assertEqual(result["state"], "presented")
                self.assertEqual(result["evidence"]["queue_client_id"], queue_client)
                self.assertEqual(result["evidence"]["receipt"]["client_id"], self.client_id)
                self.assertNotEqual(queue_client, self.client_id)
                self.assertEqual(result["acknowledged_through"], 0)
                self.assertFalse(result["paused"])

    def test_queue_item_id_change_preserves_original_evidence_and_pauses(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        queue = {"data": [{"id": "first", "clientUserMessageId": self.client_id, "input": [
            {"type": "text", "text": self.body, "text_elements": []}]}], "nextCursor": None}
        self.assertEqual(self.reconcile(queue)["state"], "queued")
        queue["data"][0]["id"] = "second"
        result = self.reconcile(queue)
        self.assertEqual(result["state"], "uncertain")
        self.assertEqual(result["evidence"]["queue_id"], "first")
        self.assertEqual(result["accepted_through"], 0)

    def test_queue_attribution_id_must_match_in_scan_and_anchor_recovery(self):
        for bad_id in (None, "different-turn"):
            for recovery in (False, True):
                with self.subTest(bad_id=bad_id, recovery=recovery):
                    # Each negative case gets its own isolated ledger/rollout.
                    if hasattr(self, "transcript"):
                        self.transcript.unlink()
                        self.ledger.path.unlink()
                    self.intent()
                    self.append({"type": "task_started", "turn_id": "alpha-turn",
                                 "turn_attribution": {"turn_trigger": "queue", "turn_id": bad_id}})
                    self.ledger.invoking(self.session, self.nonce)
                    if recovery:
                        # Simulate an old checkpoint that had accepted mismatched
                        # attribution; v2 must revalidate its saved boundary.
                        value = self.ledger.read()
                        intent = value["sessions"][self.session]["intent"]
                        a = intent["scan"]
                        a["offset"] = self.transcript.stat().st_size
                        with self.transcript.open("rb") as stream:
                            a["boundary_hash"] = receipts.checkpoint(stream, a["offset"])
                        intent["open_turn"] = "alpha-turn"
                        intent["open_turn_trigger"] = "queue"
                        intent.pop("lifecycle_version")
                        binding.atomic(self.ledger.path, value)
                    self.append(self.native_input())
                    result = self.reconcile()
                    self.assertEqual(result["state"], "uncertain")
                    self.assertEqual(result["accepted_through"], 0)
                    self.assertIsNotNone(result["diagnostic"])

    def test_prepare_seeds_anchor_lifecycle_without_recomputing_it(self):
        self.intent()
        self.reconcile(ack=4)
        self.append({"type": "task_started", "turn_id": "mid-turn",
                     "turn_attribution": {"turn_trigger": "queue", "turn_id": "mid-turn"}})
        offer = self.intent(after=4, through=8)
        self.assertEqual(offer["open_turn"], offer["scan"]["open_turn"])
        self.assertEqual(offer["open_turn_trigger"], "queue")
        self.ledger.invoking(self.session, self.nonce)
        self.append(self.native_input())
        with patch.object(receipts, "open_turn_at", side_effect=AssertionError("redundant lookup")):
            self.assertEqual(self.reconcile()["state"], "presented")

    def test_noncanonical_queue_client_id_is_not_admission_evidence(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        queue = {"data": [{"id": "alpha-queue", "clientUserMessageId": "not-a-uuid", "input": [
            {"type": "text", "text": self.body, "text_elements": []}]}], "nextCursor": None}
        result = self.reconcile(queue)
        self.assertEqual(result["state"], "uncertain")
        self.assertEqual(result["accepted_through"], 0)
        self.assertNotIn("queue_client_id", result["evidence"])

    def test_receipt_requires_queue_trigger_on_its_open_turn(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        original = self.transcript.read_bytes()
        for attribution in (None, {}, {"turn_trigger": "user"}, "queue"):
            with self.subTest(attribution=attribution):
                self.transcript.write_bytes(original)
                self.append({"type": "task_started", "turn_id": "alpha-turn",
                             "turn_attribution": attribution}, self.native_input())
                result = self.reconcile()
                self.assertEqual(result["state"], "uncertain")
                self.assertEqual(result["accepted_through"], 0)
                self.assertIsNotNone(result["diagnostic"])
        self.transcript.write_bytes(original)
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_mid_turn_trigger_and_old_checkpoint_recovery(self):
        self.intent()
        self.reconcile(ack=4)
        baseline = self.ledger.read()
        original = self.transcript.read_bytes()
        for trigger, legacy in (("user", False), ("user", True), ("queue", True)):
            with self.subTest(trigger=trigger, legacy=legacy):
                binding.atomic(self.ledger.path, baseline)
                self.transcript.write_bytes(original)
                self.append({"type": "task_started", "turn_id": "mid-turn",
                             "turn_attribution": {"turn_trigger": trigger, "turn_id": "mid-turn"}})
                offer = self.intent(after=4, through=8)
                self.assertEqual(offer["scan"]["open_turn_trigger"], trigger)
                self.ledger.invoking(self.session, self.nonce)
                self.reconcile()  # checkpoint in the middle of this turn
                if legacy:
                    value = self.ledger.read()
                    intent = value["sessions"][self.session]["intent"]
                    del intent["open_turn_trigger"]
                    intent["scan"].pop("open_turn_trigger", None)
                    binding.atomic(self.ledger.path, value)
                self.append(self.native_input())
                self.ledger = receipts.Ledger(self.registry, self.record["home"], "alpha")
                result = self.reconcile()
                self.assertEqual(result["state"], "presented" if trigger == "queue" else "uncertain")
                self.assertEqual(result["accepted_through"], 8 if trigger == "queue" else 4)

    def test_no_evidence_never_proves_rejection_and_ack_self_heals(self):
        self.intent()
        self.assertEqual(self.reconcile()["state"], "prepared")
        self.ledger.invoking(self.session, self.nonce)
        for _ in range(3):
            result = self.reconcile(ack=2)
            self.assertEqual(result["state"], "uncertain")
            self.assertTrue(result["paused"])
            self.assertEqual(result["accepted_through"], 0)
            nonce = self.nonce
            with self.assertRaisesRegex(ValueError, "outstanding"):
                self.intent()
            self.nonce = nonce
        result = self.reconcile(ack=4)
        self.assertEqual(result["state"], "acked")
        self.assertNotIn("receipt", result["evidence"])
        self.assertEqual(result["accepted_through"], 4)
        self.assertIsNone(result["diagnostic"])
        self.assertFalse(result["paused"])

    def test_uncertainty_has_immediate_bounded_stable_diagnostic_and_auto_clears(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        first = self.reconcile()
        diagnostic = first["diagnostic"]
        self.assertEqual(diagnostic["kind"], "admission_uncertain")
        self.assertEqual(diagnostic["notification_id"], self.nonce)
        self.assertLess(len(json.dumps(diagnostic).encode()), 1024)
        self.assertNotIn(self.body, json.dumps(diagnostic))
        self.assertEqual(self.reconcile()["diagnostic"], diagnostic)
        self.promote()
        result = self.reconcile()
        self.assertEqual(result["state"], "presented")
        self.assertIsNone(result["diagnostic"])
        self.assertFalse(result["paused"])

    def test_operator_only_resolution_is_atomic_audited_and_never_acks(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        before = self.ledger.path.read_bytes()
        for role in ("agent", "crew", None):
            with self.subTest(role=role), self.assertRaises(PermissionError):
                self.ledger.resolve_operator(self.session, self.nonce, "abandon", "alpha review", 123,
                                             caller_role=role)
            self.assertEqual(self.ledger.path.read_bytes(), before)
        result = self.ledger.resolve_operator(self.session, self.nonce, "abandon", "alpha review", 123,
                                             caller_role="operator")
        self.assertEqual(result["state"], "abandoned")
        self.assertEqual(result["audit"]["notification_id"], self.nonce)
        self.assertEqual(result["audit"]["actor"], "operator")
        self.assertEqual(result["audit"]["at_ms"], 123)
        saved = self.reconcile()
        self.assertEqual(saved["accepted_through"], 0)
        self.assertEqual(saved["acknowledged_through"], 0)
        self.assertIsNone(saved["diagnostic"])
        with self.assertRaisesRegex(ValueError, "operator-abandoned"):
            self.intent()
        # Abandonment is not cancellation or acknowledgement. A later actual
        # runtime ack can still settle the exact old offer without operator help.
        self.nonce = result["audit"]["notification_id"]
        self.assertEqual(self.reconcile(ack=4)["state"], "acked")

    def test_operator_mark_presented_has_distinct_provenance_and_stale_nonce_refuses(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        with self.assertRaisesRegex(ValueError, "nonce/body"):
            self.ledger.resolve_operator(self.session, str(uuid.uuid4()), "mark_presented", "alpha", 123,
                                         caller_role="operator")
        result = self.ledger.resolve_operator(self.session, self.nonce, "mark_presented", "alpha review", 123,
                                             caller_role="operator")
        self.assertEqual(result["state"], "operator_presented")
        saved = self.reconcile()
        self.assertEqual(saved["accepted_through"], 4)
        self.assertEqual(saved["acknowledged_through"], 0)
        self.assertNotIn("receipt", saved["evidence"])
        self.assertIsNone(saved["diagnostic"])

    def test_only_pre_request_failure_can_reject_an_attempt(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.assertEqual(self.reconcile(failure=True)["state"], "rejected")
        self.assertEqual(self.reconcile()["accepted_through"], 0)

    def test_quoted_marker_and_turn_start_alone_are_not_receipts(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.append({"type": "task_started", "turn_id": "alpha-turn"},
                    {"type": "agent_message", "message": self.body},
                    {"type": "task_complete", "turn_id": "alpha-turn"})
        self.assertEqual(self.reconcile()["state"], "uncertain")
        with self.transcript.open("a") as stream:
            stream.write(json.dumps({"type": "response_item", "payload": {"role": "user", "text": self.body}}) + "\n")
        self.assertEqual(self.reconcile()["state"], "uncertain")

    def test_native_input_requires_open_turn_and_exact_body(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.append({"type": "task_started", "turn_id": "alpha-turn"},
                    self.native_input(self.body + "quoted"),
                    {"type": "task_complete", "turn_id": "alpha-turn"},
                    self.native_input())
        result = self.reconcile()
        self.assertTrue(result["paused"])
        self.assertEqual(result["accepted_through"], 0)
        self.assertIn("pause_reason", result["evidence"])

    def test_incremental_recovery_beyond_tail_and_record_split(self):
        original = self.intent()
        self.ledger.invoking(self.session, self.nonce)
        # Place receipt beyond two bounded scans, then more than any tail window.
        filler = {"type": "agent_message", "message": "x" * 3000}
        self.append(*([filler] * 750))
        self.append({"type": "task_started", "turn_id": "alpha-turn",
                     "turn_attribution": {"turn_trigger": "queue", "turn_id": "alpha-turn"}})
        result = self.reconcile()
        self.assertLessEqual(result["evidence"]["scan"]["offset"] - original["scan"]["offset"], receipts.LIMIT)
        self.assertFalse(result["evidence"]["scan_caught_up"])
        self.reconcile()
        result = self.reconcile()
        self.assertEqual(result["evidence"]["open_turn"], "alpha-turn")
        offset = result["evidence"]["scan"]["offset"]
        line = json.dumps({"type": "event_msg", "payload": self.native_input()})
        with self.transcript.open("a") as stream:
            stream.write(line[:30])
        self.assertEqual(self.reconcile()["evidence"]["scan"]["offset"], offset)
        with self.transcript.open("a") as stream:
            stream.write(line[30:] + "\n")
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_truncation_rotation_compaction_and_unknown_schema_pause(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        original = self.transcript.read_bytes()
        for mode in ("truncate", "replace", "compact", "schema"):
            with self.subTest(mode=mode):
                if mode == "truncate":
                    self.transcript.write_bytes(b"")
                elif mode == "replace":
                    self.transcript.rename(self.transcript.with_suffix(".old"))
                    self.transcript.write_bytes(original)
                elif mode == "compact":
                    self.append({"type": "context_compacted"})
                else:
                    self.append({"type": "turn_started", "turn_id": "alpha"})
                result = self.reconcile()
                self.assertTrue(result["paused"])
                self.assertEqual(result["accepted_through"], 0)
                self.assertNotEqual(result["state"], "rejected")
                if mode == "replace":
                    self.transcript.unlink()
                    self.transcript.with_suffix(".old").rename(self.transcript)
                self.transcript.write_bytes(original)

    def test_paginated_missing_symlink_and_foreign_uuid_refuse_anchor(self):
        self.intent()
        bound = self.offer_record["binding"]
        for mode in ("paginated", "future", None):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "no idle mail"):
                receipts.anchor({**bound, "history_mode": mode}, self.record["cwd"])
        foreign = {**bound, "session": str(uuid.uuid4())}
        with self.assertRaisesRegex(ValueError, "metadata mismatch"):
            receipts.anchor(foreign, self.record["cwd"])
        self.transcript.rename(self.transcript.with_suffix(".old"))
        with self.assertRaises(OSError):
            receipts.anchor(bound, self.record["cwd"])
        self.transcript.symlink_to(self.transcript.with_suffix(".old"))
        with self.assertRaises(ValueError):
            receipts.anchor(bound, self.record["cwd"])

    def test_atomic_restart_storage_bound_and_preserved_history(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.promote()
        self.reconcile()
        nonce = self.nonce
        self.intent(after=4, through=6)
        saved = self.ledger.read()
        state = saved["sessions"][self.session]
        self.assertEqual(state["history"], [])
        self.assertEqual(state["nonce_digests"], [receipts.digest(nonce.encode())])
        self.assertEqual(state["accepted_through"], 4)
        before = self.ledger.path.read_bytes()
        with patch.object(receipts, "MAX_LEDGER", len(before) - 1), self.assertRaises(ValueError):
            self.ledger.invoking(self.session, self.nonce)
        self.assertEqual(self.ledger.path.read_bytes(), before)
        with patch.object(receipts, "atomic", side_effect=OSError("fixture disk failure")), self.assertRaises(OSError):
            self.ledger.invoking(self.session, self.nonce)
        self.assertEqual(self.ledger.path.read_bytes(), before)

    def test_mid_turn_anchor_closes_then_recovers_later_receipt(self):
        self.intent()
        self.reconcile(ack=4)
        self.append({"type": "task_started", "turn_id": "captain-turn"})
        intent = self.intent(after=4, through=6)
        self.assertEqual(intent["scan"]["open_turn"], "captain-turn")
        self.ledger.invoking(self.session, self.nonce)
        first = self.reconcile()
        self.assertEqual(first["evidence"]["open_turn"], "captain-turn")
        self.append({"type": "task_complete", "turn_id": "captain-turn"})
        self.assertIsNone(self.reconcile()["evidence"]["open_turn"])
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")
        self.assertEqual(self.reconcile()["accepted_through"], 6)

    def test_anchor_lifecycle_lookup_crosses_block_and_record_boundaries(self):
        self.intent()
        self.reconcile(ack=4)
        self.append({"type": "task_started", "turn_id": "captain-turn"},
                    {"type": "agent_message", "message": "x" * 200000})
        intent = self.intent(after=4, through=6)
        self.assertEqual(intent["scan"]["open_turn"], "captain-turn")
        self.ledger.invoking(self.session, self.nonce)
        self.append({"type": "turn_aborted", "turn_id": "captain-turn"})
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_b2_mid_turn_anchor_recovers_at_saved_offset_not_eof(self):
        self.intent()
        self.reconcile(ack=4)
        self.append({"type": "task_started", "turn_id": "captain-turn"})
        self.intent(after=4, through=6)
        value = self.ledger.read()
        del value["sessions"][self.session]["intent"]["scan"]["open_turn"]
        binding.atomic(self.ledger.path, value)
        self.ledger.invoking(self.session, self.nonce)
        self.append({"type": "turn_aborted", "turn_id": "captain-turn"})
        self.promote()
        self.assertEqual(self.reconcile()["state"], "presented")

    def test_anchor_lookup_is_bounded_and_terminal_mismatch_still_pauses(self):
        self.intent()
        self.reconcile(ack=4)
        self.append({"type": "task_started", "turn_id": "captain-turn"})
        self.intent(after=4, through=6)
        self.ledger.invoking(self.session, self.nonce)
        self.append({"type": "task_complete", "turn_id": "wrong-turn"})
        self.assertTrue(self.reconcile()["paused"])
        self.append(*([{ "type": "agent_message", "message": "x" * 3000}] * 360))
        with self.assertRaisesRegex(ValueError, "lookup budget"):
            receipts.anchor(self.offer_record["binding"], self.record["cwd"])

    def test_non_object_ledger_refuses_as_value_error(self):
        self.intent()
        for value in ([], None, "alpha", 3):
            binding.atomic(self.ledger.path, value)
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "schema mismatch"):
                self.ledger.read()

    def test_unchanged_reconcile_does_not_rewrite_ledger(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        with patch.object(receipts, "atomic") as atomic:
            for _ in range(3):
                self.assertEqual(self.reconcile()["state"], "uncertain")
            atomic.assert_not_called()
        self.promote()
        with patch.object(receipts, "atomic", wraps=receipts.atomic) as atomic:
            self.assertEqual(self.reconcile()["state"], "presented")
            atomic.assert_called_once()

    def test_settled_offer_compaction_keeps_digests_watermarks_and_operator_audit(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        audit = self.ledger.resolve_operator(self.session, self.nonce, "mark_presented", "alpha review", 123,
                                              caller_role="operator")["audit"]
        old_nonce = self.nonce
        self.intent(after=4, through=6)
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(state["history"], [])
        self.assertEqual(state["operator_audits"], [audit])
        self.assertIn(receipts.digest(old_nonce.encode()), state["nonce_digests"])
        self.reconcile(ack=6)
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, old_nonce, 6, 8))
        with self.assertRaisesRegex(ValueError, "nonce reused"):
            self.ledger.prepare(self.offer_record, body, 6, 8, old_nonce)
        # Many large settled bodies do not accumulate in history.
        for i in range(30):
            self.intent(after=6 + i * 2, through=8 + i * 2, padding=300000)
            self.reconcile(ack=8 + i * 2)
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(state["history"], [])
        self.assertEqual(len(state["nonce_digests"]), 31)  # Latest intent stays pinned.
        self.assertLess(self.ledger.path.stat().st_size, 400000)
        self.assertEqual(state["accepted_through"], 66)
        self.assertEqual(state["operator_audits"], [audit])

    def test_b2_history_compacts_transactionally_without_dropping_uncovered_evidence(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        offer = self.ledger.read()["sessions"][self.session]["intent"]
        value = self.ledger.read()
        state = value["sessions"][self.session]
        state["history"] = [{**offer, "nonce": str(uuid.uuid4()), "state": "acked", "through_seq": 2},
                            {**offer, "nonce": str(uuid.uuid4()), "state": "abandoned", "through_seq": 8}]
        state["accepted_through"] = 2
        binding.atomic(self.ledger.path, value)
        before = self.ledger.path.read_bytes()
        with patch.object(receipts, "atomic", side_effect=OSError("fixture disk failure")), self.assertRaises(OSError):
            self.reconcile()
        self.assertEqual(self.ledger.path.read_bytes(), before)
        self.reconcile()
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(len(state["nonce_digests"]), 1)
        self.assertEqual(len(state["history"]), 1)
        self.assertEqual(state["history"][0]["through_seq"], 8)
        self.assertEqual(state["intent"]["nonce"], self.nonce)
        self.assertEqual(state["intent"]["body"], offer["body"])

    def test_rejected_and_abandoned_bodies_compact_without_acceptance(self):
        first_nonce = None
        # Each case exceeds the old 8 MiB cap in cumulative body bytes.
        for operation in ("reject", "abandon"):
            for i in range(32):
                start = 0 if operation == "reject" else i * 4
                self.intent(after=start, through=start + 4, padding=300000)
                first_nonce = first_nonce or self.nonce
                self.ledger.invoking(self.session, self.nonce)
                if operation == "reject":
                    self.assertEqual(self.reconcile(failure=True)["state"], "rejected")
                else:
                    self.reconcile()
                    self.ledger.resolve_operator(self.session, self.nonce, "abandon", "alpha", i,
                                                 caller_role="operator")
                state = self.ledger.read()["sessions"][self.session]
                self.assertEqual(state.get("history", []), [])
                self.assertEqual(state["accepted_through"], 0)
                self.assertEqual(state["acknowledged_through"], 0)
                self.assertLess(self.ledger.path.stat().st_size, 400000)
        self.intent(after=128, through=132)
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(len(state["operator_audits"]), 32)
        for i, audit in enumerate(state["operator_audits"]):
            self.assertEqual((audit["after_seq"], audit["through_seq"]), (i * 4, i * 4 + 4))
            self.assertEqual(len(audit["body_hash"]), 64)
        self.reconcile(ack=132)
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, first_nonce, 132, 136))
        with self.assertRaisesRegex(ValueError, "nonce reused"):
            self.ledger.prepare(self.offer_record, body, 132, 136, first_nonce)

    def hexbits(self, bits):
        return format(bits, f"0{receipts.NONCE_FILTER_BITS // 4}x")

    def filled(self, ratio, avoid=None):
        """A filter at exactly `ratio` fill that is a definite negative for `avoid`."""
        count = int(receipts.NONCE_FILTER_BITS * ratio)
        skip = set(receipts.nonce_bits(receipts.digest(avoid.encode()))) if avoid else set()
        bits, bit = 0, 0
        while bin(bits).count("1") < count:
            if bit not in skip:
                bits |= 1 << bit
            bit += 1
        return self.hexbits(bits)

    def test_nonce_window_migrates_without_forgetting_evicted_hashes(self):
        self.intent()
        self.reconcile(ack=4)
        old_nonce = str(uuid.uuid4())
        other_session = str(uuid.uuid4())
        value = self.ledger.read()
        # Older ledgers have only the unbounded digest list, not a filter.
        hashes = [receipts.digest(old_nonce.encode())] + [receipts.digest(str(i).encode()) for i in range(400)]
        value["sessions"][other_session] = {"accepted_through": 0, "acknowledged_through": 0,
                                            "nonce_digests": hashes}
        value["sessions"][self.session]["nonce_digests"] = hashes
        binding.atomic(self.ledger.path, value)
        self.reconcile()
        for session in (other_session, self.session):
            saved = self.ledger.read()["sessions"][session]
            self.assertEqual(len(saved["nonce_digests"]), receipts.MAX_NONCE_DIGESTS)
            self.assertEqual(len(saved["nonce_filter"]), receipts.NONCE_FILTER_BITS // 4)
            self.assertNotIn(hashes[0], saved["nonce_digests"])
            self.assertTrue(all(receipts.seen_nonce(saved, h) for h in hashes))
        # The evicted digest still refuses in its own session's filter.
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, old_nonce, 4, 8))
        with self.assertRaises(receipts.NonceFilterClosed) as closed:
            receipts.Ledger(self.registry, self.record["home"], "alpha").prepare(
                self.offer_record, body, 4, 8, old_nonce)
        self.assertEqual(closed.exception.diagnostic["kind"], "nonce_filter_possible_reuse")
        self.assertEqual(closed.exception.diagnostic["session"], self.session)
        # Another session's saturated filter no longer blocks this one; a
        # corrupt filter anywhere still fails closed.
        for filter_value, blocks in (("f" * (receipts.NONCE_FILTER_BITS // 4), False), ("bad", True)):
            value = self.ledger.read()
            value["sessions"][other_session]["nonce_filter"] = filter_value
            value["sessions"][self.session].pop("nonce_filter")
            value["sessions"][self.session]["nonce_digests"] = []
            binding.atomic(self.ledger.path, value)
            nonce = str(uuid.uuid4())
            body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, nonce, 4, 8))
            if blocks:
                with self.assertRaises(ValueError):
                    self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
            else:
                self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
                self.nonce = nonce
                self.reconcile(ack=8)
                value = self.ledger.read()
                value["sessions"][self.session]["intent"]["state"] = "acked"
                value["sessions"][self.session]["accepted_through"] = 4
                binding.atomic(self.ledger.path, value)

    def test_filter_collision_and_capacity_have_distinct_closed_diagnostics(self):
        self.intent()
        self.reconcile(ack=4)
        value = self.ledger.read()
        nonce = str(uuid.uuid4())
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, nonce, 4, 8))
        bits = sum(1 << bit for bit in set(receipts.nonce_bits(receipts.digest(nonce.encode()))))
        state = value["sessions"][self.session]
        # Force a false positive for a never-used nonce, not exact reuse.
        state["nonce_filter"] = self.hexbits(bits)
        for saturated in (False, True):
            if saturated:
                # A quiescent session would roll over; with no archive room
                # left, saturation is backpressure exactly as before.
                state["nonce_filter"] = self.filled(0.25, avoid=nonce)
                state["filter_generations"] = [{}] * receipts.MAX_FILTER_GENERATIONS
            binding.atomic(self.ledger.path, value)
            before = self.ledger.path.read_bytes()
            with self.assertRaises(receipts.NonceFilterClosed) as closed:
                self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
            diag = closed.exception.diagnostic
            self.assertEqual(diag["kind"], "nonce_filter_saturated" if saturated else "nonce_filter_possible_reuse")
            self.assertEqual(diag["session"], self.session)
            self.assertEqual(diag["fill_ratio"], 0.25 if saturated else len(set(receipts.nonce_bits(receipts.digest(nonce.encode())))) / receipts.NONCE_FILTER_BITS)
            self.assertIn("do not resample", diag["next"])
            self.assertEqual(self.ledger.path.read_bytes(), before)

    def test_other_session_filters_never_stall_the_target_session(self):
        self.intent()
        self.reconcile(ack=4)
        nonce = str(uuid.uuid4())
        # B5b: n nearly full filters each refusing ~0.39% compound to ~39% at
        # 128. Now 127 other sessions whose filters ALL match this nonce, one
        # saturated, do not refuse it; only exact reuse spans sessions.
        bits = 0
        for bit in receipts.nonce_bits(receipts.digest(nonce.encode())):
            bits |= 1 << bit
        value = self.ledger.read()
        others = {}
        for i in range(receipts.MAX_SESSIONS - 1):
            others[str(uuid.uuid4())] = {"accepted_through": 7, "acknowledged_through": 3, "nonce_digests": [],
                "nonce_filter": self.filled(0.25) if i == 0 else self.hexbits(bits)}
        value["sessions"].update(others)
        binding.atomic(self.ledger.path, value)
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, nonce, 4, 8))
        self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
        self.nonce = nonce
        self.reconcile(ack=8)
        saved = self.ledger.read()
        for session, state in others.items():
            self.assertEqual(saved["sessions"][session], state)
        self.assertEqual(saved["sessions"][self.session]["accepted_through"], 8)
        # The saturated session itself still refuses when it cannot roll over.
        with self.assertRaises(receipts.NonceFilterClosed) as closed:
            receipts.check_nonce(saved["sessions"], str(uuid.uuid4()), next(iter(others)))
        self.assertEqual(closed.exception.diagnostic["kind"], "nonce_filter_saturated")
        # Exact reuse of another session's retained nonce still refuses.
        reused = str(uuid.uuid4())
        saved["sessions"][next(iter(others))]["nonce_digests"] = [receipts.digest(reused.encode())]
        binding.atomic(self.ledger.path, saved)
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, reused, 8, 12))
        with self.assertRaisesRegex(ValueError, "nonce reused"):
            self.ledger.prepare(self.offer_record, body, 8, 12, reused)

    def test_rollover_starts_a_new_generation_keeping_watermarks_window_and_floor(self):
        self.intent()
        self.reconcile(ack=4)
        value = self.ledger.read()
        state = value["sessions"][self.session]
        state["suppressed_through"] = 6
        nonce = str(uuid.uuid4())
        sealed = self.filled(receipts.NONCE_FILTER_ROLLOVER_FILL, avoid=nonce)
        state.update(nonce_filter=sealed, nonce_digests=[receipts.digest(b"alpha-old")],
                     operator_audits=[{"id": "alpha-audit"}])
        binding.atomic(self.ledger.path, value)
        with self.assertRaisesRegex(ValueError, "replay"):
            self.ledger.prepare(self.offer_record, receipts.encode(receipts.marker(
                self.record["home"], "alpha", self.session, nonce, 4, 8)), 4, 8, nonce)
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, nonce, 6, 9))
        self.ledger.prepare(self.offer_record, body, 6, 9, nonce)
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(state["filter_generation"], 1)
        self.assertEqual(state["filter_floor"], 6)
        self.assertEqual((state["accepted_through"], state["acknowledged_through"], state["suppressed_through"]), (4, 4, 6))
        self.assertEqual(state["operator_audits"], [{"id": "alpha-audit"}])
        [archived] = state["filter_generations"]
        self.assertEqual(archived["generation"], 0)
        # The sealed generation includes the exact window folded in by compaction.
        folded = {"nonce_filter": sealed, "nonce_digests": [receipts.digest(b"alpha-old")]}
        receipts.remember_nonces(folded, [])
        self.assertEqual(archived["filter_sha256"], receipts.digest(folded["nonce_filter"].encode()))
        self.assertGreaterEqual(archived["fill_ratio"], receipts.NONCE_FILTER_ROLLOVER_FILL)
        self.assertEqual((archived["floor"], archived["accepted_through"], archived["suppressed_through"]), (6, 4, 6))
        # The exact window survives, plus the just-retired previous intent.
        self.assertIn(receipts.digest(b"alpha-old"), state["nonce_digests"])
        self.assertLess(receipts.fill_ratio(state), 0.001)
        # Same session, not a UUID switch; the floor binds even if a watermark
        # were ever lower, and exact reuse still refuses across generations.
        self.assertEqual(state["intent"]["nonce"], nonce)
        self.nonce = nonce
        self.reconcile(ack=9)
        value = self.ledger.read()
        value["sessions"][self.session]["suppressed_through"] = 0
        value["sessions"][self.session]["accepted_through"] = 0
        binding.atomic(self.ledger.path, value)
        again = str(uuid.uuid4())
        with self.assertRaisesRegex(ValueError, "replay"):
            self.ledger.prepare(self.offer_record, receipts.encode(receipts.marker(
                self.record["home"], "alpha", self.session, again, 5, 9)), 5, 9, again)

    def test_rollover_waits_for_quiescence_and_archive_room(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        value = self.ledger.read()
        state = value["sessions"][self.session]
        state["nonce_filter"] = self.filled(0.3)
        binding.atomic(self.ledger.path, value)
        nonce = str(uuid.uuid4())
        body = receipts.encode(receipts.marker(self.record["home"], "alpha", self.session, nonce, 4, 8))
        # An uncertain offer pins the session: no rollover, no new offer.
        with self.assertRaisesRegex(ValueError, "outstanding"):
            self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
        self.assertNotIn("filter_generation", self.ledger.read()["sessions"][self.session])
        # Pinned (uncovered) history also blocks rollover; saturation refuses.
        self.reconcile(ack=4)
        value = self.ledger.read()
        state = value["sessions"][self.session]
        state["history"] = [{**state["intent"], "nonce": str(uuid.uuid4()), "state": "presented",
                             "after_seq": 8, "through_seq": 12}]
        binding.atomic(self.ledger.path, value)
        with self.assertRaises(receipts.NonceFilterClosed) as closed:
            self.ledger.prepare(self.offer_record, body, 4, 8, nonce)
        self.assertEqual(closed.exception.diagnostic["kind"], "nonce_filter_saturated")
        self.assertNotIn("filter_generation", self.ledger.read()["sessions"][self.session])

    def test_never_spawned_rejections_do_not_consume_admission_filter(self):
        # Exceed both the exact window and the cumulative body cap. Reduce the
        # window only, leaving real filter size/capacity intact.
        with patch.object(receipts, "MAX_NONCE_DIGESTS", 8):
            for _ in range(33):
                self.intent(padding=300000)
                self.ledger.invoking(self.session, self.nonce)
                self.reconcile(failure=True)
            self.intent()
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(state["rejected_count"], 33)
        self.assertEqual(len(state["rejected_nonce_digests"]), 8)
        self.assertNotIn("nonce_filter", state)
        self.assertNotIn("nonce_digests", state)
        self.assertLess(self.ledger.path.stat().st_size, 20000)
        self.assertEqual(state["accepted_through"], 0)

    def test_old_abandon_audit_migration_preserves_range_hash_and_unresolved(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        self.reconcile()
        value = self.ledger.read()
        state = value["sessions"][self.session]
        old = {**state["intent"], "nonce": str(uuid.uuid4()), "state": "abandoned",
               "operator_resolution": {"actor": "operator", "reason": "alpha"}}
        state.update(history=[old], suppressed_through=4)
        binding.atomic(self.ledger.path, value)
        self.reconcile()
        state = self.ledger.read()["sessions"][self.session]
        self.assertEqual(state["intent"]["state"], "uncertain")
        self.assertEqual(state["history"], [])
        self.assertEqual(state["operator_audits"], [{**old["operator_resolution"],
            **{key: old[key] for key in ("after_seq", "through_seq", "body_hash")}}])

    def test_index_bound_and_cross_uuid_outstanding_offer_backpressure(self):
        self.intent()
        # A fork/relaunch may not bypass an old UUID's uncertain offer.
        record = copy.deepcopy(self.offer_record)
        record["session"] = record["binding"]["session"] = str(uuid.uuid4())
        nonce = str(uuid.uuid4())
        body = receipts.encode(receipts.marker(record["home"], "alpha", record["session"], nonce, 0, 4))
        with self.assertRaisesRegex(ValueError, "outstanding"):
            self.ledger.prepare(record, body, 0, 4, nonce)
        self.reconcile(ack=4)
        with patch.object(receipts, "MAX_SESSIONS", 1), self.assertRaisesRegex(ValueError, "index full"):
            self.ledger.prepare(record, body, 0, 4, nonce)

    def test_concurrent_prepare_has_one_owner_and_stale_nonce_cannot_settle(self):
        self.intent()
        self.reconcile(ack=4)
        record = self.offer_record
        def offer(_):
            nonce = str(uuid.uuid4())
            body = receipts.encode(receipts.marker(record["home"], "alpha", self.session, nonce, 4, 6))
            try:
                return self.ledger.prepare(record, body, 4, 6, nonce)["nonce"]
            except ValueError:
                return None
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(offer, range(2)))
        self.assertEqual(len([r for r in results if r]), 1)
        with self.assertRaisesRegex(ValueError, "nonce/body mismatch"):
            self.reconcile(ack=6)
        saved = self.ledger.read()["sessions"][self.session]
        self.assertEqual(saved["accepted_through"], 4)
        self.assertEqual(saved["intent"]["state"], "prepared")

    def test_unknown_queue_schema_and_duplicate_nonce_fail_closed(self):
        self.intent()
        self.ledger.invoking(self.session, self.nonce)
        for page in ({"data": []}, {"data": [], "nextCursor": None, "unknown": True}):
            result = self.reconcile(page)
            self.assertEqual(result["accepted_through"], 0)
        item = {"id": "alpha", "clientUserMessageId": "alpha-client", "input": [
            {"type": "text", "text": self.body, "text_elements": []}]}
        self.assertTrue(self.reconcile({"data": [item, item], "nextCursor": None})["paused"])


class QueueFixtureHttpTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("queue_fixture", ROOT / "scripts/codex-queue-spike.py")
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def fixture(self):
        fixture = self.module.Fixture.__new__(self.module.Fixture)
        fixture.calls = []
        fixture.allow_body = "fixture-only-notification"
        fixture.server = SimpleNamespace(server_port=12345)
        return fixture

    def handler(self, data=b'{"input":"fixture-only-notification"}'):
        statuses = []
        return SimpleNamespace(path="/v1/responses", client_address=("127.0.0.1", 1),
            headers={"Host": "127.0.0.1:12345", "Content-Length": str(len(data))}, command="POST",
            rfile=io.BytesIO(data), wfile=io.BytesIO(), send_response=statuses.append,
            send_header=lambda *args: None, end_headers=lambda: None, statuses=statuses)

    def test_invalid_requests_record_violation_and_return_500_without_thread_assertion(self):
        for kind in ("count", "peer", "host", "path", "method", "size", "oversize", "json", "body"):
            with self.subTest(kind=kind):
                fixture, handler = self.fixture(), self.handler()
                if kind == "count":
                    fixture.calls.append({"violation": "earlier request"})
                elif kind == "peer":
                    handler.client_address = ("192.0.2.1", 1)
                elif kind in ("path", "method"):
                    setattr(handler, "path" if kind == "path" else "command", "unexpected")
                elif kind == "host":
                    handler.headers["Host"] = "unexpected"
                elif kind in ("size", "oversize"):
                    handler.headers["Content-Length"] = "invalid" if kind == "size" else str((1 << 20) + 1)
                elif kind in ("json", "body"):
                    handler = self.handler(b"not-json" if kind == "json" else b"{}")
                fixture.mock_request(handler)
                self.assertEqual(handler.statuses, [500])
                self.assertIn("violation", fixture.calls[-1])
                with self.assertRaises(AssertionError):
                    fixture.check_calls()

    def test_one_permitted_loopback_request_returns_terminal_response(self):
        fixture, handler = self.fixture(), self.handler()
        fixture.mock_request(handler)
        self.assertEqual(handler.statuses, [200])
        self.assertIn(b"response.completed", handler.wfile.getvalue())
        fixture.check_calls()


# The dormant queue boundary shares this admitted suite, but no launcher imports it.
from codex_runner_test import RunnerTests
from codex_reserve_test import ReserveTests

if __name__ == "__main__":
    unittest.main()
