#!/usr/bin/env python3
"""Telegram front door for Privateer: the captain's conversation on a phone.

Privateer runs this adapter under `privateer door run NAME` and speaks front
door protocol v1 with it over stdin and stdout (see src/door.rs). The adapter
owns no fleet state; it translates.

- Mail to the captain from any ship arrives as a `mail` event and is pushed to
  the configured chat, then reported `delivered`.
- A plain message from the chat becomes captain mail to the default recipient
  (the liaison) or to the ship chosen with /to.
- Replying to a pushed message answers it: a request or decision gets the
  captain's reply; a notice or reply gets a follow-up request to its sender.
- A typed decision (payload.decision) shows its question, why, what is blocked
  and each option. A button answers with its option id; a typed reply to the
  card answers with free text.
- A report (payload.report) shows its title and that it is in the portal; its
  content, inline or stored, is never sent to the chat.
- /status, /ships and /waiting answer from records, waking no one.

Telegram is the trust boundary: only the configured chat, and when set only
allowed_user_ids within it, is heard. Everything else is logged and dropped.
"""

import json
import os
import queue
import re
import sys
import threading
import time

import tg

POLL_SECONDS = 50
TEXT_LIMIT = 3500           # Telegram caps a message at 4096; leave room for the header
RETRY_SECONDS = 15
SWEEP_EVERY = 6 * 3600
SENT_LIMIT = 5000

ICONS = {"request": "❓", "decision": "⚖️", "reply": "↩️", "notice": "📣"}

HELP = (
    "Talk to your fleet. A plain message goes to {recipient}.\n"
    "Reply to any message from a ship to answer it, or to follow up with that ship.\n\n"
    "/to SHIP [text] — send to a ship (/to alone shows ships; /to default resets)\n"
    "/status — what each ship is doing, from records (wakes no one)\n"
    "/ships — the fleet's ships and what they handle\n"
    "/waiting — questions and decisions still waiting on you\n"
    "/dismiss ID [reason] — close one without answering (the first characters of its id are enough)\n"
    "/yolo on|off|status — tool approval mode for every first mate\n"
    "/stow [status|resume] [machine] — checkpoint this machine and hold new jobs, e.g. before an update; a linked machine's name stows that machine, and `/stow resume all` lifts every linked machine first\n"
    "/afk [until 8am] [instructions] — away: hold mail, only emergencies 🚨 get through\n"
    "/back — back from away (any ordinary message also brings you back)\n"
    "/held — what was held while you were away\n"
    "/digest — send what is held now; /afk every 30m sets how often\n"
    "/voice [on|off] — speak messages as voice notes (needs [tts])\n"
    "/help — this message\n\n"
    "Photos and files are handed to the ship itself, on whichever machine it runs. "
    "Voice notes are transcribed when [stt] is configured."
)

COMMANDS = [
    {"command": "to", "description": "Send to a ship, or pick one"},
    {"command": "status", "description": "What the fleet is doing"},
    {"command": "yolo", "description": "Fleet tool approvals: on, off or status"},
    {"command": "stow", "description": "Checkpoint this machine: stow, status or resume"},
    {"command": "afk", "description": "Away: hold mail, emergencies only"},
    {"command": "back", "description": "Back from away, with a summary"},
    {"command": "held", "description": "What was held while you were away"},
    {"command": "digest", "description": "Send the away digest now"},
    {"command": "ships", "description": "The fleet's ships"},
    {"command": "waiting", "description": "What is waiting on you"},
    {"command": "dismiss", "description": "Close one item without answering"},
    {"command": "voice", "description": "Toggle spoken messages"},
    {"command": "help", "description": "How this works"},
]

OFFSET = "offset.json"
SENT = "sent.json"
OUTGOING = "outgoing.json"
TARGET = "target.json"
DECISIONS = "decisions.json"
# Decision cards the door showed, by mail id: their message, text and buttons,
# so later mail on the same ask edits the card instead of adding a message.
CARDS = "cards.json"
CARD_LIMIT = 200


class Runtime:
    """The protocol connection to Privateer: events in, requests out."""

    def __init__(self, events: queue.Queue):
        self.events = events
        self.lock = threading.Lock()
        self.waiters: dict[str, queue.Queue] = {}
        self.counter = 0
        threading.Thread(target=self._read, name="runtime-reader", daemon=True).start()

    def _read(self) -> None:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                tg.log(f"runtime sent invalid JSON: {line[:200]!r}")
                continue
            if message.get("type") == "response":
                waiter = self.waiters.pop(str(message.get("id")), None)
                if waiter:
                    waiter.put(message)
                continue
            self.events.put(("runtime", message))
        # stdin closed: the supervisor is stopping this door.
        self.events.put(("eof", None))

    def write(self, message: dict) -> None:
        with self.lock:
            sys.stdout.write(json.dumps(message) + "\n")
            sys.stdout.flush()

    def call(self, command: str, args: dict | None = None, timeout: float = 30) -> dict:
        """One request; returns its result or raises RuntimeError."""
        with self.lock:
            self.counter += 1
            ident = f"r{self.counter}"
        waiter: queue.Queue = queue.Queue(maxsize=1)
        self.waiters[ident] = waiter
        self.write({"type": "request", "id": ident, "command": command, "args": args or {}})
        try:
            response = waiter.get(timeout=timeout)
        except queue.Empty:
            self.waiters.pop(ident, None)
            raise RuntimeError(f"Privateer did not answer {command} within {timeout:g}s")
        if not response.get("ok"):
            raise RuntimeError(response.get("error") or f"{command} failed")
        return response.get("result") or {}

    def delivered(self, ref: str) -> None:
        self.write({"type": "delivered", "ref": ref})


# Commands that change the whole fleet or close the captain's own items. In a
# group chat they run only for a listed user: any member is heard as the
# captain otherwise, and one /yolo on would put every first mate on bypass.
FLEET_COMMANDS = ("yolo", "stow", "afk", "dismiss")


def chat_kind(cfg: dict) -> str:
    """The configured chat's Telegram type (`private`, `group`, `supergroup`,
    `channel`), asked of the API; when it does not answer, the id's sign says
    it, since Telegram gives groups and channels negative ids."""
    result, err = tg.api(cfg, "getChat", {"chat_id": tg.chat_id(cfg)}, retries=2)
    if isinstance(result, dict) and isinstance(result.get("type"), str):
        return result["type"]
    tg.log(f"could not read the chat's type ({err}); judging it by its id")
    try:
        return "private" if int(tg.chat_id(cfg)) > 0 else "group"
    except ValueError:
        return "group"


