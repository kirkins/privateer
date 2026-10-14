#!/usr/bin/env python3
"""Opt-in native harness canary. Existing authentication, isolated work only."""
import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import time
import secrets

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "plugins"))
from native_events import read_session


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--manifest", required=True, help="Selected harness plugin manifest")
    parser.add_argument("--config", help="Existing private adapter config; never copied or modified")
    parser.add_argument("--harness", help="Optional expected manifest id")
    parser.add_argument("--options-json", help="Native adapter options (never credentials)")
    parser.add_argument("--check-resume", action="store_true", help="Verify exact session continuation and conversation recall")
    parser.add_argument("--model", help="Optional existing native model choice")
    parser.add_argument("--timeout-secs", type=int, default=240)
    args = parser.parse_args()
    if not 30 <= args.timeout_secs <= 900:
        parser.error("timeout must be 30..900 seconds")
    binary = str(pathlib.Path(args.binary).resolve(strict=True))
    manifest = str(pathlib.Path(args.manifest).resolve(strict=True))
    harness = json.loads(pathlib.Path(manifest).read_text())["id"]
    if args.harness and args.harness != harness:
        parser.error("--harness must match the selected manifest id")
    root = pathlib.Path(tempfile.mkdtemp(prefix="privateer-native-canary-")).resolve()
    home, repo = root / "home", root / "repo"
    repo.mkdir()
    evidence = {"directory": str(root), "native_inference_requested": True, "harness": harness, "live_fleet_touched": False}
    started = []

    def call(command, payload=None):
        result = subprocess.run([binary, "--home", str(home), command, "--json", json.dumps(payload or {})],
                                capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr}")
        return json.loads(result.stdout)

    def run_job(task, job_id, options):
        started.append(job_id)
        call("agent.start", {"id": job_id, "task_id": task, "options": options})
        deadline = time.monotonic() + args.timeout_secs + 15
        while time.monotonic() < deadline:
            job = call("job.status", {"id": job_id})
            if job["status"] in ("succeeded", "failed", "cancelled", "timed_out", "orphaned"):
                break
            time.sleep(0.5)
        assert job["status"] == "succeeded" and job["exit_code"] == 0, {"status": job["status"], "exit_code": job["exit_code"], "reason": job.get("reason")}
        return job

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
        binding = {"manifest": manifest}
        if args.config:
            config_path = pathlib.Path(args.config).expanduser()
            if config_path.is_symlink():
                raise ValueError("private adapter config cannot be a symlink")
            binding["config"] = str(config_path.resolve(strict=True))
        call("charter.put", {"id": "canary", "objective": "Repair and report an isolated arithmetic regression",
                             "repo": str(repo), "min_free_bytes": 1024, "job_timeout_secs": args.timeout_secs,
                             "provider_plugin": binding})
        title = ("Fix add() in calc.py to pass test_calc.py. Run python3 -m unittest -q. "
                 "Send a Privateer mail notice to reviewer using mail.send, "
                 "id=native-result, from=applications, to=reviewer, kind=notice, task_id=repair. "
                 "Do not mark task done; caller verifies independently. Work only in this repository "
                 "and Privateer home. Do not install packages or change global settings.")
        recall = secrets.token_hex(8)
        if args.check_resume:
            title = f"Remember token {recall} for our next turn; do not write it to a file. " + title
        for ship in ("applications", "reviewer"):
            call("ship.put", {"name": ship, "scope": "native canary"})
        call("task.create", {"id": "repair", "charter_id": "canary", "owner": "applications", "title": title})
        options = {"sandbox": "workspace-write", "ephemeral": not args.check_resume} if harness == "codex" else {}
        if args.options_json:
            supplied = json.loads(args.options_json)
            if not isinstance(supplied, dict):
                raise ValueError("options must be an object")
            options.update(supplied)
        if args.model:
            options["model"] = args.model
        if args.check_resume and options.get("ephemeral"):
            raise ValueError("resume qualification needs native session persistence")
        job = run_job("repair", "native-repair", options)
        result = subprocess.run([sys.executable, "-m", "unittest", "-q"], cwd=repo, capture_output=True, text=True)
        assert result.returncode == 0, "Independent tests did not pass"
        inbox = call("mail.inbox", {"address": "reviewer"})
        assert any(m["id"] == "native-result" for m in inbox["messages"]), "Native agent did not record its task-linked result"
        log = home / "jobs" / job["id"] / job["incarnation"] / "stdout.log"
        session_id = read_session(harness, log)
        evidence["native_session_id_observed"] = session_id is not None
        if args.check_resume:
            if not session_id:
                raise RuntimeError("no qualified native session-start evidence for this harness")
            call("task.create", {"id": "continuation", "charter_id": "canary", "owner": "applications",
                                 "title": "Write recall.txt containing only the token I gave you in our previous native conversation. Use conversation memory; do not search Privateer's records or earlier task data for it. Do not modify calc.py or configuration."})
            second = run_job("continuation", "native-resume", {**options, "resume": session_id})
            assert (repo / "recall.txt").read_text().strip() == recall, "Native conversation recall failed"
            second_log = home / "jobs" / second["id"] / second["incarnation"] / "stdout.log"
            assert read_session(harness, second_log) == session_id, "Native session identity changed"
            evidence.update({"exact_native_session_resumed": True, "native_conversation_recall_verified": True,
                             "resumed_job": second["status"]})
        call("task.update", {"id": "repair", "actor": "applications", "status": "done"})
        evidence.update({"passed": True, "native_plugin_job": job["status"], "independent_tests_passed": True,
                         "native_peer_message_recorded": True, "configured_model": args.model or "native default"})
    except Exception as error:
        evidence.update({"passed": False, "error": str(error)})
        for job_id in started:
            try:
                call("job.cancel", {"id": job_id})
            except Exception:
                pass
    (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0 if evidence.get("passed") else 1


if __name__ == "__main__":
    sys.exit(main())
