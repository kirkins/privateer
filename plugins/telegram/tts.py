#!/usr/bin/env python3
"""Text-to-speech output: the fleet's mail spoken as Telegram voice notes.

/voice (door.py) toggles the mode for the chat; with it on, every message
pushed to the captain is additionally spoken through a TTS API and sent as an
Ogg/Opus voice note (door.push -> maybe_speak). Notices can stay text-only
with [tts] speak_final_only=true. The text push always goes first and never depends on speech: an
absent [tts] section, a bad key, a slow API or a failed conversion degrades
to the ordinary text-only reply, with the reason in bridge.log and a
rate-limited note in the chat.

Every reply is spoken IN FULL, however long: the speakable prose is chunked on
sentence boundaries into per-request sizes the configured provider accepts
(openai 4096 chars per its published spec; cloudflare 1000, no published
limit, so the budget this bridge has long spoken end-to-end) and each chunk is
synthesized separately, then all parts are assembled by ffmpeg into ONE
Ogg/Opus voice note — a phone gets a single continuous bubble, never N
fragments and never just the beginning. A failed chunk fails the whole note
(a half-spoken message would lie); the text reply was already out.

Providers: "openai" (default) speaks the standard OpenAI speech shape — one
JSON POST to {base_url}/audio/speech (model, input, voice, response_format)
with raw audio back; any compatible gateway works, and response_format "opus"
is already Ogg/Opus, which Telegram voice notes want natively. "cloudflare"
speaks Workers AI's native shape instead, because Workers AI has NO
OpenAI-compatible speech route (verified 2026-10-09: .../ai/v1/audio/speech
answers 400 "No route for that URI" — the compat surface covers chat and
embeddings only): POST /client/v4/accounts/{account_id}/ai/run/{model} with
{"text": ...} for the Deepgram Aura models (raw MP3 back) or {"prompt": ...}
for @cf/myshell-ai/melotts ({"result":{"audio": <base64 WAV>}} back). Both
model families accept the other's field, so both are always sent. Whatever
arrives that is not already Ogg/Opus is converted with ffmpeg — voice-output
mode with anything but an opus-capable provider therefore requires ffmpeg.

The key hygiene is stt.py's: the [tts] api_key (or an api_key_secret Secret
Service lookup, the bot_token_secret pattern) rides to curl in a 0600 -H @file
header file, never argv, and every diagnostic string is blanked of it. The
mode itself is a state file (voice_mode.json). Every subprocess here runs
through tg.run_bounded: speech happens on the door's delivery path, and
nothing on that path may hang past its timeout.
"""

import base64
import json
import os
import re
import tempfile
import time

import tg

SUPPORTED_PROVIDERS = {"openai", "cloudflare"}
CF_BASE = "https://api.cloudflare.com/client/v4/accounts"
DEFAULTS = {
    # gpt-4o-mini-tts is OpenAI's current general-purpose TTS model; any
    # /audio/speech-compatible model name overrides it in config.
    "openai": {"model": "gpt-4o-mini-tts", "base": "https://api.openai.com/v1"},
    # melotts is the cheap, fast one (billed per audio minute); the Deepgram
    # Aura models (@cf/deepgram/aura-2-en et al.) sound better and bill per
    # character — see config.example.toml.
    "cloudflare": {"model": "@cf/myshell-ai/melotts", "base": ""},
}
DEFAULT_VOICE = "alloy"       # OpenAI's neutral default; CF ignores it unless set
DEFAULT_FORMAT = "opus"       # Ogg/Opus — what a Telegram voice note is
DEFAULT_TIMEOUT = 45          # one speech call's budget (aura-2 can take ~25s)
# Per-request synthesis input budget (characters), NOT a spoken-text cap — the
# whole reply is always spoken; this only decides where one request ends and
# the next begins. OpenAI documents /audio/speech input at maxLength 4096
# (openai-openapi CreateSpeechRequest, verified 2026-10-11). Cloudflare's
# model schemas publish no input limit, so cloudflare keeps the 1000-char
# budget this bridge has spoken end-to-end since the feature shipped.
PROVIDER_CHUNK_CHARS = {"openai": 4096, "cloudflare": 1000}
DEFAULT_CHUNK_CHARS = 4096