class Door:
    def __init__(self, hello: dict, cfg: dict, runtime: Runtime):
        self.hello = hello
        self.cfg = cfg
        self.runtime = runtime
        self.pending: list[dict] = []
        self.sent = tg.read_state(SENT, [])
        self.retry_at = 0.0
        # Whether the chat is a group, from its type at start or from any
        # update that says so; and who sent the update being handled.
        self.group = False
        self.sender = None

    # --- sending to Telegram ---------------------------------------------

    def say(self, text: str, reply_to=None, html: bool = False, markup: dict | None = None,
            quiet: bool = False):
        params = {"chat_id": tg.chat_id(self.cfg), "text": text,
                  "disable_web_page_preview": "true"}
        if html:
            params["parse_mode"] = "HTML"
        if reply_to:
            params["reply_to_message_id"] = reply_to
            params["allow_sending_without_reply"] = "true"
        if markup:
            params["reply_markup"] = json.dumps(markup)
        if quiet:
            params["disable_notification"] = "true"
        result, err = tg.api(self.cfg, "sendMessage", params)
        if err and html and "parse" in err:
            # Telegram refused the rendered HTML: send the text plainly.
            params.pop("parse_mode")
            params["text"] = text if not html else _plain(text)
            result, err = tg.api(self.cfg, "sendMessage", params)
        return (result or {}).get("message_id"), err

    def recipient(self) -> str:
        chosen = tg.read_state(TARGET, {}).get("ship")
        return chosen or self.hello.get("recipient") or ""

    # --- captain mail out ----------------------------------------------------

    def on_mail(self, event: dict) -> None:
        self.pending.append(event)
        self.flush()

    def flush(self) -> None:
        """Push pending mail in order; stop at the first failure and retry later,
        so nothing is acknowledged past a message the captain has not seen."""
        while self.pending:
            event = self.pending[0]
            mail = event.get("mail") or {}
            key = event.get("id") if event.get("type") == "digest" else mail.get("id")
            if key not in self.sent:
                err = self.push_digest(event) if event.get("type") == "digest" else self.push(event)
                if err:
                    tg.log(f"push of {event.get('ref')} failed ({err}); retrying in {RETRY_SECONDS}s")
                    self.retry_at = time.monotonic() + RETRY_SECONDS
                    return
                self.sent.append(key)
                del self.sent[:-SENT_LIMIT]
                tg.write_state(SENT, self.sent)
            self.runtime.delivered(event["ref"])
            self.pending.pop(0)

    def push_digest(self, event: dict) -> str:
        """An away digest or the return catch-up: one message, silent unless urgent."""
        text = "\n".join(str(line) for line in event.get("lines") or [])
        pieces = tg.split_markdown(text, TEXT_LIMIT) or [text]
        for piece in pieces:
            _, err = self.say(piece, quiet=not event.get("loud"))
            if err:
                return err
        return ""

    def push(self, event: dict) -> str:
        """Send one message to the captain -> error, or ""."""
        mail, ship = event["mail"], event["ship"]
        kind = mail.get("kind", "notice")
        sender = mail.get("from") or ship
        body = mail.get("body") or ""
        payload = mail.get("payload") or {}
        # One card per ask: mail the runtime attached to an open ask, and the
        # ask's closure, edit the card the captain already has.
        # High urgency is never only a silent card edit.
        if payload.get("attached_to") and kind != "reply" and mail.get("urgency") != "high":
            if self.attach(mail, payload):
                return ""
            # The card is not known here: show the mail itself, quietly.
            event = {**event, "quiet": True}
        if kind == "notice" and isinstance(payload.get("request_end"), dict):
            cards = tg.read_state(CARDS, {})
            ended = mail.get("correlation")
            if ended in cards:
                if not cards[ended].get("resolved"):
                    self.resolve(ended, payload["request_end"].get("reason") or "closed")
                return ""
            # An item added to a card ends with it; its card already says so.
            if any(ended in (card.get("members") or []) for card in cards.values()):
                return ""
        # Only a decision message carries a typed card; other mail may use the
        # same payload key for its own data.
        decision = (mail.get("payload") or {}).get("decision") if kind == "decision" else None
        # A report is read in the captain's portal: the chat says it is there,
        # by title, and never carries its content (HTML least of all).
        report = payload.get("report") if isinstance(payload.get("report"), dict) else None
        privileged = privileged_of(mail)
        if privileged is not None:
            body = privileged_text(privileged)
        elif isinstance(decision, dict):
            try:
                body = decision_text(decision, body)
            except (AttributeError, TypeError, ValueError, KeyError) as exc:
                tg.log(f"decision card of {mail.get('id')} could not be shown ({exc}); sending its text")
        elif not body and report is None:
            body = "```json\n" + json.dumps(payload, indent=2) + "\n```"
        if report is not None:
            line = report_line(report)
            body = f"{body}\n\n{line}" if body else line
        if event.get("step"):
            # The exact thing only the captain can do, ahead of the request's words.
            body = f"**Your hands:** {event['step']}\n\n{body}"
        if suppress_noop_ack(self.cfg, body):
            tg.log(f"{event['ref']}: a routine acknowledgement stays out of the chat")
            return ""
        header = f"{ICONS.get(kind, '✉️')} <b>{tg.escape_html(sender)}</b> · {kind}"
        if event.get("breakthrough"):
            header = "🚨 " + header + " · while you're away"
        elif mail.get("urgency") == "high":
            header = "🔴 " + header
        if event.get("away") and not event.get("breakthrough"):
            header += " · while you're away"
        if event.get("again"):
            header += " · (again)"
        # /waiting shows what is already outstanding, so say how old it is.
        if event.get("note"):
            header += f" · {event['note']}"
        if privileged is not None:
            header += " · privileged command: Approve, then run it at the machine"
        elif kind in ("request", "decision"):
            header += " · reply to answer"
        if event.get("step"):
            header += " · needs your hands"
        # A reply threads under the captain's message it answers.
        reply_to = None
        if mail.get("correlation"):
            reply_to = tg.read_state(OUTGOING, {}).get(mail["correlation"])
        typed = kind == "decision" and isinstance((mail.get("payload") or {}).get("decision"), dict)
        target = {"ship": ship, "mail": mail.get("id"), "kind": kind, "from": sender,
                  "typed": typed}
        markup = self.decision_markup(mail) if kind == "decision" else None
        if privileged is not None:
            # The hash this card shows rides the button, so a tap answers
            # this exact command. Only the runtime writes this kind; the buttons answer it through
            # the door's approval command, never as a reply.
            shown = str(privileged.get("hash", ""))[:8]
            markup = {"inline_keyboard": [[
                {"text": "Approve", "callback_data": f"pa:{mail.get('id')}:g:{shown}"},
                {"text": "Deny", "callback_data": f"pa:{mail.get('id')}:d:{shown}"}]]}
            typed = True
        pieces = tg.split_markdown(body, TEXT_LIMIT) or [body]
        markdown = self.cfg.get("markdown", True)
        for index, piece in enumerate(pieces):
            text = tg.md_to_html(piece) if markdown else tg.escape_html(piece)
            if index == 0:
                text = f"{header}\n{text}"
            last = index == len(pieces) - 1
            message_id, err = self.say(text, reply_to=reply_to if index == 0 else None,
                                       html=True, markup=markup if last else None,
                                       quiet=bool(event.get("quiet")))
            if err:
                return err
            if last and typed and markup:
                self.remember_card(mail.get("id"), message_id, text, markup)
            tg.remember_target(message_id, target, int(self.cfg.get("msg_map_limit", tg.MSG_MAP_LIMIT)))
        try:
            import tts
            tts.maybe_speak(self.cfg, mail.get("body") or "", target,
                            intermediate=kind == "notice")
        except Exception as e:
            tg.log(f"tts unavailable: {e!r}")
        return ""

    def remember_card(self, mail_id, message_id, text: str, markup: dict) -> None:
        cards = tg.read_state(CARDS, {})
        cards.pop(mail_id, None)
        cards[mail_id] = {"message_id": message_id, "text": text, "markup": markup,
                          "also": [], "blocked": [], "members": []}
        while len(cards) > CARD_LIMIT:
            cards.pop(next(iter(cards)))
        tg.write_state(CARDS, cards)

    def edit_card(self, card: dict, extra: str = "") -> str:
        """Rewrite a card's text. Telegram drops the buttons of an edit that
        does not carry them, so the card's own buttons are always sent again."""
        text = card["text"]
        if card.get("blocked"):
            text += "\n\n<b>Also blocked:</b> " + "; ".join(tg.escape_html(b) for b in card["blocked"])
            if card.get("markup"):
                text += "\nYour answer applies to every blocked task."
        if card.get("also"):
            text += "\n<b>Also:</b> " + ", ".join(tg.escape_html(a) for a in card["also"])
        text += extra
        _, err = tg.api(self.cfg, "editMessageText", {
            "chat_id": tg.chat_id(self.cfg), "message_id": card["message_id"], "text": text,
            "parse_mode": "HTML", "disable_web_page_preview": "true",
            "reply_markup": json.dumps(card.get("markup") or {"inline_keyboard": []})})
        return err

    def attach(self, mail: dict, payload: dict) -> bool:
        cards = tg.read_state(CARDS, {})
        card = cards.get(payload["attached_to"])
        if not card or card.get("resolved"):
            # Its card was shown before this door kept cards: show it as mail.
            tg.log(f"{mail.get('id')} joins {payload['attached_to']}, whose card is not known here")
            return False
        sender = mail.get("from") or "a ship"
        card.setdefault("members", []).append(mail.get("id"))
        if payload.get("type") == "tool_permission":
            card["also"].append(f"{sender} (same denial)")
            card["blocked"].extend(f"{sender} {item}" for item in
                                   (payload.get("decision") or {}).get("blocked") or [])
        else:
            card["also"].append(f"{sender} ({mail.get('kind', 'notice')})")
        err = self.edit_card(card)
        if err:
            tg.log(f"could not edit the card for {payload['attached_to']}: {err}")
        tg.write_state(CARDS, cards)
        return True

    def resolve(self, mail_id: str, reason: str) -> None:
        # The card is kept, marked resolved, so the closures of the items
        # added to it are recognised and send nothing.
        cards = tg.read_state(CARDS, {})
        card = cards[mail_id]
        card["markup"] = None
        card["resolved"] = reason
        err = self.edit_card(card, f"\n\n<b>Resolved:</b> {tg.escape_html(reason)}")
        if err:
            tg.log(f"could not mark the card for {mail_id} resolved: {err}")
        tg.write_state(CARDS, cards)

    def decision_markup(self, mail: dict) -> dict | None:
        """Buttons for a decision: one per typed option, or one per plain choice."""
        payload = mail.get("payload") or {}
        if isinstance(payload.get("decision"), dict):
            options = payload["decision"].get("options") or []
            ids = [o.get("id") for o in options]
            labels = [o.get("label", "") for o in options]
        else:
            labels = payload.get("options")
            if not isinstance(labels, list) or not 1 <= len(labels) <= 8:
                return None
            if not all(isinstance(o, str) and 0 < len(o) <= 64 for o in labels):
                return None
            ids = [None] * len(labels)
        if not 1 <= len(labels) <= 8:
            return None
        decisions = tg.read_state(DECISIONS, {})
        key = str(int(time.time() * 1000) % 10**10)
        decisions[key] = {"mail": mail.get("id"), "from": mail.get("from"),
                          "options": labels, "ids": ids}
        while len(decisions) > 200:
            decisions.pop(next(iter(decisions)))
        tg.write_state(DECISIONS, decisions)
        return {"inline_keyboard": [[{"text": o, "callback_data": f"d:{key}:{i}"}]
                                    for i, o in enumerate(labels)]}

    # --- captain messages in ---------------------------------------------

    def send(self, args: dict, msg_id, label: str) -> None:
        """Mail from the captain, with what the chat sees about it."""
        if self.cfg.get("send_kind") in ("request", "notice") and args.get("kind") is None:
            args["kind"] = self.cfg["send_kind"]
        try:
            result = self.runtime.call("send", args)
        except RuntimeError as e:
            if "already answered" in str(e) and args.get("kind") == "reply":
                # Answered elsewhere already: follow up instead of failing. A
                # typed answer cannot go to a request, so it goes as plain words.
                args = dict(args, kind="request")
                # Only the typed answer cannot ride a request; anything else
                # in the payload (the files of an attachment) still goes.
                payload = args.get("payload")
                if isinstance(payload, dict):
                    payload = dict(payload)
                    payload.pop("answer", None)
                    if payload:
                        args["payload"] = payload
                    else:
                        args.pop("payload", None)
                else:
                    args.pop("payload", None)
                try:
                    result = self.runtime.call("send", args)
                except RuntimeError as again:
                    self.say(f"⚠️ not sent: {again}", reply_to=msg_id)
                    return
            else:
                self.say(f"⚠️ not sent: {e}", reply_to=msg_id)
                return
        self.welcome(result.get("returned"))
        ship = result.get("ship", "")
        mail_id = (result.get("mail") or {}).get("id")
        if mail_id and msg_id:
            outgoing = tg.read_state(OUTGOING, {})
            outgoing[mail_id] = msg_id
            while len(outgoing) > tg.MSG_MAP_LIMIT:
                outgoing.pop(next(iter(outgoing)))
            tg.write_state(OUTGOING, outgoing)
            # Replying to your own message follows up with the same ship.
            tg.remember_target(msg_id, {"ship": ship, "mail": mail_id, "kind": "sent", "from": ship})
        tg.log(f"captain -> {ship}: {label} ({args.get('kind', 'request')})")
        if not self.react(msg_id):
            self.say(f"➡️ {ship}", reply_to=msg_id, quiet=True)

    def react(self, msg_id) -> bool:
        if not msg_id or not self.cfg.get("confirm_with_reaction", True):
            return False
        _, err = tg.api(self.cfg, "setMessageReaction", {
            "chat_id": tg.chat_id(self.cfg), "message_id": msg_id,
            "reaction": json.dumps([{"type": "emoji", "emoji": "👍"}])}, retries=1)
        return not err

    def route(self, text: str, msg_id, reply_to) -> dict | None:
        """Where a message goes, as `send` args, or None after saying why not."""
        if reply_to:
            target = tg.target_for_reply(reply_to)
            if not target:
                self.say("⚠️ I don't know which message that answers (it may be older than I "
                         "remember). Send it without replying, or use /to SHIP.", reply_to=msg_id)
                return None
            if target.get("kind") in ("request", "decision"):
                args = {"to": target["from"], "kind": "reply", "correlation": target["mail"],
                        "body": text}
                if target.get("typed"):
                    args["payload"] = {"answer": {"text": text}}
                return args
            return {"to": target.get("from") or target.get("ship"), "kind": "request",
                    "correlation": target.get("mail"), "body": text}
        to = tg.read_state(TARGET, {}).get("ship")
        return {"to": to, "body": text} if to else {"body": text}

    def on_update(self, update: dict) -> None:
        if "callback_query" in update:
            return self.on_callback(update["callback_query"])
        msg = update.get("message") or {}
        if not msg:
            return
        chat = (msg.get("chat") or {}).get("id")
        user = (msg.get("from") or {}).get("id")
        if not self.authorized(chat, user):
            tg.log(f"ignoring a message from chat={chat} user={user}")
            return
        kind = (msg.get("chat") or {}).get("type")
        if isinstance(kind, str) and kind != "private":
            self.group = True
        self.sender = user
        msg_id = msg.get("message_id")
        reply_to = (msg.get("reply_to_message") or {}).get("message_id")
        raw = msg.get("text") or ""
        if raw.strip().startswith("/"):
            return self.command(raw.strip(), msg_id)
        text = struck(raw, msg.get("entities")).strip()
        if msg.get("voice"):
            return self.handle_voice(msg, msg_id, reply_to)
        if attachment(msg)[0]:
            return self.handle_attachment(msg, msg_id, reply_to)
        if not text:
            return
        args = self.route(text, msg_id, reply_to)
        if args:
            self.send(args, msg_id, f"{len(text)} chars")

    def authorized(self, chat, user) -> bool:
        if not tg.chat_id(self.cfg) or str(chat) != tg.chat_id(self.cfg):
            return False
        allowed = self.cfg.get("allowed_user_ids", [])
        return not allowed or str(user) in {str(u) for u in allowed}

    def listed(self, user) -> bool:
        """Named in allowed_user_ids, not merely a member of the chat."""
        return str(user) in {str(u) for u in self.cfg.get("allowed_user_ids", [])}

    def on_callback(self, query: dict) -> None:
        chat = ((query.get("message") or {}).get("chat") or {}).get("id")
        user = (query.get("from") or {}).get("id")
        tg.api(self.cfg, "answerCallbackQuery", {"callback_query_id": query.get("id")}, retries=1)
        if not self.authorized(chat, user):
            tg.log(f"ignoring a button from chat={chat} user={user}")
            return
        data = query.get("data") or ""
        msg = query.get("message") or {}
        if data.startswith("to:"):
            ship = data[3:]
            tg.write_state(TARGET, {"ship": ship})
            self.say(f"➡️ plain messages now go to {ship}. /to default resets.", quiet=True)
        elif data.startswith("pa:"):
            # Telegram carries at most 64 bytes: the id, g or d, the hash's start.
            _, mail_id, code, shown = (data.split(":", 3) + [""])[:4]
            action = {"g": "grant", "d": "deny"}.get(code, code)
            try:
                result = self.runtime.call("approval", {"id": mail_id, "action": action,
                                                        "hash": shown or None})
            except RuntimeError as e:
                self.say(f"⚠️ not answered: {e}", reply_to=msg.get("message_id"))
                return
            if result.get("action") == "approval.grant":
                self.say(f"✅ Approved. It waits for you at {result.get('machine') or 'the machine'}: "
                         f"run <code>{tg.escape_html(result.get('run') or '')}</code> there "
                         "and type the hash's first 8 characters.",
                         reply_to=msg.get("message_id"), html=True)
            else:
                self.say("❌ Denied. Nothing runs.", reply_to=msg.get("message_id"))
            cards = tg.read_state(CARDS, {})
            if mail_id in cards:
                cards[mail_id]["markup"] = None
                tg.write_state(CARDS, cards)
            tg.api(self.cfg, "editMessageReplyMarkup", {
                "chat_id": tg.chat_id(self.cfg), "message_id": msg.get("message_id"),
                "reply_markup": json.dumps({"inline_keyboard": []})}, retries=1)
        elif data.startswith("d:"):
            _, key, index = data.split(":", 2)
            decision = tg.read_state(DECISIONS, {}).get(key)
            if not decision:
                self.say("⚠️ that decision is no longer known; reply to its message instead.")
                return
            choice = decision["options"][int(index)]
            args = {"to": decision["from"], "kind": "reply", "correlation": decision["mail"],
                    "body": choice}
            option_id = (decision.get("ids") or [None] * len(decision["options"]))[int(index)]
            if option_id:
                args["payload"] = {"answer": {"option_id": option_id}}
            self.send(args, msg.get("message_id"), f"decision: {choice}")
            # An answered card keeps no buttons, even when later mail edits it.
            cards = tg.read_state(CARDS, {})
            if decision["mail"] in cards:
                cards[decision["mail"]]["markup"] = None
                tg.write_state(CARDS, cards)
            tg.api(self.cfg, "editMessageReplyMarkup", {
                "chat_id": tg.chat_id(self.cfg), "message_id": msg.get("message_id"),
                "reply_markup": json.dumps({"inline_keyboard": []})}, retries=1)

    def command(self, text: str, msg_id) -> None:
        head, _, rest = text.partition(" ")
        name = head[1:].split("@")[0].lower()
        rest = rest.strip()
        if name in FLEET_COMMANDS and self.group and not self.listed(self.sender):
            tg.log(f"refusing /{name} from user={self.sender}: not in allowed_user_ids of a group chat")
            self.say(f"🔒 /{name} needs your Telegram user id in allowed_user_ids in a group chat.",
                     reply_to=msg_id)
            return
        try:
            if name in ("help", "start"):
                recipient = self.recipient() or "the liaison"
                self.say(HELP.format(recipient=recipient))
            elif name == "to":
                self.cmd_to(rest, msg_id)
            elif name == "status":
                self.cmd_status()
            elif name == "ships":
                self.cmd_ships()
            elif name == "waiting":
                self.cmd_waiting()
            elif name == "dismiss":
                self.cmd_dismiss(rest, msg_id)
            elif name == "yolo":
                if rest not in ("", "status", "on", "off"):
                    self.say("Usage: /yolo on|off|status", reply_to=msg_id)
                    return
                args = {} if rest in ("", "status") else {"mode":"yolo" if rest == "on" else "standard"}
                value = self.runtime.call("permissions", args)
                self.say(f"Fleet tool mode: {value['mode']}. Applies to every first mate's next launch; running sessions keep their current mode.")
            elif name == "stow":
                self.cmd_stow(rest, msg_id)
            elif name == "afk":
                self.cmd_afk(rest)
            elif name == "back":
                self.welcome(self.runtime.call("away", {"action": "back"}).get("returned"), quiet=False)
            elif name == "held":
                self.cmd_held()
            elif name == "digest":
                state = self.runtime.call("digest", {})
                if state.get("away"):
                    self.say("📋 Sending what is held now.", quiet=True)
                else:
                    self.say("📋 You are not away, so nothing is being held.", quiet=True)
            elif name == "voice":
                import tts
                self.say(tts.voice_reply(self.cfg, rest))
            else:
                self.say(f"❓ unknown command /{name} — try /help", reply_to=msg_id)
        except RuntimeError as e:
            self.say(f"⚠️ {e}", reply_to=msg_id)

    def cmd_afk(self, rest: str) -> None:
        out = self.runtime.call("away", {"action": "start", "words": rest})
        away = out.get("away") or {}
        since = time.strftime("%H:%M", time.localtime((away.get("since_ms") or 0) / 1000))
        until = f" (back about {away['until']})" if away.get("until") else ""
        text = (f"🌙 Away from {since}{until}. Requests and emergencies 🚨 reach you at once; "
                "other notices come in a digest. Any message brings you back.")
        if away.get("words"):
            text += f"\nYour instructions for the liaison: “{away['words']}”"
        self.say(text)

    def welcome(self, returned: dict | None, quiet: bool = True) -> None:
        """The records-only digest on the captain's return; the liaison's summary follows."""
        if not returned:
            if not quiet:
                self.say("You weren't away.")
            return
        if returned.get("catch_up"):
            # The catch-up follows as its own message, retried until delivered.
            return
        text = "🌅 " + returned.get("digest", "Welcome back.")
        if returned.get("summary_requested"):
            text += "\nThe liaison is writing you a summary."
        self.say(text)

    def cmd_held(self) -> None:
        out = self.runtime.call("held", {})
        held = out.get("held") or []
        if not held:
            self.say("Nothing was held.")
            return
        lines = [f"{len(held)} held:"]
        for m in held[:30]:
            when = time.strftime("%H:%M", time.localtime(m.get("created_ms", 0) / 1000))
            body = " ".join((m.get("body") or "").split())[:140]
            lines.append(f"• {m.get('ship')} {m.get('kind')} {when}: {body}")
        if len(held) > 30:
            lines.append(f"…and {len(held) - 30} more.")
        self.say("\n".join(lines))

    def cmd_stow(self, rest: str, msg_id) -> None:
        words = rest.split()
        verb = words.pop(0) if words and words[0] in ("status", "resume") else ""
        action = {"": "start", "status": "status", "resume": "resume"}[verb]
        # `/stow resume all` lifts every linked machine this one reaches first.
        everywhere = bool(words) and words[0] == "all"
        if everywhere:
            words.pop(0)
        if len(words) > 1:
            self.say("Usage: /stow [status|resume] [machine|all]", reply_to=msg_id)
            return
        args = {"action": action}
        if words:
            args["machine"] = words[0]
        if everywhere:
            args["all"] = True
        out = self.runtime.call("stow", args)
        # A stow is one machine's: say which one, and which linked machines
        # are still holding theirs. A named machine's answer carries no machines
        # of its own, so this never shows another machine's view as ours.
        this = out.get("this_machine")
        subject = (f"the fleet on {words[0]}" if words
                   else f"this machine ({this})" if this else "this machine")
        held = [m["machine"] for m in out.get("machines") or [] if m.get("held") is True]
        unknown = [m["machine"] for m in out.get("machines") or [] if "held" in m and m.get("held") is None]
        errors = {m["machine"]: m["error"] for m in out.get("machines") or [] if m.get("error")}
        machines = ""
        if held:
            machines += " Still stowed: " + ", ".join(held) + "."
        if unknown:
            machines += (" Unknown here: " + ", ".join(unknown) + "."
                         + self.why(errors, unknown))
        if action == "start":
            names = ", ".join(s["ship"] for s in out["ships"])
            self.say(f"🧳 Stowing {subject}: new jobs are held. Asked {names} to checkpoint.{machines} /stow status{' ' + words[0] if words else ''} shows who is done.")
        elif action == "resume" and not out.get("stow"):
            self.say(f"{subject[0].upper() + subject[1:]} was not stowed.{machines}")
        elif action == "resume":
            self.say(f"⚓ Resumed {subject}: new jobs start again{' there' if words else ' here'}.{machines}")
        elif not out.get("stow"):
            self.say(f"{subject[0].upper() + subject[1:]} has never been stowed.{machines}")
        else:
            # The header names the machine asked about, and says when its hold
            # is still on.
            name = words[0] if words else this
            held_here = " (held)" if out.get("held") else ""
            lines = [f"Stow on {name}{held_here}:"] if name else []
            for ship in out["ships"]:
                if ship.get("error"):
                    lines.append(f"{ship['ship']}: {ship['error']}")
                    continue
                moving = ship.get("jobs_moving", 0)
                state = "stowed" if ship.get("replied") else "not yet"
                lines.append(f"{ship['ship']}: {state}" + (f", {moving} job(s) running" if moving else ""))
            if out.get("held"):
                lines.append("Ready to update." if out.get("ready") else "Not ready yet.")
            else:
                # Name the machine: a hold is one machine's, and this sentence
                # is about the one that was asked.
                lines.append(f"{subject[0].upper() + subject[1:]} has resumed.")
            if held or unknown:
                lines.append(f"Still stowed: {', '.join(held)}." if held else "")
                lines.append(f"Unknown here: {', '.join(unknown)}.{self.why(errors, unknown)}" if unknown else "")
                if held and not everywhere:
                    lines.append("/stow resume all lifts every linked machine, then this one.")
            self.say("\n".join(line for line in lines if line))

    @staticmethod
    def why(errors: dict, names: list) -> str:
        """Why a hold is unknown, when the link said why."""
        told = [f"{name}: {errors[name]}" for name in names if errors.get(name)]
        return (" " + "; ".join(told)) if told else ""

    def cmd_to(self, rest: str, msg_id) -> None:
        if not rest:
            ships = self.runtime.call("ships")["ships"]
            current = self.recipient() or "nobody yet"
            buttons = [[{"text": s["name"] + (" · liaison" if s.get("liaison") else ""),
                         "callback_data": f"to:{s['name']}"}] for s in ships[:20]]
            self.say(f"Plain messages go to {current}. Pick a ship:",
                     markup={"inline_keyboard": buttons})
            return
        ship, _, body = rest.partition(" ")
        if ship == "default":
            tg.write_state(TARGET, {})
            self.say(f"➡️ plain messages go to {self.hello.get('recipient') or 'the liaison'} again.")
            return
        known = {s["name"] for s in self.runtime.call("ships")["ships"]}
        if ship not in known:
            self.say(f"⚠️ no ship named {ship}. /ships lists them.", reply_to=msg_id)
            return
        if body.strip():
            self.send({"to": ship, "body": body.strip()}, msg_id, f"{len(body)} chars")
        else:
            tg.write_state(TARGET, {"ship": ship})
            self.say(f"➡️ plain messages now go to {ship}. /to default resets.", quiet=True)

    def cmd_status(self) -> None:
        lines = ["<b>Fleet status</b> (from records; no one was woken)"]
        for ship in self.runtime.call("status")["ships"]:
            name = tg.escape_html(ship["name"])
            if ship.get("error"):
                lines.append(f"⚠️ <b>{name}</b>: {tg.escape_html(ship['error'])}")
                continue
            state = ("👤 captain aboard" if ship.get("attended")
                     else "⚙️ working" if ship.get("working")
                     else "💤 idle" if ship.get("dispatcher") else "⛔ dispatcher stopped")
            bits = [state, f"{ship.get('open_tasks', 0)} open tasks"]
            if ship.get("waiting_on_captain"):
                bits.append(f"<b>{ship['waiting_on_captain']} waiting on you</b>")
            if ship.get("unacknowledged_mail"):
                bits.append(f"{ship['unacknowledged_mail']} unread mail")
            tag = " · liaison" if ship.get("liaison") else ""
            lines.append(f"• <b>{name}</b>{tag}: " + " · ".join(bits))
        self.say("\n".join(lines), html=True)

    def cmd_ships(self) -> None:
        result = self.runtime.call("ships")
        lines = ["<b>Ships</b>"]
        for ship in result["ships"]:
            tag = " · liaison" if ship.get("liaison") else ""
            scope = tg.escape_html(ship.get("scope") or "no scope set")
            lines.append(f"• <b>{tg.escape_html(ship['name'])}</b>{tag} — {scope}")
        lines.append(f"\nPlain messages go to {tg.escape_html(self.recipient() or 'nobody yet')}.")
        self.say("\n".join(lines), html=True)

    def cmd_dismiss(self, rest: str, msg_id) -> None:
        ident, _, note = rest.partition(" ")
        if not ident:
            self.say("Usage: /dismiss ID [reason]", reply_to=msg_id)
            return
        closed = self.runtime.call("dismiss", {"id": ident, "note": note.strip()})
        full = closed.get("id") or ident
        # Its card, if this door showed one, stops offering answers.
        if full in tg.read_state(CARDS, {}):
            self.resolve(full, note.strip() or "dismissed")
        self.say(f"Dismissed {full} from {closed.get('ship', 'its ship')} without an answer.",
                 reply_to=msg_id)

    def cmd_waiting(self) -> None:
        listed = self.runtime.call("waiting")
        waiting = listed["waiting"]
        if not waiting:
            self.say("Nothing is waiting on you. ⚓")
            return
        self.say(f"{len(waiting)} waiting on you; reply to each to answer:")
        # Asks only the captain's hands can clear come first, each with its step.
        hands = listed.get("hands_on") or []
        # Ship ids are chosen per home, so an ask is named by its ship and id.
        hand_keys = {(item["ship"], item["mail"].get("id")) for item in hands}
        rest = [item for item in waiting
                if (item["ship"], item["mail"].get("id")) not in hand_keys]
        shown = 0
        for title, items in (("Needs your hands:", hands), ("Everything else:", rest)):
            if not items or shown >= 20:
                continue
            if hands or title != "Everything else:":
                self.say(title)
            for item in items:
                if shown == 20:
                    break
                # Shown again as replyable messages, without re-acknowledging
                # anything, and carrying the age so an old request reads as old.
                age = item.get("age_ms")
                note = f"asked {age_phrase(age)}" if isinstance(age, int) else ""
                if item.get("attached"):
                    note += f" · +{item['attached']} attached"
                err = self.push({"ref": "", "ship": item["ship"], "mail": item["mail"],
                                 "note": note, "step": item.get("step")})
                if err:
                    self.say(f"⚠️ could not show one: {err}")
                    return
                shown += 1

    # --- attachments and voice -------------------------------------------

    def fetch(self, file_id: str, name: str, msg_id) -> str:
        """Download a Telegram file into the inbox -> its path, or "" after saying why."""
        info, err = tg.api(self.cfg, "getFile", {"file_id": file_id})
        remote = (info or {}).get("file_path", "")
        if err or not remote:
            self.say(f"⚠️ couldn't locate that file: {err or 'no file_path'}", reply_to=msg_id)
            return ""
        saved = tg.safe_name(name or os.path.basename(remote))
        dest = os.path.join(tg.inbox_dir(), f"{time.strftime('%Y%m%d-%H%M%S')}-{msg_id}-{saved}")
        err = tg.download(self.cfg, remote, dest)
        if err:
            tg.log(f"download failed: {err}")
            self.say(f"⚠️ download failed: {err}", reply_to=msg_id)
            return ""
        return dest

    def too_big(self, size, msg_id) -> bool:
        cap = float(self.cfg.get("max_file_mb", 20))
        if size and size > cap * 1024 * 1024:
            self.say(f"📎 {size / 1048576:.1f} MB is over the {cap:g} MB limit "
                     "(Telegram bots can't fetch anything above 20 MB)", reply_to=msg_id)
            return True
        return False

    def handle_attachment(self, msg: dict, msg_id, reply_to) -> None:
        if not self.cfg.get("accept_files", True):
            self.say("📎 attachments are off — set accept_files = true", reply_to=msg_id)
            return
        file_id, name, size = attachment(msg)
        if self.too_big(size, msg_id):
            return
        caption = struck(msg.get("caption") or "", msg.get("caption_entities")).strip()
        args = self.route(caption or "(file)", msg_id, reply_to)
        if not args:
            return
        dest = self.fetch(file_id, name, msg_id)
        if not dest:
            return
        saved = tg.safe_name(name) if name else download_name(dest)
        ship = args.get("to") or self.recipient()
        stored = None
        if ship:
            try:
                # The file reaches the ship's own home, on this machine or a
                # linked one; the runtime fills in its path when the mail lands.
                stored = self.runtime.call(
                    "file.put", {"to": ship, "name": saved, "content_file": dest}, timeout=120)
            except RuntimeError as e:
                # An older runtime has no file.put at all; that alone falls back
                # quietly. Any other refusal loses the file for a ship on a
                # linked machine, so the captain hears about it first.
                if "unknown door command" not in str(e):
                    self.say(f"⚠️ couldn't hand {saved} to {ship}: {e}; kept at {dest}",
                             reply_to=msg_id)
                tg.log(f"file.put failed for {saved}: {e}; keeping {dest} in the inbox")
        if stored and stored.get("file_id"):
            # The runtime fills the path when the mail lands, and the mail must
            # go to the ship that owns the file, not a default chosen later.
            args["to"] = ship
            payload = args.get("payload")
            if not isinstance(payload, dict):
                payload = {}
                args["payload"] = payload
            payload["files"] = [{"file_id": stored["file_id"]}]
            args["body"] = caption or f"The captain sent a file: {saved}"
            try:
                os.unlink(dest)
            except OSError as e:
                tg.log(f"could not remove the inbox copy {dest}: {e}")
        else:
            # No file.put (an older runtime, an unknown command or a refusal):
            # the ship on this machine can still be told where the file is.
            note = f"The captain sent a file: {dest}"
            args["body"] = f"{caption}\n\n{note}" if caption else note
        self.send(args, msg_id, f"file {saved}")

    def handle_voice(self, msg: dict, msg_id, reply_to) -> None:
        import stt
        if not stt.configured(self.cfg):
            return self.handle_attachment(msg, msg_id, reply_to)
        problem = stt.config_error(self.cfg)
        if problem:
            self.say(f"🎙️ voice transcription is unavailable: {problem}", reply_to=msg_id)
            return
        voice = msg["voice"]
        if self.too_big(voice.get("file_size") or 0, msg_id):
            return
        args = self.route("(voice)", msg_id, reply_to)
        if not args:
            return
        dest = self.fetch(voice["file_id"], "voice.ogg", msg_id)
        if not dest:
            return
        transcript, err = stt.transcribe(self.cfg, dest)
        if err:
            tg.log(f"stt failed for message {msg_id} ({stt.duration_note(voice)}): {err}")
            self.say(f"🎙️ transcription failed — {stt.error_class(err)}. The audio is kept at {dest}",
                     reply_to=msg_id)
            return
        if stt.is_empty(transcript):
            # Nothing the captain said: tell them to re-record rather than send an answer.
            self.say("🎙️ your voice note came through empty, please re-record", reply_to=msg_id)
            return
        body = stt.prompt_for(transcript)
        caption = struck(msg.get("caption") or "", msg.get("caption_entities")).strip()
        args["body"] = f"{caption}\n{body}" if caption else body
        if "answer" in args.get("payload", {}):
            # A typed answer carries the captain's words, not the "(voice)" placeholder.
            spoken = transcript.strip()
            text = f"{caption}\n{spoken}" if caption else spoken
            # The runtime caps an answer at 4096 characters; the body keeps it all.
            args["payload"]["answer"]["text"] = text if len(text) <= 4096 else text[:4095].rstrip() + "…"
        if transcript.strip() and stt.stt_config(self.cfg).get("echo_transcript", True):
            shown = transcript.strip()
            shown = shown if len(shown) <= 400 else shown[:400].rstrip() + "…"
            self.say(f'🎙️ "{shown}"', reply_to=msg_id, quiet=True)
        self.send(args, msg_id, f"voice {stt.duration_note(voice)}")


