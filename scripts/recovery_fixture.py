#!/usr/bin/env python3
"""Bounded subprocess fixtures and evidence helpers for recovery-pilot.py."""
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import time


TERMINAL = {"succeeded", "failed", "cancelled", "timed_out"}
LOG_LIMIT = 1024 * 1024
OUTPUT_LIMIT = 65536


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def append(path, value):
    line = json.dumps(value, separators=(",", ":")) + "\n"
    require((path.stat().st_size if path.exists() else 0) + len(line.encode()) <= LOG_LIMIT,
            f"evidence log exceeded {LOG_LIMIT} bytes: {path.name}")
    with path.open("a") as stream:
        stream.write(line)


class Commands:
    def __init__(self, root, name, cli=None):
        self.root, self.cli = root, cli
        self.settings = json.loads((root / "scenario.json").read_text())
        self.log = root / "commands" / f"{name}.jsonl"

    def remaining(self):
        value = self.settings["deadline_monotonic"] - time.monotonic()
        require(value > 0, "pilot deadline expired")
        return value

    def run(self, argv, *, cwd=None, expect=True, timeout=15, env=None):
        start = time.monotonic()
        end = start + min(timeout, self.remaining())
        child = subprocess.Popen([str(v) for v in argv], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        buffers, error = {"stdout": bytearray(), "stderr": bytearray()}, None
        with selectors.DefaultSelector() as selector:
            for name in buffers:
                selector.register(getattr(child, name), selectors.EVENT_READ, name)
            try:
                while selector.get_map() or child.poll() is None:
                    require(time.monotonic() < end, "fixture command timed out")
                    for key, _ in selector.select(0.05):
                        data = os.read(key.fd, 4096)
                        if not data:
                            selector.unregister(key.fileobj)
                            key.fileobj.close()
                        else:
                            require(sum(map(len, buffers.values())) + len(data) <= OUTPUT_LIMIT,
                                    "fixture command exceeded the 64 KiB output bound")
                            buffers[key.data].extend(data)
                child.wait(timeout=max(0.1, end - time.monotonic()))
            except Exception as failure:
                error = str(failure)
                # This is only the command process we started. Detached runtime
                # jobs remain subject to Pilot.cleanup and their own identities.
                child.kill()
                child.wait(timeout=3)
            finally:
                for stream in (child.stdout, child.stderr):
                    stream.close()
        result = subprocess.CompletedProcess(argv, child.returncode,
                    buffers["stdout"].decode(errors="replace"), buffers["stderr"].decode(errors="replace"))
        append(self.log, {"argv": [str(v) for v in argv], "cwd": str(cwd) if cwd else None,
                          "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr,
                          "error": error, "elapsed_ms": round((time.monotonic() - start) * 1000)})
        require(error is None, error)
        require(not expect or result.returncode == 0,
                f"command failed ({result.returncode}): {argv[0]} {result.stderr[-1000:]}")
        return result

    def call(self, command, args=None, **kwargs):
        require(self.cli is not None, "CLI context missing")
        result = self.run(self.cli + [command, "--json", json.dumps(args or {})], **kwargs)
        return json.loads(result.stdout)


def process_identity(pid):
    """Observe identity as well as liveness; a recycled PID is a different process."""
    if sys.platform.startswith("linux"):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return {"pid": pid, "pgid": int(stat[2]), "started": stat[19], "state": stat[0],
                    "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip()}
        except FileNotFoundError:
            return None
    result = subprocess.run(["ps", "-p", str(pid), "-o", "stat=", "-o", "lstart="],
                            capture_output=True, text=True, timeout=3,
                            env={**os.environ, "LC_ALL": "C", "TZ": "UTC"})
    fields = result.stdout.strip().split(None, 1)
    if len(fields) != 2:
        return None
    try:
        return {"pid": pid, "pgid": os.getpgid(pid), "state": fields[0], "started": fields[1]}
    except ProcessLookupError:
        # Normal exit between the ps snapshot and the process-group query.
        return None


def group_members(pgid):
    result = subprocess.run(["ps", "-axo", "pid=,pgid=,stat="], capture_output=True, text=True, timeout=3)
    require(result.returncode == 0 and len(result.stdout.encode()) <= OUTPUT_LIMIT,
            "could not observe bounded process-group membership")
    return [int(pid) for line in result.stdout.splitlines() for pid, group, state in [line.split()]
            if int(group) == pgid and not state.startswith("Z")]


def still_alive(recorded):
    current = process_identity(recorded["pid"])
    return bool(current and current["started"] == recorded["started"]
                and current.get("boot_id") == recorded.get("boot_id")
                and not current["state"].startswith("Z"))


def wait_for(commands, description, observe):
    while commands.remaining() > 0:
        value = observe()
        if value:
            return value
        time.sleep(0.05)
    raise RuntimeError(f"deadline waiting for {description}")


def crew_args(root, binary, home):
    return {"id": "repair-job", "task_id": "repair", "heavy": True,
            "estimate": {"cpus": 1, "memory_bytes": 67108864},
            "cleanup_descendants": True, "wait_for_capacity": True, "max_wait_secs": 20,
            "command": [sys.executable, str(root / "fixture-runtime.py"), "_worker",
                        str(root), str(binary), str(home)]}


def fixture_adapter():
    request = json.loads(sys.stdin.buffer.readline(65537))
    context, options = request["context"], request["options"]
    require(request["version"] == 1 and request["method"] == "command", "unexpected fixture request")
    root = Path(options["root"])
    argv = [sys.executable, str(root / "fixture-runtime.py"), "_mate", str(root),
            json.dumps(context["control"]["cli_argv"]), context["owner"], context["brief"]]
    print(json.dumps({"id": request["id"], "version": 1, "ok": True,
                      "result": {"argv": argv, "cwd": context["repo"], "env_names": []}}))


def fixture_mate(root, cli, ship, prompt):
    require(os.environ.get("PRIVATEER_ROLE") == "agent" and os.environ.get("PRIVATEER_ACTOR") == ship,
            "first mate lost its runtime role")
    commands = Commands(root, f"mate-{ship}-{os.environ['PRIVATEER_JOB']}", json.loads(cli))
    through = int(re.search(r"through seq (\d+)", prompt).group(1))
    messages = commands.call("mail.inbox", {"address": ship, "limit": 1000})["messages"]
    for mail in (m for m in messages if m["seq"] <= through):
        if ship == "review" and mail["id"] == "captain-repair":
            commands.call("mail.send", repair_request())
        elif ship == "apps" and mail["id"] == "repair-request":
            commands.call("mail.send", {"id": "plan-check", "from": ship, "to": "review",
                                        "kind": "request", "body": "Confirm the isolated addition fixture scope."})
            commands.call("job.start", crew_args(root, commands.cli[0], commands.cli[2]))
            commands.call("mail.send", {"id": "repair-accepted", "from": ship, "to": "review",
                                        "kind": "reply", "correlation": mail["id"], "body": "Repair is supervised by apps."})
        elif ship == "review" and mail["id"] == "plan-check":
            commands.call("mail.send", {"id": "plan-confirmed", "from": ship, "to": "apps",
                                        "kind": "reply", "correlation": mail["id"], "body": "Only repair and test the fixture."})
        elif ship == "apps" and mail["from"] == "crew" and mail["kind"] == "request":
            commands.call("mail.send", {"id": "crew-plan-answer", "from": ship, "to": "crew",
                                        "kind": "reply", "correlation": mail["id"], "body": "Apply addition, then run the regression."})
        elif ship == "apps" and mail["from"] == "crew" and mail["payload"].get("status") == "succeeded":
            tested = json.loads((root / "worker-result.json").read_text())
            commands.call("task.update", {"id": "repair", "actor": ship, "status": "done"})
            commands.call("mail.send", {"id": "repair-result", "from": ship, "to": "review",
                                        "kind": "notice", "body": "Fixture regression repaired and tested.", "payload": tested})
        elif ship == "review" and mail["id"] == "repair-result":
            commands.call("mail.send", {"id": "captain-repair-reply", "from": ship, "to": "captain",
                                        "kind": "reply", "correlation": "captain-repair",
                                        "body": "Apps completed the isolated repair.", "payload": mail["payload"]})
            commands.call("task.update", {"id": "outcome", "actor": ship, "status": "done"})
    commands.call("mail.ack", {"address": ship, "through_seq": through})
    print(json.dumps({"type": "fixture.completed", "ship": ship, "through_seq": through}), flush=True)


def fixture_helper(root):
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    write_json(root / "helper.json", process_identity(os.getpid()))
    deadline = json.loads((root / "scenario.json").read_text())["deadline_monotonic"]
    try:
        while time.monotonic() < deadline:
            time.sleep(0.1)
    finally:
        (root / "helper-stopped").write_text("owned helper exited\n")


def fixture_worker(root, binary, home):
    commands = Commands(root, "worker", [binary, "--home", home])
    require(os.environ.get("PRIVATEER_ROLE") == "agent" and os.environ.get("PRIVATEER_ACTOR") == "apps",
            "crew lost its runtime role")
    append(root / "worker-invocations.jsonl", {"job": os.environ["PRIVATEER_JOB"], "pid": os.getpid()})
    subprocess.Popen([sys.executable, str(root / "fixture-runtime.py"), "_helper", str(root)])
    wait_for(commands, "helper identity", lambda: (root / "helper.json").exists())
    report = {"kind": "decision", "id": "repair-plan", "body": "Apply addition and run the local regression?"}
    decision = commands.call("crew.report", report)
    repeated = commands.call("crew.report", report)
    require(repeated["id"] == decision["id"] and repeated["duplicate"], "crew decision duplicated")
    answer = commands.call("crew.answer", {"id": decision["id"], "wait_secs": 30}, timeout=35)
    require(answer["state"] == "answered" and answer["reply"]["from"] == "apps", "crew received no owner reply")
    write_json(root / "worker-ready.json", {"process": process_identity(os.getpid()), "decision": decision["id"],
                                           "reply": answer["reply"]["id"]})
    wait_for(commands, "dispatcher interruption gate", lambda: (root / "release-repair").exists())
    repo = root / "fixture"
    (repo / "calc.py").write_text("def add(left, right):\n    return left + right\n")
    commands.run([sys.executable, "-B", "-m", "unittest", "-q"], cwd=repo)
    commands.run(["git", "add", "calc.py"], cwd=repo)
    commands.run(["git", "commit", "-qm", "Repair isolated addition regression"], cwd=repo)
    head = commands.run(["git", "rev-parse", "HEAD"], cwd=repo).stdout.strip()
    write_json(root / "worker-result.json", {"tested_head": head, "regression_passed": True,
                                            "decision": decision["id"], "native_inference": False})


def repair_request():
    return {"id": "repair-request", "from": "review", "to": "apps", "kind": "request",
            "body": "Repair the isolated addition fixture under apps custody."}


if __name__ == "__main__":
    mode = sys.argv[1]
    if mode == "_adapter":
        fixture_adapter()
    elif mode == "_mate":
        fixture_mate(Path(sys.argv[2]), *sys.argv[3:])
    elif mode == "_worker":
        fixture_worker(Path(sys.argv[2]), *sys.argv[3:])
    elif mode == "_helper":
        fixture_helper(Path(sys.argv[2]))
    else:
        raise SystemExit("unknown fixture mode")