# Pure acknowledgements — a "nothing happened" signal that still gets a
# reply — are noise as voice bubbles; the quiet text bubble is the delivery.
# Whole-message match on the speakable prose only (whitespace-collapsed),
# never a substring: an acknowledgement embedded in a substantive reply still
# speaks. Kept as a small explicit list so the rule is visible and testable.
ACK_SHAPES = (
    "captain, shipshape",
    "shipshape, captain",
    "all clear, captain",
    "captain, all clear",
)

# The routine no-op acknowledgement itself — the handled-wake all-clear a
# supervisor answers with. door.py's suppress_noop_ack drops exactly
# this string from Telegram (the text push too, not just the voice note);
# aliased to ACK_SHAPES[0] so this matcher and the voice skip cannot drift.
NOOP_ACK = ACK_SHAPES[0]


def _ack_normal(text: str) -> str:
    """The match form the acknowledgement rules share: whitespace-collapsed,
    case-folded, final full stop tolerated."""
    return " ".join((text or "").split()).strip().rstrip(".").strip().lower()


def is_pure_acknowledgement(text: str) -> bool:
    """True when the whole message is a trivial all-clear — exactly one of the
    ACK_SHAPES, with or without its final full stop. Nothing else matches."""
    return _ack_normal(text) in ACK_SHAPES


def is_noop_acknowledgement(text: str) -> bool:
    """True when the whole message is the routine no-op acknowledgement —
    'Captain, shipshape.' itself — under the same match semantics as
    is_pure_acknowledgement: whitespace-collapsed, case-insensitive, with or
    without its final full stop, whole-message only. The other ACK_SHAPES are
    all-clears the voice skip spares too, but only this string is the no-op
    acknowledgement suppress_noop_ack drops from delivery."""
    return _ack_normal(text) == NOOP_ACK


VOICE_MODE = "voice_mode.json"
WARN_STATE = "tts_warn.json"
WARN_EVERY = 3600             # chat warnings about speech failures: at most 1/h

_key_cache: dict = {}


# --- config -------------------------------------------------------------------


def tts_config(cfg: dict) -> dict:
    """The [tts] table; {} when absent or not a table."""
    section = cfg.get("tts") if isinstance(cfg, dict) else None
    return section if isinstance(section, dict) else {}


def provider(cfg: dict) -> str:
    return str(tts_config(cfg).get("provider", "openai") or "openai").strip().lower()


def configured(cfg: dict) -> bool:
    """True when the [tts] table exists and is enabled — speech's opt-in.
    Absent or disabled means /voice reports the mode unavailable; replies are
    text-only regardless of the toggle (upstream behavior)."""
    section = tts_config(cfg)
    return bool(section) and bool(section.get("enabled", True))


def api_key(cfg: dict) -> str:
    """The TTS key: literal [tts] api_key, else a Secret Service lookup named by
    api_key_secret (the bot_token_secret pattern — attributes on the command
    line, the secret never there). The config.example placeholder counts as
    unset; a lookup that fails degrades to unset, never to some other source."""
    section = tts_config(cfg)
    value = str(section.get("api_key", "") or "").strip()
    if value and "REPLACE" not in value:
        return value
    spec = section.get("api_key_secret")
    if not isinstance(spec, dict) or not spec:
        return ""
    sig = tuple(sorted((str(k), str(v)) for k, v in spec.items()))
    if sig in _key_cache:
        return _key_cache[sig]
    secret, err = tg.secret_tool_lookup(spec)
    if err or not secret:
        tg.log(f"tts: api_key_secret {sorted(spec)!r} did not resolve"
               f"{f' ({err})' if err else ' (empty)'}; tts.api_key stays unset")
        _key_cache[sig] = ""
        return ""
    _key_cache[sig] = secret
    return secret


def account_id(cfg: dict) -> str:
    """The Cloudflare account id, placeholder-guarded like every secret-ish key
    (config.example.toml's REPLACE_ME must behave exactly like unset)."""
    value = str(tts_config(cfg).get("account_id", "") or "").strip()
    return "" if "REPLACE" in value else value


def config_error(cfg: dict) -> str:
    """'' when speech can run; else the human problem, as a why-class."""
    section = tts_config(cfg)
    who = provider(cfg)
    if who not in SUPPORTED_PROVIDERS:
        return f'unknown tts provider "{who}" — "openai" and "cloudflare" are supported'
    if not api_key(cfg):
        return "tts.api_key is not set"
    if who == "cloudflare" and not account_id(cfg):
        return "tts.account_id is not set (cloudflare)"
    return ""


