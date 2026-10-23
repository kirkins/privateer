"""The live voice door's hard rules, as plain functions the door calls.

The realtime model is told what to do, but nothing here trusts it:

- Exact words: what goes to a ship is the captain's own transcribed speech,
  never the model's wording, filler and all. A draft is read back exactly
  and sent only when the captain's next utterance is a plain confirmation
  that names its recipient ("send to ops"), so a voice that misreads the
  recipient aloud cannot steer the send. The message said again becomes the
  draft; a cancel ("cancel", "actually forget it") or anything else drops
  it, and an unconfirmed draft expires.
- Never approve: this door never answers mail that asks for an approval
  (`payload.approves`, a decision with an approving option, a production or
  privileged request, a tool permission ask), never answers with an option
  id, and never runs a command outside the four it needs.
"""

import re
import secrets
import time
from collections import deque

# The only runtime commands this door ever sends. Not `permissions`, not
# `approval`, not `dismiss`: those change what the fleet may do.
ALLOWED_COMMANDS = ("send", "ships", "status", "waiting")

# Request kinds only the captain's own hands may answer.
APPROVAL_KINDS = ("production_approval", "privileged_action")

# A confirmation is the whole utterance, normalized: one of these, then the
# recipient's name, and nothing else ("send to ops").
CONFIRM_LEADS = (
    "send to", "send it to", "send that to", "send this to", "yes send to",
    "yes send it to", "confirm send to",
)
# A confirmation that names no one confirms nothing; the draft waits for one
# that does.
UNNAMED = frozenset({
    "send", "send it", "yes send", "yes send it", "send now", "send that",
    "confirm", "confirmed", "yes confirm", "yes",
})
CANCEL = frozenset({
    "cancel", "cancel it", "cancel that", "dont send", "do not send", "dont send it",
    "do not send it", "dont send that", "do not send that", "dont send this",
    "do not send this", "no", "no dont send", "stop", "never mind", "nevermind",
    "never mind that", "never mind it", "nevermind that", "forget it", "forget that",
    "forget about it", "scrap it", "scrap that", "scratch that", "drop it", "drop that",
})
# Words around a cancel that keep it a cancel: "actually forget it", "oh never
# mind that", "just scrap it please". Only the classification strips them; a
# message keeps every word.
CANCEL_FILLER = frozenset({
    "actually", "ah", "hmm", "just", "no", "oh", "ok", "okay", "please", "so", "then", "uh",
    "um", "wait",
})

# A later utterance is the message said again, and replaces the draft, when
# it has at least this many words and none of these: a near-miss
# confirmation ("yes please send it") must never become the message.
REPLACE_MIN_WORDS = 3
CONTROL_WORDS = frozenset({"send", "sent", "confirm", "confirmed", "cancel", "yes"})

UTTERANCE_LIMIT = 4000
UTTERANCES_KEPT = 20
ECHOES_KEPT = 50
# Client notices whose words may still be the screen's: one goes in each 2 s,
# and a long one stays the screen's for most of a minute.
SCREEN_KEPT = 30
# How long a notice's words stay the screen's, past the time it takes to
# say them aloud at about this many words a second.
DEFAULT_SCREEN_SECS = 6.0
SPOKEN_WORDS_PER_SEC = 2.5
SENT_KEPT = 20


class Refused(Exception):
    """A rule stopped this; the message says why, in words for the captain."""


def normalize(text: str) -> str:
    """Lowercase words only: "Send it." and "send it" are the same utterance."""
    text = str(text).lower().replace("’", "").replace("'", "")
    return " ".join(re.sub(r"[^a-z0-9]+", " ", text).split())


