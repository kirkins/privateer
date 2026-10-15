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
        self.focused = []
        self.delay_next = False
        self.fail_next = False
        self.first_snapshot_delay = 0
        self.protocol = 20
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
            time.sleep(self.first_snapshot_delay)
            self.first_snapshot_delay = 0
            return {"snapshot": {"version": "isolated-test", "protocol": self.protocol, "panes": self.panes}}
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
            self.focused.append(params["target"])
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
    def __init__(self, binary, env, args, columns=132, cwd=None):
        self.columns = columns
        gate_read, gate_write = os.pipe()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.close(gate_write)
            os.read(gate_read, 1)
            os.close(gate_read)
            os.chdir(cwd or env["HOME"])
            os.execve(str(binary), [str(binary), *args], env)
        os.close(gate_read)
        self.reaped = False
        self.output = bytearray()
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 38, columns, 0, 0))
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
        rows, cols = 38, self.columns
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
        root = Path(temporary).resolve()
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
               if not k.startswith("PRIVATEER_") and not k.startswith("HERDR_")}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_STATE_HOME=str(root / "state"), XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   PRIVATEER_FLEET=str(root / "fleet.sqlite"), PRIVATEER_SERVICE="none",
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
            tui.until(lambda: "Reply sent to apps." in tui.screen(), "reply completion did not reach the UI")
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
            tui.until(lambda: "Dispatcher for apps" in tui.screen(), "dispatcher completion did not reach the UI")
            tui.keys(b"nnewworker\tScope of worker\r")
            tui.until(lambda: "newworker" in active(), "New ship did not call ship.create")
            tui.until(lambda: "Created newworker in" in tui.screen(), "creation completion did not reach the UI")
            tui.drain(1.2)
            tui.keys(b"0jjj\rxnewworker\r")
            tui.until(lambda: "newworker" not in active(), "Retire did not call ship.retire")
            assert (root / "newworker" / ".privateer").is_dir(), "retirement deleted the folder"
            tui.until(lambda: "Retired newworker; its folder is kept." in tui.screen(), "retirement completion did not reach the UI")
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