def struck(text: str, entities) -> str:
    """`text` with each strikethrough span wrapped in "~". Telegram sends
    strikethrough from its formatting menu as an entity, not as "~", so the
    words alone would read as never struck: a struck-through "yes" would be a
    yes. The runtime refuses "~" in a captain's yes, and the ship still sees
    what was struck. Entity offsets count UTF-16 code units."""
    marks = []
    for e in entities or []:
        if not isinstance(e, dict) or e.get("type") != "strikethrough":
            continue
        try:
            start, length = int(e["offset"]), int(e["length"])
        except (KeyError, TypeError, ValueError):
            # A strike the door cannot place still marks the whole text.
            start, length = 0, len(text.encode("utf-16-le")) // 2
        marks += [start, start + length]
    if not marks:
        return text
    units = text.encode("utf-16-le")
    end = len(units) // 2
    tilde = "~".encode("utf-16-le")
    for at in sorted((min(max(m, 0), end) for m in marks), reverse=True):
        units = units[:2 * at] + tilde + units[2 * at:]
    return units.decode("utf-16-le", errors="replace")


def download_name(dest: str) -> str:
    """The file's own name, without the inbox's timestamp and message prefix."""
    base = os.path.basename(dest)
    parts = base.split("-", 3)
    if len(parts) == 4 and len(parts[0]) == 8 and len(parts[1]) == 6:
        return parts[3]
    return base


