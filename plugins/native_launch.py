#!/usr/bin/env python3
"""Replace this process with the chosen harness, clearing foreign identity only."""
import os
import sys

from native import PROFILES

IDENTITY_MARKERS = (
    "CLAUDECODE", "CURSOR_AGENT", "CURSOR_INVOKED_AS", "GEMINI_CLI", "GROK_AGENT",
    "PI_CODING_AGENT", "FM_PI_HARNESS", "FM_OMP_HARNESS", "ATLASSIAN_AGENT_TYPE",
    "ROVODEV_CLI",
)


def launch(harness, argv):
    if harness not in PROFILES or not argv or argv[0] != PROFILES[harness]["binary"]:
        raise ValueError("invalid native launch")
    env = os.environ.copy()
    for name in IDENTITY_MARKERS:
        env.pop(name, None)
    # Keep PRIVATEER_ROLE/ACTOR and all native auth/provider configuration.
    # exec preserves the job's process identity, group and cancellation path.
    os.execvpe(argv[0], argv, env)


def main():
    try:
        launch(sys.argv[1], sys.argv[2:])
    except Exception:
        print("Native harness launch failed; check the selected CLI installation locally.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
