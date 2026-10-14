#!/usr/bin/env python3
"""A real SSH RPC canary in explicitly supplied, exclusive temporary paths."""
import argparse
import json
import pathlib
import re
import shlex
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, help="Local Privateer binary")
    parser.add_argument("--host", required=True, help="Native SSH alias or user@host")
    parser.add_argument("--remote-binary", required=True, help="Already installed remote binary")
    parser.add_argument("--remote-home", required=True, help="New exclusive temporary home")
    parser.add_argument("--remote-repo", required=True, help="New exclusive temporary fixture directory")
    parser.add_argument("--cleanup-remote", action="store_true")
    options = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.][A-Za-z0-9_.-]*(?:@[A-Za-z0-9_.][A-Za-z0-9_.-]*)?", options.host):
        parser.error("Invalid SSH alias/user@host")
    for value in (options.remote_binary, options.remote_home, options.remote_repo):
        path = pathlib.PurePosixPath(value)
        if not path.is_absolute() or ".." in path.parts or any(ord(char) < 32 for char in value):
            parser.error("Remote paths must be absolute and contain no dot/control components")
    for value in (options.remote_home, options.remote_repo):
        if not value.startswith(("/tmp/", "/private/tmp/")):
            parser.error("Canary home/repository must be temporary paths")
    binary = pathlib.Path(options.binary).resolve(strict=True)
    root = pathlib.Path(tempfile.mkdtemp(prefix="privateer-ssh-evidence-"))
    local_home = root / "navigator"
    evidence = {"directory": str(root), "host": options.host, "native_inference": False,
                "remote_home": options.remote_home, "remote_repo": options.remote_repo}
    remote_created = False

    def ssh_python(code, *args):
        remote_command = " ".join(shlex.quote(token) for token in ("python3", "-c", code, *args))
        return subprocess.run(["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", options.host, remote_command],
                              text=True, capture_output=True, timeout=30, check=True)

    def local(command, args=None):
        result = subprocess.run([str(binary), "--home", str(local_home), command, "--json", json.dumps(args or {})],
                                text=True, capture_output=True, timeout=30)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr or result.stdout}")
        return json.loads(result.stdout)

    def remote(command, args=None, expect=True):
        envelope = local("peer.call", {"name": "department", "command": command, "args": args or {}})
        if expect and not envelope["ok"]:
            raise RuntimeError(envelope["error"])
        return envelope["result"] if expect else envelope

    try:
        # Refuse existing paths: this script never selects or repairs a live home.
        setup = "import pathlib,sys; paths=[pathlib.Path(p) for p in sys.argv[1:]]; assert all(not p.exists() and not p.is_symlink() for p in paths); [p.mkdir(parents=True,mode=0o700) for p in paths]"
        ssh_python(setup, options.remote_home, options.remote_repo)
        remote_created = True
        local("peer.put", {"name": "department", "host": options.host,
                           "executable": options.remote_binary, "home": options.remote_home, "timeout_ms": 20_000})
        remote("init")
        remote("charter.put", {"id": "canary", "objective": "Qualify temporary two-host RPC behavior",
                               "repo": options.remote_repo, "min_free_bytes": 1024})
        remote("task.create", {"id": "outcome", "charter_id": "canary", "owner": "infrastructure", "title": "Own the remote canary"})
        remote("task.create", {"id": "child", "parent": "outcome", "owner": "applications", "title": "Inherited local department work"})
        remote("task.handoff", {"id": "outcome", "actor": "infrastructure", "target": "applications"})
        refusal = remote("task.accept", {"id": "outcome", "actor": "navigator"}, expect=False)
        assert refusal["ok"] is False
        assert remote("task.accept", {"id": "outcome", "actor": "applications"})["owner"] == "applications"
        remote("message.send", {"id": "targeted", "task_id": "child", "from": "navigator", "to": "applications", "body": "Temporary two-host canary; no production work"})
        assert len(remote("message.inbox", {"actor": "applications"})["messages"]) == 1
        assert remote("message.inbox", {"actor": "infrastructure"})["messages"] == []
        for number in range(20):
            remote("event.emit", {"id": f"alert-{number}", "task_id": "outcome", "issue_key": "one-issue", "kind": "alert", "payload": {"number": number}})
        assert len(remote("event.pending", {"task_id": "outcome"})["events"]) == 1
        assert len(remote("event.history", {"task_id": "outcome", "limit": 100})["events"]) == 20
        remote("charter.revoke", {"id": "canary"})
        revoked = remote("job.start", {"id": "must-not-run", "task_id": "child", "cwd": options.remote_repo,
                                       "command": ["/bin/echo", "must-not-run"], "heavy": False}, expect=False)
        assert revoked["ok"] is False and "revoked" in revoked["error"]
        evidence.update({"passed": True, "real_ssh_rpc": True, "custody_acceptance": True,
                         "targeted_messages": True, "facts": 20, "pending": 1,
                         "remote_revocation_enforced": True, "cross_host_authority_inheritance": False})
    except Exception as error:
        evidence.update({"passed": False, "error": str(error)})
    finally:
        if options.cleanup_remote and remote_created:
            try:
                ssh_python("import shutil,sys; [shutil.rmtree(p) for p in sys.argv[1:]]", options.remote_home, options.remote_repo)
                evidence["remote_temporary_state_removed"] = True
            except Exception as error:
                evidence["cleanup_error"] = str(error)
                evidence["passed"] = False
        (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2))
    return 0 if evidence.get("passed") else 1


if __name__ == "__main__":
    sys.exit(main())
