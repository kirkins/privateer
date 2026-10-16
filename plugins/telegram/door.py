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
- /status, /ships and /waiting answer from records, waking no one.

Telegram is the trust boundary: only the configured chat, and when set only
allowed_user_ids within it, is heard. Everything else is logged and dropped.
"""

import json
import os
import queue
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
    "/yolo on|off|status — tool approval mode for every first mate\n"
    "/stow [status|resume] — checkpoint every ship and hold new jobs, e.g. before an update\n"
    "/afk [until 8am] [instructions] — away: hold mail, only emergencies 🚨 get through\n"
    "/back — back from away (any ordinary message also brings you back)\n"
    "/held — what was held while you were away\n"
    "/voice [on|off] — speak messages as voice notes (needs [tts])\n"
    "/help — this message\n\n"
    "Photos and files are saved and the ship is handed their path. Voice notes are "
    "transcribed when [stt] is configured."
)

COMMANDS = [
    {"command": "to", "description": "Send to a ship, or pick one"},
    {"command": "status", "description": "What the fleet is doing"},
    {"command": "yolo", "description": "Fleet tool approvals: on, off or status"},
    {"command": "stow", "description": "Checkpoint the fleet: stow, status or resume"},
    {"command": "afk", "description": "Away: hold mail, emergencies only"},
    {"command": "back", "description": "Back from away, with a summary"},
    {"command": "held", "description": "What was held while you were away"},
    {"command": "ships", "description": "The fleet's ships"},
    {"command": "waiting", "description": "What is waiting on you"},
    {"command": "voice", "description": "Toggle spoken messages"},
    {"command": "help", "description": "How this works"},
]

OFFSET = "offset.json"
SENT = "sent.json"
OUTGOING = "outgoing.json"
TARGET = "target.json"
DECISIONS = "decisions.json"


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


class Door:
    def __init__(self, hello: dict, cfg: dict, runtime: Runtime):
        self.hello = hello
        self.cfg = cfg
        self.runtime = runtime
        self.pending: list[dict] = []
        self.sent = tg.read_state(SENT, [])
        self.retry_at = 0.0

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
            if mail.get("id") not in self.sent:
                err = self.push(event)
                if err:
                    tg.log(f"push of {event.get('ref')} failed ({err}); retrying in {RETRY_SECONDS}s")
                    self.retry_at = time.monotonic() + RETRY_SECONDS
                    return
                self.sent.append(mail.get("id"))
                del self.sent[:-SENT_LIMIT]
                tg.write_state(SENT, self.sent)
            self.runtime.delivered(event["ref"])
            self.pending.pop(0)

    def push(self, event: dict) -> str:
        """Send one message to the captain -> error, or ""."""
        mail, ship = event["mail"], event["ship"]
        kind = mail.get("kind", "notice")
        sender = mail.get("from") or ship
        body = mail.get("body") or ""
        if not body:
            body = "```json\n" + json.dumps(mail.get("payload") or {}, indent=2) + "\n```"
        if suppress_noop_ack(self.cfg, body):
            tg.log(f"{event['ref']}: a routine acknowledgement stays out of the chat")
            return ""
        header = f"{ICONS.get(kind, '✉️')} <b>{tg.escape_html(sender)}</b> · {kind}"
        if event.get("breakthrough"):
            header = "🚨 " + header + " · while you're away"
        elif mail.get("urgency") == "high":
            header = "🔴 " + header
        if kind in ("request", "decision"):
            header += " · reply to answer"
        # A reply threads under the captain's message it answers.
        reply_to = None
        if mail.get("correlation"):
            reply_to = tg.read_state(OUTGOING, {}).get(mail["correlation"])
        target = {"ship": ship, "mail": mail.get("id"), "kind": kind, "from": sender}
        markup = self.decision_markup(mail) if kind == "decision" else None
        pieces = tg.split_markdown(body, TEXT_LIMIT) or [body]
        markdown = self.cfg.get("markdown", True)
        for index, piece in enumerate(pieces):
            text = tg.md_to_html(piece) if markdown else tg.escape_html(piece)
            if index == 0:
                text = f"{header}\n{text}"
            last = index == len(pieces) - 1
            message_id, err = self.say(text, reply_to=reply_to if index == 0 else None,
                                       html=True, markup=markup if last else None)
            if err:
                return err
            tg.remember_target(message_id, target, int(self.cfg.get("msg_map_limit", tg.MSG_MAP_LIMIT)))
        try:
            import tts
            tts.maybe_speak(self.cfg, mail.get("body") or "", target,
                            intermediate=kind == "notice")
        except Exception as e:
            tg.log(f"tts unavailable: {e!r}")
        return ""

    def decision_markup(self, mail: dict) -> dict | None:
        """Buttons for a decision whose payload lists its options."""
        options = (mail.get("payload") or {}).get("options")
        if not isinstance(options, list) or not 1 <= len(options) <= 8:
            return None
        if not all(isinstance(o, str) and 0 < len(o) <= 64 for o in options):
            return None
        decisions = tg.read_state(DECISIONS, {})
        key = str(int(time.time() * 1000) % 10**10)
        decisions[key] = {"mail": mail.get("id"), "from": mail.get("from"), "options": options}
        while len(decisions) > 200:
            decisions.pop(next(iter(decisions)))
        tg.write_state(DECISIONS, decisions)
        return {"inline_keyboard": [[{"text": o, "callback_data": f"d:{key}:{i}"}]
                                    for i, o in enumerate(options)]}

    # --- captain messages in ---------------------------------------------

    def send(self, args: dict, msg_id, label: str) -> None:
        """Mail from the captain, with what the chat sees about it."""
        if self.cfg.get("send_kind") in ("request", "notice") and args.get("kind") is None:
            args["kind"] = self.cfg["send_kind"]
        try:
            result = self.runtime.call("send", args)
        except RuntimeError as e:
            if "already answered" in str(e) and args.get("kind") == "reply":
                # Answered elsewhere already: follow up instead of failing.
                args = dict(args, kind="request")
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
                return {"to": target["from"], "kind": "reply", "correlation": target["mail"],
                        "body": text}
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
        msg_id = msg.get("message_id")
        reply_to = (msg.get("reply_to_message") or {}).get("message_id")
        text = (msg.get("text") or "").strip()
        if text.startswith("/"):
            return self.command(text, msg_id)
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
        elif data.startswith("d:"):
            _, key, index = data.split(":", 2)
            decision = tg.read_state(DECISIONS, {}).get(key)
            if not decision:
                self.say("⚠️ that decision is no longer known; reply to its message instead.")
                return
            choice = decision["options"][int(index)]
            self.send({"to": decision["from"], "kind": "reply", "correlation": decision["mail"],
                       "body": choice}, msg.get("message_id"), f"decision: {choice}")
            tg.api(self.cfg, "editMessageReplyMarkup", {
                "chat_id": tg.chat_id(self.cfg), "message_id": msg.get("message_id"),
                "reply_markup": json.dumps({"inline_keyboard": []})}, retries=1)

    def command(self, text: str, msg_id) -> None:
        head, _, rest = text.partition(" ")
        name = head[1:].split("@")[0].lower()
        rest = rest.strip()
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
        text = (f"🌙 Away from {since}{until}. I'll hold everything except emergencies 🚨, "
                "at most three an hour. Any message brings you back.")
        if away.get("words"):
            text += f"\nYour instructions for the liaison: “{away['words']}”"
        self.say(text)

    def welcome(self, returned: dict | None, quiet: bool = True) -> None:
        """The records-only digest on the captain's return; the liaison's summary follows."""
        if not returned:
            if not quiet:
                self.say("You weren't away.")
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
        action = {"": "start", "status": "status", "resume": "resume"}.get(rest)
        if action is None:
            self.say("Usage: /stow [status|resume]", reply_to=msg_id)
            return
        out = self.runtime.call("stow", {"action": action})
        if action == "start":
            names = ", ".join(s["ship"] for s in out["ships"])
            self.say(f"🧳 Stowing the fleet: new jobs are held. Asked {names} to checkpoint. /stow status shows who is done.")
        elif action == "resume":
            self.say("⚓ Fleet resumed: new jobs start again.")
        elif not out.get("stow"):
            self.say("The fleet has never been stowed.")
        else:
            lines = []
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
                lines.append("The fleet has resumed.")
            self.say("\n".join(lines))

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

    def cmd_waiting(self) -> None:
        waiting = self.runtime.call("waiting")["waiting"]
        if not waiting:
            self.say("Nothing is waiting on you. ⚓")
            return
        self.say(f"{len(waiting)} waiting on you; reply to each to answer:")
        for item in waiting[:20]:
            # Shown again as replyable messages, without re-acknowledging anything.
            err = self.push({"ref": "", "ship": item["ship"], "mail": item["mail"]})
            if err:
                self.say(f"⚠️ could not show one: {err}")
                return

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
        caption = (msg.get("caption") or "").strip()
        args = self.route(caption or "(file)", msg_id, reply_to)
        if not args:
            return
        dest = self.fetch(file_id, name, msg_id)
        if not dest:
            return
        note = f"The captain sent a file: {dest}"
        args["body"] = f"{caption}\n\n{note}" if caption else note
        self.send(args, msg_id, f"file {os.path.basename(dest)}")

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
        body = stt.prompt_for(transcript)
        caption = (msg.get("caption") or "").strip()
        args["body"] = f"{caption}\n{body}" if caption else body
        if transcript.strip() and stt.stt_config(self.cfg).get("echo_transcript", True):
            shown = transcript.strip()
            shown = shown if len(shown) <= 400 else shown[:400].rstrip() + "…"
            self.say(f'🎙️ "{shown}"', reply_to=msg_id, quiet=True)
        self.send(args, msg_id, f"voice {stt.duration_note(voice)}")


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
    import stt
    import tts
    for warning in stt.startup_warnings(cfg) + tts.startup_warnings(cfg):
        tg.log(warning)
    tg.api(cfg, "setMyCommands", {"commands": json.dumps(COMMANDS)}, retries=1)

    events: queue.Queue = queue.Queue()
    runtime = Runtime(events)
    door = Door(hello, cfg, runtime)
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
            if source == "runtime" and item.get("type") == "mail":
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
