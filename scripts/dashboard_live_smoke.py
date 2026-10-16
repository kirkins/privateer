"""Actual bridge input -> local runtime outcomes, using a private fake Herdr.

No live Herdr, inference, credentials, providers or services. No screen snapshots.
Core identity/custody/read-only/race coverage remains in the Rust safety tests.
"""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time


def fixture_environment(root):
    """Allowlist for disposable AUTHLESS fixture children only, never the supervisor.

    No inherited role, actor, credentials, endpoints, user config or executable path.
    Absence of an agent role is appropriate ONLY for these new local test stores.
    """
    root = root.resolve()
    paths = {"HOME": root / "user", "XDG_CONFIG_HOME": root / "config",
             "XDG_STATE_HOME": root / "state", "XDG_DATA_HOME": root / "data",
             "XDG_CACHE_HOME": root / "cache", "XDG_RUNTIME_DIR": root / "run",
             "TMPDIR": root / "tmp", "PRIVATEER_RESOURCE_HOME": root / "ledger"}
    for path in paths.values():
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
    return {**{key: str(path) for key, path in paths.items()},
            "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "xterm-256color",
            "PRIVATEER_FLEET": str(root / "fleet.sqlite"), "PRIVATEER_SERVICE": "none"}


class Herdr:
    def __init__(self, root):
        self.path = root / "herdr.sock"
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.path))
        self.listener.listen()
        self.listener.settimeout(.1)
        self.focuses = 0
        self.errors = []
        self.stopped = False
        self.pane = dict(pane_id="w1:p1", terminal_id="terminal-1", workspace_id="w1",
                         tab_id="w1:t1", agent="codex", agent_status="idle",
                         agent_session={"kind": "id", "value": "test-session"})
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stopped:
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                with conn, conn.makefile("rwb", buffering=0) as stream:
                    request = json.loads(stream.readline(1048576))
                    if request["method"] == "session.snapshot":
                        result = {"snapshot": {"version": "isolated", "protocol": 20,
                                               "panes": [self.pane]}}
                    elif request["method"] == "agent.focus":
                        assert request["params"]["target"] == self.pane["pane_id"]
                        self.focuses += 1
                        result = {"type": "agent_focused"}
                    else:
                        raise AssertionError("bridge must not create/reorder/send input: " + str(request))
                    stream.write(json.dumps({"id": request["id"], "result": result}).encode() + b"\n")
            except Exception as error:
                self.errors.append(repr(error))

    def close(self):
        self.stopped = True
        self.listener.close()
        self.thread.join(timeout=2)
        assert not self.thread.is_alive(), "fake Herdr thread did not stop"
        assert not self.errors, self.errors


