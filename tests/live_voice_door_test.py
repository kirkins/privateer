#!/usr/bin/env python3
"""The live voice front door, offline.

Runs plugins/live-voice/door.py as a real subprocess speaking front door
protocol v1 (this test plays the Privateer runtime), its page API over
loopback HTTP, and a fake `codex app-server` (tests/fake_live_voice_app_server.py)
over real pipes. No network beyond 127.0.0.1 and no real Codex.

Covers pairing and tokens, the Host and Origin checks and pairing rate limits,
the webrtc sequence and its failures, the captain's own Codex extras turned
off, a closed page ending its session, mail delivered only after it is spoken,
in order per ship and deduped by ship and id, a backlog counted in one line
and never read aloud (summary, skip, age and gap rules, restarts, bounded
state), live mail paced per batch, long mail in short form, the exact-words read-back and a
confirmation that names the recipient, every never-approve rule,
delegated-turn handoffs, declined server requests, and session stop and
restart without duplicate sends.

Usage: python3 tests/live_voice_door_test.py
"""

import http.client
import io
import json
import os
import pathlib
import queue
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLUGIN = ROOT / "plugins" / "live-voice"
DOOR = PLUGIN / "door.py"
FAKE = ROOT / "tests" / "fake_live_voice_app_server.py"
sys.dont_write_bytecode = True
sys.path.insert(0, str(PLUGIN))
import door as door_module  # noqa: E402
import pacing  # noqa: E402
import rules  # noqa: E402
import voice  # noqa: E402

OFFER = ("v=0\r\no=- 7 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n"
         "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\n"
         "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n")
COUNT_ONE = ("You have 1 older message from 1 ship. "
             "Say summary to hear who they are from, or skip.")
