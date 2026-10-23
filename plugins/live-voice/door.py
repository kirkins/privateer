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
- A backlog is never read aloud (see pacing.py): older mail is counted in
  one line, summarised per ship only if the captain asks, and acknowledged;
  live mail goes in a few notices at a time.
- The realtime model can ask the door, through the tools a delegated Codex
  turn sees, to send the captain's words to a ship, to answer a ship's
  question, or to read status, waiting and ships. What is sent is the
  captain's own transcribed speech, read back and sent only after the
  captain's next utterance confirms it by naming the recipient; mail asking
  for an approval is never answered here (see rules.py).
"""

import faulthandler
import json
import os
import queue
import signal
import subprocess
import sys
import threading
import time
import tomllib
from collections import deque

sys.dont_write_bytecode = True  # a release holds only its own files
import pacing
import rules
import voice
import web
from pacing import mail_key

SPOKEN = "spoken.json"
HEARD = "heard.json"
# spoken.json: each ship's highest acknowledged seq, and the keys of mail
# spoken or counted. A key is kept while its mail is unacknowledged; of the
# acknowledged, the newest few stay to catch a repeat under a new seq.
HANDLED_ACKED_KEPT = 200
HANDLED_LIMIT = 1000
THROUGH_LIMIT = 500
# heard.json: the keys of the questions read aloud, newest last.
HEARD_LIMIT = 50
NOTICE_LIMIT = 600
# Backlog streams in a page per ship per second; the count line waits until
# none has arrived for this long, but never longer than the second limit.
BACKLOG_SETTLE = 2.5
BACKLOG_SETTLE_MAX = 10.0
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


def now_ms() -> int:
    return int(time.time() * 1000)


def load_handled() -> tuple[dict, dict]:
    """(through, handled): each ship's highest acknowledged seq, and the
    keys of mail spoken or counted -> (ship, seq or None). Entries of any
    other shape are dropped; the first format, a list of keys, still loads."""
    kept = read_state(SPOKEN, {})
    if isinstance(kept, list):
        kept = {"handled": [[k, None, None] for k in kept[-HANDLED_ACKED_KEPT:]]}
    if not isinstance(kept, dict):
        kept = {}
    through = kept.get("through") if isinstance(kept.get("through"), dict) else {}
    through = {s: q for s, q in through.items() if isinstance(q, int) and not isinstance(q, bool)}
    handled = {}
    for entry in kept.get("handled") or []:
        if isinstance(entry, list) and len(entry) == 3 and isinstance(entry[0], str) \
                and entry[0].startswith("["):
            seq = entry[2] if isinstance(entry[2], int) and not isinstance(entry[2], bool) else None
            handled[entry[0]] = (entry[1] if isinstance(entry[1], str) else None, seq)
    return through, handled


def load_heard() -> list:
    """The keys of questions read aloud; the first format was a dict of mail."""
    kept = read_state(HEARD, [])
    keys = list(kept) if isinstance(kept, (list, dict)) else []
    return [k for k in keys if isinstance(k, str) and k.startswith("[")][-HEARD_LIMIT:]


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
        self.through, self.handled = load_handled()
        self.heard: list = load_heard()
        # Backlog: mail written before this process started, or before the
        # session and older than the age limit, or past the gap limit.
        self.door_ms = now_ms()
        self.session_ms: int | None = None
        self.backlog_age_ms = int(float(cfg.get("backlog_age_secs",
                                                pacing.DEFAULT_BACKLOG_AGE)) * 1000)
        self.gap_limit = int(cfg.get("gap_limit", pacing.DEFAULT_GAP_LIMIT))
        self.max_notices = int(cfg.get("max_notices_per_turn", pacing.DEFAULT_MAX_NOTICES))
        self.pace = float(cfg.get("pace_secs", pacing.DEFAULT_PACE))
        self.backlog_at: float | None = None
        self.backlog_wait: float | None = None
        self.counted = False
        # The summary the last count line offered, and until when.
        self.offer: tuple[list, float] | None = None
        self.reset_pacing()
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
                # A session whose client has not polled for this long is stopped.
                "heartbeat_secs": self.heartbeat,
                # The door's own read-back, so the page shows the recipient
                # whatever the voice said aloud.
                "draft": {"to": draft.to, "text": draft.text,
                          "readback": draft.readback()} if draft else None,
                "transcript": [{"role": r, "text": t} for r, t in self.transcript],
            }

    def start(self, offer) -> str:
        try:
            voice.check_offer(offer)
        except voice.SessionError as error:
            raise web.ApiError(400, "bad_offer", str(error)) from None
        with self.lock:
            if self.session is not None:
                raise web.ApiError(409, "session_running",
                                   "a voice session is already running; stop it first")
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
            raise web.ApiError(502, "session_failed", str(error)) from None
        finally:
            self.record_pid(session)
        with self.lock:
            if self.session is not session:
                # Stopped while starting.
                session.close()
                raise web.ApiError(409, "stopped_while_starting", "stopped while starting")
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
                raise web.ApiError(409, "no_session", "no voice session")
            self.live = True
            self.phase = "live"
            self.seen = time.monotonic()
            session = self.session
        self.post(("connected", session))

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
        self.session_ms = None
        self.offer = None
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

    def on_connected(self, session) -> None:
        """A session went live (main loop): mail older than it is backlog."""
        with self.lock:
            if session is not self.session or not self.live:
                return
            self.session_ms = now_ms()
        self.counted = False
        self.offer = None
        self.backlog_wait = None
        self.reset_pacing()
        self.flush()

    def on_runtime(self, message: dict) -> None:
        if message.get("type") != "mail":
            # Digests are not asked for (the manifest says no); ignore others.
            log(f"ignoring runtime message {message.get('type')!r}")
            return
        if any(event.get("ref") == message.get("ref") for event in self.pending):
            return
        # Even mail spoken before a restart waits its turn: see flush.
        self.pending.append(message)
        if not self.is_handled(message) and self.backlog(message):
            self.backlog_at = time.monotonic()
        self.mark_gaps()
        self.flush()

    # --- backlog and pacing ----------------------------------------------------

    def backlog(self, event: dict) -> bool:
        return bool(event.get("_gap")) or pacing.old(
            event, self.door_ms, self.session_ms, self.backlog_age_ms, now_ms())

    def is_handled(self, event: dict) -> bool:
        """Spoken or counted already, in this process or before a restart."""
        if pacing.event_key(event) in self.handled:
            return True
        seq = pacing.ref_seq(event)
        through = self.through.get(event.get("ship"))
        return seq is not None and through is not None and seq <= through

    def mark_gaps(self) -> None:
        """A ship with more than gap_limit live messages queued here is far
        behind: all of them are backlog. Counted in messages, not seq, since
        a home's seq numbers every mail in it, not only the captain's."""
        queued: dict = {}
        for event in self.pending:
            if not self.is_handled(event) and not self.backlog(event):
                queued.setdefault(event.get("ship"), []).append(event)
        for ship, events in queued.items():
            if len(events) > self.gap_limit:
                for event in events:
                    event["_gap"] = True
                self.backlog_at = time.monotonic()
                log(f"{len(events)} messages queued from one home; counted as backlog")

    def reset_pacing(self) -> None:
        # The open batch: notices it may still take, when it began, and
        # whether the voice has finished a reply since.
        self.batch_left = 0
        self.batch_at: float | None = None
        self.voice_done = False
        # Live mail past a full batch waits for "more"; how many were
        # offered, and when.
        self.held_back = False
        self.more_offered = 0
        self.more_at: float | None = None

    def batch_done(self, now: float) -> bool:
        return self.batch_at is None or self.voice_done or now - self.batch_at >= self.pace

    def open_batch(self, now: float) -> None:
        self.batch_left, self.batch_at, self.voice_done = self.max_notices, now, False
        self.held_back, self.more_offered, self.more_at = False, 0, None

    def may_speak(self, now: float) -> bool:
        """Whether one more live notice may go in now: at most max_notices
        a batch; a finished batch starts a new one unless mail was held
        back from it, which waits for "more"."""
        if self.batch_at is None or (not self.held_back and self.batch_done(now)):
            self.open_batch(now)
        if self.batch_left > 0:
            return True
        self.held_back = True
        return False

    def count_backlog(self, now: float) -> None:
        """Say the one count line for uncounted backlog, once it has stopped
        streaming in; the counted mail is then acknowledged like spoken mail.
        The captain has it on their other channels, and heard the count."""
        uncounted = [e for e in self.pending if not self.is_handled(e) and self.backlog(e)]
        if not uncounted:
            self.backlog_wait = None
            return
        if self.backlog_wait is None:
            self.backlog_wait = now
        if self.backlog_at is not None and now - self.backlog_at < BACKLOG_SETTLE \
                and now - self.backlog_wait < BACKLOG_SETTLE_MAX:
            return
        if not self.speak(pacing.count_line(uncounted, again=self.counted)):
            return
        log(f"counted {len(uncounted)} older messages")
        self.counted = True
        self.backlog_wait = None
        self.offer = (pacing.summary_lines(uncounted),
                      now + float(self.cfg.get("draft_ttl_secs", 60)))
        self.mark_handled(uncounted)

    def flush(self) -> None:
        """Speak queued live mail while a session is live, count backlog,
        and report each `delivered` only after it was spoken or counted (or
        was before a restart). Privateer moves a ship's cursor to the
        highest ref it is told, so no mail is acknowledged while an earlier
        one from its ship still waits. Speaking stops at the first failure."""
        now = time.monotonic()
        if self.live:
            self.count_backlog(now)
        uncounted = any(not self.is_handled(e) and self.backlog(e) for e in self.pending)
        held: set = set()
        can_speak = self.live and not uncounted
        kept = []
        acked = False
        for event in self.pending:
            ship = event.get("ship")
            if ship in held:
                kept.append(event)
                continue
            if not self.is_handled(event):
                spoke = (not self.backlog(event) and can_speak and self.may_speak(now)
                         and self.speak_notice(event))
                if not spoke:
                    if can_speak and not self.backlog(event) and not self.held_back:
                        can_speak = False  # the session refused it
                    held.add(ship)
                    kept.append(event)
                    continue
            self.runtime.delivered(event["ref"])
            self.advance(event)
            acked = True
        self.pending = kept
        if acked:
            self.save_handled()
        if self.live:
            self.offer_more(now)

    def offer_more(self, now: float) -> None:
        """Once a full batch is done, say how much live mail it held back."""
        waiting = sum(1 for e in self.pending if not self.is_handled(e) and not self.backlog(e))
        if not waiting:
            self.held_back, self.more_offered = False, 0
            return
        if not self.held_back or not self.batch_done(now) or waiting <= self.more_offered:
            return
        if self.more_at is not None and now - self.more_at < self.pace:
            return
        if self.speak(pacing.more_line(waiting)):
            self.more_offered, self.more_at = waiting, now

    def speak_notice(self, event: dict) -> bool:
        mail = event.get("mail") or {}
        text = pacing.spoken_notice(event, notice_text(event), bool(rules.approving(mail)))
        if not self.speak(text):
            return False
        self.batch_left -= 1
        self.mark_handled([event])
        if mail.get("kind") in ("request", "decision"):
            key = pacing.event_key(event)
            if key in self.heard:
                self.heard.remove(key)
            self.heard.append(key)
            del self.heard[:-HEARD_LIMIT]
            write_state(HEARD, self.heard)
        return True

    def mark_handled(self, events: list) -> None:
        """Spoken or counted: written down before any is acknowledged, so a
        restart neither speaks nor counts them again."""
        for event in events:
            key = pacing.event_key(event)
            self.handled.pop(key, None)
            self.handled[key] = (event.get("ship"), pacing.ref_seq(event))
        self.save_handled()

    def advance(self, event: dict) -> None:
        """Acknowledged: its ship's acknowledged seq moves up to it."""
        ship, seq = event.get("ship"), pacing.ref_seq(event)
        if isinstance(ship, str) and seq is not None and seq > self.through.get(ship, -1):
            self.through.pop(ship, None)
            self.through[ship] = seq

    def save_handled(self) -> None:
        """Bounded: unacknowledged keys stay; of the rest, the newest few."""
        def acknowledged(entry) -> bool:
            ship, seq = entry
            return seq is None or seq <= self.through.get(ship, -1)
        done = [k for k, entry in self.handled.items() if acknowledged(entry)]
        for key in done[:-HANDLED_ACKED_KEPT]:
            del self.handled[key]
        while len(self.handled) > HANDLED_LIMIT:
            del self.handled[next(iter(self.handled))]
        while len(self.through) > THROUGH_LIMIT:
            del self.through[next(iter(self.through))]
        write_state(SPOKEN, {"through": self.through,
                             "handled": [[k, s, q] for k, (s, q) in self.handled.items()]})

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
            else:
                # The voice finished a reply: the open batch is done.
                self.voice_done = True
                if text:
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
        if outcome == "echo":
            return
        # Any answer to a count line ends its offer: "summary" hears it,
        # "skip", silence or anything else hears nothing more.
        offer, self.offer = self.offer, None
        if outcome == "utterance":
            if offer and time.monotonic() <= offer[1] \
                    and pacing.keyword(text, pacing.SUMMARY_WORDS):
                self.speak("\n".join(offer[0]))
                return
            if self.held_back and pacing.keyword(text, pacing.MORE_WORDS):
                self.open_batch(time.monotonic())
                self.flush()
                return
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


