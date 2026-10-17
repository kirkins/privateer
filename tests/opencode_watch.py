#!/usr/bin/env python3
"""OpenCode V1 headless runs end when a provider error stalls them."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
FAKE = r'''#!/usr/bin/env python3
import json,os,sys,time
from pathlib import Path
Path(os.environ['OC_TEST_ARGV']).write_text(json.dumps({'argv':sys.argv[1:],'pid':os.getpid()}))
log=lambda small,text:print('timestamp=2026-10-17T19:52:45Z level=ERROR run=1 message="stream error" providerID=opencode '
    'modelID=space-bunny-free session.id=ses_1 small=%s agent=build mode=primary error.error="AI_APICallError: %s"'%(small,text),file=sys.stderr,flush=True)
mode=os.environ['OC_TEST_MODE']
# OpenCode starts the step, then its model stream fails.
print(json.dumps({'type':'step_start','sessionID':'ses_1'}),flush=True)
if mode=='title':
    log('true','upstream_unavailable')
if mode in ('stall','recover'):
    log('false','Rate limit exceeded. Please try again later.')
if mode=='recover':
    # OpenCode retries quietly: the retried stream shows only in its log.
    time.sleep(0.2)
    print('timestamp=2026-10-17T19:52:46Z level=INFO run=1 message=stream providerID=opencode modelID=space-bunny-free '
          'session.id=ses_1 small=false agent=build mode=primary',file=sys.stderr,flush=True)
    time.sleep(3)
if mode=='stall':
    time.sleep(60)
time.sleep(1)
print(json.dumps({'type':'step_finish','sessionID':'ses_1'}),flush=True)
'''


class OpenCodeWatch(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="privateer-opencode-watch-")
        self.root = Path(self.temp.name).resolve()
        native = self.root / "opencode"
        native.write_text(FAKE)
        native.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def run_turn(self, mode, stall_secs="2"):
        record = self.root / "argv.json"
        argv = [sys.executable, str(ROOT / "plugins/native_launch.py"), "opencode", "opencode",
                "run", "--format", "json", "--dir", str(self.root), "--", "Literal $(touch forbidden)"]
        started = time.monotonic()
        run = subprocess.run(argv, cwd=self.root, capture_output=True, timeout=20, env={
            **os.environ, "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
            "OC_TEST_MODE": mode, "OC_TEST_ARGV": str(record), "PRIVATEER_OPENCODE_STALL_SECS": stall_secs})
        return run, json.loads(record.read_text()), time.monotonic() - started

    def events(self, run):
        return [json.loads(line) for line in run.stdout.splitlines() if line.strip()]

    def test_a_stalled_provider_error_ends_the_run_with_that_error_and_stops_opencode(self):
        run, native, elapsed = self.run_turn("stall")
        self.assertEqual(run.returncode, 2, run.stderr)
        self.assertLess(elapsed, 10)
        *passed, final = self.events(run)
        self.assertEqual(passed, [{"type": "step_start", "sessionID": "ses_1"}])
        self.assertEqual(final["type"], "error")
        self.assertEqual(final["error"]["data"]["message"], "Rate limit exceeded. Please try again later.")
        # OpenCode itself is gone, not just the watch around it.
        with self.assertRaises(ProcessLookupError):
            os.kill(native["pid"], 0)
        self.assertEqual(native["argv"][:4], ["run", "--print-logs", "--log-level", "INFO"])
        self.assertEqual(native["argv"][-1], "Literal $(touch forbidden)")
        self.assertFalse((self.root / "forbidden").exists())
        # OpenCode's log lines still reach the job's stderr.
        self.assertIn(b"Rate limit exceeded", run.stderr)

    def test_a_stream_that_recovers_or_only_fails_its_title_runs_to_the_end(self):
        for mode in ("recover", "title"):
            run, _, _ = self.run_turn(mode)
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertEqual([event["type"] for event in self.events(run)], ["step_start", "step_finish"])


if __name__ == "__main__":
    unittest.main()
