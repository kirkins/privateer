#!/usr/bin/env python3
"""Check installed native command parsers with --help; never request inference."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "plugins"))
from native import PROFILES, build


def check(argv):
    # Use files rather than pipes so inherited descriptors cannot hold a probe
    # open after its process exits. Do not export native diagnostics/auth text.
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout,
                                   stderr=stderr, start_new_session=True)
        try:
            code = process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            return False, "timeout"
        stdout.seek(0)
        stderr.seek(0)
        text = (stdout.read(65536) + stderr.read(65536)).decode(errors="replace")
        return code == 0 and bool(re.search(r"(?i)usage|options|flags", text)), f"exit {code}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--harness", action="append", choices=sorted(PROFILES))
    parser.add_argument("--require-all", action="store_true", help="Fail when any selected CLI is missing")
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix="privateer-harness-probe-") as directory:
        root = Path(directory).resolve()
        repo, home = root / "repo", root / "home"
        repo.mkdir()
        home.mkdir()
        request = {"context": {"repo": str(repo), "title": "Help-only parser probe",
                               "control": {"cli_argv": ["privateer", "--home", str(home)]}}}
        for name in args.harness or PROFILES:
            profile = PROFILES[name]
            installed = shutil.which(profile["binary"]) is not None
            result = {"harness": name, "installed": installed, "inference_requested": False,
                      "authentication_checked": False, "cases": []}
            if installed:
                cases = [("headless", None)]
                if name != "kimi":
                    cases.append(("interactive", None))
                if profile["resume"]:
                    cases.append(("headless", "ses_0123456789abc" if name == "opencode" else "46a81234-1234-4567-8123-123456789abc"))
                for mode, resume in cases:
                    if name == "omp" and mode == "headless":
                        passed, reason = check([profile["binary"], "--mode", "rpc", "--no-ui", "--help"])
                        result["cases"].append({"mode": "rpc-entry", "resume": False, "passed": passed, "reason": reason})
                        continue
                    if name == "muse" and resume:
                        passed, reason = check([profile["binary"], "serve", "--help"])
                        result["cases"].append({"mode": "msp-entry", "resume": False, "passed": passed, "reason": reason})
                        continue
                    if name == "rovo":
                        if resume or mode != "headless":
                            continue
                        passed, reason = check([profile["binary"], "acp", "--help"])
                        result["cases"].append({"mode": "acp-entry", "resume": False, "passed": passed, "reason": reason})
                        continue
                    options = {"mode": mode}
                    if resume:
                        options["resume"] = resume
                    _, argv = build(name, {**request, "options": options})
                    # Before '--' so it is never mistaken for prompt text.
                    argv.insert(argv.index("--") if "--" in argv else len(argv), "--help")
                    passed, reason = check(argv)
                    result["cases"].append({"mode": mode, "resume": bool(resume), "passed": passed, "reason": reason})
            result["passed"] = installed and all(case["passed"] for case in result["cases"])
            results.append(result)
    print(json.dumps({"level": "native-help-parser-only", "results": results}, indent=2))
    return int(any(not result["passed"] and (result["installed"] or args.require_all) for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