SHIPS = [{"name": "liaison", "liaison": True, "scope": "talks to the captain"},
         {"name": "ops", "liaison": False, "scope": "infrastructure"}]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for(what, found, timeout=10.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = found()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {what}")


def now_ms() -> int:
    return int(time.time() * 1000)


def mail(ident, kind="request", sender="ops", body="hello", payload=None, **extra):
    message = {"id": ident, "from": sender, "to": "captain", "kind": kind, "body": body,
               "payload": payload or {}}
    message.update(extra)
    return message


class Door:
    """door.py under test, with this test as its runtime."""

    def __init__(self, test, scenario=None, config=None, state=None, config_path=None,
                 up_timeout=10.0):
        self.test = test
        self.tmp = tempfile.mkdtemp(prefix="live-voice-test-")
        self.state = state or os.path.join(self.tmp, "state")
        if state:
            # The earlier door's address; wait for this one's.
            os.unlink(os.path.join(state, "url"))
        self.ctl = os.path.join(self.tmp, "ctl")
        os.makedirs(os.path.join(self.ctl, "inject"))
        with open(os.path.join(self.ctl, "scenario.json"), "w") as f:
            json.dump(scenario or {}, f)
        self.port = free_port()
        self.config = os.path.join(self.tmp, "config.toml")
        settings = {"port": self.port, "codex": [sys.executable, str(FAKE)],
                    "draft_ttl_secs": 30, "start_timeout_secs": 5, "rpc_timeout_secs": 5,
                    "grace_secs": 2, "heartbeat_secs": 60, **(config or {})}
        with open(self.config, "w") as f:
            # JSON scalars and string lists are valid TOML values.
            f.write("".join(f"{k} = {json.dumps(v)}\n" for k, v in settings.items()))
        os.chmod(self.config, 0o600)
        env = dict(os.environ, FAKE_VOICE_DIR=self.ctl, OPENAI_API_KEY="sk-test-not-for-codex")
        self.stderr = open(os.path.join(self.tmp, "door.log"), "w")
        self.proc = subprocess.Popen([sys.executable, "-B", str(DOOR)], cwd=PLUGIN, env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True)
        self.requests: list = []
        self.delivered: list = []
        self.sends: list = []
        self.waiting: list = []
        self.send_error = None
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()
        self.write({"type": "hello", "version": 1, "door": "live-voice",
                    "config_path": config_path or self.config, "state_dir": self.state,
                    "recipient": "liaison"})
        self.token = None
        self.injected = 0
        self.prefix = door_module.SPEAK_PREFIX
        self.wait_up(up_timeout)

    def wait_up(self, timeout=10.0):
        """Until the door writes its page's address; a door that exits or
        stalls first fails with its exit status, its stderr and, while it
        still runs, every thread's stack (door.py dumps them on SIGUSR1)."""
        url = os.path.join(self.state, "url")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if os.path.exists(url):
                return
            if self.proc.poll() is not None:
                break
            time.sleep(0.02)
        if os.path.exists(url):
            return
        status = self.proc.poll()
        if status is None:
            self.proc.send_signal(signal.SIGUSR1)
            time.sleep(0.5)
            what = f"still running after {timeout:g}s"
        else:
            what = f"exited with status {status}"
        self.proc.kill()
        self.close()
        self.test.fail(f"the door did not come up ({what}); its stderr:\n{self.log()}")

    # runtime side
    def write(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def _read(self):
        for line in self.proc.stdout:
            message = json.loads(line)
            if message["type"] == "delivered":
                with self.lock:
                    self.delivered.append(message["ref"])
                continue
            self.test.assertEqual(message["type"], "request", message)
            with self.lock:
                self.requests.append(message)
            command, args = message["command"], message["args"]
            ok, result, error = True, {}, None
            if command == "ships":
                result = {"ships": SHIPS, "recipient": "liaison"}
            elif command == "status":
                result = {"ships": [{"name": "ops", "dispatcher": True, "working": True,
                                     "open_tasks": 2, "waiting_on_captain": 1}]}
            elif command == "waiting":
                result = {"waiting": self.waiting, "hands_on": []}
            elif command == "send":
                if self.send_error:
                    ok, error = False, self.send_error
                else:
                    with self.lock:
                        self.sends.append(args)
                    result = {"ship": args.get("to") or "liaison", "mail": {"id": args.get("id")}}
            else:
                ok, error = False, f"unknown door command {command}"
            response = {"type": "response", "id": message["id"], "ok": ok}
            if ok:
                response["result"] = result
            else:
                response["error"] = error
            self.write(response)

    def mail(self, ref, message, ship="ops"):
        """Like the runtime, every mail carries when it was written: now,
        unless the test says otherwise."""
        message = dict(message)
        message.setdefault("created_ms", now_ms())
        self.write({"type": "mail", "ref": ref, "ship": ship, "mail": message})

    # page side
    def http(self, method, path, body=None, token=None, host=None, origin=None,
             content_type="application/json", extra=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=15)
        headers = {"Host": host or f"127.0.0.1:{self.port}"}
        if origin:
            headers["Origin"] = origin
        if token:
            headers["X-Privateer-Token"] = token
        headers.update(extra or {})
        data = None
        if body is not None:
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
            headers["Content-Type"] = content_type
        conn.request(method, path, body=data, headers=headers)
        response = conn.getresponse()
        raw = response.read()
        conn.close()
        try:
            parsed = json.loads(raw)
        except ValueError:
            parsed = raw.decode(errors="replace")
        return response.status, parsed, dict(response.getheaders())

    def code(self):
        with open(os.path.join(self.state, "pairing-code")) as f:
            return f.read().strip()

    def pair(self):
        status, body, _ = self.http("POST", "/api/pair", {"code": self.code()},
                                    origin=f"http://127.0.0.1:{self.port}")
        self.test.assertEqual(status, 200, body)
        self.token = body["token"]
        return self.token

    def api(self, path, body=None):
        return self.http("POST" if body is not None else "GET", path, body, token=self.token,
                         origin=f"http://localhost:{self.port}")

    def live(self):
        if not self.token:
            self.pair()
        status, body, _ = self.api("/api/session/start", {"offer": OFFER})
        self.test.assertEqual(status, 200, body)
        status, body, _ = self.api("/api/session/connected", {})
        self.test.assertEqual(status, 200, body)
        return body

    # fake app-server side
    def inject(self, *messages):
        self.injected += 1
        path = os.path.join(self.ctl, "inject", f"{self.injected:04d}.json")
        with open(path + ".tmp", "w") as f:
            json.dump(list(messages), f)
        os.replace(path + ".tmp", path)

    def received(self, method=None):
        try:
            with open(os.path.join(self.ctl, "received.jsonl")) as f:
                lines = [json.loads(line) for line in f]
        except OSError:
            return []
        return [m for m in lines if method is None or m.get("method") == method]

    def started(self):
        try:
            with open(os.path.join(self.ctl, "started.jsonl")) as f:
                return [json.loads(line) for line in f]
        except OSError:
            return []

    def spoken(self):
        return [m["params"]["text"] for m in self.received("thread/realtime/appendText")]

    def wait_spoken(self, words, timeout=10.0):
        return wait_for(f"the door to say {words!r}",
                        lambda: [t for t in self.spoken() if words in t], timeout)

    def say(self, text, thread="thr-1"):
        self.inject({"method": "thread/realtime/transcript/done",
                     "params": {"threadId": thread, "role": "user", "text": text}})

    def tool(self, ident, tool, arguments, turn="turn-1", thread="thr-1"):
        self.inject({"id": ident, "method": "item/tool/call",
                     "params": {"threadId": thread, "turnId": turn, "callId": f"c{ident}",
                                "tool": tool, "arguments": arguments}})
        return wait_for(f"the tool result {ident}", lambda: [
            m for m in self.received() if m.get("id") == ident and "method" not in m])[0]

    def commands(self):
        with self.lock:
            return [r["command"] for r in self.requests]

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc.stdout.close()
        self.stderr.close()

    def log(self):
        with open(os.path.join(self.tmp, "door.log")) as f:
            return f.read()


def group_gone(pgid) -> bool:
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return False


class DoorTest(unittest.TestCase):
    def door(self, **kwargs) -> Door:
        door = Door(self, **kwargs)
        self.addCleanup(lambda: shutil.rmtree(door.tmp, ignore_errors=True))
        self.addCleanup(door.close)
        return door


class PairingTest(DoorTest):
    def test_the_pairing_code_is_private_and_used_once(self):
        d = self.door()
        path = os.path.join(d.state, "pairing-code")
        self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o600)
        code = d.code()
        self.assertGreaterEqual(len(code), 10)
        self.assertEqual(d.http("GET", "/api/state")[0], 401)
        self.assertEqual(d.http("POST", "/api/session/start", {"offer": OFFER})[0], 401)
        status, body, _ = d.http("POST", "/api/pair", {"code": code.lower()})
        self.assertEqual(status, 200, body)
        token = body["token"]
        self.assertGreaterEqual(len(token), 32)
        # Used once: the file is emptied and the same code no longer pairs.
        self.assertEqual(d.code(), "")
        self.assertEqual(d.http("POST", "/api/pair", {"code": code})[0], 403)
        self.assertEqual(d.http("GET", "/api/state", token="x" * 43)[0], 401)
        status, state, _ = d.http("GET", "/api/state", token=token)
        self.assertEqual((status, state["phase"]), (200, "idle"))
        token_file = os.path.join(d.state, "session-token.json")
        self.assertEqual(stat.S_IMODE(os.stat(token_file).st_mode), 0o600)

    def test_wrong_codes_are_rate_limited_and_the_code_replaced(self):
        d = self.door()
        code = d.code()
        for _ in range(5):
            self.assertEqual(d.http("POST", "/api/pair", {"code": "WRONGWRONG"})[0], 403)
        # Locked for the window, even with the right code; and the code changed.
        self.assertEqual(d.http("POST", "/api/pair", {"code": code})[0], 429)
        self.assertNotEqual(d.code(), code)
        self.assertEqual(d.http("POST", "/api/pair", {"code": d.code()})[0], 429)

    def test_host_origin_and_content_type_are_checked(self):
        d = self.door()
        token = d.pair()
        status, page, headers = d.http("GET", "/")
        self.assertEqual(status, 200)
        self.assertIn("Privateer live voice", page)
        self.assertIn("default-src 'none'", headers["Content-Security-Policy"])
        self.assertEqual(d.http("GET", "/", host=f"localhost:{d.port}")[0], 200)
        # DNS rebinding: any other host name, or another port, is refused.
        self.assertEqual(d.http("GET", "/", host=f"evil.example:{d.port}")[0], 403)
        self.assertEqual(d.http("GET", "/api/state", token=token,
                                host=f"127.0.0.1:{d.port + 1}")[0], 403)
        self.assertEqual(d.http("GET", "/api/state", token=token,
                                host="127.0.0.1")[0], 403)
        # Another site's page, even holding the token.
        self.assertEqual(d.http("POST", "/api/session/stop", {}, token=token,
                                origin="http://evil.example")[0], 403)
        self.assertEqual(d.http("POST", "/api/session/stop", {}, token=token,
                                origin=f"http://127.0.0.1:{d.port + 1}")[0], 403)
        self.assertEqual(d.http("POST", "/api/session/stop", {}, token=token,
                                extra={"Sec-Fetch-Site": "cross-site"})[0], 403)
        self.assertEqual(d.http("POST", "/api/pair", b"code=X", origin=None,
                                content_type="application/x-www-form-urlencoded")[0], 415)
        status, _, headers = d.http("OPTIONS", "/api/session/start",
                                    origin="http://evil.example")
        self.assertIn(status, (403, 405))
        self.assertFalse([h for h in headers if h.lower().startswith("access-control")])
        self.assertEqual(d.http("POST", "/api/session/stop", {}, token=token,
                                origin=f"http://127.0.0.1:{d.port}")[0], 200)

    def test_the_server_listens_on_loopback_only(self):
        d = self.door()
        with open(os.path.join(d.state, "url")) as f:
            self.assertEqual(f.read().strip(), f"http://127.0.0.1:{d.port}/")
        try:
            addresses = [a for a in socket.getaddrinfo(socket.gethostname(), None,
                                                       socket.AF_INET)
                         if not a[4][0].startswith("127.")]
        except socket.gaierror:
            addresses = []
        for address in addresses[:1]:
            with socket.socket() as s:
                s.settimeout(1)
                self.assertNotEqual(s.connect_ex((address[4][0], d.port)), 0)


class SessionTest(DoorTest):
    def test_a_session_runs_the_webrtc_sequence_and_stops(self):
        d = self.door()
        d.pair()
        status, body, _ = d.api("/api/session/start", {"offer": OFFER})
        self.assertEqual(status, 200, body)
        self.assertTrue(body["answer"].startswith("v=0"))
        self.assertEqual(list(body), ["answer"])
        methods = [m.get("method") for m in d.received() if m.get("method")]
        self.assertEqual(methods[:6], ["initialize", "initialized", "account/read",
                                       "config/read", "thread/start", "thread/realtime/start"])
        init = d.received("initialize")[0]["params"]
        self.assertTrue(init["capabilities"]["experimentalApi"])
        thread = d.received("thread/start")[0]["params"]
        self.assertEqual((thread["ephemeral"], thread["sandbox"], thread["approvalPolicy"]),
                         (True, "read-only", "untrusted"))
        self.assertEqual(thread["environments"], [])
        # The captain's own MCP servers, plugins, apps and tools are off, by name.
        config = thread["config"]
        self.assertEqual(config["mcp_servers"], {"captain-tools": {"enabled": False}})
        self.assertEqual(config["plugins"], {"captain-plugin@market": {"enabled": False}})
        self.assertEqual(config["apps"], {"_default": {"enabled": False}})
        self.assertEqual(config["web_search"], "disabled")
        for feature in ("shell_tool", "unified_exec", "plugins", "apps", "hooks",
                        "browser_use", "computer_use", "multi_agent"):
            self.assertIs(config["features"][feature], False, feature)
        self.assertNotIn("realtime_conversation", config["features"])
        self.assertEqual(sorted(t["name"] for t in thread["dynamicTools"]),
                         ["privateer_answer", "privateer_send", "privateer_ships",
                          "privateer_status", "privateer_waiting"])
        # No tool takes the message text: what is sent is never the model's.
        for spec in thread["dynamicTools"]:
            self.assertFalse({"text", "message", "body"} & set(spec["inputSchema"]["properties"]))
        realtime = d.received("thread/realtime/start")[0]["params"]
        self.assertEqual(realtime["transport"], {"type": "webrtc", "sdp": OFFER})
        self.assertEqual((realtime["model"], realtime["version"], realtime["outputModality"]),
                         ("gpt-live-1-codex", "v3", "audio"))
        self.assertIs(realtime["clientManagedHandoffs"], True)
        self.assertIs(realtime["includeStartupContext"], False)
        self.assertNotIn("prompt", realtime)
        started = d.started()
        self.assertEqual(len(started), 1)
        self.assertFalse(started[0]["api_key"], "an API key variable reached codex")
        self.assertNotEqual(started[0]["pgid"], os.getpgid(0))
        # One session at a time.
        self.assertEqual(d.api("/api/session/start", {"offer": OFFER})[0], 409)
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        self.assertEqual(len(d.received("thread/realtime/stop")), 1)
        wait_for("the app-server group to end", lambda: group_gone(started[0]["pgid"]))
        self.assertFalse(os.path.exists(os.path.join(d.state, "app-server.pid")))
        self.assertEqual(d.api("/api/state")[1]["phase"], "idle")

    def test_an_api_key_login_is_refused_before_realtime(self):
        d = self.door(scenario={"account": "apiKey"})
        d.pair()
        status, body, _ = d.api("/api/session/start", {"offer": OFFER})
        self.assertEqual(status, 502)
        self.assertIn("not a ChatGPT login", body["error"])
        self.assertEqual(d.received("thread/start"), [])
        self.assertEqual(d.received("thread/realtime/start"), [])
        d2 = self.door(scenario={"account": None})
        d2.pair()
        self.assertEqual(d2.api("/api/session/start", {"offer": OFFER})[0], 502)
        self.assertEqual(d2.received("thread/realtime/start"), [])

    def test_an_unreadable_codex_config_stops_before_thread_start(self):
        d = self.door(scenario={"config_read": "refuse"})
        d.pair()
        status, body, _ = d.api("/api/session/start", {"offer": OFFER})
        self.assertEqual(status, 502)
        self.assertIn("config/read refused", body["error"])
        self.assertEqual(d.received("thread/start"), [])
        self.assertEqual(d.received("thread/realtime/start"), [])

    def test_realtime_failures_end_the_attempt_and_redact(self):
        d = self.door(scenario={"realtime": "refuse"})
        d.pair()
        status, body, _ = d.api("/api/session/start", {"offer": OFFER})
        self.assertEqual(status, 502)
        self.assertIn("refused", body["error"])
        self.assertNotIn("abc.def", body["error"])
        wait_for("the group to end", lambda: group_gone(d.started()[0]["pgid"]))
        self.assertEqual(d.api("/api/state")[1]["phase"], "idle")
        d2 = self.door(scenario={"realtime": "silent"}, config={"start_timeout_secs": 1})
        d2.pair()
        status, body, _ = d2.api("/api/session/start", {"offer": OFFER})
        self.assertEqual(status, 502)
        self.assertIn("no SDP answer", body["error"])
        d3 = self.door(scenario={"realtime": "closed"})
        d3.pair()
        status, body, _ = d3.api("/api/session/start", {"offer": OFFER})
        self.assertEqual((status, body["error"]), (502, "boom"))
        # A bad offer never reaches Codex.
        status, _, _ = d3.api("/api/session/start", {"offer": "not sdp"})
        self.assertEqual(status, 502)
        self.assertEqual(len(d3.started()), 1)

    def test_an_exiting_app_server_ends_the_session(self):
        d = self.door()
        d.live()
        d.inject({"exit": True})
        state = wait_for("idle", lambda: (s := d.api("/api/state")[1])["phase"] == "idle" and s)
        self.assertTrue(state["error"])

    def test_server_requests_other_than_the_tools_are_declined(self):
        d = self.door()
        d.live()
        for ident, method in ((77, "item/commandExecution/requestApproval"),
                              (78, "account/chatgptAuthTokens/refresh"),
                              (79, "item/fileChange/requestApproval"),
                              (80, "item/tool/requestUserInput")):
            d.inject({"id": ident, "method": method, "params": {"threadId": "thr-1"}})
            reply = wait_for(method, lambda i=ident: [m for m in d.received()
                                                      if m.get("id") == i and "method" not in m])
            self.assertIn("error", reply[0])
            self.assertNotIn("result", reply[0])

    def test_a_delegated_turns_answer_is_handed_off_once(self):
        d = self.door()
        d.live()
        d.inject({"method": "item/completed", "params": {
            "threadId": "thr-1", "turnId": "t1", "completedAtMs": 1,
            "item": {"type": "agentMessage", "id": "i1", "text": "It is sunny."}}},
            {"method": "turn/completed", "params": {"threadId": "thr-1", "turn": {"id": "t1"}}})
        d.wait_spoken("[BACKEND] It is sunny.")
        # A turn that ran a door tool is not handed off again: the door spoke.
        d.tool(5, "privateer_status", {}, turn="t2")
        d.wait_spoken("ops: working")
        d.inject({"method": "item/completed", "params": {
            "threadId": "thr-1", "turnId": "t2", "completedAtMs": 1,
            "item": {"type": "agentMessage", "id": "i2", "text": "Ops is working."}}},
            {"method": "turn/completed", "params": {"threadId": "thr-1", "turn": {"id": "t2"}}})
        time.sleep(0.5)
        self.assertFalse([t for t in d.spoken() if "Ops is working" in t])


class MailTest(DoorTest):
    def test_mail_is_delivered_only_after_it_is_spoken_and_deduped(self):
        d = self.door()
        d.mail("ops:1", mail("m1", kind="notice", body="the build is green"))
        time.sleep(0.6)
        self.assertEqual(d.delivered, [], "delivered with no live session")
        d.pair()
        # Answered but not yet connected: still held.
        self.assertEqual(d.api("/api/session/start", {"offer": OFFER})[0], 200)
        time.sleep(0.4)
        self.assertEqual(d.delivered, [])
        self.assertEqual(d.api("/api/state")[1]["queued"], 1)
        self.assertEqual(d.api("/api/session/connected", {})[0], 200)
        wait_for("delivered", lambda: d.delivered == ["ops:1"])
        self.assertEqual(len(d.wait_spoken("ops says: the build is green")), 1)
        # At least once: the same mail again is acknowledged without speaking.
        d.mail("ops:7", mail("m1", kind="notice", body="the build is green"))
        wait_for("redelivery acked", lambda: d.delivered == ["ops:1", "ops:7"])
        self.assertEqual(len([t for t in d.spoken() if "build is green" in t]), 1)
        # A request reads as a question.
        d.mail("ops:8", mail("m2", body="Which region?"))
        d.wait_spoken("ops asks: Which region?")
        wait_for("delivered", lambda: "ops:8" in d.delivered)
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(d.state, "spoken.json")).st_mode),
                         0o600)

    def test_dedupe_is_per_ship_and_acks_never_pass_queued_mail(self):
        d = self.door()
        d.live()
        d.mail("ops:1", mail("m1", kind="notice", body="from ops"))
        wait_for("delivered", lambda: d.delivered == ["ops:1"])
        # Another ship's mail with the same id is its own message.
        d.mail("liaison:1", mail("m1", kind="notice", sender="liaison", body="from liaison"),
               ship="liaison")
        d.wait_spoken("liaison says: from liaison")
        wait_for("delivered", lambda: d.delivered == ["ops:1", "liaison:1"])
        d.api("/api/session/stop", {})
        # Queued with no session: ops:2 waits, so the already spoken ops:3
        # must wait too (Privateer would move ops's cursor past ops:2).
        d.mail("ops:2", mail("m2", kind="notice", body="second"))
        d.mail("ops:3", mail("m1", kind="notice", body="from ops"))
        # Another ship is not held up by ops.
        d.mail("liaison:2", mail("m1", kind="notice", sender="liaison", body="from liaison"),
               ship="liaison")
        wait_for("liaison acked", lambda: "liaison:2" in d.delivered)
        time.sleep(0.5)
        self.assertEqual(d.delivered, ["ops:1", "liaison:1", "liaison:2"])
        d.live()
        wait_for("in order", lambda: d.delivered[3:] == ["ops:2", "ops:3"])
        self.assertEqual(len([t for t in d.spoken() if "from ops" in t]), 1)
        self.assertEqual(len([t for t in d.spoken() if "from liaison" in t]), 1)

    def test_mail_waits_while_no_session_is_live_and_survives_a_restart(self):
        d = self.door()
        d.live()
        d.api("/api/session/stop", {})
        later = mail("m3", kind="notice", body="later", created_ms=now_ms())
        d.mail("ops:2", later)
        time.sleep(0.6)
        self.assertEqual(d.delivered, [])
        state_dir = d.state
        d.close()
        # The restarted door is sent the same mail again; still unheard, still
        # held. Written before this door started, it is now backlog: counted
        # in one line, never read out.
        d2 = self.door(state=state_dir)
        d2.mail("ops:2", later)
        time.sleep(0.6)
        self.assertEqual(d2.delivered, [])
        d2.live()
        wait_for("delivered", lambda: d2.delivered == ["ops:2"])
        self.assertEqual(d2.spoken(), [d2.prefix + COUNT_ONE])
        d2.close()
        # Counted before this restart: acknowledged at once, never again.
        d3 = self.door(state=state_dir)
        d3.mail("ops:2", later)
        wait_for("delivered", lambda: d3.delivered == ["ops:2"])
        self.assertEqual(d3.started(), [])

    def test_approving_mail_is_spoken_as_needing_the_phone(self):
        d = self.door()
        d.live()
        d.mail("ops:3", mail("m4", body="Release 1.2?", payload={
            "approves": {"action": "publish release 1.2"}}))
        d.wait_spoken("ops: Release 1.2? This needs your approval on your phone.")
        d.mail("ops:4", mail("m5", kind="decision", body="Ship it?", payload={"decision": {
            "question": "Ship it?", "options": [
                {"id": "a", "label": "Ship", "what_happens": "ships", "approves": True},
                {"id": "b", "label": "Wait", "what_happens": "waits"}]}}))
        d.wait_spoken("ops: Ship it? This needs your approval on your phone.")
        wait_for("delivered", lambda: d.delivered == ["ops:3", "ops:4"])