class Tui:
    def __init__(self, binary, env, args):
        gate_read, gate_write = os.pipe()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.close(gate_write)
            os.read(gate_read, 1)
            os.close(gate_read)
            os.chdir(env["HOME"])
            os.execve(str(binary), [str(binary), *args], env)
        os.close(gate_read)
        self.before = termios.tcgetattr(self.fd)
        self.reaped = False
        self.output = bytearray()
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 132, 0, 0))
        os.write(gate_write, b"1")
        os.close(gate_write)
        self.drain(.5)

    def drain(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([self.fd], [], [], min(.04, max(0, end-time.monotonic())))[0]:
                try:
                    data = os.read(self.fd, 32768)
                except OSError as error:
                    if error.errno == errno.EIO:
                        return
                    raise
                if not data:
                    return
                self.output.extend(data)
                assert len(self.output) < 2097152, "unbounded dashboard output"

    def keys(self, data):
        os.write(self.fd, data)
        self.drain(.4)

    def until(self, predicate, message, timeout=12):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if predicate():
                return
            self.drain(.1)
        raise AssertionError(message + "\n" + self.output[-4000:].decode(errors="replace"))

    def close(self):
        try:
            self.keys(b"\x1b")  # cancel any retained draft / leave history
            self.keys(b"q")
            end = time.monotonic() + 3
            while time.monotonic() < end:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.reaped = True
                    assert os.waitstatus_to_exitcode(status) == 0
                    break
                self.drain(.05)
            assert self.reaped, "bridge did not quit"
            after = termios.tcgetattr(self.fd)
            for flag in [termios.ECHO, termios.ICANON, termios.ISIG]:
                assert (self.before[3] & flag) == (after[3] & flag), "terminal mode not restored"
            assert b"\x1b[?1049l" in self.output, "alternate screen not restored"
            assert b"\x1b[?1006l" in self.output, "mouse capture not restored"
            print(f"Live fixture PID {self.pid}: exited 0, reaped, terminal/screen/mouse restored before cleanup")
        finally:
            if not self.reaped:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            os.close(self.fd)


def run(binary, runtime):
    with tempfile.TemporaryDirectory(prefix="privateer-bridge-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        folder = root / "apps"
        folder.mkdir()
        env = fixture_environment(root)
        # Dashboard discovery is confined to the fixture's registered fleet.
        binary, runtime = binary.resolve(strict=True), runtime.resolve(strict=True)

        def cli(*args):
            output = subprocess.run([str(runtime), *args], cwd=folder, env=env,
                                    capture_output=True, timeout=15)
            assert output.returncode == 0, output.stderr.decode()
            return output.stdout

        def command(verb, args):
            return json.loads(cli(verb, "--json", json.dumps(args)))

        cli("init", "--ship", "apps", "--harness", "codex")
        server = Herdr(root)
        presentation = folder / ".privateer" / "dashboard.json"
        presentation.write_text(json.dumps({"ships": [{"id": "apps", "name": "apps",
            "terminals": [{"pane_id": "w1:p1", "terminal_id": "terminal-1"}]}]}))
        command("mail.send", {"id": "question", "from": "apps", "to": "captain",
                "kind": "request", "body": "Please answer"})
        tui = None
        try:
            tui = Tui(binary, env, ["--herdr-socket", str(server.path)])
            tui.until(lambda: b"Please answer" in tui.output, "typed need did not load")
            tui.keys(b"j\r")  # select Fleet ship and focus ONLY its existing pane
            tui.until(lambda: server.focuses == 1, "Enter did not focus existing mate")
            tui.keys(b"\t\rGo ahead\r")
            tui.until(lambda: command("mail.show", {"id": "question"})["state"] == "answered",
                      "inline text reply did not close request")
            original = command("mail.show", {"id": "question"})
            reply = command("mail.show", {"id": original["answered_by"]})
            assert reply["from"] == "captain" and reply["correlation"] == "question"
            assert reply["body"] == "Go ahead"
            command("mail.send", {"id": "approval", "from": "apps", "to": "captain",
                "kind": "request", "request_kind": "production_approval", "task_id": "apps",
                "body": "Exact release?", "payload": {"artifact": "sha256:fixture", "environment": "offline"}})
            tui.keys(b"\x1b")
            tui.until(lambda: b"Exact release?" in tui.output, "typed approval did not load")
            approval_start = len(tui.output)
            tui.keys(b"\t\rd\r")
            tui.until(lambda: command("mail.show", {"id": "approval"})["state"] == "denied",
                      "inline denial did not record exact outcome")
            tui.until(lambda: b"Recorded answer" in tui.output[approval_start:],
                      "UI did not consume the denial outcome before opening history")
            before_history = command("mail.inbox", {"address": "captain"})["acknowledged_through"]
            tui.keys(b"i")
            tui.drain(2.5)
            assert b"Mail history" in tui.output, "history did not open"
            assert command("mail.inbox", {"address": "captain"})["acknowledged_through"] == before_history
            assert command("mail.show", {"id": "question"})["state"] == "answered"
            assert server.focuses == 1, "refresh/history stole focus"
        finally:
            try:
                if tui:
                    tui.close()
            finally:
                server.close()
        print("Bridge live PTY passed: existing focus, correlated inline reply, exact denial, separate history, quit/restoration; no launch/live service.")
