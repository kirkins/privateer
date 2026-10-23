#!/usr/bin/env python3
"""The live voice probe (plugins/live-voice/spike/probe.py) against a fake
`codex app-server` (tests/fake_codex_app_server.py) over real stdio pipes.

Covers, over websocket, the go path, a refused realtime start (401), a
session that errors and closes and the API-key-only failure (all no-go); over
webrtc (the default), a go on the SDP answer with a well-formed offer, the
API-key-only failure (no-go) and a missing answer (error); the API key
refusal that never calls realtime; a silent server that ignores EOF and
SIGTERM and must be killed with its helper; and a helper left behind by a
server that exits cleanly. No network, no real Codex.
"""

import json
import os
import pathlib
import subprocess
import sys
import importlib.util
import re
import tempfile
import unittest
import wave

ROOT = pathlib.Path(__file__).resolve().parent.parent
PROBE = ROOT / "plugins" / "live-voice" / "spike" / "probe.py"
FAKE = ROOT / "tests" / "fake_codex_app_server.py"
WEBSOCKET = ("--transport", "websocket")


def load(path):
    spec = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def parse_sdp(text):
    """A strict-enough SDP reader: CRLF lines of `x=value`, sessions then media."""
    test = unittest.TestCase()
    test.assertTrue(text.endswith("\r\n"), "SDP lines end in CRLF")
    lines = text[:-2].split("\r\n")
    test.assertEqual(lines[0], "v=0")
    session, media = [], []
    for line in lines:
        test.assertRegex(line, r"^[a-z]=\S")
        if line.startswith("m="):
            media.append([line])
        elif media:
            media[-1].append(line)
        else:
            session.append(line)
    for prefix in ("o=", "s=", "t="):
        test.assertTrue(any(line.startswith(prefix) for line in session), prefix)
    return session, media


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
        code, report = self.probe("go", *WEBSOCKET, "--save-audio", str(wav),
                                  "--settle", "0.5")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "go", report)
        self.assertEqual(report["transport"], "websocket")
        self.assertTrue(report["go_means"].startswith("audio"))
        self.assertNotIn("answer_sdp_bytes", report)
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
        code, report = self.probe("go", *WEBSOCKET, "--settle", "0.5")
        self.assertEqual(report["verdict"], "go", report)
        self.assertNotIn("saved_audio", report)
        self.assertEqual(sorted(p.name for p in self.dir.iterdir()), ["codex", "fake.jsonl"])

    def test_a_refused_realtime_start_is_a_no_go_with_the_token_redacted(self):
        code, report = self.probe("unauthorized", *WEBSOCKET)
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
        code, report = self.probe("closed", *WEBSOCKET)
        self.assertEqual(report["verdict"], "no-go", report)
        self.assertIn("401 no_matching_rule", report["realtime_errors"])
        self.assertEqual(report["events_seen"]["thread/realtime/closed"], 1)
        self.assertEqual(report["audio_bytes"], 0)

    def test_websocket_realtime_that_needs_an_api_key_is_a_no_go(self):
        # What Codex 0.162.1 did on the real websocket run.
        code, report = self.probe("needs-api-key", *WEBSOCKET)
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "no-go", report)
        self.assertIn("realtime conversation requires API key auth",
                      report["realtime_errors"])
        self.assertNotIn("thread/realtime/appendText", self.methods())

    def start_params(self):
        return next(e for e in self.sent()
                    if e.get("method") == "thread/realtime/start")["params"]

    def test_webrtc_is_the_default_and_a_go_is_an_sdp_answer_proving_auth_only(self):
        code, report = self.probe("go")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "go", report)
        self.assertEqual(report["transport"], "webrtc")
        self.assertIn("not audio", report["reason"])
        self.assertIn("audio is not proven", report["go_means"])
        self.assertEqual(report["answer_media"], ["audio", "application"])
        fake = load(FAKE)
        self.assertEqual(report["answer_sdp_bytes"], len(fake.ANSWER.encode()))
        # The answer text, and its session identifier, never reach the output.
        output = json.dumps(report)
        self.assertNotIn(fake.ANSWER_SESSION, output)
        self.assertNotIn("a=setup", output)
        self.assertEqual(report["audio_bytes"], 0)
        self.assertEqual(report["events_seen"]["thread/realtime/sdp"], 1)
        # No text turn: there is no media path to speak over.
        self.assertEqual([step["method"] for step in report["steps"]], [
            "initialize", "account/read", "thread/start", "thread/realtime/start",
            "thread/realtime/stop"])
        self.assertTrue(all(step["ok"] for step in report["steps"]), report["steps"])
        self.assertEqual(report["shutdown"], "clean")
        self.assertTrue(report["process_group_gone"])
        self.assertHelperGone()
        self.assertNoKeyPassed()

        start = self.start_params()
        self.assertEqual(start["model"], "gpt-live-1-codex")
        self.assertEqual(start["version"], "v3")
        self.assertEqual(start["outputModality"], "audio")
        self.assertEqual(set(start["transport"]), {"type", "sdp"})
        self.assertEqual(start["transport"]["type"], "webrtc")
        # The offer the server received is SDP with exactly one audio m-line.
        offer = start["transport"]["sdp"]
        self.assertNotIn(offer[:40], output)
        session, media = parse_sdp(offer)
        audio = [m for m in media if m[0].startswith("m=audio ")]
        self.assertEqual(len(audio), 1, media)
        self.assertEqual([m[0].split(" ")[0] for m in media], ["m=audio", "m=application"])
        self.assertIn("a=rtpmap:111 opus/48000/2", audio[0])
        self.assertIn("a=rtcp-mux", audio[0])
        self.assertIn("a=sendrecv", audio[0])
        self.assertTrue(media[1][0].endswith("UDP/DTLS/SCTP webrtc-datachannel"))
        self.assertIn("a=group:BUNDLE 0 1", session)
        for number, section in enumerate(media):
            self.assertIn(f"a=mid:{number}", section)
            self.assertIn("a=setup:actpass", section)
            values = dict(line[2:].split(":", 1) for line in section if line.startswith("a=")
                          and ":" in line)
            self.assertRegex(values["ice-ufrag"], r"^[A-Za-z0-9+/]{4,256}$")
            self.assertRegex(values["ice-pwd"], r"^[A-Za-z0-9+/]{22,256}$")
            self.assertRegex(values["fingerprint"], r"^sha-256 [0-9A-F]{2}(:[0-9A-F]{2}){31}$")

    def test_each_offer_has_fresh_ice_credentials_and_fingerprint(self):
        probe = load(PROBE)
        first, second = probe.build_offer(), probe.build_offer()
        def secrets(sdp):
            return re.findall(r"a=(?:ice-ufrag|ice-pwd|fingerprint):(.*)", sdp)
        self.assertEqual(len(secrets(first)), 6)
        self.assertFalse(set(secrets(first)) & set(secrets(second)))
        self.assertEqual(probe.media_kinds(first), ["audio", "application"])

    def test_webrtc_realtime_that_needs_an_api_key_is_a_no_go(self):
        code, report = self.probe("needs-api-key")
        self.assertEqual(code, 0, report)
        self.assertEqual(report["verdict"], "no-go", report)
        self.assertEqual(report["transport"], "webrtc")
        self.assertIn("before an SDP answer", report["reason"])
        self.assertIn("realtime conversation requires API key auth",
                      report["realtime_errors"])
        self.assertEqual(report["answer_sdp_bytes"], 0)
        self.assertEqual(report["answer_media"], [])
        self.assertNotIn("thread/realtime/appendText", self.methods())

    def test_webrtc_without_an_answer_is_an_error_and_the_server_is_killed(self):
        code, report = self.probe("hang", "--timeout", "1", "--grace", "0.5",
                                  "--rpc-timeout", "2")
        self.assertEqual(code, 1, report)
        self.assertEqual(report["verdict"], "error", report)
        self.assertIn("no SDP answer within 1s", report["reason"])
        self.assertEqual(report["events_seen"]["thread/realtime/started"], 1)
        self.assertEqual(report["shutdown"], "killed")
        self.assertTrue(report["process_group_gone"])
        self.assertHelperGone()

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
        code, report = self.probe("hang", *WEBSOCKET, "--timeout", "1",
                                  "--grace", "0.5", "--rpc-timeout", "2")
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
