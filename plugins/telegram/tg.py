#!/usr/bin/env python3
"""Shared plumbing for the Telegram front door.

Paths and logging, configuration and the bot token, the Telegram Bot API call,
file download, markdown rendered down to Telegram's HTML subset, and the small
state files that map Telegram messages to the captain's mail. Carried over from
the Firstmate Telegram Bridge with the Herdr parts removed: Privateer's mailbox
is the conversation's record, so nothing here reads agent transcripts.
"""

import json
import os
import re
import signal
import subprocess
import tempfile
import time
import tomllib
import urllib.parse
import urllib.request

# Set from the runtime's hello before anything is written.
STATE_DIR = os.environ.get("PRIVATEER_DOOR_STATE") or os.path.dirname(os.path.abspath(__file__))
LOG_MAX_BYTES = 2_000_000
MSG_MAP_LIMIT = 2000


def log(msg: str) -> None:
    """The door's own log. stderr also reaches the supervisor's door.log."""
    path = os.path.join(STATE_DIR, "telegram.log")
    os.makedirs(STATE_DIR, exist_ok=True)
    try:
        if os.path.getsize(path) > LOG_MAX_BYTES:
            os.replace(path, path + ".1")
    except OSError:
        pass
    with open(path, "a", encoding="utf-8") as f:
        f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {msg}\n")


def load_config(path: str) -> dict:
    """The door's private config.toml, with the bot token resolved."""
    with open(path, "rb") as f:
        return resolve_token(tomllib.load(f))


def run_bounded(cmd: list, timeout: float, grace: float = 5.0) -> tuple[int, bytes, bytes, str]:
    """Run a subprocess with a hang-proof timeout -> (returncode, stdout, stderr, error).

    subprocess.run(timeout=...) kills only the direct child and then keeps waiting
    for the output pipes to drain: a descendant that inherited them holds the
    write end open and the call never returns. Here the child heads its own
    session, expiry SIGKILLs the entire process group, and if an escaped
    descendant still holds a pipe after `grace` more seconds the output is
    abandoned rather than waited on.
    """
    try:
        proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                start_new_session=True)
    except Exception as e:
        return -1, b"", b"", repr(e)

    def kill_group() -> None:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except OSError:
            try:
                proc.kill()
            except OSError:
                pass

    try:
        out, err = proc.communicate(timeout=timeout)
        return proc.returncode, out, err, ""
    except subprocess.TimeoutExpired:
        kill_group()
    except BaseException:
        kill_group()
        raise
    try:
        out, err = proc.communicate(timeout=grace)
        return proc.returncode, out, err, f"timeout after {timeout}s"
    except subprocess.TimeoutExpired:
        note = f"timeout after {timeout}s (a descendant held the pipes past the kill)"
        for stream in (proc.stdout, proc.stderr):
            try:
                if stream:
                    stream.close()
            except OSError:
                pass
        try:
            proc.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            pass
        return -1, b"", b"", note


def secret_tool_lookup(attrs: dict) -> tuple[str, str]:
    """Fetch a secret from the Secret Service via secret-tool -> (secret, error).
    Only attribute names and values ride argv; the secret comes back on stdout,
    so it never appears in a command line, a log line or a file."""
    cmd = ["secret-tool", "lookup"]
    for key, value in attrs.items():
        cmd += [str(key), str(value)]
    code, out, _err, error = run_bounded(cmd, timeout=15)
    if error or code != 0:
        return "", error or f"secret-tool exited {code}"
    return out.decode("utf-8", "replace").rstrip("\n"), ""


def resolve_token(cfg: dict) -> dict:
    """`bot_token_secret = { service = "...", account = "..." }` resolves through
    secret-tool when no literal bot_token is set. A literal bot_token always
    wins; an unresolvable secret leaves the token unset (loud in the log)."""
    if str(cfg.get("bot_token", "")).strip():
        return cfg
    spec = cfg.get("bot_token_secret")
    if not isinstance(spec, dict) or not spec:
        return cfg
    token, err = secret_tool_lookup(spec)
    if err or not token:
        log(f"bot_token_secret {sorted(spec)!r} did not resolve ({err or 'empty'}); "
            "bot_token stays unset")
        return cfg
    cfg["bot_token"] = token
    return cfg


def chat_id(cfg: dict) -> str:
    value = str(cfg.get("chat_id", ""))
    return "" if "REPLACE" in value else value


def config_problem(cfg: dict) -> str:
    token = str(cfg.get("bot_token", ""))
    if not token or "REPLACE" in token:
        return "bot_token is not set (or bot_token_secret did not resolve)"
    if not chat_id(cfg):
        return "chat_id is not set"
    return ""