HOUR_AGO = 3600 * 1000


def per_ship_in_order(test, refs):
    """Acknowledged in order within each ship's home."""
    seqs: dict = {}
    for ref in refs:
        ship, seq = ref.rsplit(":", 1)
        seqs.setdefault(ship, []).append(int(seq))
    for ship, got in seqs.items():
        test.assertEqual(got, sorted(got), ship)


class BacklogTest(DoorTest):
    """A voice door never reads a backlog aloud."""

    def old_mail(self, d, count, ships, start=0):
        """`count` notices written an hour ago, spread over `ships`; seqs
        jump, as a home numbers every mail in it, not only the captain's."""
        refs = []
        for n in range(start, start + count):
            ship = ships[n % len(ships)]
            ref = f"{ship}:{100 + n * 7}"
            d.mail(ref, mail(f"old{n}", kind="notice", sender=ship,
                             body=f"Old report {n} about the nightly build and the cache",
                             created_ms=now_ms() - HOUR_AGO), ship=ship)
            refs.append(ref)
        return refs

    def quiet(self, d, seconds=1.5):
        before = list(d.spoken())
        time.sleep(seconds)
        self.assertEqual(d.spoken(), before)

    def test_a_fresh_door_speaks_one_count_line_for_150_old_mails(self):
        d = self.door()
        ships = [f"ship{n}" for n in range(10)]
        # Most of it is queued before the page connects, the rest streams in
        # a page at a time after: still one line.
        refs = self.old_mail(d, 100, ships)
        d.live()
        for start in (100, 120, 140):
            time.sleep(0.5)
            refs += self.old_mail(d, 20 if start < 140 else 10, ships, start)
        line = d.wait_spoken("older messages")[0]
        self.assertEqual(line, d.prefix + "You have 150 older messages from 10 ships. "
                         "Say summary to hear who they are from, or skip.")
        wait_for("all acked", lambda: len(d.delivered) == 150)
        self.assertEqual(sorted(d.delivered), sorted(refs))
        per_ship_in_order(self, d.delivered)
        self.quiet(d)
        self.assertEqual(d.spoken(), [line])
        # The captain asks: one line a ship, at most eight.
        d.say("Summary please")
        said = wait_for("the summary", lambda: d.spoken()[1:])
        self.assertEqual(len(said), 1)
        lines = said[0].removeprefix(d.prefix).split("\n")
        self.assertEqual(len(lines), pacing.SUMMARY_LINES)
        self.assertEqual(lines[0], "ship0: 15 messages, the newest about "
                         "Old report 140 about the nightly.")
        self.assertEqual(lines[-1], "And 3 other ships: 45 messages.")
        self.quiet(d)
        self.assertEqual(len(d.spoken()), 2)
        # Bounded state: 150 handled mails leave a small file, no heard list.
        self.assertLess(os.path.getsize(os.path.join(d.state, "spoken.json")), 16 * 1024)
        self.assertFalse(os.path.exists(os.path.join(d.state, "heard.json")))

    def test_skip_or_silence_hears_nothing_more(self):
        d = self.door()
        refs = self.old_mail(d, 5, ["ops"])
        d.live()
        d.wait_spoken("You have 5 older messages from 1 ship.")
        wait_for("acked", lambda: d.delivered == refs)
        self.quiet(d)
        d.say("skip")
        self.quiet(d)
        # The offer ended with that answer: a later "summary" hears nothing.
        d.say("summary")
        self.quiet(d)
        self.assertEqual(len(d.spoken()), 1)

    def test_backlog_is_counted_before_live_mail_and_both_are_acked_in_order(self):
        d = self.door()
        d.mail("ops:5", mail("o1", kind="notice", body="old one",
                             created_ms=now_ms() - HOUR_AGO))
        d.mail("ops:9", mail("n1", kind="notice", body="fresh news"))
        d.mail("liaison:3", mail("n2", kind="notice", sender="liaison", body="also fresh"),
               ship="liaison")
        d.live()
        wait_for("both", lambda: len(d.spoken()) == 3)
        self.assertEqual(d.spoken()[0], d.prefix + COUNT_ONE)
        self.assertEqual(sorted(d.spoken()[1:]), sorted([d.prefix + "ops says: fresh news",
                                                         d.prefix + "liaison says: also fresh"]))
        wait_for("acked", lambda: len(d.delivered) == 3)
        self.assertLess(d.delivered.index("ops:5"), d.delivered.index("ops:9"))

    def test_mail_older_than_the_session_and_the_age_limit_is_backlog(self):
        d = self.door(config={"backlog_age_secs": 1})
        # Written after the door started, but it waited longer than the age
        # limit for a session: counted, not read.
        d.mail("ops:1", mail("m1", kind="notice", body="waited too long"))
        time.sleep(1.5)
        d.live()
        d.wait_spoken("You have 1 older message from 1 ship.")
        wait_for("acked", lambda: d.delivered == ["ops:1"])
        self.quiet(d)
        self.assertFalse([t for t in d.spoken() if "waited too long" in t])

    def test_live_mail_goes_in_three_at_a_time(self):
        d = self.door(config={"pace_secs": 30})
        d.live()
        for n in range(1, 8):
            d.mail(f"ops:{n}", mail(f"m{n}", kind="notice", body=f"news {n}"))
        wait_for("a batch", lambda: len(d.delivered) == 3)
        self.quiet(d, 1.0)
        self.assertEqual(d.spoken(), [d.prefix + f"ops says: news {n}" for n in (1, 2, 3)])
        self.assertEqual(d.delivered, ["ops:1", "ops:2", "ops:3"])
        # The voice finished speaking: the rest is offered, not read.
        d.inject({"method": "thread/realtime/transcript/done",
                  "params": {"threadId": "thr-1", "role": "assistant", "text": "done"}})
        d.wait_spoken("And 4 more; say more to hear them.")
        self.quiet(d, 1.0)
        self.assertEqual(len(d.delivered), 3)
        d.say("more")
        wait_for("the next batch", lambda: len(d.delivered) == 6)
        self.assertEqual(d.delivered[3:], ["ops:4", "ops:5", "ops:6"])
        d.wait_spoken("ops says: news 6")
        # Without the voice finishing, the offer waits for pace_secs.
        self.quiet(d, 1.0)
        d.inject({"method": "thread/realtime/transcript/done",
                  "params": {"threadId": "thr-1", "role": "assistant", "text": "done"}})
        d.wait_spoken("And 1 more; say more to hear them.")
        d.say("Say more")
        wait_for("the last", lambda: d.delivered[-1] == "ops:7")
        notices = [t for t in d.spoken() if "says:" in t]
        self.assertEqual(len(notices), 7)

    def test_a_batch_ends_after_pace_secs_without_the_voice(self):
        d = self.door(config={"pace_secs": 1, "max_notices_per_turn": 2})
        d.live()
        for n in range(1, 5):
            d.mail(f"ops:{n}", mail(f"m{n}", kind="notice", body=f"news {n}"))
        d.wait_spoken("And 2 more; say more to hear them.")
        self.assertEqual(d.delivered, ["ops:1", "ops:2"])
        # Once the held mail is offered, a quiet spell later new mail joins it;
        # the offer is repeated with the new count, never read out unasked.
        d.mail("ops:5", mail("m5", kind="notice", body="news 5"))
        d.wait_spoken("And 3 more; say more to hear them.")
        self.assertEqual(len([t for t in d.spoken() if "says:" in t]), 2)

    def test_a_long_mail_gets_the_short_form(self):
        d = self.door()
        d.live()
        long_body = "Deploy notes for the storage migration " + "and more detail " * 60
        d.mail("ops:1", mail("m1", kind="notice", body=long_body))
        d.wait_spoken("ops sent a long message about Deploy notes for the storage migration; "
                      "it is on your phone.")
        d.mail("ops:2", mail("m2", body="Publish? " + "context " * 80,
                             payload={"approves": {"action": "publish"}}))
        d.wait_spoken("ops sent a long message about Publish? context context context "
                      "context context; it needs your approval on your phone.")
        wait_for("acked", lambda: d.delivered == ["ops:1", "ops:2"])
        for text in d.spoken():
            self.assertLessEqual(len(text.removeprefix(d.prefix)), pacing.NOTICE_SPOKEN_LIMIT)

    def test_a_large_gap_is_backlog(self):
        d = self.door(config={"gap_limit": 5})
        # Six fresh messages from one home queued with no session: far
        # behind, so counted. Another home's single message is still read.
        for n in range(1, 7):
            d.mail(f"ops:{n * 40}", mail(f"m{n}", kind="notice", body=f"burst {n}"))
        d.mail("liaison:2", mail("l1", kind="notice", sender="liaison", body="just one"),
               ship="liaison")
        d.live()
        d.wait_spoken("You have 6 older messages from 1 ship.")
        d.wait_spoken("liaison says: just one")
        wait_for("acked", lambda: len(d.delivered) == 7)
        per_ship_in_order(self, d.delivered)
        self.quiet(d)
        self.assertFalse([t for t in d.spoken() if "burst" in t])
        # Below the limit, fresh mail is read.
        d.mail("ops:400", mail("m9", kind="notice", body="after the gap"))
        d.wait_spoken("ops says: after the gap")

    def test_a_restart_neither_respeaks_nor_recounts(self):
        d = self.door()
        old = [mail(f"o{n}", kind="notice", body=f"old {n}", created_ms=now_ms() - HOUR_AGO)
               for n in range(3)]
        for n, message in enumerate(old):
            d.mail(f"ops:{n + 1}", message)
        d.live()
        d.wait_spoken("You have 3 older messages from 1 ship.")
        fresh = mail("f1", body="Which region?", created_ms=now_ms())
        d.mail("ops:9", fresh)
        d.wait_spoken("ops asks: Which region?")
        wait_for("acked", lambda: len(d.delivered) == 4)
        state_dir = d.state
        d.close()
        # As if the acknowledgements were lost: the runtime sends it all again.
        d2 = self.door(state=state_dir)
        for n, message in enumerate(old):
            d2.mail(f"ops:{n + 1}", message)
        d2.mail("ops:9", fresh)
        wait_for("acked again", lambda: d2.delivered == ["ops:1", "ops:2", "ops:3", "ops:9"])
        d2.live()
        self.quiet(d2, 3.5)
        self.assertEqual(d2.spoken(), [])
        # The question is still the one "answer that" finds.
        d2.waiting = [{"ship": "ops", "mail": fresh}]
        d2.say("Use the west region")
        result = d2.tool(5, "privateer_answer", {})
        self.assertTrue(result["result"]["success"], result)


