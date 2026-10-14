#!/usr/bin/env python3
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from protocol import serve, context, option


def handle(request):
    repo, prompt = context(request)
    options = request.get("options", {})
    provider = option(options, "provider")
    model = option(options, "model")
    if provider and not model:
        raise ValueError("Pi provider selection requires a model")
    argv = ["pi", "--print"]
    if provider:
        argv += ["--provider", provider]
    if model:
        argv += ["--model", model]
    argv += [prompt]
    return {"valid": True} if request["method"] == "validate" else {
        "argv": argv, "cwd": repo, "env_names": []}


if __name__ == "__main__":
    serve(handle)
