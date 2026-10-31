#!/usr/bin/env python3
"""Trusted MCP HTTP transport relay. Only the core authenticates/authorizes."""
import asyncio
import json
import logging
import re
import sys

sys.dont_write_bytecode = True  # a release holds only its own files
import tomllib
import uuid

import jsonschema
import mcp_types as types
from mcp.server import Server
from mcp.server.auth.middleware.bearer_auth import AuthenticatedUser
from mcp.server.auth.provider import AccessToken
from mcp.server.transport_security import TransportSecuritySettings
from starlette.responses import JSONResponse
import uvicorn

from file_transfer import Staging, UPLOAD_ENVELOPE, decode, valid_name
from identity import observe, server_version
from tools import BODY_MAX_BYTES, RECOVERY_ACK, TOOLS, render, result, tool
from argument_errors import diagnostic
from oauth_http import OAuthHTTP, configuration, source_address
from request_log import RequestLog

# SDK/HTTP diagnostics can contain untrusted request bodies, URLs or headers.
# Never emit SDK exception text. Our opt-in allowlisted request metadata is
# written separately, alongside allowlisted startup/failure diagnostics.
logging.disable(logging.CRITICAL)


class CoreUnavailable(Exception):
    pass


class Core:
    def __init__(self):
        self.pending = {}

    async def read(self, server, source):
        try:
            while line := await source.readline():
                message = json.loads(line)
                future = self.pending.get(message.get("id"))
                if message.get("type") == "response" and future and not future.done():
                    future.set_result(message)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(CoreUnavailable())
            server.should_exit = True

    async def call(self, token, command, args, resource=None):
        if len(self.pending) >= 32:
            raise CoreUnavailable()
        ident = uuid.uuid4().hex
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        try:
            # Never keep a credential in a queue, cache, log or diagnostic.
            message = {"type": "request", "id": ident, "command": command, "args": args}
            if token is not None:
                message["auth"] = {"token": token}
                if resource is not None:
                    message["auth"]["resource"] = resource
            sys.stdout.write(json.dumps(message) + "\n")
            sys.stdout.flush()
            return await asyncio.wait_for(future, 15)
        finally:
            self.pending.pop(ident, None)


JSON_MAX_DEPTH = 64


def request_json(body):
    # Python's JSON recursion ceiling varies by interpreter. Bound nesting
    # before building objects or handing them to the SDK; scan with constant
    # auxiliary memory after the bounded UTF-8 decode.
    # JSON syntax/encoding validation remains json.loads' responsibility.
    text = body.decode("utf-8")  # MCP JSON is UTF-8, not autodetected UTF-16/32.
    depth = 0
    quoted = escaped = False
    for char in text:
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
        elif char == '"':
            quoted = True
        elif char in "[{":
            depth += 1
            if depth > JSON_MAX_DEPTH:
                raise ValueError("JSON nesting limit")
        elif char in "]}":
            depth -= 1
            if depth < 0:
                raise ValueError("Invalid JSON nesting")
    return json.loads(text)


