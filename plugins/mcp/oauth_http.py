"""Bounded HTTP/browser relay; only the core issues credentials or authority."""
import sys
sys.dont_write_bytecode = True
import asyncio
import base64
import hashlib
import html
import ipaddress
import secrets
import time
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse, RedirectResponse

NO_STORE = {"Cache-Control": "no-store", "Pragma": "no-cache"}
PAGE_HEADERS = {**NO_STORE, "Content-Security-Policy": "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
                "X-Frame-Options": "DENY", "X-Content-Type-Options": "nosniff", "Referrer-Policy": "same-origin"}
CONFIG_KEYS = {"venv", "port", "allowed_hosts", "allowed_origins", "max_in_flight", "max_body_bytes",
               "bind", "public_url", "trusted_proxies", "log_requests"}
PRIVATE_PROXY_RANGES = tuple(ipaddress.ip_network(n) for n in
                             ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"))


def proxy_networks(values):
    if not isinstance(values, list) or any(not isinstance(p, str) for p in values):
        raise ValueError("proxy IPs or private CIDRs required")
    networks = set()
    for value in values:
        network = ipaddress.ip_network(value)  # strict: no silently widened host CIDRs
        if "/" in value:
            if network.prefixlen < 8 or not any(network.version == private.version and network.subnet_of(private)
                                                for private in PRIVATE_PROXY_RANGES):
                raise ValueError("proxy CIDRs must be private and /8 or narrower")
        elif str(ipaddress.ip_address(value)) != value:
            raise ValueError("canonical proxy IP required")
        networks.add(network)
    return networks


def configuration(cfg):
    if set(cfg) - CONFIG_KEYS or type(cfg.get("port", 8767)) is not int or not 1 <= cfg.get("port", 8767) <= 65535:
        raise ValueError("unsupported configuration")
    if type(cfg.get("log_requests", False)) is not bool:
        raise ValueError("log_requests must be a boolean")
    bind = str(ipaddress.ip_address(cfg.get("bind", "127.0.0.1")))
    proxies = proxy_networks(cfg.get("trusted_proxies", []))
    public = cfg.get("public_url")
    if "public_url" in cfg and (not isinstance(public, str) or not public):
        raise ValueError("public_url must be a nonempty HTTPS URL")
    base = public or f'http://127.0.0.1:{cfg.get("port", 8767)}'
    parsed = urlsplit(base)
    if (not isinstance(base, str) or not base.isascii() or any(ord(c) <= 32 or ord(c) == 127 for c in base)
            or parsed.username or parsed.password or not parsed.hostname or parsed.query or parsed.fragment
            or "?" in base or "#" in base or "\\" in base or "%" in base or base.endswith("/")
            or (public and parsed.scheme != "https")):
        raise ValueError("public_url must be an exact HTTPS base URL without trailing slash/query/fragment")
    # Accessing port also validates its syntax/range. Avoid ambiguous path routing.
    parsed.port
    if any(p in (".", "..", "") for p in parsed.path.split("/")[1:]):
        raise ValueError("invalid base path")
    if ipaddress.ip_address(bind).is_unspecified:
        raise ValueError("unspecified bind addresses are forbidden")
    if bind != "127.0.0.1" and not (public and proxies):
        raise ValueError("non-default bind requires public_url and trusted_proxies")
    if public and (parsed.netloc not in cfg.get("allowed_hosts", []) or
                   f"https://{parsed.netloc}" not in cfg.get("allowed_origins", [])):
        raise ValueError("public host and origin must be explicitly allowed")
    return bind, base, parsed.path, proxies


class SourceLimits:
    """Minute windows: 4096 source/operation keys, 1024 overflow prefix/operation keys.

    IPv6 peers share a /64. At capacity, new sources share a quarter-rate
    IPv4 /24 or IPv6 /48 bucket. No live bucket is evicted; unseen prefixes
    fail closed if the overflow table also fills. One prefix cannot drain
    another prefix's overflow budget or occupy more than one overflow key
    per operation.
    """
    VALUES = {"authorize": 32, "login": 20, "consent": 20, "token": 60, "revoke": 60, "metadata": 120}

    def __init__(self):
        self.rows = {}
        self.overflow = {}

    def admit(self, source, operation):
        now = time.monotonic()
        self.rows = {k: v for k, v in self.rows.items() if v[0] > now - 60}
        self.overflow = {k: v for k, v in self.overflow.items() if v[0] > now - 60}
        address = ipaddress.ip_address(source)
        if address.version == 6 and address.ipv4_mapped is not None:
            address = address.ipv4_mapped
        source = str(ipaddress.ip_network(f"{address}/64", strict=False)) if address.version == 6 else str(address)
        key = (source, operation)
        rows, limit = self.rows, self.VALUES[operation]
        if key not in rows and len(rows) >= 4096:
            prefix = ipaddress.ip_network(f"{address}/{48 if address.version == 6 else 24}", strict=False)
            rows, key, limit = self.overflow, (str(prefix), operation), max(1, limit // 4)
            if key not in rows and len(rows) >= 1024:
                return False
        start, count = rows.get(key, (now, 0))
        if start <= now - 60:
            start, count = now, 0
        if count >= limit:
            return False
        rows[key] = (start, count + 1)
        return True


def source_address(scope, headers, proxies, remote=False):
    address = ipaddress.ip_address(scope["client"][0])
    peer = str(address)
    if not any(address in network for network in proxies):
        if remote and not address.is_loopback:
            raise ValueError("remote access requires a trusted proxy")
        return peer
    # The trusted last-hop proxy must OVERWRITE, not append these headers.
    if headers.get(b"x-forwarded-proto") != "https":
        raise ValueError("trusted proxy must assert HTTPS")
    return str(ipaddress.ip_address(headers.get(b"x-forwarded-for", "")))


# Up to 1024 listed ships, eight scopes, and the fixed consent controls.
CONSENT_FIELD_LIMIT = 1024 + 16
CONSENT_SECONDS = 600
TRANSACTION_SECONDS = 900
COMPLETED_SECONDS = 60

# A fixed, CSP-hashed convenience script. The session lock/cache, not JS, is
# the single-issuance guarantee. Browsers without SubmitEvent.submitter
# (Safari before 15.4) never set e.submitter, so a click listener records the
# clicked button's value first; implicit Enter submission fires a simulated
# click on the form's default button, which that listener already records.
# There is no fallback to the form's first button: a null-submitter submit
# with no recorded click (requestSubmit from an extension, or autofill) adds
# no decision at all, and the server denies such a POST, never approves it.
SUBMIT_SCRIPT = "(function(){var c=null,f=document.querySelector('form');f.addEventListener('click',function(e){var b=e.target.closest('button');c=b?b.value:null;});f.addEventListener('submit',function(e){var s=e.submitter?e.submitter.value:c;if(s){var i=document.createElement('input');i.type='hidden';i.name='decision';i.value=s;f.appendChild(i);}f.querySelectorAll('button').forEach(function(b){b.disabled=true;});});})();"
SUBMIT_HASH = base64.b64encode(hashlib.sha256(SUBMIT_SCRIPT.encode()).digest()).decode()


class FormFieldLimit(ValueError):
    pass


def form(data, repeated=(), max_fields=128):
    if data and data.count("&") + 1 > max_fields:
        raise FormFieldLimit(f"Too many form fields (maximum {max_fields}); reduce the selection and retry.")
    pairs = parse_qsl(data, keep_blank_values=True, strict_parsing=True, max_num_fields=max_fields, errors="strict")
    result = {}
    for key, value in pairs:
        if key in repeated:
            result.setdefault(key, []).append(value)
        elif key in result:
            raise ValueError("duplicate parameter")
        else:
            result[key] = value
    return result


def esc(value):
    return html.escape(str(value), quote=True)


class OAuthHTTP:
    COOKIE = "privateer_oauth_browser"

    def __init__(self, core, cfg):
        self.core = core
        self.bind, self.base, self.prefix, self.proxies = configuration(cfg)
        self.remote = bool(cfg.get("public_url"))
        self.cookie_name = "__Host-privateer_oauth_browser" if self.remote else self.COOKIE
        self.resource = self.base + "/mcp"
        self.sessions = {}
        self.limits = SourceLimits()
        self.resource_metadata_path = "/.well-known/oauth-protected-resource" + self.prefix + "/mcp"
        self.resource_metadata_url = urlunsplit((*urlsplit(self.base)[:2], self.resource_metadata_path, "", ""))
        self.routes = {self.prefix + "/authorize": "authorize", self.prefix + "/login": "login",
                       self.prefix + "/consent": "consent", self.prefix + "/token": "token",
                       self.prefix + "/revoke": "revoke"}
        # The resource always has a /mcp path: a root document would describe
        # the wrong resource (RFC 9728 section 3.3). Only insertion URLs match.
        self.resource_paths = {self.resource_metadata_path}
        self.as_paths = {"/.well-known/oauth-authorization-server" + self.prefix}
        self.routes.update({p: "metadata" for p in self.resource_paths | self.as_paths})

    async def relay(self, command, args):
        response = await self.core.call(None, "oauth." + command, args)
        if not response.get("ok"):
            return {"error": "temporarily_unavailable"}
        return response["result"]

    async def metadata(self):
        metadata = await self.relay("metadata", {})
        if "error" not in metadata and (metadata.get("issuer") != self.base or
                                       metadata.get("protected_resource", {}).get("resource") != self.resource):
            raise ValueError("core issuer/resource mismatch")
        return metadata

    async def startup(self):
        if self.remote and "error" in await self.metadata():
            raise ValueError("remote OAuth configuration unavailable; run door oauth-client configure before door start")

    def challenge(self, invalid=False):
        return {"WWW-Authenticate": f'Bearer resource_metadata="{self.resource_metadata_url}"' +
                (', error="invalid_token"' if invalid else "")}

    def page(self, body, session_id=None, head=""):
        mode = "Remote HTTPS mode" if self.remote else "Local-only mode (loopback; no remote access configured)"
        response = HTMLResponse(f'<!doctype html><html><head><title>Privateer authorization</title>{head}</head><body><h1>Privateer authorization</h1><p>{mode}</p>{body}</body></html>', headers=PAGE_HEADERS)
        if session_id:
            response.set_cookie(self.cookie_name, session_id,
                                max_age=max(1, int(self.sessions.get(session_id, {}).get("expires", time.monotonic() + 300) - time.monotonic())), secure=self.remote,
                                httponly=True, samesite="lax", path="/")
        return response

    def fields(self, session):
        return f'<input type="hidden" name="csrf" value="{esc(session["csrf"])}">'

    def consent_page(self, session):
        summary = session["summary"]
        checks = "".join(f'<label><input type="checkbox" name="scopes" value="{esc(s)}">{esc(s)}</label><br>'
                         for s in summary["requested_scopes"])
        ships = "".join(f'<label><input type="checkbox" name="selected_ships" value="{esc(s)}">{esc(s)}</label><br>'
                        for s in summary["known_ships"])
        response = self.page(f'<h2>Consent</h2><p>Application: {esc(summary["display_name"])}</p>'
                         f'<p>Client: {esc(summary["client_id"])}</p><p>Redirect: {esc(summary["redirect_uri"])}</p>'
                         f'<p>Resource: {esc(summary["resource"])}</p>'
                         f'<form method="post" action="{esc(self.prefix)}/consent">{self.fields(session)}{checks}'
                         f'<fieldset><legend>Known ships (none selected by default)</legend>{ships}</fieldset>'
                         '<label>Additional ship names (space separated; empty means none): <input name="ships"></label>'
                         '<p><label><input type="checkbox" name="all_ships" value="yes">All ships (current and future)</label> '
                         '<strong>Warning: grants access to every ship, including ships added later, for the selected scopes.</strong></p>'
                         '<p>Only checked scopes and selected/named ships are approved, unless All ships is checked. Access lasts at most 30 days.</p>'
                         '<button name="decision" value="approve">Approve selected permissions</button>'
                         '<button name="decision" value="deny">Deny</button></form>'
                         f'<script>{SUBMIT_SCRIPT}</script>')
        response.headers["Content-Security-Policy"] += f"; script-src 'sha256-{SUBMIT_HASH}'"
        return response

    async def token_failure(self, operation, parameter, args=None):
        # A closed diagnostic enum can only refuse work in core, never issue
        # credentials. Do not relay malformed bodies, headers or their values.
        safe = {k: v for k, v in (args or {}).items() if k in ("client_id", "grant_type")}
        result = await self.relay(operation, {**safe, "transport_error": parameter})
        return JSONResponse(result, status_code=503 if result.get("error") == "temporarily_unavailable" else 400,
                            headers=NO_STORE)

    async def handle(self, scope, receive, body, headers, source):
        operation = self.routes.get(scope["path"])
        if operation is None:
            return JSONResponse({"error": "not_found"}, status_code=404, headers=NO_STORE)
        request = Request(scope, receive)
        method = scope["method"]
        now = time.monotonic()
        self.sessions = {k: v for k, v in self.sessions.items() if v["expires"] > now}

        def failure(error="invalid_request", status=400):
            return JSONResponse({"error": error}, status_code=status, headers=NO_STORE)

        try:
            if operation == "metadata":
                if method != "GET" or scope.get("query_string"):
                    return failure()
                metadata = await self.metadata()
                if "error" in metadata:
                    return failure("temporarily_unavailable", 503)
                return JSONResponse(metadata["protected_resource"] if scope["path"] in self.resource_paths else
                                    {k: v for k, v in metadata.items() if k != "protected_resource"}, headers=NO_STORE)
            if operation == "authorize":
                if method != "GET":
                    return JSONResponse({"error": "invalid_request"}, status_code=405,
                                        headers={**NO_STORE, "Allow": "GET"})
                if len(self.sessions) >= 1024:
                    return failure("temporarily_unavailable", 503)
                args = form(scope.get("query_string", b"").decode("ascii"))
                allowed = {"client_id", "response_type", "redirect_uri", "resource", "state",
                           "code_challenge_method", "code_challenge", "scope"}
                args = {k: v for k, v in args.items() if k in allowed}
                result = await self.relay("authorize-begin", args)
                if "error" in result:
                    return failure(result["error"])
                sid = secrets.token_urlsafe(32)
                session = {"transaction": result["transaction"], "core_csrf": result["csrf"],
                           "csrf": secrets.token_urlsafe(32), "expires": now + min(result["expires_in"], 300),
                           "absolute_expires": now + TRANSACTION_SECONDS, "lock": asyncio.Lock()}
                self.sessions[sid] = session
                return self.page(f'<h2>Owner login</h2><p>Transaction: {esc(result["transaction"])}</p>'
                                 '<p>On the owner terminal, inspect this transaction and run exactly:</p>'
                                 f'<pre>privateer door oauth-client login-code {esc(result["transaction"])}</pre>'
                                 f'<form method="post" action="{esc(self.prefix)}/login">{self.fields(session)}'
                                 '<label>One-time login code: <input name="login_code" type="password" autocomplete="off" required></label>'
                                 '<button>Log in and review consent</button></form>', sid)
            if scope.get("query_string"):
                return await self.token_failure(operation, "query") if operation in ("token", "revoke") else failure()
            if operation in ("token", "revoke"):
                if method != "POST":
                    return await self.token_failure(operation, "method")
                if headers.get(b"content-type", "").split(";")[0].lower() != "application/x-www-form-urlencoded":
                    return await self.token_failure(operation, "content_type")
                args = form(body.decode("utf-8"))
                if b"authorization" in headers:
                    scheme, _, payload = headers[b"authorization"].partition(" ")
                    if scheme.lower() != "basic" or not payload or "client_secret_basic" in args:
                        return await self.token_failure(operation, "authorization", args)
                    args["client_secret_basic"] = payload
                elif "client_secret_basic" in args:
                    return await self.token_failure(operation, "authorization", args)
                allowed = {"client_id", "client_secret", "client_secret_basic"}
                allowed |= ({"grant_type", "code", "redirect_uri", "resource", "code_verifier", "refresh_token"}
                            if operation == "token" else {"token", "token_type_hint"})
                # Parse and reject duplicates BEFORE ignoring extensions (RFC 6749).
                args = {k: v for k, v in args.items() if k in allowed}
                result = await self.relay(operation, args)
                error = result.get("error")
                status = 401 if error == "invalid_client" and b"authorization" in headers else 429 if error == "slow_down" else 400 if error else 200
                extra = {"WWW-Authenticate": 'Basic realm="privateer"'} if status == 401 else {}
                return JSONResponse(result, status_code=status, headers={**NO_STORE, **extra})
            sid = request.cookies.get(self.cookie_name, "")
            session = self.sessions.get(sid)
            if session is None:
                return self.expired_page()
            if operation == "consent" and method == "GET" and "summary" in session:
                if "completed" in session:
                    return self.forward_page(session["completed"], sid)
                return self.consent_page(session)
            if method != "POST" or headers.get(b"content-type", "").split(";")[0].lower() != "application/x-www-form-urlencoded":
                return failure()
            args = form(body.decode("utf-8"), repeated=("scopes", "selected_ships"),
                        max_fields=CONSENT_FIELD_LIMIT if operation == "consent" else 128)
            if not secrets.compare_digest(args.pop("csrf", "").encode("utf-8"), session["csrf"].encode("ascii")):
                return failure("access_denied", 403)
            if operation == "login":
                if set(args) != {"login_code"} or "summary" in session:
                    return failure()
                result = await self.relay("login-submit", {"transaction": session["transaction"], "csrf": session["core_csrf"],
                                                          "login_code": args["login_code"], "source": source})
                if "error" in result:
                    return self.expired_page() if result["error"] == "invalid_grant" else failure(result["error"])
                session["expires"] = min(time.monotonic() + min(result["expires_in"], CONSENT_SECONDS),
                                         session["absolute_expires"])
                session.pop("core_csrf")
                session["summary"] = result
                session["csrf"] = secrets.token_urlsafe(32)
                response = RedirectResponse(self.prefix + "/consent", status_code=303, headers=NO_STORE)
                response.set_cookie(self.cookie_name, sid, max_age=max(1, int(session["expires"] - time.monotonic())),
                                    secure=self.remote, httponly=True, samesite="lax", path="/")
                return response
            # Serialize duplicate POSTs across the core await; do not clear the
            # cookie on success, since the browser may already have queued one.
            async with session.setdefault("lock", asyncio.Lock()):
                if session["expires"] <= time.monotonic():
                    return self.expired_page()
                if "completed" in session:
                    return self.forward_page(session["completed"], sid)
                return await self.submit_consent(session, sid, args)
        except FormFieldLimit as error:
            if operation in ("token", "revoke"):
                return await self.token_failure(operation, "form")
            return JSONResponse({"error": "invalid_request", "error_description": str(error)},
                                status_code=400, headers=NO_STORE)
        except (ValueError, UnicodeError, KeyError):
            if operation in ("token", "revoke"):
                return await self.token_failure(operation, "form")
            return failure()

    def expired_page(self):
        response = self.page('<h2>Sign-in expired</h2><p>Your sign-in expired. Retry from the client to start a new sign-in.</p>')
        response.status_code = 403
        return response

    def forward_page(self, callback, sid):
        callback = esc(callback)
        return self.page(f'<p><a href="{callback}">Continue to the application</a></p>', sid,
                         head=f'<meta http-equiv="refresh" content="0;url={callback}">')

    async def submit_consent(self, session, sid, args):
        def failure():
            return JSONResponse({"error": "invalid_request"}, status_code=400, headers=NO_STORE)

        if "summary" not in session or set(args) - {"scopes", "ships", "selected_ships", "all_ships", "decision"}:
            return failure()
        selected = args.get("selected_ships", [])
        if (any(s not in session["summary"]["known_ships"] for s in selected)
                or args.get("all_ships", "yes") != "yes"):
            return failure()
        ships = None if "all_ships" in args else sorted(set(selected + args.get("ships", "").split()))
        result = await self.relay("consent-submit", {"transaction": session["transaction"],
                                                   "consent_token": session["summary"]["consent_token"],
                                                   "scopes": args.get("scopes", []), "ships": ships,
                                                   "decision": args.get("decision", "deny")})
        # Only the core's bound successful consent/explicit denial has a redirect.
        if "redirect_uri" not in result:
            if result.get("error") == "invalid_grant":
                return self.expired_page()
            # Keep the core's fixed, non-secret selection-limit explanation.
            detail = {k: result[k] for k in ("error", "error_description") if k in result}
            return JSONResponse(detail or {"error": "server_error"}, status_code=400, headers=NO_STORE)
        target = urlsplit(result["redirect_uri"])
        query = parse_qsl(target.query, keep_blank_values=True) + [(k, result[k]) for k in ("code", "error", "state", "iss") if k in result]
        # Preserve #487: refresh is navigation, not a form redirect subject to
        # form-action restrictions across the callback's redirect chain. The
        # replay window never extends past the transaction's absolute cap.
        session["completed"] = urlunsplit((target.scheme, target.netloc, target.path, urlencode(query), ""))
        session["expires"] = min(time.monotonic() + COMPLETED_SECONDS, session["absolute_expires"])
        return self.forward_page(session["completed"], sid)
