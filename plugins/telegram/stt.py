#!/usr/bin/env python3
"""Speech-to-text for Telegram voice notes, via an OpenAI-compatible endpoint.

Carried over from the Firstmate Telegram Bridge. No
local faster-whisper fallback, no provider autodetect ladder, no transcoding —
one multipart POST to the configured provider's OpenAI-compatible
audio/transcriptions endpoint (Telegram voice notes are Ogg/Opus, which the
endpoint takes natively). Every supported provider speaks that same standard
shape, so only the defaults differ: "groq" is the tested convenience
(whisper-large-v3-turbo at api.groq.com), "openai" the plain OpenAI API
(whisper-1 at api.openai.com), and a base_url override points either at any
other compatible gateway. The transcript is submitted to the agent exactly
like a typed message — see door.handle_voice for the routing.

The API key lives only in config.toml's [stt] table (mode 600, next to
bot_token), or in the Secret Service. It is passed to curl via a `-H @file` header file so it never rides
in argv (/proc/<pid>/cmdline), the file is unlinked in a finally, and every
diagnostic string is blanked of the key before it is logged or shown — the same
treatment tg.redact gives the bot token.
"""

import json
import os
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True  # a release holds only its own files
import tg

# One provider table, because every provider speaks the same OpenAI
# audio/transcriptions shape: only the defaults differ, and a base_url or model
# override always wins — that is what makes any other OpenAI-compatible
# transcription gateway usable with either provider value.
PROVIDER_DEFAULTS = {
    "groq": {"model": "whisper-large-v3-turbo",   # the tested default convenience
             "base_url": "https://api.groq.com/openai/v1"},
    "openai": {"model": "whisper-1",              # the plain OpenAI API
               "base_url": "https://api.openai.com/v1"},
}
SUPPORTED_PROVIDERS = set(PROVIDER_DEFAULTS)
DEFAULT_TIMEOUT = 90  # longer than tg's 20s API calls: long clips upload slowly

# internal gateway gateway semantics (run_inbound.py): an empty/whitespace transcript
# must never be submitted as an empty prompt — the agent would answer nothing and
# could loop. The sentinel says what happened and tells it not to guess.
EMPTY_SENTINEL = ("[The user sent a voice message but it came through empty or "
                  "inaudible — speech-to-text returned no words. Do not guess at "
                  "the content; ask the user to resend or type it out.]")


# --- config -------------------------------------------------------------------


def stt_config(cfg: dict) -> dict:
    """The [stt] table; {} when absent or not a table."""
    section = cfg.get("stt") if isinstance(cfg, dict) else None
    return section if isinstance(section, dict) else {}


def provider(cfg: dict) -> str:
    return str(stt_config(cfg).get("provider", "groq") or "groq").strip().lower()


def configured(cfg: dict) -> bool:
    """True when the [stt] table exists and is enabled — the feature's opt-in.
    Absent or disabled means voice notes stay plain attachments (upstream behavior)."""
    section = stt_config(cfg)
    return bool(section) and bool(section.get("enabled", True))


def api_key(section: dict) -> str:
    value = str(section.get("api_key", "") or "").strip()
    return "" if "REPLACE" in value else value  # the config.example placeholder counts as unset


def config_error(cfg: dict) -> str:
    """"" when voice transcription can run; else the human problem, as a why-class.
    door.py refuses the voice note (and says so) rather than transcribing anyway."""
    section = stt_config(cfg)
    who = provider(cfg)
    if who not in SUPPORTED_PROVIDERS:
        return (f'unknown stt provider "{who}" — supported providers: '
                + ", ".join(sorted(f'"{p}"' for p in SUPPORTED_PROVIDERS)))
    if not api_key(section):
        return "stt.api_key is not set"
    return ""


def startup_warnings(cfg: dict) -> list:
    """Startup validation: problems worth a log line before the first voice note."""
    if not configured(cfg):
        return []
    problem = config_error(cfg)
    return [f"stt: {problem} — voice notes will be refused until config.toml is fixed"] if problem else []


def redact(text: str, section: dict) -> str:
    """Blank the STT key out of a diagnostic string (mirrors tg.redact for the token)."""
    key = api_key(section)
    return text.replace(key, "<stt-key>") if key else text


# --- the transcription call -----------------------------------------------------


def _header_file(section: dict) -> str:
    """Write the Authorization header to a 0600 temp file for curl's `-H @file`:
    the key must not ride in argv where /proc can read it."""
    fd, path = tempfile.mkstemp(prefix="stt-hdr-", suffix=".txt")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(f"Authorization: Bearer {api_key(section)}\n")
    os.chmod(path, 0o600)
    return path


