#!/usr/bin/env python3
"""Token-free native class/session seam check; no model/provider/server calls."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

pi = str(Path(sys.argv[1]).resolve(strict=True))
fixture = Path(__file__).with_name("pi-calm-native.ts").resolve()
with tempfile.TemporaryDirectory(prefix="privateer-calm-native-") as scratch:
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("PRIVATEER_", "PI_"))}
    env.update(HOME=scratch, XDG_CONFIG_HOME=scratch,
               PI_CODING_AGENT_DIR=str(Path(scratch) / "agent"),
               PI_OFFLINE="1", PI_SKIP_VERSION_CHECK="1", PI_TELEMETRY="0",
               PRIVATEER_HOME=str(Path(scratch) / ".privateer"),
               PRIVATEER_ROLE="agent", PRIVATEER_ACTOR="fixture",
               PRIVATEER_ATTENDED_HARNESS="pi")
    result = subprocess.run([pi, "--no-extensions", "--no-skills", "--no-context-files",
                             "--no-prompt-templates", "--mode", "rpc", "-e", str(fixture)],
                            input="", text=True, capture_output=True, cwd=scratch,
                            env=env, timeout=30)
    print(result.stdout, end="")
    print(result.stderr, end="", file=sys.stderr)
    if result.returncode or "zero model requests" not in result.stdout:
        raise SystemExit(result.returncode or 1)
