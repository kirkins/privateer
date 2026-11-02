#!/usr/bin/env python3
"""Hermetic binding tests. No native Codex, provider, auth or live Privateer state."""
import base64
import copy
import hashlib
import json
import multiprocessing
import os
from pathlib import Path
import socket
import struct
import sys
import tempfile
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
import codex_binding as binding
import codex_socket


def uuid7(ms):
    return str(uuid.UUID(int=(ms << 80) | (7 << 76) | (2 << 62) | 1))


class FakeDaemon:
    pid = 1
    identity = {"bootId": "alpha", "startTicks": 1}
    target = Path("/alpha/socket")
    rendezvous = Path("/alpha/rendezvous")

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
        self.argv = [sys.executable, "-C", str(self.cwd), "--sandbox", "workspace-write"]
        binding.prepare(self.argv, self.env)
        self.registry = self.cwd / ".codex/privateer-attended/launch.json"
        self.record = codex_socket.load(self.registry)
        self.event = {"session_id": uuid7(self.record["started_ms"] + 1), "cwd": str(self.cwd),
                      "hook_event_name": "SessionStart", "source": "startup"}
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

    def test_resume_is_exact_and_prompt_is_not_parsed_as_flags(self):
        session = str(uuid.uuid4())
        self.assertEqual(binding.plan(["codex", "resume", session, "--", "--no-daemon"], self.cwd), (session, True))
        for args in [["resume"], ["resume", "--last"], ["resume", "alpha"], ["resume", session, "a", "b"]]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                binding.plan(["codex", *args], self.cwd)

    def test_live_same_cwd_is_refused_and_stale_relaunch_keeps_command(self):
        with self.assertRaises(ValueError):
            binding.prepare(self.argv, self.env)
        self.record["process_identity"]["bootId"] = "previous-boot"
        binding.atomic(self.registry, self.record)
        binding.prepare(self.argv, self.env)
        fresh = codex_socket.load(self.registry)
        self.assertNotEqual(fresh["generation"], self.record["generation"])
        self.assertEqual(fresh["hook_command"], self.record["hook_command"])
        self.assertEqual(len(codex_socket.load(self.cwd / ".codex/hooks.json")["hooks"]["SessionStart"]), 1)

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
        script.write_text("#!/usr/bin/env python3\nprint('x' * (2 << 20))\n")
        with self.assertRaises(ValueError):
            binding.control(self.record, "ship.presence", {})


if __name__ == "__main__":
    unittest.main()