class PacingTest(unittest.TestCase):
    def event(self, ship, seq, **fields):
        return {"type": "mail", "ref": f"{ship}:{seq}", "ship": ship,
                "mail": {"id": f"{ship}-{seq}", "from": ship, "kind": "notice",
                         "body": "hello", **fields}}

    def test_what_counts_as_old(self):
        door_ms, now = 1_000_000, 2_000_000
        e = lambda created: self.event("ops", 1, created_ms=created)  # noqa: E731
        self.assertTrue(pacing.old(e(door_ms - 1), door_ms, None, 600_000, now))
        self.assertFalse(pacing.old(e(now - 1000), door_ms, None, 600_000, now))
        # With no session, older than the age limit already.
        self.assertTrue(pacing.old(e(door_ms + 1), door_ms, None, 600_000, now))
        # Before the session and older than the age limit.
        self.assertTrue(pacing.old(e(door_ms + 1), door_ms, now - 1, 600_000, now + 600_000))
        self.assertFalse(pacing.old(e(now - 5), door_ms, now - 1, 600_000, now))
        # After the session started: live, however long it waited.
        self.assertFalse(pacing.old(e(now), door_ms, now - 1, 1, now + 10**9))
        self.assertFalse(pacing.old(self.event("ops", 1), door_ms, None, 1, now))

    def test_lines(self):
        events = [self.event("ops", n) for n in range(3)] + [self.event("liaison", 9)]
        self.assertEqual(pacing.count_line(events),
                         "You have 4 older messages from 2 ships. "
                         "Say summary to hear who they are from, or skip.")
        self.assertIn("4 more older messages", pacing.count_line(events, again=True))
        many = [self.event(f"s{n:02d}", k) for n in range(20) for k in range(n + 1)]
        lines = pacing.summary_lines(many)
        self.assertEqual(len(lines), pacing.SUMMARY_LINES)
        self.assertTrue(lines[0].startswith("s19: 20 messages"))
        self.assertEqual(lines[-1], "And 13 other ships: 91 messages.")
        self.assertEqual(pacing.subject(self.event("ops", 1, body="", payload={
            "report": {"title": "Nightly build: green on all targets today again"}})),
            "Nightly build: green on all targets")
        self.assertEqual(pacing.more_line(4), "And 4 more; say more to hear them.")
        long = self.event("x" * 200, 1, body="word " * 500)
        self.assertLessEqual(len(pacing.spoken_notice(long, "x" * 1000, True)),
                             pacing.NOTICE_SPOKEN_LIMIT)

    def test_keywords_are_short_utterances_of_their_own(self):
        for said in ("summary", "Summary please.", "give me the summary", "summarize"):
            self.assertTrue(pacing.keyword(said, pacing.SUMMARY_WORDS), said)
        for said in ("no summary", "don't summarize", "",
                     "tell ops the summary of the meeting is ready"):
            self.assertFalse(pacing.keyword(said, pacing.SUMMARY_WORDS), said)
        self.assertTrue(pacing.keyword("say more", pacing.MORE_WORDS))
        self.assertFalse(pacing.keyword("no more", pacing.MORE_WORDS))

    def test_state_files_stay_bounded(self):
        tmp = tempfile.mkdtemp(prefix="live-voice-state-")
        self.addCleanup(lambda: shutil.rmtree(tmp, ignore_errors=True))
        # The first formats: a long list of keys, and whole mail per question.
        keys = [door_module.mail_key("ops", f"k{n}") for n in range(5000)]
        with open(os.path.join(tmp, "spoken.json"), "w") as f:
            json.dump(keys, f)
        with open(os.path.join(tmp, "heard.json"), "w") as f:
            json.dump({k: {"ship": "ops", "mail": {"body": "x" * 800}} for k in keys[:200]}, f)
        saved = door_module.STATE_DIR
        door_module.STATE_DIR = tmp
        self.addCleanup(lambda: setattr(door_module, "STATE_DIR", saved))

        class Runtime:
            def __init__(self):
                self.refs = []

            def delivered(self, ref):
                self.refs.append(ref)

        runtime = Runtime()
        d = door_module.Door({"recipient": "ops"}, {}, runtime, lambda _: None)
        self.assertEqual(len(d.handled), door_module.HANDLED_ACKED_KEPT)
        self.assertEqual(len(d.heard), door_module.HEARD_LIMIT)
        d.speak = lambda text: True
        for n in range(3000):
            ship = f"ship{n % 40}"
            event = self.event(ship, n, kind="request", body="Which region? " * 30)
            d.batch_left = 1
            self.assertTrue(d.speak_notice(event))
            d.runtime.delivered(event["ref"])
            d.advance(event)
        d.save_handled()
        self.assertEqual(len(runtime.refs), 3000)
        self.assertLessEqual(len(d.handled), door_module.HANDLED_ACKED_KEPT)
        self.assertLessEqual(len(d.heard), door_module.HEARD_LIMIT)
        self.assertLess(os.path.getsize(os.path.join(tmp, "spoken.json")), 16 * 1024)
        self.assertLess(os.path.getsize(os.path.join(tmp, "heard.json")), 4 * 1024)
        # What is kept still dedupes: by key, and by each ship's acked seq.
        self.assertTrue(d.is_handled(self.event("ship39", 2999)))
        self.assertTrue(d.is_handled(self.event("ship0", 0)))
        self.assertFalse(d.is_handled(self.event("ship0", 3001)))
        # Unacknowledged keys are never trimmed away.
        pending = [self.event("held", 10_000 + n) for n in range(300)]
        d.mark_handled(pending)
        self.assertTrue(all(d.is_handled(e) for e in pending))
        reloaded = door_module.load_handled()[1]
        self.assertTrue(all(pacing.event_key(e) in reloaded for e in pending))


