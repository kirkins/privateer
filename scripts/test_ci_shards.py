"""Check shard coverage without building Rust or needing a toolchain in docs CI."""
from collections import Counter
import importlib.util
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("shard", Path(__file__).with_name("test-shard.py"))
shard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shard)


class ShardCoverage(unittest.TestCase):
    def targets(self):
        names = [name for suites in shard.SHARDS.values() for name in suites]
        return [
            {"kind": ["lib"], "name": "privateer"},
            {"kind": ["bin"], "name": "privateer"},
            {"kind": ["bin"], "name": "privateer-dashboard"},
            *({"kind": ["test"], "name": name} for name in names + ["new_suite"]),
        ]

    def test_all_targets_and_future_suites_run_exactly_once(self):
        found = self.targets()
        assigned = Counter()
        with patch.object(shard, "targets", return_value=found):
            for name in [*shard.SHARDS, "rest"]:
                for command in shard.commands(name):
                    self.assertEqual(command[:4], ["cargo", "test", "--locked", "--all-features"])
                    selected = command[4:]
                    if "--doc" in selected:
                        self.assertEqual(selected, ["--doc"])
                        assigned["doc"] += 1
                    for kind in ("lib", "bin"):
                        flag = "--lib" if kind == "lib" else "--bins"
                        if flag in selected:
                            assigned.update((kind, t["name"]) for t in found if t["kind"] == [kind])
                    assigned.update(("test", selected[i + 1]) for i, arg in enumerate(selected) if arg == "--test")
        expected = Counter((t["kind"][0], t["name"]) for t in found)
        expected["doc"] = 1
        self.assertEqual(assigned, expected)

    def test_missing_duplicate_and_unhandled_targets_fail_instead_of_skipping(self):
        found = self.targets()
        with patch.object(shard, "targets", return_value=found[:-2]):
            with self.assertRaisesRegex(SystemExit, "no such suite"):
                shard.commands("rest")
        with patch.object(shard, "SHARDS", {"slow-1": ("door", "door")}):
            with patch.object(shard, "targets", return_value=found):
                with self.assertRaisesRegex(SystemExit, "assigned twice"):
                    shard.commands("rest")
        for kind in ("bench", "example"):
            with self.subTest(kind=kind), patch.object(shard, "targets", return_value=found + [
                {"kind": [kind], "name": "new_target"},
            ]):
                with self.assertRaisesRegex(SystemExit, "no shard runs"):
                    shard.commands("rest")
        with patch.object(shard, "LIB_SHARD", "missing"):
            with self.assertRaisesRegex(SystemExit, "no shard runs the library"):
                shard.commands("rest")

    def test_failing_subprocess_stops_the_shard_and_records_its_status(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", "/var/tmp")) as temp:
            summary = Path(temp) / "summary"
            with patch.dict(os.environ, GITHUB_STEP_SUMMARY=str(summary)), \
                 patch.object(shard, "commands", return_value=[["first"], ["second"]]), \
                 patch.object(shard.subprocess, "run", return_value=SimpleNamespace(returncode=7)) as run:
                self.assertEqual(shard.main(["test-shard.py", "rest"]), 7)
                run.assert_called_once_with(["first"])
            self.assertIn("exit 7", summary.read_text())


if __name__ == "__main__":
    unittest.main()
