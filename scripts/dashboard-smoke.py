#!/usr/bin/env python3
"""Exercise the live bridge in an isolated PTY with a private fake Herdr and fleet."""
import argparse
import os
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/privateer-dashboard")
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    supervisor = dict(os.environ)
    try:
        from dashboard_live_smoke import run
        run(binary, binary.with_name("privateer"))
    finally:
        assert dict(os.environ) == supervisor, "fixture changed supervisor environment"
        print("Fixture supervisor environment unchanged; role:", supervisor.get("PRIVATEER_ROLE", "unset"))


if __name__ == "__main__":
    main()
