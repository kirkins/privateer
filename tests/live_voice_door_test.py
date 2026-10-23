#!/usr/bin/env python3
"""The live voice front door, offline.

Runs plugins/live-voice/door.py as a real subprocess speaking front door
protocol v1 (this test plays the Privateer runtime), its page API over
loopback HTTP, and a fake `codex app-server` (tests/fake_live_voice_app_server.py)
over real pipes. No network beyond 127.0.0.1 and no real Codex.

Covers pairing and tokens, the Host and Origin checks and pairing rate limits,
the webrtc sequence and its failures, mail delivered only after it is spoken
and deduped on redelivery, the exact-words read-back and confirmation, every
never-approve rule, delegated-turn handoffs, declined server requests, and
session stop and restart without duplicate sends.

Usage: python3 tests/live_voice_door_test.py
"""

import http.client
import io
import json
import os
import pathlib
import queue
import shutil
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
import rules  # noqa: E402

OFFER = ("v=0\r\no=- 7 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n"
         "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=sendrecv\r\n"
         "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n")
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


def mail(ident, kind="request", sender="ops", body="hello", payload=None, **extra):
    message = {"id": ident, "from": sender, "to": "captain", "kind": kind, "body": body,
               "payload": payload or {}}
    message.update(extra)
    return message


class Door:
    """door.py under test, with this test as its runtime."""

    def __init__(self, test, scenario=None, config=None, state=None):
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
                    "grace_secs": 2, **(config or {})}
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
                    "config_path": self.config, "state_dir": self.state, "recipient": "liaison"})
        wait_for("the page", lambda: os.path.exists(os.path.join(self.state, "url")))
        self.token = None
        self.injected = 0

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
        self.assertEqual(methods[:5], ["initialize", "initialized", "account/read",
                                       "thread/start", "thread/realtime/start"])
        init = d.received("initialize")[0]["params"]
        self.assertTrue(init["capabilities"]["experimentalApi"])
        thread = d.received("thread/start")[0]["params"]
        self.assertEqual((thread["ephemeral"], thread["sandbox"], thread["approvalPolicy"]),
                         (True, "read-only", "untrusted"))
        self.assertEqual(thread["environments"], [])
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

    def test_mail_waits_while_no_session_is_live_and_survives_a_restart(self):
        d = self.door()
        d.live()
        d.api("/api/session/stop", {})
        d.mail("ops:2", mail("m3", kind="notice", body="later"))
        time.sleep(0.6)
        self.assertEqual(d.delivered, [])
        state_dir = d.state
        d.close()
        # The restarted door is sent the same mail again; still unspoken, still held.
        d2 = self.door(state=state_dir)
        d2.mail("ops:2", mail("m3", kind="notice", body="later"))
        time.sleep(0.6)
        self.assertEqual(d2.delivered, [])
        d2.live()
        wait_for("delivered", lambda: d2.delivered == ["ops:2"])
        d2.close()
        # Spoken before this restart: acknowledged at once, never spoken twice.
        d3 = self.door(state=state_dir)
        d3.mail("ops:2", mail("m3", kind="notice", body="later"))
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