class ExactWordsTest(DoorTest):
    WORDS = "Tell ops the deploy window moves to Friday at nine"

    def test_the_captains_exact_words_are_read_back_and_sent_on_send(self):
        d = self.door()
        d.live()
        d.say(self.WORDS)
        result = d.tool(11, "privateer_send", {"to": "Ops", "message": "deploy is Friday"})
        self.assertTrue(result["result"]["success"], result)
        readback = f"I will send ops: {self.WORDS}. Say send to ops to confirm."
        d.wait_spoken(readback)
        # The page shows the door's own read-back, recipient included.
        self.assertEqual(d.api("/api/state")[1]["draft"],
                         {"to": "ops", "text": self.WORDS, "readback": readback})
        time.sleep(0.4)
        self.assertEqual(d.sends, [], "sent before the captain confirmed")
        # A confirmation must name the recipient; a bare one keeps the draft.
        d.say("Send it.")
        d.wait_spoken("Say send to ops to send it.")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])
        self.assertIsNotNone(d.api("/api/state")[1]["draft"])
        d.say("Send it to Ops.")
        wait_for("the send", lambda: d.sends)
        sent = d.sends[0]
        self.assertEqual((sent["to"], sent["kind"], sent["body"]), ("ops", "request", self.WORDS))
        self.assertNotIn("payload", sent)
        self.assertTrue(sent["id"].startswith("voice-"))
        d.wait_spoken("Sent to ops.")
        # A second "send" has nothing to confirm.
        d.say("send to ops")
        time.sleep(0.5)
        self.assertEqual(len(d.sends), 1)

    def test_a_confirmation_naming_another_ship_sends_nothing(self):
        # A steered voice could read the draft back as going elsewhere; the
        # captain's confirmation names the ship they heard, which then differs.
        d = self.door()
        d.live()
        d.say(self.WORDS)
        d.tool(15, "privateer_send", {"to": "ops"})
        d.wait_spoken("Say send to ops to confirm.")
        d.say("send to liaison")
        d.wait_spoken("That message is for ops, not the ship you named; nothing was sent.")
        self.assertIsNone(d.api("/api/state")[1]["draft"])
        d.say("send to ops")
        time.sleep(0.5)
        self.assertEqual(d.sends, [])

    def test_the_default_recipient_and_unknown_ships(self):
        d = self.door()
        d.live()
        d.say("Remind me about the audit")
        d.tool(12, "privateer_send", "{}")
        d.wait_spoken("I will send liaison: Remind me about the audit.")
        result = d.tool(13, "privateer_send", {"to": "nowhere"})
        self.assertFalse(result["result"]["success"])
        self.assertIn("no ship named nowhere", result["result"]["contentItems"][0]["text"])
        d2 = self.door()
        d2.live()
        result = d2.tool(14, "privateer_send", {})
        self.assertFalse(result["result"]["success"])
        self.assertIn("not heard your words", result["result"]["contentItems"][0]["text"])

    def test_anything_but_a_confirmation_drops_the_draft(self):
        d = self.door()
        d.live()
        d.say(self.WORDS)
        d.tool(21, "privateer_send", {"to": "ops"})
        readback = d.wait_spoken("Say send to ops to confirm")[0]
        # The door's own words heard back are not the captain.
        d.say(readback)
        d.say(readback.split(": ", 1)[1])
        time.sleep(0.4)
        self.assertIsNotNone(d.api("/api/state")[1]["draft"])
        d.say("Actually, let me think. Send it later")
        d.wait_spoken("The message to ops was not sent.")
        d.say("send to ops")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])
        d.say(self.WORDS)
        d.tool(22, "privateer_send", {"to": "ops"})
        d.say("cancel")
        d.wait_spoken("Not sent to ops.")
        d.say("send to ops")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])

    def test_an_unconfirmed_draft_expires(self):
        d = self.door(config={"draft_ttl_secs": 1})
        d.live()
        d.say(self.WORDS)
        d.tool(31, "privateer_send", {"to": "ops"})
        d.wait_spoken("expired; nothing was sent")
        d.say("send to ops")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])

    def test_a_failed_send_keeps_its_id_so_send_again_is_the_same_message(self):
        d = self.door()
        d.live()
        d.send_error = "home is busy"
        d.say(self.WORDS)
        d.tool(41, "privateer_send", {"to": "ops"})
        d.say("send to ops")
        d.wait_spoken("Not sent: home is busy. Say send to ops to try again.")
        d.send_error = None
        d.say("send to ops")
        wait_for("the send", lambda: d.sends)
        failed = [r for r in d.requests if r["command"] == "send"]
        self.assertEqual(len(failed), 2)
        self.assertEqual(failed[0]["args"]["id"], failed[1]["args"]["id"])

    def test_stop_and_restart_never_send_twice_or_confirm_an_old_draft(self):
        d = self.door()
        d.live()
        d.say(self.WORDS)
        d.tool(51, "privateer_send", {"to": "ops"})
        d.wait_spoken("Say send to ops to confirm")
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        d.live()
        d.say("send to ops", thread="thr-2")
        time.sleep(0.5)
        self.assertEqual(d.sends, [], "a new session confirmed an old draft")
        d.say(self.WORDS, thread="thr-2")
        d.tool(52, "privateer_send", {"to": "ops"}, thread="thr-2")
        d.say("send to ops", thread="thr-2")
        wait_for("the send", lambda: d.sends)
        # A page reload starts again: nothing is replayed.
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        d.live()
        d.say("send to ops", thread="thr-3")
        time.sleep(0.5)
        state_dir = d.state
        d.close()
        d2 = self.door(state=state_dir)
        d2.pair()
        time.sleep(0.5)
        self.assertEqual(len(d.sends) + len(d2.sends), 1)