class Ingress:
    def __init__(self, app, core, cfg):
        self.app, self.core = app, core
        self.oauth = OAuthHTTP(core, cfg)
        port = cfg.get("port", 8767)
        self.hosts = cfg.get("allowed_hosts", [f"127.0.0.1:{port}"])
        self.origins = cfg.get("allowed_origins", [f"http://127.0.0.1:{port}"])
        if (not isinstance(self.hosts, list) or not isinstance(self.origins, list)
                or any(not isinstance(v, str) or "*" in v for v in self.hosts + self.origins)):
            raise ValueError("exact host/origin lists required")
        self.log_requests = cfg.get("log_requests", False)
        self.active = 0
        self.limit = cfg.get("max_in_flight", 16)
        self.body_limit = cfg.get("max_body_bytes", 65536)
        if not 1 <= self.limit <= 64 or not 1024 <= self.body_limit <= 1048576:
            raise ValueError("invalid limits")

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        request_log = RequestLog(scope, set(self.oauth.routes) | {self.oauth.prefix + "/mcp"}) if self.log_requests else None

        async def logged_send(message):
            if message["type"] == "http.response.start":
                request_log.record["status"] = message["status"]
            await send(message)

        try:
            return await self.handle(scope, receive, logged_send if request_log else send, request_log)
        finally:
            if request_log:
                request_log.finish()

    async def handle(self, scope, receive, send, request_log):
        async def reject(code, text, headers=None):
            await JSONResponse({"error": text}, status_code=code,
                               headers={"Cache-Control": "no-store", **(headers or {})})(scope, receive, send)

        # Reject duplicate security headers as well as mismatched exact values.
        headers = {}
        for key, value in scope["headers"]:
            if key in headers and key in (b"host", b"origin", b"authorization", b"cookie", b"content-type", b"x-forwarded-for", b"x-forwarded-proto"):
                return await reject(403, "Forbidden")
            headers[key] = value.decode("latin-1")
        if (headers.get(b"host") not in self.hosts or
                (b"origin" in headers and headers[b"origin"] not in self.origins)):
            return await reject(403, "Forbidden")
        if len(scope.get("query_string", b"")) > 8192:
            return await reject(414, "Query too large")
        protected = scope["path"] == self.oauth.prefix + "/mcp"
        if (not protected and scope["path"] not in self.oauth.routes) or (protected and scope.get("query_string")):
            return await reject(404, "Not found")
        try:
            source = source_address(scope, headers, self.oauth.proxies, remote=self.oauth.remote)
        except ValueError:
            return await reject(403, "Forbidden")
        if not protected and not self.oauth.limits.admit(source, self.oauth.routes[scope["path"]]):
            return await reject(429, "slow_down", {"Retry-After": "60"})
        token = None
        if protected:
            authorization = headers.get(b"authorization", "")
            kind, _, token = authorization.partition(" ")
            if (kind.lower() != "bearer" or not token or len(token) > 4096
                    or re.fullmatch(r"[A-Za-z0-9._~+/-]+=*", token) is None):
                return await reject(401, "Authentication required", self.oauth.challenge())
        if self.active >= self.limit:
            return await reject(503, "Busy")
        self.active += 1
        try:
            capabilities = None
            resource = None
            if protected:
                # Authenticate before receiving or parsing ANY body. A syntactic
                # Bearer token alone must never earn the upload memory budget.
                # The OAuth prefix is only a routing hint, not authentication.
                resource = self.oauth.resource if token.startswith("pv_access_") else None
                try:
                    checked = await self.core.call(token, "capabilities", {}, resource=resource)
                except (CoreUnavailable, TimeoutError):
                    return await reject(503, "Core unavailable")
                if not checked.get("ok"):
                    if checked.get("error", {}).get("code") == "unavailable":
                        return await reject(503, "Core unavailable")
                    return await reject(401, "Authentication required", self.oauth.challenge(invalid=True))
                capabilities = checked["result"]
            ceiling = (UPLOAD_ENVELOPE if capabilities and "file:transfer" in capabilities["scopes"]
                       else self.body_limit)
            # Bound receive time and bytes before JSON parsing. Check chunks
            # before copying: even one oversized ASGI message earns no buffer.
            body = bytearray()
            try:
                async with asyncio.timeout(10):
                    while True:
                        message = await receive()
                        if message["type"] == "http.disconnect":
                            return
                        chunk = message.get("body", b"")
                        if len(chunk) > ceiling - len(body):
                            return await reject(413, "Request too large")
                        body.extend(chunk)
                        if not message.get("more_body", False):
                            break
            except TimeoutError:
                return await reject(408, "Request timeout")
            if protected and body:
                # Guard recursive JSON even under the ordinary limit, before
                # SDK parsing. Empty bodies remain valid for GET/DELETE paths.
                oversized = len(body) > self.body_limit
                try:
                    parsed = request_json(body)
                except (ValueError, UnicodeError, RecursionError):
                    return await reject(413 if oversized else 400,
                                        "Request too large" if oversized else "Invalid JSON")
                # Only one exact upload call earns the larger envelope. No
                # batch, malformed JSON, discovery or other tool gets it.
                if oversized and not (
                        isinstance(parsed, dict) and parsed.get("jsonrpc") == "2.0"
                        and parsed.get("method") == "tools/call"
                        and isinstance(parsed.get("params"), dict)
                        and parsed["params"].get("name") == "send_file"
                        and set(parsed) <= {"jsonrpc", "id", "method", "params"}):
                    return await reject(413, "Request too large")
                del parsed
            if not protected:
                try:
                    response = await self.oauth.handle(scope, receive, bytes(body), headers, source)
                except (CoreUnavailable, TimeoutError):
                    return await reject(503, "Core unavailable")
                return await response(scope, receive, send)
            if request_log:
                request_log.body(body)
            # SDK session ownership uses only core-issued safe identity, not a
            # token or cached authorization. Each HTTP request repeats admission.
            scope["user"] = AuthenticatedUser(AccessToken(
                token="", client_id=capabilities["provenance"]["client"], scopes=[]))
            scope.setdefault("state", {})["capabilities"] = capabilities
            scope["state"]["oauth_resource"] = resource
            # The raw header remains request-local, for relay on tools/call.
            first = True

            async def replay():
                nonlocal first
                if first:
                    first = False
                    return {"type": "http.request", "body": bytes(body), "more_body": False}
                return await receive()

            if self.oauth.prefix:
                scope = {**scope, "path": "/mcp", "raw_path": b"/mcp"}
            await self.app(scope, replay, send)
        finally:
            self.active -= 1