# --- Telegram API ------------------------------------------------------------


def curl_url_file(url: str) -> str:
    """The token-bearing URL as a 0600 curl config file instead of argv, so a
    same-user process listing sees only a throwaway path, never the token."""
    os.makedirs(STATE_DIR, exist_ok=True)
    fd, path = tempfile.mkstemp(prefix=".curl-url-", dir=STATE_DIR)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(f'url = "{url}"\n')
    os.chmod(path, 0o600)
    return path


def api(cfg: dict, method: str, params: dict, timeout: int = 20, retries: int = 3):
    """Call a Telegram Bot API method -> (result, error message).

    curl first (it has been the reliable TLS path), urllib as the fallback.
    """
    token = str(cfg.get("bot_token", ""))
    if not token or "REPLACE" in token:
        return None, "bot_token not set"
    base = cfg.get("api_base", "https://api.telegram.org").rstrip("/")
    url = f"{base}/bot{token}/{method}"

    urlfile = curl_url_file(url)
    cmd = ["curl", "-sm", str(timeout), "-K", urlfile]
    for k, v in params.items():
        cmd += ["--data-urlencode", f"{k}={v}"]
    proxy = cfg.get("proxy", "")
    if proxy:
        cmd[1:1] = ["-x", proxy]

    err = ""
    try:
        for attempt in range(max(1, retries)):
            code, out, _errb, failure = run_bounded(cmd, timeout=timeout + 10)
            if failure:
                err = redact(f"curl: {failure}", cfg)
            else:
                try:
                    body = json.loads(out.decode() or "{}")
                except ValueError:
                    body = {}
                if body.get("ok"):
                    return body.get("result"), ""
                if body.get("description"):
                    # Telegram answered: a refusal is not worth retrying.
                    return None, redact(f"telegram: {body.get('description')}", cfg)
                err = redact(f"curl exit={code} body={out[:200]!r}", cfg)
            if attempt + 1 < max(1, retries):
                time.sleep(1 + attempt)
    finally:
        try:
            os.unlink(urlfile)
        except OSError:
            pass

    try:
        data = urllib.parse.urlencode({k: str(v) for k, v in params.items()}).encode()
        with urllib.request.urlopen(urllib.request.Request(url, data=data),
                                    timeout=timeout + 5) as resp:
            body = json.loads(resp.read().decode())
        if body.get("ok"):
            return body.get("result"), ""
        return None, redact(f"{err}; api: {body.get('description')}", cfg)
    except Exception as e:
        return None, redact(f"{err}; urllib: {e!r}", cfg)


def send_voice(cfg: dict, path: str, duration: int = 0) -> tuple[dict | None, str]:
    """Send one Ogg/Opus file as a Telegram voice note -> (result, error)."""
    token = str(cfg.get("bot_token", ""))
    if not token or "REPLACE" in token:
        return None, "bot_token not set"
    chat = chat_id(cfg)
    if not chat:
        return None, "chat_id not set"
    base = cfg.get("api_base", "https://api.telegram.org").rstrip("/")
    urlfile = curl_url_file(f"{base}/bot{token}/sendVoice")
    cmd = ["curl", "-sm", "60", "-K", urlfile,
           "-F", f"voice=@{path}", "-F", f"chat_id={chat}"]
    if duration > 0:
        cmd += ["-F", f"duration={int(duration)}"]
    proxy = cfg.get("proxy", "")
    if proxy:
        cmd[1:1] = ["-x", proxy]
    try:
        rc, out, errb, failure = run_bounded(cmd, timeout=90)
        if failure:
            return None, redact(f"curl: {failure}", cfg)
        if rc != 0:
            detail = errb.decode(errors="replace").strip()[:160]
            return None, redact(f"curl exit={rc} {detail}", cfg)
        try:
            body = json.loads(out.decode(errors="replace") or "{}")
        except ValueError:
            return None, redact(f"unparseable response: {out[:160]!r}", cfg)
        if body.get("ok"):
            return body.get("result") or {}, ""
        return None, redact(f"telegram: {body.get('description') or out[:160]!r}", cfg)
    finally:
        try:
            os.unlink(urlfile)
        except OSError:
            pass


def redact(text: str, cfg: dict) -> str:
    """Blank the bot token out of a diagnostic string."""
    token = str(cfg.get("bot_token", ""))
    return text.replace(token, "<token>") if token else text


