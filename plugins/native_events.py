"""Read native session-start evidence; never infer task success from text."""
import json


def session_started(harness, event):
    if not isinstance(event, dict):
        return None
    kind = event.get("event") if harness == "agy" else event.get("type")
    value = None
    if harness == "codex" and kind == "thread.started":
        value = event.get("thread_id")
    elif harness in ("rovo", "kimi", "devin", "grok", "muse") and kind == "session.started" and event.get("harness") == harness:
        value = event.get("session_id")
    elif harness in ("pi", "pi-signed", "gateway-pi", "omp") and kind == "session":
        value = event.get("id")
    elif harness in ("claude", "cursor") and kind == "system" and event.get("subtype") == "init":
        value = event.get("session_id")
    elif harness == "gemini" and kind == "init":
        value = event.get("session_id")
    elif harness == "agy" and kind == "init":
        value = event.get("conversation_id")
    elif harness in ("opencode", "opencode-v2") and kind == "step_start":
        value = event.get("sessionID")
    if isinstance(value, str) and 0 < len(value) <= 512 and not any(c.isspace() or ord(c) < 32 for c in value):
        return value
    return None


def read_session(harness, path):
    # Native session identity occurs at startup. Bound both total input and
    # individual lines, ignoring non-JSON diagnostics rather than guessing IDs.
    with open(path, "rb") as stream:
        remaining = 1024 * 1024
        while remaining > 0:
            line = stream.readline(min(remaining, 65537))
            remaining -= len(line)
            if not line:
                break
            if len(line) > 65536:
                return None
            try:
                value = session_started(harness, json.loads(line))
            except (ValueError, UnicodeError):
                continue
            if value:
                return value
    return None
