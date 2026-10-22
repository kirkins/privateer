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
import re
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
        self.next_file = 0
        # What getChat says the configured chat is.
        self.chat_type = "private"
        # Telegram file ids to their remote paths, and the bytes at each path.
        self.file_paths: dict[str, str] = {}
        self.files: dict[str, bytes] = {}
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                path = self.path.split("?")[0]
                prefix = f"/file/bot{TOKEN}/"
                assert path.startswith(prefix), path
                data = fake.files.get(path[len(prefix):])
                if data is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

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
            if method == "getFile":
                remote = self.file_paths.get(form.get("file_id"))
                return {"file_path": remote} if remote else None
            if method == "getChat":
                return {"id": int(form.get("chat_id", 0)), "type": self.chat_type}
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

    def send_file(self, data: bytes, name: str, caption: str | None = None,
                  photo: bool = False) -> int:
        """A photo or document from the captain, downloadable via getFile."""
        with self.lock:
            self.next_file += 1
            file_id = f"file{self.next_file}"
            self.next_message += 1
            ident = self.next_message
        remote = f"documents/{name}"
        self.file_paths[file_id] = remote
        self.files[remote] = data
        if photo:
            media = {"photo": [{"file_id": file_id, "file_size": len(data),
                                "width": 800, "height": 600}]}
        else:
            media = {"document": {"file_id": file_id, "file_name": name,
                                  "file_size": len(data)}}
        message = {"message_id": ident, "chat": {"id": CHAT}, "from": {"id": CAPTAIN}, **media}
        if caption:
            message["caption"] = caption
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


