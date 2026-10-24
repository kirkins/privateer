"""Opt-in, allowlisted transport metadata only; never serialize request values."""
import json
import sys
import time

# Unknown strings must not reach logs: even a method/version/path can contain
# a credential supplied by a hostile client. These are diagnostics, not routing
# or negotiation policy; the SDK remains authoritative for protocol support.
HTTP_METHODS = frozenset(("GET", "POST", "DELETE", "OPTIONS", "HEAD", "PUT", "PATCH"))
MCP_METHODS = frozenset(("initialize", "notifications/initialized", "server/discover", "ping",
                         "tools/list", "tools/call", "resources/list", "resources/read",
                         "resources/templates/list", "prompts/list", "prompts/get"))
VERSIONS = frozenset(("2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"))


class RequestLog:
    def __init__(self, scope, paths):
        self.started = time.monotonic()
        versions = [v.decode("latin-1") for k, v in scope["headers"] if k == b"mcp-protocol-version"]
        version = versions[0] if len(versions) == 1 else None
        self.record = {
            "event": "mcp_http_request",
            "method": scope["method"] if scope["method"] in HTTP_METHODS else "[other]",
            "path": scope["path"] if scope["path"] in paths else "[other]",
            "mcp_method": None,
            "protocol_version": version if version in VERSIONS else "[other]" if versions else None,
            "authorization": "yes" if any(k == b"authorization" for k, _ in scope["headers"]) else "no",
            "status": None,
        }

    def body(self, body):
        # Only called for a bounded MCP body, never for OAuth form bodies.
        try:
            value = json.loads(body)
            method = value.get("method") if isinstance(value, dict) else None
            self.record["mcp_method"] = method if isinstance(method, str) and method in MCP_METHODS else "[other]"
        except (ValueError, RecursionError):
            self.record["mcp_method"] = "[other]"

    def finish(self):
        self.record["duration_ms"] = round((time.monotonic() - self.started) * 1000, 3)
        print(json.dumps(self.record, separators=(",", ":")), file=sys.stderr, flush=True)
