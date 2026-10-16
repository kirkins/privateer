import importlib.util
import os
from pathlib import Path
import sys
import tempfile
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

    def test_authless_fixture_allowlist_never_changes_supervisor_or_inherits_secrets(self):
        from dashboard_live_smoke import fixture_environment
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {
                "PRIVATEER_ROLE": "agent", "PRIVATEER_ACTOR": "privateer",
                "PRIVATEER_HOME": "/real/home", "HERDR_SOCKET": "/real/herdr.sock",
                "OPENAI_API_KEY": "not-a-real-key", "SSH_AUTH_SOCK": "/real/ssh.sock",
                "AWS_SHARED_CREDENTIALS_FILE": "/real/credentials", "PATH": "/untrusted/bin"}):
            before = dict(os.environ)
            root = Path(temporary).resolve()
            env = fixture_environment(root)
            assert dict(os.environ) == before
            assert set(env) == {"HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME",
                                "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "TMPDIR", "PATH", "LANG",
                                "TERM", "PRIVATEER_FLEET", "PRIVATEER_RESOURCE_HOME", "PRIVATEER_SERVICE"}
            assert env["PATH"] == "/usr/bin:/bin"
            for key in ["HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME",
                        "XDG_CACHE_HOME", "XDG_RUNTIME_DIR", "TMPDIR", "PRIVATEER_RESOURCE_HOME"]:
                assert Path(env[key]).is_relative_to(root) and Path(env[key]).is_dir()
            assert Path(env["PRIVATEER_FLEET"]).is_relative_to(root)
            assert env["PRIVATEER_SERVICE"] == "none"


if __name__ == "__main__":
    unittest.main()
