#!/usr/bin/env python3
"""Opt-in real Codex binding-only fixture; no login, credentials or real model.

Run with --binary <built-privateer> --codex <installed-codex>. All state is
throwaway; the sole outside-root exception is its own home-derived Unix socket.
This is NOT the live attended-mail qualification, and does not enqueue mail.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import fcntl

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
from codex_socket import Daemon, load, process_identity, socket_paths


class Fixture:
    def __init__(self, binary, codex):
        self.root = Path(tempfile.mkdtemp(prefix="alpha-cx-", dir="/var/tmp"))
        self.binary = str(Path(binary).resolve(strict=True))
        self.codex = str(Path(codex).resolve(strict=True))
        self.children = []
        self.closed = False
        self.daemon = None
        self.calls = []
        self.env = {"PATH": str(self.root / "bin") + os.pathsep + os.environ["PATH"],
                    "TERM": "xterm-256color", "PRIVATEER_SERVICE": "none"}
        for key, name in {"HOME": "home", "CODEX_HOME": "c", "PRIVATEER_HOME": "p",
                          "PRIVATEER_RESOURCE_HOME": "resources", "TMPDIR": "tmp",
                          "XDG_STATE_HOME": "state", "XDG_DATA_HOME": "data", "XDG_CONFIG_HOME": "config"}.items():
            (self.root / name).mkdir(mode=0o700)
            self.env[key] = str(self.root / name)
        self.env["PRIVATEER_FLEET"] = str(self.root / "fleet.sqlite")
        (self.root / "bin").mkdir()
        (self.root / "bin/codex").symlink_to(self.codex)
        self.home = Path(self.env["CODEX_HOME"])
        (self.home / "app-server-control").mkdir(mode=0o700)
        self.rendezvous, self.physical = socket_paths(self.home)
        assert not self.rendezvous.is_symlink() and not self.physical.exists() and not self.physical.is_symlink()
        assert not self.physical.with_suffix(".lock").exists()
        fixture = self
        class Mock(BaseHTTPRequestHandler):
            def do_POST(self):
                fixture.calls.append(self.path)
                self.send_response(500)
                self.end_headers()
            do_GET = do_POST
            def log_message(self, *args):
                pass
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def run(self, argv, cwd=None):
        result = subprocess.run(argv, cwd=cwd or self.root, env=self.env,
                                text=True, capture_output=True, timeout=30)
        if result.returncode:
            raise AssertionError(f"fixture subprocess failed: {argv[:2]}: {result.stderr[-2000:]}")
        return result.stdout

    def cli(self, command, args, home=None):
        return json.loads(self.run([self.binary, "--home", str(home or self.env["PRIVATEER_HOME"]), command, "--json", json.dumps(args)]))

    def setup(self):
        config = f'''model = "mock-model"
model_provider = "fixture"
check_for_update_on_startup = false
[model_providers.fixture]
name = "Local fixture only"
base_url = "http://127.0.0.1:{self.server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
[analytics]
enabled = false
[feedback]
enabled = false
'''
        self.stopper = self.root / "stop.py"
        self.stopper.write_text('import json\nprint(json.dumps({"continue":False,"stopReason":"Fixture: no provider call"}))\n')
        self.commands = {}
        for name in ["alpha", "beta", "gamma"]:
            folder = self.root / name
            self.cli("ship.create", {"name": name, "folder": str(folder), "harness": "codex"})
            self.run(["git", "init", "-q", str(folder)])
            self.cli("ship.attend", {"ship": name, "holder": f"host:{name}", "ttl_secs": 300, "harness": "codex"}, home=folder / ".privateer")
            project = folder / ".codex"
            project.mkdir(mode=0o700, exist_ok=True)
            registry = project / "privateer-attended/launch.json"
            command = shlex.join([sys.executable, "-B", str(ROOT / "plugins/attended-codex/codex_binding.py"), "--registry", str(registry)])
            self.commands[name] = command
            (project / "hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [
                {"hooks": [{"type": "command", "command": command, "timeout": 10}]},
                {"hooks": [{"type": "command", "command": shlex.join([sys.executable, str(self.stopper)])}]}]}}))
            config += f'\n[projects.{json.dumps(str(folder))}]\ntrust_level = "trusted"\n'
        self.config = config
        (self.home / "config.toml").write_text(config)
        settings = self.home / "app-server-daemon"
        settings.mkdir()
        (settings / "settings.json").write_text(json.dumps({"remoteControlEnabled": False,
            "shutdownGraceSeconds": 5, "updater": {"autoUpdateEnabled": False}}))
        self.daemon = json.loads(self.run(["codex", "app-server", "daemon", "start"]))
        assert self.daemon["status"] == "started"
        assert self.daemon["socketPath"] == str(self.rendezvous)
        with Daemon(self.home) as daemon:
            assert daemon.pid == self.daemon["pid"]
            for name in ["alpha", "beta", "gamma"]:
                hooks = daemon.call("hooks/list", {"cwds": [str(self.root / name)]})["data"][0]["hooks"]
                assert len(hooks) == 2
                for hook in hooks:
                    assert hook["source"] == "project" and hook["trustStatus"] == "untrusted"
                    # Gamma's binder is deliberately NOT trusted. Its stopper
                    # is trusted, so even the negative test cannot call a model.
                    if name == "gamma" and hook["command"] == self.commands[name]:
                        continue
                    config += f'\n[hooks.state.{json.dumps(hook["key"])}]\ntrusted_hash = {json.dumps(hook["currentHash"])}\n'
        (self.home / "config.toml").write_text(config)

    def pane(self, name, trusted=True):
        folder = self.root / name
        env = {**self.env, "PRIVATEER_HOME": str(folder / ".privateer"),
               "PRIVATEER_ROLE": "agent", "PRIVATEER_ACTOR": name,
               "PRIVATEER_PRESENCE_HOLDER": f"host:{name}", "PRIVATEER_HOSTED": "1",
               "PRIVATEER_CONTROL_CLI": self.binary, "PRIVATEER_CODEX_ATTENDED": "1"}
        argv = [sys.executable, "-B", str(ROOT / "plugins/native_launch.py"), "--attended-policy",
                '{"mode":"standard"}', "codex", "codex", "-C", str(folder),
                "--sandbox", "workspace-write", "--add-dir", env["PRIVATEER_HOME"], "--no-alt-screen"]
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
        try:
            child = subprocess.Popen(argv, cwd=folder, env=env, stdin=slave, stdout=slave,
                                     stderr=slave, start_new_session=True)
        finally:
            os.close(slave)
        self.children.append((child, master))
        log = bytearray()
        deadline = time.monotonic() + 30
        registry = folder / ".codex/privateer-attended/launch.json"
        # Untrusted hooks stop at native consent BEFORE the startup turn. Do
        # not type through that screen or accidentally authorize the binder.
        terminal_evidence = b"Hook stopped" if trusted else b"Hooks need review"
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                break
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            log.extend(data)
            assert len(log) <= 1 << 20
            for query, response in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"), (b"\x1b[>c", b"\x1b[>0;0;0c")]:
                if query in data:
                    os.write(master, response)
            if terminal_evidence in log:
                break
        assert b"Running without the shared background server" not in log
        assert terminal_evidence in log, f"startup hook/consent evidence missing: {bytes(log[-3000:])!r}"
        record = load(registry)
        assert record["delivery_enabled"] is False
        assert record["state"] == ("bound" if trusted else "unbound"), f"unexpected {name} binding: {record['state']}; PTY {bytes(log[-2000:])!r}"
        if trusted:
            assert record["binding"]["daemon_pid"] == self.daemon["pid"]
            assert record["hook_command"] == self.commands[name]
            assert record["pid"] == child.pid
            assert Path(f"/proc/{child.pid}/exe").resolve() == Path(self.codex)
            assert record["argv"][-2] == "--" and "first mate" in record["argv"][-1]
            # Verify a real hosted delivery_bind, not a mocked CLI receipt.
            import sqlite3
            with sqlite3.connect(folder / ".privateer/privateer.sqlite") as db:
                session = db.execute("SELECT session FROM hosted_delivery WHERE ship=?", (name,)).fetchone()
                assert session == (record["binding"]["session"],)
        return record

    def close(self):
        if self.closed:
            return
        for child, master in self.children:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            os.close(master)
        self.children.clear()
        try:
            if self.daemon is None and (self.home / "app-server-daemon/daemon.pid").exists():
                # A timed-out start wrapper is not proof its payload stopped.
                record = load(self.home / "app-server-daemon/daemon.pid")
                executable = Path(f"/proc/{record['pid']}/exe").resolve()
                assert executable.is_relative_to(self.home / "packages")
                assert process_identity(record["pid"]) == record["processIdentity"]
                self.daemon = {"pid": record["pid"]}
            if self.daemon is not None:
                try:
                    process_identity(self.daemon["pid"])
                except (FileNotFoundError, ValueError):
                    alive = False
                else:
                    alive = True
                if alive:
                    with Daemon(self.home) as daemon:
                        assert daemon.pid == self.daemon["pid"]
                    result = json.loads(self.run(["codex", "app-server", "daemon", "stop"]))
                    assert result["status"] == "stopped"
                    try:
                        process_identity(self.daemon["pid"])
                    except (FileNotFoundError, ValueError):
                        pass
                    else:
                        raise AssertionError("fixture daemon payload is still running")
                # Only the exact precomputed, initially absent owned paths.
                for path in [self.rendezvous, self.physical, self.physical.with_suffix(".lock")]:
                    if path.is_symlink() or path.exists():
                        assert path.lstat().st_uid == os.getuid()
                        path.unlink()
                try:
                    Daemon(self.home)
                except (OSError, ValueError):
                    pass
                else:
                    raise AssertionError("dead daemon unexpectedly connected")
        finally:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join()
        assert self.calls == [], f"unexpected local mock-provider requests: {self.calls}"
        shutil.rmtree(self.root)
        self.closed = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--codex", required=True)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("this qualification fixture currently requires Linux peer/process identity")
    fixture = Fixture(args.binary, args.codex)
    try:
        fixture.setup()
        gamma = fixture.pane("gamma", trusted=False)
        alpha = fixture.pane("alpha")
        beta = fixture.pane("beta")
        assert alpha["binding"]["session"] != beta["binding"]["session"]
        assert gamma["state"] == "unbound"
        print(json.dumps({"binding_only": True, "experimental": True,
                          "cli_version": fixture.daemon["cliVersion"], "daemon_version": fixture.daemon["appServerVersion"],
                          "distinct_project_hooks": True, "untrusted_unbound": True,
                          "real_delivery_bind": True, "model_calls": len(fixture.calls),
                          "mail_delivery_qualified": False}))
    finally:
        fixture.close()


if __name__ == "__main__":
    main()
