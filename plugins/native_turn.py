#!/usr/bin/env python3
"""Confirm native terminal JSON events while the runtime owns supervision."""
import json
import subprocess
import sys
import uuid
from pathlib import Path

from native_launch import native_environment

MAX_LINE = 1024 * 1024
TOKEN_FIELDS = ("input", "output", "cacheRead", "cacheWrite", "reasoning", "totalTokens")


class Turn:
    def __init__(self, expected=None):
        self.expected = expected
        self.session = None
        self.settled = False
        self.stop = None
        self.usage = {}

    def observe(self, value):
        if not isinstance(value, dict):
            raise ValueError("invalid-event")
        kind = value.get("type")
        if kind == "session":
            identity = value.get("id")
            if not isinstance(identity, str) or not 0 < len(identity) <= 512 or any(c.isspace() or ord(c) < 32 for c in identity):
                raise ValueError("invalid-session")
            if self.session is not None and self.session != identity:
                raise ValueError("changed-session")
            if self.expected is not None and identity != self.expected:
                raise ValueError("resume-identity")
            self.session = identity
        elif kind in ("agent_start", "turn_start", "auto_retry_start"):
            self.settled, self.stop = False, None
        elif kind in ("tool_execution_start", "compaction_start"):
            self.settled = False
        elif kind == "message_end":
            message = value.get("message", {})
            if not isinstance(message, dict):
                raise ValueError("invalid-message")
            if message.get("role") == "assistant":
                self.settled = False
                self.stop = message.get("stopReason")
            # Only authoritative completed messages count. Streaming deltas,
            # turn_end and agent_end repeat those same messages.
            if message.get("role") in ("assistant", "toolResult"):
                self.add_usage(message.get("usage"))
        elif kind == "compaction_end":
            result = value.get("result")
            if isinstance(result, dict):
                self.add_usage(result.get("usage"))
        elif kind == "agent_settled":
            self.settled = value.get("aborted") is False

    def add_usage(self, usage):
        if not isinstance(usage, dict):
            return
        for key in TOKEN_FIELDS:
            count = usage.get(key)
            if isinstance(count, int) and not isinstance(count, bool) and 0 <= count <= 2**53 - 1:
                total = self.usage.get(key, 0) + count
                if total > 2**53 - 1:
                    raise ValueError("usage-overflow")
                self.usage[key] = total

    def complete(self):
        return self.session is not None and self.settled and self.stop == "stop"


class AgyTurn(Turn):
    def __init__(self, argv):
        expected = argv[argv.index("--conversation") + 1] if "--conversation" in argv else None
        super().__init__(expected)
        self.cwd = Path.cwd().resolve()
        self.model = argv[argv.index("--model") + 1] if "--model" in argv else None
        self.result = False

    def observe(self, value):
        if not isinstance(value, dict):
            raise ValueError("invalid-event")
        kind = value.get("event")
        if kind == "init":
            if self.session is not None or self.result:
                raise ValueError("duplicate-init")
            identity = value.get("conversation_id")
            if (not isinstance(identity, str) or not 0 < len(identity) <= 512
                    or any(c.isspace() or ord(c) < 32 for c in identity)
                    or (self.expected is not None and identity != self.expected)):
                raise ValueError("conversation-identity")
            info = value.get("init")
            if (not isinstance(info, dict) or not isinstance(info.get("cwd"), str)
                    or not Path(info["cwd"]).is_absolute() or Path(info["cwd"]).resolve() != self.cwd):
                raise ValueError("conversation-workspace")
            if self.model is not None and info.get("model") != self.model:
                raise ValueError("model-unconfirmed")
            self.session = identity
        elif kind == "result":
            if self.result or self.session is None:
                raise ValueError("unexpected-result")
            result = value.get("result")
            if not isinstance(result, dict) or result.get("conversation_id") != self.session:
                raise ValueError("result-identity")
            self.result = True
            self.settled = result.get("status") == "SUCCESS"
            self.stop = "stop" if self.settled else None
            usage = result.get("usage")
            if isinstance(usage, dict):
                self.add_usage({new: usage[old] for old, new in [
                    ("input_tokens", "input"), ("output_tokens", "output"),
                    ("cache_read_tokens", "cacheRead"), ("cache_write_tokens", "cacheWrite"),
                    ("thinking_tokens", "reasoning"), ("total_tokens", "totalTokens")
                ] if old in usage})
        elif kind == "step_update":
            if self.session is None or self.result:
                raise ValueError("step-outside-turn")
            step = value.get("step_update")
            if not isinstance(step, dict) or step.get("conversation_id") != self.session:
                raise ValueError("step-identity")
        else:
            raise ValueError("unknown-native-event")


def requested_session(argv):
    flags = argv[:argv.index("--")] if "--" in argv else argv
    if "--session" not in flags:
        return None
    selected = flags[flags.index("--session") + 1]
    if Path(selected).is_absolute():
        with open(selected, "rb") as stream:
            header = stream.readline(65537)
        if len(header) > 65536:
            raise ValueError("session-header-size")
        value = json.loads(header)
        if not isinstance(value, dict) or value.get("type") != "session":
            raise ValueError("session-header")
        selected = value["id"]
    return str(uuid.UUID(selected))


def run(harness, argv):
    if harness not in ("pi", "pi-signed", "agy") or not argv or argv[0] != harness:
        raise ValueError("invalid-native-command")
    turn = AgyTurn(argv) if harness == "agy" else Turn(requested_session(argv))
    # The child stays in the owning job's group. Do not create a competing
    # process group, retry a model call, or infer task completion from prose.
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             env=native_environment(), bufsize=0)
    try:
        pending = bytearray()
        while chunk := child.stdout.read(65536):
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            pending.extend(chunk)
            while b"\n" in pending:
                line, _, pending = pending.partition(b"\n")
                if len(line) > MAX_LINE:
                    raise ValueError("event-size")
                if line.strip():
                    turn.observe(json.loads(line))
            if len(pending) > MAX_LINE:
                raise ValueError("event-size")
        # A missing final LF is a truncated stream, not a completion fact.
        native_status = child.wait()
        success = native_status == 0 and not pending and turn.complete()
        if pending:
            sys.stdout.buffer.write(b"\n")
            sys.stdout.buffer.flush()
        summary = {"type": "turn.completed" if success else "native.turn.failed",
                   "harness": harness, "session_id": turn.session, "native_exit_code": native_status}
        if turn.usage:
            summary["usage"] = turn.usage
        print(json.dumps(summary, separators=(",", ":")), flush=True)
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


def main():
    try:
        return run(sys.argv[1], sys.argv[2:])
    except Exception:
        print("\n" + json.dumps({"type": "native.turn.failed", "reason": "Native completion was not confirmed."}), flush=True)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