def unavailability(cfg: dict) -> str:
    """Why voice output cannot run right now, or "". The /voice toggle and the
    send path share this, so they always agree on what 'unavailable' means."""
    if not configured(cfg):
        return "no [tts] section in config.toml (voice output needs one)"
    return config_error(cfg)


def startup_warnings(cfg: dict) -> list:
    """Startup validation: warn
    only when the mode is ON and speech cannot run, because that combination
    silently produces text-only replies the operator asked to hear."""
    if not voice_mode():
        return []
    problem = unavailability(cfg)
    return [f"tts: {problem} — /voice is on but replies will stay text-only"] if problem else []


def redact(text: str, cfg: dict) -> str:
    """Blank the TTS key out of a diagnostic string (mirrors tg.redact)."""
    key = api_key(cfg)
    return text.replace(key, "<tts-key>") if key else text


# --- the /voice toggle ----------------------------------------------------------


def voice_mode() -> bool:
    return bool(tg.read_state(VOICE_MODE, {}).get("enabled", False))


def set_voice_mode(on: bool) -> None:
    tg.write_state(VOICE_MODE, {"enabled": bool(on), "ts": int(time.time())})


def voice_reply(cfg: dict, arg: str) -> str:
    """What /voice answers. No argument toggles; on/off/status are explicit.
    Turning on with broken or missing config reports why instead of a silent
    no-op; turning off always works."""
    arg = (arg or "").strip().lower()
    if arg not in ("", "on", "off", "status"):
        return "🗣 /voice on | off | status"
    on = voice_mode()
    problem = unavailability(cfg)
    if arg == "off" or (arg == "" and on):
        set_voice_mode(False)
        return "🔇 voice output off — replies stay text-only"
    if arg == "status":
        state = "🔊 voice output is on" if on else "🔇 voice output is off"
        return f"{state} — but unavailable: {problem}" if (on and problem) else state
    if problem:
        return f"🗣️ voice output is unavailable: {problem}"
    set_voice_mode(True)
    return "🔊 voice output on — replies will also arrive as spoken voice notes"


# --- markdown -> speakable prose -----------------------------------------------


OMITTED = {"code": "Code omitted.", "table": "Table omitted."}
HEADING_MARK = re.compile(r"^\s{0,3}#{1,6}\s+")
QUOTE_MARK = re.compile(r"^\s{0,3}(?:&gt;|>)\s?")
BULLET_MARK = re.compile(r"^(\s*)[-*+][ \t]+")
IMAGE = re.compile(r"!\[([^\]]*)\]\([^)]*\)")
LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")


def _speak_line(line: str) -> str:
    line = HEADING_MARK.sub("", line)
    line = QUOTE_MARK.sub("", line)
    line = BULLET_MARK.sub(r"\1", line)
    line = IMAGE.sub(lambda m: m.group(1) or "image", line)
    line = LINK.sub(r"\1", line)
    line = tg.INLINE_CODE.sub(r"\1", line)
    # reuse tg's conservative emphasis patterns: a lone * or _ inside a word
    # (5*3, snake_case) stays, delimiters around spans go
    line = tg.BOLD.sub(r"\1", line)
    line = tg.BOLD_ALT.sub(r"\1", line)
    line = tg.ITALIC.sub(r"\1", line)
    line = tg.ITALIC_ALT.sub(r"\1", line)
    line = tg.STRIKE.sub(r"\1", line)
    return line


def speech_text(text: str, cap: int = 0) -> str:
    """The reply as prose worth hearing: fenced code and tables become a short
    spoken placeholder (the text message carries them in full), markdown marks
    are stripped with tg's own emphasis patterns, and whitespace collapses to
    single spaces. Never capped by default — every reply is spoken in full,
    however long (speech_chunks splits the synthesis into provider-sized
    requests); an explicit positive `cap` ([tts] max_chars) still truncates on
    a sentence boundary when one lands in the back half of the budget."""
    parts = []
    for kind, body, _info in tg._blocks(text or ""):
        if kind in OMITTED:
            parts.append(OMITTED[kind])
        elif body.strip():
            parts.append(" ".join(_speak_line(ln) for ln in body.split("\n")))
    spoken = " ".join(p for p in parts if p)
    spoken = " ".join(spoken.split())
    # a reply that is entirely code or tables has nothing worth hearing — the
    # "omitted" placeholder alone would be a useless voice note
    if spoken and all(p in OMITTED.values() for p in parts if p):
        return ""
    if cap <= 0 or len(spoken) <= cap:
        return spoken
    cut = spoken[:cap]
    end = max(cut.rfind(". "), cut.rfind("! "), cut.rfind("? "))
    if end >= cap // 2:
        return cut[:end + 1]
    return cut


