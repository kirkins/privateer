"""The door's local page and its small JSON API, on 127.0.0.1 only.

Even on this machine the page is guarded:
- Pairing: at start the door writes a random one-time code to a 0600 file in
  its state directory. The page trades it once for a random session token;
  every API call carries the token. Wrong codes are rate limited, and after a
  few the code is replaced.
- Host must be 127.0.0.1 or localhost on the door's port (against DNS
  rebinding); an Origin, when sent, must be that same origin; POST bodies must
  be JSON (so another site cannot post a simple form). There is no CORS.
- Nothing the browser receives carries a Codex or ChatGPT credential: the
  browser only ever gets the SDP answer.
"""

import hmac
import http.server
import json
import os
import secrets
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


def serve(port: int, pairing: Pairing, api) -> http.server.ThreadingHTTPServer:
    """Start the server on 127.0.0.1:port in a thread -> the server.

    `api` has `state()`, `start(offer) -> answer`, `connected()`, `stop()`;
    each may raise ApiError."""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), _handler(pairing, api))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, name="web", daemon=True).start()
    return server


class ApiError(Exception):
    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status


def _handler(pairing: Pairing, api):
    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = "privateer-live-voice"
        sys_version = ""

        def log_message(self, *_):
            pass

        def origins(self) -> set:
            port = self.server.server_address[1]
            return {f"127.0.0.1:{port}", f"localhost:{port}"}

        def reply(self, status: int, body: bytes, kind: str = "application/json") -> None:
            self.send_response(status)
            for name, value in HEADERS.items():
                self.send_header(name, value)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def json(self, status: int, obj: dict) -> None:
            self.reply(status, json.dumps(obj).encode())

        def guard(self) -> bool:
            """Host and Origin checks; False after answering."""
            if self.headers.get("Host", "") not in self.origins():
                self.json(403, {"error": "wrong host"})
                return False
            origin = self.headers.get("Origin")
            if origin is not None and origin not in {f"http://{o}" for o in self.origins()}:
                self.json(403, {"error": "wrong origin"})
                return False
            site = self.headers.get("Sec-Fetch-Site")
            if site is not None and site not in ("same-origin", "none"):
                self.json(403, {"error": "cross-site request"})
                return False
            return True

        def authorized(self) -> bool:
            if pairing.check(self.headers.get("X-Privateer-Token", "")):
                return True
            self.json(401, {"error": "not paired"})
            return False

        def body(self) -> dict | None:
            if self.headers.get("Content-Type", "").split(";")[0].strip() != "application/json":
                self.json(415, {"error": "JSON only"})
                return None
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                length = -1
            if not 0 <= length <= BODY_LIMIT:
                self.json(413, {"error": "body too large"})
                return None
            try:
                data = json.loads(self.rfile.read(length) or b"{}")
            except ValueError:
                data = None
            if not isinstance(data, dict):
                self.json(400, {"error": "body must be a JSON object"})
                return None
            return data

        def do_GET(self):
            if not self.guard():
                return
            path = self.path.split("?", 1)[0]
            if path in STATIC:
                name, kind = STATIC[path]
                with open(os.path.join(PAGE_DIR, name), "rb") as f:
                    self.reply(200, f.read(), kind)
                return
            if path == "/api/state":
                if self.authorized():
                    self.json(200, api.state())
                return
            self.json(404, {"error": "not found"})

        do_HEAD = do_GET

        def do_POST(self):
            if not self.guard():
                return
            path = self.path.split("?", 1)[0]
            data = self.body()
            if data is None:
                return
            if path == "/api/pair":
                status, result = pairing.pair(str(data.get("code", "")))
                self.json(status, {"token": result} if status == 200 else {"error": result})
                return
            if not self.authorized():
                return
            try:
                if path == "/api/session/start":
                    self.json(200, {"answer": api.start(data.get("offer"))})
                elif path == "/api/session/connected":
                    api.connected()
                    self.json(200, {"ok": True})
                elif path == "/api/session/stop":
                    api.stop()
                    self.json(200, {"ok": True})
                else:
                    self.json(404, {"error": "not found"})
            except ApiError as error:
                self.json(error.status, {"error": str(error)})

        def do_OPTIONS(self):
            # No CORS: a preflight from another site gets nothing it can use.
            self.json(405, {"error": "no"})

        do_PUT = do_DELETE = do_PATCH = do_OPTIONS

    return Handler
