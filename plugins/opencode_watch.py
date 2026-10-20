#!/usr/bin/env python3
"""Run a headless OpenCode V1 turn, ending it when a provider error stalls it.

On a provider error such as a spent free allowance, OpenCode logs one
`stream error` and then waits silently, printing no event and never exiting,
so the run would hold its crew until the job deadline and the route would
never back off. This runs OpenCode with its logs on stderr and passes its
event stream through unchanged, keeping only warnings and errors from the log.
When the main agent's stream fails and neither an event nor a retried stream
follows within the grace period, it stops OpenCode and ends the stream with
that error as OpenCode's own final `error` event, which the dispatcher reads
like any other throttle. A stream that recovers keeps running.

OpenCode also sleeps out a provider's `retry-after` without a log line, so a
run told to wait longer than the grace period is ended too: its route backs
off and the crew moves on, instead of holding a slot for an unknown time.

A rate or usage limit is not a stall to wait out: the provider has said the
allowance is spent, so however long OpenCode would sit there, the answer will
not change. Such an error ends the run after a short grace instead (5 s by
default, `PRIVATEER_OPENCODE_RATE_SECS`), still as OpenCode's own final
`error` event, which the dispatcher reads as a throttle: the route is held
until its reset at once and the crew can move to another route. Any other
error, and silence, keep the full stall grace.
"""
import io
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time

GRACE_SECS = 300
RATE_GRACE_SECS = 5
ERROR = re.compile(r'error\.error="((?:[^"\\]|\\.)*)"')
SESSION = re.compile(r" session\.id=(\S+)")


def main_stream(line):
    """Whether an OpenCode log line is about the main agent's model stream. The
    small model that titles a session streams and fails on its own."""
    return " small=false" in line and " providerID=" in line


def stream_error(line):
    """The provider error in an OpenCode log line, if it failed the main agent's
    stream."""
    if 'message="stream error"' not in line or not main_stream(line):
        return None
    found = ERROR.search(line)
    if not found:
        return "stream error"
    return re.sub(r"\\(.)", r"\1", found.group(1)).removeprefix("AI_APICallError: ")


def grace():
    try:
        return max(0.1, float(os.environ.get("PRIVATEER_OPENCODE_STALL_SECS", GRACE_SECS)))
    except ValueError:
        return GRACE_SECS


def rate_grace():
    """The grace for a rate or usage limit: long enough for a retry the
    provider does itself to land, short of holding the crew while its
    allowance is spent."""
    try:
        return max(0.1, float(os.environ.get("PRIVATEER_OPENCODE_RATE_SECS", RATE_GRACE_SECS)))
    except ValueError:
        return RATE_GRACE_SECS


def is_limit(error):
    """Whether a stream error is a rate or usage limit: the wordings the
    route ledger reads as a throttle, which is how OpenCode's free tier says
    its allowance is spent."""
    lowered = (error or "").lower()
    return any(sign in lowered for sign in ("rate limit", "rate_limit", "usage limit", "usage_limit"))


def run(argv):
    if len(argv) < 2 or argv[0] != "opencode" or argv[1] != "run":
        raise ValueError("invalid-opencode-command")
    if "--print-logs" not in argv:
        argv = [argv[0], argv[1], "--print-logs", "--log-level", "INFO", *argv[2:]]
    # The child stays in the job's process group, so cancelling the job ends it.
    child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
    state = {"output": time.monotonic(), "error": None, "at": None, "session": None, "newline": True}
    lock = threading.Lock()

    def forward_stdout():
        while chunk := child.stdout.read(65536):
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            with lock:
                state["output"], state["newline"] = time.monotonic(), chunk.endswith(b"\n")

    def forward_stderr():
        for raw in io.BufferedReader(child.stderr, 65536):
            line = raw.decode("utf-8", "replace")
            session = SESSION.search(line)
            session = session and session.group(1)
            if " level=INFO " not in line and " level=DEBUG " not in line:
                sys.stderr.buffer.write(raw)
                sys.stderr.buffer.flush()
            error = stream_error(line)
            with lock:
                if error:
                    state.update(error=error, at=time.monotonic(), session=session)
                elif (" message=stream " in line and main_stream(line)
                      and session == state["session"]):
                    # OpenCode is retrying that stream: not stalled yet.
                    state["at"] = None

    def end(signum, _frame):
        child.terminate()
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, end)
    readers = [threading.Thread(target=forward_stdout, daemon=True),
               threading.Thread(target=forward_stderr, daemon=True)]
    for reader in readers:
        reader.start()
    limit = grace()
    short = rate_grace()
    while child.poll() is None:
        time.sleep(min(1.0, limit / 4, short / 4))
        with lock:
            error = state["error"]
            # Silence counts from the error or the last event, whichever came
            # later: the two pipes are read on their own threads, so an event
            # OpenCode printed just before the error may be read after it.
            waited = (None if state["at"] is None
                      else time.monotonic() - max(state["at"], state["output"]))
            # A rate or usage limit is a spent allowance, so it ends the run
            # after the short grace; any other error, and silence, keep the
            # full one.
            ended = short if error is not None and is_limit(error) else limit
            stalled = waited is not None and waited >= ended
        # A run that ended meanwhile keeps its own exit status.
        if stalled and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            readers[0].join(timeout=2)
            # The final event starts its own line even after a partial one.
            sys.stdout.buffer.write((b"" if state["newline"] else b"\n") + (json.dumps({
                "type": "error", "source": "privateer-opencode-watch",
                "stalled_secs": round(ended), "error": {"name": "ProviderStalled", "data": {"message": error}},
            }, separators=(",", ":")) + "\n").encode())
            sys.stdout.buffer.flush()
            return 2
    for reader in readers:
        reader.join(timeout=5)
    code = child.returncode
    return 128 - code if code < 0 else code


if __name__ == "__main__":
    raise SystemExit(run(sys.argv[1:]))
