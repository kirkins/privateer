#!/usr/bin/env python3
"""Replace this process with the chosen harness, clearing foreign identity only."""
import os
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True  # a release holds only its own files
from native import PROFILES

IDENTITY_MARKERS = (
    "CLAUDECODE", "CURSOR_AGENT", "CURSOR_INVOKED_AS", "GEMINI_CLI", "GROK_AGENT",
    "PI_CODING_AGENT", "FM_PI_HARNESS", "FM_OMP_HARNESS", "ATLASSIAN_AGENT_TYPE",
    "ROVODEV_CLI", "PRIVATEER_CLAUDE_MAIL_UNTIL",
)


def native_environment():
    env = os.environ.copy()
    # subprocess cwd does not update the inherited shell variable. Some native
    # CLIs (including OpenCode V2) prefer PWD over getcwd to select a workspace.
    env["PWD"] = os.getcwd()
    for name in IDENTITY_MARKERS:
        env.pop(name, None)
    return env


def launch_environment():
    """The native environment without any inherited credential paths: a run
    never inherits a parent's, and gets only its own selected bundles'."""
    env = native_environment()
    for name in [name for name in env if name.startswith(("PRIVATEER_CREDENTIAL_", "PRIVATEER_MCP_"))]:
        del env[name]
    return env


# Settings a harness reads from its working directory that would change the
# provider, endpoint or model the route chose: a crew writes its worktree,
# so these are refused at launch rather than policed by the route's argv.
# Claude's `env` can set ANTHROPIC_BASE_URL or a Bedrock/Vertex switch, so any
# env block refuses; the project's own hooks and permissions are not read.
ROUTE_SETTINGS = {
    "claude": ((".claude/settings.json", ".claude/settings.local.json"),
               ("env", "apiKeyHelper", "model", "awsAuthRefresh", "awsCredentialExport")),
    "pi": ((".pi/settings.json",), ("defaultProvider", "defaultModel")),
    "pi-signed": ((".pi/settings.json",), ("defaultProvider", "defaultModel")),
}
MAX_SETTINGS_BYTES = 1 << 20


class RouteOverride(ValueError):
    """A working-directory settings file would change the provider route."""

    def __init__(self, harness, path, key):
        self.public_message = (f"{harness} settings in the working directory would change the provider "
                               f"route ({path}: {key}); the launch was refused.")
        super().__init__(self.public_message)


def route_overrides(harness, cwd):
    """Refuse a launch whose working directory's harness settings name a
    provider, endpoint or model. Values are never repeated: the file and key."""
    if harness not in ROUTE_SETTINGS:
        return
    files, keys = ROUTE_SETTINGS[harness]
    for name in files:
        path = Path(cwd) / name
        try:
            with path.open("rb") as stream:
                data = stream.read(MAX_SETTINGS_BYTES + 1)
        except FileNotFoundError:
            continue
        try:
            settings = json.loads(data) if len(data) <= MAX_SETTINGS_BYTES else None
        except ValueError:
            settings = None
        if not isinstance(settings, dict):
            raise RouteOverride(harness, name, "unreadable")
        for key in keys:
            if key in settings:
                raise RouteOverride(harness, name, key)