# --- chunking ------------------------------------------------------------------

SENTENCE_GAP = re.compile(r"(?<=[.!?]) (?=\S)")


def chunk_chars(cfg: dict) -> int:
    """The per-request synthesis input budget in characters: [tts] chunk_chars
    when set positive, else the provider's documented limit (openai 4096 per
    its published spec; cloudflare 1000 — no limit is published, and this is
    the budget the bridge has long synthesized without complaint). Not a cap:
    the whole reply is always spoken, this only sizes the requests."""
    section = tts_config(cfg)
    who = provider(cfg)
    try:
        value = int(section.get("chunk_chars", 0) or 0)
    except (TypeError, ValueError):
        value = 0
    return value if value > 0 else PROVIDER_CHUNK_CHARS.get(who, DEFAULT_CHUNK_CHARS)


def _split_long(sentence: str, limit: int) -> list:
    """One sentence longer than the budget, split at the last word boundary
    that fits (at the budget itself for a single unbroken token), so every
    piece is speakable alone and nothing is dropped."""
    pieces = []
    while len(sentence) > limit:
        cut = sentence.rfind(" ", 0, limit)
        if cut <= 0:
            cut = limit
        pieces.append(sentence[:cut])
        sentence = sentence[cut:].lstrip()
    pieces.append(sentence)
    return pieces


def speech_chunks(text: str, limit: int) -> list:
    """Speakable prose split into synthesis requests of at most `limit`
    characters, cut on sentence boundaries wherever one exists: sentences are
    packed greedily into full chunks so each request carries as much speech as
    the provider accepts, a sentence longer than the budget is word-split, and
    the chunks joined back together reproduce the input exactly. Chunking only
    moves where a request ends — never what is spoken."""
    text = " ".join((text or "").split())
    if not text:
        return []
    if limit <= 0 or len(text) <= limit:
        return [text]
    units = []
    for sentence in SENTENCE_GAP.split(text):
        units.extend(_split_long(sentence, limit) if len(sentence) > limit
                     else [sentence])
    chunks, current = [], ""
    for unit in units:
        if not current:
            current = unit
        elif len(current) + 1 + len(unit) <= limit:
            current = f"{current} {unit}"
        else:
            chunks.append(current)
            current = unit
    if current:
        chunks.append(current)
    return chunks


# --- the speech call ------------------------------------------------------------


def _run(cmd, timeout):
    """The subprocess seam (tests stub this; production runs tg.run_bounded —
    speech happens on the door's delivery path, where a helper that wedges past
    its timeout is not allowed to exist)."""
    return tg.run_bounded(cmd, timeout=timeout)


def _header_file(cfg: dict) -> str:
    """The Authorization header in a 0600 temp file for curl's `-H @file`:
    the key must not ride in argv where /proc can read it."""
    fd, path = tempfile.mkstemp(prefix="tts-hdr-", suffix=".txt")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(f"Authorization: Bearer {api_key(cfg)}\n")
    os.chmod(path, 0o600)
    return path


def _body_file(body: dict) -> str:
    """The JSON request body in a file: the reply text can run to thousands of
    characters, which has no business on a command line."""
    fd, path = tempfile.mkstemp(prefix="tts-body-", suffix=".json")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(body, f)
    os.chmod(path, 0o600)
    return path


def _err_message(data: bytes) -> str:
    """The human line out of an error body: OpenAI's {"error":{"message"}} or
    Workers AI's {"errors":[{"message"}]} shape, else the raw bytes."""
    try:
        doc = json.loads(data.decode(errors="replace") or "{}")
    except ValueError:
        return repr(data[:160])
    if isinstance(doc.get("error"), dict) and doc["error"].get("message"):
        return str(doc["error"]["message"])
    errs = doc.get("errors")
    if isinstance(errs, list) and errs and isinstance(errs[0], dict):
        return str(errs[0].get("message") or errs[0])
    return repr(data[:160])


