"""Translate the captain's launch policy; never edit native/global settings."""
import os


class UnsupportedYolo(ValueError):
    public_code = "unsupported-tool-policy"
    public_message = "Fleet YOLO is unavailable for this harness transport; select a supported transport or standard mode."


def policy(value=None):
    value = value or {"mode": "standard", "claude_allow": []}
    if (not isinstance(value, dict) or value.get("mode") not in ("standard", "yolo")
            or not isinstance(value.get("claude_allow", []), list)):
        raise ValueError("invalid fleet permission policy")
    rules = value.get("claude_allow", [])
    if len(rules) > 64 or any(not isinstance(r, str) or not r or len(r) > 512 or r != r.strip() or r.startswith("-")
                              or any(ord(c) < 32 for c in r) for r in rules):
        raise ValueError("invalid fleet tool rules")
    return value


def options_for(harness, options, value):
    value = policy(value)
    options = dict(options)
    if harness == "claude":
        options.setdefault("permission_mode", "acceptEdits")
    if value["mode"] != "yolo":
        return options
    if harness == "codex":
        options.pop("sandbox", None)
        options["yolo"] = True
    elif harness in ("claude", "grok") and not (harness == "grok" and options.get("mode", "headless") == "headless"):
        options["permission_mode"] = "bypassPermissions"
    elif harness == "omp":
        options["permission_mode"] = "yolo"
    elif harness == "gemini":
        options.update(permission_mode="yolo", trust=True)
    elif harness == "cursor":
        options.update(force=True, sandbox="disabled", trust=True)
    elif harness == "agy":
        options.pop("permission_mode", None)
        options.update(skip_permissions=True, sandbox=False)
    elif harness in ("rovo", "grok") or options.get("transport") == "acp":
        options["acp_permissions"] = "allow_once"
    elif harness == "muse":
        options.update(disable_sandbox=True, trust=True)
        if options.get("transport") == "msp":
            options.update(msp_permissions="allow_once", msp_approval_mode="allowAll")
        else:
            options["permission_mode"] = "never"
    elif harness == "devin":
        options.update(permission_mode="dangerous", sandbox=False)
    elif harness == "opencode-v2":
        options["auto"] = True
    elif harness not in ("pi", "pi-signed", "opencode"):
        raise UnsupportedYolo()
    return options


def claude_rules(value, control=None):
    value = policy(value)
    cli = control["cli_argv"][0] if control else os.environ.get("PRIVATEER_CONTROL_CLI")
    rules = ([f"Bash({cli}:*)"] if cli else []) + ["Bash(git *)", "Bash(gh *)"] + value.get("claude_allow", [])
    return list(dict.fromkeys(rules))


def attended_argv(harness, argv, value):
    value = policy(value)
    flags = {
        "claude": ["--permission-mode", "bypassPermissions"],
        "codex": ["--dangerously-bypass-approvals-and-sandbox"],
        "omp": ["--approval-mode", "yolo"],
        "gemini": ["--approval-mode", "yolo", "--skip-trust"],
        "cursor": ["--force", "--sandbox", "disabled", "--trust"],
        "grok": ["--permission-mode", "bypassPermissions"],
        "agy": ["--dangerously-skip-permissions"],
        "muse": ["--yolo"],
        "devin": ["--permission-mode", "dangerous"],
        "opencode-v2": ["--auto"],
        "opencode": [], "pi": [], "pi-signed": [],
    }
    split = argv.index("--") if "--" in argv else len(argv)
    prefix, tail = argv[:split], argv[split:]
    extra = []
    if value["mode"] == "yolo":
        if harness not in flags:
            raise UnsupportedYolo()
        takes_value = {
            "claude": {"--permission-mode"},
            "codex": {"--sandbox", "-s", "--ask-for-approval", "-a"},
            "omp": {"--approval-mode"}, "gemini": {"--approval-mode"},
            "cursor": {"--sandbox"}, "grok": {"--permission-mode", "--sandbox"},
            "agy": {"--mode"}, "muse": {"--approval-mode", "--permission-profile", "--sandbox-network"},
            "devin": {"--permission-mode", "--respect-workspace-trust"},
        }.get(harness, set())
        switches = {"--dangerously-skip-permissions", "--dangerously-bypass-approvals-and-sandbox", "--full-auto", "--force", "--trust", "--skip-trust", "--yolo", "--auto"}
        if harness in ("agy", "devin"):
            switches.add("--sandbox")
        kept, skip = [], False
        for arg in prefix:
            if skip:
                skip = False
            elif arg in takes_value:
                skip = True
            elif arg.split("=", 1)[0] not in takes_value | switches:
                kept.append(arg)
        prefix, extra = kept, flags[harness]
        if harness == "opencode":
            os.environ["OPENCODE_PERMISSION"] = '{"*":"allow"}'
    if harness == "claude":
        if value["mode"] == "standard" and not any(x.split("=", 1)[0] == "--permission-mode" for x in prefix):
            extra += ["--permission-mode", "acceptEdits"]
        extra += ["--allowedTools", *claude_rules(value)]
    return [prefix[0], *extra, *prefix[1:], *tail]
