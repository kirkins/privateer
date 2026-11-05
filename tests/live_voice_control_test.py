#!/usr/bin/env python3
"""The fake app-server's control logs under a live writer (#713).

A scoped voice fixture read `voice-control/received.jsonl` while the fake
app-server was still appending to it and tried to parse a half-written last
line (`door::scoped_voice_recorded_detail_crosses_linked_gateways_without_text_or_actions`).
These tests pin the two halves of the fix without a sleep in place of a
condition: a record is appended as one whole line, a poller counts only lines
that reached their newline, and a finished line that is not JSON still fails.

Usage: python3 tests/live_voice_control_test.py
"""

import json
import os
import pathlib
import shutil
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parent.parent
TESTS = ROOT / "tests"
sys.path.insert(0, str(TESTS))
import live_voice_control as control  # noqa: E402

# The fake app-server reads FAKE_VOICE_DIR when it is imported and appends its
# control logs there; import the real writer so these tests cover the code the
# fixtures run, not a copy of it.
CONTROL = tempfile.mkdtemp(prefix="voice-control-")
os.environ["FAKE_VOICE_DIR"] = CONTROL
import fake_live_voice_app_server as fake  # noqa: E402


def tearDownModule():
    shutil.rmtree(CONTROL, ignore_errors=True)


class TheControlLog(unittest.TestCase):
    def setUp(self):
        self.dir = pathlib.Path(tempfile.mkdtemp(prefix="log-", dir=CONTROL))
        self.log = self.dir / "received.jsonl"
        # The fake appends every record under FAKE_VOICE_DIR itself.
        self.recorded = pathlib.Path(fake.DIR) / "received.jsonl"
        self.recorded.unlink(missing_ok=True)

    def append(self, message):
        control.append_record(self.log, message)

    def test_a_trailing_fragment_without_its_newline_is_not_a_record(self):
        # The flake: a finished line, then the kernel has copied only part of
        # the next append when the poller reads. The old fixture reader parsed
        # every split line and raised JSONDecodeError on the fragment.
        self.append({"id": "done", "result": {"ok": True}})
        with open(self.log, "ab") as f:
            f.write(b'{"id": "half", "result": {"cont')
        self.assertEqual(control.read_records(self.log), [{"id": "done", "result": {"ok": True}}])
        with self.assertRaises(json.JSONDecodeError):
            [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_the_fragment_becomes_a_record_once_its_newline_lands(self):
        self.append({"id": "done"})
        with open(self.log, "ab") as f:
            f.write(b'{"id": "half"')
        self.assertEqual(control.read_records(self.log), [{"id": "done"}])
        with open(self.log, "ab") as f:
            f.write(b", \"result\": {}}\n")
        self.assertEqual(
            control.read_records(self.log),
            [{"id": "done"}, {"id": "half", "result": {}}],
        )

    def test_a_finished_line_that_is_not_json_still_fails(self):
        with open(self.log, "wb") as f:
            f.write(b'{"id": "done"}\n{"id": \n')
        with self.assertRaises(json.JSONDecodeError):
            control.read_records(self.log)

    def test_a_log_that_is_absent_empty_or_only_a_fragment_reads_as_nothing(self):
        self.assertEqual(control.read_records(self.dir / "missing.jsonl"), [])
        self.log.write_bytes(b"")
        self.assertEqual(control.read_records(self.log), [])
        self.log.write_bytes(b'{"id": "half"')
        self.assertEqual(control.read_records(self.log), [])

    def test_an_append_is_always_one_whole_line(self):
        messages = [{"id": f"m{i}", "pad": "a" * 4000} for i in range(20)]
        for message in messages:
            self.append(message)
        self.assertTrue(self.log.read_bytes().endswith(b"\n"))
        self.assertEqual(control.read_records(self.log), messages)

    def test_concurrent_writers_never_splice_each_others_lines(self):
        # The fake serializes its own appends; large bodies cross the stdio
        # buffer a single JSON line would otherwise be split across.
        expected = [{"id": f"c{index}", "pad": "b" * 4000} for index in range(120)]

        def write(chunk):
            for message in chunk:
                fake.record("received.jsonl", message)

        threads = [
            threading.Thread(target=write, args=(expected[index::8],)) for index in range(8)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(60)
        self.assertFalse(any(thread.is_alive() for thread in threads), "a writer hung")
        records = control.read_records(self.recorded)
        self.assertEqual(len(records), len(expected))
        self.assertEqual(sorted(r["id"] for r in records), sorted(r["id"] for r in expected))
        for record in records:
            self.assertEqual(len(record["pad"]), 4000)

    def test_a_poller_reads_only_whole_records_while_the_fake_writes(self):
        expected = [{"id": f"p{index}", "pad": "c" * 6000} for index in range(60)]

        def write():
            for message in expected:
                fake.record("received.jsonl", message)

        writer = threading.Thread(target=write)
        writer.start()
        seen = {}
        deadline = time.monotonic() + 30
        while len(seen) < len(expected):
            # No sleep: the loop ends on the record count, and every poll must
            # either return finished records or nothing new.
            self.assertLess(time.monotonic(), deadline, "the poller never saw every record")
            for record in control.read_records(self.recorded):
                self.assertEqual(len(record["pad"]), 6000, record)
                seen[record["id"]] = record
        writer.join(30)
        self.assertEqual(seen, {message["id"]: message for message in expected})


if __name__ == "__main__":
    unittest.main()