def _sniff(data: bytes) -> str:
    if data[:4] == b"OggS":
        return "ogg"
    if data[:4] == b"RIFF":
        return "wav"
    if data[:3] == b"ID3" or (len(data) > 1 and data[0] == 0xFF and data[1] & 0xE0 == 0xE0):
        return "mp3"
    return ""


def synthesize(cfg: dict, text: str) -> tuple[bytes, str, str]:
    """Speak `text` -> (audio bytes, format name, error). The format is "opus"
    / "ogg" when the payload is already a Telegram-shaped Ogg, else whatever
    arrived ("mp3", "wav", "") for to_ogg to convert. Errors are key-redacted
    and safe to log."""
    section = tts_config(cfg)
    who = provider(cfg)
    if who not in SUPPORTED_PROVIDERS:
        return b"", "", f'unknown tts provider "{who}"'
    if not api_key(cfg):
        return b"", "", "tts.api_key is not set"
    model = str(section.get("model", "") or DEFAULTS[who]["model"])
    voice = str(section.get("voice", "") or DEFAULT_VOICE)
    try:
        timeout = int(section.get("timeout_secs", 0) or DEFAULT_TIMEOUT)
    except (TypeError, ValueError):
        timeout = DEFAULT_TIMEOUT
    if who == "openai":
        base = str(section.get("base_url", "") or DEFAULTS["openai"]["base"]).rstrip("/")
        url = f"{base}/audio/speech"
        fmt = str(section.get("response_format", "") or DEFAULT_FORMAT)
        body = {"model": model, "input": text, "voice": voice, "response_format": fmt}
    else:  # cloudflare — native run endpoint; both input fields, see module doc
        account = account_id(cfg)
        if not account:
            return b"", "", "tts.account_id is not set (cloudflare)"
        url = f"{CF_BASE}/{account}/ai/run/{model}"
        body = {"text": text, "prompt": text}
        if str(section.get("voice", "") or "").strip():
            body["voice"] = str(section["voice"]).strip()

    afd, audio = tempfile.mkstemp(prefix="tts-audio-")
    os.close(afd)
    hdr = _header_file(cfg)
    bodyf = _body_file(body)
    cmd = ["curl", "-sS", "-m", str(timeout), "-H", f"@{hdr}",
           "-H", "Content-Type: application/json",
           "-d", f"@{bodyf}", "-o", audio, "-w", "%{http_code} %{content_type}"]
    proxy = cfg.get("proxy", "")
    if proxy:
        cmd[1:1] = ["-x", proxy]
    cmd.append(url)
    try:
        rc, meta, errb, failure = _run(cmd, timeout + 15)
        if failure:
            return b"", "", redact(f"curl: {failure}", cfg)
        try:
            with open(audio, "rb") as f:
                data = f.read()
        except OSError as e:
            return b"", "", redact(repr(e), cfg)
        if rc != 0:
            detail = errb.decode(errors="replace").strip()[:160]
            return b"", "", redact(f"curl exit={rc} {detail}", cfg)
        code, _, ctype = meta.decode(errors="replace").strip().partition(" ")
        ctype = ctype.split(";")[0].strip().lower()
        try:
            code = int(code or 0)
        except ValueError:
            return b"", "", redact(f"unparseable response: {meta[:120]!r}", cfg)
        if code != 200:
            return b"", "", redact(f"http {code}: {_err_message(data)}", cfg)
        if ctype.startswith("audio/"):
            if "ogg" in ctype or "opus" in ctype:
                return data, "opus", ""
            return data, ctype.split("/", 1)[1] or _sniff(data), ""
        if ctype == "application/json":
            try:
                doc = json.loads(data.decode(errors="replace") or "{}")
            except ValueError:
                return b"", "", redact(f"bad json body: {data[:160]!r}", cfg)
            b64 = (doc.get("result") or {}).get("audio") if isinstance(doc.get("result"), dict) else None
            if not b64:
                return b"", "", redact(f"json response with no audio: {_err_message(data)}", cfg)
            try:
                return base64.b64decode(b64), "wav", ""
            except Exception as e:
                return b"", "", f"bad base64 audio: {e!r}"
        if data[:4] == b"OggS":  # no usable content-type: trust the magic bytes
            return data, "opus", ""
        return b"", "", f"unknown audio payload ({ctype or 'no content-type'}, {_sniff(data) or 'unrecognized'})"
    finally:
        for path in (hdr, bodyf, audio):
            try:
                os.unlink(path)
            except OSError:
                pass