def onboarding(binary, runtime):
    with tempfile.TemporaryDirectory(prefix="privateer-onboard-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        bins = root / "bin"
        bins.mkdir()
        harness = bins / "codex"
        harness.write_text("#!/usr/bin/env python3\nimport os,time\nfrom pathlib import Path\n"
                           "Path(os.environ['HOME'],'harness-started').touch()\n"
                           "while not Path(os.environ['HOME'],'release').exists(): time.sleep(.05)\n")
        harness.chmod(0o755)
        (bins / "privateer").symlink_to(runtime)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith("PRIVATEER_") and not k.startswith("HERDR_")}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_STATE_HOME=str(root / "state"), XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   PRIVATEER_FLEET=str(root / "fleet.sqlite"), PRIVATEER_SERVICE="none",
                   PRIVATEER_RESOURCE_HOME=str(root / "ledger"), TERM="xterm-256color", PATH=str(bins)+":/usr/bin:/bin")
        # Real bare-runtime entry works with every HERDR_* variable absent,
        # as well as inside Herdr, before initializing the current folder.
        for inside_herdr in [False, True]:
            entry_root = root / ("herdr-entry" if inside_herdr else "ordinary-entry")
            fresh = entry_root / "fresh"
            user = entry_root / "user"
            fresh.mkdir(parents=True)
            user.mkdir()
            entry_env = dict(env, HOME=str(user), XDG_CONFIG_HOME=str(entry_root / "config"),
                             PRIVATEER_FLEET=str(entry_root / "fleet.sqlite"),
                             PRIVATEER_RESOURCE_HOME=str(entry_root / "ledger"))
            assert not any(key.startswith("HERDR_") for key in entry_env)
            if inside_herdr:
                entry_env["HERDR_ENV"] = "1"
            skipped = Tui(runtime, entry_env, [], cwd=fresh)
            try:
                skipped.until(lambda: "Welcome" in skipped.screen(), "bare privateer did not open real welcome")
                assert not list(fresh.iterdir()), "welcome initialized the current folder"
                assert not (user / ".privateer").exists()
                assert not (entry_root / "fleet.sqlite").exists()
                assert not (user / "ships").exists()
                assert not (user / "harness-started").exists()
                skipped.keys(b"\x1b")
            finally:
                skipped.close()
            assert not list(fresh.iterdir()), "skip initialized the current folder"
            assert not (user / ".privateer").exists()
            assert not (entry_root / "fleet.sqlite").exists()
            assert not (user / "ships").exists()
            assert not (user / "harness-started").exists()
        server = Herdr(root, env)
        server.protocol = 19
        tui = None
        folder = root / "custom contact ' folder"
        home = folder / ".privateer"
        try:
            tui = Tui(binary, env, ["--herdr-socket", str(server.path)])
            assert "liaison" in tui.screen() and "codex" in tui.screen()
            tui.until(lambda: "HERDR UNAVAILABLE" in tui.screen(), "initial Herdr observation did not fail")
            tui.keys(b"\x15contact\t\x15" + str(folder).encode() + b"\t\x1b[C\r")
            tui.until(lambda: (home / "privateer.sqlite").is_file(), "onboarding did not initialize the chosen folder")
            tui.until(lambda: "Bridge" in tui.screen(), "created first ship did not become selected")
            tui.drain(2.2)
            assert server.creates == 0 and not (root / "harness-started").exists()
            server.protocol = 20
            tui.until(lambda: (root / "harness-started").exists(), "onboarding did not automatically Board after Herdr recovery", timeout=25)
            def query(verb, args):
                out = subprocess.run([str(runtime), "--home", str(home), verb, "--json", json.dumps(args)],
                                     env=env, capture_output=True, timeout=10)
                assert out.returncode == 0, out.stderr.decode()
                return json.loads(out.stdout)
            tui.until(lambda: query("ship.presence", {"ship":"contact"})["attended"], "onboarded ship has no presence")
            assert query("charter.show", {"id":"contact"})["actions"] == ["inspect", "spawn", "implement", "test", "publish_pr", "merge_green", "message", "recovery", "cleanup"]
            assert json.loads((home / "config.json").read_text())["preset"] == "liaison"
            assert "Onboarding the captain" in (folder / "AGENTS.md").read_text()
            tui.until(lambda: "Bridge" in tui.screen(), "created first ship did not become selected")
            tui.drain(2.5)
            assert server.creates == 1 and server.focuses == 0 and not server.errors, server.errors
            # Everyday entry must reach the native conversation, leaving this
            # dashboard alive as the observability tab. Reopening must reuse
            # the exact presence binding rather than start another first mate.
            tui.close()
            tui = None
            tui = Tui(binary, env, ["--herdr-socket", str(server.path)])
            tui.drain(2.5)
            assert server.creates == 1 and server.focuses == 0, "direct dashboard entry launched or focused an agent"
            tui.close()
            tui = None
            once = subprocess.run([str(binary), "--once", "--board-liaison", "--herdr-socket", str(server.path)],
                                  env=env, capture_output=True, check=True, timeout=10)
            assert json.loads(once.stdout)["read_only"] is True
            assert server.creates == 1 and server.focuses == 0, "--once performed a startup action"
            tui = Tui(runtime, env, [], cwd=root)
            tui.until(lambda: "to talk directly, run:" in tui.screen(), "ordinary entry did not explain how to reach the liaison")
            tui.keys(b"l")
            assert server.creates == 1 and server.focuses == 0, "ordinary entry discovered an unconfigured socket"
            tui.close()
            tui = None
            entry_env = dict(env, HERDR_ENV="1", HERDR_SOCKET_PATH=str(server.path))
            tui = Tui(runtime, entry_env, [], cwd=root)
            tui.until(lambda: server.focuses == 1, "bare privateer did not automatically focus the liaison")
            tui.drain(2.5)
            assert server.creates == 1 and server.focuses == 1
            tui.keys(b"0l")
            tui.until(lambda: server.focuses == 2, "Talk to liaison needed another Board action")
            screen = tui.screen().splitlines()
            row, line = next((row, line) for row, line in enumerate(screen) if "[l Talk to liaison]" in line)
            column = line.index("[l Talk to liaison]")
            tui.keys(f"\x1b[<0;{column+1};{row+1}M\x1b[<0;{column+1};{row+1}m".encode())
            tui.until(lambda: server.focuses == 3, "Talk to liaison click did not focus the conversation")
            tui.close()
            tui = None
            # End the attended session while retaining its old Herdr pane.
            # Next entry must create a fresh conversation automatically.
            (root / "release").touch()
            for child in server.children:
                _, error = child.communicate(timeout=5)
                assert child.returncode == 0, error.decode()
            (root / "release").unlink()
            (root / "harness-started").unlink()
            tui = Tui(runtime, entry_env, [], cwd=root)
            tui.until(lambda: (root / "harness-started").exists(), "bare privateer did not open an offline liaison")
            tui.until(lambda: query("ship.presence", {"ship":"contact"})["attended"], "automatic liaison launch did not claim presence")
            assert query("ship.presence", {"ship":"contact"})["lease"]["binding"]["pane"]["pane_id"] == "w1:p2"
            tui.drain(2.5)
            assert server.creates == 2 and server.focuses == 3 and not server.errors, server.errors
        finally:
            if tui:
                tui.close()
            server.close()
        for change in ["ambiguous", "removed", "contact", "home", "navigation"]:
            for attended in [False, True]:
                startup_root = root / f"startup-{change}-{attended}"
                startup_root.mkdir()
                startup_env = dict(env, HOME=str(startup_root), XDG_CONFIG_HOME=str(startup_root / "config"),
                                   PRIVATEER_FLEET=str(startup_root / "fleet.sqlite"),
                                   PRIVATEER_RESOURCE_HOME=str(startup_root / "ledger"))
                def startup_command(name, *arguments):
                    return subprocess.run([str(runtime), "--home", str(startup_root / name / ".privateer"), *arguments],
                                          cwd=startup_root, env=startup_env, capture_output=True, check=True, timeout=10).stdout
                startup_command("contact", "init", "--ship", "contact", "--harness", "codex", "--liaison")
                if change == "navigation":
                    startup_command("other", "init", "--ship", "other", "--harness", "codex")
                startup_server = Herdr(startup_root, startup_env)
                startup_entry = dict(startup_env, HERDR_ENV="1", HERDR_SOCKET_PATH=str(startup_server.path))
                startup = None
                try:
                    if attended:
                        startup = Tui(runtime, startup_entry, [])
                        startup.until(lambda: json.loads(startup_command("contact", "ship.presence", "--json", '{"ship":"contact"}'))["attended"],
                                      "startup fixture did not establish attended presence")
                        startup.close()
                        startup = None
                    counts = (startup_server.creates, startup_server.focuses)
                    startup_server.protocol = 19
                    startup = Tui(runtime, startup_entry, [])
                    startup.until(lambda: "HERDR UNAVAILABLE" in startup.screen(), "startup did not observe the Herdr outage")
                    assert "No Herdr connection configured" not in startup.screen(), "configured outage was reported as missing context"
                    assert "Liaison · your default contact" in startup.screen()
                    if change == "navigation":
                        for tab in ["2 Assignments", "3 Terminals", "4 Logbook"]:
                            startup.keys(tab[0].encode())
                            startup.drain(1.2)
                            for direction in [b"\x1b[B", b"\x1b[A"]:
                                startup.keys(direction)
                                assert f"contact / {tab}" in startup.screen().splitlines()[1], "pending startup stole workspace focus"
                                assert "automatic Board cancelled" not in startup.screen(), "workspace navigation cancelled startup Board"
                        startup.keys(b"0")
                    elif change in ["ambiguous", "contact"]:
                        arguments = ["init", "--ship", "other", "--harness", "codex"]
                        if change == "ambiguous":
                            arguments.append("--liaison")
                        startup_command("other", *arguments)
                        if change == "contact":
                            (startup_root / "contact/.privateer/dashboard.json").write_text(
                                json.dumps({"ships":[{"id":"other", "name":"other", "liaison":True}]}))
                    else:
                        startup_command("contact", "ship", "retire", "contact")
                        if change == "home":
                            startup_command("replacement", "init", "--ship", "contact", "--harness", "codex", "--liaison")
                    startup.until(lambda: "automatic Board cancelled" in startup.screen(),
                                  f"{change} liaison did not cancel startup Board")
                    if change == "navigation":
                        assert " / 1 Bridge" not in startup.screen().splitlines()[1], "startup stole fleet navigation"
                        startup.keys(b"i")
                        startup.until(lambda: "Captain's inbox · message details" in startup.screen(), "startup prevented inbox navigation")
                    startup_server.protocol = 20
                    startup.until(lambda: "HERDR UNAVAILABLE" not in startup.screen(), "startup did not observe Herdr recovery", timeout=25)
                    startup.drain(2.5)
                    assert (startup_server.creates, startup_server.focuses) == counts, (change, attended)
                    if change == "navigation":
                        assert "Captain's inbox · message details" in startup.screen(), "Herdr recovery stole inbox navigation"
                        startup.keys(b"l")
                        expected = (counts[0], counts[1] + 1) if attended else (1, 0)
                        startup.until(lambda: (startup_server.creates, startup_server.focuses) == expected,
                                      "explicit Talk did not work after cancelling automatic startup")
                        startup.until(lambda: (startup_root / "harness-started").exists(), "explicit Talk did not reach the harness")
                    if change == "ambiguous":
                        startup_command("other", "ship", "retire", "other")
                        startup.until(lambda: "Liaison · your default contact" in startup.screen(), "unique contact did not recover")
                        startup.drain(2.5)
                        assert (startup_server.creates, startup_server.focuses) == counts, "cancelled startup intent resumed"
                    assert (startup_root / "harness-started").exists() == (attended or change == "navigation")
                    assert not startup_server.errors, startup_server.errors
                finally:
                    if startup:
                        startup.close()
                    startup_server.close()
        # Input during startup (a scroll over Navigation, a key) waits for the
        # first Herdr observation instead of cancelling the liaison handoff.
        early_root = root / "startup-early-input"
        early_root.mkdir()
        early_env = dict(env, HOME=str(early_root), XDG_CONFIG_HOME=str(early_root / "config"),
                         PRIVATEER_FLEET=str(early_root / "fleet.sqlite"),
                         PRIVATEER_RESOURCE_HOME=str(early_root / "ledger"))
        subprocess.run([str(runtime), "--home", str(early_root / "contact/.privateer"), "init", "--ship", "contact",
                        "--harness", "codex", "--liaison"], cwd=early_root, env=early_env, capture_output=True,
                       check=True, timeout=10)
        early_server = Herdr(early_root, early_env)
        early_server.first_snapshot_delay = 0.6
        early = None
        try:
            early = Tui(runtime, dict(early_env, HERDR_ENV="1", HERDR_SOCKET_PATH=str(early_server.path)), [])
            early.keys(b"\x1b[<64;10;8M")
            early.until(lambda: early_server.creates == 1, "early input cancelled the startup liaison handoff")
            early.until(lambda: (early_root / "harness-started").exists(), "startup handoff did not reach the harness")
            assert "automatic Board cancelled" not in early.screen()
            assert not early_server.errors, early_server.errors
        finally:
            if early:
                early.close()
            early_server.close()
        ordinary_root = root / "ordinary-onboarding"
        ordinary_root.mkdir()
        ordinary_env = dict(env, HOME=str(ordinary_root), XDG_CONFIG_HOME=str(ordinary_root / "config"),
                            PRIVATEER_FLEET=str(ordinary_root / "fleet.sqlite"),
                            PRIVATEER_RESOURCE_HOME=str(ordinary_root / "ledger"))
        ordinary_server = Herdr(ordinary_root, ordinary_env)
        ordinary_server.protocol = 19
        ordinary = None
        ordinary_home = ordinary_root / "chosen/.privateer"
        try:
            ordinary = Tui(binary, ordinary_env, ["--herdr-socket", str(ordinary_server.path)])
            ordinary.until(lambda: "HERDR UNAVAILABLE" in ordinary.screen(), "ordinary onboarding did not observe Herdr outage")
            ordinary.keys(b"\x15chosen\t\x15" + str(ordinary_home.parent).encode() + b"\t\t\x15ship\r")
            ordinary.until(lambda: "Bridge" in ordinary.screen(), "ordinary ship was not created and selected")
            assert json.loads((ordinary_home / "config.json").read_text()).get("preset") != "liaison"
            assert ordinary_server.creates == 0
            ordinary_server.protocol = 20
            ordinary.until(lambda: (ordinary_root / "harness-started").exists(), "ordinary onboarding did not Board after recovery", timeout=25)
            presence = subprocess.run([str(runtime), "--home", str(ordinary_home), "ship.presence", "--json", '{"ship":"chosen"}'],
                                      env=ordinary_env, capture_output=True, check=True, timeout=10)
            assert json.loads(presence.stdout)["attended"]
            ordinary.drain(2.5)
            assert ordinary_server.creates == 1 and ordinary_server.focuses == 0
            assert not ordinary_server.errors, ordinary_server.errors
        finally:
            if ordinary:
                ordinary.close()
            ordinary_server.close()
        multi_root = root / "multiple-presets"
        multi_root.mkdir()
        multi_env = dict(env, HOME=str(multi_root), XDG_CONFIG_HOME=str(multi_root / "config"),
                         PRIVATEER_FLEET=str(multi_root / "fleet.sqlite"), PRIVATEER_RESOURCE_HOME=str(multi_root / "ledger"))
        def multi_command(name, *arguments):
            home = multi_root / name / ".privateer"
            return subprocess.run([str(runtime), "--home", str(home), *arguments], cwd=multi_root,
                                  env=multi_env, capture_output=True, check=True, timeout=10).stdout
        for name in ["alpha", "beta", "gamma"]:
            arguments = ["init", "--ship", name, "--harness", "codex"]
            if name != "gamma":
                arguments.append("--liaison")
            multi_command(name, *arguments)
        multi_server = Herdr(multi_root, multi_env)
        multi = None
        try:
            multi = Tui(binary, multi_env, ["--herdr-socket", str(multi_server.path)])
            multi.until(lambda: "gamma" in multi.screen(), "multiple preset fleet did not load")
            multi.keys(b"l")
            assert "No liaison configured" in multi.screen()
            assert "PRESENTATION UNAVAILABLE" not in multi.screen()
            for index, name in enumerate(["alpha", "beta", "gamma"]):
                multi.keys(b"0" + b"j" * (index + 2) + b"\rb")
                multi.until(lambda: json.loads(multi_command(name, "ship.presence", "--json", json.dumps({"ship":name})))["attended"],
                            f"multiple presets blocked Board for {name}")
                assert multi_server.creates == index + 1
            multi.drain(2.5)
            alpha_config = multi_root / "alpha/.privateer/dashboard.json"
            beta_config = multi_root / "beta/.privateer/dashboard.json"
            gamma = {"id":"gamma", "name":"gamma", "liaison":True}
            alpha_config.write_text(json.dumps({"ships":[gamma]}))
            multi.until(lambda: "Liaison · your default contact" in multi.screen(), "explicit contact did not override preset candidates")
            multi.keys(b"l")
            multi.until(lambda: multi_server.focused == ["w1:p3"], "explicit contact did not focus the chosen ordinary ship")
            beta_config.write_text(json.dumps({"ships":[{"id":"beta", "name":"beta", "liaison":True}]}))
            multi.until(lambda: "PRESENTATION UNAVAILABLE" in multi.screen(), "conflicting explicit contacts did not fail safely")
            multi.keys(b"0jj\rbflf")
            multi.drain(2.2)
            assert (multi_server.creates, multi_server.focuses) == (3, 1)
            beta_config.unlink()
            multi.until(lambda: "PRESENTATION UNAVAILABLE" not in multi.screen(), "explicit contact conflict did not recover")
            bound = {key: multi_server.panes[0][key] for key in ("pane_id", "terminal_id")}
            alpha_config.write_text(json.dumps({"ships":[dict(gamma, terminals=[bound])]}))
            multi.until(lambda: "PRESENTATION UNAVAILABLE" in multi.screen(), "manual claim against another preset's live pane was accepted")
            multi.keys(b"bflf")
            multi.drain(2.2)
            assert (multi_server.creates, multi_server.focuses) == (3, 1)
            alpha_config.unlink()
            multi.until(lambda: "PRESENTATION UNAVAILABLE" not in multi.screen(), "pane conflict did not recover")
            multi.keys(b"l")
            assert "No liaison configured" in multi.screen()
            assert not multi_server.errors, multi_server.errors
        finally:
            if multi:
                multi.close()
            multi_server.close()
        replacement_root = root / "replacement"
        replacement_root.mkdir()
        replacement_env = dict(env, HOME=str(replacement_root), XDG_CONFIG_HOME=str(replacement_root / "config"),
                               PRIVATEER_FLEET=str(replacement_root / "fleet.sqlite"), PRIVATEER_RESOURCE_HOME=str(replacement_root / "ledger"))
        replacement_server = Herdr(replacement_root, replacement_env)
        replacement_server.protocol = 19
        replacement = None
        try:
            replacement = Tui(binary, replacement_env, ["--herdr-socket", str(replacement_server.path)])
            replacement.until(lambda: "HERDR UNAVAILABLE" in replacement.screen(), "replacement scenario needs a failed Herdr observation")
            original_folder = replacement_root / "original"
            original_home = original_folder / ".privateer"
            replacement.keys(b"\x15contact\t\x15" + str(original_folder).encode() + b"\t\t\x15liaison\r")
            replacement.until(lambda: (original_home / "privateer.sqlite").is_file(), "original ship was not created")
            replacement.until(lambda: "Bridge" in replacement.screen(), "original ship was not observed")
            def replacement_command(home, arguments):
                return subprocess.run([str(runtime), "--home", str(home), *arguments], cwd=replacement_root,
                                      env=replacement_env, capture_output=True, check=True, timeout=10).stdout
            replacement_command(original_home, ["ship", "retire", "contact"])
            new_folder = replacement_root / "new-home"
            new_home = new_folder / ".privateer"
            replacement_command(new_home, ["init", "--ship", "contact", "--harness", "codex", "--liaison"])
            members = json.loads(replacement_command(new_home, ["fleet.list", "--json", '{}']))["ships"]
            assert len(members) == 1 and Path(members[0]["home"]) == new_home.resolve()
            replacement_server.protocol = 20
            replacement.until(lambda: "automatic Board cancelled" in replacement.screen(), "a replacement registry mapping did not cancel the pending Board", timeout=25)
            replacement.until(lambda: "HERDR UNAVAILABLE" not in replacement.screen(), "replacement scenario did not reconnect", timeout=25)
            replacement.drain(2.5)
            assert replacement_server.creates == 0 and replacement_server.focuses == 0
            assert not (replacement_root / "harness-started").exists()
            assert not json.loads(replacement_command(new_home, ["ship.presence", "--json", '{"ship":"contact"}']))["attended"]
            assert not replacement_server.errors, replacement_server.errors
        finally:
            if replacement:
                replacement.close()
            replacement_server.close()
        for original_role in ["ship", "liaison"]:
            occupied_root = root / f"occupied-{original_role}"
            occupied_root.mkdir()
            occupied_env = dict(env, HOME=str(occupied_root), XDG_CONFIG_HOME=str(occupied_root / "config"),
                                PRIVATEER_FLEET=str(occupied_root / "fleet.sqlite"), PRIVATEER_RESOURCE_HOME=str(occupied_root / "ledger"))
            occupied = Tui(binary, occupied_env, ["--no-herdr"])
            try:
                occupied_folder = occupied_root / "desk"
                occupied_folder.mkdir()
                occupied_home = occupied_folder / ".privateer"
                def invoke(arguments):
                    return subprocess.run([str(runtime), "--home", str(occupied_home), *arguments],
                                          env=occupied_env, cwd=occupied_root, capture_output=True, check=True, timeout=10).stdout
                init = ["init", "--ship", "existing", "--harness", "codex"]
                if original_role == "liaison":
                    init.append("--liaison")
                invoke(init)
                invoke(["charter.put", "--json", json.dumps({"id":"existing", "actions":["inspect", "message"], "objective":"Captain edited"})])
                config = (occupied_home / "config.json").read_bytes()
                instructions = (occupied_folder / "AGENTS.md").read_bytes()
                charter = json.loads(invoke(["charter.show", "--json", '{"id":"existing"}']))
                registry = json.loads(invoke(["fleet.list", "--json", '{}']))
                for index, requested_role in enumerate(["liaison" if original_role == "ship" else "ship", original_role]):
                    if index:
                        occupied.keys(b"\t")
                    occupied.keys(b"\x15existing\t\x15" + str(occupied_folder).encode() + b"\t\t\x15" + requested_role.encode() + b"\r")
                    occupied.drain(1.2)
                    occupied.until(lambda: "choose an empty ship folder" in occupied.screen(), "onboarding accepted an initialized folder")
                    assert (occupied_home / "config.json").read_bytes() == config
                    assert (occupied_folder / "AGENTS.md").read_bytes() == instructions
                    assert json.loads(invoke(["charter.show", "--json", '{"id":"existing"}'])) == charter
                    assert json.loads(invoke(["fleet.list", "--json", '{}'])) == registry
            finally:
                occupied.keys(b"\x1b")
                occupied.close()
        for role, args in [("liaison", []), ("ship", ["--no-herdr"])]:
            plain_root = root / f"offline {role}'s $(touch escaped)"
            plain_root.mkdir()
            plain_env = dict(env, HOME=str(plain_root), XDG_CONFIG_HOME=str(plain_root / "config"), PRIVATEER_FLEET=str(plain_root / "fleet.sqlite"),
                             PRIVATEER_RESOURCE_HOME=str(plain_root / "ledger"))
            # Keep path components unbroken so reading back the displayed shell
            # command does not insert spaces at hard wraps in long worktrees.
            plain = Tui(binary, plain_env, args, columns=max(132, 2 * len(str(plain_root)) + 80))
            try:
                plain.keys(b"\x15apps\t\t\t\x15" + role.encode() + b"\r")
                plain_home = plain_root / "ships/apps/.privateer"
                plain.until(lambda: (plain_home / "privateer.sqlite").is_file(), f"{role} first ship was not created")
                canonical_home = plain_home.resolve(strict=True)
                command = "privateer --home " + shlex.quote(str(canonical_home)) + " start"
                def details():
                    return " ".join(" ".join(line.split("│")[-2] for line in plain.screen().splitlines()
                                             if line.count("│") >= 2).split())
                plain.until(lambda: "Created apps." in details() and "Talk to its first mate directly: " + command
                            in details(), "creation success and direct start command were not shown")
                plain.drain(2.2)
                screen = details()
                displayed = screen.split("directly: ", 1)[1].split(" start", 1)[0] + " start"
                assert shlex.split(displayed) == ["privateer", "--home", str(canonical_home), "start"]
                assert not (plain_root / "harness-started").exists(), "no-socket creation launched a session"
                config = json.loads((plain_home / "config.json").read_text())
                assert (config.get("preset") == "liaison") == (role == "liaison")
                out = subprocess.run([str(runtime), "--home", str(plain_home), "charter.show", "--json", '{"id":"apps"}'],
                                     env=plain_env, capture_output=True, check=True, timeout=10)
                # Liaison or not, a captain-made ship can work but not deploy.
                actions = json.loads(out.stdout)["actions"]
                assert "implement" in actions and "deploy_production" not in actions
                (plain_root / "release").touch()
                subprocess.run(["sh", "-c", displayed], cwd=plain_root, env=plain_env,
                               capture_output=True, check=True, timeout=10)
                assert (plain_root / "harness-started").exists()
                assert not (plain_root / "escaped").exists()
            finally:
                plain.close()
        print("Onboarding PTY passed: ordinary/Herdr bare-runtime welcome and skip without state or harness launch, renamed liaison/custom folder/installed harness, ordinary and liaison onboarding Board after Herdr recovery, bare entry opens/focuses the liaison with keyboard/mouse return and no duplicate launch, startup cancels ambiguous/removed/changed contacts and homes for idle and attended ships without resuming or retargeting, multiple presets remain boardable with explicit contact preference and conflict rejection, replacement home never boarded automatically, existing ordinary/liaison homes preserved on rejection, both offline roles with executable quoted start guidance; no model calls.")
