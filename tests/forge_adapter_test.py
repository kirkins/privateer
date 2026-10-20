#!/usr/bin/env python3
"""Tests for plugins/forge.py against a local http.server fixture."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "plugins"))

import forge


class MockForgeHandler(http.server.BaseHTTPRequestHandler):
    """Configurable mock HTTP handler for GitHub and Forgejo APIs."""

    def log_message(self, format, *args):
        # Suppress standard logging to keep test output clean
        pass

    def do_GET(self):
        self.server.record_request("GET", self.path, self.headers, None)
        self._dispatch("GET")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length > 0 else b""
        self.server.record_request("POST", self.path, self.headers, body)
        self._dispatch("POST")

    def do_PUT(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length > 0 else b""
        self.server.record_request("PUT", self.path, self.headers, body)
        self._dispatch("PUT")

    def _dispatch(self, method):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        route_key = (method, path)

        routes = self.server.routes
        if route_key in routes:
            status, resp_body = routes[route_key]
        elif (method, "*") in routes:
            status, resp_body = routes[(method, "*")]
        else:
            status = 404
            resp_body = {"message": f"Not Found: {method} {path}"}

        resp_bytes = json.dumps(resp_body).encode("utf-8") if isinstance(resp_body, (dict, list)) else resp_body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp_bytes)))
        self.end_headers()
        self.wfile.write(resp_bytes)


class MockForgeServer(http.server.ThreadingHTTPServer):
    def __init__(self, server_address):
        super().__init__(server_address, MockForgeHandler)
        self.routes = {}
        self.requests = []
        self._lock = threading.Lock()

    def record_request(self, method, path, headers, body):
        with self._lock:
            body_json = None
            if body:
                try:
                    body_json = json.loads(body.decode("utf-8"))
                except Exception:
                    body_json = body
            self.requests.append({
                "method": method,
                "path": path,
                "headers": {k.lower(): v for k, v in headers.items()},
                "body": body_json,
            })

    def get_requests(self):
        with self._lock:
            return list(self.requests)

    def clear_requests(self):
        with self._lock:
            self.requests.clear()


class ForgeAdapterTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = MockForgeServer(("127.0.0.1", 0))
        cls.port = cls.server.server_port
        cls.server_thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.server_thread.start()
        cls.base_url = f"http://127.0.0.1:{cls.port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.server.routes.clear()
        self.server.clear_requests()
        self.temp_dir = tempfile.TemporaryDirectory(prefix="privateer-forge-test-")

    def tearDown(self):
        self.temp_dir.cleanup()

    # -------------------------------------------------------------------------
    # GitHub Tests
    # -------------------------------------------------------------------------

    def test_github_pr_view_success(self):
        self.server.routes = {
            ("GET", "/repos/owner/repo/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": "base-sha-111"},
                "head": {"sha": "head-sha-222"},
                "mergeable": True,
                "merged": False,
            }),
            ("GET", "/repos/owner/repo/commits/head-sha-222/status"): (200, {
                "statuses": [
                    {"context": "runtime (ubuntu-latest)", "state": "success"}
                ]
            }),
            ("GET", "/repos/owner/repo/commits/head-sha-222/check-runs"): (200, {
                "check_runs": [
                    {"name": "runtime (macos-latest)", "status": "completed", "conclusion": "success"}
                ]
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "token": "gh-test-token",
            "required_checks": ["runtime (ubuntu-latest)", "runtime (macos-latest)"],
        }
        res = forge.handle("pr.view", req)

        self.assertTrue(res["ok"])
        self.assertEqual(res["state"], "open")
        self.assertEqual(res["base"], "main")
        self.assertEqual(res["base_sha"], "base-sha-111")
        self.assertEqual(res["head_sha"], "head-sha-222")
        self.assertTrue(res["mergeable"])
        self.assertFalse(res["merged"])
        self.assertEqual(res["checks"]["runtime (ubuntu-latest)"], "success")
        self.assertEqual(res["checks"]["runtime (macos-latest)"], "success")
        self.assertTrue(res["required_checks_ok"])

        # Verify request headers and calls
        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 3)
        self.assertEqual(reqs[0]["headers"].get("authorization"), "Bearer gh-test-token")
        self.assertEqual(reqs[0]["headers"].get("accept"), "application/vnd.github+json")
        self.assertEqual(reqs[0]["headers"].get("x-github-api-version"), "2022-11-28")

    def test_github_pr_view_missing_required_check(self):
        self.server.routes = {
            ("GET", "/repos/owner/repo/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": "base-sha-111"},
                "head": {"sha": "head-sha-222"},
            }),
            ("GET", "/repos/owner/repo/commits/head-sha-222/status"): (200, {
                "statuses": [
                    {"context": "runtime (ubuntu-latest)", "state": "failure"}
                ]
            }),
            ("GET", "/repos/owner/repo/commits/head-sha-222/check-runs"): (200, {
                "check_runs": []
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "required_checks": ["runtime (ubuntu-latest)", "runtime (macos-latest)"],
        }
        res = forge.handle("pr.view", req)
        self.assertTrue(res["ok"])
        self.assertFalse(res["required_checks_ok"])

    def test_github_review_post_new(self):
        self.server.routes = {
            ("GET", "/repos/owner/repo/pulls/10/reviews"): (200, []),
            ("POST", "/repos/owner/repo/pulls/10/reviews"): (200, {
                "id": 9901,
                "state": "APPROVED",
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "commit_id": "head-sha-222",
            "event": "APPROVE",
            "body": "Looks good!\n<!-- privateer-review rev-uuid-123 -->",
            "reviewer": "forge-reviewer-gh",
            "token": "rev-token",
        }
        res = forge.handle("review.post", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])
        self.assertFalse(res["already_exists"])
        self.assertEqual(res["review_id"], 9901)

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 2)
        # Check GET reviews
        self.assertEqual(reqs[0]["method"], "GET")
        self.assertEqual(reqs[0]["path"], "/repos/owner/repo/pulls/10/reviews")
        # Check POST review shape
        self.assertEqual(reqs[1]["method"], "POST")
        self.assertEqual(reqs[1]["path"], "/repos/owner/repo/pulls/10/reviews")
        self.assertEqual(reqs[1]["body"]["commit_id"], "head-sha-222")
        self.assertEqual(reqs[1]["body"]["event"], "APPROVE")
        self.assertIn("<!-- privateer-review rev-uuid-123 -->", reqs[1]["body"]["body"])
        self.assertEqual(reqs[1]["headers"].get("authorization"), "Bearer rev-token")

    def test_github_review_post_idempotent_marker_lookup(self):
        # Existing review contains marker from the reviewer account
        self.server.routes = {
            ("GET", "/repos/owner/repo/pulls/10/reviews"): (200, [
                {
                    "id": 8801,
                    "user": {"login": "forge-reviewer-gh"},
                    "body": "Earlier run\n<!-- privateer-review rev-uuid-123 -->",
                    "state": "APPROVED",
                }
            ]),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "commit_id": "head-sha-222",
            "event": "APPROVE",
            "body": "Replay attempt\n<!-- privateer-review rev-uuid-123 -->",
            "reviewer": "forge-reviewer-gh",
            "token": "rev-token",
        }
        res = forge.handle("review.post", req)

        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 8801)

        reqs = self.server.get_requests()
        # Only GET was called; NO POST was executed!
        self.assertEqual(len(reqs), 1)
        self.assertEqual(reqs[0]["method"], "GET")

    def test_github_pr_merge_success(self):
        self.server.routes = {
            ("PUT", "/repos/owner/repo/pulls/10/merge"): (200, {
                "sha": "merged-commit-sha-333",
                "merged": True,
                "message": "Pull Request successfully merged",
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "head_sha": "head-sha-222",
            "method": "merge",
            "token": "ship-gh-token",
        }
        res = forge.handle("pr.merge", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["merged"])
        self.assertEqual(res["merge_sha"], "merged-commit-sha-333")

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 1)
        self.assertEqual(reqs[0]["method"], "PUT")  # GitHub uses PUT!
        self.assertEqual(reqs[0]["path"], "/repos/owner/repo/pulls/10/merge")
        self.assertEqual(reqs[0]["body"]["sha"], "head-sha-222")
        self.assertEqual(reqs[0]["body"]["merge_method"], "merge")
        self.assertEqual(reqs[0]["headers"].get("authorization"), "Bearer ship-gh-token")

    def test_github_pr_merge_refusal_when_head_moved(self):
        self.server.routes = {
            ("PUT", "/repos/owner/repo/pulls/10/merge"): (409, {
                "message": "Head branch was modified. Review and try the merge again."
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 10,
            "head_sha": "stale-head-sha",
        }
        res = forge.handle("pr.merge", req)

        self.assertFalse(res["ok"])
        self.assertFalse(res["merged"])
        self.assertEqual(res["status_code"], 409)
        self.assertIn("Head branch was modified", res["error"])

    def test_github_protection_read_compliant(self):
        self.server.routes = {
            ("GET", "/repos/owner/repo/branches/main/protection"): (200, {
                "enforce_admins": {"enabled": True},
                "required_pull_request_reviews": {
                    "required_approving_review_count": 1,
                    "require_code_owner_reviews": True,
                    "dismiss_stale_reviews": True,
                    "require_last_push_approval": True,
                },
                "required_status_checks": {
                    "contexts": ["runtime (ubuntu-latest)", "runtime (macos-latest)"],
                    "checks": [],
                },
                "allow_force_pushes": {"enabled": False},
                "allow_deletions": {"enabled": False},
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "branch": "main",
            "required_checks": ["runtime (ubuntu-latest)", "runtime (macos-latest)"],
        }
        res = forge.handle("protection.read", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["protected"])
        self.assertTrue(res["enforce_admins"])
        self.assertEqual(res["required_approvals"], 1)
        self.assertTrue(res["require_code_owner_reviews"])
        self.assertTrue(res["dismiss_stale_approvals"])
        self.assertTrue(res["require_last_push_approval"])
        self.assertEqual(res["warnings"], [])

    def test_github_protection_read_missing_protections_warns(self):
        self.server.routes = {
            ("GET", "/repos/owner/repo/branches/main/protection"): (200, {
                "enforce_admins": {"enabled": False},
                "required_pull_request_reviews": {
                    "required_approving_review_count": 0,
                    "require_code_owner_reviews": False,
                    "dismiss_stale_reviews": False,
                    "require_last_push_approval": False,
                },
                "required_status_checks": {
                    "contexts": ["runtime (ubuntu-latest)"],
                    "checks": [],
                },
                "allow_force_pushes": {"enabled": True},
                "allow_deletions": {"enabled": True},
            }),
        }

        req = {
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "branch": "main",
            "required_checks": ["runtime (ubuntu-latest)", "runtime (macos-latest)"],
        }
        res = forge.handle("protection.read", req)

        self.assertTrue(res["ok"])
        self.assertIn("enforce_admins is not enabled", res["warnings"])
        self.assertIn("required approving review count is less than 1", res["warnings"])
        self.assertIn("require code owner reviews is not enabled", res["warnings"])
        self.assertIn("dismiss stale reviews is not enabled", res["warnings"])
        self.assertIn("require approval of most recent push is not enabled", res["warnings"])
        self.assertIn("force pushes are allowed", res["warnings"])
        self.assertIn("branch deletions are allowed", res["warnings"])
        self.assertTrue(any("runtime (macos-latest)" in w for w in res["warnings"]))

    # -------------------------------------------------------------------------
    # Forgejo Tests
    # -------------------------------------------------------------------------

    def test_forgejo_pr_view_success(self):
        self.server.routes = {
            ("GET", "/api/v1/repos/hermes/app/pulls/5"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": "fj-base-sha-1"},
                "head": {"sha": "fj-head-sha-2"},
                "mergeable": True,
                "merged": False,
            }),
            ("GET", "/api/v1/repos/hermes/app/commits/fj-head-sha-2/statuses"): (200, [
                {"context": "continuous-integration", "status": "success"},
                {"context": "lint", "status": "success"},
            ]),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "pr": 5,
            "token": "fj-token-123",
            "required_checks": ["continuous-integration"],
        }
        res = forge.handle("pr.view", req)

        self.assertTrue(res["ok"])
        self.assertEqual(res["state"], "open")
        self.assertEqual(res["base"], "main")
        self.assertEqual(res["head_sha"], "fj-head-sha-2")
        self.assertEqual(res["checks"]["continuous-integration"], "success")
        self.assertEqual(res["checks"]["lint"], "success")
        self.assertTrue(res["required_checks_ok"])

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 2)
        self.assertEqual(reqs[0]["headers"].get("authorization"), "token fj-token-123")
        self.assertEqual(reqs[0]["headers"].get("accept"), "application/json")

    def test_forgejo_review_post_approved_event_shape(self):
        self.server.routes = {
            ("GET", "/api/v1/repos/hermes/app/pulls/5/reviews"): (200, []),
            ("POST", "/api/v1/repos/hermes/app/pulls/5/reviews"): (200, {
                "id": 4321,
                "state": "APPROVED",
            }),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "pr": 5,
            "commit_id": "fj-head-sha-2",
            "event": "APPROVE",  # Caller sends APPROVE; Forgejo needs APPROVED!
            "body": "Verdict: approve\n<!-- privateer-review fj-rev-9 -->",
            "reviewer": "reviewer",
            "token": "fj-rev-token",
        }
        res = forge.handle("review.post", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])
        self.assertEqual(res["review_id"], 4321)

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 2)
        # Verify Forgejo receives "APPROVED" (with D!)
        self.assertEqual(reqs[1]["method"], "POST")
        self.assertEqual(reqs[1]["path"], "/api/v1/repos/hermes/app/pulls/5/reviews")
        self.assertEqual(reqs[1]["body"]["event"], "APPROVED")
        self.assertEqual(reqs[1]["body"]["commit_id"], "fj-head-sha-2")
        self.assertEqual(reqs[1]["headers"].get("authorization"), "token fj-rev-token")

    def test_forgejo_review_post_idempotent_marker_lookup(self):
        self.server.routes = {
            ("GET", "/api/v1/repos/hermes/app/pulls/5/reviews"): (200, [
                {
                    "id": 4320,
                    "user": {"username": "reviewer"},
                    "body": "Previous run <!-- privateer-review fj-rev-9 -->",
                    "state": "APPROVED",
                }
            ]),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "pr": 5,
            "commit_id": "fj-head-sha-2",
            "event": "APPROVE",
            "body": "Previous run <!-- privateer-review fj-rev-9 -->",
            "reviewer": "reviewer",
        }
        res = forge.handle("review.post", req)

        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 4320)

        # No POST request
        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 1)
        self.assertEqual(reqs[0]["method"], "GET")

    def test_forgejo_pr_merge_shape(self):
        self.server.routes = {
            ("POST", "/api/v1/repos/hermes/app/pulls/5/merge"): (200, {
                "sha": "fj-merge-commit-sha",
                "merged": True,
                "message": "merged",
            }),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "pr": 5,
            "head_sha": "fj-head-sha-2",
            "token": "fj-merger-token",
        }
        res = forge.handle("pr.merge", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["merged"])
        self.assertEqual(res["merge_sha"], "fj-merge-commit-sha")

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 1)
        # Forgejo uses POST with Do: merge and head_commit_id!
        self.assertEqual(reqs[0]["method"], "POST")
        self.assertEqual(reqs[0]["path"], "/api/v1/repos/hermes/app/pulls/5/merge")
        self.assertEqual(reqs[0]["body"]["head_commit_id"], "fj-head-sha-2")
        self.assertEqual(reqs[0]["body"]["Do"], "merge")
        self.assertEqual(reqs[0]["headers"].get("authorization"), "token fj-merger-token")

    def test_forgejo_protection_read_compliant(self):
        self.server.routes = {
            ("GET", "/api/v1/repos/hermes/app/branch_protections/main"): (200, {
                "branch_name": "main",
                "enable_push": False,
                "apply_to_admins": True,
                "required_approvals": 1,
                "enable_approvals_whitelist": True,
                "approvals_whitelist_usernames": ["reviewer"],
                "enable_merge_whitelist": True,
                "merge_whitelist_usernames": ["merger"],
                "dismiss_stale_approvals": True,
                "block_on_rejected_reviews": True,
                "enable_status_check": True,
                "status_check_contexts": ["ci-check"],
            }),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "branch": "main",
            "reviewer": "reviewer",
            "merger": "merger",
            "required_checks": ["ci-check"],
        }
        res = forge.handle("protection.read", req)

        self.assertTrue(res["ok"])
        self.assertTrue(res["protected"])
        self.assertFalse(res["enable_push"])
        self.assertTrue(res["apply_to_admins"])
        self.assertEqual(res["required_approvals"], 1)
        self.assertEqual(res["approvals_whitelist"], ["reviewer"])
        self.assertEqual(res["merge_whitelist"], ["merger"])
        self.assertEqual(res["warnings"], [])

    def test_forgejo_protection_read_missing_protections_warns(self):
        self.server.routes = {
            ("GET", "/api/v1/repos/hermes/app/branch_protections/main"): (200, {
                "branch_name": "main",
                "enable_push": True,
                "apply_to_admins": False,
                "required_approvals": 0,
                "enable_approvals_whitelist": False,
                "approvals_whitelist_usernames": [],
                "enable_merge_whitelist": False,
                "merge_whitelist_usernames": [],
                "dismiss_stale_approvals": False,
                "block_on_rejected_reviews": False,
                "enable_status_check": False,
                "status_check_contexts": [],
            }),
        }

        req = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": "hermes/app",
            "branch": "main",
            "reviewer": "reviewer",
            "merger": "merger",
            "required_checks": ["ci-check"],
        }
        res = forge.handle("protection.read", req)

        self.assertTrue(res["ok"])
        self.assertIn("enable_push is not false", res["warnings"])
        self.assertIn("apply_to_admins is not true", res["warnings"])
        self.assertIn("required_approvals is less than 1", res["warnings"])
        self.assertIn("enable_approvals_whitelist is not true", res["warnings"])
        self.assertIn("reviewer 'reviewer' not in approvals_whitelist_usernames", res["warnings"])
        self.assertIn("enable_merge_whitelist is not true", res["warnings"])
        self.assertIn("merger 'merger' not in merge_whitelist_usernames", res["warnings"])
        self.assertIn("dismiss_stale_approvals is not true", res["warnings"])
        self.assertIn("block_on_rejected_reviews is not true", res["warnings"])
        self.assertIn("enable_status_check is not true", res["warnings"])
        self.assertTrue(any("ci-check" in w for w in res["warnings"]))

    # -------------------------------------------------------------------------
    # CLI subprocess and token file integration tests
    # -------------------------------------------------------------------------

    def test_cli_subprocess_with_token_file(self):
        token_path = Path(self.temp_dir.name) / "forge-token"
        token_path.write_text("file-secret-token\n")

        self.server.routes = {
            ("GET", "/repos/owner/repo/pulls/42"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": "b1"},
                "head": {"sha": "h1"},
            }),
            ("GET", "/repos/owner/repo/commits/h1/status"): (200, {"statuses": []}),
            ("GET", "/repos/owner/repo/commits/h1/check-runs"): (200, {"check_runs": []}),
        }

        req = {
            "id": "cli-test-req-1",
            "version": 1,
            "forge": "github",
            "api": self.base_url,
            "repo": "owner/repo",
            "pr": 42,
        }

        env = os.environ.copy()
        env["PRIVATEER_FORGE_TOKEN_FILE"] = str(token_path)

        proc = subprocess.run(
            [sys.executable, str(ROOT / "plugins/forge.py"), "pr.view"],
            input=json.dumps(req),
            capture_output=True,
            text=True,
            env=env,
        )

        self.assertEqual(proc.returncode, 0, proc.stderr)
        data = json.loads(proc.stdout)
        self.assertTrue(data["ok"])
        self.assertEqual(data["id"], "cli-test-req-1")
        self.assertEqual(data["head_sha"], "h1")

        reqs = self.server.get_requests()
        self.assertEqual(reqs[0]["headers"].get("authorization"), "Bearer file-secret-token")

    def test_cli_subprocess_invalid_json(self):
        proc = subprocess.run(
            [sys.executable, str(ROOT / "plugins/forge.py"), "pr.view"],
            input="not valid json",
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0)
        data = json.loads(proc.stdout)
        self.assertFalse(data["ok"])
        self.assertIn("error", data)


if __name__ == "__main__":
    unittest.main()
