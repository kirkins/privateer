"""One realtime voice session through the official `codex app-server`.

Codex keeps its own managed ChatGPT login; this module never reads its
credential files, never sets or passes an API key (it strips the variables
that would let Codex bill one), and refuses anything but a `chatgpt` account
before realtime is called. The browser's WebRTC offer goes in and the SDP
answer comes out; audio then flows between the browser and OpenAI directly.

Sequence: initialize (experimental opt-in), initialized, account/read,
config/read (to name what must be turned off), thread/start (ephemeral,
read-only, untrusted approvals, empty scratch cwd, no environment, the
captain's own MCP servers, plugins, apps, hooks and other tools turned off,
the door's tools), thread/realtime/start (webrtc transport with the offer,
client-managed handoffs), then the answer in thread/realtime/sdp.
Stopping calls thread/realtime/stop, closes stdin, and escalates to SIGTERM
and SIGKILL of the app-server's whole process group.
"""

import json
import os
import queue
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time

CLIENT = {"name": "privateer-live-voice", "title": None, "version": "0.1.0"}
# Variables that would let Codex bill an API key instead of the managed login.
API_KEY_VARIABLES = ("OPENAI_API_KEY", "CODEX_API_KEY", "AZURE_OPENAI_API_KEY")
MESSAGE_LIMIT = 300
OFFER_LIMIT = 64 * 1024
# Codex features a delegated turn must not have: no shell, no browser or
# computer, no plugins, apps or hooks, no agents of its own. Names are those
# of codex-cli 0.162.1 (`codex features list`); realtime itself stays on.
FEATURES_OFF = (
    "apps", "browser_use", "browser_use_external", "computer_use", "hooks",
    "image_generation", "memories", "multi_agent", "plugins", "remote_plugin",
    "shell_tool", "skill_mcp_dependency_install", "tool_suggest", "unified_exec",
)
SECRET_PATTERNS = [
    re.compile(r"(?i)bearer\s+\S+"),
    re.compile(r"\bsk-[A-Za-z0-9_\-]{8,}"),
    re.compile(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]*"),
]


def redact(text, limit=MESSAGE_LIMIT) -> str:
    """Strip anything token-shaped and bound the length of server text."""
    text = str(text)
    for pattern in SECRET_PATTERNS:
        text = pattern.sub("[redacted]", text)
    return text if len(text) <= limit else text[:limit] + "..."


class SessionError(Exception):
    """The session could not start or failed; the text is safe to show."""


def check_offer(offer) -> str:
    if not isinstance(offer, str) or not offer.startswith("v=0") or len(offer) > OFFER_LIMIT:
        raise SessionError("the offer is not an SDP offer")
    if not any(line.startswith("m=audio") for line in offer.splitlines()):
        raise SessionError("the offer has no audio track")
    return offer


def lockdown(effective: dict) -> dict:
    """The thread/start `config` overrides that turn off the captain's own
    Codex extras, from Codex's effective config (config/read's `config`).

    Overrides merge into the captain's config rather than replace it (an
    empty `mcp_servers` table turns nothing off), so each MCP server and
    plugin the config names is disabled by name."""
    effective = effective if isinstance(effective, dict) else {}

    def names(table) -> list:
        return sorted(k for k in table if isinstance(k, str)) if isinstance(table, dict) else []

    return {
        "mcp_servers": {name: {"enabled": False} for name in names(effective.get("mcp_servers"))},
        "plugins": {name: {"enabled": False} for name in names(effective.get("plugins"))},
        "features": {name: False for name in FEATURES_OFF},
        "apps": {"_default": {"enabled": False}},
        "web_search": "disabled",
    }