def attachment(msg: dict) -> tuple[str, str, int]:
    """(file_id, sender-supplied name, size) for whatever file this message carries."""
    if msg.get("photo"):
        best = max(msg["photo"], key=lambda p: p.get("file_size") or p.get("width", 0))
        return best.get("file_id", ""), "", best.get("file_size", 0)
    for key in ("document", "video", "animation", "audio", "voice", "video_note"):
        obj = msg.get(key)
        if isinstance(obj, dict) and obj.get("file_id"):
            return obj["file_id"], obj.get("file_name", ""), obj.get("file_size", 0)
    return "", "", 0


def suppress_noop_ack(cfg: dict, body: str) -> bool:
    """`suppress_noop_ack = true` keeps a bare "Captain, shipshape." out of the chat."""
    if not cfg.get("suppress_noop_ack", False):
        return False
    import tts
    return tts.is_noop_acknowledgement(body)


def decision_text(decision: dict, body: str) -> str:
    """The card's words: the question, why and what is blocked, then each option
    with what it does, its cost and the recommended one marked, then the ship's
    own line of context."""
    head = [f"**{decision.get('question', '')}**"]
    if decision.get("why"):
        head.append(decision["why"])
    if decision.get("blocked"):
        head.append("Blocked: " + "; ".join(decision["blocked"]))
    options = []
    for option in decision.get("options") or []:
        line = f"- **{option.get('label', '')}**: {option.get('what_happens', '')}"
        if option.get("cost"):
            line += f" (cost: {option['cost']})"
        if option.get("recommended"):
            line += " (recommended)"
        options.append(line)
    parts = ["\n\n".join(head), "\n".join(options)]
    if body:
        parts.append(body)
    return "\n\n".join(part for part in parts if part)


