#!/usr/bin/env python3
"""Replace this process with the chosen harness, clearing foreign identity only."""
import os
import json
import sys
from pathlib import Path

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


def launch(harness, argv, control=None, bundles=None, attended=False):
    if harness not in PROFILES or not argv or argv[0] != PROFILES[harness]["binary"]:
        raise ValueError("invalid native launch")
    if bundles is not None:
        if harness != "pi":
            raise ValueError("bounded resources support only Pi")
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
        argv = preflight(bundles, argv, env=native_environment(), runtime_extensions=extensions)
    if harness == "opencode-v2":
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("opencode_v2.py")),
                                  *argv], native_environment())
    # Pi JSON mode may exit zero after an assistant error. Its event stream,
    # not the process status alone, must confirm a completed unattended run.
    flags = argv[:argv.index("--")] if "--" in argv else argv
    if harness == "claude" and "--print" in flags and "stream-json" in flags:
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("claude_turn.py")),
                                  "--control", json.dumps(control), *argv], native_environment())
    if harness in ("pi", "pi-signed") and "--print" in flags and any(
            flags[i:i+2] == ["--mode", "json"] for i in range(len(flags))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], native_environment())
    if (harness in ("agy", "cursor", "gemini")
            and ("--prompt" if harness == "gemini" else "--print") in flags and any(
            flags[i:i+2] == ["--output-format", "stream-json"] for i in range(len(flags)))):
        os.execve(sys.executable, [sys.executable, str(Path(__file__).with_name("native_turn.py")),
                                  harness, *argv], native_environment())
    # Keep PRIVATEER_ROLE/ACTOR and all native auth/provider configuration.
    # exec preserves the job's process identity, group and cancellation path.
    os.execvpe(argv[0], argv, native_environment())


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
                os.execvpe(args[3], args[3:], native_environment())
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
        from tool_permissions import UnsupportedYolo
        if isinstance(error, UnsupportedYolo):
            print(UnsupportedYolo.public_message, file=sys.stderr)
            return 2
        print("Native harness launch failed; check the selected CLI installation locally.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
