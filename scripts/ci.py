#!/usr/bin/env python3
"""CI routing and offline documentation/research checks; no Cargo or model calls."""
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys


def lightweight(name):
    """Allow only documentation and offline research inputs; default to full CI."""
    path = PurePosixPath(name)
    return (
        path.suffix == ".md" and (len(path.parts) == 1 or name.startswith("docs/"))
        or name.startswith("docs/research/") and path.suffix == ".json"
        or name.startswith("scripts/research/") and path.suffix == ".py"
    )


def changed_paths(base, head):
    # No rename detection: moving runtime code into docs still reports its
    # deletion. NUL delimiters preserve spaces, newlines and shell metacharacters.
    out = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", f"{base}...{head}", "--"],
        check=True, capture_output=True,
    ).stdout
    return [os.fsdecode(name) for name in out.split(b"\0") if name]


def runtime_required(event_name, event):
    # Main pushes still qualify every commit and produce exact-commit releases;
    # manual dispatch always provides a way to request the full suite.
    if event_name != "pull_request":
        return True
    pr = event["pull_request"]
    paths = changed_paths(pr["base"]["sha"], pr["head"]["sha"])
    return not paths or any(not lightweight(name) for name in paths)


def check_docs():
    # Check tracked files only, never a local venv or downloaded upstream tree.
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", "docs", "scripts/research"],
        check=True, capture_output=True,
    ).stdout
    checked = 0
    for name in filter(None, out.split(b"\0")):
        path = Path(os.fsdecode(name))
        try:
            if path.suffix == ".json":
                json.loads(path.read_text(encoding="utf-8"))
            elif path.suffix == ".py" and path.parts[:2] == ("scripts", "research"):
                # Compile bytes without importing dependencies, executing research,
                # downloading tokenizer data or leaving __pycache__ in the checkout.
                compile(path.read_bytes(), str(path), "exec")
            else:
                continue
        except (OSError, ValueError, SyntaxError) as error:
            raise SystemExit(f"{path}: {error}") from error
        print(f"checked {path}")
        checked += 1
    print(f"Checked {checked} JSON/Python files; research scripts were not executed.")


def main(argv):
    if argv == ["check-docs"]:
        check_docs()
    elif argv == ["plan"]:
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
        required = runtime_required(os.environ["GITHUB_EVENT_NAME"], event)
        result = f"runtime={str(required).lower()}\n"
        print(result, end="")
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(result)
    else:
        raise SystemExit("usage: ci.py plan|check-docs")


if __name__ == "__main__":
    main(sys.argv[1:])
