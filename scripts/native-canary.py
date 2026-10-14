#!/usr/bin/env python3
"""Opt-in real Codex plugin canary. Uses existing authentication, isolated work only."""
import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--manifest", required=True, help="Native Codex plugin manifest")
    parser.add_argument("--model", help="Optional existing native model choice")
    parser.add_argument("--timeout-secs", type=int, default=240)
    args = parser.parse_args()
    if not 30 <= args.timeout_secs <= 900:
        parser.error("timeout must be 30..900 seconds")
    binary = str(pathlib.Path(args.binary).resolve(strict=True))
    manifest = str(pathlib.Path(args.manifest).resolve(strict=True))
    root = pathlib.Path(tempfile.mkdtemp(prefix="privateer-native-canary-")).resolve()
    home, repo = root / "home", root / "repo"
    repo.mkdir()
    evidence = {"directory": str(root), "native_inference": True, "harness": "codex", "live_fleet_touched": False}

    def call(command, payload=None):
        result = subprocess.run([binary, "--home", str(home), command, "--json", json.dumps(payload or {})],
                                capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr}")
        return json.loads(result.stdout)

    try:
        (repo / "calc.py").write_text("def add(a, b):\n    return a - b\n")
        (repo / "test_calc.py").write_text("import unittest\nfrom calc import add\nclass TestAddition(unittest.TestCase):\n    def test_positive_and_negative(self):\n        self.assertEqual(add(3, 2), 5)\n        self.assertEqual(add(-3, 2), -1)\n")
        (repo / ".gitignore").write_text("__pycache__/\n")
        for command in (["git", "init", "-q", str(repo)],
                        ["git", "-C", str(repo), "add", "."],
                        ["git", "-C", str(repo), "-c", "user.name=Privateer Canary", "-c", "user.email=canary@example.invalid", "commit", "-qm", "Seed regression"]):
            subprocess.run(command, check=True, capture_output=True)
        assert subprocess.run([sys.executable, "-m", "unittest", "-q"], cwd=repo, capture_output=True).returncode != 0
        call("init", {"resource_home": str(root / "ledger")})
        call("charter.put", {"id": "canary", "objective": "Repair and report an isolated arithmetic regression",
                             "repo": str(repo), "min_free_bytes": 1024, "job_timeout_secs": args.timeout_secs,
                             "provider_plugin": {"manifest": manifest}})
        title = ("Fix add() in calc.py to pass test_calc.py. Run python3 -m unittest -q. "
                 "Send a Privateer mail notice to reviewer using mail.send, "
                 "id=native-result, from=applications, to=reviewer, kind=notice, task_id=repair. "
                 "Do not mark task done; caller verifies independently. Work only in this repository "
                 "and Privateer home. Do not install packages or change global settings.")
        for ship in ("applications", "reviewer"):
            call("ship.put", {"name": ship, "scope": "native canary"})
        call("task.create", {"id": "repair", "charter_id": "canary", "owner": "applications", "title": title})
        options = {"sandbox": "workspace-write", "ephemeral": True}
        if args.model:
            options["model"] = args.model
        call("agent.start", {"id": "native-repair", "task_id": "repair", "options": options})
        deadline = time.monotonic() + args.timeout_secs + 15
        while time.monotonic() < deadline:
            job = call("job.status", {"id": "native-repair"})
            if job["status"] in ("succeeded", "failed", "cancelled", "timed_out", "orphaned"):
                break
            time.sleep(0.5)
        assert job["status"] == "succeeded" and job["exit_code"] == 0, {"status": job["status"], "exit_code": job["exit_code"], "reason": job.get("reason")}
        result = subprocess.run([sys.executable, "-m", "unittest", "-q"], cwd=repo, capture_output=True, text=True)
        assert result.returncode == 0, "Independent tests did not pass"
        inbox = call("mail.inbox", {"address": "reviewer"})
        assert any(m["id"] == "native-result" for m in inbox["messages"]), "Native agent did not record its task-linked result"
        call("task.update", {"id": "repair", "actor": "applications", "status": "done"})
        evidence.update({"passed": True, "native_plugin_job": job["status"], "independent_tests_passed": True,
                         "native_peer_message_recorded": True, "configured_model": args.model or "native default"})
    except Exception as error:
        evidence.update({"passed": False, "error": str(error)})
        try:
            call("job.cancel", {"id": "native-repair"})
        except Exception:
            pass
    (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0 if evidence.get("passed") else 1


if __name__ == "__main__":
    sys.exit(main())
