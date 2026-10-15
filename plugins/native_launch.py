#!/usr/bin/env python3
"""Replace this process with the chosen harness, clearing foreign identity only."""
import os
import sys
from pathlib import Path

from native import PROFILES

IDENTITY_MARKERS = (
    "CLAUDECODE", "CURSOR_AGENT", "CURSOR_INVOKED_AS", "GEMINI_CLI", "GROK_AGENT",
    "PI_CODING_AGENT", "FM_PI_HARNESS", "FM_OMP_HARNESS", "ATLASSIAN_AGENT_TYPE",
    "ROVODEV_CLI",
)


def native_environment():
    env = os.environ.copy()
    for name in IDENTITY_MARKERS:
        env.pop(name, None)
    return env


def launch(harness, argv):
    if harness not in PROFILES or not argv or argv[0] != PROFILES[harness]["binary"]:
        raise ValueError("invalid native launch")
    # Pi JSON mode may exit zero after an assistant error. Its event stream,
    # not the process status alone, must confirm a completed unattended run.
    flags = argv[:argv.index("--")] if "--" in argv else argv
    if harness in ("pi", "pi-signed") and "--print" in flags and any(
            flags[i:i+2] == ["--mode", "json"] for i in range(len(flags))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], native_environment())
    if harness == "agy" and "--print" in flags and any(
            flags[i:i+2] == ["--output-format", "stream-json"] for i in range(len(flags))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], native_environment())
    # Keep PRIVATEER_ROLE/ACTOR and all native auth/provider configuration.
    # exec preserves the job's process identity, group and cancellation path.
    os.execvpe(argv[0], argv, native_environment())


def main():
    try:
        launch(sys.argv[1], sys.argv[2:])
    except Exception:
        print("Native harness launch failed; check the selected CLI installation locally.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