def approving(mail: dict) -> str:
    """Why this mail asks the captain for an approval, or "" when it does not.

    Anything a reply could turn into a recorded approval counts, so a plain
    "yes" spoken to it is never sent."""
    if not isinstance(mail, dict):
        return "it is not a message this door can read"
    payload = mail.get("payload")
    payload = payload if isinstance(payload, dict) else {}
    if "approves" in payload:
        return "it asks for your approval"
    if mail.get("request_kind") in APPROVAL_KINDS:
        return f"it is a {mail['request_kind'].replace('_', ' ')} request"
    if payload.get("privileged") is not None:
        return "it is a privileged command"
    if payload.get("type") == "tool_permission":
        return "it asks for a tool permission"
    decision = payload.get("decision")
    if isinstance(decision, dict):
        for option in decision.get("options") or []:
            if isinstance(option, dict) and option.get("approves"):
                return "one of its options approves"
    return ""


def answerable(mail: dict) -> str:
    """Why this door cannot answer this mail, or "" when it can."""
    why = approving(mail)
    if why:
        return why
    if mail.get("kind") not in ("request", "decision"):
        return "it is not a question"
    if not mail.get("id") or not (mail.get("from") or mail.get("ship")):
        return "it has no id or sender"
    return ""


def check_send(args: dict, target: dict | None) -> None:
    """The last gate before `send`: raises Refused for anything that could
    approve. `target` is the mail a reply answers, None for a new message."""
    payload = args.get("payload")
    if isinstance(payload, dict):
        answer = payload.get("answer")
        if isinstance(answer, dict) and "option_id" in answer:
            raise Refused("this door never answers with an option")
        if "approves" in payload or "approval" in payload:
            raise Refused("this door never sends an approval")
    if args.get("kind") not in (None, "request", "reply"):
        raise Refused("this door sends requests and replies only")
    if target is None:
        if args.get("kind") == "reply" or args.get("correlation"):
            raise Refused("a reply needs the message it answers")
        return
    why = answerable(target)
    if why:
        raise Refused(f"not sent: {why}; this needs your approval on your phone"
                      if approving(target) else f"not sent: {why}")
    if args.get("correlation") != target.get("id"):
        raise Refused("a reply must answer the message it names")


def cancels(words: str) -> bool:
    """A normalized utterance that cancels the draft, filler around it and all."""
    if words in CANCEL:
        return True
    said = words.split()
    while said and said[0] in CANCEL_FILLER:
        said.pop(0)
    while said and said[-1] in CANCEL_FILLER:
        said.pop()
    return " ".join(said) in CANCEL


def controls(words: str) -> bool:
    """A normalized utterance that confirms, names no one, or cancels: never
    a message."""
    return cancels(words) or words in UNNAMED or confirmed_ship(words) is not None


def said_again(words: str) -> bool:
    """A normalized utterance that replaces a waiting draft."""
    said = words.split()
    return len(said) >= REPLACE_MIN_WORDS and not CONTROL_WORDS & set(said)


def spoken_id(mail_id) -> str:
    """A mail id short enough to say aloud."""
    return str(mail_id or "").removeprefix("voice-")[:6]


def confirmed_ship(words: str) -> str | None:
    """The recipient a normalized confirmation names, or None if it is not one."""
    for lead in CONFIRM_LEADS:
        if words.startswith(lead + " ") and words[len(lead) + 1:]:
            return words[len(lead) + 1:]
    return None


