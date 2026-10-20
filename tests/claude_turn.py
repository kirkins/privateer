#!/usr/bin/env python3
"""Headless Claude terminal facts across the real native launcher subprocesses."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = "4fa4f4bc-5b2d-4553-8ae1-eb09e501ea63"
OTHER = "4fa4f4bc-5b2d-4553-8ae1-eb09e501ea64"
FAKE = r'''#!/usr/bin/env python3
import os,sys,time,subprocess
from pathlib import Path
pid_file = os.environ.get('CLAUDE_TEST_PID')
if pid_file:
    Path(pid_file).write_text(str(os.getpid()))
grandchild_file = os.environ.get('CLAUDE_TEST_GRANDCHILD_PID')
if grandchild_file:
    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    Path(grandchild_file).write_text(str(proc.pid))
mode = os.environ.get('CLAUDE_TEST_MODE')
if mode == 'sleep':
    time.sleep(60)
stream_path = os.environ.get('CLAUDE_TEST_STREAM')
if stream_path and Path(stream_path).is_file():
    sys.stdout.buffer.write(Path(stream_path).read_bytes())
    sys.stdout.buffer.flush()
if mode == 'eof':
    os.close(1)
    time.sleep(60)
if mode == 'stall':
    time.sleep(60)
raise SystemExit(int(os.environ.get('CLAUDE_TEST_EXIT', '0')))
'''
# Runs claude_turn with a slowed reader or with macOS's killpg, which refuses
# (EPERM) a process group whose members have all exited but are not yet reaped.
DRIVER = r'''
import errno,os,sys,time
from pathlib import Path
sys.path.insert(0, sys.argv.pop(1))
import claude_turn
delay = float(os.environ.get('CLAUDE_TEST_READER_DELAY', '0'))
if delay:
    observe = claude_turn.ClaudeTurn.observe
    def slow(self, value):
        time.sleep(delay)
        return observe(self, value)
    claude_turn.ClaudeTurn.observe = slow
if os.environ.get('CLAUDE_TEST_DARWIN_KILLPG'):
    killpg = os.killpg
    def darwin_killpg(pgid, signum):
        try:
            state = Path(f'/proc/{pgid}/stat').read_text().rsplit(')', 1)[1].split()[0]
        except OSError:
            state = None
        if state == 'Z':
            raise PermissionError(errno.EPERM, os.strerror(errno.EPERM))
        return killpg(pgid, signum)
    os.killpg = darwin_killpg
claude_turn.main(sys.argv)
'''
# Event shapes captured from Claude Code 2.1.300: hooks report before init.
HOOK = {"type":"system", "subtype":"hook_response", "session_id":IDENTITY, "exit_code":0}
INIT = {"type":"system", "subtype":"init", "session_id":IDENTITY, "model":"claude-haiku-5-5"}
LIMIT = {"type":"rate_limit_event", "session_id":IDENTITY,
         "rate_limit_info":{"status":"allowed", "resetsAt":1792214400, "rateLimitType":"five_hour"}}
RESULT = {"type":"result", "subtype":"success", "is_error":False, "session_id":IDENTITY, "result":"ok",
          "usage":{"input_tokens":2, "output_tokens":67, "cache_read_input_tokens":14166,
                   "cache_creation_input_tokens":10049, "server_tool_use":{"web_search_requests":0}}}


class ClaudeTurn(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-claude-turn-")
        self.root = Path(self.temp.name).resolve()
        path = self.root / "claude"
        path.write_text(FAKE)
        path.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def run_stream(self, events, *, code=0, raw=None, resume=None, env_extra=None, driver=False):
        stream = self.root / "stream.jsonl"
        stream.write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in events))
        flags = ["--print", "--verbose", "--output-format", "stream-json"]
        if resume:
            flags += ["--resume", resume]
        pid_file = self.root / "pid"
        run_env = {
            **os.environ,
            "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
            "CLAUDE_TEST_STREAM": str(stream),
            "CLAUDE_TEST_EXIT": str(code),
            "CLAUDE_TEST_PID": str(pid_file),
            **(env_extra or {}),
        }
        if driver:
            script = self.root / "driver.py"
            script.write_text(DRIVER)
            command = [sys.executable, str(script), str(ROOT / "plugins"), "--control", "{}",
                       str(self.root / "claude")]
        else:
            command = [sys.executable, str(ROOT / "plugins/native_launch.py"), "claude", "claude"]
        run = subprocess.run([*command, *flags, "--", "prompt"], cwd=self.root, capture_output=True,
                             timeout=10, env=run_env)
        return run, json.loads(run.stdout.splitlines()[-1])

    def test_a_run_without_output_ends_at_startup_bound(self):
        started = time.monotonic()
        run, summary = self.run_stream([], env_extra={"PRIVATEER_CLAUDE_STARTUP_SECS": "0.3",
                                                      "CLAUDE_TEST_MODE": "sleep"})
        elapsed = time.monotonic() - started
        self.assertEqual(run.returncode, 2)
        self.assertLess(elapsed, 4.0)
        self.assertEqual(summary["type"], "native.turn.failed")
        self.assertEqual(summary["reason"], "no-output")
        pid_file = self.root / "pid"
        self.assertTrue(pid_file.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)

    def test_a_stream_that_prints_then_stalls_ends_at_idle_bound(self):
        started = time.monotonic()
        run, summary = self.run_stream([HOOK, INIT], env_extra={"PRIVATEER_CLAUDE_IDLE_SECS": "0.3",
                                                                "CLAUDE_TEST_MODE": "stall"})
        elapsed = time.monotonic() - started
        self.assertEqual(run.returncode, 2)
        self.assertLess(elapsed, 4.0)
        self.assertEqual(summary["type"], "native.turn.failed")
        self.assertEqual(summary["reason"], "stalled")
        self.assertEqual(summary["session_id"], IDENTITY)
        pid_file = self.root / "pid"
        self.assertTrue(pid_file.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)

    def test_stall_terminates_entire_process_group_including_grandchildren(self):
        grandchild_pid_file = self.root / "grandchild_pid"
        run, summary = self.run_stream([HOOK, INIT], env_extra={
            "PRIVATEER_CLAUDE_IDLE_SECS": "0.3",
            "CLAUDE_TEST_MODE": "stall",
            "CLAUDE_TEST_GRANDCHILD_PID": str(grandchild_pid_file),
        })
        self.assertEqual(run.returncode, 2)
        self.assertEqual(summary["type"], "native.turn.failed")
        self.assertEqual(summary["reason"], "stalled")
        pid_file = self.root / "pid"
        self.assertTrue(pid_file.exists())
        self.assertTrue(grandchild_pid_file.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)
        with self.assertRaises(ProcessLookupError):
            os.kill(int(grandchild_pid_file.read_text()), 0)

    def test_stream_eof_with_the_child_alive_is_bounded_and_leaves_no_process(self):
        # Claude closes stdout (EOF) but keeps running: the launcher must not
        # wait on it forever. It bounds the wait, stops the group, and names the
        # fault as stream-eof.
        started = time.monotonic()
        run, summary = self.run_stream([], env_extra={"CLAUDE_TEST_MODE": "eof"})
        elapsed = time.monotonic() - started
        self.assertEqual(run.returncode, 2)
        self.assertEqual(summary["type"], "native.turn.failed")
        self.assertEqual(summary["reason"], "stream-eof")
        self.assertLess(elapsed, 9.0)
        pid_file = self.root / "pid"
        self.assertTrue(pid_file.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(pid_file.read_text()), 0)

    def test_a_grandchild_holding_stdout_after_the_leader_exits_is_stopped(self):
        # The leader exits after a clean run but a grandchild inherited its
        # stdout. The launcher must not block on that pipe; it stops the group,
        # so the grandchild is gone and the completed run still settles.
        grandchild_pid_file = self.root / "grandchild_pid"
        started = time.monotonic()
        run, summary = self.run_stream([INIT, RESULT], env_extra={
            "CLAUDE_TEST_GRANDCHILD_PID": str(grandchild_pid_file)})
        elapsed = time.monotonic() - started
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual(summary["type"], "turn.completed")
        self.assertLess(elapsed, 9.0)
        self.assertTrue(grandchild_pid_file.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(int(grandchild_pid_file.read_text()), 0)

    def test_resolution_prefers_real_path_binary_over_shims_and_mise(self):
        sys.path.insert(0, str(ROOT / "plugins"))
        from claude_turn import resolve_claude

        shim_dir = self.root / "shims"
        shim_dir.mkdir()
        shim_bin = shim_dir / "claude"
        shim_bin.write_text("#!/bin/bash\nexec mise x claude -- \"$@\"\n")
        shim_bin.chmod(0o755)

        symlink_shim_dir = self.root / "symlink_shims"
        symlink_shim_dir.mkdir()
        symlink_shim = symlink_shim_dir / "claude"
        symlink_shim.symlink_to(shim_bin)

        installer_dir = self.root / "local_bin"
        installer_dir.mkdir()
        version_dir = self.root / "claude_versions" / "1.0.0"
        version_dir.mkdir(parents=True)
        version_bin = version_dir / "claude"
        version_bin.write_text("#!/bin/sh\nexit 0\n")
        version_bin.chmod(0o755)
        path_claude = installer_dir / "claude"
        path_claude.symlink_to(version_bin)

        mise_root = self.root / "mise"
        install_bin = mise_root / "installs/claude/latest/claude"
        install_bin.parent.mkdir(parents=True)
        install_bin.write_text("#!/bin/sh\nexit 0\n")
        install_bin.chmod(0o755)

        # 1. Real PATH binary wins over earlier shims and mise install
        path_str = os.pathsep.join([str(shim_dir), str(symlink_shim_dir), str(installer_dir)])
        env = {"MISE_DATA_DIR": str(mise_root), "PATH": path_str}
        self.assertEqual(resolve_claude("claude", env=env), str(path_claude))

        # 2. When real binary is gone, mise install wins over shims on PATH
        path_claude.unlink()
        self.assertEqual(resolve_claude("claude", env=env), str(install_bin))

        # 3. When mise install is gone, falls back to first shim on PATH
        install_bin.unlink()
        self.assertEqual(resolve_claude("claude", env=env), str(shim_bin))

    def test_idle_bound_respects_bash_max_timeout(self):
        sys.path.insert(0, str(ROOT / "plugins"))
        from claude_turn import idle_bound
        with mock.patch.dict(os.environ, {"BASH_MAX_TIMEOUT_MS": "600000", "PRIVATEER_CLAUDE_IDLE_SECS": "100"}):
            self.assertEqual(idle_bound(), 1200.0)
        with mock.patch.dict(os.environ, {"BASH_MAX_TIMEOUT_MS": "900000", "PRIVATEER_CLAUDE_IDLE_SECS": "100"}):
            self.assertEqual(idle_bound(), 1800.0)

    def test_a_completed_run_reports_its_session_and_normalized_usage(self):
        run, summary = self.run_stream([HOOK, INIT, {"type":"assistant", "session_id":IDENTITY}, LIMIT, RESULT],
                                       resume=IDENTITY)
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual((summary["type"], summary["session_id"]), ("turn.completed", IDENTITY))
        self.assertEqual(summary["usage"], {"input":2, "output":67, "cacheRead":14166,
                                            "cacheWrite":10049, "totalTokens":24284})

    def test_a_rising_second_result_settles_the_run_with_the_last_result(self):
        waiting = {**RESULT, "result":"Waiting for the test job to finish.", "result_index":0,
                   "usage":{"input_tokens":2, "output_tokens":20, "cache_read_input_tokens":100,
                            "cache_creation_input_tokens":10}}
        finished = {**RESULT, "result":"The job finished and the work is done.", "result_index":1,
                    "usage":{"input_tokens":3, "output_tokens":5, "cache_read_input_tokens":200,
                             "cache_creation_input_tokens":20}}
        run, summary = self.run_stream([HOOK, INIT, waiting, finished])
        self.assertEqual((run.returncode, summary["type"]), (0, "turn.completed"), run.stdout)
        self.assertEqual(summary["session_id"], IDENTITY)
        self.assertEqual(summary["usage"], {"input":5, "output":25, "cacheRead":300,
                                            "cacheWrite":30, "totalTokens":360})
        # The last result, not an earlier provisional one, decides the run.
        run, summary = self.run_stream([INIT, {**waiting, "is_error":True, "subtype":"error_max_turns"},
                                        finished])
        self.assertEqual((run.returncode, summary["type"]), (0, "turn.completed"), run.stdout)
        run, summary = self.run_stream([INIT, waiting, {**finished, "is_error":True, "subtype":"error_during_execution"}])
        self.assertEqual((run.returncode, summary["type"]), (2, "native.turn.failed"), run.stdout)

    def test_permission_denials_from_every_result_are_kept(self):
        waiting = {**RESULT, "result_index":0, "permission_denials":[
            {"tool_name":"Bash", "tool_input":{"command":"curl -sS https://one.example/"}}]}
        finished = {**RESULT, "result_index":1, "permission_denials":[
            {"tool_name":"Write", "tool_input":{"file_path":"/x/two"}}]}
        run, line = self.run_stream([INIT, waiting, finished])
        self.assertEqual(run.returncode, 2, run.stdout)
        self.assertEqual(line["type"], "native.permission_blocked")
        self.assertEqual([d["tool_name"] for d in line["denials"]], ["Bash", "Write"])
        self.assertEqual(json.loads(run.stdout.splitlines()[-2])["type"], "native.turn.failed")

    def test_a_repeated_result_without_a_rising_index_still_fails(self):
        first = {**RESULT, "result_index":0}
        for second in ({**RESULT, "result_index":0}, {**RESULT, "result_index":-1}, RESULT,
                       {**RESULT, "result_index":"1"}):
            with self.subTest(second=second):
                run, summary = self.run_stream([INIT, first, second])
                self.assertEqual((run.returncode, summary["type"]), (2, "native.turn.failed"), run.stdout)
                self.assertEqual(summary["reason"], "repeated-result")
        run, summary = self.run_stream([INIT, {**RESULT, "result_index":0},
                                        {**RESULT, "result_index":1, "session_id":OTHER}])
        self.assertEqual((run.returncode, summary["reason"]), (2, "changed-session"))

    def test_exit_zero_cannot_hide_a_failed_missing_or_foreign_result(self):
        failed = {**RESULT, "is_error":True, "result":"API Error: 529 overloaded"}
        for events in ([INIT, failed], [INIT], [RESULT], [INIT, RESULT, RESULT],
                        [INIT, {**RESULT, "session_id":OTHER}], [INIT, {**RESULT, "subtype":"error_max_turns"}]):
            with self.subTest(events=events):
                run, summary = self.run_stream(events)
                self.assertEqual(run.returncode, 2)
                self.assertEqual(summary["type"], "native.turn.failed")
        run, summary = self.run_stream([INIT, RESULT], code=1)
        self.assertEqual((run.returncode, summary["native_exit_code"]), (2, 1))

    def test_a_resume_must_continue_the_requested_session(self):
        run, summary = self.run_stream([INIT, RESULT], resume=OTHER)
        self.assertEqual(run.returncode, 2)
        self.assertEqual(summary["reason"], "resume-identity")

    def test_a_denied_run_fails_and_ends_with_a_short_denial_line(self):
        # The stream Claude Code 2.1.300 printed for a curl it was not allowed to run.
        fixture = (ROOT / "tests/fixtures/claude/denied-accept-edits.jsonl").read_bytes()
        run, line = self.run_stream([], raw=fixture)
        self.assertEqual(run.returncode, 2, run.stdout)
        self.assertEqual(line["type"], "native.permission_blocked")
        self.assertEqual(line["denials"], [{"tool_name":"Bash", "tool_input":{
            "command":"curl -sS https://example.com/ -o /dev/null",
            "description":"Fetch example.com and discard output"}}])
        summary = json.loads(run.stdout.splitlines()[-2])
        self.assertEqual(summary["type"], "native.turn.failed")
        # A denied Write of a large file is clipped, and at most four are kept.
        big = [{"tool_name":"Write", "tool_input":{"file_path":f"/x/{i}", "content":"y" * 100_000}}
               for i in range(6)]
        run, line = self.run_stream([INIT, {**RESULT, "permission_denials":big}])
        self.assertEqual(len(line["denials"]), 4)
        self.assertEqual(line["denied"], 6)
        self.assertEqual(line["denials"][0]["tool_input"]["file_path"], "/x/0")
        self.assertLess(len(run.stdout.splitlines()[-1]), 20_000)

    def test_selected_mcp_servers_must_all_connect_before_the_first_turn(self):
        # Records as the launcher sets them for a bounded run that selects servers.
        records = json.dumps([{"name":"apify", "url":"https://mcp.apify.com", "credential":"APIFY", "timeout":120}])

        def run(servers, tools=("Bash", "mcp__apify__search-actors")):
            stream = self.root / "stream.jsonl"
            stream.write_bytes(b"".join((json.dumps(e)+"\n").encode()
                                        for e in [{**INIT, "mcp_servers":servers, "tools":list(tools)}, RESULT]))
            done = subprocess.run([sys.executable, str(ROOT / "plugins/claude_turn.py"), "--control", "{}",
                                   str(self.root / "claude"), "--print", "--output-format", "stream-json", "--", "p"],
                                  capture_output=True, timeout=5,
                                  env={**os.environ, "PRIVATEER_MCP_SERVERS":records,
                                       "CLAUDE_TEST_STREAM":str(stream), "CLAUDE_TEST_EXIT":"0"})
            return done.returncode, json.loads(done.stdout.splitlines()[-1])

        code, summary = run([{"name":"apify", "status":"connected", "source":"dynamic"}])
        self.assertEqual((code, summary["type"]), (0, "turn.completed"))
        connected = [{"name":"apify", "status":"connected"}]
        for servers, tools, reason in (([{"name":"apify", "status":"failed"}], None, "mcp-unavailable:apify"),
                                       ([], None, "mcp-unavailable:apify"),
                                       (connected, ["Bash"], "mcp-unavailable:apify"),
                                       (connected + [{"name":"personal", "status":"connected"}], None,
                                        "mcp-foreign:personal"),
                                       (connected + [{"name":["x"]}], None, "mcp-foreign:unknown")):
            with self.subTest(servers=servers, tools=tools):
                code, summary = run(servers, *([tools] if tools else []))
                self.assertEqual((code, summary["type"], summary["reason"]), (2, "native.turn.failed", reason))
        # A run that selects no server keeps whatever servers it has.
        run_, summary = self.run_stream([{**INIT, "mcp_servers":[{"name":"personal", "status":"failed"}]}, RESULT])
        self.assertEqual((run_.returncode, summary["type"]), (0, "turn.completed"))

    def test_truncated_and_malformed_streams_fail(self):
        good = b"".join((json.dumps(e)+"\n").encode() for e in [INIT, RESULT])
        for raw, reason in ((good[:-1], "truncated-stream"), (good+b"not-json\n", "invalid-json")):
            run, summary = self.run_stream([], raw=raw)
            self.assertEqual(run.returncode, 2)
            self.assertEqual(summary["reason"], reason)

    def test_a_reader_reason_survives_killpg_refusing_an_exited_group(self):
        # The reader names the fault before the child is reaped; macOS then
        # refuses killpg of the exited group, which must not hide the reason.
        good = b"".join((json.dumps(e)+"\n").encode() for e in [INIT, RESULT])
        cases = ((dict(events=[INIT, RESULT], resume=OTHER), "resume-identity"),
                 (dict(events=[], raw=good[:-1]), "truncated-stream"),
                 (dict(events=[], raw=good+b"not-json\n"), "invalid-json"))
        for kwargs, reason in cases:
            with self.subTest(reason=reason):
                run, summary = self.run_stream(**kwargs, driver=True,
                                               env_extra={"CLAUDE_TEST_DARWIN_KILLPG": "1"})
                self.assertEqual((run.returncode, summary["type"]), (2, "native.turn.failed"), run.stderr)
                self.assertEqual(summary["reason"], reason)

    def test_a_reader_slower_than_the_child_still_names_its_reason(self):
        # The child exits and is reaped by the poll loop well before the
        # delayed reader records its fault; the specific reason still wins.
        for darwin in ("", "1"):
            with self.subTest(darwin=bool(darwin)):
                run, summary = self.run_stream([INIT, RESULT], resume=OTHER, driver=True, env_extra={
                    "CLAUDE_TEST_READER_DELAY": "2", "CLAUDE_TEST_DARWIN_KILLPG": darwin})
                self.assertEqual((run.returncode, summary["type"]), (2, "native.turn.failed"), run.stderr)
                self.assertEqual(summary["reason"], "resume-identity")
                self.assertEqual(summary["native_exit_code"], 0)


if __name__ == "__main__":
    unittest.main()
