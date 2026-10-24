"""Bounded HTTP/browser relay; only the core issues credentials or authority."""
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


def form(data, repeated=()):
    pairs = parse_qsl(data, keep_blank_values=True, strict_parsing=True, max_num_fields=128, errors="strict")
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
            raise ValueError("remote OAuth configuration unavailable")

    def challenge(self, invalid=False):
        return {"WWW-Authenticate": f'Bearer resource_metadata="{self.resource_metadata_url}"' +
                (', error="invalid_token"' if invalid else "")}

    def page(self, body, session_id=None):
        mode = "Remote HTTPS mode" if self.remote else "Local-only mode (loopback; no remote access configured)"
        response = HTMLResponse(f'<!doctype html><html><head><title>Privateer authorization</title></head><body><h1>Privateer authorization</h1><p>{mode}</p>{body}</body></html>', headers=PAGE_HEADERS)
        if session_id:
            response.set_cookie(self.cookie_name, session_id, max_age=300, secure=self.remote,
                                httponly=True, samesite="lax", path="/")
        return response

    def fields(self, session):
        return f'<input type="hidden" name="csrf" value="{esc(session["csrf"])}">'

    def consent_page(self, session):
        summary = session["summary"]
        checks = "".join(f'<label><input type="checkbox" name="scopes" value="{esc(s)}">{esc(s)}</label><br>'
                         for s in summary["requested_scopes"])
        response = self.page(f'<h2>Consent</h2><p>Application: {esc(summary["display_name"])}</p>'
                         f'<p>Client: {esc(summary["client_id"])}</p><p>Redirect: {esc(summary["redirect_uri"])}</p>'
                         f'<p>Resource: {esc(summary["resource"])}</p>'
                         f'<form method="post" action="{esc(self.prefix)}/consent">{self.fields(session)}{checks}'
                         '<label>Ship names (space separated; empty means none): <input name="ships"></label>'
                         '<p>Only checked scopes and named ships are approved. Access lasts at most 30 days.</p>'
                         '<button name="decision" value="approve">Approve selected permissions</button>'
                         '<button name="decision" value="deny">Deny</button></form>')
        # Browsers may enforce form-action on the final OAuth redirect too.
        # Only the core-validated registered callback origin is added; never
        # place arbitrary display strings or URL paths into a CSP directive.
        target = urlsplit(summary["redirect_uri"])
        response.headers["Content-Security-Policy"] = PAGE_HEADERS["Content-Security-Policy"].replace(
            "form-action 'self'", f"form-action 'self' {target.scheme}://{target.netloc}")
        return response

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
                           "csrf": secrets.token_urlsafe(32), "expires": now + min(result["expires_in"], 300)}
                self.sessions[sid] = session
                return self.page(f'<h2>Owner login</h2><p>Transaction: {esc(result["transaction"])}</p>'
                                 '<p>On the owner terminal, inspect this transaction and run exactly:</p>'
                                 f'<pre>privateer door oauth-client login-code {esc(result["transaction"])}</pre>'
                                 f'<form method="post" action="{esc(self.prefix)}/login">{self.fields(session)}'
                                 '<label>One-time login code: <input name="login_code" type="password" autocomplete="off" required></label>'
                                 '<button>Log in and review consent</button></form>', sid)
            if scope.get("query_string"):
                return failure()
            if operation in ("token", "revoke"):
                if method != "POST" or headers.get(b"content-type", "").split(";")[0].lower() != "application/x-www-form-urlencoded":
                    return failure()
                args = form(body.decode("utf-8"))
                if b"authorization" in headers:
                    scheme, _, payload = headers[b"authorization"].partition(" ")
                    if scheme.lower() != "basic" or not payload or "client_secret_basic" in args:
                        return failure("invalid_client")
                    args["client_secret_basic"] = payload
                elif "client_secret_basic" in args:
                    return failure("invalid_client")
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
                return failure("access_denied", 403)
            if operation == "consent" and method == "GET" and "summary" in session:
                return self.consent_page(session)
            if method != "POST" or headers.get(b"content-type", "").split(";")[0].lower() != "application/x-www-form-urlencoded":
                return failure()
            args = form(body.decode("utf-8"), repeated=("scopes",))
            if not secrets.compare_digest(args.pop("csrf", "").encode("utf-8"), session["csrf"].encode("ascii")):
                return failure("access_denied", 403)
            if operation == "login":
                if set(args) != {"login_code"} or "summary" in session:
                    return failure()
                result = await self.relay("login-submit", {"transaction": session["transaction"], "csrf": session["core_csrf"],
                                                          "login_code": args["login_code"], "source": source})
                if "error" in result:
                    return failure(result["error"])
                session.pop("core_csrf")
                session["summary"] = result
                session["csrf"] = secrets.token_urlsafe(32)
                return RedirectResponse(self.prefix + "/consent", status_code=303, headers=NO_STORE)
            if "summary" not in session or set(args) - {"scopes", "ships", "decision"}:
                return failure()
            result = await self.relay("consent-submit", {"transaction": session["transaction"],
                                                       "consent_token": session["summary"]["consent_token"],
                                                       "scopes": args.get("scopes", []), "ships": args.get("ships", "").split(),
                                                       "decision": args.get("decision", "deny")})
            # Only the core's bound successful consent/explicit denial has a redirect.
            if "redirect_uri" not in result:
                return failure(result.get("error", "server_error"))
            target = urlsplit(result["redirect_uri"])
            query = parse_qsl(target.query, keep_blank_values=True) + [(k, result[k]) for k in ("code", "error", "state", "iss") if k in result]
            del self.sessions[sid]
            response = RedirectResponse(urlunsplit((target.scheme, target.netloc, target.path, urlencode(query), "")), status_code=303, headers=NO_STORE)
            response.delete_cookie(self.cookie_name, path="/", secure=self.remote, httponly=True, samesite="lax")
            return response
        except (ValueError, UnicodeError, KeyError):
            return failure()