class Draft:
    def __init__(self, to: str, text: str, after: int, created: float, target: dict | None):
        self.to = to
        self.text = text
        # The seq of the last utterance the draft has seen; only a later one confirms.
        self.after = after
        self.created = created
        self.target = target
        # The mail id the send uses, so a retried send is the same message.
        # It also names the draft to a client (/api/draft/confirm).
        self.mail_id = "voice-" + secrets.token_hex(12)
        # Taken by the one path that sends it: a spoken confirmation or a click.
        self.claimed = False
        # The id the runtime recorded the send under, once sent.
        self.sent_id: str | None = None
        # The seq of a bare "send" it was kept through, already answered.
        self.nudged = -1

    def readback(self) -> str:
        """The whole utterance goes as said, filler included, so the read-back
        says so rather than trimming it."""
        verb = f"answer {self.to}" if self.target else f"send {self.to}"
        return (f"I will {verb} exactly: '{self.text}'. "
                f"Say send to {self.to} to confirm, or say it again.")

    def args(self) -> dict:
        """The runtime `send` args: the captain's exact words, nothing else."""
        if self.target is None:
            return {"id": self.mail_id, "to": self.to, "kind": "request", "body": self.text}
        args = {"id": self.mail_id, "to": self.to, "kind": "reply",
                "correlation": self.target["id"], "body": self.text}
        payload = self.target.get("payload")
        if self.target.get("kind") == "decision" and isinstance(payload, dict) \
                and isinstance(payload.get("decision"), dict):
            # Free text on a typed decision; never an option id.
            args["payload"] = {"answer": {"text": self.text[:4096]}}
        return args


