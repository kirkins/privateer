#!/usr/bin/env python3
"""Native process/event regressions for Pi's zero-exit JSON failure behavior."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
HEADER = {"type": "session", "id": "fixture-pi-session"}
SETTLED = {"type": "agent_settled", "aborted": False}


def assistant(stop="stop", usage=None):
    return {"type": "message_end", "message": {"role": "assistant", "stopReason": stop,
            "usage": usage or {"input": 4, "output": 2}}}


class PiTurn(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-pi-turn-")
        self.root = Path(self.temp.name).resolve()
        self.record = self.root / "argv.json"
        self.native = self.root / "pi"
        self.native.write_text("""#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
Path(os.environ['PI_TEST_ARGV']).write_text(json.dumps({'argv':sys.argv[1:],'role':os.getenv('PRIVATEER_ROLE'),'route':os.getenv('PI_TEST_ROUTE'),'foreign':os.getenv('CLAUDECODE')}))
sys.stdout.buffer.write(Path(os.environ['PI_TEST_STREAM']).read_bytes())
sys.stdout.buffer.flush()
raise SystemExit(int(os.environ['PI_TEST_EXIT']))
""")
        self.native.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def run_stream(self, events, code=0, raw=None, resume=None):
        stream = self.root / "stream.jsonl"
        stream.write_bytes(raw if raw is not None else b"".join((json.dumps(e)+"\n").encode() for e in events))
        run = subprocess.run([sys.executable, str(ROOT / "plugins/native_launch.py"), "pi", "pi",
                              "--print", "--mode", "json", *(["--session", resume] if resume else []),
                              "--", "Literal $(touch forbidden)"],
                             capture_output=True, timeout=5,
                             env={**os.environ, "PATH": str(self.root)+os.pathsep+os.environ["PATH"],
                                  "PI_TEST_STREAM": str(stream), "PI_TEST_ARGV": str(self.record),
                                  "PI_TEST_EXIT": str(code), "PRIVATEER_ROLE": "agent",
                                  "PI_TEST_ROUTE": "unchanged", "CLAUDECODE": "foreign"})
        return run

    def summary(self, run):
        return json.loads(run.stdout.splitlines()[-1])

    def test_error_retries_then_zero_exit_do_not_become_success(self):
        run = self.run_stream([HEADER, {"type":"agent_start"}, assistant("error"),
                               {"type":"agent_end","willRetry":True},
                               {"type":"auto_retry_start"}, assistant("error"), SETTLED])
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual(self.summary(run)["type"], "native.turn.failed")
        self.assertEqual(self.summary(run)["native_exit_code"], 0)

    def test_recovery_waits_for_settled_and_counts_only_authoritative_usage(self):
        run = self.run_stream([HEADER, {"type":"agent_start"}, assistant("error", {"input":1}),
            {"type":"agent_end","willRetry":True}, {"type":"auto_retry_start"},
            {"type":"agent_start"}, {"type":"message_update","usage":{"input":100}},
            assistant("toolUse", {"input":7,"output":3}),
            {"type":"turn_end","message":{"role":"assistant","usage":{"input":7}}},
            {"type":"message_end","message":{"role":"toolResult","usage":{"input":2}}},
            assistant("stop", {"input":5,"output":2}),
            {"type":"agent_end","messages":[{"usage":{"input":5}}]}, SETTLED])
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self.summary(run)["usage"], {"input":15,"output":5})
        self.assertEqual(self.summary(run)["type"], "turn.completed")
        record = json.loads(self.record.read_text())
        self.assertEqual(record['argv'][-1], 'Literal $(touch forbidden)')
        self.assertEqual(record['role'], 'agent')
        self.assertEqual(record['route'], 'unchanged')
        self.assertIsNone(record['foreign'])

    def test_repeated_message_updates_are_thinned_but_final_reply_and_usage_survive(self):
        snapshot = {"type": "message_update", "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "streaming snapshot " + "x" * 1800}]}}
        events = [HEADER, *([snapshot] * 800),
                  {"type": "message_end", "message": {"role": "assistant", "stopReason": "stop",
                   "content": [{"type": "text", "text": "final reply survives"}],
                   "usage": {"input": 42, "output": 7}}}, SETTLED]
        run = self.run_stream(events)
        self.assertEqual(run.returncode, 0, run.stderr)
        stream_bytes = (self.root / "stream.jsonl").stat().st_size
        self.assertGreater(stream_bytes, 1024 * 1024)
        self.assertLess(len(run.stdout), 1024 * 1024)
        lines = [json.loads(line) for line in run.stdout.splitlines()]
        self.assertFalse(any(event.get("type") == "message_update" for event in lines))
        final = next(event for event in lines if event.get("type") == "message_end")
        self.assertEqual(final["message"]["content"][0]["text"], "final reply survives")
        self.assertIn({"type": "agent_settled", "aborted": False}, lines)
        summary = lines[-1]
        self.assertEqual(summary["session_id"], HEADER["id"])
        self.assertEqual(summary["usage"], {"input": 42, "output": 7})
        self.assertEqual(summary["message_updates_omitted"], 800)

    def test_partial_aborted_and_unsettled_successes_stay_unconfirmed(self):
        for tail in ([assistant()], [assistant("length"),SETTLED],
                     [assistant("toolUse"),SETTLED], [assistant(),{"type":"agent_settled","aborted":True}],
                     [assistant(),SETTLED,{"type":"agent_start"}],
                     [assistant(),SETTLED,{"type":"tool_execution_start"}]):
            with self.subTest(tail=tail):
                run = self.run_stream([HEADER,*tail])
                self.assertNotEqual(run.returncode, 0)

    def test_native_process_failure_and_missing_or_changed_identity_fail(self):
        for events, code in [([HEADER,assistant(),SETTLED],7),
                             ([assistant(),SETTLED],0),
                             ([HEADER,{"type":"session","id":"different"},assistant(),SETTLED],0)]:
            run = self.run_stream(events, code)
            self.assertNotEqual(run.returncode, 0)
            if code:
                self.assertEqual(self.summary(run)["native_exit_code"],code)

    def test_explicit_resume_must_match_the_id_or_existing_session_file(self):
        identity='46a81234-1234-4567-8123-123456789abc'
        events=[{'type':'session','id':identity},assistant(),SETTLED]
        self.assertEqual(self.run_stream(events,resume=identity).returncode,0)
        self.assertNotEqual(self.run_stream([HEADER,assistant(),SETTLED],resume=identity).returncode,0)
        saved=self.root/'session.jsonl'
        saved.write_text(json.dumps(events[0])+'\n')
        self.assertEqual(self.run_stream(events,resume=str(saved)).returncode,0)
        self.assertNotEqual(self.run_stream([HEADER,assistant(),SETTLED],resume=str(saved)).returncode,0)
        self.record.unlink()
        saved.unlink()
        self.assertNotEqual(self.run_stream(events,resume=str(saved)).returncode,0)
        self.assertFalse(self.record.exists(),'missing explicit session must fail before native launch')

    def test_truncated_malformed_and_oversized_frames_cannot_claim_completion(self):
        valid = b"".join((json.dumps(e)+"\n").encode() for e in [HEADER,assistant(),SETTLED])
        for raw in (valid[:-1], valid+b"not-json\n", valid+b"x"*(1024*1024+1)):
            with self.subTest(size=len(raw)):
                run = self.run_stream([],raw=raw)
                self.assertNotEqual(run.returncode, 0)
                self.assertEqual(self.summary(run)["type"], "native.turn.failed")

    def test_unicode_line_separators_are_content_not_frame_boundaries(self):
        event = assistant()
        event['message']['content']=[{'type':'text','text':'a\u2028b\u2029c'}]
        raw = b"".join((json.dumps(e,ensure_ascii=False)+"\n").encode() for e in [HEADER,event,SETTLED])
        run = self.run_stream([],raw=raw)
        self.assertEqual(run.returncode, 0, run.stderr)


if __name__ == '__main__':
    unittest.main()
