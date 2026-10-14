"""Tiny adapter protocol helpers; never print raw exception/config contents."""
import json
import os
import stat
import sys
from pathlib import Path


def serve(handle):
    request = None
    try:
        line = sys.stdin.buffer.readline(65538)
        if len(line) > 65536:
            raise ValueError("request too large")
        request = json.loads(line)
        if request.get("version") != 1 or request.get("method") not in ("validate", "command"):
            raise ValueError("unsupported request")
        result = handle(request)
        response = {"id": request["id"], "version": 1, "ok": True, "result": result}
    except Exception:
        response = {"id": request.get("id") if isinstance(request, dict) else None,
                    "version": 1, "ok": False,
                    "error": "Adapter validation failed; inspect private configuration locally."}
    print(json.dumps(response, separators=(",", ":")))


def context(request):
    value = request["context"]
    repo = Path(value["repo"]).resolve(strict=True)
    if not repo.is_dir():
        raise ValueError("repository missing")
    prompt = value.get("brief", value["title"])
    return str(repo), prompt


def option(options, key):
    value = options.get(key)
    if value is not None and (not isinstance(value, str) or not value or len(value) > 512
                              or value.startswith("-") or "\0" in value or "\n" in value or "\r" in value):
        raise ValueError("invalid option")
    return value


def private_json(path):
    path = Path(path).expanduser()
    if not path.is_absolute() or path.is_symlink():
        raise ValueError("private config must be a regular absolute path")
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_uid != os.getuid():
        raise ValueError("private config permissions invalid")
    if info.st_size > 1024 * 1024:
        raise ValueError("config too large")
    return json.loads(path.read_text())