class AppServer:
    """One `codex app-server` child on stdio, in its own process group.

    Responses are matched to waiting calls; every notification and server
    request goes to `on_message` on the reader thread."""

    def __init__(self, argv: list, cwd: str, on_message):
        env = dict(os.environ)
        for name in API_KEY_VARIABLES:
            env.pop(name, None)
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Server logs are not read: they can sit next to credentials.
            stderr=subprocess.DEVNULL,
            cwd=cwd,
            env=env,
            start_new_session=True,
        )
        self.on_message = on_message
        self.lock = threading.Lock()
        self.waiters: dict[int, queue.Queue] = {}
        self.next_id = 0
        self.alive = True
        threading.Thread(target=self._read, name="app-server-reader", daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if not isinstance(message, dict):
                continue
            if "method" not in message and "id" in message:
                waiter = self.waiters.pop(message["id"], None)
                if waiter:
                    waiter.put(message)
                continue
            self.on_message(message)
        self.alive = False
        for waiter in list(self.waiters.values()):
            waiter.put(None)
        self.waiters.clear()
        self.on_message(None)

    def send(self, message: dict) -> None:
        with self.lock:
            self.proc.stdin.write(json.dumps(message).encode() + b"\n")
            self.proc.stdin.flush()

    def call(self, method: str, params: dict, timeout: float) -> dict:
        """One request -> its result; SessionError on an error or a timeout."""
        with self.lock:
            self.next_id += 1
            ident = self.next_id
        waiter: queue.Queue = queue.Queue(maxsize=1)
        self.waiters[ident] = waiter
        if not self.alive:
            self.waiters.pop(ident, None)
            raise SessionError(f"{method}: the app-server exited")
        try:
            self.send({"id": ident, "method": method, "params": params})
        except OSError:
            self.waiters.pop(ident, None)
            raise SessionError(f"{method}: the app-server closed its input") from None
        try:
            response = waiter.get(timeout=timeout)
        except queue.Empty:
            self.waiters.pop(ident, None)
            raise SessionError(f"{method} timed out") from None
        if response is None:
            raise SessionError(f"{method}: the app-server exited")
        if "error" in response:
            error = response.get("error") or {}
            raise SessionError(f"{method} refused: {redact(error.get('message', ''))}")
        return response.get("result") or {}

    def notify(self, method: str, params=None) -> None:
        message = {"method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def respond(self, ident, result: dict) -> None:
        self.send({"id": ident, "result": result})

    def decline(self, ident, why: str) -> None:
        self.send({"id": ident, "error": {"code": -32000, "message": why}})

    def shutdown(self, grace: float) -> tuple[str, bool]:
        """Close stdin and wait; terminate, then kill the whole group.
        -> (how it ended, whether the group is gone)."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        outcome = "clean"
        try:
            self.proc.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            outcome = "terminated"
            self._signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                outcome = "killed"
                self._signal(signal.SIGKILL)
                self.proc.wait(timeout=grace)
        # The app-server may have started helpers. Its exit is not proof
        # they stopped, so the group is swept and checked.
        self._signal(signal.SIGKILL)
        return outcome, not self._group_alive()

    def _signal(self, sig) -> None:
        try:
            os.killpg(self.proc.pid, sig)
        except (ProcessLookupError, PermissionError, OSError):
            pass

    def _group_alive(self) -> bool:
        for _ in range(40):
            try:
                os.killpg(self.proc.pid, 0)
            except (ProcessLookupError, PermissionError):
                return False
            time.sleep(0.05)
        return True


def codex_argv(cfg: dict) -> list:
    """`codex app-server`, from `codex` in the config (a path or an argv list)."""
    codex = cfg.get("codex", "codex")
    argv = list(codex) if isinstance(codex, list) else [str(codex)]
    if not argv or not all(isinstance(a, str) and a for a in argv):
        raise SessionError("config: codex must be a path or a list of strings")
    if os.sep not in argv[0]:
        found = shutil.which(argv[0])
        if not found:
            raise SessionError(f"{argv[0]} not found on PATH; set codex in the config")
        argv[0] = found
    return argv + ["app-server"]


class VoiceSession:
    """One realtime session. Events go to `post(("voice", (session, message)))`;
    a None message means the app-server is gone."""

    def __init__(self, cfg: dict, post, tools: list, instructions: str, prompt: str):
        self.cfg = cfg
        self.post = post
        self.tools = tools
        self.instructions = instructions
        self.prompt = prompt
        self.server: AppServer | None = None
        self.scratch = None
        self.thread_id = None
        self.answer = None
        self.failure = None
        self.closed = False
        self.settled = threading.Event()
        self.ended = False

    def _on_message(self, message):
        # The answer and an early failure are watched here, on the reader
        # thread, because start() is waiting for them; the rest is the door's.
        if message is None:
            self.closed = True
            self.failure = self.failure or "the app-server exited"
            self.settled.set()
        else:
            method = message.get("method")
            params = message.get("params") or {}
            if method == "thread/realtime/sdp" and isinstance(params.get("sdp"), str):
                if self.answer is None:
                    self.answer = params["sdp"]
                self.settled.set()
            elif method == "thread/realtime/error":
                self.failure = self.failure or redact(params.get("message", "realtime error"))
                self.settled.set()
            elif method == "thread/realtime/closed":
                self.closed = True
                self.failure = self.failure or redact(params.get("reason") or "realtime closed")
                self.settled.set()
        self.post(("voice", (self, message)))

    def start(self, offer: str) -> str:
        """Run the sequence for this offer -> the SDP answer. On any failure
        the app-server is stopped and SessionError says why."""
        check_offer(offer)
        timeout = float(self.cfg.get("rpc_timeout_secs", 20))
        self.scratch = tempfile.TemporaryDirectory(prefix="live-voice-")
        try:
            self.server = AppServer(codex_argv(self.cfg), self.scratch.name, self._on_message)
        except OSError as error:
            self.close()
            raise SessionError(f"could not start codex app-server: {redact(error)}") from None
        try:
            server = self.server
            server.call("initialize", {
                "clientInfo": CLIENT,
                "capabilities": {"experimentalApi": True, "requestAttestation": False},
            }, timeout)
            server.notify("initialized")
            account = server.call("account/read", {"refreshToken": False}, timeout)
            login = account.get("account")
            mode = login.get("type") if isinstance(login, dict) else None
            if mode != "chatgpt":
                # An API key bills per use; the captain has not approved that.
                raise SessionError(f"refused: the Codex login is {mode or 'none'!r}, not a "
                                   "ChatGPT login (run `codex login`); realtime was not called")
            # What the captain's own Codex config would add to a delegated
            # turn; without it the door cannot turn those off, so it stops.
            effective = server.call("config/read", {"includeLayers": False,
                                                    "cwd": self.scratch.name}, timeout)
            if not isinstance(effective.get("config"), dict):
                raise SessionError("config/read returned no config; realtime was not called")
            thread = server.call("thread/start", {
                "cwd": self.scratch.name,
                "ephemeral": True,
                "sandbox": "read-only",
                "approvalPolicy": "untrusted",
                # No environment: no shell or file access for delegated turns.
                "environments": [],
                "config": lockdown(effective["config"]),
                "developerInstructions": self.instructions,
                "dynamicTools": self.tools,
            }, timeout)
            self.thread_id = (thread.get("thread") or {}).get("id")
            if not self.thread_id:
                raise SessionError("thread/start returned no thread")
            realtime = {
                "threadId": self.thread_id,
                "model": self.cfg.get("model", "gpt-live-1-codex"),
                "version": self.cfg.get("version", "v3"),
                "outputModality": "audio",
                "transport": {"type": "webrtc", "sdp": offer},
                "includeStartupContext": False,
                "clientManagedHandoffs": True,
            }
            if self.prompt:
                # Replaces Codex's own voice prompt, so it is opt-in.
                realtime["prompt"] = self.prompt
            server.call("thread/realtime/start", realtime, timeout)
            self.settled.wait(float(self.cfg.get("start_timeout_secs", 30)))
            if self.answer is None:
                raise SessionError(self.failure or "no SDP answer from realtime in time")
            return self.answer
        except SessionError:
            self.close()
            raise
        except OSError:
            self.close()
            raise SessionError("the app-server closed its input") from None

    def append(self, text: str, role: str = "user", speech: bool = False) -> None:
        """Put text into the live session; SessionError when it did not take."""
        if self.server is None or self.closed or not self.thread_id:
            raise SessionError("no live session")
        timeout = float(self.cfg.get("rpc_timeout_secs", 20))
        if speech:
            self.server.call("thread/realtime/appendSpeech",
                             {"threadId": self.thread_id, "text": text}, timeout)
        else:
            self.server.call("thread/realtime/appendText",
                             {"threadId": self.thread_id, "text": text, "role": role}, timeout)

    def close(self) -> dict:
        """Stop realtime and the app-server; safe to call more than once."""
        report = {}
        if self.ended:
            return report
        self.ended = True
        server = self.server
        if server is not None:
            if self.thread_id and not self.closed and server.alive:
                try:
                    server.call("thread/realtime/stop", {"threadId": self.thread_id},
                                float(self.cfg.get("rpc_timeout_secs", 20)))
                    report["stopped"] = True
                except SessionError as error:
                    report["stop_error"] = str(error)
            outcome, gone = server.shutdown(float(self.cfg.get("grace_secs", 3)))
            report.update(shutdown=outcome, process_group_gone=gone)
        if self.scratch is not None:
            self.scratch.cleanup()
            self.scratch = None
        self.closed = True
        return report
