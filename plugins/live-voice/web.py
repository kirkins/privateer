"""The door's local page and its small JSON API, on 127.0.0.1 only.

Even on this machine the page is guarded:
- Pairing: at start the door writes a random one-time code to a 0600 file in
  its state directory. The page trades it once for a random session token;
  every API call carries the token. Wrong codes are rate limited, and after a
  few the code is replaced.
- Host must be 127.0.0.1 or localhost on the door's port (against DNS
  rebinding); an Origin, when sent, must be that same origin or one listed in
  `allowed_origins`; POST bodies must be JSON (so another site cannot post a
  simple form). Only a listed Origin gets CORS headers: its exact origin,
  never "*", and no credentials (the token travels in a header).
- Nothing the browser receives carries a Codex or ChatGPT credential: the
  browser only ever gets the SDP answer.

The API is client protocol version 1, written down in PROTOCOL.md: every
error is {"error": "<message>", "code": "<stable code>"}.
"""

import hmac
import http.server
import json
import os
import re
import secrets
import socketserver
import sys
import threading
import time

PAGE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "page")
STATIC = {"/": ("index.html", "text/html; charset=utf-8"),
          "/app.js": ("app.js", "text/javascript; charset=utf-8")}
CODE_FILE = "pairing-code"
TOKEN_FILE = "session-token.json"
CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
CODE_LENGTH = 10
FAILURES_PER_WINDOW = 5
FAILURE_WINDOW = 60.0
BODY_LIMIT = 128 * 1024
# Drain ordinary oversize uploads before replying, so closing a socket with
# unread bytes does not turn a useful 413 into a reset on the sender.
BODY_DRAIN_MULTIPLE = 4
BODY_DRAIN_SECS = 1.0
PROTOCOL = 1
# Additive changes within version 1 are named here; breaking ones bump PROTOCOL.
FEATURES = ["session_client", "draft_actions", "operator_instructions", "client_tools",
            "client_notices", "client_context", "client_result_large", "client_result_untrusted",
            "client_context_object"]
# Each API path and the one method it answers.
API = {"/api/version": "GET", "/api/state": "GET", "/api/pair": "POST",
       "/api/session/start": "POST", "/api/session/connected": "POST",
       "/api/session/stop": "POST", "/api/draft/confirm": "POST",
       "/api/draft/cancel": "POST", "/api/tool/result": "POST", "/api/say": "POST"}
TOKEN_HEADER = "X-Privateer-Token"
CORS_HEADERS = f"Content-Type, {TOKEN_HEADER}"
CORS_MAX_AGE = "600"
EXTENSION_SCHEMES = ("chrome-extension", "moz-extension", "safari-web-extension")
LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "[::1]")
ORIGIN = re.compile(r"([a-z][a-z0-9+.-]*)://([A-Za-z0-9.-]+|\[::1\])(?::([0-9]+))?")
HEADERS = {
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Resource-Policy": "same-origin",
    "Content-Security-Policy": ("default-src 'none'; script-src 'self'; style-src 'self' "
                                "'unsafe-inline'; connect-src 'self'; media-src 'self' blob: "
                                "mediastream:; base-uri 'none'; form-action 'none'; "
                                "frame-ancestors 'none'"),
}