def inbox_dir() -> str:
    path = os.path.join(STATE_DIR, "inbox")
    os.makedirs(path, exist_ok=True)
    return path


def safe_name(name: str, limit: int = 80) -> str:
    """A sender-supplied filename is untrusted: keep a flat, boring basename so
    nothing can climb out of the inbox, and keep the suffix."""
    name = re.sub(r"[^\w.\-]+", "_", os.path.basename(name or "")).strip("._")
    if not name:
        return "file"
    if len(name) > limit:
        stem, dot, ext = name.rpartition(".")
        ext = f".{ext}" if dot and 0 < len(ext) <= 10 else ""
        name = (stem or name)[:limit - len(ext)] + ext
    return name


def download(cfg: dict, file_path: str, dest: str) -> str:
    """Fetch a Telegram file to dest. Returns an error message, or ""."""
    token = str(cfg.get("bot_token", ""))
    base = cfg.get("api_base", "https://api.telegram.org").rstrip("/")
    urlfile = curl_url_file(f"{base}/file/bot{token}/{file_path}")
    cmd = ["curl", "-sfL", "-m", "120", "-o", dest, "-K", urlfile]
    proxy = cfg.get("proxy", "")
    if proxy:
        cmd[1:1] = ["-x", proxy]
    try:
        rc, _out, errb, failure = run_bounded(cmd, timeout=180)
    finally:
        try:
            os.unlink(urlfile)
        except OSError:
            pass
    if failure:
        return redact(failure, cfg)
    if rc != 0:
        return redact(f"curl exit={rc} {errb.decode(errors='replace').strip()[:160]}", cfg)
    try:
        return "" if os.path.getsize(dest) else "downloaded 0 bytes"
    except OSError as e:
        return repr(e)


def sweep_inbox(days: float) -> int:
    """Delete inbox files older than `days`. Returns how many went."""
    if days <= 0:
        return 0
    cutoff = time.time() - days * 86400
    gone = 0
    for name in os.listdir(inbox_dir()):
        path = os.path.join(inbox_dir(), name)
        try:
            if os.path.isfile(path) and os.path.getmtime(path) < cutoff:
                os.remove(path)
                gone += 1
        except OSError:
            pass
    return gone


# --- markdown -> Telegram HTML -----------------------------------------------
#
# Telegram accepts a fixed, tiny set of tags (b/i/u/s/code/pre/a/blockquote)
# and nothing else, so mail bodies (GitHub-flavoured markdown) are rendered
# down to it: headings become bold, lists become bullets, tables become
# monospaced blocks.

TABLE_RULE = re.compile(r"^\s*\|?[\s:|-]*-[\s:|-]*\|[\s:|-]*$")
FENCE = re.compile(r"^\s*(```|~~~)(.*)$")
HEADING = re.compile(r"^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$")
BULLET = re.compile(r"^(\s*)[-*+][ \t]+")
QUOTE = re.compile(r"^\s{0,3}(?:&gt;|>)[ \t]?")  # runs after escaping, so match both forms
RULE = re.compile(r"^\s{0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$")
INLINE_CODE = re.compile(r"`([^`\n]+)`")
LINK = re.compile(r"\[([^\]\n]*)\]\(([^)\s]+)\)")
BOLD = re.compile(r"\*\*(.+?)\*\*", re.S)
BOLD_ALT = re.compile(r"(?<!\w)__(.+?)__(?!\w)", re.S)
ITALIC = re.compile(r"(?<![\w*])\*([^*\n]+?)\*(?![\w*])")
ITALIC_ALT = re.compile(r"(?<![\w_])_([^_\n]+?)_(?![\w_])")
STRIKE = re.compile(r"~~(.+?)~~", re.S)
STASHED = re.compile(r"\x00(\d+)\x00")


