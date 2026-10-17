"""Claude Code bounded loader: selected skills only, through supported flags.

Exposure hygiene, not an OS/credential sandbox. Personal hooks, skills,
plugins and claude.ai connectors are dropped; a few harmless personal settings
are carried. A user settings file holding a provider route or credentials
refuses, since dropping it would change the route. Registry, selection and
pins live in bundles.py.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import uuid

from bundles import file_bytes, present, require, safe_path, validate_plan

# Keep the project's .claude settings (Privateer's hooks and permissions) and
# drop the user source; no MCP servers unless --mcp-config names them.
FLAGS = ["--setting-sources", "project,local", "--strict-mcp-config"]
CARRIED = ("model", "modelSettings", "effort", "autoMode", "skipDangerousModePermissionPrompt")
ROUTE = re.compile(r"env|key|auth|credential|token|secret|login|helper|provider|bedrock|vertex|foundry|url|proxy", re.I)
# --plugin-dir loads a skill root as a plugin; stage one is skills only.
PLUGIN_PARTS = ("hooks", ".mcp.json", ".lsp.json", "agents", "commands", "skills", "bin", "settings.json",
                "monitors", "output-styles", ".claude-plugin")
# Claude also loads these from --add-dir folders, which are not the project.
FOLDER_PARTS = (".claude/skills", ".claude/commands", ".claude/agents", ".mcp.json")
FORBIDDEN = ("--plugin-dir", "--plugin-url", "--settings", "--setting-sources", "--mcp-config", "--bare",
             "--safe-mode", "--agents", "--resume", "--continue", "--fork-session", "--session-id",
             "--from-pr", "--teleport")


def settings(env):
    """The carried allowlist from the user settings Claude would have loaded."""
    directory = Path(env.get("CLAUDE_CONFIG_DIR") or Path(env.get("HOME") or Path.home()) / ".claude")
    require(directory.is_absolute())
    path = directory / "settings.json"
    if not present(path):
        return {}
    config = json.loads(file_bytes(safe_path(str(path))))
    require(isinstance(config, dict) and not any(ROUTE.search(key) for key in config if key not in CARRIED))
    carried = {key: config[key] for key in CARRIED if key in config}
    require(len(json.dumps(carried)) <= 4096)
    return carried


def skills_only(skills):
    for item in skills:
        require(not any(present(Path(item["root"]) / part) for part in PLUGIN_PARTS))


def plan(registry, selected, skills):
    # Pi extension integrations cannot load into Claude.
    require("claude_version" in registry and registry["integrations"]["extensions"] == [])
    skills_only(skills)
    return {"schema_version": 1, "claude_version": registry["claude_version"], "selection": selected,
            "skills": skills, "settings": settings(os.environ)}


def preflight(value, argv, cwd=None, env=None, runtime_plugins=(), attended=False):
    """Recheck pins, carried settings and folders before native execution.
    Resume is the runtime's exact --resume, checked against the session's
    recorded plan digest before planning; attended starts are always fresh.
    """
    validate_plan(value)
    require("claude_version" in value)
    skills_only(value["skills"])
    env = os.environ if env is None else env
    require(settings(env) == value["settings"])
    require(argv and argv[0] == "claude")
    args = argv[1:]
    flags = args[:args.index("--")] if "--" in args else args
    runtime = [str(path) for path in runtime_plugins]
    folders = []
    i = 0
    while i < len(flags):
        arg = flags[i]
        name = arg.split("=", 1)[0]
        if arg == "--plugin-dir" and i + 1 < len(flags) and flags[i+1] in runtime:
            runtime.remove(flags[i+1])
            i += 2
        elif name == "--add-dir":
            i += 1
            if "=" in arg:
                folders.append(arg.split("=", 1)[1])
            else:
                while i < len(flags) and not flags[i].startswith("-"):
                    folders.append(flags[i])
                    i += 1
        elif (arg == "--resume" and not attended and i + 1 < len(flags)
                and str(uuid.UUID(flags[i+1])) == flags[i+1].lower()):
            i += 2
        else:
            require(name not in FORBIDDEN)
            # Short clusters such as -c, -r or -pc continue/resume sessions.
            require(not (arg.startswith("-") and not arg.startswith("--") and set(arg[1:]) & set("cr")))
            i += 1
    for folder in folders:
        require(not any(present(Path(folder) / part) for part in FOLDER_PARTS))
    binary = shutil.which(argv[0], path=env.get("PATH"))
    require(binary is not None)
    binary = Path(binary).resolve(strict=True)
    require(binary.name == "claude" and binary.is_file())
    # Refuse activation wrappers (for example mise) rather than run them.
    with open(binary, "rb") as stream:
        require(stream.read(2) != b"#!")
    probe = subprocess.run([str(binary), "--version"], env=env, cwd=cwd or os.getcwd(),
                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           timeout=10, check=False)
    require(probe.returncode == 0 and probe.stdout.split()[:1] == [value["claude_version"].encode()])
    carried = ["--settings", json.dumps(value["settings"], separators=(",", ":"))] if value["settings"] else []
    plugins = [arg for item in value["skills"] for arg in ("--plugin-dir", item["root"])]
    return [argv[0], *FLAGS, *carried, *plugins, *args]
