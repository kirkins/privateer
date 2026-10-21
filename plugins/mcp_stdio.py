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

Before exec every inherited descriptor from 3 up is closed, so the server
holds only its stdio. Where the platform execs a descriptor (Linux), the
executable is exec'd through the very descriptor its pin was hashed from, so
the bytes hashed are the bytes run; elsewhere (macOS) by its path, rechecked
just before.
"""
import json
import os
from pathlib import Path
import resource
import sys

# -I leaves this script's directory off sys.path; bundles.py is beside it.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from bundles import CREDENTIAL_ENV, CREDENTIAL_NAME, HEX, command, plain, require  # noqa: E402

EXIT_REFUSED = 78
PATH = "/usr/bin:/bin"
KEPT = ("HOME", "LANG", "TMPDIR")


def spec(text):
    """The checked SPEC and the close-on-exec descriptor its executable was
    hashed through, or a refusal."""
    value = json.loads(text)
    require(isinstance(value, dict) and {"command", "sha256"} <= set(value) <= {"command", "sha256", "credential"})
    require(isinstance(value["sha256"], str) and HEX.fullmatch(value["sha256"]))
    credential = value.get("credential")
    if credential is not None:
        require(isinstance(credential, dict) and set(credential) == {"name", "file"}
                and isinstance(credential["name"], str) and CREDENTIAL_NAME.fullmatch(credential["name"])
                and isinstance(credential["file"], str) and credential["file"].startswith("/")
                and plain(credential["file"]))
    # Path, owner, mode and content again, as close to exec as this can be.
    digest, descriptor = command(value["command"], keep=True)
    if digest != value["sha256"]:
        os.close(descriptor)
        require(False)
    return value, descriptor


def close_inherited(keep):
    """Close every descriptor from 3 up, up to the soft open-file limit, but `keep`."""
    soft, _ = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft == resource.RLIM_INFINITY:
        soft = os.sysconf("SC_OPEN_MAX")
    os.closerange(3, keep)
    os.closerange(keep + 1, max(soft, keep + 1))


def environment(inherited, credential):
    env = {name: value for name, value in inherited.items() if name in KEPT or name.startswith("LC_")}
    env["PATH"] = PATH
    if credential is not None:
        env[f"{CREDENTIAL_ENV}{credential['name']}_FILE"] = credential["file"]
    return env


def main(argv):
    try:
        require(len(argv) == 2)
        value, descriptor = spec(argv[1])
    except Exception:
        # One fixed line: no path, digest or argument from the spec.
        print("privateer: stdio MCP server refused: its executable or record no longer matches its approval.",
              file=sys.stderr)
        return EXIT_REFUSED
    env = environment(os.environ, value.get("credential"))
    try:
        close_inherited(descriptor)
        # The descriptor is close-on-exec, so the server never holds it.
        if os.execve in os.supports_fd:
            os.execve(descriptor, value["command"], env)
        os.close(descriptor)
        os.execve(value["command"][0], value["command"], env)
    except OSError:
        print("privateer: stdio MCP server could not start.", file=sys.stderr)
        return EXIT_REFUSED


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
