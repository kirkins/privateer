#!/usr/bin/env python3
"""Native harness command adapters. No account, endpoint or model defaults."""
import json
import sys
import uuid
from pathlib import Path

from protocol import context, option, serve


PROFILES = {
    "codex": {"binary": "codex", "resume": "uuid", "effort": None},
    "pi": {"binary": "pi", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]},
    "pi-signed": {"binary": "pi-signed", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]},
    "omp": {"binary": "omp", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max", "auto"]},
    "claude": {"binary": "claude", "resume": "uuid", "effort": ["low", "medium", "high", "xhigh", "max"]},
    "opencode": {"binary": "opencode", "resume": "opencode", "effort": None},
    "gemini": {"binary": "gemini", "resume": "uuid", "effort": []},
    "cursor": {"binary": "cursor-agent", "resume": "uuid", "effort": []},
    "grok": {"binary": "grok", "resume": "uuid", "effort": None},
    "agy": {"binary": "agy", "resume": "id", "effort": ["low", "medium", "high", "xhigh", "max"]},
    "muse": {"binary": "muse", "resume": None, "effort": ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]},
    "devin": {"binary": "devin", "resume": "id", "effort": []},
    "kimi": {"binary": "kimi", "resume": "id", "effort": []},
}


def selector(options, key, allowed):
    value = option(options, key)
    if value is not None and allowed is not None and value not in allowed:
        raise ValueError("unsupported selector")
    return value


def boolean(options, key):
    value = options.get(key, False)
    if not isinstance(value, bool):
        raise ValueError("invalid boolean")
    return value


def session(value, profile):
    if not value:
        return None
    kind = profile["resume"]
    if kind is None or value in ("latest", "last"):
        raise ValueError("exact native session required")
    if kind == "uuid-or-file" and Path(value).is_absolute():
        path = Path(value).resolve(strict=True)
        if not path.is_file():
            raise ValueError("session file missing")
        return str(path)
    if kind == "opencode":
        if not value.startswith("ses_") or not value[4:].isalnum():
            raise ValueError("exact OpenCode session required")
    elif kind == "id":
        if not all(c.isascii() and (c.isalnum() or c in "-_") for c in value):
            raise ValueError("invalid native session id")
    elif str(uuid.UUID(value)) != value.lower():
        raise ValueError("full native session UUID required")
    return value