def origin_problem(entry) -> str:
    """Why `entry` cannot be an allowed origin -> "" when it can.

    An exact scheme://host[:port] as a browser sends it in Origin: no
    wildcard, no "null", no path. A browser extension's origin, or an http(s)
    origin on this machine's loopback; any other web origin is a site the
    captain does not control, so it is refused."""
    if not isinstance(entry, str):
        return "each origin must be a string"
    if entry == "null":
        return '"null" is not an origin a door can trust'
    if "*" in entry:
        return f"{entry!r}: no wildcards; list each exact origin"
    match = ORIGIN.fullmatch(entry)
    if not match:
        return (f"{entry!r} is not an exact origin: scheme://host[:port], lower-case scheme, "
                "no path or trailing slash")
    scheme, host, port = match.groups()
    if scheme in EXTENSION_SCHEMES:
        if port is not None:
            return f"{entry!r}: an extension origin has no port"
        return ""
    if scheme not in ("http", "https"):
        return (f"{entry!r}: the scheme must be one of {', '.join(EXTENSION_SCHEMES)}, "
                "or http(s) on loopback")
    if host not in LOOPBACK_HOSTS:
        return f"{entry!r}: an http(s) origin must be loopback ({', '.join(LOOPBACK_HOSTS)})"
    if port is not None:
        if port.startswith("0") or not 1 <= int(port) <= 65535:
            return f"{entry!r}: the port must be a number from 1 to 65535"
        if port == {"http": "80", "https": "443"}[scheme]:
            return f"{entry!r}: leave out the default port; a browser sends {scheme}://{host}"
    return ""


def write_private(path: str, text: str) -> None:
    """Write a file only its owner can read, atomically."""
    tmp = f"{path}.{os.getpid()}.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


class Pairing:
    """The one-time code and the paired page's token."""

    def __init__(self, state_dir: str, clock=time.monotonic):
        self.state_dir = state_dir
        self.clock = clock
        self.lock = threading.Lock()
        self.failures: list[float] = []
        self.code = ""
        self.new_code()
        self.token = self._load_token()

    def new_code(self) -> None:
        self.code = "".join(secrets.choice(CODE_ALPHABET) for _ in range(CODE_LENGTH))
        write_private(os.path.join(self.state_dir, CODE_FILE), self.code + "\n")

    def _load_token(self) -> str:
        try:
            with open(os.path.join(self.state_dir, TOKEN_FILE), encoding="utf-8") as f:
                token = json.load(f).get("token")
            return token if isinstance(token, str) and len(token) >= 32 else ""
        except (OSError, ValueError, AttributeError):
            return ""

    def pair(self, code: str) -> tuple[int, str]:
        """-> (HTTP status, token or why not)."""
        with self.lock:
            now = self.clock()
            self.failures = [t for t in self.failures if now - t < FAILURE_WINDOW]
            if len(self.failures) >= FAILURES_PER_WINDOW:
                return 429, "too many wrong codes; wait a minute"
            given = "".join(str(code).split()).upper()
            if not self.code or not hmac.compare_digest(given.encode(), self.code.encode()):
                self.failures.append(now)
                if len(self.failures) >= FAILURES_PER_WINDOW:
                    # A guesser does not get to keep trying the same code.
                    self.new_code()
                return 403, "wrong pairing code"
            # Used once: the file is emptied and the code forgotten. A new
            # token replaces any earlier page's.
            self.code = ""
            write_private(os.path.join(self.state_dir, CODE_FILE), "")
            self.token = secrets.token_urlsafe(32)
            write_private(os.path.join(self.state_dir, TOKEN_FILE),
                          json.dumps({"token": self.token}))
            return 200, self.token

    def check(self, token: str) -> bool:
        with self.lock:
            return bool(self.token) and bool(token) and hmac.compare_digest(
                token.encode(), self.token.encode())


class LoopbackServer(http.server.ThreadingHTTPServer):
    """HTTPServer.server_bind names itself with socket.getfqdn(host), a
    reverse DNS lookup that can stall startup for many seconds where the
    resolver is slow (macOS runners). The page needs no server name, so
    the door never asks the resolver anything."""

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


def serve(port: int, pairing: Pairing, api, allowed_origins=(), door_version: str = "",
          body_limits=None) -> http.server.ThreadingHTTPServer:
    """Start the server on 127.0.0.1:port in a thread -> the server.

    `api` has `state()`, `start(offer, client, tools, context) -> answer`,
    `connected()`, `stop()`, `confirm_draft(body)`, `cancel_draft(body)`,
    `client_tool_result(body)` and `say(body)`; each may raise ApiError. `allowed_origins` were checked with
    origin_problem. `body_limits` raises BODY_LIMIT for the paths it names,
    for a paired client only."""
    server = LoopbackServer(("127.0.0.1", port),
                            _handler(pairing, api, frozenset(allowed_origins), door_version,
                                     dict(body_limits or {})))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, name="web", daemon=True).start()
    return server


