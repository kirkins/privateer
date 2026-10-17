#!/usr/bin/env python3
"""Headless Claude terminal facts across the real native launcher subprocesses."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = "4fa4f4bc-5b2d-4553-8ae1-eb09e501ea63"
OTHER = "4fa4f4bc-5b2d-4553-8ae1-eb09e501ea64"
FAKE = r'''#!/usr/bin/env python3
import os,sys
from pathlib import Path
sys.stdout.buffer.write(Path(os.environ['CLAUDE_TEST_STREAM']).read_bytes());sys.stdout.buffer.flush()
raise SystemExit(int(os.environ['CLAUDE_TEST_EXIT']))
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

    def run_stream(self, events, *, code=0, raw=None, resume=None):
        stream = self.root / "stream.jsonl"
        stream.write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in events))
        flags = ["--print", "--verbose", "--output-format", "stream-json"]
        if resume:
            flags += ["--resume", resume]
        run = subprocess.run([sys.executable, str(ROOT / "plugins/native_launch.py"), "claude", "claude",
                              *flags, "--", "prompt"], cwd=self.root, capture_output=True, timeout=5,
                             env={**os.environ, "PATH":str(self.root)+os.pathsep+os.environ["PATH"],
                                  "CLAUDE_TEST_STREAM":str(stream), "CLAUDE_TEST_EXIT":str(code)})
        return run, json.loads(run.stdout.splitlines()[-1])

    def test_a_completed_run_reports_its_session_and_normalized_usage(self):
        run, summary = self.run_stream([HOOK, INIT, {"type":"assistant", "session_id":IDENTITY}, LIMIT, RESULT],
                                       resume=IDENTITY)
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual((summary["type"], summary["session_id"]), ("turn.completed", IDENTITY))
        self.assertEqual(summary["usage"], {"input":2, "output":67, "cacheRead":14166,
                                            "cacheWrite":10049, "totalTokens":24284})

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

    def test_truncated_and_malformed_streams_fail(self):
        good = b"".join((json.dumps(e)+"\n").encode() for e in [INIT, RESULT])
        for raw, reason in ((good[:-1], "truncated-stream"), (good+b"not-json\n", "invalid-json")):
            run, summary = self.run_stream([], raw=raw)
            self.assertEqual(run.returncode, 2)
            self.assertEqual(summary["reason"], reason)


if __name__ == "__main__":
    unittest.main()
