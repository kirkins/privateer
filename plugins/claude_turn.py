#!/usr/bin/env python3
"""Relay Claude JSON and surface native permission denials as durable questions."""
import json
import os
import subprocess
import sys

from native_launch import native_environment


def run(argv, control):
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             env=native_environment())
    denied = False
    try:
        while line := child.stdout.readline(1024 * 1024 + 1):
            if len(line) > 1024 * 1024:
                raise ValueError("native event exceeds limit")
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
            if line.strip():
                event = json.loads(line)
                if isinstance(event, dict) and event.get("type") == "result":
                    denied |= bool(event.get("permission_denials"))
        status = child.wait()
        if denied:
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
            return 2
        return status
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