class NeverApproveTest(DoorTest):
    ASKS = {
        "approves": mail("a1", body="Publish?", payload={"approves": {"action": "publish"}}),
        "approving option": mail("a2", kind="decision", body="Ship?", payload={"decision": {
            "question": "Ship?", "options": [
                {"id": "a", "label": "Ship", "what_happens": "ships", "approves": True},
                {"id": "b", "label": "Wait", "what_happens": "waits"}]}}),
        "production": mail("a3", body="Deploy to prod?", request_kind="production_approval"),
        "privileged": mail("a4", body="Run as root?", request_kind="privileged_action",
                           payload={"privileged": {"program": "/usr/bin/true"}}),
        "tool permission": mail("a5", body="Allow git push?",
                                payload={"type": "tool_permission", "block": {}}),
    }

    def test_the_door_never_answers_an_approval(self):
        d = self.door(config={"max_notices_per_turn": len(self.ASKS)})
        d.live()
        d.waiting = [{"ship": "ops", "mail": m} for m in self.ASKS.values()]
        ident = 100
        for name, ask in self.ASKS.items():
            ident += 1
            d.say("yes")
            result = d.tool(ident, "privateer_answer", {"mail_id": ask["id"]})
            self.assertFalse(result["result"]["success"], name)
            self.assertIn("approval on your phone", result["result"]["contentItems"][0]["text"],
                          name)
            self.assertIsNone(d.api("/api/state")[1]["draft"], name)
            d.say("send to ops")
        # Spoken first, then asked for "the latest question": still refused.
        for n, ask in enumerate(self.ASKS.values()):
            d.mail(f"ops:{n + 10}", ask)
        wait_for("delivered", lambda: len(d.delivered) == len(self.ASKS))
        d.say("yes")
        result = d.tool(199, "privateer_answer", {})
        self.assertFalse(result["result"]["success"])
        d.say("send to ops")
        time.sleep(0.5)
        self.assertEqual(d.sends, [])
        self.assertTrue(set(d.commands()) <= {"ships", "status", "waiting", "send"})
        self.assertNotIn("permissions", d.commands())

    def test_a_plain_question_is_answered_with_words_never_an_option(self):
        d = self.door()
        d.live()
        plain = mail("q1", body="Which region?")
        typed = mail("q2", kind="decision", body="Which plan?", payload={"decision": {
            "question": "Which plan?", "options": [
                {"id": "a", "label": "Small", "what_happens": "cheap"},
                {"id": "b", "label": "Large", "what_happens": "fast"}]}})
        d.waiting = [{"ship": "ops", "mail": plain}, {"ship": "ops", "mail": typed}]
        d.mail("ops:1", plain)
        d.wait_spoken("ops asks: Which region?")
        d.say("Use the eu west region")
        d.tool(201, "privateer_answer", {})
        d.wait_spoken("I will answer ops: Use the eu west region. Say send to ops to confirm.")
        d.say("send to ops")
        wait_for("the reply", lambda: d.sends)
        self.assertEqual(d.sends[0], {"id": d.sends[0]["id"], "to": "ops", "kind": "reply",
                                      "correlation": "q1", "body": "Use the eu west region"})
        d.say("Large please")
        d.tool(202, "privateer_answer", {"mail_id": "q2"})
        d.say("confirm send to ops")
        wait_for("the second reply", lambda: len(d.sends) == 2)
        self.assertEqual(d.sends[1]["payload"], {"answer": {"text": "Large please"}})
        self.assertEqual(d.sends[1]["correlation"], "q2")
        # No longer waiting: not answerable.
        d.waiting = []
        d.say("again")
        result = d.tool(203, "privateer_answer", {"mail_id": "q1"})
        self.assertFalse(result["result"]["success"])

    def test_waiting_and_status_are_read_from_records(self):
        d = self.door()
        d.live()
        d.waiting = [{"ship": "ops", "mail": self.ASKS["approves"]},
                     {"ship": "ops", "mail": mail("q9", body="Which region?")}]
        d.tool(301, "privateer_waiting", {})
        d.wait_spoken("2 waiting on you.")
        d.wait_spoken("ops, id a1: Publish? This needs your approval on your phone.")
        d.tool(302, "privateer_ships", {})
        d.wait_spoken("liaison, the liaison: talks to the captain.")
        d.tool(303, "privateer_status", {})
        d.wait_spoken("ops: working, 2 open tasks, 1 waiting on you.")


