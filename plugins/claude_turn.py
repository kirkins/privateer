#!/usr/bin/env python3
"""Confirm a headless Claude run and record its native permission denials for the runtime."""
import json
import os
import subprocess
import sys

from native_launch import native_environment
from native_turn import MAX_LINE, Turn

USAGE_FIELDS = (("input_tokens", "input"), ("output_tokens", "output"),
                ("cache_read_input_tokens", "cacheRead"), ("cache_creation_input_tokens", "cacheWrite"))


class ClaudeTurn(Turn):
    """Claude's init names the session and one final result ends the run."""

    def __init__(self, argv, servers=()):
        flags = argv[:argv.index("--")] if "--" in argv else argv
        super().__init__(flags[flags.index("--resume") + 1] if "--resume" in flags else None)
        self.result = None
        self.denied = False
        self.denials = []
        self.servers = list(servers)

    def check_servers(self, reported, tools):
        """Every selected MCP server connected with tools, and no other: the Pi
        gate's guarantee, checked at init, before any tool runs. A run
        that selects none is not checked: unbounded runs keep the user's own
        servers, and a bounded one has --strict-mcp-config with no config."""
        if not self.servers:
            return
        reported = reported if isinstance(reported, list) else []
        tools = [tool for tool in tools if isinstance(tool, str)] if isinstance(tools, list) else []
        status = {}
        for item in reported:
            name = item.get("name") if isinstance(item, dict) else None
            if not (isinstance(name, str) and name in self.servers):
                shown = name if isinstance(name, str) and name.isascii() and name.isprintable() else "unknown"
                raise ValueError("mcp-foreign:" + shown[:32])
            status[name] = item.get("status")
        for name in self.servers:
            # Connected and listing tools, as the Pi gate requires.
            if status.get(name) != "connected" or not any(t.startswith(f"mcp__{name}__") for t in tools):
                raise ValueError("mcp-unavailable:" + name)

    def observe(self, value):
        if not isinstance(value, dict):
            raise ValueError("invalid-event")
        kind = value.get("type")
        if kind == "system" and value.get("subtype") == "init":
            super().observe({"type": "session", "id": value.get("session_id")})
            self.check_servers(value.get("mcp_servers"), value.get("tools"))
        elif kind == "result":
            if self.result is not None:
                raise ValueError("repeated-result")
            if value.get("session_id") != self.session:
                raise ValueError("changed-session")
            self.result = value
            denials = value.get("permission_denials")
            self.denied |= bool(denials)
            if isinstance(denials, list):
                self.denials = [d for d in denials if isinstance(d, dict)]
            # The result's usage already covers the whole run.
            usage = value.get("usage")
            self.usage = {}
            if isinstance(usage, dict):
                self.add_usage({new: usage[old] for old, new in USAGE_FIELDS if old in usage})
            if self.usage:
                self.usage["totalTokens"] = sum(self.usage.values())

    def complete(self):
        result = self.result or {}
        return (self.session is not None and result.get("subtype") == "success"
                and result.get("is_error") is False)


# The runtime reads denials from this line at settlement, not from Claude's
# result line, which a long log can lose. It stays short: a few inputs, clipped.
MAX_DENIALS = 4
INPUT_BYTES = 4096


def clipped(value):
    """A tool input as it was, or with its long strings cut to about INPUT_BYTES."""
    if len(json.dumps(value, separators=(",", ":"))) <= INPUT_BYTES:
        return value
    if isinstance(value, dict):
        return {k: (v[:INPUT_BYTES] if isinstance(v, str) else v) for k, v in value.items()
                if isinstance(v, str) or len(json.dumps(v)) <= INPUT_BYTES}
    return json.dumps(value)[:INPUT_BYTES]


def report_denial(denials):
    seen, kept = set(), []
    for denial in denials:
        entry = {"tool_name": str(denial.get("tool_name", "")), "tool_input": clipped(denial.get("tool_input"))}
        key = json.dumps(entry, sort_keys=True)
        if entry["tool_name"] and key not in seen:
            seen.add(key)
            kept.append(entry)
    print(json.dumps({"type": "native.permission_blocked", "denials": kept[:MAX_DENIALS],
                      "denied": len(kept),
                      "reason": "A native tool was denied; the runtime asks the captain once."},
                     separators=(",", ":")), flush=True)


def selected_servers(env):
    """Names of the run's selected MCP servers, from the launcher's records."""
    records = json.loads(env.get("PRIVATEER_MCP_SERVERS") or "[]")
    if not isinstance(records, list) or not all(isinstance(r, dict) and isinstance(r.get("name"), str)
                                                for r in records):
        raise ValueError("mcp-selection")
    return [record["name"] for record in records]


def run(argv, control):
    turn = ClaudeTurn(argv, selected_servers(os.environ))
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             env=native_environment())
    reason = None
    try:
        try:
            while line := child.stdout.readline(MAX_LINE + 1):
                if len(line) > MAX_LINE:
                    raise ValueError("event-size")
                if not line.endswith(b"\n"):
                    # A missing final LF is a truncated stream, not a completion fact.
                    raise ValueError("truncated-stream")
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
                if line.strip():
                    turn.observe(json.loads(line))
        except json.JSONDecodeError:
            reason = "invalid-json"
        except ValueError as error:
            reason = str(error)
        if reason:
            child.terminate()
        status = child.wait()
        success = reason is None and status == 0 and turn.complete() and not turn.denied
        summary = {"type": "turn.completed" if success else "native.turn.failed",
                   "harness": "claude", "session_id": turn.session, "native_exit_code": status}
        if reason:
            summary["reason"] = reason
        if turn.usage:
            summary["usage"] = turn.usage
        print(json.dumps(summary, separators=(",", ":")), flush=True)
        if turn.denied:
            report_denial(turn.denials)
        return 0 if success else 2
    finally:
        child.stdout.close()
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=2)


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[3:], json.loads(sys.argv[2])))
    except Exception:
        print('{"type":"native.turn.failed","reason":"Claude stream could not be read."}', flush=True)
        raise SystemExit(2)
