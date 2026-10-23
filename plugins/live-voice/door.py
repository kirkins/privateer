#!/usr/bin/env python3
"""Live voice front door for Privateer: talk to the fleet from a browser page.

Privateer runs this adapter under `privateer door run NAME` and speaks front
door protocol v1 with it over stdin and stdout (see src/door.rs). The adapter
owns no fleet state; it translates.

- A local page on 127.0.0.1 (see web.py) pairs once, then starts a realtime
  voice session: its WebRTC offer goes to the official `codex app-server`,
  which opens realtime on Codex's own ChatGPT login, and the SDP answer comes
  back. Audio flows between the browser and OpenAI directly (see voice.py).
- Mail to the captain is queued, spoken into the live session as a short
  notice, and only then reported `delivered`, in order per ship. With no
  live session it waits. A session whose page stops checking in (a closed
  tab) is stopped, so mail is never spoken into a session nobody hears.
- The realtime model can ask the door, through the tools a delegated Codex
  turn sees, to send the captain's words to a ship, to answer a ship's
  question, or to read status, waiting and ships. What is sent is the
  captain's own transcribed speech, read back and sent only after the
  captain's next utterance confirms it by naming the recipient; mail asking
  for an approval is never answered here (see rules.py).
"""

import json
import os
import queue
import signal
import sys
import threading
import time
import tomllib
from collections import deque

sys.dont_write_bytecode = True  # a release holds only its own files
import rules
import voice
import web

SPOKEN = "spoken.json"
HEARD = "heard.json"
SPOKEN_LIMIT = 5000
HEARD_LIMIT = 200
NOTICE_LIMIT = 600
TRANSCRIPT_KEPT = 60
APP_SERVER_PID = "app-server.pid"
DEFAULT_PORT = 8765
DEFAULT_HEARTBEAT = 10

SPEAK_PREFIX = "[Privateer door] Say this to the captain, word for word: "
HANDOFF_PREFIX = "[BACKEND] "

INSTRUCTIONS = """You are the captain's voice front door to a Privateer fleet.
Use only the privateer_* tools; do not run commands or read files.
- When the captain wants a ship told something, call privateer_send with the ship's name (or no name for the default ship). Never write the message yourself: the door sends the captain's own words and reads them back first. The captain confirms by saying "send to" and the ship's name.
- When the captain answers a ship's question, call privateer_answer (with the question's id when known).
- For what the fleet is doing, call privateer_status; for what waits on the captain, privateer_waiting; for the ships, privateer_ships.
- The door speaks every tool result to the captain itself. Reply with one short line at most.
- Nothing here approves anything. Approvals happen only on the captain's phone."""

