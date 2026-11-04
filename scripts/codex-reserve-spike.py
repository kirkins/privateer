#!/usr/bin/env python3
"""Opt-in ACT2 fresh-session reservation fixture; no login, provider or model.

Run with --binary <built-privateer> --codex <installed-codex>. A fixture-owned
daemon in a throwaway CODEX_HOME, the same narrow socket exception as the
binding fixture, and a loopback mock provider that must receive ZERO requests.
It proves, against the real 0.166 daemon and TUI:
  1. a zero-turn legacy thread/start UUID alone is NOT attachable
     (`codex resume` refuses: no rollout found);
  2. the reservation protocol (thread/start, durable UUID, thread/name/set,
     exact-UUID verification) is, including crashes on both sides of the UUID
     commit, with the lost-response orphan never adopted;
  3. the operator control `attended.codex.session` sets first_mate.codex_session;
  4. an exact `codex resume <UUID>` TUI attaches with zero turns and zero
     provider requests, and which model/effort the attach actually applied.
This is not attended-mail or live qualification; no binding/delivery is run.
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import sys
import termios
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "plugins/attended-codex"))
from codex_socket import Daemon, load

spec = importlib.util.spec_from_file_location("binding_fixture", ROOT / "scripts/codex-binding-spike.py")
binding_fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(binding_fixture)
RESERVE = ROOT / "plugins/attended-codex/codex_reserve.py"


class Mutating(Daemon):
    # Fixture-only: the baseline thread/start that is NOT a reservation.
    METHODS = Daemon.METHODS | {"thread/start"}


class Fixture(binding_fixture.Fixture):
    def setup(self):
        config = f'''model = "mock-model"
model_provider = "fixture"
check_for_update_on_startup = false
[model_providers.fixture]
name = "Local fixture only"
base_url = "http://127.0.0.1:{self.server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
[analytics]
enabled = false
[feedback]
enabled = false
'''
        for name in ("alpha", "beta"):
            folder = self.root / name
            if name == "alpha":
                self.cli("ship.create", {"name": name, "folder": str(folder), "harness": "codex"})
            else:
                folder.mkdir()
            self.run(["git", "init", "-q", str(folder)])
            config += f'\n[projects.{json.dumps(str(folder))}]\ntrust_level = "trusted"\n'
        (self.home / "config.toml").write_text(config)
        settings = self.home / "app-server-daemon"
        settings.mkdir()
        (settings / "settings.json").write_text(json.dumps({"remoteControlEnabled": False,
            "shutdownGraceSeconds": 5, "updater": {"autoUpdateEnabled": False}}))
        self.daemon = json.loads(self.run(["codex", "app-server", "daemon", "start"]))
        assert self.daemon["status"] == "started" and self.daemon["socketPath"] == str(self.rendezvous)

    def loaded(self):
        with Daemon(self.home) as daemon:
            assert daemon.pid == self.daemon["pid"]
            return set(daemon.call("thread/loaded/list", {"limit": 32})["data"])

    def read(self, session):
        with Daemon(self.home) as daemon:
            return daemon.call("thread/read", {"threadId": session})["thread"]

    def reserve(self, folder, inject=None, current=None):
        """The real adapter child, optionally crashing at one boundary."""
        request = {"ship": folder.name, "cwd": str(folder.resolve()), "model": None,
                   "name": f"{folder.name} first mate", "current": current}
        path = folder.resolve() / ".codex/privateer-attended/reservation.json"
        if inject is None:
            argv = ["/usr/bin/python3", "-I", "-B", str(RESERVE), "create", str(path)]
        else:
            # Simulated crash: the process dies (no cleanup) at the boundary.
            argv = ["/usr/bin/python3", "-I", "-B", "-c", (
                "import os, sys; sys.path.insert(0, %r); sys.argv = ['codex_reserve.py', 'create', %r]\n"
                "import codex_reserve as r\n"
                "if %r == 'before_commit':\n"
                "    r.update = lambda *a, **k: os._exit(9)\n"
                "else:\n"
                "    call = r.Reserver.call\n"
                "    def crash(self, method, params):\n"
                "        if method == 'thread/name/set': os._exit(9)\n"
                "        return call(self, method, params)\n"
                "    r.Reserver.call = crash\n"
                "raise SystemExit(r.main(sys.argv))\n") % (str(RESERVE.parent), str(path), inject)]
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": self.env["HOME"],
               "CODEX_HOME": self.env["CODEX_HOME"]}
        result = subprocess.run(argv, cwd=folder, env=env, input=json.dumps(request), text=True,
                                capture_output=True, timeout=60)
        return result.returncode, (json.loads(result.stdout) if result.stdout.strip() else None), load(path)

    def tui(self, folder, session, seconds=12):
        """Exact `codex resume <UUID>`, no prompt: no turn, so no provider call."""
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
        try:
            child = subprocess.Popen([self.codex, "resume", session, "-C", str(folder), "--no-alt-screen"],
                                     cwd=folder, env=self.env, stdin=slave, stdout=slave, stderr=slave,
                                     start_new_session=True)
        finally:
            os.close(slave)
        self.children.append((child, master))
        log = bytearray()
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and child.poll() is None:
            ready, _, _ = select.select([master], [], [], 0.25)
            if ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                log += data
            if b"state=idle" in log:
                break
        text = " ".join(bytes(log).decode("utf-8", "replace").split())
        return child, text, b"state=idle" in log

    def stop(self, child):
        if child.poll() is None:
            os.killpg(child.pid, 9)
        child.wait(timeout=10)

    def probe(self):
        beta, alpha = self.root / "beta", self.root / "alpha"
        evidence = {}
        # 1. Baseline: a bare zero-turn legacy thread/start UUID is not attachable.
        with Mutating(self.home) as daemon:
            assert daemon.pid == self.daemon["pid"]
            bare = daemon.call("thread/start", {"cwd": str(beta), "historyMode": "legacy",
                                                "ephemeral": False})["thread"]
        assert not Path(bare["path"]).exists(), "0.166 now materializes zero-turn rollouts; requalify"
        child, text, _ = self.tui(beta, bare["id"])
        child.wait(timeout=20)
        assert child.returncode != 0 and "no rollout found" in text, text[-600:]
        evidence["bare_thread_start_attachable"] = False
        # 2a. Crash BEFORE the UUID commit: a lost response leaves an orphan.
        before = self.loaded()
        code, _, record = self.reserve(beta, inject="before_commit")
        assert code == 9 and record["state"] == "reserving" and "session" not in record
        orphans = self.loaded() - before
        assert len(orphans) <= 1, orphans  # Any native thread is unknown to us.
        code, answer, record = self.reserve(beta)
        assert code == 0 and answer["state"] == "verified", answer
        assert answer["session"] not in orphans and record["kept"][-1]["session"] is None
        evidence.update(lost_response_orphan_adopted=False, lost_response_loaded_orphans=len(orphans))
        # 2b. Crash AFTER the UUID commit: continue with that exact UUID.
        code, _, record = self.reserve(beta, inject="after_commit", current=answer["session"])
        assert code == 9 and record["state"] == "reserved"
        saved = record["session"]
        code, answer, record = self.reserve(beta, current=None)
        assert code == 0 and answer["session"] == saved and answer["adopted"], answer
        evidence["committed_uuid_recovered_exactly"] = True
        # 3. The Rust operator control, end to end through the built binary.
        result = self.cli("attended.codex.session", {"ship": "alpha"}, home=alpha / ".privateer")
        session = result["session"]
        assert result["reservation"]["state"] == "verified" and result["launch"] == "next_launch", result
        shown = self.cli("ship.show", {"name": "alpha"}, home=alpha / ".privateer")
        assert shown["first_mate"]["codex_session"] == session
        thread = self.read(session)
        assert thread["turns"] == [] and thread["historyMode"] == "legacy" and not thread["ephemeral"]
        rollout = Path(thread["path"])
        assert [json.loads(line)["type"] for line in rollout.read_text().splitlines()] == ["session_meta"]
        # 4. Exact resume attaches: idle, zero turns, zero provider requests.
        child, text, idle = self.tui(alpha, session)
        try:
            assert child.poll() is None and idle, text[-600:]
            thread = self.read(session)
            assert thread["turns"] == [] and thread["status"] == {"type": "idle"}
            applied = [json.loads(line)["payload"] for line in rollout.read_text().splitlines()
                       if json.loads(line).get("type") == "event_msg"]
            settings = [p.get("thread_settings", {}) for p in applied if p.get("type") == "thread_settings_applied"]
            assert all(p.get("type") == "thread_settings_applied" for p in applied), applied
            evidence["attach"] = {"exit": child.poll(), "turns": len(thread["turns"]),
                                  "applied_model": settings[-1].get("model") if settings else None,
                                  "thread_model": thread.get("model"),
                                  "thread_effort": thread.get("reasoningEffort")}
        finally:
            self.stop(child)
        self.check_calls()
        evidence.update(session=session, provider_requests=len(self.calls))
        return evidence


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--codex", required=True)
    args = parser.parse_args()
    fixture = Fixture(args.binary, args.codex)
    try:
        fixture.setup()
        evidence = fixture.probe()
        evidence.update(cli_version=fixture.daemon.get("cliVersion"),
                        daemon_version=fixture.daemon.get("appServerVersion"))
    finally:
        fixture.close()
    print(json.dumps(evidence, sort_keys=True))


if __name__ == "__main__":
    main()
