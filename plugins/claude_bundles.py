"""Claude Code bounded loader: selected skills and HTTP or pinned stdio MCP
servers only, through supported flags.

Exposure hygiene, not an OS/credential sandbox. Personal hooks, skills,
plugins and claude.ai connectors are dropped; a few harmless personal settings
are carried. A user settings key outside the carried and dropped lists
refuses, since dropping a route, credential or restriction would change the
route or widen the launch. Registry, selection and pins live in bundles.py.
"""
import json
import os
from pathlib import Path
import shlex
import shutil
import sys
import uuid

import hashlib
import stat
import subprocess

from bundles import (SEMVER, at, credential_directory, credential_environment, file_bytes, plain, present,
                     require, require_flags, safe_path, validate_plan)

# Keep the project's .claude settings (Privateer's hooks and permissions) and
# drop the user source; no MCP servers unless --mcp-config names them.
FLAGS = ["--setting-sources", "project,local", "--strict-mcp-config"]
# The carried user settings stay bounded, large enough for an autoMode block
# and within the 24 KiB plan limit with room for the selected skills.
MAX_CARRIED_BYTES = 16384
CARRIED = ("model", "modelSettings", "effort", "autoMode", "skipDangerousModePermissionPrompt")
# User settings dropped without refusal: cosmetic or housekeeping keys that
# select no route or account, and the personal exposure a bounded launch drops.
DROPPED = (
    "$schema",              # editor validation hint only
    "theme",                # terminal colours
    "statusLine",           # interactive status bar; headless never shows it
    "includeCoAuthoredBy",  # commit attribution text, not authority
    "cleanupPeriodDays",    # local transcript retention
    "disableLoginPrompt",   # suppresses a prompt; never selects an account
    "enabledPlugins",       # personal plugins: what a bounded launch exists to drop
    "hooks",                # personal hooks, likewise; the project's own hooks still load
)
# Anything else refuses, notably permissions, sandbox and
# disableBypassPermissionsMode: dropping a restriction would give a bounded
# launch more freedom than an unbounded one, and env/apiKeyHelper/auth keys
# would change the route.
# --plugin-dir loads a skill root as a plugin; stage one is skills only.
PLUGIN_PARTS = ("hooks", ".mcp.json", ".lsp.json", "agents", "commands", "skills", "bin", "settings.json",
                "monitors", "output-styles", ".claude-plugin")