TOOLS = [
    {"type": "function", "name": "privateer_send",
     "description": "Draft a message to a ship from the captain's most recent words, exactly as "
                    "spoken. The door reads it back and sends it only after the captain says "
                    "send to that ship.",
     "inputSchema": {"type": "object", "properties": {
         "to": {"type": "string", "description": "The ship's name; omit for the default ship."}},
         "additionalProperties": False}},
    {"type": "function", "name": "privateer_answer",
     "description": "Draft an answer to a ship's question from the captain's most recent words. "
                    "Read back first; sent only after the captain says send to that ship. "
                    "Never for approvals.",
     "inputSchema": {"type": "object", "properties": {
         "mail_id": {"type": "string",
                     "description": "The question's id; omit for the latest question."}},
         "additionalProperties": False}},
    {"type": "function", "name": "privateer_status",
     "description": "What each ship is doing, from records; wakes no one.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"type": "function", "name": "privateer_waiting",
     "description": "Questions and decisions waiting on the captain.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"type": "function", "name": "privateer_ships",
     "description": "The fleet's ships and what each handles.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
]

STATE_DIR = ""


def log(msg: str) -> None:
    """stderr reaches the supervisor's door.log. Never the captain's words."""
    print(f"{time.strftime('%Y-%m-%d %H:%M:%S')} live-voice: {msg}", file=sys.stderr, flush=True)


def state_path(name: str) -> str:
    return os.path.join(STATE_DIR, name)


def read_state(name: str, default):
    try:
        with open(state_path(name), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def write_state(name: str, obj) -> None:
    web.write_private(state_path(name), json.dumps(obj))


def squash(text, limit: int = NOTICE_LIMIT) -> str:
    text = " ".join(str(text or "").split())
    return text if len(text) <= limit else text[:limit - 1].rstrip() + "…"


class Runtime:
    """The protocol connection to Privateer: events in, requests out."""

    def __init__(self, post, stdin=None, stdout=None):
        self.post = post
        self.stdin = stdin or sys.stdin
        self.stdout = stdout or sys.stdout
        self.lock = threading.Lock()
        self.waiters: dict[str, queue.Queue] = {}
        self.counter = 0
        threading.Thread(target=self._read, name="runtime-reader", daemon=True).start()

    def _read(self) -> None:
        for line in self.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                log(f"runtime sent invalid JSON ({len(line)} bytes)")
                continue
            if message.get("type") == "response":
                waiter = self.waiters.pop(str(message.get("id")), None)
                if waiter:
                    waiter.put(message)
                continue
            self.post(("runtime", message))
        # stdin closed: the supervisor is stopping this door.
        self.post(("eof", None))

    def write(self, message: dict) -> None:
        with self.lock:
            self.stdout.write(json.dumps(message) + "\n")
            self.stdout.flush()

    def call(self, command: str, args: dict | None = None, timeout: float = 30) -> dict:
        """One request; returns its result or raises RuntimeError."""
        if command not in rules.ALLOWED_COMMANDS:
            # Never `permissions` or `approval`: this door changes nothing
            # about what the fleet may do.
            raise RuntimeError(f"the voice door does not send {command}")
        with self.lock:
            self.counter += 1
            ident = f"v{self.counter}"
        waiter: queue.Queue = queue.Queue(maxsize=1)
        self.waiters[ident] = waiter
        self.write({"type": "request", "id": ident, "command": command, "args": args or {}})
        try:
            response = waiter.get(timeout=timeout)
        except queue.Empty:
            self.waiters.pop(ident, None)
            raise RuntimeError(f"Privateer did not answer {command} within {timeout:g}s") from None
        if not response.get("ok"):
            raise RuntimeError(response.get("error") or f"{command} failed")
        return response.get("result") or {}

    def delivered(self, ref: str) -> None:
        self.write({"type": "delivered", "ref": ref})


def notice_text(event: dict) -> str:
    """The short spoken notice for one mail to the captain."""
    mail = event.get("mail") or {}
    sender = mail.get("from") or event.get("ship") or "a ship"
    kind = mail.get("kind", "notice")
    payload = mail.get("payload") if isinstance(mail.get("payload"), dict) else {}
    body = squash(mail.get("body"))
    report = payload.get("report")
    if isinstance(report, dict) and report.get("title"):
        body = squash(f"report: {report['title']}. {body}")
    if rules.approving(mail):
        return f"{sender}: {body} This needs your approval on your phone."
    decision = payload.get("decision") if kind == "decision" else None
    if isinstance(decision, dict):
        labels = [squash(o.get("label"), 60) for o in decision.get("options") or []
                  if isinstance(o, dict)]
        question = squash(decision.get("question") or body)
        return f"{sender} asks you to decide: {question} Options: {', '.join(labels)}."
    if kind in ("request", "decision"):
        return f"{sender} asks: {body}"
    if kind == "reply":
        return f"{sender} replies: {body}"
    return f"{sender} says: {body}"


def short_id(mail_id: str) -> str:
    return str(mail_id or "")[:8]


def mail_key(ship, mail_id) -> str:
    """Mail ids are unique within a ship's home, not across the fleet."""
    return json.dumps([str(ship or ""), str(mail_id or "")])


def load_spoken() -> list:
    """The keys of mail already spoken; entries of any other shape are dropped."""
    kept = read_state(SPOKEN, [])
    return [k for k in kept if isinstance(k, str) and k.startswith("[")] \
        if isinstance(kept, list) else []


class Door:
    """The door's state, shared by the main loop and the page's threads."""

    def __init__(self, hello: dict, cfg: dict, runtime: Runtime, post):
        self.hello = hello
        self.cfg = cfg
        self.runtime = runtime
        self.post = post
        self.lock = threading.RLock()
        self.session: voice.VoiceSession | None = None
        self.phase = "idle"
        self.live = False
        self.conversation: rules.Conversation | None = None
        self.error = ""
        self.pending: list[dict] = []
        self.spoken: list = load_spoken()
        self.spoken_set = set(self.spoken)
        heard = read_state(HEARD, {})
        self.heard: dict = {k: v for k, v in heard.items() if k.startswith("[")} \
            if isinstance(heard, dict) else {}
        # When the page last checked in; a session it stops checking on ends.
        self.seen = time.monotonic()
        self.heartbeat = float(cfg.get("heartbeat_secs", DEFAULT_HEARTBEAT))
        self.transcript: deque = deque(maxlen=TRANSCRIPT_KEPT)
        self.door_turns: set = set()
        self.agent_text: dict = {}

    # --- the page's API (web threads) ----------------------------------------

    def state(self) -> dict:
        with self.lock:
            # The page polls this every second: it is the session's heartbeat.
            self.seen = time.monotonic()
            draft = self.conversation.draft if self.conversation else None
            return {
                "phase": self.phase,
                "live": self.live,
                "error": self.error,
                "queued": len(self.pending),
                "recipient": self.hello.get("recipient") or "",
                # The door's own read-back, so the page shows the recipient
                # whatever the voice said aloud.
                "draft": {"to": draft.to, "text": draft.text,
                          "readback": draft.readback()} if draft else None,
                "transcript": [{"role": r, "text": t} for r, t in self.transcript],
            }

    def start(self, offer) -> str:
        with self.lock:
            if self.session is not None:
                raise web.ApiError(409, "a voice session is already running; stop it first")
            session = voice.VoiceSession(self.cfg, self.post, TOOLS, INSTRUCTIONS,
                                         str(self.cfg.get("voice_prompt", "")))
            self.session = session
            self.phase = "starting"
            self.error = ""
        try:
            answer = session.start(offer)
        except voice.SessionError as error:
            with self.lock:
                if self.session is session:
                    self.session = None
                    self.phase = "idle"
                    self.error = str(error)
            log(f"session did not start: {error}")
            raise web.ApiError(502, str(error)) from None
        finally:
            self.record_pid(session)
        with self.lock:
            if self.session is not session:
                # Stopped while starting.
                session.close()
                raise web.ApiError(409, "stopped while starting")
            self.phase = "answered"
            self.seen = time.monotonic()
            self.conversation = rules.Conversation(float(self.cfg.get("draft_ttl_secs", 60)))
            self.transcript.clear()
            self.door_turns.clear()
            self.agent_text.clear()
        log("session answered")
        return answer

    def connected(self) -> None:
        with self.lock:
            if self.session is None or self.phase not in ("answered", "live"):
                raise web.ApiError(409, "no voice session")
            self.live = True
            self.phase = "live"
            self.seen = time.monotonic()
        self.post(("flush", None))

    def stop(self, error: str = "") -> None:
        with self.lock:
            session = self.session
            self.end_locked(error)
        if session is not None:
            report = session.close()
            log(f"session stopped: {report}")
        self.record_pid(None)

    def end_locked(self, error: str = "") -> None:
        self.session = None
        self.live = False
        self.phase = "idle"
        # Drafts belong to one session: a later "send" cannot confirm them.
        self.conversation = None
        if error:
            self.error = error

    def record_pid(self, session) -> None:
        server = session.server if session is not None else None
        if server is not None and server.proc.poll() is None:
            web.write_private(state_path(APP_SERVER_PID), str(server.proc.pid))
        else:
            try:
                os.unlink(state_path(APP_SERVER_PID))
            except OSError:
                pass

    # --- runtime events (main loop) ------------------------------------------

    def on_runtime(self, message: dict) -> None:
        if message.get("type") != "mail":
            # Digests are not asked for (the manifest says no); ignore others.
            log(f"ignoring runtime message {message.get('type')!r}")
            return
        if any(event.get("ref") == message.get("ref") for event in self.pending):
            return
        # Even mail spoken before a restart waits its turn: see flush.
        self.pending.append(message)
        self.flush()

    def flush(self) -> None:
        """Speak queued mail in order while a session is live, and report
        each `delivered` only after it was spoken (or was spoken before a
        restart). Privateer moves a ship's cursor to the highest ref it is
        told, so no mail is acknowledged while an earlier one from its ship
        still waits. Speaking stops at the first failure."""
        held: set = set()
        can_speak = self.live
        kept = []
        for event in self.pending:
            ship = event.get("ship")
            mail = event.get("mail") or {}
            key = mail_key(ship, mail.get("id"))
            if ship in held:
                kept.append(event)
                continue
            if key not in self.spoken_set:
                if not can_speak or not self.speak(notice_text(event)):
                    can_speak = False
                    held.add(ship)
                    kept.append(event)
                    continue
                self.remember(key, ship, mail)
            self.runtime.delivered(event["ref"])
        self.pending = kept

    def remember(self, key: str, ship, mail: dict) -> None:
        self.spoken.append(key)
        self.spoken_set.add(key)
        if len(self.spoken) > SPOKEN_LIMIT:
            for old in self.spoken[:-SPOKEN_LIMIT]:
                self.spoken_set.discard(old)
            del self.spoken[:-SPOKEN_LIMIT]
        write_state(SPOKEN, self.spoken)
        if mail.get("kind") in ("request", "decision"):
            self.heard[key] = {"ship": ship, "mail": mail}
            while len(self.heard) > HEARD_LIMIT:
                self.heard.pop(next(iter(self.heard)))
            write_state(HEARD, self.heard)

    def speak(self, text: str) -> bool:
        """Put the door's own words into the live session -> whether it took."""
        with self.lock:
            session, conversation = self.session, self.conversation
        if session is None or conversation is None:
            return False
        speech = self.cfg.get("speak_with", "text") == "speech"
        framed = text if speech else SPEAK_PREFIX + text
        conversation.door_said(text)
        conversation.door_said(framed)
        try:
            session.append(framed, role=str(self.cfg.get("speak_role", "user")), speech=speech)
        except voice.SessionError as error:
            log(f"could not speak into the session: {error}")
            return False
        with self.lock:
            self.transcript.append(("door", text))
        return True

    # --- voice events (main loop) --------------------------------------------

    def on_voice(self, session, message) -> None:
        with self.lock:
            current = session is self.session
        if message is None or message.get("method") in ("thread/realtime/closed",):
            if current:
                why = session.failure or "the voice session ended"
                with self.lock:
                    self.end_locked(why if message is None or session.failure else "")
                log(f"session ended: {why}")
                session.close()
                self.record_pid(None)
            return
        method = message.get("method")
        if "id" in message:
            self.on_request(session, current, message)
            return
        if not current:
            return
        params = message.get("params") or {}
        if method == "thread/realtime/error":
            with self.lock:
                self.error = voice.redact(params.get("message", ""))
        elif method == "thread/realtime/transcript/done":
            text = str(params.get("text") or "").strip()
            if params.get("role") == "user":
                self.on_utterance(text)
            elif text:
                with self.lock:
                    self.transcript.append(("voice", text))
        elif method == "item/completed":
            item = params.get("item") or {}
            if item.get("type") == "agentMessage" and isinstance(item.get("text"), str):
                self.agent_text[params.get("turnId")] = item["text"]
        elif method == "turn/completed":
            turn = (params.get("turn") or {}).get("id")
            text = self.agent_text.pop(turn, "")
            if turn in self.door_turns:
                # The door already spoke this turn's tool results itself.
                self.door_turns.discard(turn)
            elif text.strip():
                # Client-managed handoff: the delegated turn's answer goes
                # back to the voice only through this explicit append.
                try:
                    session.append(HANDOFF_PREFIX + text, role="user")
                    with self.lock:
                        if self.conversation:
                            self.conversation.door_said(HANDOFF_PREFIX + text)
                except voice.SessionError as error:
                    log(f"handoff not appended: {error}")

    def on_request(self, session, current: bool, message: dict) -> None:
        """A server request: the door's own tools are answered, everything
        else (command and file approvals, token refreshes) is declined."""
        method = message.get("method")
        ident = message["id"]
        params = message.get("params") or {}
        server = session.server
        if server is None:
            return
        try:
            if method != "item/tool/call" or not current:
                server.decline(ident, "declined by the live voice door")
                return
            self.door_turns.add(params.get("turnId"))
            ok, text = self.tool(str(params.get("tool", "")), params.get("arguments"))
            server.respond(ident, {
                "contentItems": [{"type": "inputText",
                                  "text": f"The door told the captain: {text}"}],
                "success": ok,
            })
        except OSError:
            return
        self.speak(text)

    def tool(self, name: str, arguments) -> tuple[bool, str]:
        if isinstance(arguments, str):
            try:
                arguments = json.loads(arguments)
            except ValueError:
                arguments = {}
        args = arguments if isinstance(arguments, dict) else {}
        try:
            if name == "privateer_send":
                return True, self.draft_send(args.get("to"))
            if name == "privateer_answer":
                return True, self.draft_answer(args.get("mail_id"))
            if name == "privateer_status":
                return True, self.status_text()
            if name == "privateer_waiting":
                return True, self.waiting_text()
            if name == "privateer_ships":
                return True, self.ships_text()
            return False, f"there is no tool named {name}"
        except rules.Refused as refused:
            return False, str(refused)
        except RuntimeError as error:
            return False, f"Privateer did not answer: {voice.redact(error)}"

    # --- drafts, confirmation and sending ------------------------------------

    def ship_named(self, to) -> str:
        ships = self.runtime.call("ships").get("ships") or []
        names = [s.get("name") for s in ships if isinstance(s, dict) and s.get("name")]
        if not to:
            recipient = self.hello.get("recipient")
            if not recipient:
                raise rules.Refused("there is no default ship; name the ship")
            return recipient
        spoken = rules.normalize(to)
        for name in names:
            if rules.normalize(name) == spoken:
                return name
        raise rules.Refused(f"there is no ship named {to}; the ships are {', '.join(names)}")

    def draft_send(self, to) -> str:
        ship = self.ship_named(to)
        with self.lock:
            if self.conversation is None:
                raise rules.Refused("no live voice session")
            draft = self.conversation.propose(ship)
        return draft.readback()

    def waiting_items(self) -> list:
        listed = self.runtime.call("waiting")
        return [i for i in listed.get("waiting") or []
                if isinstance(i, dict) and isinstance(i.get("mail"), dict)]

    def draft_answer(self, mail_id) -> str:
        waiting = self.waiting_items()
        target = None
        if mail_id:
            wanted = str(mail_id).strip()
            matches = [i for i in waiting if str(i["mail"].get("id", "")).startswith(wanted)]
            if len(matches) != 1:
                raise rules.Refused("I cannot find that question among those waiting on you")
            target = matches[0]
        else:
            # The latest question this door spoke that is still waiting.
            open_ids = {mail_key(i.get("ship"), i["mail"].get("id")): i
                        for i in waiting}
            for known in reversed(list(self.heard)):
                if known in open_ids:
                    target = open_ids[known]
                    break
            if target is None:
                raise rules.Refused("no question I read to you is still waiting; name its id")
        mail = dict(target["mail"])
        sender = mail.get("from") or target.get("ship")
        with self.lock:
            if self.conversation is None:
                raise rules.Refused("no live voice session")
            draft = self.conversation.propose(sender, target=mail)
        return draft.readback()

    def on_utterance(self, text: str) -> None:
        with self.lock:
            conversation = self.conversation
            if conversation is None:
                return
            outcome, draft = conversation.heard(text)
            if outcome != "echo":
                self.transcript.append(("captain", text))
        if outcome == "confirm":
            self.send_draft(draft)
        elif outcome == "unnamed":
            self.speak(f"Say send to {draft.to} to send it.")
        elif outcome == "wrong ship":
            self.speak(f"That message is for {draft.to}, not the ship you named; "
                       "nothing was sent.")
        elif outcome == "cancelled":
            self.speak(f"Not sent to {draft.to}.")
        elif outcome == "dropped":
            self.speak(f"The message to {draft.to} was not sent.")
        elif outcome == "expired":
            self.speak(f"The message to {draft.to} expired; nothing was sent.")

    def send_draft(self, draft: rules.Draft) -> None:
        args = draft.args()
        try:
            rules.check_send(args, draft.target)
            result = self.runtime.call("send", args)
        except rules.Refused as refused:
            self.speak(str(refused))
            return
        except RuntimeError as error:
            # Kept with its mail id, so "send" again is the same message.
            with self.lock:
                if self.conversation is not None:
                    draft.after = self.conversation.seq
                    draft.created = self.conversation.clock()
                    self.conversation.draft = draft
            self.speak(f"Not sent: {voice.redact(error)}. Say send to {draft.to} to try again.")
            return
        log(f"captain -> {result.get('ship', draft.to)}: {len(draft.text)} chars ({args['kind']})")
        self.speak(f"Sent to {result.get('ship', draft.to)}.")

    def tick(self) -> None:
        with self.lock:
            gone = (self.session is not None and self.phase in ("answered", "live")
                    and time.monotonic() - self.seen > self.heartbeat)
        if gone:
            # A closed tab: nobody hears this session, but it runs on the
            # captain's plan and mail would be spoken into it. Queued mail
            # stays unacknowledged.
            log("the page stopped checking in; stopping the session")
            self.stop("the page stopped checking in, so the session was stopped")
        with self.lock:
            draft = self.conversation.expire() if self.conversation else None
        if draft is not None:
            self.speak(f"The message to {draft.to} expired; nothing was sent.")
        if self.pending and self.live:
            self.flush()

    # --- records ---------------------------------------------------------------

    def status_text(self) -> str:
        lines = []
        for ship in self.runtime.call("status").get("ships") or []:
            name = ship.get("name", "?")
            if ship.get("error"):
                lines.append(f"{name}: cannot read its records.")
                continue
            state = ("the captain is aboard" if ship.get("attended")
                     else "working" if ship.get("working")
                     else "idle" if ship.get("dispatcher") else "its dispatcher is stopped")
            bits = [state, f"{ship.get('open_tasks', 0)} open tasks"]
            if ship.get("waiting_on_captain"):
                bits.append(f"{ship['waiting_on_captain']} waiting on you")
            lines.append(f"{name}: " + ", ".join(bits) + ".")
        return " ".join(lines) or "No ships."

    def waiting_text(self) -> str:
        waiting = self.waiting_items()
        if not waiting:
            return "Nothing is waiting on you."
        lines = [f"{len(waiting)} waiting on you."]
        for item in waiting[:8]:
            mail = item["mail"]
            sender = mail.get("from") or item.get("ship")
            what = squash(mail.get("body"), 160)
            if rules.approving(mail):
                lines.append(f"{sender}, id {short_id(mail.get('id'))}: {what} "
                             "This needs your approval on your phone.")
            else:
                lines.append(f"{sender}, id {short_id(mail.get('id'))}: {what}")
        return " ".join(lines)

    def ships_text(self) -> str:
        result = self.runtime.call("ships")
        parts = []
        for ship in result.get("ships") or []:
            tag = ", the liaison" if ship.get("liaison") else ""
            scope = squash(ship.get("scope") or "no scope set", 120)
            parts.append(f"{ship.get('name')}{tag}: {scope}.")
        default = result.get("recipient") or self.hello.get("recipient") or "nobody"
        return " ".join(parts) + f" Plain messages go to {default}."


def load_config(path) -> dict:
    if not path:
        return {}
    with open(path, "rb") as f:
        return tomllib.load(f)


def config_problem(cfg: dict) -> str:
    port = cfg.get("port", DEFAULT_PORT)
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        return "port must be a number from 1 to 65535"
    if cfg.get("isolate_codex", True) is not True:
        return "isolate_codex was removed: delegated Codex turns never get an environment"
    if cfg.get("speak_with", "text") not in ("text", "speech"):
        return 'speak_with must be "text" or "speech"'
    if cfg.get("speak_role", "user") not in ("user", "developer"):
        return 'speak_role must be "user" or "developer"'
    for key in ("draft_ttl_secs", "heartbeat_secs", "start_timeout_secs", "rpc_timeout_secs",
                "grace_secs"):
        value = cfg.get(key, 1)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or value <= 0:
            return f"{key} must be a positive number"
    try:
        voice.codex_argv(cfg)
    except voice.SessionError as error:
        return str(error)
    return ""


def reap_stale(path: str) -> None:
    """An app-server left by a door that died outright: end its group, but
    only when the recorded pid is still a `codex app-server` (Linux /proc)."""
    try:
        with open(path, encoding="utf-8") as f:
            pid = int(f.read().strip())
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            argv = f.read().split(b"\0")
    except (OSError, ValueError):
        return
    if b"app-server" in argv:
        try:
            os.killpg(pid, signal.SIGKILL)
            log(f"ended a stale app-server group {pid}")
        except OSError:
            pass
    try:
        os.unlink(path)
    except OSError:
        pass


def main() -> int:
    global STATE_DIR
    hello_line = sys.stdin.readline()
    if not hello_line:
        return 0
    hello = json.loads(hello_line)
    if hello.get("type") != "hello" or hello.get("version") != 1:
        print(f"unsupported front door protocol: {hello_line[:200]}", file=sys.stderr)
        return 2
    STATE_DIR = hello["state_dir"]
    os.makedirs(STATE_DIR, mode=0o700, exist_ok=True)
    try:
        cfg = load_config(hello.get("config_path"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        print(f"live voice door config: {error}", file=sys.stderr)
        return 2
    problem = config_problem(cfg)
    if problem:
        print(f"live voice door config: {problem}", file=sys.stderr)
        return 2
    reap_stale(state_path(APP_SERVER_PID))

    events: queue.Queue = queue.Queue()
    runtime = Runtime(events.put)
    door = Door(hello, cfg, runtime, events.put)
    pairing = web.Pairing(STATE_DIR)
    try:
        server = web.serve(cfg.get("port", DEFAULT_PORT), pairing, door)
    except OSError as error:
        print(f"live voice door: cannot listen on 127.0.0.1:{cfg.get('port', DEFAULT_PORT)}: "
              f"{error}", file=sys.stderr)
        return 2
    port = server.server_address[1]
    web.write_private(state_path("url"), f"http://127.0.0.1:{port}/\n")
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, lambda *_: events.put(("eof", None)))
    log(f"door {hello.get('door')} up at http://127.0.0.1:{port}/; the pairing code is in "
        f"{state_path(web.CODE_FILE)}")
    try:
        while True:
            try:
                source, item = events.get(timeout=1)
            except queue.Empty:
                source, item = None, None
            try:
                if source == "eof":
                    break
                if source == "runtime":
                    door.on_runtime(item)
                elif source == "voice":
                    door.on_voice(*item)
                elif source == "flush":
                    door.flush()
                door.tick()
            except BrokenPipeError:
                break
            except Exception as e:
                log(f"handler error: {e!r}")
    finally:
        door.stop()
        server.shutdown()
    log("door stopping")
    return 0


if __name__ == "__main__":
    sys.exit(main())
