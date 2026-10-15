"""Live dashboard PTY against real Privateer binaries and a private Herdr socket.

No real Herdr session, provider, user state directory or systemd is contacted.
"""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import unicodedata
import select
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


class Herdr:
    def __init__(self, root, env):
        self.root, self.env = root, env
        self.path = root / "herdr.sock"
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.path))
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.stopped = False
        self.panes, self.children, self.errors = [], [], []
        self.creates = self.focuses = 0
        self.delay_next = False
        self.fail_next = False
        self.expected_failures = set()
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopped:
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self.client, args=(conn,), daemon=True).start()

    def client(self, conn):
        try:
            with conn, conn.makefile("rwb", buffering=0) as stream:
                while line := stream.readline(1048576):
                    request = json.loads(line)
                    with self.lock:
                        result = self.respond(request)
                    stream.write(json.dumps({"id": request["id"], "result": result}).encode() + b"\n")
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            self.errors.append(repr(error))

    def respond(self, request):
        method, params = request["method"], request["params"]
        if method == "session.snapshot":
            return {"snapshot": {"version": "isolated-test", "protocol": 20, "panes": self.panes}}
        if method == "tab.create":
            self.creates += 1
            n = self.creates
            pane = dict(pane_id=f"w1:p{n}", terminal_id=f"terminal-{n}", workspace_id="w1",
                        tab_id=f"w1:t{n}", agent=None, agent_status="unknown", agent_session=None)
            self.panes.append(pane)
            self.cwd, self.extra_env = params["cwd"], params["env"]
            assert params["focus"] is True
            return {"type": "tab_created", "root_pane": dict(pane), "tab": {"tab_id": pane["tab_id"]}}
        if method == "pane.send_input":
            pane = next(p for p in self.panes if p["pane_id"] == params["pane_id"])
            assert pane["agent"] is None
            assert params["keys"] == ["Enter"]
            argv = shlex.split(params["text"])
            assert argv[0] == "exec" and argv[-1] == "start" and argv[2] == "--home", argv
            env = dict(self.env, **self.extra_env)
            env.update(HERDR_ENV="1", HERDR_SOCKET_PATH=str(self.path), HERDR_PANE_ID=pane["pane_id"],
                       HERDR_TAB_ID=pane["tab_id"], HERDR_WORKSPACE_ID=pane["workspace_id"])
            # The real runtime owns the fake harness and presence lifecycle.
            launch = argv[1:]
            if self.fail_next:
                self.fail_next = False
                # Fail before Privateer can claim presence. A harness that
                # briefly ran may legitimately clear the startup guard.
                launch = [sys.executable, "-c", "raise SystemExit(1)"]
            elif self.delay_next:
                self.delay_next = False
                launch = [sys.executable, "-c",
                          "import os,sys,time\nfrom pathlib import Path\n"
                          "while not Path(sys.argv[1]).exists(): time.sleep(.02)\n"
                          "os.execv(sys.argv[2],sys.argv[2:])\n",
                          str(self.root / "allow-start"), *launch]
            child = subprocess.Popen(launch, cwd=self.cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
            self.children.append(child)
            pane.update(agent="codex", agent_status="idle", agent_session={"kind": "id", "value": "isolated-session"})
            return {"type": "pane_input_sent"}
        if method == "agent.focus":
            assert any(p["pane_id"] == params["target"] and p["agent"] == "codex" for p in self.panes)
            self.focuses += 1
            return {"type": "agent_focused"}
        raise AssertionError(f"unexpected Herdr method: {method}")

    def close(self):
        (self.root / "release").touch()
        (self.root / "allow-start").touch()
        for child in self.children:
            try:
                _, error = child.communicate(timeout=5)
                assert child.returncode == (1 if child.pid in self.expected_failures else 0), error.decode()
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                raise AssertionError("isolated attended session did not exit")
        self.stopped = True
        self.listener.close()
        self.thread.join(timeout=1)


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
        self.reaped = False
        self.output = bytearray()
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 132, 0, 0))
        os.write(gate_write, b"1")
        os.close(gate_write)
        self.drain(0.4)

    def drain(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([self.fd], [], [], min(0.04, max(0, end-time.monotonic())))[0]:
                try:
                    data = os.read(self.fd, 32768)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                self.output.extend(data)
                assert len(self.output) < 2097152, "unbounded dashboard output"

    def keys(self, data):
        os.write(self.fd, data)
        self.drain(0.3)

    def until(self, predicate, message, timeout=10):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if predicate():
                return
            self.drain(0.1)
        raise AssertionError(message + "\n" + self.output[-4000:].decode(errors="replace"))

    def screen(self):
        # Interpret cursor placement in Ratatui's incremental ANSI output.
        # Substrings in raw bytes miss text reused from the preceding frame.
        rows, cols = 38, 132
        cells = [[" " for _ in range(cols)] for _ in range(rows)]
        row = col = 0
        def text(value):
            nonlocal row, col
            for char in value:
                if char == "\r": col = 0
                elif char == "\n": row = min(rows-1, row+1)
                elif char >= " " and not unicodedata.combining(char):
                    if 0 <= row < rows and 0 <= col < cols: cells[row][col] = char
                    col += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
        data = self.output.decode(errors="replace")
        start = 0
        for match in re.finditer(r"\x1b\[([0-9;?]*)([@-~])", data):
            text(data[start:match.start()])
            params, code = match.groups()
            numbers = [int(n) if n else 0 for n in params.lstrip("?").split(";")]
            n = numbers[0] or 1
            if code in ("H", "f"):
                row, col = n-1, (numbers[1] or 1)-1 if len(numbers)>1 else 0
            elif code == "A": row = max(0,row-n)
            elif code == "B": row = min(rows-1,row+n)
            elif code == "C": col = min(cols-1,col+n)
            elif code == "D": col = max(0,col-n)
            elif code == "G": col = n-1
            elif code == "J" and numbers[0] in (2,3): cells = [[" " for _ in range(cols)] for _ in range(rows)]
            elif code == "K" and 0 <= row < rows:
                for x in range(max(0,col),cols): cells[row][x] = " "
            start = match.end()
        text(data[start:])
        return "\n".join("".join(line) for line in cells)

    def close(self):
        try:
            self.keys(b"q")
            end = time.monotonic() + 3
            while time.monotonic() < end:
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.reaped = True
                    assert os.waitstatus_to_exitcode(status) == 0
                    break
                self.drain(0.05)
            assert self.reaped, "live dashboard did not quit (a form may still be open)"
        finally:
            if not self.reaped:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            os.close(self.fd)


def run(binary, runtime):
    with tempfile.TemporaryDirectory(prefix="privateer-dash-", dir="/tmp") as temporary:
        root = Path(temporary)
        folder = root / "apps ' $fleet"
        folder.mkdir()
        bins = root / "bin"
        bins.mkdir()
        harness = bins / "codex"
        harness.write_text("#!/usr/bin/env python3\nimport os,time\nfrom pathlib import Path\n"
                           "root=Path(os.environ['HOME'])\npane=os.environ.get('HERDR_PANE_ID','headless')\n"
                           "(root/'harness-started').touch()\n(root/f'harness-started-{pane}').touch()\n"
                           "while not (root/'release').exists() and not (root/f'release-{pane}').exists(): time.sleep(.05)\n")
        harness.chmod(0o755)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("PRIVATEER_") and k not in {"HERDR_ENV", "HERDR_SOCKET_PATH"}}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"), PRIVATEER_FLEET=str(root / "fleet.sqlite"), PRIVATEER_SERVICE="none",
                   PRIVATEER_RESOURCE_HOME=str(root / "ledger"), TERM="xterm-256color",
                   PATH=os.pathsep.join([str(bins), str(Path(sys.executable).parent), os.environ.get("PATH", "/usr/bin:/bin")]))

        def cli(*args, cwd=folder):
            result = subprocess.run([str(runtime), *args], cwd=cwd, env=env, capture_output=True, timeout=10)
            assert result.returncode == 0, result.stderr.decode()
            return result.stdout

        def command(verb, args):
            return json.loads(cli(verb, "--json", json.dumps(args)))

        def active():
            return {s["name"] for s in command("fleet.list", {})["ships"] if s["retired_ms"] is None}

        cli("init", "--ship", "apps", "--harness", "codex")
        command("mail.send", {"id": "question", "from": "apps", "to": "captain", "kind": "request", "body": "Please answer"})
        server = Herdr(root, env)
        tui = None
        try:
            tui = Tui(binary, env, ["--herdr-socket", str(server.path)])
            tui.until(lambda: b"question" in tui.output or b"Please" in tui.output, "captain mail did not load")
            tui.keys(b"irGo ahead\r")
            tui.until(lambda: command("mail.show", {"id": "question"})["state"] == "answered", "reply did not close the real request")
            tui.until(lambda: "mail.send:" in tui.screen(), "reply completion did not reach the UI")
            tui.keys(b"0jj\rb")
            tui.until(lambda: (root / "harness-started").exists(), "Board did not run privateer start")
            tui.until(lambda: command("ship.presence", {"ship": "apps"})["attended"], "Board did not hold presence")
            lease = command("ship.presence", {"ship": "apps"})["lease"]
            assert lease["binding"]["pane"]["pane_id"] == "w1:p1", lease
            tui.drain(2.5)
            tui.keys(b"b")
            tui.until(lambda: server.focuses == 1, "Board again did not focus the bound pane")
            assert server.creates == 1
            presentation = folder / ".privateer" / "dashboard.json"
            terminal = {key: lease["binding"]["pane"][key] for key in ("pane_id", "terminal_id")}
            own = {"id": "apps", "name": "Apps", "liaison": True}
            foreign = {"id": "crew", "name": "Crew", "terminals": [terminal]}
            valid = dict(own, terminals=[terminal])
            for configuration in [json.dumps({"ships": [own, foreign]}),
                                  json.dumps({"ships": [valid, foreign]}), "invalid JSON"]:
                presentation.write_text(configuration)
                tui.until(lambda: "PRESENTATION UNAVAILABLE" in tui.screen(), "presentation conflict was not surfaced")
                tui.keys(b"bflf")
                assert (server.creates, server.focuses) == (1, 1), "invalid claims allowed Board/focus navigation"
                presentation.write_text(json.dumps({"ships": [valid]}))
                tui.until(lambda: "PRESENTATION UNAVAILABLE" not in tui.screen(), "presentation did not recover")
            original_session = lease["binding"]["pane"]["agent_session"]
            assert original_session is not None, "fake native session was not pinned"
            with server.lock:
                server.panes[0]["agent_session"] = {"kind": "id", "value": "replacement"}
            tui.keys(b"3")
            tui.until(lambda: "No connected terminals bound" in tui.screen(), "manual binding bypassed the pinned native session")
            tui.keys(b"bflf")
            tui.drain(2.2)
            assert (server.creates, server.focuses) == (1, 1), "replaced native session allowed Board/focus navigation"
            with server.lock:
                server.panes[0]["agent_session"] = original_session
            tui.keys(b"3")
            tui.until(lambda: "No connected terminals bound" not in tui.screen(), "native binding did not recover")
            tui.keys(b"b")
            tui.until(lambda: server.focuses == 2, "Board did not focus after identity recovery")
            tui.keys(b"1")
            tui.keys(b"d\r")
            tui.until(lambda: "service:" in tui.screen(), "dispatcher completion did not reach the UI")
            tui.keys(b"nnewworker\tScope of worker\r")
            tui.until(lambda: "newworker" in active(), "New ship did not call ship.create")
            tui.until(lambda: "ship.create:" in tui.screen(), "creation completion did not reach the UI")
            tui.drain(1.2)
            tui.keys(b"0jjj\rxnewworker\r")
            tui.until(lambda: "newworker" not in active(), "Retire did not call ship.retire")
            assert (root / "newworker" / ".privateer").is_dir(), "retirement deleted the folder"
            tui.until(lambda: "ship.retire:" in tui.screen(), "retirement completion did not reach the UI")
            presentation.write_text(json.dumps({"ships": [own]}))
            tui.drain(1.2)

            def board_until(count, message):
                def requested():
                    if server.creates < count:
                        tui.keys(b"0jj\rb")
                    # tab.create precedes pane.send_input; wait for the child
                    # before inspecting it or checking startup behavior.
                    return len(server.children) >= count
                tui.until(requested, message, timeout=25)
                assert server.creates == len(server.children) == count

            def finish(pane_number):
                (root / f"release-w1:p{pane_number}").touch()
                tui.until(lambda: not command("ship.presence", {"ship": "apps"})["attended"],
                          "completed harness retained its presence")
                assert server.children[pane_number-1].wait(timeout=5) == 0
                assert any(p["pane_id"] == f"w1:p{pane_number}" for p in server.panes)

            def bound_to(pane_number):
                presence = command("ship.presence", {"ship": "apps"})
                return presence["attended"] and presence["lease"]["binding"]["pane"]["pane_id"] == f"w1:p{pane_number}"

            def pending_guard(message):
                def blocked():
                    # A stale fleet observation can still route the first key
                    # to the completed pane. Retry while the fresh observation
                    # arrives, within the 15-second startup guard window.
                    tui.keys(b"0jj\rb")
                    # Let the worker result render before another key replaces
                    # its notice with "Checking ship and terminal".
                    tui.drain(1.0)
                    return "launch already submitted" in tui.screen()
                tui.until(blocked, message, timeout=8)

            finish(1)
            board_until(2, "Board could not restart after a completed harness left its pane")
            tui.until(lambda: bound_to(2), "restart did not claim a fresh bound lease")
            tui.until(lambda: (root / "harness-started-w1:p2").exists(), "restart did not run a fresh harness")
            tui.drain(2.5)
            finish(2)
            server.fail_next = True
            board_until(3, "Board could not submit the failed-start scenario")
            failed = server.children[2]
            server.expected_failures.add(failed.pid)
            assert failed.wait(timeout=5) == 1
            assert not command("ship.presence", {"ship": "apps"})["attended"]
            pending_guard("failed startup lost its uncertainty guard")
            assert server.creates == 3
            server.delay_next = True
            board_until(4, "failed startup permanently blocked Board")
            assert not command("ship.presence", {"ship": "apps"})["attended"]
            pending_guard("delayed startup did not retain its pending guard")
            assert server.creates == 4
            assert not (root / "harness-started-w1:p4").exists()
            (root / "allow-start").touch()
            tui.until(lambda: bound_to(4), "delayed startup did not claim presence when released")
            tui.until(lambda: (root / "harness-started-w1:p4").exists(), "delayed startup did not run its harness")
            competitor = subprocess.run([str(runtime), "start"], cwd=folder, env=env,
                                        capture_output=True, timeout=5)
            assert competitor.returncode != 0 and b"already aboard" in competitor.stderr
            tui.keys(b"0jj\rbbb")
            tui.drain(2.2)
            assert server.creates == 4
            assert sorted(p.name for p in root.glob("harness-started-*")) == [
                "harness-started-w1:p1", "harness-started-w1:p2", "harness-started-w1:p4"]
            assert not server.errors, server.errors
        finally:
            if tui:
                tui.close()
            server.close()
        assert not command("ship.presence", {"ship": "apps"})["attended"]
        print("Live dashboard PTY passed: captain reply, Board/start/presence/binding, retained-pane restart, failed/delayed startup guards, conflict and session rejection/recovery, focus, dispatcher status, new/retire; isolated fake Herdr only.")