class ExactWordsTest(DoorTest):
    WORDS = "Tell ops the deploy window moves to Friday at nine"

    def test_the_captains_exact_words_are_read_back_and_sent_on_send(self):
        d = self.door()
        d.live()
        d.say(self.WORDS)
        result = d.tool(11, "privateer_send", {"to": "Ops", "message": "deploy is Friday"})
        self.assertTrue(result["result"]["success"], result)
        d.wait_spoken(f"I will send ops: {self.WORDS}. Say send to confirm.")
        self.assertEqual(d.api("/api/state")[1]["draft"], {"to": "ops", "text": self.WORDS})
        time.sleep(0.4)
        self.assertEqual(d.sends, [], "sent before the captain confirmed")
        d.say("Send it.")
        wait_for("the send", lambda: d.sends)
        sent = d.sends[0]
        self.assertEqual((sent["to"], sent["kind"], sent["body"]), ("ops", "request", self.WORDS))
        self.assertNotIn("payload", sent)
        self.assertTrue(sent["id"].startswith("voice-"))
        d.wait_spoken("Sent to ops.")
        # A second "send" has nothing to confirm.
        d.say("send")
        time.sleep(0.5)
        self.assertEqual(len(d.sends), 1)

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
        readback = d.wait_spoken("Say send to confirm")[0]
        # The door's own words heard back are not the captain.
        d.say(readback)
        d.say(readback.split(": ", 1)[1])
        time.sleep(0.4)
        self.assertIsNotNone(d.api("/api/state")[1]["draft"])
        d.say("Actually, let me think. Send it later")
        d.wait_spoken("The message to ops was not sent.")
        d.say("send")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])
        d.say(self.WORDS)
        d.tool(22, "privateer_send", {"to": "ops"})
        d.say("cancel")
        d.wait_spoken("Not sent to ops.")
        d.say("send")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])

    def test_an_unconfirmed_draft_expires(self):
        d = self.door(config={"draft_ttl_secs": 1})
        d.live()
        d.say(self.WORDS)
        d.tool(31, "privateer_send", {"to": "ops"})
        d.wait_spoken("expired; nothing was sent")
        d.say("send")
        time.sleep(0.4)
        self.assertEqual(d.sends, [])

    def test_a_failed_send_keeps_its_id_so_send_again_is_the_same_message(self):
        d = self.door()
        d.live()
        d.send_error = "home is busy"
        d.say(self.WORDS)
        d.tool(41, "privateer_send", {"to": "ops"})
        d.say("send")
        d.wait_spoken("Not sent: home is busy. Say send to try again.")
        d.send_error = None
        d.say("send")
        wait_for("the send", lambda: d.sends)
        failed = [r for r in d.requests if r["command"] == "send"]
        self.assertEqual(len(failed), 2)
        self.assertEqual(failed[0]["args"]["id"], failed[1]["args"]["id"])

    def test_stop_and_restart_never_send_twice_or_confirm_an_old_draft(self):
        d = self.door()
        d.live()
        d.say(self.WORDS)
        d.tool(51, "privateer_send", {"to": "ops"})
        d.wait_spoken("Say send to confirm")
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        d.live()
        d.say("send", thread="thr-2")
        time.sleep(0.5)
        self.assertEqual(d.sends, [], "a new session confirmed an old draft")
        d.say(self.WORDS, thread="thr-2")
        d.tool(52, "privateer_send", {"to": "ops"}, thread="thr-2")
        d.say("send", thread="thr-2")
        wait_for("the send", lambda: d.sends)
        # A page reload starts again: nothing is replayed.
        self.assertEqual(d.api("/api/session/stop", {})[0], 200)
        d.live()
        d.say("send", thread="thr-3")
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
        d = self.door()
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
            d.say("send")
        # Spoken first, then asked for "the latest question": still refused.
        for n, ask in enumerate(self.ASKS.values()):
            d.mail(f"ops:{n + 10}", ask)
        wait_for("delivered", lambda: len(d.delivered) == len(self.ASKS))
        d.say("yes")
        result = d.tool(199, "privateer_answer", {})
        self.assertFalse(result["result"]["success"])
        d.say("send")
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
        d.wait_spoken("I will answer ops: Use the eu west region.")
        d.say("send")
        wait_for("the reply", lambda: d.sends)
        self.assertEqual(d.sends[0], {"id": d.sends[0]["id"], "to": "ops", "kind": "reply",
                                      "correlation": "q1", "body": "Use the eu west region"})
        d.say("Large please")
        d.tool(202, "privateer_answer", {"mail_id": "q2"})
        d.say("confirm")
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
        self.assertEqual(talk.heard("  SEND!  "), ("confirm", draft))
        self.assertIsNone(talk.draft)
        talk.heard("Tell ops bye")
        draft = talk.propose("ops")
        clock[0] = 11
        self.assertIs(talk.expire(), draft)
        talk.heard("x")
        draft = talk.propose("ops")
        clock[0] = 30
        self.assertEqual(talk.heard("send"), ("expired", draft))


if __name__ == "__main__":
    unittest.main(verbosity=1)