# --- convert + deliver ------------------------------------------------------------


def to_ogg(cfg: dict, data: bytes, fmt: str) -> tuple[str, str]:
    """Audio bytes -> (path to an Ogg/Opus file Telegram accepts as a voice
    note, error). Already-Ogg payloads ship untouched; everything else converts
    through ffmpeg (mono 24 kHz, 32 kbps — phone voice quality, small uploads).
    ffmpeg missing or failing is an error class of its own, never a crash."""
    if fmt in ("ogg", "opus") and data[:4] == b"OggS":
        fd, path = tempfile.mkstemp(prefix="tts-voice-", suffix=".ogg")
        with os.fdopen(fd, "wb") as f:
            f.write(data)
        return path, ""
    fd, src = tempfile.mkstemp(prefix="tts-src-", suffix=f".{fmt or 'bin'}")
    with os.fdopen(fd, "wb") as f:
        f.write(data)
    out = src + ".ogg"
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", src,
           "-vn", "-c:a", "libopus", "-b:a", "32k", "-ar", "24000", "-ac", "1", out]
    try:
        rc, _out, errb, failure = _run(cmd, 120)
        if failure or rc != 0:
            detail = failure or errb.decode(errors="replace").strip()[:160]
            return "", f"ffmpeg failed: {detail}"
        return out, ""
    finally:
        try:
            os.unlink(src)
        except OSError:
            pass


def assemble_ogg(cfg: dict, parts: list) -> tuple[str, str]:
    """The chunk audio joined into ONE Ogg/Opus voice note: every part rides
    into a single ffmpeg run as its own input (decode + concat filter + the
    same libopus settings to_ogg uses), so mixed formats across chunks (mp3
    aura parts, ogg opus parts) still join cleanly and the phone gets one
    continuous bubble instead of N fragments. Any part ffmpeg cannot read
    fails the whole assembly — a half-spoken message would lie about its own
    content."""
    if not parts:
        return "", "no audio parts to assemble"
    paths = []
    try:
        for i, (data, fmt) in enumerate(parts):
            fd, path = tempfile.mkstemp(prefix=f"tts-part-{i:03d}-",
                                        suffix=f".{fmt or 'bin'}")
            with os.fdopen(fd, "wb") as f:
                f.write(data)
            paths.append(path)
        out = paths[0] + ".ogg"
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
        for path in paths:
            cmd += ["-i", path]
        cmd += ["-filter_complex",
                "".join(f"[{i}:a]" for i in range(len(paths)))
                + f"concat=n={len(paths)}:v=0:a=1[a]",
                "-map", "[a]", "-vn", "-c:a", "libopus", "-b:a", "32k",
                "-ar", "24000", "-ac", "1", out]
        rc, _out, errb, failure = _run(cmd, 60 + 60 * len(paths))
        if failure or rc != 0:
            detail = failure or errb.decode(errors="replace").strip()[:160]
            return "", f"ffmpeg failed: {detail}"
        return out, ""
    finally:
        for path in paths:
            try:
                os.unlink(path)
            except OSError:
                pass


def voice_duration(path: str) -> int:
    """Duration in seconds via ffprobe, 0 when it can't be told — Telegram
    infers it from the file, the explicit value just makes the bubble honest."""
    rc, out, _err, failure = _run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration",
         "-of", "csv=p=0", path], 20)
    if failure or rc != 0:
        return 0
    try:
        return int(float(out.decode(errors="replace").strip()))
    except (TypeError, ValueError):
        return 0


def _warn_chat(cfg: dict, text: str) -> None:
    """One speech-failure note per hour, at most: the text reply already went
    out, so the warning is a diagnostic, not an alert that must repeat."""
    chat = tg.chat_id(cfg)
    if not chat:
        return
    now = time.time()
    seen = tg.read_state(WARN_STATE, {})
    if now - float(seen.get("ts", 0) or 0) < WARN_EVERY:
        return
    tg.write_state(WARN_STATE, {"ts": now})
    tg.api(cfg, "sendMessage", {"chat_id": chat, "text": text})


def speak_final_only(cfg: dict) -> bool:
    """[tts] speak_final_only: with it true, notices still relay as text but
    never synthesize a voice note, so a chatty ship cannot flood the chat with
    audio; requests, decisions and replies are still spoken. Default false:
    every delivered message is spoken."""
    value = tts_config(cfg).get("speak_final_only", False)
    if isinstance(value, str):
        return value.strip().lower() in ("true", "1", "yes", "on")
    return bool(value)


