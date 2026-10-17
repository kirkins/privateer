#!/usr/bin/env python3
"""Run an isolated two-ship recovery fixture; retain evidence, never run inference."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

import recovery_fixture
from recovery_fixture import (Commands, TERMINAL, append, configure_workspaces, crew_args, group_members, repair_request,
                              require, still_alive, wait_for, write_json)


class Pilot:
    def __init__(self, root, binary, timeout):
        self.root, self.binary = root, binary
        self.repo = root / "fixture"
        self.homes = {name: root / name for name in ("review", "apps")}
        self.dispatchers = {}
        self.dispatcher_logs = []
        self.environment = {key: value for key, value in os.environ.items() if key in ("PATH", "LANG", "LC_ALL")}
        self.environment.update(HOME=str(root / "user"), XDG_STATE_HOME=str(root / "state"),
                                XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"),
                                TMPDIR=str(root / "tmp"), PRIVATEER_FLEET=str(root / "fleet.sqlite"),
                                PRIVATEER_RESOURCE_HOME=str(root / "ledger"), PRIVATEER_SERVICE="none",
                                GIT_CONFIG_NOSYSTEM="1", PYTHONDONTWRITEBYTECODE="1", LC_ALL="C", TZ="UTC")
        for folder in ("commands", "user", "state", "config", "cache", "tmp", "fixture"):
            (root / folder).mkdir(mode=0o700)
        shutil.copyfile(recovery_fixture.__file__, root / "fixture-runtime.py")
        write_json(root / "scenario.json", {"deadline_monotonic": time.monotonic() + timeout,
                                            "timeout_secs": timeout})
        self.commands = Commands(root, "driver")
        self.evidence = {"schema_version": 1, "directory": str(root), "binary": str(binary),
                         "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                         "pilot_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                         "fixture_sha256": hashlib.sha256((root / "fixture-runtime.py").read_bytes()).hexdigest(),
                         "native_inference": False, "assertions": {}, "cleanup": {"passed": False},
                         "custody_scope": "Accepted handoff changes one task in the apps home; cross-home mail coordinates separate tasks.",
                         "limitations": ["Scripted fixture choices, not model judgement or native fleet autonomy.",
                                         "Local peer transport only; no real SSH, provider, forge or deployment.",
                                         "No existing fleet migration or credentials are used."]}

    def cli(self, ship):
        return [str(self.binary), "--home", str(self.homes[ship])]

    def call(self, ship, command, args=None, actor=None):
        env = dict(self.environment)
        if actor:
            env.update(PRIVATEER_ROLE="agent", PRIVATEER_ACTOR=actor)
        argv = self.cli(ship) + [command, "--json", json.dumps(args or {})]
        return json.loads(self.commands.run(argv, env=env).stdout)

    def check(self, name, condition, detail=None):
        require(condition, f"assertion failed: {name}")
        self.evidence["assertions"][name] = {"passed": True, "detail": detail}

    def start_dispatcher(self, ship):
        number = 1 + sum(path.name.startswith(f"dispatcher-{ship}-") for path in self.dispatcher_logs)
        path = self.root / f"dispatcher-{ship}-{number}.jsonl"
        self.dispatcher_logs.append(path)
        argv = self.cli(ship) + ["serve", "--json", '{"interval_secs":1}']
        with path.open("w") as log:
            child = subprocess.Popen(argv, env=self.environment, stdout=log, stderr=subprocess.STDOUT)
        self.dispatchers[ship] = child
        append(self.commands.log, {"argv": argv, "spawned_pid": child.pid, "log": str(path)})
        return child

    def stop_dispatcher(self, ship, abrupt=False):
        child = self.dispatchers.pop(ship, None)
        if child is None:
            return
        if child.poll() is None:
            child.kill() if abrupt else child.terminate()
        try:
            code = child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            code = child.wait(timeout=5)
        append(self.commands.log, {"dispatcher": ship, "pid": child.pid,
                                   "signal": "SIGKILL" if abrupt else "SIGTERM", "returncode": code})

    def job(self):
        return self.call("apps", "job.status", {"id": "repair-job"})

    def prepare(self):
        self.evidence["binary_version"] = self.commands.run([self.binary, "--version"], env=self.environment).stdout.strip()
        (self.repo / "calc.py").write_text("def add(left, right):\n    return left - right\n")
        (self.repo / "test_calc.py").write_text(
            "import unittest\nfrom calc import add\n"
            "class Regression(unittest.TestCase):\n"
            "    def test_signed_addition(self):\n"
            "        for left, right in [(3, 2), (-3, 2), (0, 0), (8, -5)]:\n"
            "            self.assertEqual(add(left, right), left + right)\n")
        for argv in (["git", "init", "-q"], ["git", "config", "user.name", "Privateer Recovery Pilot"],
                     ["git", "config", "user.email", "pilot@example.invalid"], ["git", "add", "."],
                     ["git", "commit", "-qm", "Seed isolated failing fixture"]):
            self.commands.run(argv, cwd=self.repo, env=self.environment)
        baseline = self.commands.run([sys.executable, "-B", "-m", "unittest", "-q"],
                                     cwd=self.repo, expect=False, env=self.environment)
        self.check("baseline_regression_fails", baseline.returncode != 0)
        self.source_head = self.commands.run(["git", "rev-parse", "HEAD"], cwd=self.repo, env=self.environment).stdout.strip()
        manifest = self.root / "manifest.json"
        write_json(manifest, {"schema_version": 1, "id": "recovery-fixture", "executable":
                              [sys.executable, str(self.root / "fixture-runtime.py"), "_adapter"],
                              "timeout_ms": 5000, "max_output_bytes": 65536})
        for ship in self.homes:
            self.call(ship, "init", {"resource_home": str(self.root / "ledger")})
            self.call(ship, "charter.put", {"id": "fixture", "objective": "Repair only the disposable fixture",
                                           "repo": str(self.repo), "min_free_bytes": 0,
                                           "actions": ["inspect", "spawn", "implement", "test", "message", "recovery", "cleanup"],
                                           "job_timeout_secs": 60, "max_heavy_jobs": 1})
            self.call(ship, "task.create", {"id": f"{ship}-duty", "charter_id": "fixture", "owner": ship,
                                           "title": f"Coordinate the {ship} fixture"})
            self.call(ship, "ship.put", {"name": ship, "scope": "Isolated recovery fixture", "first_mate":
                {"manifest": str(manifest), "duty_task": f"{ship}-duty", "options": {"root": str(self.root)},
                 "batch_secs": 0, "max_delay_secs": 0, "retry_backoff_secs": 1, "max_wakes_per_hour": 20}})
        configure_workspaces(self.root, self.homes.values())
        self.evidence["workspace_backend"] = "test protocol fixture backed by real Git worktrees"
        # One shared heavy slot; these scripted payloads write one small
        # file, so they claim no heavy-job memory or disk appetite.
        self.call("apps", "resource.configure", {"max_heavy_jobs": 1, "max_load_per_cpu": 1024,
                  "min_available_memory_bytes": 0, "ramp_secs": 0,
                  "heavy_estimate": {"memory_bytes": 0, "disk_bytes": 0}})
        for ship, other in (("review", "apps"), ("apps", "review")):
            self.call(ship, "peer.put", {"name": other, "transport": "local", "executable": str(self.binary),
                                        "home": str(self.homes[other])})
            self.call(ship, "ship.put", {"name": other, "scope": "Isolated peer", "location": {"peer": other}})
        self.call("review", "task.create", {"id": "outcome", "charter_id": "fixture", "owner": "review",
                                             "title": "Deliver independent fixture repair evidence"})
        self.call("apps", "task.create", {"id": "repair", "charter_id": "fixture", "owner": "review",
                                           "title": "Repair the local addition regression"})
        offered = self.call("apps", "task.handoff", {"id": "repair", "actor": "review", "target": "apps"}, "review")
        self.check("handoff_preserves_owner_until_acceptance", offered["owner"] == "review" and offered["handoff_to"] == "apps")
        accepted = self.call("apps", "task.accept", {"id": "repair", "actor": "apps"}, "apps")
        self.check("accepted_task_has_one_owner", accepted["owner"] == "apps" and accepted["handoff_to"] is None)

    def exercise(self):
        for ship in self.homes:
            self.start_dispatcher(ship)
        self.call("review", "mail.send", {"id": "captain-repair", "from": "captain", "to": "review",
                                           "kind": "request", "body": "Arrange the isolated fixture repair."})
        ready_path = self.root / "worker-ready.json"
        wait_for(self.commands, "crew decision and reply", lambda: ready_path.exists())
        ready = json.loads(ready_path.read_text())
        helper = json.loads((self.root / "helper.json").read_text())
        before = self.job()
        self.check("supervised_crew_running", before["status"] == "running" and before["payload"]["pid"] == ready["process"]["pid"], before)
        workspace = self.call("apps", "workspace.status", {"task_id": "repair"})
        repair_repo = Path(before["cwd"])
        self.check("repair_uses_leased_workspace", repair_repo == Path(workspace["path"])
                   and repair_repo != self.repo and before["workspace_lease_id"] == workspace["lease_id"], workspace)
        self.check("crew_decision_answered", self.call("apps", "crew.answer", {"id": ready["decision"]})["state"] == "answered", ready)
        self.stop_dispatcher("apps", abrupt=True)
        self.check("payload_survives_dispatcher_interruption", still_alive(ready["process"]) and still_alive(helper)
                   and self.job()["status"] == "running", {"worker": ready["process"], "helper": helper})
        (self.root / "release-repair").write_text("dispatcher is stopped; continue the fixture\n")
        def finished_job():
            job = self.job()
            return job if job["status"] in TERMINAL else None
        result = wait_for(self.commands, "detached repair completion", finished_job)
        self.check("repair_finishes_while_dispatcher_is_down", result["status"] == "succeeded" and result["exit_code"] == 0, result)
        self.commands.run([sys.executable, "-B", "-m", "unittest", "-q"], cwd=repair_repo, env=self.environment)
        head = self.commands.run(["git", "rev-parse", "HEAD"], cwd=repair_repo, env=self.environment).stdout.strip()
        worker = json.loads((self.root / "worker-result.json").read_text())
        self.check("independent_regression_passes_at_recorded_head", head == worker["tested_head"]
                   and Path(worker["cwd"]) == repair_repo, worker)
        source_head = self.commands.run(["git", "rev-parse", "HEAD"], cwd=self.repo, env=self.environment).stdout.strip()
        source_status = self.commands.run(["git", "status", "--porcelain"], cwd=self.repo, env=self.environment).stdout
        self.check("source_checkout_unchanged", source_head == self.source_head and not source_status
                   and (self.repo / "calc.py").read_text() == "def add(left, right):\n    return left - right\n")
        replayed = self.call("apps", "job.start", crew_args(self.root, self.binary, self.homes["apps"]), "apps")
        self.check("job_replay_preserves_incarnation", replayed["incarnation"] == result["incarnation"])
        inbox = self.call("apps", "mail.inbox", {"address": "apps", "all": True, "limit": 1000})["messages"]
        outcomes = [m for m in inbox if m["from"] == "crew" and m["payload"].get("status") == "succeeded"]
        self.check("one_durable_crew_outcome_before_restart", len(outcomes) == 1 and outcomes[0]["task_id"] == "repair", outcomes)
        wait_for(self.commands, "owned helper cleanup", lambda: not still_alive(helper) and not still_alive(ready["process"]))
        self.check("payload_and_owned_helper_exit_confirmed", (self.root / "helper-stopped").exists()
                   and not group_members(ready["process"]["pgid"]), helper)
        self.start_dispatcher("apps")
        def answered():
            request = self.call("review", "mail.show", {"id": "captain-repair"})
            return request if request["state"] == "answered" else None
        response = wait_for(self.commands, "post-restart captain reply", answered)
        self.check("restart_delivers_durable_result", response["answered_by"] == "captain-repair-reply", response)
        self.call("review", "mail.send", repair_request(), "review")
        def settled():
            for ship in self.homes:
                wakes = self.call(ship, "wake.list", {"ship": ship})["wakes"]
                if not wakes or any(w["status"] != "succeeded" for w in wakes):
                    return False
                if self.call(ship, "mail.inbox", {"address": ship})["messages"]:
                    return False
            return True
        wait_for(self.commands, "settled first mates", settled)
        for ship in self.homes:
            self.stop_dispatcher(ship)
            self.call(ship, "serve", {"once": True})
        tasks = {ship: self.call(ship, "task.show", {"id": "repair" if ship == "apps" else "outcome"}) for ship in self.homes}
        self.check("custody_survives_recovery", all(task["owner"] == ship and task["status"] == "done"
                   and task["handoff_to"] is None for ship, task in tasks.items()), tasks)
        requests = [self.call(ship, "mail.show", {"id": "repair-request"}) for ship in self.homes]
        self.check("cross_home_request_and_reply_are_durable", all(m["state"] == "answered"
                   and m["answered_by"] == "repair-accepted" for m in requests), requests)
        reports = self.call("apps", "mail.inbox", {"address": "apps", "all": True, "limit": 1000})["messages"]
        self.check("one_decision_and_one_outcome_after_replays", sum(m["payload"].get("report") == "decision" for m in reports) == 1
                   and sum(m["payload"].get("status") == "succeeded" for m in reports) == 1)
        self.check("one_worker_invocation", len((self.root / "worker-invocations.jsonl").read_text().splitlines()) == 1)
        self.evidence["tested_head"] = head

    def cleanup(self):
        # Scenario failure may have exhausted its deadline or evidence cap.
        # Neither budget may prevent stopping a dispatcher or cancelling crew.
        self.commands = Commands(self.root, "cleanup")
        self.commands.settings["deadline_monotonic"] = time.monotonic() + 20
        errors, jobs, groups = [], {}, {}
        for ship in list(self.dispatchers):
            try:
                self.stop_dispatcher(ship)
            except Exception as error:
                errors.append(f"dispatcher {ship}: {error}")
        for ship in self.homes:
            if not (self.homes[ship] / "privateer.sqlite").exists():
                continue
            try:
                observed = self.call(ship, "job.list", {"limit": 1000})["jobs"]
                for job in observed:
                    if job["status"] not in TERMINAL:
                        self.call(ship, "job.cancel", {"id": job["id"]})
                def stopped():
                    current = self.call(ship, "job.list", {"limit": 1000})["jobs"]
                    return current if all(j["status"] in TERMINAL for j in current) else None
                finished = wait_for(self.commands, f"{ship} job cleanup", stopped) if observed else []
                jobs[ship] = [{key: job[key] for key in ("id", "status", "incarnation", "payload", "supervisor")}
                              for job in finished]
                for job in finished:
                    if job.get("payload"):
                        pgid = job["payload"]["pid"]
                        groups[str(pgid)] = group_members(pgid)
                        require(not groups[str(pgid)], f"job {job['id']} still has live process-group members")
            except Exception as error:
                errors.append(f"jobs {ship}: {error}")
        try:
            resource = self.call("apps", "resource.status") if (self.homes["apps"] / "privateer.sqlite").exists() else None
            require(resource is None or (not resource["reservations"] and not resource["queue"]), "capacity remained reserved")
            for filename in ("worker-ready.json", "helper.json"):
                if (self.root / filename).exists():
                    record = json.loads((self.root / filename).read_text())
                    require(not still_alive(record.get("process", record)), f"recorded {filename} process is still alive")
        except Exception as error:
            errors.append(str(error))
        self.evidence["cleanup"] = {"passed": not errors, "errors": errors, "jobs": jobs, "live_group_members": groups}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path, help="new evidence directory; otherwise a retained temporary directory")
    parser.add_argument("--timeout", type=int, default=90, help="scenario deadline in seconds (30..300)")
    options = parser.parse_args()
    require(30 <= options.timeout <= 300, "timeout must be between 30 and 300 seconds")
    binary = options.binary.resolve(strict=True)
    if options.output:
        root = options.output.absolute()
        root.mkdir(mode=0o700)
        root = root.resolve(strict=True)
    else:
        root = Path(tempfile.mkdtemp(prefix="privateer-recovery-pilot-")).resolve()
    pilot = Pilot(root, binary, options.timeout)
    try:
        pilot.prepare()
        pilot.exercise()
        pilot.evidence["passed"] = True
    except Exception as error:
        pilot.evidence.update(passed=False, error=str(error))
    finally:
        pilot.cleanup()
    pilot.evidence["passed"] = pilot.evidence["passed"] and pilot.evidence["cleanup"]["passed"]
    pilot.evidence["command_logs"] = [str(path.relative_to(root)) for path in sorted((root / "commands").glob("*.jsonl"))]
    pilot.evidence["dispatcher_logs"] = [str(path.relative_to(root)) for path in pilot.dispatcher_logs]
    write_json(root / "evidence.json", pilot.evidence)
    print(json.dumps({"passed": pilot.evidence["passed"], "native_inference": False,
                      "directory": str(root), "evidence": str(root / "evidence.json"),
                      "error": pilot.evidence.get("error"), "cleanup_errors": pilot.evidence["cleanup"]["errors"]}))
    return 0 if pilot.evidence["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
