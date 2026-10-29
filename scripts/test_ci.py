"""Exercise CI routing through real Git histories and subprocess entry points."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import ci

SCRIPT = Path(__file__).with_name("ci.py").resolve()


class Routing(unittest.TestCase):
    def test_only_known_docs_and_research_paths_are_lightweight(self):
        for name in ("README.md", "AGENTS.md", "docs/design/new.md",
                     "docs/research/costs.json", "scripts/research/costs.py"):
            with self.subTest(name=name):
                self.assertTrue(ci.lightweight(name))
        for name in ("Cargo.lock", "rust-toolchain.toml", "src/main.rs",
                     ".github/workflows/ci.yml", "scripts/ci.py", "scripts/test-shard.py",
                     "docs/examples/profile.json", "docs/research/tool.sh",
                     "docs/research/tool.py", "plugins/README.md", "future.config"):
            with self.subTest(name=name):
                self.assertFalse(ci.lightweight(name))

    def test_push_dispatch_and_future_events_always_run_runtime(self):
        for event in ("push", "workflow_dispatch", "merge_group"):
            with self.subTest(event=event):
                self.assertTrue(ci.runtime_required(event, {}))


class GitFixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP", "/var/tmp"))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.write("README.md", "# Fixture\n")
        self.write("docs/old.md", "# Old\n")
        self.write("src/main.rs", "fn main() {}\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=CI Fixture", "-c", "user.email=ci@example.invalid", *args],
            cwd=self.repo, check=True, capture_output=True, text=True,
        ).stdout.strip()

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def run_script(self, command, **env):
        return subprocess.run(
            [sys.executable, str(SCRIPT), command], cwd=self.repo,
            env={**os.environ, **env}, capture_output=True, text=True,
        )

    def plan(self, base, head):
        event = self.root / "event.json"
        output = self.root / "output"
        output.unlink(missing_ok=True)
        event.write_text(json.dumps({"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}))
        result = self.run_script("plan", GITHUB_EVENT_NAME="pull_request",
                                 GITHUB_EVENT_PATH=str(event), GITHUB_OUTPUT=str(output))
        return result, output.read_text() if output.exists() else ""

    def assert_plan(self, required, base=None, head=None):
        result, output = self.plan(base or self.base, head or self.git("rev-parse", "HEAD"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, f"runtime={str(required).lower()}\n")

    def test_docs_research_and_deleted_docs_take_the_lightweight_path(self):
        self.write("docs/new name; $(false).md", "# New\n")
        self.write("docs/research/results.json", "{}\n")
        self.write("scripts/research/offline.py", "import unavailable_dependency\n")
        (self.repo / "docs/old.md").unlink()
        self.commit()
        self.assert_plan(False)

    def test_no_changes_or_an_unreadable_base_cannot_skip_runtime(self):
        self.assert_plan(True)
        result, output = self.plan("f" * 40, self.base)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, "")

    def test_runtime_deletion_hidden_in_a_rename_still_runs_runtime(self):
        self.git("mv", "src/main.rs", "docs/moved.md")
        self.commit()
        self.assert_plan(True)

    def test_runtime_after_hundreds_of_docs_is_not_truncated(self):
        for i in range(350):
            self.write(f"docs/{i:03d}.md", "# Doc\n")
        self.write("src/main.rs", "fn main() { println!(\"changed\"); }\n")
        self.commit()
        self.assert_plan(True)

    def test_merge_base_ignores_unrelated_changes_on_the_base_branch(self):
        self.git("switch", "-qc", "topic")
        self.write("docs/topic.md", "# Topic\n")
        head = self.commit()
        self.git("switch", "-q", "main")
        self.write("src/main.rs", "fn main() { println!(\"base advanced\"); }\n")
        base = self.commit()
        self.assert_plan(False, base, head)

    def test_research_checks_do_not_import_execute_or_write_bytecode(self):
        self.write("scripts/research/offline.py", "import unavailable_dependency\nraise RuntimeError('never run')\n")
        self.write("docs/research/results.json", '{"tokens": 42}\n')
        self.commit()
        result = self.run_script("check-docs")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Checked 2 JSON/Python files", result.stdout)
        self.assertEqual(list(self.repo.rglob("__pycache__")), [])

    def test_invalid_research_inputs_fail_checks(self):
        for name, source in (("docs/research/results.json", "{invalid}"),
                             ("scripts/research/offline.py", "def invalid(:\n")):
            with self.subTest(name=name):
                self.write(name, source)
                self.commit()
                result = self.run_script("check-docs")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(name, result.stderr)
                (self.repo / name).unlink()


if __name__ == "__main__":
    unittest.main()