# Claude also loads these from --add-dir folders, which are not the project.
FOLDER_PARTS = (".claude/skills", ".claude/commands", ".claude/agents", ".mcp.json")
# Claude runs this to get a selected server's Authorization header, so the
# token is read from its credential file only when Claude connects.
HEADERS = Path(__file__).resolve().with_name("claude_mcp_headers.py")
# Flags a bounded Claude launch passes, plus --print for the headless runs
# that use it. Any installed Claude declaring these will do.
NEEDED = ["--setting-sources", "--strict-mcp-config", "--settings", "--plugin-dir", "--print"]
FORBIDDEN = ("--plugin-dir", "--plugin-url", "--setting-sources", "--mcp-config", "--bare",
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
    require(isinstance(config, dict) and all(key in CARRIED or key in DROPPED for key in config))
    carried = {key: config[key] for key in CARRIED if key in config}
    # autoMode can hold a whole environment description and its allow rules.
    require(len(json.dumps(carried)) <= MAX_CARRIED_BYTES)
    return carried


def installed_claude(binary):
    """A resolved Claude Code executable: named claude (npm, mise), or the
    official installer's claude/versions/<version>, which ~/.local/bin/claude
    links to."""
    return binary.name == "claude" or (binary.parent.name == "versions" and binary.parent.parent.name == "claude"
                                       and SEMVER.fullmatch(binary.name) is not None)


def skills_only(skills):
    for item in skills:
        require(not any(present(Path(item["root"]) / part) for part in PLUGIN_PARTS))


def plan(registry, selected, skills):
    at("claude_version", lambda: require("claude_version" in registry))
    # Pi extension integrations cannot load into Claude.
    at("integrations.extensions", lambda: require(registry["integrations"]["extensions"] == []))
    skills_only(skills)
    return {"schema_version": 1, "claude_version": registry["claude_version"], "selection": selected,
            "skills": skills, "settings": settings(os.environ)}


def preflight(value, argv, cwd=None, env=None, runtime_plugins=(), attended=False):
    """Recheck pins (skills and stdio executables), carried settings and
    folders before native execution.
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
    kept_flags = []
    incoming_settings = []
    i = 0
    while i < len(flags):
        arg = flags[i]
        name = arg.split("=", 1)[0]
        if arg == "--plugin-dir" and i + 1 < len(flags) and flags[i+1] in runtime:
            kept_flags.extend([arg, flags[i+1]])
            runtime.remove(flags[i+1])
            i += 2
        elif name == "--add-dir":
            kept_flags.append(arg)
            i += 1
            if "=" in arg:
                folders.append(arg.split("=", 1)[1])
            else:
                while i < len(flags) and not flags[i].startswith("-"):
                    folders.append(flags[i])
                    kept_flags.append(flags[i])
                    i += 1
        elif (arg == "--resume" and not attended and i + 1 < len(flags)
                and str(uuid.UUID(flags[i+1])) == flags[i+1].lower()):
            kept_flags.extend([arg, flags[i+1]])
            i += 2
        elif name == "--settings":
            if "=" in arg:
                raw = arg.split("=", 1)[1]
                i += 1
            elif i + 1 < len(flags):
                raw = flags[i+1]
                i += 2
            else:
                require(False)
            try:
                if not raw.strip().startswith("{") and len(raw) < 1024 and present(Path(raw)):
                    incoming = json.loads(file_bytes(safe_path(raw)))
                else:
                    incoming = json.loads(raw)
            except Exception:
                require(False)
            require(isinstance(incoming, dict))
            require(all(k == "autoMode" for k in incoming))
            incoming_settings.append(incoming)
        else:
            require(name not in FORBIDDEN)
            # Short clusters such as -c, -r or -pc continue/resume sessions.
            require(not (arg.startswith("-") and not arg.startswith("--") and set(arg[1:]) & set("cr")))
            kept_flags.append(arg)
            i += 1
    for folder in folders:
        # A relative folder resolves against Claude's cwd, not this process's.
        require(Path(folder).is_absolute())
        require(not any(present(Path(folder) / part) for part in FOLDER_PARTS))
    binary = shutil.which(argv[0], path=env.get("PATH"))
    require(binary is not None)
    binary = Path(binary).resolve(strict=True)
    require(binary.is_file() and installed_claude(binary))
    # Refuse activation wrappers (for example mise) rather than run them.
    with open(binary, "rb") as stream:
        require(stream.read(2) != b"#!")
    require_flags(str(binary), env, cwd or os.getcwd(), "Claude Code", NEEDED)
    merged_settings = dict(value["settings"])
    for inc in incoming_settings:
        if "autoMode" in inc:
            auto = dict(merged_settings.get("autoMode", {}))
            inc_auto = inc["autoMode"]
            require(isinstance(inc_auto, dict))
            for key in ("allow", "environment"):
                inc_list = inc_auto.get(key, [])
                require(isinstance(inc_list, list) and all(isinstance(x, str) for x in inc_list))
                if key in auto:
                    existing = list(auto[key])
                    if "$defaults" not in existing and "$defaults" in inc_list:
                        existing.insert(0, "$defaults")
                    for item in inc_list:
                        if item not in existing:
                            existing.append(item)
                    auto[key] = existing
                else:
                    auto[key] = list(inc_list)
            merged_settings["autoMode"] = auto
    if merged_settings:
        require(len(json.dumps(merged_settings)) <= MAX_CARRIED_BYTES)
    carried = ["--settings", json.dumps(merged_settings, separators=(",", ":"))] if merged_settings else []
    plugins = [arg for item in value["skills"] for arg in ("--plugin-dir", item["root"])]
    tail = args[args.index("--"):] if "--" in args else []
    return [argv[0], *FLAGS, *mcp_config(value.get("mcp_servers", []), env, value), *carried, *plugins, *kept_flags, *tail]


def config_directory(env):
    """Beside the credential store: fleet.mcp-configs, 0700."""
    return credential_directory(env).with_suffix(".mcp-configs")


def mcp_entry(server, helper, paths):
    """One server's Claude config. HTTP: its header comes from HEADERS reading
    its credential file. stdio: the pinned argv, and in env only its own
    credential's file path, never a value."""
    if "url" in server:
        return {"type": "http", "url": server["url"], "headersHelper": f"{helper} {server['credential']}"}
    env = {}
    if "credential" in server:
        name = f"PRIVATEER_CREDENTIAL_{server['credential']}_FILE"
        # Claude expands ${VAR} in a config's env values.
        at("mcp_servers.credential", lambda: require(name in paths and plain(paths[name])))
        env[name] = paths[name]
    return {"type": "stdio", "command": server["command"][0], "args": server["command"][1:], "env": env}


def mcp_config(servers, env, plan=None):
    """--mcp-config for the plan's selected servers, beside --strict-mcp-config,
    which keeps every other server out. Records only: no token in argv, env
    values or the file. The config is a 0600 file named by its content, not an
    argument, since a URL may carry a secret in its path or query and argv is
    world-readable. One `=` argument, since --mcp-config takes several values
    and would swallow the next one."""
    if not servers:
        return []
    helper = shlex.join([sys.executable, "-I", str(HEADERS)])
    # Rechecks each stored credential file; only stdio servers carry a path.
    paths = credential_environment(plan, env) if any("command" in s and "credential" in s for s in servers) else {}
    config = {"mcpServers": {server["name"]: mcp_entry(server, helper, paths) for server in servers}}
    body = json.dumps(config, separators=(",", ":"), sort_keys=True).encode()
    directory = config_directory(env)
    directory.parent.mkdir(parents=True, exist_ok=True)
    if not present(directory):
        directory.mkdir(mode=0o700)
    safe_path(str(directory), directory=True)
    require(stat.S_IMODE(directory.lstat().st_mode) == 0o700)
    path = directory / (hashlib.sha256(body).hexdigest()[:32] + ".json")
    if not (present(path) and stat.S_IMODE(path.lstat().st_mode) == 0o600
            and file_bytes(safe_path(str(path))) == body):
        partial = path.with_name(path.name + f".{os.getpid()}")
        descriptor = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(body)
        os.replace(partial, path)
    return ["--mcp-config=" + str(path)]


def check_headers(servers, env):
    """Run each selected HTTP server's header helper once, its output discarded,
    so a missing or malformed token refuses before launch: Claude itself would
    connect without the header. The token stays in the helper's process."""
    for server in (server for server in servers if "url" in server):
        done = subprocess.run([sys.executable, "-I", str(HEADERS), server["credential"]], env=env,
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL, timeout=10)
        at("mcp_servers.credential", lambda: require(done.returncode == 0))