def maybe_speak(cfg: dict, text: str, target: dict | None,
                intermediate: bool = False) -> None:
    """The one voice-output entry point, called by door.push once the
    text push went out. `intermediate` marks a notice: with
    [tts] speak_final_only it stays text-only. Never
    raises, never gates the text: every refusal logs its reason, and failures
    warn the chat at most hourly."""
    try:
        _speak(cfg, text, target, intermediate=intermediate)
    except Exception as e:
        tg.log(f"tts: speech path failed: {e!r}")


def _speak(cfg: dict, text: str, target: dict | None,
           intermediate: bool = False) -> None:
    """maybe_speak's body, kept apart so no failure escapes the contract."""
    if not voice_mode():
        return
    if intermediate and speak_final_only(cfg):
        tg.log("tts: speak_final_only — a notice stays text-only")
        return
    problem = unavailability(cfg)
    if problem:
        tg.log(f"tts: /voice is on but speech is unavailable: {problem}")
        _warn_chat(cfg, f"🗣️ voice output is unavailable: {problem} — replies stay text-only")
        return
    try:
        cap = int(tts_config(cfg).get("max_chars", 0) or 0)
    except (TypeError, ValueError):
        cap = 0
    spoken = speech_text(text, cap)   # uncapped by default: the full reply
    if not spoken:
        tg.log("tts: nothing speakable in the reply (code/table only) — no voice note")
        return
    if is_pure_acknowledgement(spoken):
        tg.log("tts: pure acknowledgement stays text-only — no voice note")
        return
    limit = chunk_chars(cfg)
    chunks = speech_chunks(spoken, limit)
    parts = []
    for idx, chunk in enumerate(chunks):
        data, fmt, err = synthesize(cfg, chunk)
        if err:
            where = f" (chunk {idx + 1}/{len(chunks)})" if len(chunks) > 1 else ""
            tg.log(f"tts: synthesize failed{where}: {err}")
            _warn_chat(cfg, f"🎙️ speech failed — {error_class(err)}; the text reply was sent")
            return
        parts.append((data, fmt))
    if len(parts) == 1:
        path, err = to_ogg(cfg, *parts[0])
    else:
        path, err = assemble_ogg(cfg, parts)
    if err:
        tg.log(f"tts: {err}")
        _warn_chat(cfg, f"🎙️ speech failed — {error_class(err)}")
        return
    try:
        result, err = tg.send_voice(cfg, path, duration=voice_duration(path))
        if err:
            tg.log(f"tts: sendVoice failed: {err}")
            _warn_chat(cfg, f"🎙️ voice delivery failed — {error_class(err)}")
            return
        if target:
            # replying to the voice bubble must route like replying to the text
            tg.remember_target((result or {}).get("message_id"), target,
                               int(cfg.get("msg_map_limit", tg.MSG_MAP_LIMIT)))
        size = os.path.getsize(path)
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass
    tail = f", {len(chunks)} chunks" if len(chunks) > 1 else ""
    tg.log(f"tts: voice note sent ({len(spoken)} chars spoken{tail}, {size} bytes opus)")


def error_class(err: str) -> str:
    """A short human why-class for a speech error — what the operator gets.
    The redacted raw string goes to the log, never to the chat."""
    e = (err or "").lower()
    if "api_key is not set" in e:
        return "no tts api_key configured"
    if "account_id is not set" in e:
        return "no cloudflare account_id configured"
    if "unknown tts provider" in e:
        return "unsupported tts provider"
    if "ffmpeg" in e:
        return "audio conversion failed (is ffmpeg installed?)"
    if " 401" in e or " 403" in e or "unauthorized" in e or "invalid api key" in e:
        return "the TTS API rejected the key"
    if " 429" in e or "rate" in e:
        return "TTS rate limit"
    if "http 4" in e:
        return "the TTS API rejected the request"
    if "http 5" in e:
        return "TTS server error"
    if "timeout" in e or "timed out" in e:
        return "network timeout"
    if "curl exit" in e:
        return "network error"
    if "bot_token" in e or "chat_id" in e:
        return "the bridge is not configured to send"
    if "telegram" in e or "sendvoice" in e:
        return "Telegram refused the voice note"
    return "speech failed"