def launch(harness, argv, control=None, bundles=None, attended=False):
    if harness not in PROFILES or not argv or argv[0] != PROFILES[harness]["binary"]:
        raise ValueError("invalid native launch")
    route_overrides(harness, os.getcwd())
    env = launch_environment()
    if bundles is not None:
        # A session that selected a bundle may run Python from a pinned skill
        # root: the harness itself, or an agent tool importing a skill script.
        # Importing skill code must never write a __pycache__ into the pinned
        # tree: skills.sha256 has no ignore list. The runtime's own plugins
        # already run with bytecode writing off; every bounded session's
        # launch environment carries the same switch, probes included.
        env["PYTHONDONTWRITEBYTECODE"] = "1"
    secrets = {}
    if bundles is not None:
        from bundles import credential_environment, stdio_role
        if attended:
            # An attended launch is a first mate's pane, never a job: its stop
            # signals the harness pid alone, so a stdio server could outlive it.
            stdio_role(bundles.get("mcp_servers", []), "first_mate")
        # Check every selected ref's stored file before any native probe.
        # Paths only, never values: the plan and argv carry names and refs.
        secrets = credential_environment(bundles, env)
        if bundles.get("mcp_servers"):
            # Records only (names, URLs or pinned commands, credential names,
            # limits): a token is read from its file inside the harness, and a
            # stdio server gets only its file's path, never a value.
            # Pi's gate and the headless Claude check both read them.
            env["PRIVATEER_MCP_SERVERS"] = json.dumps(bundles["mcp_servers"], separators=(",", ":"), sort_keys=True)
            if harness == "pi" and not attended and any("command" in server for server in bundles["mcp_servers"]):
                # Pi's gate runs each stdio server through Privateer's launcher
                # with this interpreter, by absolute path: never a PATH lookup.
                # A job's launch only: without it the gate refuses every stdio record.
                from bundles import interpreter
                env["PRIVATEER_MCP_PYTHON"] = interpreter()
    if bundles is not None and harness == "claude":
        from claude_bundles import check_headers, preflight
        check_headers(bundles.get("mcp_servers", []), {**env, **secrets})
        # Privateer's attended mail adapter is the one runtime-owned plugin.
        plugins = [Path(__file__).resolve().parent / "attended-claude"] if attended else []
        argv = preflight(bundles, argv, env=env, runtime_plugins=plugins, attended=attended)
    elif bundles is not None:
        if harness != "pi":
            raise ValueError("bounded resources support only Pi and Claude")
        from pi_bundles import preflight
        directory = Path(__file__).resolve().parent
        extensions = [directory / "attended/mail.ts"] if attended else []
        crew = str(directory / "attended/crew.ts")
        # #96 injects the lifecycle adapter after plugin planning for hosted
        # crew. Recognize that exact runtime-owned seam, never an inherited
        # first-mate selection or arbitrary -e override. Stale hosted markers
        # alone cannot add the extension to a nested/headless run.
        flags = argv[:argv.index("--")] if "--" in argv else argv
        hosted_crew = os.environ.get("PRIVATEER_HOSTED_CREW") == "1" and any(
            flags[i:i+2] == ["-e", crew] for i in range(len(flags)))
        if hosted_crew:
            extensions.append(crew)
        captain = str(directory / "attended/captain.ts")
        # The captain's slash commands ride with attended mail only, as the
        # exact runtime injection; a headless or crew run never carries them.
        if attended and any(flags[i:i+2] == ["-e", captain] for i in range(len(flags))):
            extensions.append(captain)
        calm = str(directory / "attended/calm.ts")
        # Calm is a runtime-owned presentation adapter, not an operator
        # integration. Only the exact attended/hosted injection may survive
        # bounded preflight; headless runs and stale hosted markers cannot
        # authorize an arbitrary extension (or add Calm by inheritance).
        if (attended or hosted_crew) and any(
                flags[i:i+2] == ["-e", calm] for i in range(len(flags))):
            extensions.append(calm)
        argv = preflight(bundles, argv, env=env, runtime_extensions=extensions)
    env.update(secrets)
    if harness == "opencode-v2":
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("opencode_v2.py")),
                                  *argv], env)
    # Pi JSON mode may exit zero after an assistant error. Its event stream,
    # not the process status alone, must confirm a completed unattended run.
    flags = argv[:argv.index("--")] if "--" in argv else argv
    if harness == "claude" and "--print" in flags and "stream-json" in flags:
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("claude_turn.py")),
                                  "--control", json.dumps(control), *argv], env)
    if harness in ("pi", "pi-signed") and "--print" in flags and any(
            flags[i:i+2] == ["--mode", "json"] for i in range(len(flags))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], env)
    if (harness in ("agy", "cursor", "gemini")
            and ("--prompt" if harness == "gemini" else "--print") in flags and any(
            flags[i:i+2] == ["--output-format", "stream-json"] for i in range(len(flags)))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], env)
    # OpenCode V1 waits silently after a provider error instead of failing the
    # run, so a headless turn runs under a watch that ends it.
    if harness == "opencode" and argv[1:2] == ["run"] and any(
            flags[i:i+2] == ["--format", "json"] for i in range(len(flags))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("opencode_watch.py")),
                                  *argv], env)
    # Keep PRIVATEER_ROLE/ACTOR and all native auth/provider configuration.
    # exec preserves the job's process identity, group and cancellation path.
    os.execvpe(argv[0], argv, env)


def main():
    try:
        args = sys.argv[1:]
        bundles = None
        if args[0] == "--tool-bundles":
            bundles = json.loads(args[1])
            args = args[2:]
        if args[0] == "--attended-policy":
            from tool_permissions import attended_argv
            value, harness = json.loads(args[1]), args[2]
            if bundles is None and harness not in PROFILES and value.get("mode") == "standard":
                # Preserve the human CLI's existing custom-harness escape hatch.
                os.execvpe(args[3], args[3:], launch_environment())
            launch(harness, attended_argv(harness, args[3:], value), bundles=bundles, attended=True)
        else:
            if args[0] == "--opencode-yolo":
                if args[1] != "opencode":
                    raise ValueError("invalid permission environment")
                os.environ["OPENCODE_PERMISSION"] = '{"*":"allow"}'
                args = args[1:]
            control = None
            if args[0] == "--control":
                control = json.loads(args[1])
                args = args[2:]
            launch(args[0], args[1:], control, bundles=bundles)
    except Exception as error:
        from bundles import StdioRole
        from tool_permissions import UnsupportedYolo
        if isinstance(error, UnsupportedYolo):
            print(UnsupportedYolo.public_message, file=sys.stderr)
            return 2
        if isinstance(error, (StdioRole, RouteOverride)):
            print(error.public_message, file=sys.stderr)
            return 2
        print("Native harness launch failed; check the selected CLI installation locally.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
