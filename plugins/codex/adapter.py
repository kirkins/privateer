#!/usr/bin/env python3
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from protocol import serve, context, option


def handle(request):
    repo, prompt = context(request)
    options = request.get("options", {})
    model = option(options, "model")
    argv = ["codex", "exec", "--json", "-C", repo]
    if model:
        argv += ["--model", model]
    sandbox = option(options, "sandbox")
    if sandbox:
        if sandbox not in ("read-only", "workspace-write", "danger-full-access"):
            raise ValueError("invalid Codex sandbox")
        argv += ["--sandbox", sandbox]
        if sandbox == "workspace-write":
            home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
            if not home.is_dir():
                raise ValueError("operational home missing")
            argv += ["--add-dir", str(home)]
    ephemeral = options.get("ephemeral", False)
    if not isinstance(ephemeral, bool):
        raise ValueError("ephemeral must be a boolean")
    if ephemeral:
        argv += ["--ephemeral"]
    argv += [prompt]
    return {"valid": True} if request["method"] == "validate" else {
        "argv": argv, "cwd": repo, "env_names": []}


if __name__ == "__main__":
    serve(handle)