def build(harness, request):
    profile = PROFILES[harness]
    repo, prompt = context(request)
    options = request.get("options", {})
    allowed = {"model", "effort", "resume", "mode"}
    if harness == "codex":
        allowed |= {"sandbox", "ephemeral"}
    if harness in ("pi", "pi-signed", "omp"):
        allowed |= {"provider", "ephemeral"}
    if harness in ("claude", "grok", "gemini"):
        allowed |= {"permission_mode"}
    if harness == "gemini":
        allowed |= {"trust"}
    if harness == "claude":
        allowed |= {"bare"}
    if harness == "devin":
        allowed |= {"permission_mode", "sandbox", "trust"}
    if harness == "cursor":
        allowed |= {"sandbox", "trust"}
    if not isinstance(options, dict) or set(options) - allowed:
        raise ValueError("unknown native option")
    mode = selector(options, "mode", ["headless", "interactive"]) or "headless"
    model = option(options, "model")
    effort = selector(options, "effort", profile["effort"])
    resume = session(option(options, "resume"), profile)
    headless = mode == "headless"
    argv = [profile["binary"]]

    if harness == "codex":
        # Exec options belong before the resume subcommand. Passing -C or
        # --sandbox after `exec resume` is rejected by native Codex.
        if headless:
            argv += ["exec", "--json"]
        elif resume:
            argv += ["resume"]
        argv += ["-C", repo]
        sandbox = selector(options, "sandbox", ["read-only", "workspace-write", "danger-full-access"])
        if sandbox:
            argv += ["--sandbox", sandbox]
            if sandbox == "workspace-write":
                home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
                if not home.is_dir():
                    raise ValueError("operational home missing")
                argv += ["--add-dir", str(home)]
        if effort:
            argv += ["-c", "model_reasoning_effort=" + json.dumps(effort)]
        if boolean(options, "ephemeral"):
            if resume or not headless:
                raise ValueError("ephemeral requires a fresh headless run")
            argv += ["--ephemeral"]
        if model:
            argv += ["--model", model]
        if resume:
            argv += (["resume", resume] if headless else [resume])
        argv += ["--", prompt]
    elif harness in ("pi", "pi-signed", "omp"):
        if headless:
            argv += ["--print"]
        provider = option(options, "provider")
        if provider and not model:
            raise ValueError("provider selection requires a model")
        if provider:
            argv += ["--provider", provider]
        if model:
            argv += ["--model", model]
        if headless:
            argv += ["--mode", "json"]
        if effort:
            argv += ["--thinking", effort]
        if resume:
            argv += ["--resume" if harness == "omp" else "--session", resume]
        if boolean(options, "ephemeral"):
            if resume:
                raise ValueError("cannot resume an ephemeral session")
            argv += ["--no-session"]
        argv += ["--", prompt]
    elif harness == "claude":
        if boolean(options, "bare"):
            argv += ["--bare"]
        if headless:
            argv += ["--print", "--verbose", "--output-format", "stream-json"]
        permissions = selector(options, "permission_mode", ["acceptEdits", "auto", "bypassPermissions", "manual", "dontAsk", "plan"])
        if permissions:
            argv += ["--permission-mode", permissions]
        if model:
            argv += ["--model", model]
        if effort:
            argv += ["--effort", effort]
        if resume:
            argv += ["--resume", resume]
        argv += ["--", prompt]
    elif harness == "opencode":
        if headless:
            argv += ["run", "--format", "json", "--dir", repo]
        if model:
            argv += ["--model", model]
        if effort:
            if not headless:
                raise ValueError("OpenCode interactive variant is not qualified")
            argv += ["--variant", effort]
        if resume:
            argv += ["--session", resume]
        argv += (["--", prompt] if headless else ["--prompt", prompt])
    elif harness == "gemini":
        if boolean(options, "trust"):
            argv += ["--skip-trust"]
        if headless:
            argv += ["--output-format", "stream-json"]
        permissions = selector(options, "permission_mode", ["default", "auto_edit", "yolo", "plan"])
        if permissions:
            argv += ["--approval-mode", permissions]
        if model:
            argv += ["--model", model]
        if resume:
            argv += ["--resume", resume]
        argv += ["--prompt" if headless else "--prompt-interactive", prompt]
    elif harness == "cursor":
        if headless:
            argv += ["--print", "--output-format", "stream-json"]
        sandbox = selector(options, "sandbox", ["enabled", "disabled"])
        if sandbox:
            argv += ["--sandbox", sandbox]
        if boolean(options, "trust"):
            argv += ["--trust"]
        if model:
            argv += ["--model", model]
        if resume:
            argv += ["--resume", resume]
        argv += ["--", prompt]
    elif harness == "grok":
        permissions = selector(options, "permission_mode", ["default", "acceptEdits", "auto", "dontAsk", "bypassPermissions", "plan"])
        if permissions:
            argv += ["--permission-mode", permissions]
        if model:
            argv += ["--model", model]
        if effort:
            argv += ["--reasoning-effort", effort]
        if resume:
            argv += ["--resume", resume]
        argv += (["--output-format", "streaming-json", "--single", prompt] if headless else ["--", prompt])
    elif harness == "agy":
        if headless:
            argv += ["--output-format", "stream-json"]
        if model:
            argv += ["--model", model]
        if effort:
            argv += ["--effort", effort]
        if resume:
            argv += ["--conversation", resume]
        argv += ["--print" if headless else "--prompt-interactive", prompt]
    elif harness == "muse":
        if headless:
            argv += ["exec", "--json"]
        if model:
            argv += ["--model", model]
        if effort:
            argv += ["--reasoning-effort", effort]
        argv += ["--", prompt]
    elif harness == "devin":
        permissions = selector(options, "permission_mode", ["normal", "auto", "accept-edits", "smart", "dangerous", "autonomous"])
        sandbox = boolean(options, "sandbox")
        if permissions == "autonomous" and not sandbox:
            raise ValueError("Devin autonomous mode requires its sandbox")
        if permissions:
            argv += ["--permission-mode", permissions]
        if sandbox:
            argv += ["--sandbox"]
        if boolean(options, "trust"):
            argv += ["--respect-workspace-trust", "false"]
        if model:
            argv += ["--model", model]
        if resume:
            argv += ["--resume", resume]
        if headless:
            argv += ["--print"]
        argv += ["--", prompt]
    elif harness == "kimi":
        if not headless:
            raise ValueError("Kimi interactive briefs need a qualified readiness/delivery backend")
        if model:
            argv += ["--model", model]
        if resume:
            argv += ["--session", resume]
        # Current Kimi Code 2.x, not the archived MoonshotAI/kimi-cli.
        # --prompt already selects unattended mode and conflicts with --auto.
        argv += ["--output-format", "stream-json", "--prompt", prompt]
    return repo, argv


def handle(harness, request):
    repo, argv = build(harness, request)
    return {"valid": True} if request["method"] == "validate" else {
        "argv": [sys.executable, str(Path(__file__).with_name("native_launch.py")), harness, *argv],
        "cwd": repo, "env_names": []}


if __name__ == "__main__":
    if sys.argv[1:] == ["--describe"]:
        print(json.dumps({name: {"binary": p["binary"], "headless": True,
                                "interactive_command": name != "kimi", "resume_selector": p["resume"],
                                "effort": p["effort"], "live_injection": False,
                                "structured_headless_output": name != "devin",
                                "qualification": "command-plan; native pilots required"}
                          for name, p in PROFILES.items()}, indent=2))
    elif len(sys.argv) == 2 and sys.argv[1] in PROFILES:
        serve(lambda request: handle(sys.argv[1], request))
    else:
        raise SystemExit("Expected a native harness id or --describe")
