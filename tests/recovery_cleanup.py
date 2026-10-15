#!/usr/bin/env python3
"""Exhaust scenario evidence while real crew is alive, then verify cleanup."""
import argparse
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
from recovery_fixture import (Commands, LOG_LIMIT, crew_args, group_members,
                              process_identity, require, still_alive, wait_for, write_json)

spec = importlib.util.spec_from_file_location("recovery_pilot", SCRIPTS / "recovery-pilot.py")
pilot_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pilot_module)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    options = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="privateer-recovery-cleanup-")).resolve()
    pilot = pilot_module.Pilot(root, options.binary.resolve(strict=True), 45)
    evidence = {"native_inference": False, "passed": False, "directory": str(root)}
    try:
        pilot.prepare()
        # A real dispatcher tests that cleanup switches logs before stopping it.
        pilot.start_dispatcher("review")
        pilot.call("apps", "job.start", crew_args(root, pilot.binary, pilot.homes["apps"]), "apps")
        wait_for(pilot.commands, "live crew helper", lambda: (root / "helper.json").exists())
        job = pilot.job()
        worker = process_identity(job["payload"]["pid"])
        helper = json.loads((root / "helper.json").read_text())
        require(job["status"] == "running" and still_alive(worker) and still_alive(helper), "crew was not alive before cleanup")
        # One valid JSONL record fills the evidence cap exactly. Expire the
        # scenario deadline as well; cleanup must own both independent budgets.
        record = {"test": "exhausted scenario evidence", "padding": ""}
        overhead = len((json.dumps(record, separators=(",", ":")) + "\n").encode())
        record["padding"] = "x" * (LOG_LIMIT - overhead)
        pilot.commands.log.write_text(json.dumps(record, separators=(",", ":")) + "\n")
        pilot.commands.settings["deadline_monotonic"] = time.monotonic() - 1
        evidence["driver_log_bytes"] = pilot.commands.log.stat().st_size
        pilot.cleanup()
        evidence["cleanup"] = pilot.evidence["cleanup"]
        require(evidence["cleanup"]["passed"], "cleanup was blocked by exhausted scenario evidence")
        observer = Commands(root, "test-observer", pilot.cli("apps"))
        observer.settings["deadline_monotonic"] = time.monotonic() + 10
        result = observer.call("job.status", {"id": "repair-job"}, env=pilot.environment)
        resource = observer.call("resource.status", env=pilot.environment)
        require(result["status"] == "cancelled", "crew job was not cancelled")
        require(not still_alive(worker) and not still_alive(helper) and not group_members(worker["pgid"]),
                "recorded crew payload/group survived cleanup")
        require(not resource["reservations"] and not resource["queue"], "cleanup left capacity reserved")
        evidence.update(passed=True, job_status=result["status"], live_group_members=group_members(worker["pgid"]),
                        reservations=resource["reservations"], queue=resource["queue"])
    except Exception as error:
        evidence["error"] = str(error)
    finally:
        # Keep a regression against the broken implementation from leaking its
        # test payload. The primary result above remains a failure after rescue.
        if not evidence["passed"]:
            pilot.commands = Commands(root, "test-rescue")
            pilot.commands.settings["deadline_monotonic"] = time.monotonic() + 20
            pilot.cleanup()
            evidence["rescue_cleanup"] = pilot.evidence["cleanup"]
        write_json(root / "evidence.json", evidence)
    print(json.dumps({"passed": evidence["passed"], "native_inference": False,
                      "evidence": str(root / "evidence.json")}))
    return 0 if evidence["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