class ApiError(Exception):
    """An error reply: its status, a stable code (PROTOCOL.md) and a message."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(message)
        self.status = status
        self.code = code


PAIR_CODES = {403: "wrong_code", 429: "rate_limited"}


def _handler(pairing: Pairing, api, allowed: frozenset, door_version: str, body_limits: dict):
    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = "privateer-live-voice"
        sys_version = ""

        def log_message(self, *_):
            pass

        cors = None

        def origins(self) -> set:
            port = self.server.server_address[1]
            return {f"127.0.0.1:{port}", f"localhost:{port}"}

        def path_only(self) -> str:
            return self.path.split("?", 1)[0]

        def common_headers(self) -> None:
            for name, value in HEADERS.items():
                self.send_header(name, value)
            # Only a listed origin may read the reply, and only its own.
            self.send_header("Vary", "Origin")
            if self.cors:
                self.send_header("Access-Control-Allow-Origin", self.cors)

        def reply(self, status: int, body: bytes, kind: str = "application/json") -> None:
            self.send_response(status)
            self.common_headers()
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def json(self, status: int, obj: dict) -> None:
            self.reply(status, json.dumps(obj).encode())

        def fail(self, status: int, code: str, message: str) -> None:
            # Early errors (host/origin, route, media type, pairing) can
            # otherwise close over an upload still in progress and reset it.
            if not getattr(self, "body_read", False):
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                except ValueError:
                    length = -1
                self.discard_body(length, BODY_LIMIT)
            self.json(status, {"error": message, "code": code})

        def guard(self) -> bool:
            """Host and Origin checks; False after answering."""
            origin = self.headers.get("Origin")
            listed = origin is not None and origin in allowed
            self.cors = origin if listed else None
            if self.headers.get("Host", "") not in self.origins():
                self.fail(403, "wrong_host", "wrong host")
                return False
            if origin is not None and not listed \
                    and origin not in {f"http://{o}" for o in self.origins()}:
                self.fail(403, "wrong_origin", "wrong origin")
                return False
            # A listed origin is another site by definition (an extension,
            # another loopback port): cross-site is what it sends.
            site = self.headers.get("Sec-Fetch-Site")
            if site is not None and not listed and site not in ("same-origin", "none"):
                self.fail(403, "cross_site", "cross-site request")
                return False
            return True

        def authorized(self) -> bool:
            if pairing.check(self.headers.get(TOKEN_HEADER, "")):
                return True
            self.fail(401, "not_paired", "not paired")
            return False

        def discard_body(self, length: int, limit: int) -> None:
            """Best-effort drain, bounded in both bytes and wall time.

            Invalid or enormous lengths cannot be drained safely. A stalled
            or truncated upload must not keep a request thread forever.
            """
            self.close_connection = True
            self.body_read = True
            if not 0 < length <= limit * BODY_DRAIN_MULTIPLE:
                return
            timeout = self.connection.gettimeout()
            deadline = time.monotonic() + BODY_DRAIN_SECS
            try:
                while length:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    self.connection.settimeout(remaining)
                    chunk = self.rfile.read1(min(length, 64 * 1024))
                    if not chunk:
                        break
                    length -= len(chunk)
            except OSError:
                pass
            finally:
                self.connection.settimeout(timeout)

        def body(self) -> dict | None:
            if self.headers.get("Content-Type", "").split(";")[0].strip() != "application/json":
                self.fail(415, "json_only", "JSON only")
                return None
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                length = -1
            limit = BODY_LIMIT
            if self.path_only() in body_limits and pairing.check(
                    self.headers.get(TOKEN_HEADER, "")):
                limit = max(limit, body_limits[self.path_only()])
            if not 0 <= length <= limit:
                self.discard_body(length, limit)
                self.fail(413, "body_too_large", "body too large")
                return None
            try:
                raw = self.rfile.read(length)
                self.body_read = True
                data = json.loads(raw or b"{}")
            except (ValueError, RecursionError):
                data = None
            if not isinstance(data, dict):
                self.fail(400, "bad_body", "body must be a JSON object")
                return None
            return data

        def call(self, run) -> None:
            """Answer with run() -> body, or its ApiError."""
            try:
                self.json(200, run())
            except ApiError as error:
                self.fail(error.status, error.code, str(error))
            except Exception as error:  # a bug must still answer in the documented shape
                print(f"live-voice: {self.path_only()} failed: {error!r}", file=sys.stderr,
                      flush=True)
                self.fail(500, "internal", "internal error")

        def do_GET(self):
            if not self.guard():
                return
            path = self.path_only()
            if path in STATIC:
                name, kind = STATIC[path]
                with open(os.path.join(PAGE_DIR, name), "rb") as f:
                    self.reply(200, f.read(), kind)
                return
            if path == "/api/version":
                self.json(200, {"protocol": PROTOCOL, "door": door_version,
                                "features": FEATURES})
                return
            if path == "/api/state":
                if self.authorized():
                    self.call(api.state)
                return
            if API.get(path) == "POST":
                self.fail(405, "method_not_allowed", "use POST")
                return
            self.fail(404, "not_found", "not found")

        do_HEAD = do_GET

        def do_POST(self):
            if not self.guard():
                return
            path = self.path_only()
            if path not in API:
                self.fail(404, "not_found", "not found")
                return
            if API[path] != "POST":
                self.fail(405, "method_not_allowed", "use GET")
                return
            data = self.body()
            if data is None:
                return
            if path == "/api/pair":
                status, result = pairing.pair(str(data.get("code", "")))
                if status == 200:
                    self.json(200, {"token": result})
                else:
                    self.fail(status, PAIR_CODES[status], result)
                return
            if not self.authorized():
                return
            if path == "/api/session/start":
                self.call(lambda: {"answer": api.start(data.get("offer"), data.get("client"),
                                                       data.get("tools"), data.get("context"))})
            elif path == "/api/session/connected":
                self.call(lambda: api.connected() or {"ok": True})
            elif path == "/api/draft/confirm":
                self.call(lambda: api.confirm_draft(data))
            elif path == "/api/draft/cancel":
                self.call(lambda: api.cancel_draft(data))
            elif path == "/api/tool/result":
                self.call(lambda: api.client_tool_result(data))
            elif path == "/api/say":
                self.call(lambda: api.say(data))
            else:
                self.call(lambda: api.stop() or {"ok": True})

        def do_OPTIONS(self):
            # CORS preflight, for a listed origin only. Any other origin (or
            # none) gets what it always got: nothing it can use.
            origin = self.headers.get("Origin")
            if origin is None or origin not in allowed:
                self.cors = None
                self.fail(405, "method_not_allowed", "no")
                return
            if not self.guard():
                return
            path = self.path_only()
            if path not in API:
                self.fail(404, "not_found", "not found")
                return
            if self.headers.get("Access-Control-Request-Method") != API[path]:
                self.fail(405, "method_not_allowed", f"use {API[path]}")
                return
            self.send_response(204)
            self.common_headers()
            self.send_header("Access-Control-Allow-Methods", API[path])
            self.send_header("Access-Control-Allow-Headers", CORS_HEADERS)
            self.send_header("Access-Control-Max-Age", CORS_MAX_AGE)
            if self.headers.get("Access-Control-Request-Private-Network") == "true":
                # Chrome's Private Network Access asks this of a request to
                # loopback; the origin is the captain's own, so yes.
                self.send_header("Access-Control-Allow-Private-Network", "true")
            self.send_header("Content-Length", "0")
            self.end_headers()

        def refuse(self):
            self.cors = None
            self.fail(405, "method_not_allowed", "no")

        do_PUT = do_DELETE = do_PATCH = refuse

    return Handler