def make_app(core, cfg, staging=None, identity=None):
    identity = observe() if identity is None else identity

    async def list_tools(ctx, params):
        scopes = ctx.request.state.capabilities["scopes"]
        return types.ListToolsResult(tools=[tool(n) for n, (_, s, _, _) in TOOLS.items() if s in scopes])

    async def invalid_arguments(ctx, name, field, rule, text):
        try:
            recorded = await core.call(
                ctx.request.headers["authorization"].partition(" ")[2], "tool.reject",
                {"tool": name, "field": field, "rule": rule},
                resource=ctx.request.state.oauth_resource)
        except (CoreUnavailable, TimeoutError):
            return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
        if not recorded.get("ok"):
            # Reauthenticate before exposing even a visible tool's schema.
            code = "unavailable" if recorded.get("error", {}).get("code") == "unavailable" else "not_found"
            return result({"error": {"code": code, "message": "Core unavailable" if code == "unavailable" else "Not found"}}, True)
        return result({"error": {"code": "invalid_arguments", "message": text,
                                  "field": field, "rule": rule}}, True)

    async def call_tool(ctx, params):
        spec = TOOLS.get(params.name)
        if spec is None or spec[1] not in ctx.request.state.capabilities["scopes"]:
            return result({"error": {"code": "not_found", "message": "Not found"}}, True)
        args = params.arguments or {}
        try:
            jsonschema.validate(args, spec[2])
        except jsonschema.ValidationError as error:
            return await invalid_arguments(ctx, params.name, *diagnostic(error))
        if params.name in {"send_message", "reply", "ask_liaison"}:
            body = args["body"]
            # UTF-8 and JSON escaping are separate budgets. The latter matches
            # serde_json's compact UTF-8 encoding, not Python's ASCII wire relay.
            if len(body.encode("utf-8")) > BODY_MAX_BYTES:
                return await invalid_arguments(ctx, params.name, "body", "maxUtf8Bytes",
                                               f"body: exceeds {BODY_MAX_BYTES} bytes of UTF-8")
            if len(json.dumps(body, ensure_ascii=False).encode("utf-8")) - 2 > BODY_MAX_BYTES:
                return await invalid_arguments(ctx, params.name, "body", "maxJsonBytes",
                                               f"body: exceeds {BODY_MAX_BYTES} bytes after JSON escaping")
        args = dict(args)
        if params.name == "send_file":
            try:
                if not valid_name(args["name"]):
                    raise ValueError("name")
                data = decode(args["content"])
            except ValueError as error:
                field = "name" if str(error) == "name" else "content"
                return await invalid_arguments(ctx, params.name, field, "schema",
                                               field + ": invalid file name or canonical base64/size")
            try:
                if staging is None:
                    raise OSError("staging unavailable")
                # Always stage before retry lookup. The core hashes actual
                # bytes, so fresh random paths do not alter key identity.
                with staging.stage(ctx.request.state.capabilities["provenance"]["client"], data) as path:
                    del args["content"]
                    args["content_file"] = path
                    response = await core.call(ctx.request.headers["authorization"].partition(" ")[2],
                                               spec[0], args, resource=ctx.request.state.oauth_resource)
            except (OSError, CoreUnavailable, TimeoutError):
                return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
            return relay_result(params.name, response)
        if params.name in {"approval_prepare", "approval_answer"}:
            if "note" in args and len(args["note"].encode("utf-8")) > 4096:
                return await invalid_arguments(ctx, params.name, "note", "maxUtf8Bytes",
                                               "note: exceeds 4096 bytes of UTF-8")
            args["mode"] = "prepare" if params.name == "approval_prepare" else "answer"
        elif params.name == "away_status":
            args = {"action": "status"}
        elif params.name == "reply":
            args["kind"] = "reply"
        elif params.name == "ask_liaison":
            args["kind"] = "request"
        try:
            response = await core.call(ctx.request.headers["authorization"].partition(" ")[2], spec[0], args,
                                       resource=ctx.request.state.oauth_resource)
        except (CoreUnavailable, TimeoutError):
            if params.name == "resource_recover":
                return recovery_unknown()
            return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
        if params.name == "resource_recover":
            return relay_result(params.name, response)
        if not response.get("ok") and response.get("error", {}).get("code") == "message_too_large":
            return await invalid_arguments(ctx, params.name, "body", "maxEnvelopeBytes",
                                           "body: complete mail/envelope exceeds 65536 bytes; shorten body or use a report reference")
        return relay_result(params.name, response)

    def recovery_unknown():
        return result({"error": {"code": "outcome_unknown", "message": "Request outcome unknown. Retry only with the same key and arguments; never a fresh key."}}, True)

    def relay_result(name, response):
        if name == "resource_recover" and (not isinstance(response, dict) or not isinstance(response.get("ok"), bool)):
            return recovery_unknown()
        if not response.get("ok"):
            if name == "resource_recover" and (not isinstance(response.get("error"), dict)
                                              or not isinstance(response["error"].get("code"), str)):
                return recovery_unknown()
            code = response.get("error", {}).get("code")
            approval_refusal = name in {"approval_prepare", "approval_answer"} and code in {
                "approval_stale", "approval_unavailable", "trusted_confirmation_required"}
            recovery_refusal = name in {"resource_recover", "resource_recovery_reference"} and code in {
                "recovery_target_stale", "recovery_not_eligible", "recovery_process_alive",
                "recovery_identity_unknown", "recovery_approval_required", "recovery_approval_stale", "unsupported"}
            reply_refusal = name == "reply" and code == "reply_wrong_mailbox"
            if reply_refusal or approval_refusal or recovery_refusal or code in {"unavailable", "outcome_unknown", "retry_capacity",
                                           "continuation_invalid", "reader_window_full", "legacy_writer_fenced",
                                           "resource_change_refused_relaxes",
                                           "resource_change_refused_exceeds_capacity",
                                           "resource_change_refused_unsupported_setting"}:
                return result({"error": response["error"]}, True)
            if name == "resource_recover" and code != "not_found":
                return recovery_unknown()
            return result({"error": {"code": "not_found", "message": "Not found"}}, True)
        if name == "resource_recover":
            try:
                jsonschema.validate(response.get("result"), RECOVERY_ACK, format_checker=jsonschema.FormatChecker())
            except jsonschema.ValidationError:
                return recovery_unknown()
        return result(render(name, response["result"]))

    # No listChanged: modern HTTP has no persistent server-to-client stream,
    # and grants are checked request-locally, not watched in background sessions.
    server = Server("privateer", version=server_version(identity), on_list_tools=list_tools, on_call_tool=call_tool)
    server.middleware.clear()  # no automatic tracing/export of fleet content
    app = server.streamable_http_app(
        json_response=True, max_request_body_size=UPLOAD_ENVELOPE,
        max_sessions=32, session_idle_timeout=60,
        # Our stricter exact checks run first and return 403 for Host AND Origin.
        transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False))
    return Ingress(app, core, cfg)


