#!/usr/bin/env python3
"""ACT3b native PTY evidence, NOT a live hosted-mail/autonomy pilot.

Installed Codex, owned app-server, isolated homes, one loopback mock request.
Use a new private /var/tmp root; all logs and evidence are retained there.
No credentials, managed daemon installation, production grant or route changes.
The raw bounded logs/evidence remain in --root. Native attachment gaps are
reported as negative qualification, never inferred from loaded/idle.
"""
import argparse
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import shlex
import struct
import subprocess
import sys
import termios
import threading
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
import codex_binding as binding
import codex_receipts as receipts
from codex_socket import Daemon, process_identity, socket_paths


class Native(Daemon):
    METHODS = Daemon.METHODS | {"thread/start", "thread/name/set"}


class Pane:
    def __init__(self, fixture, label, argv, env):
        self.fixture, self.label = fixture, label
        self.log = bytearray()
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
        try:
            self.child = subprocess.Popen(argv, cwd=fixture.folder, env=env, stdin=slave,
                stdout=slave, stderr=slave, start_new_session=True)
        finally:
            os.close(slave)
        fixture.panes.append(self)

    def drain(self):
        try:
            data = os.read(self.master, 65536)
        except OSError:
            data = b""
        self.log.extend(data)
        assert len(self.log) <= 2 << 20, "PTY log bound"
        for query, answer in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"),
                              (b"\x1b[>c", b"\x1b[>0;0;0c")]:
            if query in data:
                os.write(self.master, answer)
        return data

    def send(self, text):
        os.write(self.master, text.encode())

    def stop(self):
        if self.master is None:
            return
        if self.child.poll() is None:
            self.child.terminate()
        try:
            self.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait(timeout=5)
        (self.fixture.root / (self.label + ".pty.log")).write_bytes(self.log)
        os.close(self.master)
        self.master = None
        assert not binding.live({"pid": self.child.pid,
                                 "process_identity": self.fixture.pane_identities.get(self.child.pid)}), "pane still live"


