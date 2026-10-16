#!/usr/bin/env python3
"""End to end: the Telegram front door under the real Privateer supervisor.

A two-ship fleet (a liaison and `ops`) on a scratch HOME, `privateer door run`
supervising plugins/telegram, and a fake Telegram Bot API on localhost. Checks
mail both ways, reply routing and threading, decisions, the trust boundary,
records-only status, and delivery across a supervisor restart.

Usage: telegram_door.py /path/to/privateer
"""

import http.server
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
TOKEN = "123:secret-token"
CHAT = 42
CAPTAIN = 7


class FakeTelegram:
    """Just enough Bot API: queued updates, recorded calls, numbered messages."""

    def __init__(self):
        self.lock = threading.Lock()
        self.calls: list[tuple[str, dict]] = []
        self.updates: list[dict] = []
        self.next_update = 1
        self.next_message = 1000
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                form = dict(urllib.parse.parse_qsl(self.rfile.read(length).decode()))
                path = self.path.split("?")[0]
                assert path.startswith(f"/bot{TOKEN}/"), path
                result = fake.handle(path.rsplit("/", 1)[1], form)
                body = json.dumps({"ok": True, "result": result}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def handle(self, method: str, form: dict):
        if method == "getUpdates":
            offset = int(form.get("offset", 0))
            deadline = time.time() + min(float(form.get("timeout", 0)), 0.5)
            while True:
                with self.lock:
                    if offset == -1:
                        return self.updates[-1:]
                    ready = [u for u in self.updates if u["update_id"] >= offset]
                if ready or time.time() >= deadline:
                    return ready
                time.sleep(0.05)
        with self.lock:
            self.calls.append((method, form))
            if method == "sendMessage":
                self.next_message += 1
                form["_message_id"] = self.next_message
                return {"message_id": self.next_message, "chat": {"id": CHAT}}
        return True

    def update(self, message: dict | None = None, callback: dict | None = None):
        with self.lock:
            update = {"update_id": self.next_update}
            self.next_update += 1
            if message is not None:
                update["message"] = message
            if callback is not None:
                update["callback_query"] = callback
            self.updates.append(update)

    def message(self, text: str, reply_to: int | None = None, chat: int = CHAT,
                user: int = CAPTAIN) -> int:
        with self.lock:
            self.next_message += 1
            ident = self.next_message
        message = {"message_id": ident, "chat": {"id": chat}, "from": {"id": user},
                   "text": text}
        if reply_to:
            message["reply_to_message"] = {"message_id": reply_to}
        self.update(message)
        return ident

    def sent(self) -> list[dict]:
        with self.lock:
            return [form for method, form in self.calls if method == "sendMessage"]

    def called(self, method: str) -> list[dict]:
        with self.lock:
            return [form for m, form in self.calls if m == method]


def wait_for(what: str, predicate, timeout: float = 30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


class Fleet:
    def __init__(self, binary: str, root: pathlib.Path):
        self.binary, self.root = binary, root
        bin_dir = root / "bin"
        bin_dir.mkdir()
        codex = bin_dir / "codex"
        codex.write_text("#!/bin/sh\nexit 0\n")
        codex.chmod(0o755)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("PRIVATEER_")}
        self.env.update({
            "HOME": str(root), "PATH": f"{bin_dir}:{os.environ.get('PATH', '/usr/bin:/bin')}",
            "PRIVATEER_FLEET": str(root / "state" / "fleet.sqlite"),
            "PRIVATEER_SERVICE": "none", "PRIVATEER_RESOURCE_HOME": str(root / "ledger"),
            "PRIVATEER_PLUGINS": str(ROOT / "plugins"),
        })
        self.homes = {}

    def run(self, args, cwd=None, check=True):
        out = subprocess.run([self.binary, *args], cwd=cwd or self.root, env=self.env,
                             capture_output=True, text=True, stdin=subprocess.DEVNULL)
        if check and out.returncode != 0:
            raise AssertionError(f"{args}: {out.stderr}")
        return out

    def ship(self, name: str, liaison: bool = False):
        folder = self.root / "ships" / name
        folder.mkdir(parents=True)
        self.run(["init", "--ship", name, "--harness", "codex"] + (["--liaison"] if liaison else []),
                 cwd=folder)
        self.homes[name] = folder / ".privateer"

    def call(self, ship: str, command: str, args: dict):
        out = self.run(["--home", str(self.homes[ship]), command, "--json", json.dumps(args)])
        return json.loads(out.stdout)

    def inbox(self, ship: str, address: str):
        return self.call(ship, "mail.inbox", {"address": address, "all": True})["messages"]


def main() -> int:
    binary = os.path.abspath(sys.argv[1])
    telegram = FakeTelegram()
    with tempfile.TemporaryDirectory() as temp:
        root = pathlib.Path(temp).resolve()
        fleet = Fleet(binary, root)
        fleet.ship("liaison", liaison=True)
        fleet.ship("ops")

        config = root / "telegram.toml"
        config.write_text(f'bot_token = "{TOKEN}"\nchat_id = "{CHAT}"\n'
                          f'allowed_user_ids = [{CAPTAIN}]\napi_base = "{telegram.url}"\n')
        refused = fleet.run(["door", "add", "telegram"], check=False)
        assert refused.returncode != 0 and "--config" in refused.stderr, refused.stderr
        config.chmod(0o644)
        refused = fleet.run(["door", "add", "telegram", "--config", str(config)], check=False)
        assert refused.returncode != 0 and "private" in refused.stderr, refused.stderr
        config.chmod(0o600)
        fleet.run(["door", "add", "telegram", "--config", str(config)])

        # Mail waiting before the door ever ran still reaches the captain.
        early = fleet.call("ops", "mail.send", {"from": "ops", "to": "captain", "kind": "request",
                                                "body": "Deploy **v2** to staging?"})

        def start():
            return subprocess.Popen([binary, "door", "run", "telegram"], env=fleet.env,
                                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True)

        supervisor = start()
        state = root / "state" / "doors" / "telegram" / "state"
        try:
            pushed = wait_for("the early request on the phone", lambda: [
                m for m in telegram.sent() if "Deploy <b>v2</b>" in m["text"]])
            assert "<b>ops</b> · request" in pushed[0]["text"], pushed[0]
            assert pushed[0]["chat_id"] == str(CHAT)
            early_tg = pushed[0]["_message_id"]
            assert telegram.called("setMyCommands"), "command menu published"

            # A plain message goes to the liaison as a request from the captain.
            question = telegram.message("How is the API refactor going?")
            asked = wait_for("the question in the liaison's mailbox", lambda: [
                m for m in fleet.inbox("liaison", "liaison")
                if m["body"] == "How is the API refactor going?"])[0]
            assert asked["from"] == "captain" and asked["kind"] == "request", asked
            wait_for("a 👍 confirmation", lambda: [
                r for r in telegram.called("setMessageReaction") if r["message_id"] == str(question)])

            # The liaison's answer threads under the captain's question.
            fleet.call("liaison", "mail.send", {"from": "liaison", "to": "captain", "kind": "reply",
                                                "correlation": asked["id"], "body": "Two PRs left."})
            answer = wait_for("the liaison's answer", lambda: [
                m for m in telegram.sent() if "Two PRs left." in m["text"]])[0]
            assert answer.get("reply_to_message_id") == str(question), answer

            # Replying to the ops request answers it, in ops's home.
            telegram.message("yes, go ahead", reply_to=early_tg)
            replied = wait_for("the captain's reply in ops", lambda: [
                m for m in fleet.inbox("ops", "ops")
                if m["kind"] == "reply" and m["correlation"] == early["id"]])[0]
            assert replied["body"] == "yes, go ahead" and replied["from"] == "captain"
            shown = fleet.call("ops", "mail.show", {"id": early["id"]})
            assert shown["state"] == "answered", shown

            # Strangers are not heard, even in the right chat.
            telegram.message("rm -rf everything", user=666)
            telegram.message("hello from elsewhere", chat=99)

            # /to sends straight to a ship.
            telegram.message("/to ops check the disk")
            wait_for("the /to message in ops", lambda: [
                m for m in fleet.inbox("ops", "ops") if m["body"] == "check the disk"])

            # A reply to something the door never carried is refused, not redirected.
            telegram.message("which one?", reply_to=5)
            wait_for("the unknown-reply warning", lambda: [
                m for m in telegram.sent() if "don't know which message" in m["text"]])

            # Status comes from records.
            telegram.message("/status")
            status = wait_for("the status answer", lambda: [
                m for m in telegram.sent() if "Fleet status" in m["text"]])[0]
            assert "liaison" in status["text"] and "ops" in status["text"], status

            # Captain commands configure all ships without inference or settings edits.
            telegram.message("/yolo on", user=999)
            time.sleep(.5)
            assert json.loads(fleet.run(["permissions"]).stdout)["mode"] == "standard"
            telegram.message("/yolo on")
            wait_for("fleet YOLO confirmation", lambda: [m for m in telegram.sent() if "Fleet tool mode: yolo" in m["text"]])
            assert json.loads(fleet.run(["permissions"]).stdout)["mode"] == "yolo"
            telegram.message("/yolo off")
            wait_for("fleet standard confirmation", lambda: [m for m in telegram.sent() if "Fleet tool mode: standard" in m["text"]])
            assert json.loads(fleet.run(["permissions"]).stdout)["mode"] == "standard"

            # A decision with options gets buttons; a button answers it.
            decision = fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "decision", "body": "Which region?",
                "payload": {"options": ["us-east", "eu-west"]}})
            card = wait_for("the decision card", lambda: [
                m for m in telegram.sent() if "Which region?" in m["text"]])[0]
            keyboard = json.loads(card["reply_markup"])["inline_keyboard"]
            assert [row[0]["text"] for row in keyboard] == ["us-east", "eu-west"], keyboard
            telegram.update(callback={"id": "cb1", "from": {"id": CAPTAIN},
                                      "data": keyboard[1][0]["callback_data"],
                                      "message": {"message_id": 1, "chat": {"id": CHAT}}})
            wait_for("the decision's answer", lambda: [
                m for m in fleet.inbox("ops", "ops")
                if m["kind"] == "reply" and m["correlation"] == decision["id"]
                and m["body"] == "eu-west"])

            for ship in ("liaison", "ops"):
                for m in fleet.inbox(ship, ship):
                    assert "everything" not in m["body"] and "elsewhere" not in m["body"], m

            # Away: /afk holds routine mail, an emergency breaks through marked,
            # /held lists the rest, and an ordinary message is the return.
            telegram.message("/afk until 8am merge #9 if green")
            wait_for("the away confirmation", lambda: [
                m for m in telegram.sent()
                if m["text"].startswith("🌙 Away from") and "merge #9 if green" in m["text"]])
            before = len(telegram.sent())
            fleet.call("ops", "mail.send", {"from": "ops", "to": "captain", "kind": "notice",
                                            "body": "Nightly build done."})
            fleet.call("ops", "mail.send", {"from": "ops", "to": "captain", "kind": "notice",
                                            "urgency": "high", "body": "Production is down."})
            alarm = wait_for("the emergency", lambda: [
                m for m in telegram.sent()[before:] if "Production is down." in m["text"]])[0]
            assert alarm["text"].startswith("🚨"), alarm
            cards = lambda: [m for m in telegram.sent()[before:]
                             if "<b>ops</b>" in m["text"] and "Nightly build" in m["text"]]
            time.sleep(2)
            assert not cards(), "routine mail reached the phone while away"
            telegram.message("/held")
            wait_for("the held list", lambda: [
                m for m in telegram.sent()[before:]
                if m["text"].startswith("1 held:") and "Nightly build done." in m["text"]])
            telegram.message("good morning")
            welcome = wait_for("the return digest", lambda: [
                m for m in telegram.sent()[before:] if m["text"].startswith("🌅 Welcome back.")])[0]
            assert "1 emergency sent" in welcome["text"], welcome
            summary = [m for m in fleet.inbox("liaison", "liaison")
                       if m["body"].startswith("I'm back after")]
            assert summary and "Nightly build done." in summary[-1]["body"], summary
            time.sleep(2)
            assert not cards(), "held mail was replayed after the return"

            # Stopping the supervisor stops the adapter.
            adapter = int((state / "adapter.pid").read_text())
            supervisor.send_signal(signal.SIGTERM)
            supervisor.wait(timeout=20)
            wait_for("the adapter to exit", lambda: not alive(adapter), timeout=10)

            # Mail sent while the door was down arrives once it is back, and
            # what was already delivered is not sent again.
            before = len(telegram.sent())
            fleet.call("ops", "mail.send", {"from": "ops", "to": "captain", "kind": "notice",
                                            "body": "Disk is fine."})
            supervisor = start()
            wait_for("the notice sent while down", lambda: [
                m for m in telegram.sent()[before:] if "Disk is fine." in m["text"]])
            time.sleep(3)
            repeats = [m for m in telegram.sent()[before:] if "Deploy" in m["text"]
                       or "Two PRs left." in m["text"]]
            assert not repeats, repeats
        finally:
            supervisor.send_signal(signal.SIGTERM)
            try:
                supervisor.wait(timeout=20)
            except subprocess.TimeoutExpired:
                supervisor.kill()
        print("telegram door: ok")
    return 0


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


if __name__ == "__main__":
    sys.exit(main())
