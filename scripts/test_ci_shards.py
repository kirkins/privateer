"""Check shard coverage without building Rust or needing a toolchain in docs CI."""
from collections import Counter
import importlib.util
import os
from pathlib import Path
import re
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

    def shard_commands(self):
        found = self.targets()
        with patch.object(shard, "targets", return_value=found):
            return {name: shard.commands(name) for name in [*shard.SHARDS, "rest"]}

    def test_all_targets_and_future_suites_run_exactly_once(self):
        assigned = Counter()
        lib_runs = []
        for name, commands in self.shard_commands().items():
            for command in commands:
                self.assertEqual(command[:4], ["cargo", "test", "--locked", "--all-features"])
                selected = command[4:]
                if "--doc" in selected:
                    self.assertEqual(selected, ["--doc"])
                    assigned["doc"] += 1
                if "--bins" in selected:
                    assigned.update(("bin", t["name"]) for t in self.targets() if t["kind"] == ["bin"])
                if "--lib" in selected:
                    lib_runs.append((name, selected))
                    assigned["lib"] += 0  # counted once below, as both halves
                assigned.update(("test", selected[i + 1]) for i, arg in enumerate(selected) if arg == "--test")
        # The two filter halves partition the library, so it runs exactly once.
        self.assertEqual(len(lib_runs), 2)
        assigned.update(("lib", t["name"]) for t in self.targets() if t["kind"] == ["lib"])
        expected = Counter((t["kind"][0], t["name"]) for t in self.targets())
        expected["doc"] = 1
        self.assertEqual(assigned, expected)

    def test_library_halves_partition_by_complementary_filters(self):
        lib_runs = [(name, command) for name, commands in self.shard_commands().items()
                    for command in commands if "--lib" in command]
        # One run per half, on two different shards.
        self.assertEqual(len(lib_runs), 2)
        self.assertNotEqual(lib_runs[0][0], lib_runs[1][0])
        halves = {name: command[command.index("--") + 1:] if "--" in command else []
                  for name, command in lib_runs}
        self.assertEqual(sorted(halves), sorted([shard.LIB_SHARD, shard.LIB_SKIP_SHARD]))
        # The include half names the prefixes; the skip half skips each of them,
        # so together they run every library test and share none.
        include = halves[shard.LIB_SHARD]
        skip = halves[shard.LIB_SKIP_SHARD]
        self.assertEqual(include, list(shard.LIB_INCLUDE))
        self.assertEqual(skip, [arg for p in shard.LIB_INCLUDE for arg in ("--skip", p)])
        # A filtered library half never shares a cargo run with suites: the
        # filter would apply to the suites too and could drop their tests.
        for name, commands in self.shard_commands().items():
            for command in commands:
                if "--test" in command:
                    self.assertNotIn("--lib", command)

    def test_missing_duplicate_and_unhandled_targets_fail_instead_of_skipping(self):
        found = self.targets()
        with patch.object(shard, "targets", return_value=found[:-2]):
            with self.assertRaisesRegex(SystemExit, "no such suite"):
                shard.commands("rest")
        with patch.object(shard, "SHARDS", {**shard.SHARDS,
                                             "slow-1": (*shard.SHARDS["slow-1"], "door")}):
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
            with self.assertRaisesRegex(SystemExit, "no shard pair runs the library"):
                shard.commands("rest")
        with patch.object(shard, "LIB_SKIP_SHARD", shard.LIB_SHARD):
            with self.assertRaisesRegex(SystemExit, "halves share a shard"):
                shard.commands("rest")
        with patch.object(shard, "LIB_INCLUDE", ()):
            with self.assertRaisesRegex(SystemExit, "include list is empty"):
                shard.commands("rest")
        with patch.object(shard, "LIB_INCLUDE", ("door",)):
            with self.assertRaisesRegex(SystemExit, "must end in '::'"):
                shard.commands("rest")

    def test_heavy_pair_never_shares_a_shard(self):
        """The two ~2.2x-on-macOS suites together timed out a 25-minute job."""
        for name, suites in shard.SHARDS.items():
            with self.subTest(shard=name):
                self.assertFalse(set(shard.HEAVY_PAIR) <= set(suites))
        found = self.targets()
        regrouped = {**shard.SHARDS,
                     "slow-4": tuple(s for s in shard.SHARDS["slow-4"]
                                     if s not in shard.HEAVY_PAIR),
                     "slow-5": (*shard.SHARDS["slow-5"], "upgrade_linked")}
        with patch.object(shard, "SHARDS", regrouped), \
             patch.object(shard, "targets", return_value=found):
            with self.assertRaisesRegex(SystemExit, "share shard"):
                shard.commands("rest")

    def test_workflow_matrix_lists_every_shard(self):
        """The reusable workflow must run exactly the shards the script knows."""
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/test-shards.yml"
        matrix = re.search(r"shard: \[([^]]+)\]", workflow.read_text())
        self.assertIsNotNone(matrix, "test-shards.yml has no shard matrix")
        listed = {name.strip() for name in matrix.group(1).split(",")}
        self.assertEqual(listed, {*shard.SHARDS, "rest"})

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
