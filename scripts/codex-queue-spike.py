#!/usr/bin/env python3
"""B2 opt-in native queue-promoter fixture, NOT live mail qualification.

Fixture-owned daemon, throwaway homes, exact --remote/--thread, no --resume or
turn/start. Fixture-only hooks stop every turn BEFORE any provider request.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import sys
import time
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
from codex_socket import Daemon, load

spec = importlib.util.spec_from_file_location("binding_fixture", ROOT / "scripts/codex-binding-spike.py")
binding_fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(binding_fixture)


class Observer(Daemon):
    # Mutations are confined to this opt-in fixture. Production Daemon remains
    # read-only. Every connection verifies the fixture PID as well as peer creds.
    METHODS = Daemon.METHODS | {"thread/start"}

    def __init__(self, fixture, timeout=30):
        self.events = []
        super().__init__(fixture.home, timeout)
        assert self.pid == fixture.daemon["pid"]

    def receive(self):
        message = super().receive()
        if "method" in message:
            self.events.append(message)
        return message

    def completed(self, session):
        while True:
            for event in self.events:
                params = event.get("params", {})
                if event.get("method") == "turn/completed" and params.get("threadId") == session:
                    return params
            self.receive()


class Fixture(binding_fixture.Fixture):
    def setup(self):
        super().setup()
        self.hook_log = self.root / "hook-events.jsonl"
        # Both SessionStart and UserPromptSubmit stop. SessionStart alone only
        # stops the first entry and would NOT protect later queued turns.
        self.stopper.write_text("import json,sys\n"
            "event=json.load(sys.stdin)\n"
            f"with open({str(self.hook_log)!r},'a') as out: out.write(json.dumps(event)+'\\n')\n"
            "print(json.dumps({'continue':False,'stopReason':'Fixture: no provider call'}))\n")
        config = self.home / "config.toml"
        text = config.read_text()
        for name in ("alpha", "beta"):
            hooks = self.root / name / ".codex/hooks.json"
            value = json.loads(hooks.read_text())
            value["hooks"]["UserPromptSubmit"] = [{"hooks": [{"type": "command",
                "command": shlex.join([sys.executable, str(self.stopper)])}]}]
            hooks.write_text(json.dumps(value))
            with Observer(self) as daemon:
                entries = daemon.call("hooks/list", {"cwds": [str(self.root / name)]})["data"]
                for hook in entries[0]["hooks"]:
                    if hook["eventName"] == "userPromptSubmit":
                        text += f'\n[hooks.state.{json.dumps(hook["key"])}]\ntrusted_hash = {json.dumps(hook["currentHash"])}\n'
        config.write_text(text)

    def create(self, daemon, name):
        thread = daemon.call("thread/start", {"cwd": str(self.root / name),
            "historyMode": "legacy", "ephemeral": False})["thread"]
        assert thread["historyMode"] == "legacy"
        assert thread["id"] == thread["sessionId"]
        return thread

    def seed(self, name):
        # Fixture-only zero-turn materialized create. Not a production binder or
        # a claim that paginated reservation has become safe.
        with Observer(self) as daemon:
            return self.create(daemon, name)["id"]

    def queue(self, session, body):
        with Observer(self) as daemon:
            assert session in daemon.call("thread/loaded/list", {"limit": 32})["data"]
            daemon.check()
            # Intentionally discard English stdout: it is not admission or a
            # presentation receipt. Recovery must use native evidence instead.
            self.run([self.codex, "queue", "--remote", "unix://" + str(daemon.target),
                      "--thread", session, "--message", body])

    def probe(self):
        # First an exact-resume real TUI, with binding/delivery still disabled.
        alpha = self.pane("alpha")
        session = alpha["session"]
        with Observer(self) as observer:
            # Subscribe to this exact already loaded UUID without a mutating
            # resume: thread/read notifications aren't guaranteed subscriptions;
            # rollout+hook log provide the durable barrier below instead.
            alpha_body = "alpha native queue probe " + str(uuid.uuid4())
            self.queue(session, alpha_body)
            self.wait_hook(session, alpha_body)
        # No TUI has ever attached to beta. The fixture app-server client creates
        # and keeps the thread loaded; only codex queue can trigger this turn.
        with Observer(self) as observer:
            beta = self.create(observer, "beta")
            beta_body = "beta detached native queue probe " + str(uuid.uuid4())
            self.queue(beta["id"], beta_body)
            completed = observer.completed(beta["id"])
            assert completed["turn"]["status"] in ("completed", "interrupted")
            self.wait_hook(beta["id"])
            queue = observer.call("thread/queue/list", {"threadId": beta["id"], "limit": 128})
            assert queue == {"data": [], "nextCursor": None}
            starts = [event for event in observer.events if event.get("method") == "turn/started"
                      and event.get("params", {}).get("threadId") == beta["id"]]
            assert len(starts) == 1
            # Read the materialized native record, not quoted marker prose.
            thread = observer.call("thread/read", {"threadId": beta["id"]})["thread"]
            rows = [json.loads(line) for line in Path(thread["path"]).read_text().splitlines()]
            events = [row["payload"] for row in rows if row.get("type") == "event_msg"]
            native_starts = [e for e in events if e.get("type") == "task_started"]
            assert len(native_starts) == 1
            assert native_starts[0]["turn_attribution"]["turn_trigger"] == "queue"
            assert not any(e.get("type") == "user_message" and e.get("message") == beta_body for e in events)
        assert not self.calls
        return {"binding_only": False, "delivery_enabled": False,
                "cli_version": self.daemon["cliVersion"], "daemon_version": self.daemon["appServerVersion"],
                "attached_queue_reached_prompt_hook": True,
                "loaded_without_tui_queue_promoted_by_daemon": True,
                "detached_native_queue_turns": len(native_starts),
                "requires_attached_for_future_delivery": True,
                "hook_stopped_is_not_presentation": True,
                "model_calls": len(self.calls), "mail_delivery_qualified": False}

    def wait_hook(self, session, prompt=None):
        # Event-driven filesystem barrier, not a sleep/retry wrapper. Install an
        # inotify watch before checking the file; drain TUI output concurrently
        # so a native terminal query cannot block the observed hook.
        import ctypes
        import select
        libc = ctypes.CDLL(None, use_errno=True)
        fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        if fd < 0:
            raise OSError(ctypes.get_errno(), "fixture inotify init")
        try:
            if libc.inotify_add_watch(fd, os.fsencode(self.root), 0x00000002 | 0x00000100) < 0:
                raise OSError(ctypes.get_errno(), "fixture inotify watch")
            deadline = time.monotonic() + 30
            while True:
                if self.hook_log.exists():
                    events = [json.loads(line) for line in self.hook_log.read_text().splitlines()]
                    if any(e.get("session_id") == session and (prompt is None or e.get("prompt") == prompt)
                           for e in events):
                        return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError("native queued hook barrier missing")
                masters = [master for child, master in self.children]
                ready, _, _ = select.select([fd, *masters], [], [], remaining)
                for source in ready:
                    data = os.read(source, 65536)
                    if source != fd:
                        for query, response in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"),
                                                (b"\x1b[>c", b"\x1b[>0;0;0c")]:
                            if query in data:
                                os.write(source, response)
        finally:
            os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--codex", required=True)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("fixture requires Linux peer/process identity")
    fixture = Fixture(args.binary, args.codex)
    try:
        fixture.setup()
        result = fixture.probe()
    finally:
        fixture.close()
    print(json.dumps(result))


if __name__ == "__main__":
    main()