async def main():
    hello = json.loads(sys.argv[1])
    if hello.get("version") != 2:
        raise ValueError("scoped v2 required")
    cfg = {}
    if hello.get("config_path"):
        with open(hello["config_path"], "rb") as source:
            cfg = tomllib.load(source)
    bind, _, _, _ = configuration(cfg)
    core = Core()
    staging = Staging(hello["state_dir"])
    app = make_app(core, cfg, staging, observe(hello.get("identity")))
    server = uvicorn.Server(uvicorn.Config(
        app, host=bind, port=cfg.get("port", 8767), access_log=False,
        log_config=None, proxy_headers=False, server_header=False,
        limit_concurrency=80, backlog=64, timeout_keep_alive=5,
        timeout_graceful_shutdown=2))
    # A cancellable POSIX pipe reader avoids an executor thread blocked on
    # stdin surviving graceful HTTP shutdown (Linux/macOS door platforms).
    # A 256 KiB report can expand sixfold under JSON control escaping.
    source = asyncio.StreamReader(limit=2 * 1024 * 1024)
    transport, _ = await asyncio.get_running_loop().connect_read_pipe(
        lambda: asyncio.StreamReaderProtocol(source), sys.stdin.buffer)
    reader = asyncio.create_task(core.read(server, source))
    try:
        await app.oauth.startup()
        print("MCP remote HTTPS mode: explicit proxy trust; core-bound OAuth" if cfg.get("public_url") else
              "MCP local-only: listening on 127.0.0.1; owner bearer or configured OAuth required",
              file=sys.stderr, flush=True)
        await server.serve()
    finally:
        reader.cancel()
        transport.close()
        staging.close()
        await asyncio.gather(reader, return_exceptions=True)
    # No endpoint/token state is written by the adapter.


