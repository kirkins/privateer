"""Run deterministic browser-boundary checks against the actual scoped modules."""
import pathlib
import shutil
import subprocess
import unittest


class ScopedPageTest(unittest.TestCase):
    def test_scoped_page_async_lifecycle_and_readonly_workspace(self):
        node = shutil.which("node")
        if not node:
            self.skipTest("node is needed to exercise the bundled page")
        script = pathlib.Path(__file__).with_suffix(".mjs")
        done = subprocess.run([node, "--experimental-vm-modules", str(script)],
                              capture_output=True, text=True, timeout=20)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("scoped page behavior checks passed", done.stdout)


if __name__ == "__main__":
    unittest.main()
