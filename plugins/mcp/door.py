#!/usr/bin/env python3
"""Trusted loopback MCP transport relay. Only the core authenticates/authorizes."""
import asyncio
import json
import logging
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

# SDK/HTTP diagnostics can contain untrusted request bodies, URLs or headers.
# Emit only our constant startup/failure diagnostics, never SDK exception text.
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

    async def call(self, token, command, args):
        if len(self.pending) >= 32:
            raise CoreUnavailable()
        ident = uuid.uuid4().hex
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        try:
            # Never keep a credential in a queue, cache, log or diagnostic.
            sys.stdout.write(json.dumps({"type": "request", "id": ident, "command": command,
                                         "auth": {"token": token}, "args": args}) + "\n")
            sys.stdout.flush()
            return await asyncio.wait_for(future, 15)
        finally:
            self.pending.pop(ident, None)


class Ingress:
    def __init__(self, app, core, cfg):
        self.app, self.core = app, core
        port = cfg.get("port", 8767)
        self.hosts = cfg.get("allowed_hosts", [f"127.0.0.1:{port}"])
        self.origins = cfg.get("allowed_origins", [f"http://127.0.0.1:{port}"])
        if (not isinstance(self.hosts, list) or not isinstance(self.origins, list)
                or any(not isinstance(v, str) or "*" in v for v in self.hosts + self.origins)):
            raise ValueError("exact host/origin lists required")
        self.active = 0
        self.limit = cfg.get("max_in_flight", 16)
        self.body_limit = cfg.get("max_body_bytes", 65536)
        if not 1 <= self.limit <= 64 or not 1024 <= self.body_limit <= 1048576:
            raise ValueError("invalid limits")

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)

        async def reject(code, text, headers=None):
            await JSONResponse({"error": text}, status_code=code, headers=headers)(scope, receive, send)

        # Reject duplicate security headers as well as mismatched exact values.
        headers = {}
        for key, value in scope["headers"]:
            if key in headers and key in (b"host", b"origin", b"authorization"):
                return await reject(403, "Forbidden")
            headers[key] = value.decode("latin-1")
        if (headers.get(b"host") not in self.hosts or
                (b"origin" in headers and headers[b"origin"] not in self.origins)):
            return await reject(403, "Forbidden")
        if scope["path"] != "/mcp" or scope.get("query_string"):
            return await reject(404, "Not found")
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
            authorization = headers.get(b"authorization", "")
            kind, _, token = authorization.partition(" ")
            if kind.lower() != "bearer" or not token or len(token) > 4096:
                return await reject(401, "Authentication required", {"WWW-Authenticate": 'Bearer realm="privateer"'})
            try:
                checked = await self.core.call(token, "capabilities", {})
            except (CoreUnavailable, TimeoutError):
                return await reject(503, "Core unavailable")
            if not checked.get("ok"):
                return await reject(401, "Authentication required", {"WWW-Authenticate": 'Bearer realm="privateer", error="invalid_token"'})
            capabilities = checked["result"]
            # SDK session ownership uses only core-issued safe identity, not a
            # token or cached authorization. Each HTTP request repeats admission.
            scope["user"] = AuthenticatedUser(AccessToken(
                token="", client_id=capabilities["provenance"]["client"], scopes=[]))
            scope.setdefault("state", {})["capabilities"] = capabilities
            # The raw header remains request-local, for relay on tools/call.
            first = True

            async def replay():
                nonlocal first
                if first:
                    first = False
                    return {"type": "http.request", "body": bytes(body), "more_body": False}
                return await receive()

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
        if params.name == "away_status":
            args = {"action": "status"}
        try:
            response = await core.call(ctx.request.headers["authorization"].partition(" ")[2], spec[0], args)
        except (CoreUnavailable, TimeoutError):
            return result({"error": {"code": "unavailable", "message": "Core unavailable"}}, True)
        if not response.get("ok"):
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
    allowed = {"venv", "port", "allowed_hosts", "allowed_origins", "max_in_flight", "max_body_bytes"}
    if set(cfg) - allowed or not 1 <= cfg.get("port", 8767) <= 65535:
        raise ValueError("unsupported configuration")
    core = Core()
    app = make_app(core, cfg)
    server = uvicorn.Server(uvicorn.Config(
        app, host="127.0.0.1", port=cfg.get("port", 8767), access_log=False,
        log_config=None, proxy_headers=False, server_header=False,
        limit_concurrency=80, backlog=64, timeout_keep_alive=5,
        timeout_graceful_shutdown=2))
    print("MCP local-only: listening on 127.0.0.1; owner-issued bearer token required", file=sys.stderr, flush=True)
    # A cancellable POSIX pipe reader avoids an executor thread blocked on
    # stdin surviving graceful HTTP shutdown (Linux/macOS door platforms).
    source = asyncio.StreamReader(limit=1024 * 1024)
    transport, _ = await asyncio.get_running_loop().connect_read_pipe(
        lambda: asyncio.StreamReaderProtocol(source), sys.stdin.buffer)
    reader = asyncio.create_task(core.read(server, source))
    try:
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
        print("MCP local-only: stopped; check configuration and core availability", file=sys.stderr)
        sys.exit(1)
