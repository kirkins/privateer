import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "dashboard_smoke", Path(__file__).with_name("dashboard-smoke.py")
)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class BinaryRouting(unittest.TestCase):
    def test_both_option_syntaxes_select_the_same_demo_and_live_artifact(self):
        binary = Path("target/review-artifact/privateer-dashboard").resolve()
        for arguments in [["--binary", str(binary)], [f"--binary={binary}"]]:
            with self.subTest(arguments=arguments), \
                    patch.object(sys, "argv", ["dashboard-smoke.py", *arguments]), \
                    patch.object(smoke, "demo") as demo, \
                    patch("dashboard_live_smoke.run") as live:
                smoke.main()
                demo.assert_called_once_with(str(binary))
                live.assert_called_once_with(binary, binary.with_name("privateer"))
                # Old onboarding/Board controls are intentionally not bridge entry points.


if __name__ == "__main__":
    unittest.main()
