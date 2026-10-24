#!/usr/bin/env python3
"""Resolve the owner-selected venv without installing anything at door start."""
import json
import os
from pathlib import Path
import sys
import tomllib


def main():
    hello = json.loads(sys.stdin.buffer.readline(65537))
    if hello.get("type") != "hello" or hello.get("version") != 2:
        raise ValueError("scoped v2 required")
    cfg = {}
    if hello.get("config_path"):
        with open(hello["config_path"], "rb") as source:
            cfg = tomllib.load(source)
    venv = Path(cfg.get("venv", str(Path(hello["state_dir"]) / ".venv")))
    if not venv.is_absolute() or not (venv / "pyvenv.cfg").is_file():
        raise ValueError("bootstrap a private venv first")
    python = str(venv / "bin" / "python")
    os.execv(python, [python, "-E", "-s", str(Path(__file__).with_name("door.py")),
                     json.dumps(hello)])


if __name__ == "__main__":
    try:
        main()
    except Exception:
        print("MCP local-only: startup refused; check private venv and configuration", file=sys.stderr)
        sys.exit(1)
