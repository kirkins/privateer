#!/usr/bin/python3
"""SSH-shaped move fault injector: executes the real remote gateway command.

State and fault schedules are fixture-local files, never production knobs.
"""
import json
import os
import pathlib
import subprocess
import sys
import time

root = pathlib.Path(__file__).resolve().parent
request = sys.stdin.buffer.readline()
command = json.loads(request)["command"]
with (root / "move-calls.jsonl").open("a") as log:
    log.write(request.decode())
counts_path = root / "move-counts.json"
counts = json.loads(counts_path.read_text()) if counts_path.exists() else {}
index = counts.get(command, 0)
counts[command] = index + 1
counts_path.write_text(json.dumps(counts))
schedules = json.loads((root / "move-faults.json").read_text())
schedule = schedules.get(command, [])
fault = schedule[min(index, len(schedule) - 1)] if schedule else "pass"
if fault == "fail":
    sys.exit(17)
if fault == "old":
    print(json.dumps({"id": json.loads(request)["id"], "ok": True, "result": {"transfer": 0}}))
    sys.exit(0)
if fault == "corrupt":
    body = json.loads(request)
    for chunk in body["args"]["chunks"]:
        if chunk["hex"]:
            chunk["hex"] = ("00" if chunk["hex"][:2] != "00" else "ff") + chunk["hex"][2:]
            break
    request = (json.dumps(body) + "\n").encode()
env = dict(os.environ, HOME=str(root), PATH=f"{root}/bin:/usr/bin:/bin",
           PRIVATEER_FLEET=str(root / "fleet.sqlite"),
           PRIVATEER_RESOURCE_HOME=str(root / "ledger"))
result = subprocess.run(["/bin/sh", "-c", sys.argv[-1]], input=request,
                        stdout=subprocess.PIPE, env=env, check=False)
# Delay after the target applied the batch: tests a genuinely lost response.
if fault == "timeout":
    time.sleep(5)
sys.stdout.buffer.write(result.stdout)
sys.exit(result.returncode)