def door_version() -> str:
    """The plugin's own version, from its manifest -> "" when unreadable."""
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "manifest.json"),
                  encoding="utf-8") as f:
            version = json.load(f).get("version")
    except (OSError, ValueError, AttributeError):
        return ""
    return version if isinstance(version, str) else ""


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
                "grace_secs", "backlog_age_secs", "pace_secs"):
        value = cfg.get(key, 1)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or value <= 0:
            return f"{key} must be a positive number"
    for key in ("max_notices_per_turn", "gap_limit"):
        value = cfg.get(key, 1)
        if not isinstance(value, int) or isinstance(value, bool) or value < 1:
            return f"{key} must be a whole number of at least 1"
    origins = cfg.get("allowed_origins", [])
    if not isinstance(origins, list):
        return "allowed_origins must be a list of origins"
    for origin in origins:
        problem = web.origin_problem(origin)
        if problem:
            return f"allowed_origins: {problem}"
    try:
        voice.codex_argv(cfg)
    except voice.SessionError as error:
        return str(error)
    return ""


def command_line(pid: int) -> list:
    """A live process's argv words, from `ps` (Linux and macOS alike; macOS
    has no /proc) -> [] when it is gone or unreadable."""
    try:
        done = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True,
                              text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return []
    return done.stdout.split() if done.returncode == 0 else []


def reap_stale(path: str) -> None:
    """An app-server left by a door that died outright: end its group, but
    only when the recorded pid is still a `codex app-server`."""
    try:
        with open(path, encoding="utf-8") as f:
            pid = int(f.read().strip())
    except (OSError, ValueError):
        return
    if pid > 1 and "app-server" in command_line(pid):
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
    if hasattr(signal, "SIGUSR1"):
        # `kill -USR1` dumps every thread's stack to door.log: a door stuck
        # starting (or anywhere) says where.
        faulthandler.register(signal.SIGUSR1, all_threads=True)
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
        server = web.serve(cfg.get("port", DEFAULT_PORT), pairing, door,
                           cfg.get("allowed_origins", []), door_version())
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
                elif source == "connected":
                    door.on_connected(item)
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