def privileged_of(mail: dict) -> dict | None:
    """The staged command of a privileged request, or None."""
    payload = mail.get("payload") or {}
    staged = payload.get("privileged")
    if mail.get("request_kind") != "privileged_action" or not isinstance(staged, dict):
        return None
    return staged


def shown_arg(arg) -> str:
    """One argument as the runtime shows it: bare when plainly safe, otherwise
    quoted, with anything but printable ASCII escaped."""
    arg = str(arg)
    if arg and re.fullmatch(r"[A-Za-z0-9\-_./=:,+@%]+", arg):
        return arg
    out = []
    for c in arg:
        if c == "'":
            out.append("\\'")
        elif c == "\\":
            out.append("\\\\")
        elif " " <= c <= "~" and c != "`":
            out.append(c)
        else:
            out.append("\\u{%x}" % ord(c))
    return "'" + "".join(out) + "'"


def privileged_text(staged: dict) -> str:
    """The exact command, one argument per line, then where, why, any
    warnings and its hash. Everything the ship wrote is escaped as the
    runtime escapes an argument, inside code spans markdown cannot leave."""
    argv = "\n".join(shown_arg(a) for a in staged.get("argv") or [])
    warnings = "".join(f"warning: `{shown_arg(a)}` is not under a root-owned folder; "
                       "it can change after you approve\n"
                       for a in (staged.get("unrooted") or [])[:64])
    return (f"**Run as root on {shown_arg(staged.get('machine', '?'))}**\n```\n{argv}\n```\n"
            f"in `{shown_arg(staged.get('cwd', ''))}`\n"
            f"Reason: `{shown_arg(staged.get('reason', ''))}`\n"
            f"{warnings}"
            f"Hash `{shown_arg(staged.get('hash', ''))}`")


