"""Translate the captain's launch policy; never edit native/global settings."""
import os
import sys


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
    authorized = value.get("authorized", {})
    if not isinstance(authorized, dict) or any(
            not isinstance(statements, list) or any(not isinstance(t, str) for t in statements)
            for statements in authorized.values()):
        raise ValueError("invalid fleet authorizations")
    approved = value.get("approved", [])
    if not isinstance(approved, list):
        raise ValueError("invalid captain approvals")
    # One bad sentence must never stop a launch: drop it and say so.
    kept = [t for t in approved if isinstance(t, str) and t and len(t) <= 600][:8]
    if len(kept) != len(approved):
        print(f"privateer: dropped {len(approved) - len(kept)} invalid captain approval(s)", file=sys.stderr)
        value = {**value, "approved": kept}
    if "forges" in value:
        forges = value["forges"]
        if not isinstance(forges, dict):
            raise ValueError("invalid fleet forges")
        for forge_name, actions in forges.items():
            if not isinstance(forge_name, str) or not isinstance(actions, dict):
                raise ValueError("invalid fleet forges")
            for action, rule in actions.items():
                if not isinstance(action, str) or not isinstance(rule, str):
                    raise ValueError("invalid fleet forges")
    return value


def options_for(harness, options, value, role=None):
    value = policy(value)
    options = dict(options)
    if role == "reviewer":
        # A review reads code and writes nothing, whatever the fleet's mode.
        if harness == "claude":
            options["permission_mode"] = "dontAsk"
            options["chrome"] = False
        return options
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


