#!/usr/bin/env python3
"""The live voice probe (plugins/live-voice/spike/probe.py) against a fake
`codex app-server` (tests/fake_codex_app_server.py) over real stdio pipes.

Covers the go path, a refused realtime start (401) and a session that errors
and closes (both no-go), the API key refusal that never calls realtime, and a
silent server that ignores EOF and SIGTERM and must be killed with its helper,
and a helper left behind by a server that exits cleanly.
No network, no real Codex.
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
import wave

ROOT = pathlib.Path(__file__).resolve().parent.parent
PROBE = ROOT / "plugins" / "live-voice" / "spike" / "probe.py"
FAKE = ROOT / "tests" / "fake_codex_app_server.py"


class ProbeTest(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.dir = pathlib.Path(self.scratch.name)
        self.log = self.dir / "fake.jsonl"
        # The probe execs `--codex PATH` directly, as it would the real
        # binary; a wrapper pins the fake to this interpreter.
        self.codex = self.dir / "codex"
        self.codex.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{FAKE}" "$@"\n')
        self.codex.chmod(0o755)

    def tearDown(self):
        self.scratch.cleanup()

    def probe(self, scenario, *extra, timeout=60):
        env = dict(os.environ, FAKE_CODEX_SCENARIO=scenario, FAKE_CODEX_LOG=str(self.log),
                   OPENAI_API_KEY="sk-test-must-not-pass", CODEX_API_KEY="sk-test-must-not-pass")
        done = subprocess.run(
            [sys.executable, str(PROBE), "--codex", str(self.codex), *extra],
            capture_output=True, text=True, env=env, timeout=timeout)
        lines = done.stdout.strip().splitlines()
        self.assertEqual(len(lines), 1, done.stdout + done.stderr)
        self.assertNotIn("sk-", done.stdout)
        self.assertNotIn("eyJ", done.stdout)
        self.assertNotIn("someone@example.com", done.stdout)
        return done.returncode, json.loads(lines[0])

    def sent(self):
        entries = [json.loads(line) for line in self.log.read_text().splitlines()]
        return [entry for entry in entries if "method" in entry or "error" in entry]

    def methods(self):
        return [entry["method"] for entry in self.sent() if entry.get("method")]

    def assertHelperGone(self):
        helper = next(json.loads(line)["helper"] for line in self.log.read_text().splitlines()
                      if '"helper"' in line)
        with self.assertRaises(ProcessLookupError):
            os.kill(helper, 0)

    def assertNoKeyPassed(self):
        starts = [json.loads(line) for line in self.log.read_text().splitlines()
                  if '"start"' in line]
        self.assertEqual(len(starts), 1)
        self.assertFalse(starts[0]["api_key_in_env"])

    def test_go_collects_audio_and_transcript_and_declines_server_requests(self):
        wav = self.dir / "answer.wav"
        code, report = self.probe("go", "--save-audio", str(wav), "--settle", "0.5")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "go", report)
        self.assertEqual(report["codex_version"], "codex-cli 0.0.0-fake")
        self.assertEqual(report["auth_mode"], "chatgpt")
        self.assertEqual(report["plan_type"], "plus")
        self.assertEqual(report["audio_bytes"], 3 * 960)
        self.assertEqual(report["audio_format"],
                         [{"sample_rate": 24000, "channels": 1, "chunks": 3}])
        self.assertEqual(report["transcript"], "Ready.")
        self.assertEqual(report["events_seen"]["thread/realtime/outputAudio/delta"], 3)
        self.assertEqual(report["events_seen"]["thread/realtime/started"], 1)
        self.assertEqual(report["realtime_item_types"],
                         {"thread/realtime/itemAdded:response.created": 1})
        self.assertEqual(report["codex_turns_interrupted"], 1)
        # The server exited on EOF; the helper it left behind was swept.
        self.assertEqual(report["shutdown"], "clean")
        self.assertTrue(report["process_group_gone"])
        self.assertHelperGone()
        self.assertEqual([step["method"] for step in report["steps"]], [
            "initialize", "account/read", "thread/start", "thread/realtime/start",
            "thread/realtime/appendText", "thread/realtime/stop"])
        self.assertTrue(all(step["ok"] for step in report["steps"]), report["steps"])
        self.assertNoKeyPassed()

        sent = self.sent()
        initialize = next(e for e in sent if e.get("method") == "initialize")
        self.assertTrue(initialize["params"]["capabilities"]["experimentalApi"])
        self.assertIn("initialized", self.methods())
        start = next(e for e in sent if e.get("method") == "thread/realtime/start")["params"]
        self.assertEqual(start["model"], "gpt-live-1-codex")
        self.assertEqual(start["version"], "v3")
        self.assertEqual(start["outputModality"], "audio")
        self.assertEqual(start["transport"], {"type": "websocket"})
        text = next(e for e in sent if e.get("method") == "thread/realtime/appendText")
        self.assertEqual(text["params"], {"threadId": "thr_fake",
                                          "text": "Say the word ready.", "role": "user"})
        # The approval request was declined, not approved.
        answer = next(e for e in sent if e.get("id") == "srv-1")
        self.assertIsNotNone(answer["error"])
        self.assertIn("turn/interrupt", self.methods())

        with wave.open(str(wav)) as audio:
            self.assertEqual(audio.getframerate(), 24000)
            self.assertEqual(audio.getnchannels(), 1)
            self.assertEqual(audio.getsampwidth(), 2)
            self.assertEqual(audio.getnframes(), 3 * 480)

    def test_audio_is_not_saved_unless_asked(self):
        code, report = self.probe("go", "--settle", "0.5")
        self.assertEqual(report["verdict"], "go", report)
        self.assertNotIn("saved_audio", report)
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["codex", "fake.jsonl"])

    def test_a_refused_realtime_start_is_a_no_go_with_the_token_redacted(self):
        code, report = self.probe("unauthorized")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "no-go", report)
        failed = report["steps"][-1]
        self.assertEqual(failed["method"], "thread/realtime/start")
        self.assertFalse(failed["ok"])
        self.assertEqual(failed["error"]["code"], -32603)
        self.assertIn("401", failed["error"]["message"])
        self.assertIn("[redacted]", failed["error"]["message"])
        self.assertEqual(report["audio_bytes"], 0)
        self.assertNotIn("thread/realtime/appendText", self.methods())
        self.assertEqual(report["shutdown"], "clean")
        self.assertNoKeyPassed()

    def test_a_session_that_errors_and_closes_is_a_no_go(self):
        code, report = self.probe("closed")
        self.assertEqual(report["verdict"], "no-go", report)
        self.assertIn("401 no_matching_rule", report["realtime_errors"])
        self.assertEqual(report["events_seen"]["thread/realtime/closed"], 1)
        self.assertEqual(report["audio_bytes"], 0)

    def test_an_api_key_login_is_refused_before_any_thread_or_realtime_call(self):
        code, report = self.probe("apikey")
        self.assertEqual(code, 1, report)
        self.assertEqual(report["verdict"], "error", report)
        self.assertEqual(report["auth_mode"], "apiKey")
        self.assertIn("refused", report["reason"])
        methods = self.methods()
        self.assertEqual(methods, ["initialize", "initialized", "account/read"])
        self.assertFalse(any(m.startswith("thread/") for m in methods))
        self.assertNoKeyPassed()

    def test_silence_times_out_and_a_stubborn_server_is_killed_with_its_helper(self):
        code, report = self.probe("hang", "--timeout", "1", "--grace", "0.5",
                                  "--rpc-timeout", "2")
        self.assertEqual(code, 1, report)
        self.assertEqual(report["verdict"], "error", report)
        self.assertIn("no audio within 1s", report["reason"])
        self.assertEqual(report["shutdown"], "killed")
        self.assertTrue(report["process_group_gone"])
        self.assertHelperGone()

    def test_a_missing_codex_is_an_error_not_a_verdict(self):
        env = dict(os.environ, PATH=str(self.dir / "empty"))
        done = subprocess.run([sys.executable, str(PROBE)], capture_output=True,
                              text=True, env=env, timeout=30)
        report = json.loads(done.stdout)
        self.assertEqual(done.returncode, 1)
        self.assertEqual(report["verdict"], "error")
        self.assertIn("codex not found", report["reason"])


if __name__ == "__main__":
    unittest.main()
