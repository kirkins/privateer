#!/usr/bin/env python3
"""OpenCode V2 launch preflight. Never turn a missing resume into a new session."""
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import time

from native_launch import native_environment


def inspect(argv, limit=65536, seconds=10):
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, env=native_environment(), bufsize=0,
                             start_new_session=True)
    end = time.monotonic() + seconds
    data = bytearray()
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while True:
                remaining = end - time.monotonic()
                if remaining <= 0:
                    raise ValueError("native-inspection-timeout")
                if not selector.select(remaining):
                    raise ValueError("native-inspection-timeout")
                chunk = os.read(child.stdout.fileno(), min(4096, limit + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > limit:
                    raise ValueError("native-inspection-size")
        if child.wait(timeout=max(0.01, end-time.monotonic())) != 0:
            raise ValueError("native-inspection-failed")
        return bytes(data)
    finally:
        child.stdout.close()
        # These short, read-only inspections own their private server group.
        # A descendant holding stdout open must not outlive a failed preflight.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=2)


def preflight(argv):
    if not argv or argv[0] != "opencode":
        raise ValueError("native-command")
    flags = argv[:argv.index("--")] if "--" in argv else argv
    selected = {}
    switches = set()
    i = 1
    while i < len(flags):
        value = flags[i]
        if value in ("--session", "--model", "--format", "--prompt", "--agent", "--title", "--file"):
            if value in selected or i + 1 == len(flags):
                raise ValueError("invalid-native-option")
            selected[value] = flags[i + 1]
            i += 2
            continue
        if value not in ("run", "--standalone", "--help", "--auto", "--thinking"):
            if value.startswith("-") or not Path(value).is_absolute() or Path(value).resolve() != Path.cwd().resolve():
                raise ValueError("private-exact-session-required")
        elif value.startswith("--"):
            switches.add(value)
        i += 1
    if "--standalone" not in switches:
        raise ValueError("private-exact-session-required")
    version = inspect([argv[0], "--version"], limit=4096, seconds=5).decode().strip()
    if not re.fullmatch(r"(?:opencode v)?2\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?", version):
        raise ValueError("OpenCode-V2-required")
    # --help is a read-only parser probe, not a request to resume its sample ID.
    if "--help" in switches:
        return True
    if "--session" in selected:
        session = selected["--session"]
        if not re.fullmatch(r"ses_[A-Za-z0-9]+", session):
            raise ValueError("exact-session-required")
        result = json.loads(inspect([argv[0], "api", "--standalone", "session.get", "--param", "sessionID="+session]))
        metadata = result.get("data") if isinstance(result, dict) else None
        location = metadata.get("location") if isinstance(metadata, dict) else None
        directory = location.get("directory") if isinstance(location, dict) else None
        if (not isinstance(metadata, dict) or metadata.get("id") != session
                or not isinstance(directory, str) or not Path(directory).is_absolute()
                or Path(directory).resolve() != Path.cwd().resolve()):
            raise ValueError("resume-session-workspace-mismatch")
    return False


def reconcile_final(session, message):
    """Read one exact final step when V2's JSON projection drops its finish.

    The native CLI can reconcile final text from storage without projecting the
    corresponding step_finish. Require both that same assistant message and a
    subsequent successful idle marker, never just a zero exit or response text.
    All inspections are bounded and read-only; this never resubmits a prompt.
    """
    result = json.loads(inspect([
        "opencode", "api", "--standalone", "session.message.list",
        "--param", "sessionID=" + session, "--param", "limit=2", "--param", "order=desc",
    ]))
    data = result.get("data") if isinstance(result, dict) else None
    if not isinstance(data, list) or len(data) != 2:
        raise ValueError("missing-final-projection")
    idle, final = data
    if (not isinstance(idle, dict) or idle.get("type") != "idle"
            or idle.get("outcome") != "succeeded" or idle.get("error") is not None
            or not isinstance(final, dict) or final.get("type") != "assistant"
            or final.get("id") != message or final.get("finish") != "stop"
            or final.get("error") is not None or final.get("retry") is not None):
        raise ValueError("unconfirmed-final-projection")
    final_time, idle_time = final.get("time"), idle.get("time")
    if not isinstance(final_time, dict) or not isinstance(idle_time, dict):
        raise ValueError("missing-final-time")
    times = [final_time.get("created"), final_time.get("completed"), idle_time.get("created")]
    if (any(type(t) is not int or t <= 0 for t in times)
            or not times[0] <= times[1] <= times[2]):
        raise ValueError("unsettled-final-projection")
    content = final.get("content")
    if not isinstance(content, list) or any(
        not isinstance(item, dict) or item.get("type") not in ("text", "reasoning", "tool")
        or (item.get("type") == "tool" and (
            not isinstance(item.get("state"), dict)
            or item["state"].get("status") not in ("completed", "error")))
        for item in content
    ):
        raise ValueError("unsettled-final-content")
    result = json.loads(inspect([
        "opencode", "api", "--standalone", "session.get", "--param", "sessionID=" + session,
    ]))
    metadata = result.get("data") if isinstance(result, dict) else None
    location = metadata.get("location") if isinstance(metadata, dict) else None
    directory = location.get("directory") if isinstance(location, dict) else None
    native_time = metadata.get("time") if isinstance(metadata, dict) else None
    if (not isinstance(metadata, dict) or metadata.get("id") != session
            or metadata.get("outcome") != "succeeded"
            or not isinstance(directory, str) or not Path(directory).is_absolute()
            or Path(directory).resolve() != Path.cwd().resolve()
            or not isinstance(native_time, dict) or type(native_time.get("idle")) is not int
            or native_time.get("idle") != times[2]):
        raise ValueError("final-session-workspace-or-idle-mismatch")
    return {"type": "step_finish", "sessionID": session, "part": {
        "type": "step-finish", "sessionID": session, "messageID": message,
        "reason": "stop", "tokens": final.get("tokens"),
    }}


def main():
    try:
        argv = sys.argv[1:]
        help_only = preflight(argv)
        if argv[1:2] == ["run"] and not help_only:
            from native_turn import run
            return run("opencode-v2", argv)
        os.execvpe(argv[0], argv, native_environment())
    except Exception:
        print(json.dumps({"type":"native.turn.failed", "harness":"opencode-v2",
                          "reason":"OpenCode V2 preflight or native completion was not confirmed."}), flush=True)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