def report_line(report: dict) -> str:
    """Where a report is: its title, and that it is in the portal."""
    title = report.get("title")
    title = " ".join(title.split()) if isinstance(title, str) and title.strip() else "A report"
    return f"📄 {title} (report; read it in the portal)"


def age_phrase(age_ms: int) -> str:
    """How long ago, in the words a person would use: "12 min ago", "2 d ago"."""
    seconds = max(0, int(age_ms)) // 1000
    if seconds < 90:
        return "just now" if seconds < 10 else f"{seconds} s ago"
    if seconds < 5400:
        return f"{round(seconds / 60)} min ago"
    if seconds < 172800:
        return f"{round(seconds / 3600)} h ago"
    return f"{round(seconds / 86400)} d ago"


def _plain(html: str) -> str:
    import html as h
    import re
    return h.unescape(re.sub(r"<[^>]+>", "", html))


def poll_telegram(cfg: dict, events: queue.Queue, stop: threading.Event) -> None:
    """Long-poll getUpdates and hand each update to the main loop. The offset is
    saved before handling, so a poisonous update is never replayed forever."""
    offset = tg.read_state(OFFSET, {}).get("offset")
    failures = 0
    while not stop.is_set():
        if offset is None:
            # First run: skip what is already queued (setup chatter, old bots).
            updates, err = tg.api(cfg, "getUpdates", {"offset": -1, "timeout": 0}, retries=2)
            if err:
                tg.log(f"could not read the backlog ({err}); retrying")
                stop.wait(5)
                continue
            offset = (updates[-1]["update_id"] + 1) if updates else 0
            tg.write_state(OFFSET, {"offset": offset})
            continue
        updates, err = tg.api(cfg, "getUpdates", {
            "offset": offset, "timeout": POLL_SECONDS,
            "allowed_updates": '["message","callback_query"]'},
            timeout=POLL_SECONDS + 15, retries=1)
        if err:
            failures += 1
            tg.log(f"getUpdates failed ({err}); backing off")
            stop.wait(min(60, 5 * failures))
            continue
        failures = 0
        for update in updates or []:
            offset = update.get("update_id", 0) + 1
            tg.write_state(OFFSET, {"offset": offset})
            events.put(("telegram", update))