# Claude tools whose work would outlive the job or escape the fleet's records.
CLAUDE_DETACHED = ["CronCreate", "ScheduleWakeup", "RemoteTrigger"]
# A first mate delegates through crew jobs, not Claude's own agents or worktrees.
CLAUDE_DELEGATION = ["Agent", "Task", "Workflow", "EnterWorktree"]
# A reviewer reads the checked-out head: no shell, no writes, no network.
CLAUDE_REVIEWER_ALLOWED = ["Read", "Grep", "Glob"]
CLAUDE_REVIEWER_DISALLOWED = ["Bash", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch"]

BUILTIN_FORGE_RULES = {
    "github": {
        "publish_pr": "Bash(gh pr create *)",
        "merge_green": "Bash(gh pr merge *)",
    },
}

FORGE_ALIASES = {
    "gh": "github",
    "github": "github",
}


def charter_rules(actions, forge=None, role=None, errand=False, policy_value=None):
    if role in ("crew", "reviewer") or errand or not actions:
        return []
    if isinstance(forge, list):
        forges = forge
    elif forge:
        forges = [forge]
    else:
        forges = ["github"]
    custom_forges = (policy_value or {}).get("forges", {}) if isinstance(policy_value, dict) else {}
    rules = []
    for f in forges:
        canonical = FORGE_ALIASES.get(f, f)
        mapping = BUILTIN_FORGE_RULES.get(canonical) or custom_forges.get(canonical) or {}
        for action in actions:
            if action in mapping:
                rule = mapping[action]
                if not (rule.startswith("Bash(") and rule.endswith(")")):
                    rule = f"Bash({rule})"
                rules.append(rule)
    return list(dict.fromkeys(rules))


def claude_auto_mode_settings(actions, forge=None, role=None, errand=False, ship=None, authorized=(),
                              approved=()):
    """Tell Claude's auto-mode classifier what the captain already authorized.

    The classifier trusts settings, not what an agent reads in mail or notes,
    so a captain's standing grant must reach it here or it is asked again.
    The captain's expiring approvals (`approved`) widen allow only.

    Claude reads these only at launch: a running session never re-reads its
    --settings file, in place or renamed (tests/fixtures/claude/settings-reload.md),
    so they stay inline and an approval recorded mid-session needs a relaunch."""
    if role in ("crew", "reviewer") or errand or not (actions or authorized or approved):
        return None
    allow = ["$defaults"]
    if "merge_green" in (actions or []):
        allow.append("Merging this ship's own pull requests once CI is green, when Opus 5.5 wrote the change "
                     "or approved its current head, is authorized by the ship's charter (merge_green) and the "
                     "captain's merge rule, even without a human review.")
    if "publish_pr" in (actions or []):
        allow.append("Creating and publishing pull requests is authorized by the ship's charter (publish_pr).")
    allow += [f"The captain has authorized this: {text}" for text in authorized]
    allow += list(approved)
    env = ["$defaults"]
    if actions:
        env.append(f"Charter actions granted: {', '.join(sorted(actions))}.")
    if ship:
        env.append(f"Ship {ship} operates under its active charter.")
    return {
        "autoMode": {
            "allow": allow,
            "environment": env,
        }
    }


def authorized_for(value, ship):
    """The captain's standing authorizations for one ship."""
    return list(policy(value).get("authorized", {}).get(ship or "", []))


def merge_auto_mode_settings(target_settings, incoming_settings):
    if not incoming_settings or "autoMode" not in incoming_settings:
        return target_settings
    res = dict(target_settings)
    auto = dict(res.get("autoMode", {}))
    incoming_auto = incoming_settings["autoMode"]
    if not isinstance(incoming_auto, dict):
        return target_settings
    for key in ("allow", "environment"):
        incoming_list = incoming_auto.get(key, [])
        if not isinstance(incoming_list, list):
            continue
        if key in auto:
            existing = list(auto[key])
            if "$defaults" not in existing and "$defaults" in incoming_list:
                existing.insert(0, "$defaults")
            for item in incoming_list:
                if item not in existing:
                    existing.append(item)
            auto[key] = existing
        else:
            auto[key] = list(incoming_list)
    res["autoMode"] = auto
    return res


def claude_disallowed(role):
    if role == "reviewer":
        return CLAUDE_REVIEWER_DISALLOWED + CLAUDE_DETACHED + CLAUDE_DELEGATION
    return CLAUDE_DETACHED + (CLAUDE_DELEGATION if role == "first_mate" else [])


def claude_rules(value, control=None, actions=None, forge=None, role=None, errand=False):
    pol = policy(value)
    if actions is None and isinstance(value, dict):
        actions = value.get("actions")
    if forge is None and isinstance(value, dict):
        forge = value.get("forge") or value.get("forges")
    if role is None and isinstance(value, dict):
        role = value.get("role")
    if not errand and isinstance(value, dict):
        errand = value.get("errand", False)
    if role == "reviewer":
        return list(CLAUDE_REVIEWER_ALLOWED)
    cli = control["cli_argv"][0] if control else os.environ.get("PRIVATEER_CONTROL_CLI")
    extra_charter = charter_rules(actions, forge=forge, role=role, errand=errand, policy_value=pol)
    rules = ([f"Bash({cli}:*)"] if cli else []) + ["Bash(git *)", "Bash(gh *)"] + extra_charter + pol.get("claude_allow", [])
    return list(dict.fromkeys(rules))


def attended_argv(harness, argv, value):
    import json
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
        actions = value.get("actions")
        forge = value.get("forge") or value.get("forges")
        ship = value.get("ship")
        extra += ["--allowedTools", *claude_rules(value, actions=actions, forge=forge, role="first_mate")]
        # An attended Claude session is the ship's first mate.
        extra += ["--disallowedTools", *claude_disallowed("first_mate")]
        auto_settings = claude_auto_mode_settings(actions, forge=forge, role="first_mate", ship=ship,
                                                  authorized=authorized_for(value, ship),
                                                  approved=value.get("approved", []))
        if auto_settings:
            settings_idx = None
            for idx, arg in enumerate(prefix):
                if arg == "--settings" or arg.startswith("--settings="):
                    settings_idx = idx
                    break
            if settings_idx is not None:
                arg = prefix[settings_idx]
                if "=" in arg:
                    raw = arg.split("=", 1)[1]
                    existing = json.loads(raw)
                    merged = merge_auto_mode_settings(existing, auto_settings)
                    prefix[settings_idx] = f"--settings={json.dumps(merged)}"
                elif settings_idx + 1 < len(prefix):
                    raw = prefix[settings_idx + 1]
                    existing = json.loads(raw)
                    merged = merge_auto_mode_settings(existing, auto_settings)
                    prefix[settings_idx + 1] = json.dumps(merged)
            else:
                extra += ["--settings", json.dumps(auto_settings)]
    return [prefix[0], *extra, *prefix[1:], *tail]
