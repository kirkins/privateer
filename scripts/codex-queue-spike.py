#!/usr/bin/env python3
"""Opt-in native queue/attachment/receipt probe, NOT live mail qualification.

Fixture-owned daemon, throwaway homes, exact --remote/--thread, no --resume or
turn/start. All turns stop at trusted hooks except ONE explicitly selected queued
turn, permitted to reach this fixture's loopback-only mock (captain B4 scope).
No real provider, login or auth.json; this is not live model qualification.
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
from codex_binding import atomic
import codex_receipts as receipts

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
        # SessionStart stops; UserPromptSubmit permits only the exact selected
        # nonce/body/session once. SessionStart alone would NOT protect later turns.
        self.allow_body = None
        self.permit = self.root / "permit.json"
        self.allow_once = self.root / "allowed-once"
        self.stopper.write_text("import json,sys,os\n"
            "event=json.load(sys.stdin)\n"
            f"with open({str(self.hook_log)!r},'a') as out: out.write(json.dumps(event)+'\\n')\n"
            "allowed=False\n"
            f"permit_path={str(self.permit)!r}\n"
            "try:\n"
            " permit=json.load(open(permit_path))\n"
            "except (OSError,ValueError):\n"
            " permit={}\n"
            "if (event.get('hook_event_name') == 'UserPromptSubmit' and isinstance(permit,dict)\n"
            "    and permit.get('session') and isinstance(permit.get('body'),str)\n"
            "    and event.get('session_id') == permit.get('session') and event.get('prompt') == permit.get('body')):\n"
            " try:\n"
            f"  fd=os.open({str(self.allow_once)!r},os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)\n"
            "  os.close(fd)\n"
            " except OSError:\n"
            "  pass\n"
            " else:\n"
            "  allowed=True\n"
            "print(json.dumps({'continue':allowed,'stopReason':'Fixture: stop all but the one mock turn'}))\n")
        config = self.home / "config.toml"
        text = config.read_text().replace('requires_openai_auth = false',
            'requires_openai_auth = false\nrequest_max_retries = 0\nstream_max_retries = 0')
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
        # Even legacy thread/start does not immediately provide a durable
        # resumable rollout on 0.166. Fixture-only queue one hook-stopped turn
        # on the returned exact UUID before attaching the TUI. No model call,
        # cwd/newest selection, turn/start or production reservation workaround.
        with Observer(self) as daemon:
            thread = self.create(daemon, name)
            self.queue(thread["id"], "Fixture seed: stop before any provider request.")
            daemon.completed(thread["id"])
            native = daemon.call("thread/read", {"threadId": thread["id"]})["thread"]
            assert Path(native["path"]).is_file(), "hook-stopped legacy seed has no materialized rollout"
            return thread["id"]

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
            alpha_thread = observer.call("thread/read", {"threadId": session})["thread"]
            self.wait_terminal(Path(alpha_thread["path"]))
            attached = self.snapshot(observer, session)
            alpha_events = self.rollout_events(alpha_thread["path"])
            assert not any(e.get("type") == "user_message" and e.get("message") == alpha_body
                           for e in alpha_events)
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
            self.wait_terminal(Path(thread["path"]))
            events = self.rollout_events(thread["path"])
            native_starts = [e for e in events if e.get("type") == "task_started"]
            assert len(native_starts) == 1
            assert native_starts[0]["turn_attribution"]["turn_trigger"] == "queue"
            assert not any(e.get("type") == "user_message" and e.get("message") == beta_body for e in events)
            never_tui = self.snapshot(observer, beta["id"])
            # Terminate/reap only our exact real PTY child, never the daemon.
            child, master = self.children[0]
            assert child.pid == alpha["pid"]
            child.terminate()
            try:
                child.wait(timeout=5)
            except binding_fixture.subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            os.close(master)
            self.children.remove((child, master))
            detached = self.snapshot(observer, session)
        # These are negative qualification results, NOT a guessed discriminator.
        # The recorded origin/source describe creation, not current attachment.
        assert attached["loaded"] and never_tui["loaded"] and detached["loaded"]
        assert attached["status"] == never_tui["status"] == detached["status"] == {"type": "idle"}
        assert attached["thread_fields"] == never_tui["thread_fields"]
        assert not self.calls
        receipt = self.qualify_receipt(alpha)
        self.check_calls()
        return {"binding_only": False, "delivery_enabled": False,
                "cli_version": self.daemon["cliVersion"], "daemon_version": self.daemon["appServerVersion"],
                "attached_queue_reached_prompt_hook": True,
                "loaded_without_tui_queue_promoted_by_daemon": True,
                "detached_native_queue_turns": len(native_starts),
                "requires_hosted_custody_for_future_delivery": True,
                "future_delivered_turns_possibly_unattended": True,
                "hook_stopped_is_not_presentation": True,
                "attachment_observations": {"owned_tui": attached, "never_tui": never_tui,
                                            "after_owned_tui_exit": detached},
                "attached_discriminator_qualified": False,
                "attachment_blocker": "0.166 thread/read status and loaded/list do not prove a TUI subscriber; no qualified public exact-pane attachment evidence",
                "native_user_message_qualified": True,
                "native_receipt_observation": receipt,
                "mock_provider_requests": self.calls,
                "scope_change": "B4 captain allowed exactly one loopback mock request; B3b assignment forbade even mock requests",
                "runner_implemented": False,
                "model_calls": 0, "mock_calls": len(self.calls), "mail_delivery_qualified": False}

    def mock_request(self, handler):
        # The only permitted inference endpoint is our bound loopback server.
        # Return a terminal canned response without tools or retries.
        call = {"path": handler.path, "peer": handler.client_address[0],
                "host": handler.headers["Host"], "method": handler.command, "allowed_body": False}
        self.calls.append(call)  # count even malformed/unexpected requests
        def refuse(reason):
            # socketserver swallows handler exceptions. Retain the violation
            # for the main-thread check and return an explicit failure instead.
            call["violation"] = reason
            handler.send_response(500)
            handler.send_header("Content-Length", "0")
            handler.end_headers()

        if len(self.calls) != 1:
            return refuse("more than the one captain-approved mock request")
        if (call["peer"] != "127.0.0.1" or call["host"] != f"127.0.0.1:{self.server.server_port}"
                or call["path"] != "/v1/responses" or call["method"] != "POST"):
            return refuse("unexpected loopback mock endpoint")
        try:
            size = int(handler.headers.get("Content-Length", "0"))
            if not 0 < size <= 1 << 20:
                return refuse("invalid mock request size")
            request = json.loads(handler.rfile.read(size))
        except (ValueError, OSError):
            return refuse("malformed mock request")
        def contains_body(value):
            if isinstance(value, str):
                return self.allow_body is not None and self.allow_body in value
            if isinstance(value, dict):
                return any(contains_body(item) for item in value.values())
            return isinstance(value, list) and any(contains_body(item) for item in value)
        call["allowed_body"] = contains_body(request)
        if not call["allowed_body"]:
            return refuse("request lacks the one permitted notification")
        response = {"id": "resp_fixture", "object": "response", "status": "completed",
                    "output": [], "usage": {"input_tokens": 1, "output_tokens": 0, "total_tokens": 1}}
        data = "event: response.completed\ndata: " + json.dumps({"type": "response.completed", "response": response}) + "\n\n"
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Content-Length", str(len(data.encode())))
        handler.end_headers()
        handler.wfile.write(data.encode())

    def check_calls(self):
        assert len(self.calls) == 1, f"expected exactly one local mock request: {self.calls}"
        assert self.calls[0]["peer"] == "127.0.0.1"
        assert self.calls[0]["host"] == f"127.0.0.1:{self.server.server_port}"
        assert self.calls[0]["path"] == "/v1/responses" and self.calls[0]["method"] == "POST"
        assert not any("violation" in call for call in self.calls), self.calls
        assert self.calls[0]["allowed_body"] is True

    def qualify_receipt(self, record):
        session, nonce = record["session"], str(uuid.uuid4())
        registry = self.root / "alpha/.codex/privateer-attended/launch.json"
        self.allow_body = receipts.encode(receipts.marker(record["home"], record["ship"], session, nonce, 0, 1)) + "\nFixture notification, not an acknowledgement."
        ledger = receipts.Ledger(registry, record["home"], record["ship"])
        # Dormant reducer exercised by the fixture, NOT production authorization.
        # Delivery remains false. Snapshot anchor before the one permitted turn.
        ledger.prepare(record, self.allow_body, 0, 1, nonce)
        ledger.invoking(session, nonce)
        atomic(self.permit, {"session": session, "body": self.allow_body})
        with Observer(self) as observer:
            thread = observer.call("thread/read", {"threadId": session})["thread"]
            before = len(self.rollout_events(thread["path"]))
            self.queue(session, self.allow_body)
            self.wait_hook(session, self.allow_body)
            self.wait_terminal(Path(thread["path"]))
            events = self.rollout_events(thread["path"])[before:]
            # Print the ACTUAL record before asserting any contract. A failure
            # reports evidence instead of quietly guessing a client-ID alias.
            print(json.dumps({"accepted_turn_native_events": events}), file=sys.stderr)
            inputs = [event for event in events if event.get("type") == "user_message"]
            assert len(inputs) == 1
            user = inputs[0]
            client = receipts.native_client(user)
            assert user["message"] == self.allow_body
            starts = [event for event in events if event.get("type") == "task_started"]
            ends = [event for event in events if event.get("type") == "task_complete"]
            assert len(starts) == len(ends) == 1
            assert starts[0]["turn_attribution"]["turn_trigger"] == "queue"
            assert starts[0]["turn_id"] == ends[0]["turn_id"]
            assert events.index(starts[0]) < events.index(user) < events.index(ends[0])
            queue = observer.call("thread/queue/list", {"threadId": session, "limit": 128})
            # A new reducer instance proves durable anchored recovery without
            # relying on CLI stdout or a transient notification subscription.
            recovered = receipts.Ledger(registry, record["home"], record["ship"]).reconcile(session, nonce, queue)
            assert recovered["state"] == "presented"
            assert recovered["accepted_through"] == 1 and recovered["acknowledged_through"] == 0
            assert recovered["evidence"]["receipt"] == {"turn_id": starts[0]["turn_id"], "client_id": client}
            assert load(registry)["delivery_enabled"] is False
            repeated = binding_fixture.subprocess.run([sys.executable, str(self.stopper)],
                input=json.dumps({"hook_event_name": "UserPromptSubmit", "session_id": session,
                                  "prompt": self.allow_body}), env=self.env, text=True,
                capture_output=True, timeout=5, check=True)
            assert json.loads(repeated.stdout)["continue"] is False, "one-use allowance reopened"
            return {"user_message": user, "events": events, "anchored_ledger_recovered": True,
                    "one_use_allowance_rejects_repeat": True,
                    "accepted_through": 1, "acknowledged_through": 0}

    @staticmethod
    def rollout_events(path):
        rows = [json.loads(line) for line in Path(path).read_text().splitlines()]
        return [row["payload"] for row in rows if row.get("type") == "event_msg"]

    def snapshot(self, observer, session):
        loaded = observer.call("thread/loaded/list", {"limit": 32})
        assert loaded["nextCursor"] is None
        thread = observer.call("thread/read", {"threadId": session})["thread"]
        assert thread["id"] == session
        return {"loaded": session in loaded["data"], "status": thread["status"],
                "thread_fields": sorted(thread), "source": thread["source"],
                "originator": thread.get("originator")}

    def wait_terminal(self, path):
        def finished():
            events = self.rollout_events(path)
            lifecycle = [e["type"] for e in events
                         if e.get("type") in ("task_started", "task_complete", "turn_aborted")]
            return lifecycle and lifecycle[-1] in ("task_complete", "turn_aborted")
        self.wait_file(path.parent, finished)

    def wait_hook(self, session, prompt=None):
        def found():
            if not self.hook_log.exists():
                return False
            events = [json.loads(line) for line in self.hook_log.read_text().splitlines()]
            return any(e.get("session_id") == session and (prompt is None or e.get("prompt") == prompt)
                       for e in events)
        self.wait_file(self.root, found)

    def wait_file(self, folder, predicate):
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
            if libc.inotify_add_watch(fd, os.fsencode(folder), 0x00000002 | 0x00000100) < 0:
                raise OSError(ctypes.get_errno(), "fixture inotify watch")
            deadline = time.monotonic() + 30
            while True:
                if predicate():
                    return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError("native fixture filesystem barrier missing")
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