def startup_error(error):
    # Never reflect arbitrary exception text: config, SDK and OS errors can
    # contain credentials, URLs, paths or untrusted core response data.
    safe = {
        "remote OAuth configuration unavailable; run door oauth-client configure before door start",
        "core issuer/resource mismatch", "unsupported configuration", "scoped v2 required",
        "exact host/origin lists required", "invalid limits", "log_requests must be a boolean",
        "public_url must be a nonempty HTTPS URL",
        "public_url must be an exact HTTPS base URL without trailing slash/query/fragment",
        "non-default bind requires public_url and trusted_proxies",
        "public host and origin must be explicitly allowed", "unspecified bind addresses are forbidden",
        "proxy IPs or private CIDRs required", "proxy CIDRs must be private and /8 or narrower",
        "canonical proxy IP required", "invalid base path",
    }
    if isinstance(error, ValueError) and str(error) in safe:
        return str(error)
    if isinstance(error, (CoreUnavailable, TimeoutError)):
        return "core unavailable during startup"
    if isinstance(error, OSError):
        return "startup I/O failed; check private configuration, listener and core availability"
    return "check configuration and core availability (details withheld to protect secrets)"


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception as error:
        print("MCP stopped: " + startup_error(error), file=sys.stderr, flush=True)
        sys.exit(1)