class Fixture:
    def __init__(self, root, codex):
        self.root = root.resolve()
        assert self.root.is_relative_to(Path("/var/tmp")) and self.root != Path("/var/tmp")
        self.root.mkdir(mode=0o700, exist_ok=False)
        self.codex = str(Path(codex).resolve(strict=True))
        self.env = {"PATH": "/usr/bin:/bin", "TERM": "xterm-256color", "PRIVATEER_SERVICE": "none"}
        for key, name in {"HOME": "home", "CODEX_HOME": "c", "TMPDIR": "tmp",
                          "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state",
                          "PRIVATEER_RESOURCE_HOME": "resources", "PRIVATEER_HOME": "privateer"}.items():
            (self.root / name).mkdir(mode=0o700)
            self.env[key] = str(self.root / name)
        self.env["PRIVATEER_FLEET"] = str(self.root / "fleet.sqlite")
        self.home, self.folder = self.root / "c", self.root / "alpha"
        self.folder.mkdir(mode=0o700)
        self.panes, self.pane_identities, self.calls = [], {}, []
        self.daemon = self.server = self.worker = None
        self.evidence = {"version": subprocess.check_output([self.codex, "--version"], env=self.env, text=True).strip(),
                         "provider_requests": self.calls, "qualification": {}}
        self.registry = self.folder / ".codex/privateer-attended/launch.json"
        self.hook_log, self.permit = self.root / "hooks.jsonl", self.root / "permit.json"
        self.gate = self.root / "hook-release"
        os.mkfifo(self.gate, 0o600)
        # Open RDWR so a bounded writer never blocks waiting for a hook reader.
        self.gate_fd = os.open(self.gate, os.O_RDWR | os.O_NONBLOCK)

    def client(self):
        fixture = self
        class Owned(Native):
            def __init__(self):
                super().__init__(fixture.home, timeout=10)
                assert self.pid == fixture.daemon.pid
        return Owned()

    def setup(self):
        subprocess.run(["git", "init", "-q", str(self.folder)], env=self.env, check=True)
        for name in ("app-server-control", "app-server-daemon"):
            (self.home / name).mkdir(mode=0o700)
        self.rendezvous, self.physical = socket_paths(self.home)
        for path in (self.rendezvous, self.physical, self.physical.with_suffix(".lock")):
            assert not path.exists() and not path.is_symlink()
        fixture = self
        class Mock(BaseHTTPRequestHandler):
            def do_POST(self):
                size = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(size) if 0 < size <= 1 << 20 else b""
                def contains(value):
                    if isinstance(value, str):
                        return fixture.allowed in value
                    if isinstance(value, dict):
                        return any(contains(item) for item in value.values())
                    return isinstance(value, list) and any(contains(item) for item in value)
                try:
                    permitted = contains(json.loads(body))
                except ValueError:
                    permitted = False
                valid = (self.path == "/v1/responses" and self.client_address[0] == "127.0.0.1"
                         and self.headers["Host"] == f"127.0.0.1:{fixture.server.server_port}"
                         and len(fixture.calls) == 0 and permitted)
                fixture.calls.append({"path": self.path, "valid": valid})
                response = {"id": "resp_fixture", "object": "response", "status": "completed", "output": [],
                            "usage": {"input_tokens": 1, "output_tokens": 0, "total_tokens": 1}}
                data = ("event: response.completed\ndata: " + json.dumps(
                    {"type": "response.completed", "response": response}) + "\n\n").encode()
                self.send_response(200 if valid else 500)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            def log_message(self, *args):
                pass
        self.allowed = "NOT PERMITTED"
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        self.worker = threading.Thread(target=self.server.serve_forever)
        self.worker.start()
        stopper = self.root / "stop.py"
        # Every input is logged before the gate. Only one exact body can pass,
        # guarded by O_EXCL; all captain and navigation inputs stop pre-provider.
        stopper.write_text("import json,os,select,sys\n"
            "e=json.load(sys.stdin)\n"
            f"with open({str(self.hook_log)!r},'a') as f:\n f.write(json.dumps(e)+'\\n'); f.flush(); os.fsync(f.fileno())\n"
            f"try: p=json.load(open({str(self.permit)!r}))\nexcept (OSError,ValueError): p={{}}\n"
            "allowed=False\n"
            "if e.get('hook_event_name')=='UserPromptSubmit' and e.get('prompt')==p.get('body'):\n"
            f" fd=os.open({str(self.gate)!r},os.O_RDONLY|os.O_NONBLOCK)\n"
            " try:\n"
            "  if not select.select([fd],[],[],20)[0]: raise RuntimeError('fixture gate timeout')\n"
            "  os.read(fd,1)\n"
            " finally: os.close(fd)\n"
            " if p.get('allow'):\n"
            f"  fd=os.open({str(self.root / 'allowed-once')!r},os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)\n"
            "  os.close(fd); allowed=True\n"
            "print(json.dumps({'continue':allowed,'stopReason':'Fixture input barrier'}))\n")
        project = self.folder / ".codex"
        project.mkdir(mode=0o700)
        command = shlex.join(["/usr/bin/python3", "-I", "-B", str(stopper)])
        (project / "hooks.json").write_text(json.dumps({"hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": binding.hook_command(self.registry), "timeout": 10}]},
                             {"hooks": [{"type": "command", "command": command}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": command, "timeout": 25}]}]}}))
        config = f'''model = "mock-model"
model_provider = "fixture"
check_for_update_on_startup = false
[model_providers.fixture]
name = "Loopback only"
base_url = "http://127.0.0.1:{self.server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
[analytics]
enabled = false
[feedback]
enabled = false
[projects.{json.dumps(str(self.folder))}]
trust_level = "trusted"
'''
        (self.home / "config.toml").write_text(config)
        self.start_daemon()
        with self.client() as daemon:
            hooks = daemon.call("hooks/list", {"cwds": [str(self.folder)]})["data"][0]["hooks"]
            for hook in hooks:
                config += f'\n[hooks.state.{json.dumps(hook["key"])}]\ntrusted_hash = {json.dumps(hook["currentHash"])}\n'
            (self.home / "config.toml").write_text(config)
            thread = daemon.call("thread/start", {"cwd": str(self.folder), "historyMode": "legacy", "ephemeral": False})["thread"]
            self.session = thread["id"]
            daemon.call("thread/name/set", {"threadId": self.session, "name": "ACT3b fixture"})
        # Offline control protocol double: no actual hosted lease/grant/runtime.
        control = self.root / "control.py"
        control.write_text('#!/usr/bin/python3 -I\nimport json,sys\nprint(json.dumps({"attended":True,"lease":{"holder":"host:alpha"}} if sys.argv[3]=="ship.presence" else {"recorded":True}))\n')
        control.chmod(0o700)
        self.context = {**self.env, "PRIVATEER_CONTROL_CLI": str(control), "PRIVATEER_ROLE": "agent",
            "PRIVATEER_ACTOR": "alpha", "PRIVATEER_HOSTED": "1", "PRIVATEER_PRESENCE_HOLDER": "host:alpha"}
        launcher = self.root / "launch.py"
        launcher.write_text("import os,sys\nsys.path.insert(0," + repr(str(ROOT / "plugins/attended-codex")) + ")\n"
            "from codex_binding import prepare\na=prepare(sys.argv[1:],os.environ); os.execv(a[0],a)\n")
        self.launcher = launcher

    def start_daemon(self):
        for path in (self.rendezvous, self.physical, self.physical.with_suffix(".lock")):
            assert not path.exists() and not path.is_symlink()
        with (self.root / "daemon.log").open("a") as log:
            self.daemon = subprocess.Popen([self.codex, "app-server", "--listen", "unix://" + str(self.rendezvous)],
                env=self.env, cwd=self.root, stdout=log, stderr=log)
        self.identity = process_identity(self.daemon.pid)
        binding.atomic(self.home / "app-server-daemon/daemon.pid", {
            "pid": self.daemon.pid, "processIdentity": self.identity,
            "processStartTime": subprocess.check_output(["ps", "-o", "lstart=", "-p", str(self.daemon.pid)], text=True).strip()})
        self.wait(lambda: self.rendezvous.is_symlink())

    def stop_daemon(self):
        if self.daemon is None:
            return
        if self.daemon.poll() is None:
            assert process_identity(self.daemon.pid) == self.identity
            self.daemon.terminate()
        try:
            self.daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait(timeout=10)
        try:
            process_identity(self.daemon.pid)
        except (FileNotFoundError, ValueError):
            pass
        else:
            raise AssertionError("owned daemon still live")
        self.evidence["daemon_exited_confirmed"] = True
        for path in (self.rendezvous, self.physical, self.physical.with_suffix(".lock")):
            if path.exists() or path.is_symlink():
                assert path.lstat().st_uid == os.getuid()
                path.unlink()
        self.daemon = None

    def wait(self, predicate, timeout=20):
        deadline = time.monotonic() + timeout
        while True:
            if predicate():
                return
            assert self.daemon is None or self.daemon.poll() is None, "owned daemon exited"
            remaining = deadline - time.monotonic()
            assert remaining > 0, "fixture barrier timed out"
            active = [pane for pane in self.panes if pane.master is not None]
            ready, _, _ = select.select([pane.master for pane in active], [], [], min(.05, remaining))
            for pane in active:
                if pane.master in ready:
                    pane.drain()

    def attach(self, label, attended=False):
        settings = ["--model", "ship-model", "-c", 'model_reasoning_effort="high"']
        argv = [self.codex, "resume", self.session, "--remote", "unix://" + str(self.physical),
                "--no-alt-screen", "-C", str(self.folder), *settings]
        # Production prepare owns and verifies --remote; omit it from its input.
        if attended:
            argv = ["/usr/bin/python3", "-I", "-B", str(self.launcher), self.codex,
                    "resume", self.session, "--no-alt-screen", "-C", str(self.folder), *settings]
        pane = Pane(self, label, argv, self.context if attended else self.env)
        self.pane_identities[pane.child.pid] = process_identity(pane.child.pid)
        self.wait(lambda: b"Hook stopped" in pane.log if attended else b"state=idle" in pane.log)
        assert pane.child.poll() is None
        assert b"Running without the shared background server" not in pane.log
        return pane

    def thread(self):
        with self.client() as daemon:
            return daemon.call("thread/read", {"threadId": self.session})["thread"]

    def hooks(self):
        if not self.hook_log.exists():
            return []
        return [json.loads(line) for line in self.hook_log.read_text().splitlines()]

    def prompt_count(self, body):
        return sum(e.get("hook_event_name") == "UserPromptSubmit" and e.get("prompt") == body for e in self.hooks())

    def events(self):
        return [row["payload"] for row in (json.loads(line) for line in Path(self.thread()["path"]).read_text().splitlines())
                if row.get("type") == "event_msg"]

    def terminal(self):
        lifecycle = [e["type"] for e in self.events() if e.get("type") in ("task_started", "task_complete", "turn_aborted")]
        return lifecycle and lifecycle[-1] in ("task_complete", "turn_aborted")

    def queue(self, body):
        with self.client() as daemon:
            daemon.check()
            subprocess.run([self.codex, "queue", "--remote", "unix://" + str(daemon.target),
                "--thread", self.session, "--message", body], env=self.env, cwd=self.folder,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10, check=True)

    def probe(self):
        pane = self.attach("owned", attended=True)
        record = binding.load(self.registry)
        assert record["state"] == "bound" and not record["delivery_enabled"]
        self.wait(self.terminal)
        # 1: A complete draft remains unsent through a native queued turn.
        draft = "ACT3b unsent captain draft"
        pane.send(draft)
        self.wait(lambda: draft.encode() in pane.log)
        body = "ACT3b stopped draft-preservation mail"
        binding.atomic(self.permit, {"body": body, "allow": False})
        self.queue(body)
        self.wait(lambda: self.prompt_count(body) == 1)
        assert self.prompt_count(draft) == 0
        os.write(self.gate_fd, b"x")
        self.wait(self.terminal)
        assert self.prompt_count(draft) == 0
        pane.send("\r")
        self.wait(lambda: self.prompt_count(draft) == 1)
        self.wait(self.terminal)
        self.evidence["qualification"]["draft"] = {"unsent_during_delivery": True,
            "submitted_exactly_once_after_explicit_enter": self.prompt_count(draft) == 1}
        # 2: Hold native mail at UserPromptSubmit, type a suffix and submit.
        # Only this queue-triggered mail may reach the loopback mock once.
        prefix, suffix = "ACT3b racing captain", " typed suffix"
        pane.send(prefix)
        self.wait(lambda: prefix.encode() in pane.log)
        nonce = "7bcb5e78-5c21-4d1a-a360-30fabee2ba2a"
        self.allowed = receipts.encode(receipts.marker(record["home"], "alpha", self.session, nonce, 0, 1)) + "\nACT3b mail notification, not acknowledgement."
        ledger = receipts.Ledger(self.registry, record["home"], "alpha")
        ledger.prepare(record, self.allowed, 0, 1, nonce)
        ledger.invoking(self.session, nonce)
        binding.atomic(self.permit, {"body": self.allowed, "allow": True})
        before = len(self.events())
        self.queue(self.allowed)
        self.wait(lambda: self.prompt_count(self.allowed) == 1)
        busy_status = self.thread()["status"]
        assert busy_status["type"] == "active", busy_status
        # Native busy composer uses Tab to queue, not Enter (which retains
        # the draft). Exercise its actual submit key, without terminal injection
        # by the adapter: these bytes represent only the fixture captain.
        pane.send(suffix)
        self.wait(lambda: b"suffix" in pane.log)
        pane.send("\t")
        # This is the TUI's LOCAL follow-up queue, not daemon queue/list.
        # Its rendered acknowledgment is the barrier before mail may finish.
        self.wait(lambda: b"Queued follow-up inputs" in pane.log)
        os.write(self.gate_fd, b"x")
        captain = prefix + suffix
        self.wait(lambda: self.prompt_count(captain) == 1)
        self.wait(self.terminal)
        events = self.events()[before:]
        users = [e for e in events if e.get("type") == "user_message"]
        assert len(users) == 1 and users[0]["message"] == self.allowed
        with self.client() as daemon:
            queue = daemon.call("thread/queue/list", {"threadId": self.session, "limit": 128})
        result = receipts.Ledger(self.registry, record["home"], "alpha").reconcile(self.session, nonce, queue)
        assert result["state"] == "presented" and result["accepted_through"] == 1
        assert result["acknowledged_through"] == 0
        assert self.prompt_count(self.allowed) == self.prompt_count(captain) == 1
        self.evidence["qualification"]["race"] = {"captain_prompt": captain, "captain_hook_count": 1,
            "mail_hook_count": 1, "busy_status": busy_status, "events": events, "receipt": result["evidence"]["receipt"],
            "accepted_through": 1, "acknowledged_through": 0, "recovered_from_new_ledger": True}
        # Reverse ordering: captain wins the idle->busy race before delivery.
        # Hold that ordinary user turn at its hook and prove native queue/add
        # retains exactly one pending mail, without interruption or a receipt.
        submit_first = "ACT3b captain submit first"
        queued_later = "ACT3b mail after captain submission"
        pane.send(submit_first)
        self.wait(lambda: submit_first.encode() in pane.log)
        binding.atomic(self.permit, {"body": submit_first, "allow": False})
        before = len(self.events())
        pane.send("\r")
        self.wait(lambda: self.prompt_count(submit_first) == 1)
        self.queue(queued_later)
        with self.client() as daemon:
            pending = daemon.call("thread/queue/list", {"threadId": self.session, "limit": 128})
        assert len(pending["data"]) == 1 and pending["data"][0]["input"] == [
            {"type": "text", "text": queued_later, "text_elements": []}]
        assert self.prompt_count(queued_later) == 0
        os.write(self.gate_fd, b"x")
        self.wait(lambda: self.prompt_count(queued_later) == 1)
        self.wait(self.terminal)
        reverse = self.events()[before:]
        starts = [e for e in reverse if e.get("type") == "task_started"]
        assert [e["turn_attribution"]["turn_trigger"] for e in starts] == ["user", "queue"]
        assert not any(e.get("type") == "user_message" for e in reverse)
        self.evidence["qualification"]["submit_first_race"] = {"captain_hook_count": self.prompt_count(submit_first),
            "mail_hook_count": self.prompt_count(queued_later), "pending_count_before_release": 1,
            "turn_triggers": ["user", "queue"], "pending": pending,
            "events": reverse, "stopped_hooks_are_not_receipts": True}
        # 3: Same UUID second idle client is a known negative discriminator.
        # Fixture-owned subset of the Rust custody record, for the callback
        # revocation consumer only. NOT a runtime lease/grant or runner proof.
        binding.atomic(self.registry.with_name("custody.json"), {
            "state": "owned", "generation": record["generation"]})
        custody_token = binding.load(self.registry.with_name("custody-poison.json"))["token"]
        hooks_before = len(self.hooks())
        second = self.attach("second")
        snapshot = self.thread()
        assert len(self.hooks()) == hooks_before, "idle attach unexpectedly fired a callback"
        assert snapshot["status"] == {"type": "idle"}
        self.evidence["qualification"]["second_client"] = {"idle_attach_callback": False,
            "native_status": snapshot["status"], "binding_state": binding.load(self.registry)["state"],
            "fenced_at_attach": False, "reason": "native public API has no per-TUI attachment identity"}
        second.send("ACT3b second-client probe")
        self.wait(lambda: b"second-client probe" in second.log)
        second.send("\r")
        self.wait(lambda: self.prompt_count("ACT3b second-client probe") == 1)
        self.wait(self.terminal)
        observed = self.hooks()[hooks_before:]
        assert not any(e.get("hook_event_name") == "SessionStart" for e in observed)
        assert binding.load(self.registry.with_name("custody-poison.json"))["token"] == custody_token
        self.evidence["qualification"]["second_client"].update(
            first_submission_session_start=False, first_submission_hooks=observed,
            first_submission_fenced=False, fixture_custody_subset=True,
            resume_callback_fence="deterministic tests only; this native attach does not emit one")
        second.stop()
        self.wait(self.terminal)
        # 4: Exit and exact reconnect: old process proof is dead, prepare starts
        # a new generation. The ledger remains bound to this exact UUID.
        old_generation = record["generation"]
        pane.stop()
        assert not binding.live(record)
        same_daemon = self.attach("same-daemon-reconnect", attended=True)
        same_record = binding.load(self.registry)
        assert same_record["state"] == "bound"
        assert same_record["binding"]["settings"] == {"model": "ship-model", "effort": "high"}
        assert same_record["generation"] != old_generation and same_record["session"] == self.session
        assert same_record["binding"]["daemon_pid"] == record["binding"]["daemon_pid"]
        assert "--remote" in same_record["argv"]
        self.evidence["qualification"]["same_daemon_reconnect"] = {
            "state": same_record["state"], "delivery_enabled": same_record["delivery_enabled"],
            "new_generation": True, "same_uuid": True, "same_daemon": True,
            "settings": same_record["binding"]["settings"], "session_start": True,
            "prepare_argv": same_record["argv"], "attach_daemon": same_record["attach_daemon"]}
        same_daemon.stop()
        self.stop_daemon()
        self.start_daemon()
        replacement = self.attach("reconnect-after-daemon-restart", attended=True)
        fresh = binding.load(self.registry)
        assert fresh["generation"] != old_generation and fresh["session"] == self.session
        assert fresh["state"] == "bound"
        assert fresh["binding"]["settings"] == {"model": "ship-model", "effort": "high"}
        assert fresh["attach_daemon"] != same_record["attach_daemon"]
        assert self.thread()["modelProvider"] == "fixture"
        recovered = receipts.Ledger(self.registry, record["home"], "alpha").reconcile(self.session, nonce, queue)
        assert recovered["accepted_through"] == 1 and recovered["acknowledged_through"] == 0
        self.evidence["qualification"]["reconnect"] = {"dead_old_process_refused": True,
            "new_generation": True, "same_uuid": True, "accepted_through": 1, "replayed": False,
            "settings": fresh["binding"]["settings"], "new_daemon": True,
            "prepare_argv": fresh["argv"], "attach_daemon": fresh["attach_daemon"]}
        self.wait(self.terminal)
        # Idle /new is invisible to SessionStart until first turn entry.
        hooks_before = len(self.hooks())
        replacement.send("/new")
        self.wait(lambda: b"/new" in replacement.log)
        replacement.send("\r")
        def navigated():
            with self.client() as daemon:
                return any(identity != self.session for identity in
                           daemon.call("thread/loaded/list", {"limit": 32})["data"])
        self.wait(navigated)
        assert len(self.hooks()) == hooks_before
        before_navigation = binding.load(self.registry)
        replacement.send("ACT3b navigation probe")
        self.wait(lambda: b"navigation probe" in replacement.log)
        replacement.send("\r")
        self.wait(lambda: any(e.get("hook_event_name") == "SessionStart" and
                             e.get("session_id") != self.session for e in self.hooks()))
        self.wait(lambda: binding.load(self.registry)["state"] == "unbound")
        poison = binding.load(self.registry.parent / "custody-poison.json")
        assert poison.get("reason") == "callback_uuid_changed"
        self.evidence["qualification"]["navigation"] = {"idle_callback": False,
            "binding_before_turn": before_navigation["state"], "callback_unbound": True,
            "custody_poison_reason": poison["reason"], "instantaneous_ui_continuity": False}
        replacement.stop()
        assert self.calls == [{"path": "/v1/responses", "valid": True}], self.calls
        self.evidence["hooks"] = self.hooks()
        self.evidence["live_mail_qualified"] = False

    def close(self):
        try:
            for pane in self.panes:
                pane.stop()
            self.stop_daemon()
        finally:
            os.close(self.gate_fd)
            if self.server is not None:
                self.server.shutdown()
                self.server.server_close()
                self.worker.join()
            (self.root / "evidence.json").write_text(json.dumps(self.evidence, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--root", type=Path, required=True, help="new private /var/tmp directory; retained")
    args = parser.parse_args()
    fixture = Fixture(args.root, args.codex)
    try:
        fixture.setup()
        fixture.probe()
    finally:
        fixture.close()
    print(json.dumps({"evidence": str(fixture.root / "evidence.json"),
                      "qualification": fixture.evidence["qualification"], "mock_requests": len(fixture.calls)}))


if __name__ == "__main__":
    main()
