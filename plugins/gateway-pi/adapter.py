#!/usr/bin/env python3
"""Optional private gateway adapter. No keys are returned by the protocol."""
import os
import json
import sys
from pathlib import Path
from urllib.parse import urlsplit
sys.dont_write_bytecode = True  # a release holds only its own files
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from protocol import serve, context, private_json
from native import build
from native_launch import launch as native_launch


def route(config_path):
    config = private_json(config_path)
    if config.get("schema_version") != 1:
        raise ValueError("unsupported gateway config")
    if set(config) != {"schema_version", "pi_config", "endpoint", "provider", "model"}:
        raise ValueError("gateway config accepts route metadata only")
    endpoint, provider, model = (config.get(k) for k in ("endpoint", "provider", "model"))
    if any(not isinstance(v, str) or not v or "\0" in v for v in (endpoint, provider, model)):
        raise ValueError("route incomplete")
    if any(v.startswith("-") or len(v) > 512 or "\n" in v or "\r" in v for v in (provider, model)):
        raise ValueError("invalid native selector")
    url = urlsplit(endpoint)
    if url.scheme not in ("http", "https") or not url.hostname or url.username or url.password or url.query or url.fragment:
        raise ValueError("invalid credential-free endpoint")
    selected_path = Path(config["pi_config"]).expanduser()
    native = private_json(selected_path)
    selected = selected_path.resolve(strict=True)
    active_dir = Path(os.environ.get("PI_CODING_AGENT_DIR", "~/.pi/agent")).expanduser()
    if selected != (active_dir / "models.json").resolve(strict=True):
        raise ValueError("selected Pi config differs from active config")
    entry = native.get("providers", {}).get(provider, {})
    if entry.get("baseUrl") != endpoint:
        raise ValueError("gateway endpoint mismatch")
    models = [item for item in entry.get("models", []) if isinstance(item, dict) and item.get("id") == model]
    if len(models) != 1:
        raise ValueError("model missing from configured gateway")
    override = entry.get("modelOverrides", {}).get(model, {})
    if models[0].get("baseUrl", endpoint) != endpoint or override.get("baseUrl", endpoint) != endpoint:
        raise ValueError("model endpoint differs from gateway")
    return provider, model


def handle(request):
    repo, prompt = context(request)
    config_path = request["config_path"]
    provider, model = route(config_path)
    options = pinned_options(request.get("options", {}), provider, model)
    build("pi", {**request, "options": options})
    # Recheck at invocation, so changing the route after planning does not cause silent fallback.
    return {"valid": True} if request["method"] == "validate" else {
        "argv": [sys.executable, str(Path(__file__).resolve()), "--launch", config_path, repo, json.dumps(options), prompt],
        "cwd": repo, "env_names": ["PI_CODING_AGENT_DIR"]}


def pinned_options(options, provider, model):
    if not isinstance(options, dict):
        raise ValueError("invalid gateway options")
    for key, value in (("provider", provider), ("model", model)):
        if key in options and options[key] != value:
            raise ValueError("gateway selection differs from the pinned route")
    return {**options, "provider": provider, "model": model}


def launch():
    try:
        if len(sys.argv) != 6:
            raise ValueError("invalid launcher arguments")
        provider, model = route(sys.argv[2])
        repo = Path(sys.argv[3]).resolve(strict=True)
        options = pinned_options(json.loads(sys.argv[4]), provider, model)
        _, argv = build("pi", {"context": {"repo": str(repo), "title": sys.argv[5]}, "options": options})
        os.chdir(repo)
        native_launch("pi", argv)
    except Exception:
        print("Gateway route validation failed; refusing Pi launch.", file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--launch":
        launch()
    else:
        serve(handle)