def _curl(cmd, timeout):
    """The subprocess seam (tests stub this; production runs real curl)."""
    return subprocess.run(cmd, capture_output=True, timeout=timeout, check=False)


def transcribe(cfg: dict, audio_path: str) -> tuple[str, str]:
    """POST one audio file to the provider's transcription endpoint -> (transcript, error).

    The transcript comes back verbatim and may be empty/whitespace — callers apply
    the empty-sentinel. On failure the error string is key-redacted and safe to log.
    """
    section = stt_config(cfg)
    if not api_key(section):
        return "", "stt.api_key is not set"
    # door.py only transcribes after config_error cleared the provider; an unknown
    # one falls back to the tested defaults rather than guessing a gateway.
    defaults = PROVIDER_DEFAULTS.get(provider(cfg), PROVIDER_DEFAULTS["groq"])
    model = str(section.get("model", "") or defaults["model"])
    base = str(section.get("base_url", "") or defaults["base_url"]).rstrip("/")
    language = str(section.get("language", "") or "").strip()
    try:
        timeout = int(section.get("timeout_secs", 0) or DEFAULT_TIMEOUT)
    except (TypeError, ValueError):
        timeout = DEFAULT_TIMEOUT

    cmd = ["curl", "-sS", "-m", str(timeout), "-o", "-", "-w", "\n%{http_code}"]
    hdr = _header_file(section)
    try:
        cmd += ["-H", f"@{hdr}",
                "-F", f"file=@{audio_path}",
                "-F", f"model={model}",
                "-F", "response_format=json"]
        if language:
            cmd += ["-F", f"language={language}"]
        cmd.append(f"{base}/audio/transcriptions")
        try:
            out = _curl(cmd, timeout + 15)
        except Exception as e:  # timeout expired etc. — repr embeds argv, so redact
            return "", redact(repr(e), section)
        body = out.stdout.decode(errors="replace")
        if out.returncode != 0:
            detail = out.stderr.decode(errors="replace").strip()[:200]
            return "", redact(f"curl exit={out.returncode} {detail}", section)
        payload, _, status = body.rpartition("\n")
        try:
            code = int(status.strip() or 0)
        except ValueError:
            return "", redact(f"unparseable response: {body[:200]!r}", section)
        try:
            doc = json.loads(payload or "{}")
        except ValueError:
            return "", redact(f"bad json (http {code}): {payload[:200]!r}", section)
        if code != 200:
            message = doc["error"].get("message", "") if isinstance(doc.get("error"), dict) else ""
            return "", redact(f"http {code}: {message or payload[:200]!r}", section)
        return str(doc.get("text", "")), ""
    finally:
        try:
            os.unlink(hdr)
        except OSError:
            pass


def error_class(err: str) -> str:
    """A short human why-class for a transcription error — what the operator gets.
    The redacted raw string goes to the log, never to the chat."""
    e = (err or "").lower()
    if "api_key is not set" in e:
        return "no stt api_key configured"
    if "unknown stt provider" in e:
        return "unsupported stt provider"
    if " 401" in e or " 403" in e or "invalid api key" in e or "unauthorized" in e:
        return "Groq rejected the API key"
    if " 429" in e or "rate" in e:
        return "Groq rate limit"
    if "http 4" in e:
        return "Groq rejected the request"
    if "http 5" in e:
        return "Groq server error"
    if "timed out" in e or "timeout" in e:
        return "network timeout"
    if "curl exit" in e:
        return "network error"
    return "transcription failed"


def duration_note(voice: dict) -> str:
    """'12s' from Telegram's declared voice.duration, for logs (no local probing)."""
    try:
        secs = int(voice.get("duration") or 0)
        return f"{secs}s" if secs > 0 else "?s"
    except (TypeError, ValueError):
        return "?s"


# --- prompt semantics (hermes gateway) ------------------------------------------


def is_empty(transcript: str) -> bool:
    """True when the transcript has no letters or digits. Whisper returns "." or
    "..." for silent or noisy audio, and those are not something the captain said.
    isalnum (not an ASCII check) so letters in any script still count."""
    return not any(ch.isalnum() for ch in transcript)


def prompt_for(transcript: str) -> str:
    """What a successful transcription is submitted as. Non-empty: a plain quoted
    line — wrappers like "The user sent a voice message..." get read as
    meta-instructions; hermes found the bare quote is what an LLM treats as
    ordinary user speech. Empty, whitespace, or punctuation only: the sentinel
    above, never an empty prompt (the agent would answer nothing and could loop)."""
    if is_empty(transcript):
        return EMPTY_SENTINEL
    return f'"{transcript.strip()}"'
