#!/usr/bin/env python3
"""Claude's headersHelper for one selected MCP server: print its Authorization
header from the captain-stored credential file. Claude runs it when it
connects, so the token never sits in argv, env values or the MCP config.
The same token rules as plugins/pi-mcp/selected.mjs readToken."""
import json
import os
import re
import sys

EXIT_UNAVAILABLE = 78
CREDENTIAL = re.compile(r"[A-Z][A-Z0-9_]{0,63}")
TOKEN = re.compile(r"[A-Za-z0-9._~+/-]+=*")
MAX_TOKEN = 4096


def header(name, env):
    """The header for credential `name`, or a ValueError that carries no value."""
    if not CREDENTIAL.fullmatch(name):
        raise ValueError("credential")
    path = env.get(f"PRIVATEER_CREDENTIAL_{name}_FILE")
    if not path:
        raise ValueError("credential")
    with open(path, "rb") as stream:
        raw = stream.read(MAX_TOKEN + 2)
    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError:
        raise ValueError("token") from None
    if text.endswith("\n"):
        text = text[:-1]
    if len(text) > MAX_TOKEN or not TOKEN.fullmatch(text):
        raise ValueError("token")
    return {"Authorization": f"Bearer {text}"}


def main(argv):
    name = argv[1] if len(argv) == 2 else ""
    try:
        print(json.dumps(header(name, os.environ)))
    except (OSError, ValueError):
        # One fixed line naming only the credential, as the Pi gate names its server.
        shown = name if CREDENTIAL.fullmatch(name) else "unknown"
        print(f"privateer: MCP credential {shown} is unavailable.", file=sys.stderr)
        return EXIT_UNAVAILABLE
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
