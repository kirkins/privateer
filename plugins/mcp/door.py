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

from tools import TOOLS, result, tool
from oauth_http import OAuthHTTP, configuration, source_address
from request_log import RequestLog

# SDK/HTTP diagnostics can contain untrusted request bodies, URLs or headers.
# Never emit SDK exception text. Our opt-in allowlisted request metadata is
# written separately, alongside constant startup/failure diagnostics.
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
            # Bound receive time and bytes before JSON parsing and core admission.
            body = bytearray()
            try:
                async with asyncio.timeout(10):
                    while True:
                        message = await receive()
                        if message["type"] == "http.disconnect":
                            return
                        body.extend(message.get("body", b""))
                        if len(body) > self.body_limit:
                            return await reject(413, "Request too large")
                        if not message.get("more_body", False):
                            break
            except TimeoutError:
                return await reject(408, "Request timeout")
            if not protected:
                try:
                    response = await self.oauth.handle(scope, receive, bytes(body), headers, source)
                except (CoreUnavailable, TimeoutError):
                    return await reject(503, "Core unavailable")
                return await response(scope, receive, send)
            if request_log:
                request_log.body(body)
            # Purpose-specific opaque OAuth prefix is only a routing hint; core
            # validation remains authoritative. Never retry without audience.
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


def make_app(core, cfg):
    async def list_tools(ctx, params):
        scopes = ctx.request.state.capabilities["scopes"]
        return types.ListToolsResult(tools=[tool(n) for n, (_, s, _, _) in TOOLS.items() if s in scopes])

    async def call_tool(ctx, params):
        spec = TOOLS.get(params.name)
        if spec is None or spec[1] not in ctx.request.state.capabilities["scopes"]:
            return result({"error": {"code": "not_found", "message": "Not found"}}, True)
        args = params.arguments or {}
        try:
            jsonschema.validate(args, spec[2])
        except jsonschema.ValidationError:
            return result({"error": {"code": "not_found", "message": "Not found"}}, True)
        args = dict(args)
        if params.name == "away_status":
            args = {"action": "status"}
        elif params.name == "reply":
            args["kind"] = "reply"
        elif params.name == "ask_liaison":
            args["kind"] = "request"
        try:
            response = await core.call(ctx.request.headers["authorization"].partition(" ")[2], spec[0], args,
                                       resource=ctx.request.state.oauth_resource)
        except (CoreUnavailable, TimeoutError):
            return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
        if not response.get("ok"):
            if response.get("error", {}).get("code") == "unavailable":
                return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
            return result({"error": {"code": "not_found", "message": "Not found"}}, True)
        return result(response["result"])

    server = Server("privateer", version="0.1.0", on_list_tools=list_tools, on_call_tool=call_tool)
    server.middleware.clear()  # no automatic tracing/export of fleet content
    app = server.streamable_http_app(
        json_response=True, max_request_body_size=cfg.get("max_body_bytes", 65536),
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
    app = make_app(core, cfg)
    server = uvicorn.Server(uvicorn.Config(
        app, host=bind, port=cfg.get("port", 8767), access_log=False,
        log_config=None, proxy_headers=False, server_header=False,
        limit_concurrency=80, backlog=64, timeout_keep_alive=5,
        timeout_graceful_shutdown=2))
    # A cancellable POSIX pipe reader avoids an executor thread blocked on
    # stdin surviving graceful HTTP shutdown (Linux/macOS door platforms).
    source = asyncio.StreamReader(limit=1024 * 1024)
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
        await asyncio.gather(reader, return_exceptions=True)
    # No endpoint/token state is written by the adapter.


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception:
        print("MCP stopped; check configuration and core availability", file=sys.stderr)
        sys.exit(1)