class RulesTest(unittest.TestCase):
    def test_every_approval_shape_is_recognised(self):
        for name, ask in NeverApproveTest.ASKS.items():
            self.assertTrue(rules.approving(ask), name)
            self.assertTrue(rules.answerable(ask), name)
        self.assertEqual(rules.approving(mail("p", body="hi")), "")
        self.assertEqual(rules.answerable(mail("p", body="hi")), "")
        self.assertTrue(rules.answerable(mail("n", kind="notice")))

    def test_check_send_refuses_anything_that_could_approve(self):
        plain = mail("p1")
        ok = {"to": "ops", "kind": "reply", "correlation": "p1", "body": "yes"}
        rules.check_send(ok, plain)
        rules.check_send({"to": "ops", "kind": "request", "body": "hi"}, None)
        refused = [
            ({**ok, "payload": {"answer": {"option_id": "a"}}}, plain),
            ({**ok, "payload": {"approves": {}}}, plain),
            ({**ok, "correlation": "a1"}, NeverApproveTest.ASKS["approves"]),
            (ok, NeverApproveTest.ASKS["approves"]),
            (ok, NeverApproveTest.ASKS["approving option"]),
            (ok, NeverApproveTest.ASKS["production"]),
            ({"to": "ops", "kind": "reply", "correlation": "x", "body": "yes"}, None),
            ({"to": "ops", "kind": "notice", "body": "x"}, None),
            ({**ok, "correlation": "other"}, plain),
        ]
        for args, target in refused:
            with self.assertRaises(rules.Refused, msg=(args, target)):
                rules.check_send(args, target)

    def test_the_runtime_connection_refuses_other_commands(self):
        import door
        sent = io.StringIO()
        runtime = door.Runtime(lambda _: None, stdin=io.StringIO(""), stdout=sent)
        for command in ("permissions", "approval", "dismiss", "away", "file.put"):
            with self.assertRaises(RuntimeError):
                runtime.call(command, {}, timeout=0.1)
        self.assertEqual(sent.getvalue(), "")

    def test_confirmation_is_a_whole_new_utterance(self):
        clock = [0.0]
        talk = rules.Conversation(10, clock=lambda: clock[0])
        with self.assertRaises(rules.Refused):
            talk.propose("ops")
        talk.heard("Tell ops to rebase, then send it")
        draft = talk.propose("ops")
        self.assertEqual(draft.text, "Tell ops to rebase, then send it")
        self.assertIs(talk.propose("ops"), draft)
        self.assertEqual(talk.heard("Send it, and also tell liaison"), ("dropped", draft))
        talk.heard("Tell ops hello")
        draft = talk.propose("ops")
        talk.door_said(draft.readback())
        self.assertEqual(talk.heard(draft.readback()), ("echo", None))
        # Naming no one confirms nothing, and the draft still waits.
        self.assertEqual(talk.heard("  SEND!  "), ("unnamed", draft))
        self.assertIs(talk.draft, draft)
        self.assertEqual(talk.heard("Send it to OPS!"), ("confirm", draft))
        self.assertIsNone(talk.draft)
        talk.heard("Tell ops hi")
        draft = talk.propose("ops")
        self.assertEqual(talk.heard("send to liaison"), ("wrong ship", draft))
        self.assertIsNone(talk.draft)
        talk.heard("Tell ops hi")
        draft = talk.propose("ops")
        self.assertEqual(talk.heard("send to ops and liaison"), ("wrong ship", draft))
        self.assertIsNone(rules.confirmed_ship("send to"))
        self.assertEqual(rules.confirmed_ship("yes send it to first mate"), "first mate")
        talk.heard("Tell ops bye")
        draft = talk.propose("ops")
        clock[0] = 11
        self.assertIs(talk.expire(), draft)
        talk.heard("x")
        draft = talk.propose("ops")
        clock[0] = 30
        self.assertEqual(talk.heard("send"), ("expired", draft))


