#!/usr/bin/env python3
"""Native harness command adapters. No account, endpoint or model defaults."""
import json
import sys
import uuid
from pathlib import Path

from protocol import context, option, serve
from tool_permissions import options_for, claude_rules, policy


PROFILES = {
    "codex": {"binary": "codex", "resume": "uuid", "effort": None},
    "pi": {"binary": "pi", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]},
    "pi-signed": {"binary": "pi-signed", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]},
    "omp": {"binary": "omp", "resume": "uuid-or-file", "effort": ["off", "minimal", "low", "medium", "high", "xhigh", "max", "auto"]},
    "claude": {"binary": "claude", "resume": "uuid", "effort": ["low", "medium", "high", "xhigh", "max"]},
    "opencode": {"binary": "opencode", "resume": "opencode", "effort": None},
    "opencode-v2": {"binary": "opencode", "resume": "opencode", "effort": None},
    "gemini": {"binary": "gemini", "resume": "uuid", "effort": []},
    "cursor": {"binary": "cursor-agent", "resume": "uuid", "effort": []},
    "grok": {"binary": "grok", "resume": "uuid", "effort": None},
    "agy": {"binary": "agy", "resume": "id", "effort": ["low", "medium", "high", "xhigh", "max"]},
    "muse": {"binary": "muse", "resume": "uuid", "effort": ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]},
    "devin": {"binary": "devin", "resume": "id", "effort": []},
    "kimi": {"binary": "kimi", "resume": "id", "effort": []},
    "rovo": {"binary": "rovo", "resume": "id", "effort": []},
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
    if harness in ("rovo", "kimi", "devin", "grok"):
        allowed |= {"transport", "acp_permissions"}
    if harness == "codex":
        allowed |= {"sandbox", "ephemeral", "yolo"}
    if harness == "opencode-v2":
        allowed |= {"auto"}
    if harness in ("pi", "pi-signed", "omp"):
        allowed |= {"provider", "ephemeral"}
    if harness in ("pi", "pi-signed", "omp"):
        allowed |= {"bare"}
    if harness == "omp":
        allowed |= {"permission_mode", "transport"}
    if harness in ("claude", "grok", "gemini"):
        allowed |= {"permission_mode"}
    if harness == "gemini":
        allowed |= {"trust"}
    if harness == "claude":
        allowed |= {"bare"}
    if harness == "devin":
        allowed |= {"permission_mode", "sandbox", "trust"}
    if harness == "cursor":
        allowed |= {"sandbox", "trust", "force"}
    if harness == "agy":
        allowed |= {"permission_mode", "skip_permissions", "sandbox", "disable_slash_commands"}
    if harness == "muse":
        allowed |= {"transport", "provider", "trust", "disable_sandbox", "sandbox_network",
                    "permission_mode", "bare", "msp_permissions", "msp_approval_mode"}
    if not isinstance(options, dict) or set(options) - allowed:
        raise ValueError("unknown native option")
    tool_policy = policy(request["context"].get("tool_permissions"))
    options = options_for(harness, options, tool_policy)
    mode = selector(options, "mode", ["headless", "interactive"]) or "headless"
    model = option(options, "model")
    effort = selector(options, "effort", profile["effort"])
    resume = session(option(options, "resume"), profile)
    headless = mode == "headless"
    argv = [profile["binary"]]
    if harness == "muse":
        transport = selector(options, "transport", ["native", "msp"]) or "native"
        if transport == "msp":
            if not headless or "permission_mode" in options or "bare" in options:
                raise ValueError("unsupported MSP options")
            provider = selector(options, "provider", ["echo", "meta", "local"])
            argv += ["serve"]
            if provider:
                argv += ["--provider", provider]
            if model:
                argv += ["--model", model]
            if boolean(options, "trust"):
                argv += ["--trust-workspace"]
            if boolean(options, "disable_sandbox"):
                argv += ["--disable-sandbox"]
            network = selector(options, "sandbox_network", ["restricted", "enabled", "proxy-only"])
            if network:
                argv += ["--sandbox-network", network]
            roots = [repo]
            control = request["context"]["control"]
            home = Path(control["cli_argv"][2]).resolve(strict=True)
            if not home.is_dir():
                raise ValueError("MSP operational home missing")
            for path in [home, *control.get("writable", [])]:
                path = Path(path).resolve()
                if path.is_dir() and str(path) not in roots:
                    roots.append(str(path))
            payload = {"argv": argv, "cwd": repo, "roots": roots, "prompt": prompt,
                       "model": model, "provider": provider, "resume": resume, "effort": effort,
                       "permissions": selector(options, "msp_permissions", ["deny", "allow_once"]) or "deny",
                       "approval_mode": selector(options, "msp_approval_mode", ["allowAll", "promptUnmatched", "onRequest", "denyUnmatched"])}
            return repo, [sys.executable, str(Path(__file__).with_name("msp.py")), "--request", json.dumps(payload)]
        if resume or "msp_permissions" in options or "msp_approval_mode" in options:
            raise ValueError("MSP transport required for unattended resume")
    if harness in ("rovo", "kimi", "devin", "grok"):
        transport = selector(options, "transport", ["native", "acp"]) or ("acp" if harness == "rovo" or (harness == "grok" and headless) else "native")
        if transport == "acp":
            accepted = {"mode", "model", "resume", "transport", "acp_permissions"}
            if harness == "grok":
                accepted.add("effort")
            if not headless or set(options) - accepted:
                raise ValueError("unsupported ACP options")
            permissions = selector(options, "acp_permissions", ["deny", "allow_once"]) or "deny"
            payload = {"binary": profile["binary"], "cwd": repo, "prompt": prompt, "model": model,
                       "resume": resume, "permissions": "auto" if permissions == "allow_once" else "ask"}
            if harness == "grok":
                payload["effort"] = effort
            return repo, [sys.executable, str(Path(__file__).with_name("acp.py")), "--request", json.dumps(payload)]
        if harness == "grok" and headless:
            raise ValueError("Grok headless jobs require their own ACP host")
        if harness == "rovo" or "acp_permissions" in options:
            raise ValueError("ACP transport required")

    if harness == "codex":
        # Exec options belong before the resume subcommand. Passing -C or
        # --sandbox after `exec resume` is rejected by native Codex.
        if headless:
            argv += ["exec", "--json"]
        elif resume:
            argv += ["resume"]
        argv += ["-C", repo]
        if boolean(options, "yolo"):
            argv += ["--dangerously-bypass-approvals-and-sandbox"]
        sandbox = selector(options, "sandbox", ["read-only", "workspace-write", "danger-full-access"])
        if sandbox:
            argv += ["--sandbox", sandbox]
            if sandbox == "workspace-write":
                home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
                if not home.is_dir():
                    raise ValueError("operational home missing")
                argv += ["--add-dir", str(home)]
                # The runtime also names the shared resource ledger, which a
                # first mate needs to start crew jobs.
                for extra in request["context"]["control"].get("writable", []):
                    extra = Path(extra).resolve()
                    if extra != home and extra.is_dir():
                        argv += ["--add-dir", str(extra)]
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
        transport = "native"
        if harness == "omp":
            transport = selector(options, "transport", ["native", "rpc"]) or ("rpc" if headless else "native")
            if transport == "rpc" and not headless:
                raise ValueError("OMP RPC requires headless mode")
            if resume and transport != "rpc":
                raise ValueError("OMP resume requires verified RPC preflight")
            # Worker-local overrides: never inherit "latest session", a plan
            # prompt, or an automatic model handoff from personal settings.
            argv += ["--config", str(Path(__file__).with_name("omp") / "session.yml"), "--no-prewalk", "--cwd", repo]
            home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
            if not home.is_dir():
                raise ValueError("OMP operational home missing")
            added = set()
            for path in [home, *request["context"]["control"].get("writable", [])]:
                path = Path(path).resolve()
                if path.is_dir() and path not in added:
                    argv += ["--add-dir", str(path)]
                    added.add(path)
            permissions = selector(options, "permission_mode", ["always-ask", "write", "yolo"])
            if permissions:
                argv += ["--approval-mode", permissions]
            if boolean(options, "bare"):
                argv += ["--no-extensions", "--no-skills", "--no-rules", "--no-title", "--no-lsp", "--no-pty"]
        if harness != "omp" and boolean(options, "bare"):
            # Per-run resource isolation; retain native authentication and
            # model/provider files, including any private gateway route.
            argv += ["--no-extensions", "--no-mcp", "--no-skills", "--no-prompt-templates",
                     "--no-context-files", "--no-themes"]
        if headless and transport != "rpc":
            argv += ["--print"]
        provider = option(options, "provider")
        if provider and not model:
            raise ValueError("provider selection requires a model")
        if provider:
            argv += ["--provider", provider]
        if model:
            argv += ["--model", model]
        if transport == "rpc":
            argv += ["--mode", "rpc", "--no-ui"]
        elif headless:
            argv += ["--mode", "json"]
        if effort:
            argv += ["--thinking", effort]
        if resume:
            argv += ["--resume" if harness == "omp" else "--session", resume]
        if boolean(options, "ephemeral"):
            if resume:
                raise ValueError("cannot resume an ephemeral session")
            argv += ["--no-session"]
        if transport == "rpc":
            payload = {"argv": argv, "cwd": repo, "prompt": prompt, "resume": resume,
                       "model": model, "provider": provider, "effort": effort,
                       "ephemeral": boolean(options, "ephemeral")}
            return repo, [sys.executable, str(Path(__file__).with_name("omp_rpc.py")), "--request", json.dumps(payload)]
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
        # Claude ignores a project's permissions.allow until someone accepts
        # its trust dialog, which a new ship's folder never had. Grant the
        # Privateer CLI on the command line so the agent can reach its home.
        control = request["context"].get("control")
        argv += ["--allowedTools", *claude_rules(tool_policy, control)]
        argv += ["--", prompt]
    elif harness == "opencode-v2":
        if boolean(options, "auto"):
            argv += ["--auto"]
        # V2 uses a different CLI/server/plugin contract. A private server
        # keeps tools in this job's actor environment, outside a shared daemon.
        if not headless and (model or effort):
            # The full V2 TUI has no --model flag. Do not silently discard an
            # explicit selection or substitute the different mini interface.
            raise ValueError("select OpenCode V2 interactive models in the native TUI")
        if effort:
            if not model or "#" in model or len(effort) > 64 or not all(c.isascii() and (c.isalnum() or c in "-_") for c in effort):
                raise ValueError("OpenCode V2 variant requires an unqualified explicit model")
            model += "#" + effort
        if headless:
            argv += ["run", "--standalone", "--format", "json"]
        else:
            argv += [repo, "--standalone"]
        if model:
            argv += ["--model", model]
        if resume:
            argv += ["--session", resume]
        argv += (["--", prompt] if headless else ["--prompt", prompt])
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
            if sandbox == "enabled":
                home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
                if not home.is_dir():
                    raise ValueError("operational home missing")
                argv += ["--add-dir", str(home)]
                for extra in request["context"]["control"].get("writable", []):
                    extra = Path(extra).resolve()
                    if extra != home and extra.is_dir():
                        argv += ["--add-dir", str(extra)]
        if boolean(options, "force"):
            argv += ["--force"]
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
        permissions = selector(options, "permission_mode", ["accept-edits", "plan"])
        if permissions:
            argv += ["--mode", permissions]
        if boolean(options, "skip_permissions"):
            argv += ["--dangerously-skip-permissions"]
        if boolean(options, "sandbox"):
            argv += ["--sandbox"]
        home = Path(request["context"]["control"]["cli_argv"][2]).resolve(strict=True)
        if not home.is_dir():
            raise ValueError("AGY operational home missing")
        added = set()
        for path in [home, *request["context"]["control"].get("writable", [])]:
            path = Path(path).resolve()
            if path.is_dir() and path not in added:
                argv += ["--add-dir", str(path)]
                added.add(path)
        if boolean(options, "disable_slash_commands"):
            if not headless:
                raise ValueError("AGY slash expansion control requires print mode")
            argv += ["--disable-slash-commands"]
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
        provider = selector(options, "provider", ["echo", "meta"])
        if provider:
            argv += ["--provider", provider]
        permissions = selector(options, "permission_mode", ["untrusted", "on-request", "never"])
        if permissions:
            argv += ["--approval-mode", permissions]
        if boolean(options, "trust"):
            argv += ["--trust-workspace"]
        if boolean(options, "disable_sandbox"):
            argv += ["--disable-sandbox"]
        network = selector(options, "sandbox_network", ["restricted", "enabled", "proxy-only"])
        if network:
            argv += ["--sandbox-network", network]
        if boolean(options, "bare"):
            if not headless:
                raise ValueError("Muse bare options require exec mode")
            argv += ["--no-foreign-personal-context", "--disable-reminders", "--disable-web-tools"]
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
    prefix = (["--control", json.dumps(request["context"].get("control"))] if harness == "claude" else [])
    if harness == "opencode" and policy(request["context"].get("tool_permissions"))["mode"] == "yolo":
        prefix = ["--opencode-yolo"]
    return {"valid": True} if request["method"] == "validate" else {
        "argv": argv if len(argv) > 1 and argv[0] == sys.executable and argv[1] in [str(Path(__file__).with_name(name)) for name in ("acp.py", "msp.py", "omp_rpc.py")] else [sys.executable, str(Path(__file__).with_name("native_launch.py")), *prefix, harness, *argv],
        "cwd": repo, "env_names": []}


if __name__ == "__main__":
    if sys.argv[1:] == ["--describe"]:
        print(json.dumps({name: {"binary": p["binary"], "headless": True,
                                "interactive_command": name not in ("kimi", "rovo"), "resume_selector": p["resume"],
                                "effort": p["effort"], "live_injection": False,
                                "structured_headless_output": name != "devin",
                                "acp_transport": name in ("rovo", "kimi", "devin", "grok"),
                                "msp_transport": name == "muse",
                                "rpc_transport": name == "omp",
                                "qualification": "command-plan; native pilots required"}
                          for name, p in PROFILES.items()}, indent=2))
    elif len(sys.argv) == 2 and sys.argv[1] in PROFILES:
        serve(lambda request: handle(sys.argv[1], request))
    else:
        raise SystemExit("Expected a native harness id or --describe")