class Conversation:
    """One live session: the captain's utterances and at most one draft."""

    def __init__(self, ttl: float, clock=time.monotonic, screen_secs: float = DEFAULT_SCREEN_SECS):
        self.ttl = ttl
        self.clock = clock
        self.screen_secs = screen_secs
        self.utterances: deque = deque(maxlen=UTTERANCES_KEPT)
        self.seq = 0
        self.draft: Draft | None = None
        # What the door itself put into the session, so it is never heard
        # back as the captain.
        self.echoes: deque = deque(maxlen=ECHOES_KEPT)
        # Only non-client door lines: unlike screen notices, reading these
        # aloud must not renew a screen window, even if they quote a notice.
        self.door_lines: deque = deque(maxlen=ECHOES_KEPT)
        # Client notices said into the session: [until, framed words,
        # sentences]. Heard back before `until`, any run of their words is
        # the screen's, never the captain's.
        self.screen: deque = deque(maxlen=SCREEN_KEPT)
        # The utterances already sent, so a retried draft cannot send them again.
        self.sent: deque = deque(maxlen=SENT_KEPT)
        # The draft that last left, and how: ("sent" | "expired" | ..., draft).
        self.last: tuple[str, Draft] | None = None

    def door_said(self, text: str, client: bool = False) -> None:
        words = normalize(text)
        self.echoes.append(words)
        if not client:
            self.door_lines.append(words)

    def screen_said(self, framed: str, text: str) -> None:
        """A client notice went into the session as `framed`: its bare
        `text`, every sentence of it and any run of its words are the
        screen's while it may still be heard back, which is screen_secs
        past the time it takes to say aloud."""
        words = normalize(framed)
        sentences = {normalize(s) for s in re.split(r"[.!?;:\n]+", text)} | {normalize(text)}
        spoken = len(normalize(text).split()) / SPOKEN_WORDS_PER_SEC
        until = self.clock() + self.screen_secs + spoken
        self.screen.append([until, words, sentences - {""}])

    def voice_said(self, text: str) -> None:
        """The voice finished a reply: a notice it read out, even in part,
        stays the screen's for screen_secs more, as its echo may still come."""
        words = normalize(text)
        # Our own read-backs and refusals may quote a notice's sentence.
        # Reading those out is not another reading of the screen notice.
        said, now = f" {words} ", self.clock()
        # Remove whole door lines, not just exact transcript matches: the
        # voice may add filler or read a notice alongside a door line.
        for line in sorted(set(self.door_lines), key=len, reverse=True):
            if line:
                said = said.replace(f" {line} ", " ")
        for entry in self.screen:
            if now < entry[0] and any(f" {s} " in said for s in entry[2]):
                entry[0] = max(entry[0], now + self.screen_secs)

    def from_screen(self, words: str) -> bool:
        """Whether normalized `words` were a client notice's, heard back."""
        now = self.clock()
        return any(now < until and f" {words} " in f" {framed} "
                   for until, framed, _ in self.screen)

    def heard(self, text: str) -> tuple[str, Draft | None]:
        """A finished captain utterance -> (what happened, the draft it settled).

        "confirm": send the draft; "unnamed": a confirmation without the
        recipient, the draft still waits; "replaced": the message said
        again, now the draft (returned) for the same ship; "cancelled",
        "dropped", "wrong ship" or "expired": it is gone unsent; "echo": the
        door's own words, ignored; "screen": a client notice's words heard
        back, never the captain's, so they confirm, cancel and replace
        nothing (the draft, returned, still waits); "utterance": noted."""
        words = normalize(text)
        if not words or words in self.echoes:
            return "echo", None
        if self.from_screen(words):
            return "screen", self.draft
        self.seq += 1
        self.utterances.append((self.seq, str(text)[:UTTERANCE_LIMIT].strip()))
        draft = self.draft
        if draft is None or self.seq <= draft.after:
            return "utterance", None
        self.draft = None
        if self.clock() - draft.created > self.ttl:
            self.last = ("expired", draft)
            return "expired", draft
        if cancels(words):
            self.last = ("cancelled", draft)
            return "cancelled", draft
        named = confirmed_ship(words)
        if named is not None:
            if named == normalize(draft.to):
                return "confirm", draft
            self.last = ("wrong ship", draft)
            return "wrong ship", draft
        if words in UNNAMED:
            # Kept, unchanged: only a later utterance naming the recipient sends it.
            draft.after = draft.nudged = self.seq
            self.draft = draft
            return "unnamed", draft
        if said_again(words):
            # The captain said the message again: these words are the message
            # now, for the same ship, and are read back like any draft.
            self.draft = Draft(draft.to, self.utterances[-1][1], self.seq, self.clock(),
                               draft.target)
            return "replaced", self.draft
        self.last = ("dropped", draft)
        return "dropped", draft

    def claim(self, draft: Draft) -> bool:
        """Take `draft` for sending -> whether this caller may send it. One
        draft sends at most once, whichever path confirms it."""
        if draft.claimed:
            return False
        draft.claimed = True
        if self.draft is draft:
            self.draft = None
        return True

    def settled(self, draft: Draft, how: str, sent_id: str | None = None) -> None:
        """A claimed draft's send ended: "sent" (under the runtime's
        `sent_id`), "refused" (gone), or "failed" (it waits again, same mail
        id, for another confirmation)."""
        if how in ("sent", "refused"):
            if how == "sent":
                draft.sent_id = sent_id or draft.mail_id
                self.sent.append(draft.after)
            self.last = (how, draft)
            return
        draft.claimed = False
        draft.after = self.seq
        draft.created = self.clock()
        self.draft = draft

    def latest(self) -> tuple[int, str] | None:
        return self.utterances[-1] if self.utterances else None

    def propose(self, to: str, target: dict | None = None) -> Draft:
        """A draft of the captain's latest words to `to`; raises Refused."""
        if target is not None:
            why = answerable(target)
            if why:
                raise Refused(f"{why}; this needs your approval on your phone"
                              if approving(target) else why)
        latest = self.latest()
        if latest is None:
            raise Refused("I have not heard your words yet; say the message, then ask me to send it")
        seq, text = latest
        current = self.draft
        same_target = (current.target or {}).get("id") == (target or {}).get("id") if current else False
        if current and current.after == seq and current.to == to and same_target:
            return current
        if controls(normalize(text)):
            # "send to ops" is a confirmation for the door, never a message.
            raise Refused("your last words were a confirmation, not a message; say the message, "
                          "then ask me to send it")
        if seq in self.sent:
            raise Refused("those words were already sent; say a new message to send another")
        self.draft = Draft(to, text, seq, self.clock(), target)
        return self.draft

    def expire(self) -> Draft | None:
        """The draft, if it just ran out of time; it is dropped."""
        if self.draft and self.clock() - self.draft.created > self.ttl:
            draft, self.draft = self.draft, None
            self.last = ("expired", draft)
            return draft
        return None
