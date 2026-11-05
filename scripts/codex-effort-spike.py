#!/usr/bin/env python3
"""ACT3a, opt-in zero-turn model/effort qualification on an owned app-server.

Uses the installed executable directly (no package/cache copies), one reusable
/var/tmp root, loopback mock only, and exact UUIDs. No credentials or model turn.
The small daemon.pid is fixture observation metadata, not a managed-daemon
installation; all TUI attaches therefore explicitly use the verified --remote.
This is not a live hosted pane, hook-trust or attended-mail qualification.
"""
import argparse
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import sys
import termios
import threading
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
from codex_socket import Daemon, process_identity, socket_paths
from codex_binding import prepare


class Mutating(Daemon):
    METHODS = Daemon.METHODS | {"thread/start", "thread/name/set", "thread/settings/update"}


def probe(root, codex):
    root = root.resolve()
    assert root.is_relative_to(Path("/var/tmp")) and root != Path("/var/tmp")
    root.mkdir(mode=0o700, exist_ok=True)
    assert root.stat().st_uid == os.getuid() and not root.stat().st_mode & 0o077
    env = {"PATH": "/usr/bin:/bin", "TERM": "xterm-256color", "PRIVATEER_SERVICE": "none"}
    for key, name in {"HOME": "home", "CODEX_HOME": "c", "TMPDIR": "tmp",
                      "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state",
                      "PRIVATEER_RESOURCE_HOME": "resources"}.items():
        (root / name).mkdir(mode=0o700, exist_ok=True)
        env[key] = str(root / name)
    env["PRIVATEER_FLEET"] = str(root / "fleet.sqlite")
    home, folder = root / "c", root / "alpha"
    folder.mkdir(exist_ok=True)
    subprocess.run(["git", "init", "-q", str(folder)], check=True, env=env)
    for name in ("app-server-control", "app-server-daemon"):
        (home / name).mkdir(mode=0o700, exist_ok=True)
    rendezvous, physical = socket_paths(home)
    assert not rendezvous.exists() and not rendezvous.is_symlink()
    assert not physical.exists() and not physical.is_symlink()
    assert not physical.with_suffix(".lock").exists()
    calls = []

    class Mock(BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(self.path)
            self.send_response(500)
            self.end_headers()
        do_GET = do_POST
        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    worker = threading.Thread(target=server.serve_forever)
    worker.start()
    daemon, children = None, []
    evidence = {"version": subprocess.check_output([codex, "--version"], env=env, text=True).strip(),
                "provider_requests": calls}
    old_cwd = Path.cwd()
    try:
        (home / "config.toml").write_text(f'''model = "mock-model"
model_provider = "fixture"
check_for_update_on_startup = false
[model_providers.fixture]
name = "Loopback only"
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
[analytics]
enabled = false
[feedback]
enabled = false
[projects.{json.dumps(str(folder))}]
trust_level = "trusted"
''')
        with (root / "daemon.log").open("w") as log:
            argv = [codex, "app-server", "--listen", "unix://" + str(rendezvous)]
            evidence["server_argv"] = argv
            daemon = subprocess.Popen(argv, env=env, cwd=root, stdout=log, stderr=log)
        identity = process_identity(daemon.pid)
        pidfile = home / "app-server-daemon/daemon.pid"
        pidfile.write_text(json.dumps({"pid": daemon.pid, "processIdentity": identity}))
        pidfile.chmod(0o600)
        deadline = time.monotonic() + 10
        while not rendezvous.is_symlink():
            assert daemon.poll() is None and time.monotonic() < deadline, "app-server did not start"
            time.sleep(.05)
        with Mutating(home, timeout=10) as client:
            assert client.pid == daemon.pid
            started = client.call("thread/start", {"cwd": str(folder), "historyMode": "legacy",
                "ephemeral": False, "model": "ship-model", "config": {"model_reasoning_effort": "high"}})
            evidence["start"] = started
            session = started["thread"]["id"]
            assert started["model"] == "ship-model" and started["reasoningEffort"] == "high"
            client.call("thread/name/set", {"threadId": session, "name": "alpha first mate"})

        def read():
            with Daemon(home, timeout=10) as client:
                assert client.pid == daemon.pid
                return client.call("thread/read", {"threadId": session})["thread"]

        def tui(label, argv):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
            try:
                child = subprocess.Popen(argv, cwd=folder, env=env, stdin=slave, stdout=slave,
                                         stderr=slave, start_new_session=True)
            finally:
                os.close(slave)
            children.append((child, master))
            output = bytearray()
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and child.poll() is None:
                ready, _, _ = select.select([master], [], [], .1)
                if ready:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        break
                    output.extend(data)
                    assert len(output) <= 1 << 20
                    for query, response in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c"),
                                            (b"\x1b[>c", b"\x1b[>0;0;0c")]:
                        if query in data:
                            os.write(master, response)
                if b"state=idle" in output:
                    break
            (root / (label + ".pty.log")).write_bytes(output)
            evidence[label] = {"argv": argv, "alive": child.poll() is None,
                               "idle": b"state=idle" in output, "read": read()}
            assert child.poll() is None and evidence[label]["idle"], "TUI did not attach"
            assert b"Running without the shared background server" not in output
            assert evidence[label]["read"]["turns"] == []
            return child, master

        def stop(pair):
            child, master = pair
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            os.close(master)
            children.remove(pair)

        baseline = tui("baseline", [codex, "resume", session, "-C", str(folder),
                                    "--no-alt-screen", "--remote", "unix://" + str(physical)])
        # Mechanism (a) works after idle attach. It is NOT qualified as a
        # first-turn hook-time update (could race/block behind turn entry).
        with Mutating(home, timeout=10) as client:
            params = {"threadId": session, "model": "updated-model", "effort": "medium"}
            result = client.call("thread/settings/update", params)
            evidence["settings_update"] = {"params": params, "result": result, "read": read()}
        assert evidence["settings_update"]["read"]["model"] == "updated-model"
        assert evidence["settings_update"]["read"]["reasoningEffort"] == "medium"
        stop(baseline)
        # Mechanism (b), using production prepare's peer verification/insertion.
        os.chdir(folder)
        context = {**env, "PRIVATEER_HOME": env["HOME"], "PRIVATEER_CONTROL_CLI": "/fixture/privateer",
                   "PRIVATEER_ROLE": "agent", "PRIVATEER_ACTOR": "alpha",
                   "PRIVATEER_PRESENCE_HOLDER": "host:alpha", "PRIVATEER_HOSTED": "1"}
        argv = prepare([codex, "resume", session, "-C", str(folder), "--no-alt-screen",
                        "--model", "ship-model", "-c", 'model_reasoning_effort="high"'], context)
        # This is deliberately a zero-turn settings probe, not a binding/mail
        # fixture: do NOT submit the hosted bearings prompt or trust any hook.
        assert argv[-2] == "--" and "first mate of alpha" in argv[-1]
        argv = argv[:-2]
        attached = tui("remote_overrides", argv)
        effective = evidence["remote_overrides"]["read"]
        assert effective["model"] == "ship-model" and effective["reasoningEffort"] == "high"
        assert effective["modelProvider"] == "fixture", "provider changed"
        stop(attached)
        assert calls == [], calls
    finally:
        os.chdir(old_cwd)
        for child, master in children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            os.close(master)
        try:
            if daemon is not None:
                if daemon.poll() is None:
                    daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=10)
                evidence["daemon_exit"] = daemon.returncode
                try:
                    process_identity(daemon.pid)
                except (FileNotFoundError, ValueError):
                    evidence["daemon_exited_confirmed"] = True
                else:
                    raise AssertionError("owned app-server still alive")
                for path in (rendezvous, physical, physical.with_suffix(".lock")):
                    if path.exists() or path.is_symlink():
                        assert path.lstat().st_uid == os.getuid()
                        path.unlink()
        finally:
            server.shutdown()
            server.server_close()
            worker.join()
            (root / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"version": evidence["version"], "model": effective["model"],
                      "effort": effective["reasoningEffort"], "provider_requests": len(calls),
                      "daemon_exited_confirmed": evidence["daemon_exited_confirmed"],
                      "evidence": str(root / "evidence.json")}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--root", type=Path, default=Path("/var/tmp/act3a-probe"))
    args = parser.parse_args()
    probe(args.root, str(Path(args.codex).resolve(strict=True)))
