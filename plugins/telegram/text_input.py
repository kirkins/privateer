"""Telegram ingress text policy; never shorten captain content.

Telegram entity offsets and the split-paste threshold count UTF-16 units.
VT, FF and NEL are pasted layout whitespace, normalized one-for-one to LF
before interpreting entities. Other Cc controls remain a visible refusal.
"""
import json
import re
import unicodedata

MAIL_BYTES = 65_536
# Leave room for runtime-owned envelope fields, file paths and topic metadata.
ENVELOPE_RESERVE = 2048
PASTE_THRESHOLD = 3500


def units(text):
    return len(text.encode("utf-16-le")) // 2


def normalize(text):
    return text.translate({0x0B: 0x0A, 0x0C: 0x0A, 0x85: 0x0A})


def rejected_control(text):
    return next(((i, ord(c)) for i, c in enumerate(text, 1)
                 if unicodedata.category(c) == "Cc" and c not in "\t\n\r"), None)


def bounded(text, limit=3000):
    """Bound adapter notices, never captain input; no split surrogate pair."""
    if units(text) <= limit:
        return text
    return text.encode("utf-16-le")[:2 * (limit - 1)].decode("utf-16-le", errors="ignore") + "…"


def command(text, entities, commands, username=""):
    # Only a complete known command token at offset zero. If Telegram supplies
    # entities, require its bot_command entity to match the whole token.
    match = re.match(r"^/([a-zA-Z0-9_]{1,32})(?:@([a-zA-Z0-9_]{1,32}))?(?=\s|$)", text)
    if not match or match[1].lower() not in commands:
        return False
    if match[2] and (not username or match[2].lower() != username.lower()):
        return False
    bot_entities = [e for e in entities or [] if isinstance(e, dict) and e.get("type") == "bot_command"]
    if bot_entities:
        return any(e.get("offset") == 0 and e.get("length") == match.end() for e in bot_entities)
    return True


def fits(args):
    # Rust serializes UTF-8, not Python's default ASCII escapes. The protocol
    # line itself stays small after spill, even with heavily escaped text.
    return len(json.dumps(args, ensure_ascii=False, separators=(",", ":")).encode()) <= MAIL_BYTES - ENVELOPE_RESERVE


def plain_part(update):
    msg = update.get("message") or {}
    if not isinstance(msg.get("text"), str) or not msg["text"]:
        return None
    if any(msg.get(k) for k in ("reply_to_message", "entities", "caption", "document", "photo",
                               "voice", "audio", "video", "animation", "sticker", "forward_origin")):
        return None
    # Even unrecognized slash-leading pastes are not continuation candidates.
    if msg["text"].lstrip().startswith("/"):
        return None
    return msg
