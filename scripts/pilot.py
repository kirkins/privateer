#!/usr/bin/env python3
"""Exercise the real runtime in retained temporary homes; no native inference."""
import argparse
import json
import pathlib
import subprocess
import sys
import tempfile
import time

from recovery_fixture import configure_workspaces


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    options = parser.parse_args()
    binary = pathlib.Path(options.binary).resolve(strict=True)
    root = pathlib.Path(tempfile.mkdtemp(prefix="privateer-pilot-"))
    repo = root / "fixture"
    repo.mkdir()
    homes = {name: root / name for name in ("navigator", "applications", "infrastructure")}
    ledger = root / "host-resources"
    python = sys.executable
    evidence = {"directory": str(root), "binary": str(binary), "native_inference": False}

    def invoke(home, command, arguments=None, expect=True):
        result = subprocess.run(
            [str(binary), "--home", str(home), command, "--json", json.dumps(arguments or {})],
            text=True, capture_output=True, timeout=20,
        )
        if expect:
            if result.returncode:
                raise RuntimeError(f"{command} failed: {result.stderr or result.stdout}")
            return json.loads(result.stdout)
        return result

    def remote(home, peer, command, arguments=None):
        envelope = invoke(home, "peer.call", {"name": peer, "command": command, "args": arguments or {}})
        if not envelope["ok"]:
            raise RuntimeError(envelope["error"])
        return envelope["result"]

    def rows(value, *keys):
        if isinstance(value, list):
            return value
        for key in keys:
            if isinstance(value.get(key), list):
                return value[key]
        raise RuntimeError(f"Unexpected collection: {value}")

    def git(*arguments):
        subprocess.run(["git", "-C", str(repo), *arguments], check=True, capture_output=True)

    try:
        (repo / "calc.py").write_text("def add(left, right):\n    return left - right\n")
        (repo / "test_calc.py").write_text(
            "import unittest\nfrom calc import add\n"
            "class AdditionRegression(unittest.TestCase):\n"
            "    def test_addition_handles_positive_and_negative_values(self):\n"
            "        self.assertEqual(add(3, 2), 5)\n"
            "        self.assertEqual(add(-3, 2), -1)\n"
            "        self.assertEqual(add(0, 0), 0)\n"
        )
        (repo / ".gitignore").write_text("__pycache__/\n")
        git("init", "-q")
        git("config", "user.name", "Privateer Pilot")
        git("config", "user.email", "pilot@example.invalid")
        git("add", ".")
        git("commit", "-qm", "Seed failing addition fixture")
        baseline = subprocess.run([python, "-m", "unittest", "-q"], cwd=repo, capture_output=True)
        assert baseline.returncode != 0, "Fixture must reproduce a real failure"

        for name, home in homes.items():
            invoke(home, "init", {"resource_home": str(ledger)})
            invoke(home, "charter.put", {
                "id": "fixture", "objective": "Repair and validate the isolated fixture",
                "repo": str(repo), "max_heavy_jobs": 1, "min_free_bytes": 1024,
                "job_timeout_secs": 20,
            })
        configure_workspaces(root, homes.values())
        evidence["workspace_backend"] = "test protocol fixture backed by real Git worktrees"
        # One shared heavy slot for the whole host makes the cross-home refusal
        # deterministic; live load, memory and disk limits are left generous
        # here. These scripted payloads write one small file, so they do not
        # claim the host's heavy-job disk appetite.
        invoke(homes["navigator"], "resource.configure", {
            "max_heavy_jobs": 1, "max_load_per_cpu": 64, "min_available_memory_bytes": 0,
            "heavy_estimate": {"memory_bytes": 0, "disk_bytes": 0},
        })
        for source, targets in (
            ("navigator", ("applications", "infrastructure")),
            ("applications", ("infrastructure",)),
            ("infrastructure", ("applications",)),
        ):
            for target in targets:
                invoke(homes[source], "peer.put", {
                    "name": target, "transport": "local", "executable": str(binary),
                    "home": str(homes[target]),
                })

        invoke(homes["navigator"], "task.create", {
            "id": "outcome", "charter_id": "fixture", "owner": "navigator",
            "title": "Deliver a validated fixture repair",
        })
        remote(homes["navigator"], "applications", "task.create", {
            "id": "repair", "charter_id": "fixture", "owner": "applications",
            "title": "Repair the arithmetic regression",
        })
        remote(homes["navigator"], "infrastructure", "task.create", {
            "id": "diagnosis", "charter_id": "fixture", "owner": "infrastructure",
            "title": "Verify the diagnostic lane remains responsive",
        })

        remote(homes["applications"], "infrastructure", "message.send", {
            "id": "direct-request", "task_id": "diagnosis", "from": "applications",
            "to": "infrastructure", "body": "Please check capacity while I repair the fixture.",
        })
        inbox = invoke(homes["infrastructure"], "message.inbox", {"actor": "infrastructure"})
        assert len(rows(inbox, "messages", "events", "items")) == 1
        remote(homes["infrastructure"], "applications", "message.send", {
            "id": "direct-reply", "task_id": "repair", "from": "infrastructure",
            "to": "applications", "body": "Diagnostic lane is responsive; heavy work must share one slot.",
        })
        reply = invoke(homes["applications"], "message.inbox", {"actor": "applications"})
        assert len(rows(reply, "messages", "events", "items")) == 1
        evidence["direct_peer_messages"] = True

        for sequence in range(50):
            invoke(homes["navigator"], "event.emit", {
                "id": f"alert-{sequence}", "task_id": "outcome", "issue_key": "same-failure",
                "kind": "alert", "payload": {"attempt": sequence},
            })
        pending = rows(invoke(homes["navigator"], "event.pending", {"task_id": "outcome"}), "pending", "events", "items")
        history = rows(invoke(homes["navigator"], "event.history", {"task_id": "outcome", "limit": 100}), "history", "events", "items")
        assert len(pending) == 1 and len(history) == 50, (pending, history)
        evidence["notification_storm"] = {"facts": 50, "pending": 1}

        worker_code = (
            "import pathlib,subprocess,time; time.sleep(1.5); "
            "pathlib.Path('calc.py').write_text('def add(left, right):\\n    return left + right\\n'); "
            "subprocess.run([" + repr(python) + ",'-m','unittest','-q'],check=True); "
            "subprocess.run(['git','add','calc.py'],check=True); "
            "subprocess.run(['git','commit','-qm','Repair validated addition regression'],check=True)"
        )
        job = invoke(homes["applications"], "job.start", {
            "id": "repair-job", "task_id": "repair", "command": [python, "-c", worker_code],
            "cwd": str(repo), "heavy": True, "estimate": {"cpus": 1, "memory_bytes": 64 * 1024 * 1024},
        })
        assert job["id"] == "repair-job"
        refusal = invoke(homes["infrastructure"], "job.start", {
            "id": "competing-job", "task_id": "diagnosis", "command": [python, "-c", "print('diagnosis')"],
            "cwd": str(repo), "heavy": True, "estimate": {"cpus": 1, "memory_bytes": 64 * 1024 * 1024},
        }, expect=False)
        assert refusal.returncode != 0, "Second heavy job must be refused across homes"
        evidence["shared_capacity_enforced"] = True

        # A separate navigator RPC session is killed while the department's
        # detached supervisor continues. The job.start launcher already exited.
        session = subprocess.Popen(
            [str(binary), "--home", str(homes["navigator"]), "rpc"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        session.stdin.write(json.dumps({"id": "navigator-session", "command": "status", "args": {}}) + "\n")
        session.stdin.flush()
        assert json.loads(session.stdout.readline())["ok"]
        session.terminate()
        session.wait(timeout=5)
        evidence["navigator_session_stopped"] = True

        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            state = invoke(homes["applications"], "job.status", {"id": "repair-job"})
            if state["status"] in ("succeeded", "failed", "cancelled", "timed_out"):
                break
            time.sleep(0.05)
        assert state["status"] == "succeeded", state
        assert state["exit_code"] == 0, state
        invoke(homes["applications"], "task.update", {"id": "repair", "actor": "applications", "status": "done"})
        invoke(homes["infrastructure"], "task.update", {"id": "diagnosis", "actor": "infrastructure", "status": "done"})
        repair_repo = pathlib.Path(state["cwd"])
        workspace = invoke(homes["applications"], "workspace.status", {"task_id": "repair"})
        assert repair_repo == pathlib.Path(workspace["path"]) and repair_repo != repo
        assert state["workspace_lease_id"] == workspace["lease_id"]
        tests = subprocess.run([python, "-m", "unittest", "-q"], cwd=repair_repo, capture_output=True)
        assert tests.returncode == 0
        assert (repo / "calc.py").read_text() == "def add(left, right):\n    return left - right\n"
        head = subprocess.run(["git", "-C", str(repair_repo), "rev-parse", "HEAD"], text=True, capture_output=True, check=True).stdout.strip()
        invoke(homes["navigator"], "task.update", {"id": "outcome", "actor": "navigator", "status": "done"})
        assert remote(homes["navigator"], "applications", "task.show", {"id": "repair"})["status"] == "done"
        evidence.update({"passed": True, "detached_job": state["status"], "tested_head": head,
                         "repair_workspace": str(repair_repo), "source_checkout_unchanged": True,
                         "durable_outcome_owner": "navigator"})
    except Exception as error:
        evidence.update({"passed": False, "error": str(error)})
        (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2))
        return 1
    (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
