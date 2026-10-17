#!/usr/bin/env python3
"""Confirm a headless Claude run and surface native permission denials as durable questions."""
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

    def __init__(self, argv):
        flags = argv[:argv.index("--")] if "--" in argv else argv
        super().__init__(flags[flags.index("--resume") + 1] if "--resume" in flags else None)
        self.result = None
        self.denied = False

    def observe(self, value):
        if not isinstance(value, dict):
            raise ValueError("invalid-event")
        kind = value.get("type")
        if kind == "system" and value.get("subtype") == "init":
            super().observe({"type": "session", "id": value.get("session_id")})
        elif kind == "result":
            if self.result is not None:
                raise ValueError("repeated-result")
            if value.get("session_id") != self.session:
                raise ValueError("changed-session")
            self.result = value
            self.denied |= bool(value.get("permission_denials"))
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


def report_denial(control):
    reported = False
    if control and os.environ.get("PRIVATEER_JOB"):
        try:
            result = subprocess.run(control["cli_argv"] + ["permissions.blocked", "--json",
                                    json.dumps({"job_id": os.environ["PRIVATEER_JOB"]})],
                                    capture_output=True, timeout=15)
            reported = result.returncode == 0
        except (OSError, subprocess.TimeoutExpired):
            pass
    print(json.dumps({"type":"native.permission_blocked", "captain_question_recorded":reported,
                      "reason":"A native tool was denied; use privateer permissions to configure fleet access."}), flush=True)


def run(argv, control):
    turn = ClaudeTurn(argv)
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
            report_denial(control)
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