class HeartbeatTest(DoorTest):
    def test_a_page_that_stops_checking_in_ends_its_session(self):
        d = self.door(config={"heartbeat_secs": 1})
        d.live()
        # A closed tab: no more polls. The session is stopped on Codex too.
        wait_for("realtime stopped", lambda: d.received("thread/realtime/stop"))
        wait_for("the group to end", lambda: group_gone(d.started()[0]["pgid"]))
        state = d.api("/api/state")[1]
        self.assertEqual((state["phase"], state["live"]), ("idle", False))
        self.assertIn("stopped checking in", state["error"])
        # Mail is not spoken into the dead session, nor acknowledged.
        d.mail("ops:1", mail("m1", kind="notice", body="nobody hears this yet"))
        time.sleep(0.6)
        self.assertEqual(d.delivered, [])
        self.assertFalse([t for t in d.spoken() if "nobody hears" in t])
        self.assertEqual(d.api("/api/state")[1]["queued"], 1)
        # The next page hears it.
        d.live()
        wait_for("delivered", lambda: d.delivered == ["ops:1"])
        d.wait_spoken("ops says: nobody hears this yet")

    def test_a_polling_page_keeps_its_session(self):
        d = self.door(config={"heartbeat_secs": 1})
        d.live()
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            self.assertEqual(d.api("/api/state")[1]["phase"], "live")
            time.sleep(0.25)
        self.assertEqual(d.received("thread/realtime/stop"), [])

    def test_closing_the_page_asks_for_a_token_checked_stop(self):
        page = (PLUGIN / "page" / "app.js").read_text()
        self.assertIn('addEventListener("pagehide"', page)
        self.assertIn("keepalive: true", page)
        d = self.door()
        d.live()
        # Without the token the stop is refused and the session runs on.
        self.assertEqual(d.http("POST", "/api/session/stop", {},
                                origin=f"http://127.0.0.1:{d.port}")[0], 401)
        self.assertEqual(d.received("thread/realtime/stop"), [])
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        self.assertEqual(len(d.received("thread/realtime/stop")), 1)


class StartupTest(unittest.TestCase):
    def test_the_page_server_never_asks_the_resolver(self):
        # HTTPServer's own server_bind does a reverse lookup of its host,
        # which stalls where the resolver is slow; the door's does none.
        import web

        def no_lookup(*_):
            raise AssertionError("the door asked the resolver")
        tmp = tempfile.mkdtemp(prefix="live-voice-test-")
        self.addCleanup(shutil.rmtree, tmp, True)
        saved = socket.getfqdn, socket.gethostbyaddr, socket.gethostbyname
        socket.getfqdn = socket.gethostbyaddr = socket.gethostbyname = no_lookup
        try:
            server = web.serve(0, web.Pairing(tmp), None)
        finally:
            socket.getfqdn, socket.gethostbyaddr, socket.gethostbyname = saved
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.assertEqual(server.server_address[0], "127.0.0.1")
        conn = http.client.HTTPConnection("127.0.0.1", server.server_address[1], timeout=5)
        conn.request("GET", "/api/state", headers={"Host": f"127.0.0.1:{server.server_address[1]}"})
        self.assertEqual(conn.getresponse().status, 401)
        conn.close()

    def test_a_door_that_cannot_start_says_why(self):
        with self.assertRaises(AssertionError) as caught:
            Door(self, config={"port": "not a port"})
        self.assertIn("exited with status 2", str(caught.exception))
        self.assertIn("port must be a number", str(caught.exception))

    def test_a_door_stuck_starting_shows_where(self):
        # Its config is a FIFO nobody writes: the door blocks reading it.
        tmp = tempfile.mkdtemp(prefix="live-voice-test-")
        self.addCleanup(shutil.rmtree, tmp, True)
        fifo = os.path.join(tmp, "config.toml")
        os.mkfifo(fifo)
        with self.assertRaises(AssertionError) as caught:
            Door(self, config_path=fifo, up_timeout=1.0)
        self.assertIn("still running", str(caught.exception))
        self.assertIn("load_config", str(caught.exception))

    def test_a_stale_app_server_is_reaped_only_when_it_is_one(self):
        import door
        tmp = tempfile.mkdtemp(prefix="live-voice-test-")
        self.addCleanup(shutil.rmtree, tmp, True)
        path = os.path.join(tmp, "app-server.pid")
        sleeper = [sys.executable, "-c", "import time; time.sleep(60)"]
        other = subprocess.Popen(sleeper + ["not-codex"], start_new_session=True)
        stale = subprocess.Popen(sleeper + ["app-server"], start_new_session=True)
        self.addCleanup(lambda: [p.kill() or p.wait() for p in (other, stale)])
        wait_for("ps to show it", lambda: "app-server" in door.command_line(stale.pid))
        # Another process under a recorded pid is left alone.
        with open(path, "w") as f:
            f.write(str(other.pid))
        door.reap_stale(path)
        self.assertIsNone(other.poll())
        self.assertFalse(os.path.exists(path))
        with open(path, "w") as f:
            f.write(str(stale.pid))
        door.reap_stale(path)
        self.assertEqual(stale.wait(timeout=10), -signal.SIGKILL)
        self.assertFalse(os.path.exists(path))


class ConfigTest(unittest.TestCase):
    CODEX = {"codex": [sys.executable]}

    def test_settings_are_checked(self):
        import door
        self.assertEqual(door.config_problem(self.CODEX), "")
        self.assertEqual(door.config_problem({**self.CODEX, "port": 1}), "")
        self.assertEqual(door.config_problem({**self.CODEX, "port": 65535}), "")
        for port in (0, -1, 65536, "8765", True):
            self.assertEqual(door.config_problem({**self.CODEX, "port": port}),
                             "port must be a number from 1 to 65535", port)
        # Delegated turns never get an environment; the old opt-out is refused.
        self.assertIn("isolate_codex was removed",
                      door.config_problem({**self.CODEX, "isolate_codex": False}))
        self.assertIn("heartbeat_secs", door.config_problem({**self.CODEX, "heartbeat_secs": 0}))
        for key in ("backlog_age_secs", "pace_secs"):
            self.assertIn(key, door.config_problem({**self.CODEX, key: 0}))
        for key in ("max_notices_per_turn", "gap_limit"):
            self.assertEqual(door.config_problem({**self.CODEX, key: 2}), "")
            for bad in (0, 1.5, True, "3"):
                self.assertIn(key, door.config_problem({**self.CODEX, key: bad}), bad)

    def test_lockdown_turns_off_every_named_extra(self):
        effective = {"mcp_servers": {"b": {"enabled": True}, "a": {}},
                     "plugins": {"p@m": {"enabled": True}}, "apps": {"x": {}}}
        locked = voice.lockdown(effective)
        self.assertEqual(locked["mcp_servers"], {"a": {"enabled": False}, "b": {"enabled": False}})
        self.assertEqual(locked["plugins"], {"p@m": {"enabled": False}})
        self.assertEqual(locked["apps"], {"_default": {"enabled": False}})
        self.assertTrue(all(v is False for v in locked["features"].values()))
        self.assertIn("shell_tool", locked["features"])
        self.assertNotIn("realtime_conversation", locked["features"])
        empty = voice.lockdown({"mcp_servers": None, "plugins": "x"})
        self.assertEqual((empty["mcp_servers"], empty["plugins"]), ({}, {}))


if __name__ == "__main__":
    unittest.main(verbosity=1)