def main() -> int:
    hello_line = sys.stdin.readline()
    if not hello_line:
        return 0
    hello = json.loads(hello_line)
    if hello.get("type") != "hello" or hello.get("version") != 1:
        print(f"unsupported front door protocol: {hello_line[:200]}", file=sys.stderr)
        return 2
    tg.STATE_DIR = hello["state_dir"]
    os.makedirs(tg.STATE_DIR, exist_ok=True)
    with open(tg.state_path("adapter.pid"), "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))
    if not hello.get("config_path"):
        print("the Telegram door needs a config: privateer door add NAME --config PATH",
              file=sys.stderr)
        return 2
    cfg = tg.load_config(hello["config_path"])
    problem = tg.config_problem(cfg)
    if problem:
        print(f"Telegram door config: {problem}", file=sys.stderr)
        return 2
    kind = chat_kind(cfg)
    if kind != "private" and not cfg.get("allowed_user_ids"):
        print(f"Telegram door config: chat_id names a {kind}, and allowed_user_ids is not set; "
              "anyone in it would be heard as the captain, so set allowed_user_ids to your user id",
              file=sys.stderr)
        return 2
    import stt
    import tts
    for warning in stt.startup_warnings(cfg) + tts.startup_warnings(cfg):
        tg.log(warning)
    tg.api(cfg, "setMyCommands", {"commands": json.dumps(COMMANDS)}, retries=1)

    events: queue.Queue = queue.Queue()
    runtime = Runtime(events)
    door = Door(hello, cfg, runtime)
    door.group = kind != "private"
    stop = threading.Event()
    threading.Thread(target=poll_telegram, args=(cfg, events, stop),
                     name="telegram-poller", daemon=True).start()
    tg.log(f"door {hello.get('door')} up; plain messages go to {door.recipient() or 'nobody yet'}")
    next_sweep = 0.0
    while True:
        try:
            source, item = events.get(timeout=1)
        except queue.Empty:
            source, item = None, None
        try:
            if source == "eof":
                break
            if source == "runtime" and item.get("type") in ("mail", "digest"):
                door.on_mail(item)
            elif source == "runtime":
                tg.log(f"ignoring runtime message {item.get('type')!r}")
            elif source == "telegram":
                door.on_update(item)
        except BrokenPipeError:
            break
        except Exception as e:
            tg.log(f"handler error: {e!r}")
        if door.pending and time.monotonic() >= door.retry_at:
            door.flush()
        if time.monotonic() >= next_sweep:
            tg.sweep_inbox(float(cfg.get("inbox_days", 7)))
            next_sweep = time.monotonic() + SWEEP_EVERY
    stop.set()
    tg.log("door stopping: Privateer closed the connection")
    return 0


if __name__ == "__main__":
    sys.exit(main())
