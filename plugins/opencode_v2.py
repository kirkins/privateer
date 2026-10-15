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