def escape_html(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def _blocks(text: str):
    """Split markdown into ("code"|"table"|"text", body, info) runs."""
    lines = text.split("\n")
    i, plain = 0, []
    while i < len(lines):
        fence = FENCE.match(lines[i])
        table = (
            "|" in lines[i]
            and i + 1 < len(lines)
            and TABLE_RULE.match(lines[i + 1])
            and "|" in lines[i + 1]
        )
        if fence:
            if plain:
                yield "text", "\n".join(plain), ""
                plain = []
            marker, info, i = fence.group(1), fence.group(2).strip(), i + 1
            body = []
            while i < len(lines) and not lines[i].strip().startswith(marker):
                body.append(lines[i])
                i += 1
            i += 1  # closing fence (or end of input)
            yield "code", "\n".join(body), info
        elif table:
            if plain:
                yield "text", "\n".join(plain), ""
                plain = []
            body = []
            while i < len(lines) and "|" in lines[i]:
                body.append(lines[i].strip())
                i += 1
            yield "table", "\n".join(body), ""
        else:
            plain.append(lines[i])
            i += 1
    if plain:
        yield "text", "\n".join(plain), ""


def _render_text(text: str) -> str:
    stash: list[str] = []

    def keep(m):
        stash.append(m.group(1))
        return f"\x00{len(stash) - 1}\x00"

    text = INLINE_CODE.sub(keep, text)
    text = escape_html(text)

    out = []
    for ln in text.split("\n"):
        if RULE.match(ln):
            out.append("─" * 24)
            continue
        head = HEADING.match(ln)
        if head:
            out.append(f"<b>{head.group(1).replace('**', '')}</b>")
            continue
        ln = QUOTE.sub("│ ", ln)
        out.append(BULLET.sub(lambda m: m.group(1) + "• ", ln))
    text = "\n".join(out)

    text = LINK.sub(lambda m: f'<a href="{m.group(2).replace(chr(34), "&quot;")}">'
                              f"{m.group(1) or m.group(2)}</a>", text)
    text = BOLD.sub(r"<b>\1</b>", text)
    text = BOLD_ALT.sub(r"<b>\1</b>", text)
    text = ITALIC.sub(r"<i>\1</i>", text)
    text = ITALIC_ALT.sub(r"<i>\1</i>", text)
    text = STRIKE.sub(r"<s>\1</s>", text)
    return STASHED.sub(lambda m: f"<code>{escape_html(stash[int(m.group(1))])}</code>", text)


def md_to_html(text: str) -> str:
    """Render markdown into the HTML subset Telegram's parse_mode=HTML accepts."""
    out = []
    for kind, body, info in _blocks(text):
        if kind in ("code", "table"):
            # tag tables too: an untagged <pre> gets a guessed language and
            # a plain table syntax-highlighted into confetti
            lang = info.split()[0] if (kind == "code" and info.split()) else "plaintext"
            out.append(f'<pre><code class="language-{escape_html(lang)}">'
                       f"{escape_html(body)}</code></pre>")
        elif body.strip():
            out.append(_render_text(body))
    return "\n".join(out)


def split_markdown(text: str, limit: int) -> list[str]:
    """Chunk markdown so each piece fits one Telegram message, never cutting a
    fenced code block in half: an open fence is closed and reopened across the seam."""
    chunks: list[str] = []
    cur: list[str] = []
    size = 0
    fence = info = ""

    def flush(reopen: bool) -> None:
        nonlocal cur, size
        if fence:
            cur.append(fence)
        chunks.append("\n".join(cur))
        cur = [f"{fence}{info}"] if (reopen and fence) else []
        size = sum(len(x) + 1 for x in cur)

    for line in text.split("\n"):
        while len(line) > limit:  # a single monstrous line
            if cur:
                flush(True)
            cut = limit - size
            cur.append(line[:cut])
            size += cut + 1
            flush(True)
            line = line[cut:]
        if cur and size + len(line) + 1 > limit:
            flush(True)
        m = FENCE.match(line)
        if m:
            if fence and line.strip().startswith(fence):
                fence = info = ""
            elif not fence:
                fence, info = m.group(1), m.group(2).strip()
        cur.append(line)
        size += len(line) + 1
    if cur:
        flush(False)
    return [c for c in chunks if c.strip()]


# --- state files -------------------------------------------------------------


def state_path(name: str) -> str:
    return os.path.join(STATE_DIR, name)


def read_state(name: str, default):
    try:
        with open(state_path(name), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return default


def write_state(name: str, obj) -> None:
    """Atomic write: a crash leaves the old file or the new one, never half."""
    os.makedirs(STATE_DIR, exist_ok=True)
    tmp = state_path(f"{name}.{os.getpid()}.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f)
    os.replace(tmp, state_path(name))


MESSAGES = "messages.json"


def remember_target(message_id, target: dict, limit: int = MSG_MAP_LIMIT) -> None:
    """Record which mail a Telegram message carries, so a reply to it finds its
    way back. Bounded: the oldest entries go first."""
    if not message_id or not target:
        return
    entries = read_state(MESSAGES, {})
    entries.pop(str(message_id), None)
    entries[str(message_id)] = target
    while len(entries) > max(1, limit):
        entries.pop(next(iter(entries)))
    write_state(MESSAGES, entries)


def target_for_reply(message_id) -> dict:
    return read_state(MESSAGES, {}).get(str(message_id), {})