def check_age_phrases() -> None:
    """The exact words the door puts on an age: a stow incident made this matter."""
    probe = ("import json, sys; sys.path.insert(0, sys.argv[1]); import door; "
             "print(json.dumps([door.age_phrase(ms) for ms in "
             "(0, -5000, 5_000, 30_000, 120_000, 7_200_000, 3 * 86_400_000)]))")
    out = subprocess.run([sys.executable, "-c", probe, str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    # A clock that reads behind the mail never says "0 s ago" or a negative age.
    assert json.loads(out.stdout) == ["just now", "just now", "just now", "30 s ago",
                                      "2 min ago", "2 h ago", "3 d ago"], out.stdout


CARD_PROBE = """
import json, sys, tempfile
sys.path.insert(0, sys.argv[1])
import door, tg
tg.STATE_DIR = tempfile.mkdtemp()
said = []
d = door.Door({"door": "t"}, {"tts": {}}, None)
d.say = lambda text, reply_to=None, html=False, markup=None, quiet=False: (
    said.append({"text": text, "quiet": quiet}) or (len(said), ""))
unknown = {"id": "m1", "from": "ops", "kind": "notice", "urgency": "normal",
           "body": "joins an ask shown before cards were kept", "payload": {"attached_to": "gone"}}
urgent = {"id": "m2", "from": "ops", "kind": "notice", "urgency": "high",
          "body": "prod is down too", "payload": {"attached_to": "gone"}}
digest = {"type": "digest", "ref": "digest:x", "id": "x", "lines": ["a", "b"], "loud": False}
for event in ({"ref": "ops:1", "ship": "ops", "mail": unknown},
              {"ref": "ops:2", "ship": "ops", "mail": urgent}):
    assert d.push(event) == ""
assert d.push_digest(digest) == ""
print(json.dumps(said))
"""


def check_card_rendering() -> None:
    """Mail attached to a card this door does not know is shown, quietly;
    high urgency is never only a silent card edit; a digest is one message."""
    out = subprocess.run([sys.executable, "-c", CARD_PROBE, str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    said = json.loads(out.stdout)
    assert len(said) == 3, said
    assert "joins an ask" in said[0]["text"] and said[0]["quiet"], said
    assert "prod is down too" in said[1]["text"] and not said[1]["quiet"], said
    assert said[2] == {"text": "a\nb", "quiet": True}, said


VOICE_PROBE = """
import json, sys, tempfile
sys.path.insert(0, sys.argv[1])
import door, stt, tg
tg.STATE_DIR = tempfile.mkdtemp()
out = {"prompts": {t: stt.prompt_for(t) for t in
                   (".", "...", " . ", "ok.", "5", "こんにちは")}}
# The door end to end with a fake transcriber: a punctuation-only transcript is
# answered with a re-record request and never sent to a ship.
cfg = {"stt": {"api_key": "test-key"}, "tts": {}}
said, sent = [], []
d = door.Door({"door": "t"}, cfg, None)
d.say = lambda text, reply_to=None, html=False, markup=None, quiet=False: (
    said.append(text) or (len(said), ""))
d.send = lambda args, msg_id, label: sent.append(args)
d.route = lambda text, msg_id, reply_to: {"to": "ops", "kind": "request"}
d.fetch = lambda file_id, name, msg_id: "/nonexistent/voice.ogg"
voice = {"voice": {"file_id": "f", "duration": 3}, "message_id": 9}
stt.transcribe = lambda cfg, path: (".", "")
d.handle_voice(voice, 9, None)
out["said"], out["sent"] = list(said), list(sent)
# A voice reply to a typed decision answers with the transcript, not "(voice)".
typed = []
d.send = lambda args, msg_id, label: typed.append(args)
d.route = lambda text, msg_id, reply_to: {"to": "ops", "kind": "reply", "correlation": "m1",
                                          "body": text, "payload": {"answer": {"text": text}}}
stt.transcribe = lambda cfg, path: ("go with b", "")
d.handle_voice(voice, 9, 7)
stt.transcribe = lambda cfg, path: ("word " * 1200, "")
d.handle_voice(voice, 9, 7)
out["typed"] = typed
print(json.dumps(out))
"""


def check_voice_transcripts() -> None:
    """A transcript with no letters or digits is empty: the door asks the captain to
    re-record and sends nothing; real words, including a bare "5" and non-Latin
    text, pass through as the captain's answer."""
    out = subprocess.run([sys.executable, "-c", VOICE_PROBE, str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    got = json.loads(out.stdout)
    prompts = got["prompts"]
    for silent in (".", "...", " . "):
        assert prompts[silent].startswith("[The user sent a voice message but it came through empty"), prompts
    assert prompts["ok."] == '"ok."' and prompts["5"] == '"5"', prompts
    assert prompts["こんにちは"] == '"こんにちは"', prompts
    assert got["said"] == ["🎙️ your voice note came through empty, please re-record"], got
    assert got["sent"] == [], got
    short, long = got["typed"]
    assert short["payload"]["answer"]["text"] == "go with b" and short["body"] == '"go with b"', got
    assert len(long["payload"]["answer"]["text"]) <= 4096 and len(long["body"]) > 4096, long["body"][:80]


ATTACHMENT_PROBE = """
import json, os, sys, tempfile
sys.path.insert(0, sys.argv[1])
import door, tg
tg.STATE_DIR = tempfile.mkdtemp()
sent, said = [], []

class NoPut:
    def call(self, command, args=None, timeout=30):
        raise RuntimeError("unknown door command \\"file.put\\"")

class Refuse:
    def call(self, command, args=None, timeout=30):
        raise RuntimeError("cannot reach ship macops")

class AlreadyAnswered:
    def __init__(self, seen):
        self.seen = seen
    def call(self, command, args=None, timeout=30):
        self.seen.append(args)
        if args.get("kind") == "reply":
            raise RuntimeError("mail already answered")
        return {"returned": None}

class Put:
    def __init__(self, seen):
        self.seen = seen
    def call(self, command, args=None, timeout=30):
        assert command == "file.put", command
        self.seen.append(args)
        return {"file_id": "file7", "name": args["name"]}

def phone(runtime, path, msg_id, spoken=None):
    d = door.Door({"door": "t", "recipient": "ops"}, {"tts": {}}, runtime)
    d.say = lambda text, *a, **k: (spoken.append(text) if spoken is not None else None, "")
    d.send = lambda args, msg_id, label: sent.append(args)
    d.route = lambda text, msg_id, reply_to: {"to": "ops", "kind": "request", "body": text}
    d.fetch = lambda file_id, name, msg_id: path
    msg = {"message_id": msg_id, "document": {
        "file_id": "f", "file_name": "notes.bin", "file_size": os.path.getsize(path)}}
    d.handle_attachment(msg, msg_id, None)

kept = os.path.join(tg.inbox_dir(), "kept.bin")
open(kept, "wb").write(b"fallback bytes")
quiet = []
phone(NoPut(), kept, 3, quiet)
out = {"fallback": sent[-1], "kept": os.path.exists(kept), "quiet": quiet}
warned = []
refused = os.path.join(tg.inbox_dir(), "refused.bin")
open(refused, "wb").write(b"refused bytes")
phone(Refuse(), refused, 5, warned)
out["refusal"], out["warned"] = sent[-1], warned
seen = []
stored = os.path.join(tg.inbox_dir(), "stored.bin")
open(stored, "wb").write(b"stored bytes")
phone(Put(seen), stored, 4)
out["put_args"], out["deleted"], out["stored"] = seen, not os.path.exists(stored), sent[-1]

answered = []
d = door.Door({"door": "t", "recipient": "ops"}, {"tts": {}}, AlreadyAnswered(answered))
d.say = lambda *a, **k: (None, "")
d.send({"to": "ops", "kind": "reply", "body": "done",
        "payload": {"answer": {"text": "done"}, "files": [{"file_id": "file9"}]}}, 6, "reply")
out["retry"] = answered
print(json.dumps(out))
"""


def check_attachment_put_and_fallback() -> None:
    """A file goes through file.put and rides payload.files; when the runtime
    has no file.put, its inbox path is kept and the ship is told instead. Any
    other refusal warns the captain. An already-answered retry keeps files."""
    out = subprocess.run([sys.executable, "-c", ATTACHMENT_PROBE,
                          str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    got = json.loads(out.stdout)
    fallback = got["fallback"]
    assert "The captain sent a file:" in fallback["body"], fallback
    assert os.path.basename(fallback["body"].rsplit(": ", 1)[1]) == "kept.bin", fallback
    assert "files" not in (fallback.get("payload") or {}), fallback
    assert got["kept"], got
    assert got["quiet"] == [], got["quiet"]
    refusal = got["refusal"]
    warned = "⚠️ couldn't hand notes.bin to ops: cannot reach ship macops"
    assert warned in got["warned"][0], got["warned"]
    assert refusal["body"].startswith("The captain sent a file: "), refusal
    assert "files" not in (refusal.get("payload") or {}), refusal
    reply, retry = got["retry"]
    assert reply["kind"] == "reply" and reply["payload"]["answer"] == {"text": "done"}, reply
    assert retry["kind"] == "request", retry
    assert retry["payload"] == {"files": [{"file_id": "file9"}]}, retry
    assert got["put_args"] == [{"to": "ops", "name": "notes.bin",
                                "content_file": got["put_args"][0]["content_file"]}], got
    assert got["put_args"][0]["content_file"].endswith("stored.bin"), got["put_args"]
    assert got["deleted"], got
    stored = got["stored"]
    assert stored["payload"] == {"files": [{"file_id": "file7"}]}, stored
    assert stored["body"] == "The captain sent a file: notes.bin", stored


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

    def run(self, args, cwd=None, check=True, env=None):
        out = subprocess.run([self.binary, *args], cwd=cwd or self.root, env=env or self.env,
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

    def other_machine(self, ship: str) -> str:
        """A second machine holding one ship, linked over a local fake ssh."""
        root = self.root / "mac"
        (root / "bin").mkdir(parents=True)
        codex = root / "bin" / "codex"
        codex.write_text("#!/bin/sh\nexit 0\n")
        codex.chmod(0o755)
        folder = root / ship
        folder.mkdir()
        self.run(["init", "--ship", ship, "--harness", "codex"], cwd=folder,
                 env={"HOME": str(root), "PATH": f"{root / 'bin'}:{os.environ.get('PATH', '/usr/bin:/bin')}",
                      "PRIVATEER_FLEET": str(root / "fleet.sqlite"), "PRIVATEER_SERVICE": "none",
                      "PRIVATEER_RESOURCE_HOME": str(root / "ledger"),
                      "PRIVATEER_PLUGINS": str(ROOT / "plugins")})
        # Like OpenSSH, run the last argument as a shell command line, on the
        # other machine's own home, fleet and PATH.
        ssh = self.root / "mac-ssh"
        ssh.write_text(f"#!/bin/sh\nfor argument do remote=$argument; done\n"
                       f"exec env HOME={root} PATH={root / 'bin'}:/usr/bin:/bin"
                       f" PRIVATEER_FLEET={root / 'fleet.sqlite'}"
                       f" PRIVATEER_RESOURCE_HOME={root / 'ledger'} PRIVATEER_SERVICE=none"
                       f" PRIVATEER_PLUGINS={ROOT / 'plugins'} /bin/sh -c \"$remote\"\n")
        ssh.chmod(0o755)
        self.env["PRIVATEER_SSH_EXECUTABLE"] = str(ssh)
        self.run(["machine", "add", "mac", "test-host", "--executable", self.binary,
                  "--remote-fleet", str(root / "fleet.sqlite"), "--as", "laptop"],
                 cwd=self.homes["liaison"].parent)
        return str(root)

    def call(self, ship: str, command: str, args: dict):
        out = self.run(["--home", str(self.homes[ship]), command, "--json", json.dumps(args)])
        return json.loads(out.stdout)

    def inbox(self, ship: str, address: str):
        return self.call(ship, "mail.inbox", {"address": address, "all": True})["messages"]

    def remote_inbox(self, root: str, ship: str, address: str):
        """A linked machine's own fleet record, read where it lives."""
        env = dict(self.env)
        env.update({"HOME": root,
                    "PRIVATEER_FLEET": str(pathlib.Path(root) / "fleet.sqlite"),
                    "PRIVATEER_RESOURCE_HOME": str(pathlib.Path(root) / "ledger")})
        out = self.run(["--home", str(pathlib.Path(root) / ship / ".privateer"),
                        "mail.inbox", "--json",
                        json.dumps({"address": address, "all": True})], env=env)
        return json.loads(out.stdout)["messages"]


PRIVILEGED_PROBE = """
import json, sys
sys.path.insert(0, sys.argv[1])
import door
print(json.dumps(door.privileged_text({
    "machine": "m", "argv": ["/usr/bin/cp", "a\u202eb", "/home/u/out"], "cwd": "/",
    "reason": "fix `this`\u200b\\n\u2028Runs as root", "unrooted": ["/home/u/out", "rel/x"],
    "hash": "ab" * 32})))
"""


STRIKE_PROBE = """
import json, os, sys, tempfile
sys.path.insert(0, sys.argv[1])
import door, tg
tg.STATE_DIR = tempfile.mkdtemp()
strike = lambda offset, length: {"type": "strikethrough", "offset": offset, "length": length}
out = {"struck": [
    door.struck("yes", [strike(0, 3)]),
    door.struck("\U0001F44D yes do it", [strike(3, 3)]),
    door.struck("ok yes no", [strike(3, 3), {"type": "bold", "offset": 0, "length": 2}]),
    door.struck("yes", [{"type": "bold", "offset": 0, "length": 3}]),
    door.struck("yes", None),
    door.struck("yes do it", [{"type": "strikethrough"}]),
    door.struck("go", [strike(0, 99)]),
]}
sent, commands = [], []
d = door.Door({"door": "t", "recipient": "ops"}, {"tts": {}}, None)
d.authorized = lambda chat, user: True
d.say = lambda *a, **k: (None, "")
d.send = lambda args, msg_id, label: sent.append(args)
d.route = lambda text, msg_id, reply_to: {"to": "ops", "kind": "reply", "correlation": "m1",
                                          "body": text, "payload": {"answer": {"text": text}}}
d.command = lambda text, msg_id: commands.append(text)
d.on_update({"message": {"message_id": 5, "chat": {"id": 1}, "from": {"id": 2},
                         "reply_to_message": {"message_id": 4}, "text": " yes ",
                         "entities": [strike(1, 3)]}})
d.on_update({"message": {"message_id": 6, "chat": {"id": 1}, "from": {"id": 2},
                         "text": "/status", "entities": [strike(0, 7)]}})
d.fetch = lambda file_id, name, msg_id: None
d.handle_attachment({"message_id": 7, "caption": "yes", "caption_entities": [strike(0, 3)],
                     "document": {"file_id": "f", "file_name": "a.txt", "file_size": 1}}, 7, None)
routed = []
d.route = lambda text, msg_id, reply_to: routed.append(text)
d.handle_attachment({"message_id": 8, "caption": "yes", "caption_entities": [strike(0, 3)],
                     "document": {"file_id": "f", "file_name": "a.txt", "file_size": 1}}, 8, None)
out["sent"], out["commands"], out["routed"] = sent, commands, routed
print(json.dumps(out))
"""


GROUP_PROBE = """
import json, sys, tempfile
sys.path.insert(0, sys.argv[1])
import door, tg
tg.STATE_DIR = tempfile.mkdtemp()
class Runtime:
    def __init__(self): self.calls = []
    def call(self, command, args=None, timeout=30):
        self.calls.append(command); return {"mode": "yolo"}
out = {}
def message(user, text, kind=None):
    chat = {"id": -100}
    if kind: chat["type"] = kind
    return {"message": {"message_id": 1, "chat": chat, "from": {"id": user}, "text": text}}
for name, cfg in (("listed", {"chat_id": "-100", "allowed_user_ids": [2]}), ("empty", {"chat_id": "-100"})):
    runtime = Runtime()
    d = door.Door({"door": "t", "recipient": "ops"}, cfg, runtime)
    said = []
    d.say = lambda text, *a, **k: said.append(text)
    d.group = True
    for user in (2, 3):
        d.on_update(message(user, "/yolo on"))
    d.on_update(message(3, "/help"))
    out[name] = {"calls": runtime.calls, "said": said}
# A door that thought its chat private learns otherwise from an update.
runtime = Runtime()
d = door.Door({"door": "t", "recipient": "ops"}, {"chat_id": "-100"}, runtime)
said = []
d.say = lambda text, *a, **k: said.append(text)
d.on_update(message(3, "/dismiss abc", kind="supergroup"))
out["learned"] = {"calls": runtime.calls, "said": said, "group": d.group}
print(json.dumps(out))
"""


def check_group_chat_guard(telegram: FakeTelegram) -> None:
    """In a group, any member is heard as the captain unless allowed_user_ids
    says who: the door refuses to start a group chat without it, and the
    fleet-wide commands (/yolo, /stow, /afk, /dismiss) run only for a listed
    user, never for a member an empty list would let through."""
    plugin = ROOT / "plugins" / "telegram"
    with tempfile.TemporaryDirectory() as temp:
        root = pathlib.Path(temp)
        config = root / "telegram.toml"
        hello = json.dumps({"type": "hello", "version": 1, "door": "telegram",
                            "state_dir": str(root / "state"), "config_path": str(config)})

        def start(chat_type: str, allowed: str) -> subprocess.CompletedProcess:
            config.write_text(f'bot_token = "{TOKEN}"\nchat_id = "{-CHAT}"\n{allowed}'
                              f'api_base = "{telegram.url}"\n')
            config.chmod(0o600)
            telegram.chat_type = chat_type
            try:
                return subprocess.run([sys.executable, str(plugin / "door.py")], input=hello + "\n",
                                      capture_output=True, text=True, timeout=30, cwd=ROOT)
            finally:
                telegram.chat_type = "private"

        refused = start("supergroup", "")
        assert refused.returncode == 2 and "allowed_user_ids" in refused.stderr, refused.stderr
        assert not (root / "state" / "offset.json").exists(), "the refused door must not poll"
        for chat_type, allowed in (("private", ""), ("supergroup", f"allowed_user_ids = [{CAPTAIN}]\n")):
            started = start(chat_type, allowed)
            assert started.returncode == 0, (chat_type, started.stderr)
            assert "door telegram up" in (root / "state" / "telegram.log").read_text(), chat_type
    out = subprocess.run([sys.executable, "-c", GROUP_PROBE, str(plugin)], capture_output=True, text=True,
                         cwd=ROOT)
    assert out.returncode == 0, out.stderr
    got = json.loads(out.stdout)
    # With a list, the listed user runs /yolo and the unlisted member is not even heard.
    assert got["listed"]["calls"] == ["permissions"], got["listed"]
    # Without one, every member is heard, but /yolo runs for none; /help still answers.
    assert got["empty"]["calls"] == [], got["empty"]
    refusal = "🔒 /yolo needs your Telegram user id in allowed_user_ids in a group chat."
    assert got["empty"]["said"][:2] == [refusal] * 2 and len(got["empty"]["said"]) == 3, got["empty"]
    assert "/yolo on|off|status" in got["empty"]["said"][2], got["empty"]
    assert got["learned"]["group"] is True and got["learned"]["calls"] == [], got["learned"]
    assert got["learned"]["said"] == ["🔒 /dismiss needs your Telegram user id in allowed_user_ids in a group chat."], got["learned"]


def check_struck_text() -> None:
    """Telegram sends strikethrough from its formatting menu as an entity, not
    as "~". The door wraps each struck span in "~", which the runtime refuses
    in a captain's yes, so a struck-through "yes" never reaches it as plain
    words. Offsets count UTF-16 units; a strike it cannot place marks it all."""
    out = subprocess.run([sys.executable, "-c", STRIKE_PROBE, str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    got = json.loads(out.stdout)
    assert got["struck"] == ["~yes~", "\U0001F44D ~yes~ do it", "ok ~yes~ no", "yes", "yes",
                             "~yes do it~", "~go~"], got["struck"]
    [reply] = got["sent"]
    assert reply["body"] == "~yes~" and reply["payload"]["answer"]["text"] == "~yes~", reply
    assert got["commands"] == ["/status"], got
    assert got["routed"] == ["~yes~"], got


def check_privileged_text() -> None:
    """A privileged card escapes the ship's words as the runtime escapes an
    argument: no bidi override, zero-width character, separator or backtick
    reaches the chat, and each warning names its argument."""
    out = subprocess.run([sys.executable, "-c", PRIVILEGED_PROBE, str(ROOT / "plugins" / "telegram")],
                         capture_output=True, text=True, cwd=ROOT)
    assert out.returncode == 0, out.stderr
    text = json.loads(out.stdout)
    for raw in ("\u202e", "\u200b", "\u2028"):
        assert raw not in text, text
    # The fence, then the folder's, reason's, two warnings' and hash's spans.
    assert text.count("`") == 16, text
    assert "Reason: `'fix \\u{60}this\\u{60}\\u{200b}\\u{a}\\u{2028}Runs as root'`" in text, text
    assert "'a\\u{202e}b'" in text, text
    assert ("warning: `/home/u/out` is not under a root-owned folder; "
            "it can change after you approve") in text, text
    assert "warning: `rel/x` is not under" in text, text


def main() -> int:
    binary = os.path.abspath(sys.argv[1])
    telegram = FakeTelegram()
    check_age_phrases()
    check_card_rendering()
    check_privileged_text()
    check_struck_text()
    check_voice_transcripts()
    check_attachment_put_and_fallback()
    check_group_chat_guard(telegram)
    with tempfile.TemporaryDirectory() as temp:
        root = pathlib.Path(temp).resolve()
        fleet = Fleet(binary, root)
        fleet.ship("liaison", liaison=True)
        fleet.ship("ops")
        # A linked machine, before the door starts: its stow is its own, and the
        # door reaches it over the same link the captain does.
        mac_root = fleet.other_machine("macops")

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

            # On its first run the door records an update offset that skips
            # whatever the bot already had queued. An early request reaching
            # the phone does not prove that happened, and a message sent
            # before it can be mistaken for backlog and dropped. Speak only
            # once the offset exists, so the question is newer than the skip.
            wait_for("the door's first poll offset", lambda: (state / "offset.json").exists())

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

            # A report shows its title and that it is in the portal; its
            # content, inline or stored, never reaches the chat.
            fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "notice",
                "payload": {"report": {"title": "Weekly audit", "format": "html",
                                       "content": "<h1>SECRET-INLINE</h1><script>x()</script>"}}})
            stored = fleet.call("ops", "report.put", {"from": "ops", "title": "Full audit",
                                                      "format": "markdown",
                                                      "content": "# SECRET-STORED"})
            fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "notice", "body": "Sign off the audit?",
                "payload": {"report": {"report_id": stored["report_id"]}}})
            inline = wait_for("the inline report's title", lambda: [
                m for m in telegram.sent() if "Weekly audit" in m["text"]])[0]
            assert "read it in the portal" in inline["text"], inline
            referenced = wait_for("the stored report's title", lambda: [
                m for m in telegram.sent() if "Sign off the audit?" in m["text"]])[0]
            assert "Full audit" in referenced["text"] and "portal" in referenced["text"], referenced
            for sent in telegram.sent():
                assert "SECRET" not in sent["text"] and "script" not in sent["text"], sent

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

            # /waiting re-shows what is still outstanding, saying how long ago
            # each request was asked, so an old one reads as old.
            fleet.call("ops", "mail.send", {"from": "ops", "to": "captain", "kind": "request",
                                            "body": "Is the Mac still stowed?"})
            telegram.message("/waiting")
            listed = wait_for("the waiting list", lambda: [
                m for m in telegram.sent() if "waiting on you" in m["text"]])
            assert listed[0]["text"].startswith("1 waiting on you"), listed[0]
            reread = wait_for("the request re-shown with its age", lambda: [
                m for m in telegram.sent()
                if "Is the Mac still stowed?" in m["text"] and "· asked" in m["text"]])
            assert re.search(r"asked (just now|\d+ [smhd] ago)", reread[0]["text"]), reread[0]
            assert "reply to answer" in reread[0]["text"], reread[0]

            # An ask only the captain's hands can clear comes first, under its own
            # heading, with the exact step beside it.
            hands = fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "request",
                "body": "The disk check finished.",
                "payload": {"needs_hands": {"step": "Run `sudo privateer-unlock ops` on the Mac"}}})
            before = len(telegram.sent())
            telegram.message("/waiting")

            def reshown() -> list:
                # Only the cards after the list's header: a request's own push
                # may still be in flight and land after `before`, ahead of the list.
                texts = [m["text"] for m in telegram.sent()[before:]]
                header = next((i for i, t in enumerate(texts) if "waiting on you" in t), None)
                if header is None:
                    return []
                texts = texts[header:]
                both = any("needs your hands" in t for t in texts) and any(
                    "Is the Mac still stowed?" in t for t in texts)
                return texts if both else []

            texts = wait_for("the hands-on card and the ordinary card", reshown)
            hands_at = next(i for i, t in enumerate(texts) if "needs your hands" in t)
            ordinary_at = next(i for i, t in enumerate(texts) if "Is the Mac still stowed?" in t)
            assert hands_at < ordinary_at, texts
            assert texts[hands_at - 1] == "Needs your hands:", texts
            assert "privateer-unlock ops" in texts[hands_at], texts[hands_at]

            # /dismiss closes one ask without answering it, by the first
            # characters of its id; the ship is told and it stops waiting.
            before = len(telegram.sent())
            telegram.message(f"/dismiss {hands['id'][:8]} done by hand")
            done = wait_for("the dismissal", lambda: [
                m for m in telegram.sent()[before:] if "Dismissed" in m["text"]])[0]
            assert hands["id"] in done["text"] and "ops" in done["text"], done
            closed = fleet.call("ops", "mail.show", {"id": hands["id"]})
            assert closed["state"] == "dismissed", closed
            assert closed["end_reason"] == "done by hand", closed
            told = [m for m in fleet.inbox("ops", "ops") if m.get("correlation") == hands["id"]]
            assert len(told) == 1 and told[0]["from"] == "captain", told
            waiting = fleet.call("liaison", "captain.waiting", {})["waiting"]
            assert all(w["mail"]["id"] != hands["id"] for w in waiting), waiting
            before = len(telegram.sent())
            telegram.message("/dismiss no-such-ask")
            wait_for("the unknown-id refusal", lambda: [
                m for m in telegram.sent()[before:] if "no-such-ask" in m["text"] and "⚠️" in m["text"]])

            # A photo or document from the captain reaches the ship's own home
            # and rides its mail as payload.files; the door keeps no inbox copy.
            photo = b"\x89PNG\r\n\x1a\n" + bytes(range(64))
            telegram.send_file(photo, "chart.png", caption="Quarterly chart",
                               photo=True)
            mailed = wait_for("the photo in the liaison's inbox", lambda: [
                m for m in fleet.inbox("liaison", "liaison")
                if (m.get("payload") or {}).get("files")])[0]
            named = mailed["payload"]["files"]
            assert len(named) == 1, named
            assert named[0]["name"] == "chart.png", named
            assert named[0]["bytes"] == len(photo), named
            assert pathlib.Path(named[0]["path"]).read_bytes() == photo, named
            assert mailed["body"] == "Quarterly chart", mailed
            inbox = root / "state" / "doors" / "telegram" / "state" / "inbox"
            assert not list(inbox.iterdir()), list(inbox.iterdir())

            # A document for the ship on the linked machine crosses the link
            # too, and only the Mac's own record names its path there.
            letters = b"from the captain's phone\n"
            received_before = len(fleet.remote_inbox(mac_root, "macops", "macops"))
            telegram.message("/to macops")
            wait_for("the Mac as the default ship", lambda: [
                m for m in telegram.sent() if "plain messages now go to macops" in m["text"]])
            telegram.send_file(letters, "note.txt", caption="Read this on the Mac")
            elsewhere = wait_for("the file on the Mac", lambda: [
                m for m in fleet.remote_inbox(mac_root, "macops", "macops")[
                    received_before:]
                if (m.get("payload") or {}).get("files")])[0]
            carried = elsewhere["payload"]["files"][0]
            assert pathlib.Path(carried["path"]).read_bytes() == letters, carried
            assert carried["name"] == "note.txt", carried
            assert not list(inbox.iterdir()), list(inbox.iterdir())
            telegram.message("/to default")
            wait_for("the liaison as the default ship again", lambda: [
                m for m in telegram.sent()
                if "plain messages go to liaison again" in m["text"]])

            # A stow names its machine. The Mac is linked (before the door
            # starts, so its calls can reach it), and its hold is its own: this
            # machine's status names it, a resume of one machine never speaks
            # for the other, and each names the machine it is about.
            def stow(command: str, marker: str) -> str:
                # Only what arrives after this command counts, so an earlier
                # reply that still says the same thing cannot answer for it.
                before = len(telegram.sent())
                telegram.message(command)
                return wait_for(f"{command}: {marker}", lambda: [
                    m for m in telegram.sent()[before:] if marker in m["text"]])[0]["text"]

            never = stow("/stow status", "has never been stowed")
            assert never.startswith("This machine (laptop) has never been stowed."), never
            assert "Still stowed" not in never and "Unknown here" not in never, never
            stowing = stow("/stow mac", "Stowing the fleet on mac")
            assert "new jobs are held" in stowing and "macops to checkpoint" in stowing, stowing
            stowed = stow("/stow", "Stowing this machine (laptop)")
            assert "ops to checkpoint" in stowed, stowed
            held = stow("/stow status", "(held)")
            assert "Stow on laptop (held)" in held and "Still stowed: mac." in held, held
            remote = stow("/stow status mac", "(held)")
            assert remote.startswith("Stow on mac (held)") and "Not ready yet" in remote, remote
            assert "Still stowed" not in remote and "laptop" not in remote, remote
            resumed = stow("/stow resume", "Resumed this machine (laptop)")
            assert "Still stowed: mac." in resumed, resumed
            # Resuming this machine does not speak for the Mac: it is still held.
            said = stow("/stow status mac", "(held)")
            assert "Stow on mac (held)" in said and "has resumed" not in said, said
            alone = stow("/stow status", "has resumed")
            assert "This machine (laptop) has resumed." in alone, alone
            assert "Still stowed: mac." in alone and "Stow on laptop:" in alone, alone
            # `--all` lifts the linked machine first, so the notice leaves no
            # machine named stowed, and it leaves nothing held behind it.
            stow("/stow", "Stowing this machine (laptop)")
            every = stow("/stow resume all", "Resumed this machine (laptop)")
            assert "Still stowed" not in every and "Unknown here" not in every, every

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

            # A privileged command is one card, its argv one per line, with
            # Approve and Deny. Approve grants it, and the captain is told it
            # waits for them at the machine; a text reply grants nothing.
            staged = fleet.call("ops", "privileged.stage", {
                "from": "ops", "task_id": "ops-duty", "program": "/usr/bin/id",
                "args": ["-u", "a b"], "cwd": "/", "reason": "Check the effective user"})
            card = wait_for("the privileged card", lambda: [
                m for m in telegram.sent()
                if "Run as root" in m["text"] and staged["hash"] in m["text"]])[0]
            assert "/usr/bin/id\n-u\n'a b'" in card["text"], card["text"]
            assert "reply to answer" not in card["text"], card["text"]
            keyboard = json.loads(card["reply_markup"])["inline_keyboard"]
            assert [b["text"] for b in keyboard[0]] == ["Approve", "Deny"], keyboard
            before = len(telegram.sent())
            telegram.message("yes do it", reply_to=card["_message_id"])
            wait_for("the reply's refusal", lambda: [
                m for m in telegram.sent()[before:]
                if "not sent" in m["text"] and "approval.grant" in m["text"]])
            assert fleet.call("ops", "mail.show", {"id": staged["id"]})["state"] == "open"
            telegram.update(callback={"id": "cbp", "from": {"id": CAPTAIN},
                                      "data": keyboard[0][0]["callback_data"],
                                      "message": {"message_id": card["_message_id"],
                                                  "chat": {"id": CHAT}}})
            wait_for("the approval's confirmation", lambda: [
                m for m in telegram.sent()[before:]
                if m["text"].startswith("✅ Approved.")
                and f"privileged run {staged['id']}" in m["text"]])
            assert fleet.call("ops", "mail.show", {"id": staged["id"]})["state"] == "approved"

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

            # A typed decision shows its question, why, what is blocked and each
            # option; one button per option. A tap answers with the option id.
            typed = fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "decision", "body": "",
                "payload": {"decision": {
                    "question": "Ship the release today?",
                    "why": "The build is green but the notes are unreviewed.",
                    "blocked": ["Store submission"],
                    "options": [
                        {"id": "ship", "label": "Ship it", "cost": "Notes unreviewed",
                         "what_happens": "The release goes out now."},
                        {"id": "wait", "label": "Wait for review", "recommended": True,
                         "what_happens": "Nothing ships until review."},
                        {"id": "split", "label": "Ship the core",
                         "what_happens": "Only the core goes out."}]}}})
            typed_card = wait_for("the typed decision card", lambda: [
                m for m in telegram.sent() if "Ship the release today?" in m["text"]])[0]
            for words in ("The build is green", "Store submission", "Notes unreviewed",
                          "Nothing ships until review.", "(recommended)"):
                assert words in typed_card["text"], typed_card["text"]
            keyboard = json.loads(typed_card["reply_markup"])["inline_keyboard"]
            assert [row[0]["text"] for row in keyboard] == [
                "Ship it", "Wait for review", "Ship the core"], keyboard
            telegram.update(callback={"id": "cb2", "from": {"id": CAPTAIN},
                                      "data": keyboard[1][0]["callback_data"],
                                      "message": {"message_id": 1, "chat": {"id": CHAT}}})
            tapped = wait_for("the typed tap's answer", lambda: [
                m for m in fleet.inbox("ops", "ops")
                if m["kind"] == "reply" and m["correlation"] == typed["id"]])[0]
            assert tapped["body"] == "Wait for review", tapped
            assert tapped["payload"] == {"answer": {"option_id": "wait"}}, tapped
            assert fleet.call("ops", "mail.show", {"id": typed["id"]})["state"] == "answered"

            # Free text typed as a reply to the card answers it with text.
            asked = fleet.call("ops", "mail.send", {
                "from": "ops", "to": "captain", "kind": "decision", "body": "",
                "payload": {"decision": {"question": "Which build goes out?", "options": [
                    {"id": "b11", "label": "Build 11", "what_happens": "The older build."},
                    {"id": "b12", "label": "Build 12", "what_happens": "The newer build."}]}}})
            asked_card = wait_for("the text decision card", lambda: [
                m for m in telegram.sent() if "Which build goes out?" in m["text"]])[0]
            telegram.message("Build 12, with the notes fixed.", reply_to=asked_card["_message_id"])
            written = wait_for("the typed text answer", lambda: [
                m for m in fleet.inbox("ops", "ops")
                if m["kind"] == "reply" and m["correlation"] == asked["id"]])[0]
            assert written["payload"] == {"answer": {"text": "Build 12, with the notes fixed."}}, written
            assert fleet.call("ops", "mail.show", {"id": asked["id"]})["state"] == "answered"

            # A tool denial is one card with its buttons. Mail the runtime adds
            # to that ask edits the card, buttons kept, and sends no new message.
            blocked = fleet.call("ops", "permissions.blocked", {
                "task_id": "ops-duty", "tool": "Bash", "input": {"command": "apify run actor/x"}})
            ask = blocked["request_id"]
            block_card = wait_for("the block card", lambda: [
                m for m in telegram.sent() if "was denied Bash(apify run" in m["text"]])[0]
            block_buttons = json.loads(block_card["reply_markup"])["inline_keyboard"]
            assert [row[0]["text"] for row in block_buttons] == [
                "Allow Bash(apify run *)", "Continue without it", "Stop the task"], block_buttons
            before = len(telegram.sent())
            relay = fleet.call("liaison", "mail.send", {
                "from": "liaison", "to": "captain", "kind": "request", "about": ask,
                "body": "ops is blocked on apify; please allow it"})
            assert relay["attached_to"] == ask, relay
            assert f"added to {ask}" in relay["note"], relay
            edit = wait_for("the card edit", lambda: [
                m for m in telegram.called("editMessageText")
                if m.get("message_id") == str(block_card["_message_id"])])[-1]
            assert "liaison (request)" in edit["text"], edit
            assert json.loads(edit["reply_markup"]) == json.loads(block_card["reply_markup"]), edit
            # The same denial on another ship joins the card too, which then
            # says the answer applies to every blocked task.
            same = fleet.call("liaison", "permissions.blocked", {
                "task_id": "liaison-duty", "tool": "Bash", "input": {"command": "apify run actor/y"}})
            assert same["request_id"] != ask, same
            joined = wait_for("the same-denial edit", lambda: [
                m for m in telegram.called("editMessageText")
                if m.get("message_id") == str(block_card["_message_id"])
                and "applies to every blocked task" in m["text"]])[-1]
            assert "liaison-duty" in joined["text"], joined
            assert json.loads(joined["reply_markup"]) == json.loads(block_card["reply_markup"]), joined
            time.sleep(2)
            assert len(telegram.sent()) == before, telegram.sent()[before:]
            # Its closure resolves the same card and clears its buttons; the
            # items added to it end with it and send nothing new.
            fleet.call("ops", "mail.withdraw", {"id": ask, "reason": "done another way"})
            closed = wait_for("the resolved card", lambda: [
                m for m in telegram.called("editMessageText")
                if m.get("message_id") == str(block_card["_message_id"])
                and "Resolved:" in m["text"]])[-1]
            assert json.loads(closed["reply_markup"]) == {"inline_keyboard": []}, closed
            assert len(telegram.sent()) == before, telegram.sent()[before:]

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
            # /digest sends what is held now, as one quiet message.
            telegram.message("/digest")
            digest = wait_for("the digest", lambda: [
                m for m in telegram.sent()[before:] if m["text"].startswith("📋 Away digest")])[0]
            assert "Nightly build done." in digest["text"], digest
            assert digest.get("disable_notification") == "true", digest
            telegram.message("good morning")
            welcome = wait_for("the return catch-up", lambda: [
                m for m in telegram.sent()[before:] if m["text"].startswith("🌅 Welcome back.")])[0]
            assert "1 emergency sent" in welcome["text"], welcome
            assert welcome.get("disable_notification") is None, welcome
            # The digest already told it, and the catch-up was not cut: no
            # liaison summary is spent.
            summary = [m for m in fleet.inbox("liaison", "liaison")
                       if m["body"].startswith("I'm back after")]
            assert not summary, summary
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
