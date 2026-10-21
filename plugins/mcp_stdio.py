#!/usr/bin/env python3
"""Start one selected stdio MCP server for Claude or Pi: re-pin its
executable, then exec it in place with a minimal environment.

The harness runs this as `INTERPRETER -I mcp_stdio.py SPEC`, SPEC being the
runtime-written JSON {"command": [...], "sha256": HEX, "credential"?:
{"name": NAME, "file": PATH}}, never read from the environment. Both harnesses
hand a stdio server their whole environment (provider keys, Claude's
messaging token, other credentials' paths); the server gets only PATH, HOME,
LANG, LC_*, TMPDIR and its own credential's file path instead.

exec keeps this process's pid and process group: no setsid or fork, so the
harness's stop and the job's teardown reach the server itself.
"""
import json
import os
from pathlib import Path
import sys

# -I leaves this script's directory off sys.path; bundles.py is beside it.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from bundles import CREDENTIAL_ENV, CREDENTIAL_NAME, HEX, command, plain, require  # noqa: E402

EXIT_REFUSED = 78
PATH = "/usr/bin:/bin"
KEPT = ("HOME", "LANG", "TMPDIR")


def spec(text):
    """The checked SPEC and the server's environment-to-be, or a refusal."""
    value = json.loads(text)
    require(isinstance(value, dict) and {"command", "sha256"} <= set(value) <= {"command", "sha256", "credential"})
    require(isinstance(value["sha256"], str) and HEX.fullmatch(value["sha256"]))
    # Path, owner, mode and content again, as close to exec as this can be.
    require(command(value["command"]) == value["sha256"])
    credential = value.get("credential")
    if credential is not None:
        require(isinstance(credential, dict) and set(credential) == {"name", "file"}
                and isinstance(credential["name"], str) and CREDENTIAL_NAME.fullmatch(credential["name"])
                and isinstance(credential["file"], str) and credential["file"].startswith("/")
                and plain(credential["file"]))
    return value


def environment(inherited, credential):
    env = {name: value for name, value in inherited.items() if name in KEPT or name.startswith("LC_")}
    env["PATH"] = PATH
    if credential is not None:
        env[f"{CREDENTIAL_ENV}{credential['name']}_FILE"] = credential["file"]
    return env


def main(argv):
    try:
        require(len(argv) == 2)
        value = spec(argv[1])
    except Exception:
        # One fixed line: no path, digest or argument from the spec.
        print("privateer: stdio MCP server refused: its executable or record no longer matches its approval.",
              file=sys.stderr)
        return EXIT_REFUSED
    try:
        os.execve(value["command"][0], value["command"], environment(os.environ, value.get("credential")))
    except OSError:
        print("privateer: stdio MCP server could not start.", file=sys.stderr)
        return EXIT_REFUSED


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
