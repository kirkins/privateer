#!/usr/bin/env python3
"""Tests for plugins/forge.py against a local loopback http.server fixture.

Covers the security review's blocking findings: the token comes only from the
file named by PRIVATEER_FORGE_TOKEN_FILE; redirects are refused and never
forwarded with the token; api/repo/pr/branch/sha are validated with an https
pin (loopback http excepted); the review marker is bound to the token's own
login, the exact marker string, the requested commit, and a matching review
state; and failed check reads are errors while required_checks is never
allowed to be empty.
"""
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

TOKEN = "file-token-abc123"
GH_REPO = "owner/repo"
FJ_REPO = "hermes/app"
HEAD_SHA = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"
BASE_SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "f1e2d3c4b5a697886970615243f4e5d6c7b8a990"
GH_IDENTITY = "ship-reviewer"
FJ_IDENTITY = "hermes-reviewer"


class MockForgeHandler(http.server.BaseHTTPRequestHandler):
    """Configurable mock HTTP handler for GitHub and Forgejo APIs."""

    def log_message(self, format, *args):
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
        query = urllib.parse.parse_qs(parsed.query)

        routes = self.server.routes
        page_routes = self.server.page_routes
        extra_headers = {}
        if (method, path) in page_routes:
            status, resp_body = page_routes[(method, path)](query)
        elif (method, path) in routes:
            route = routes[(method, path)]
            if len(route) == 3:
                status, resp_body, extra_headers = route
            else:
                status, resp_body = route
        else:
            status = 404
            resp_body = {"message": f"Not Found: {method} {path}"}

        resp_bytes = (json.dumps(resp_body).encode("utf-8")
                      if isinstance(resp_body, (dict, list))
                      else resp_body.encode("utf-8"))
        self.send_response(status)
        for key, value in extra_headers.items():
            self.send_header(key, value)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp_bytes)))
        self.end_headers()
        self.wfile.write(resp_bytes)


class MockForgeServer(http.server.ThreadingHTTPServer):
    def __init__(self, server_address):
        super().__init__(server_address, MockForgeHandler)
        self.routes = {}
        self.page_routes = {}
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

        cls.evil_server = MockForgeServer(("127.0.0.1", 0))
        cls.evil_port = cls.evil_server.server_port
        cls.evil_thread = threading.Thread(target=cls.evil_server.serve_forever, daemon=True)
        cls.evil_thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.evil_server.shutdown()
        cls.evil_server.server_close()

    def setUp(self):
        self.server.routes = {}
        self.server.page_routes = {}
        self.server.clear_requests()
        self.evil_server.routes = {}
        self.evil_server.page_routes = {}
        self.evil_server.clear_requests()
        self.temp_dir = tempfile.TemporaryDirectory(prefix="privateer-forge-test-")
        self._saved_env = {}
        for var in ("PRIVATEER_FORGE_TOKEN_FILE", "PRIVATEER_FORGE_TOKEN",
                    "GITHUB_TOKEN", "FORGEJO_TOKEN"):
            self._saved_env[var] = os.environ.pop(var, None)
        self.set_token_file()

    def tearDown(self):
        self.temp_dir.cleanup()
        for var, value in self._saved_env.items():
            if value is None:
                os.environ.pop(var, None)
            else:
                os.environ[var] = value

    # -- helpers -----------------------------------------------------------

    def set_token_file(self, content=TOKEN + "\n"):
        path = Path(self.temp_dir.name) / "forge-token"
        path.write_text(content)
        os.environ["PRIVATEER_FORGE_TOKEN_FILE"] = str(path)
        return path

    def gh_request(self, **overrides):
        request = {
            "forge": "github",
            "api": self.base_url,
            "repo": GH_REPO,
            "pr": 10,
        }
        request.update(overrides)
        return request

    def fj_request(self, **overrides):
        request = {
            "forge": "forgejo",
            "api": f"{self.base_url}/api/v1",
            "repo": FJ_REPO,
            "pr": 5,
        }
        request.update(overrides)
        return request

    def gh_routes_pr_view(self, statuses=None, check_runs=None):
        self.server.routes.update({
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
                "mergeable": True,
                "merged": False,
            }),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/status"): (200, {
                "statuses": statuses if statuses is not None else [
                    {"context": "runtime (ubuntu-latest)", "state": "success"},
                ],
            }),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/check-runs"): (200, {
                "total_count": len(check_runs or []),
                "check_runs": check_runs if check_runs is not None else [
                    {"name": "runtime (macos-latest)", "status": "completed", "conclusion": "success"},
                ],
            }),
        })

    def gh_routes_review(self, existing=None, identity=GH_IDENTITY):
        self.server.routes.update({
            ("GET", "/user"): (200, {"login": identity}),
            ("GET", f"/repos/{GH_REPO}/pulls/10/reviews"): (200, existing or []),
            ("POST", f"/repos/{GH_REPO}/pulls/10/reviews"): (200, {"id": 9901, "state": "APPROVED"}),
        })

    def fj_routes_review(self, existing=None, identity=FJ_IDENTITY):
        self.server.routes.update({
            ("GET", "/api/v1/user"): (200, {"username": identity}),
            ("GET", f"/api/v1/repos/{FJ_REPO}/pulls/5/reviews"): (200, existing or []),
            ("POST", f"/api/v1/repos/{FJ_REPO}/pulls/5/reviews"): (200, {"id": 4321, "state": "APPROVED"}),
        })

    def assertNoRequests(self, server=None):
        server = server or self.server
        self.assertEqual(server.get_requests(), [])

    def request_paths(self, server=None):
        return [r["path"] for r in (server or self.server).get_requests()]

    # ---------------------------------------------------------------------
    # Token source (blocking finding 1)
    # ---------------------------------------------------------------------

    def test_request_token_refused(self):
        self.gh_routes_pr_view()
        for key in ("token", "token_file"):
            with self.subTest(key=key):
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError) as ctx:
                    forge.handle("pr.view", self.gh_request(**{key: "req-token"}))
                self.assertIn("PRIVATEER_FORGE_TOKEN_FILE", str(ctx.exception))
                self.assertNoRequests()

    def test_env_tokens_ignored_file_used(self):
        os.environ["PRIVATEER_FORGE_TOKEN"] = "env-generic-token"
        os.environ["GITHUB_TOKEN"] = "env-github-token"
        os.environ["FORGEJO_TOKEN"] = "env-forgejo-token"
        self.gh_routes_pr_view()
        res = forge.handle("pr.view", self.gh_request(required_checks=["runtime (ubuntu-latest)"]))
        self.assertTrue(res["ok"])
        self.assertEqual(
            self.server.get_requests()[0]["headers"].get("authorization"),
            f"Bearer {TOKEN}",
        )

    def test_missing_token_file_fails_with_no_request(self):
        os.environ["PRIVATEER_FORGE_TOKEN_FILE"] = str(
            Path(self.temp_dir.name) / "does-not-exist")
        self.gh_routes_pr_view()
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request())
        self.assertIn("cannot read", str(ctx.exception))
        self.assertNoRequests()

    def test_unset_token_variable_fails_with_no_request(self):
        del os.environ["PRIVATEER_FORGE_TOKEN_FILE"]
        self.gh_routes_pr_view()
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request())
        self.assertIn("PRIVATEER_FORGE_TOKEN_FILE", str(ctx.exception))
        self.assertNoRequests()

    def test_empty_token_file_fails_with_no_request(self):
        self.set_token_file("   \n")
        self.gh_routes_pr_view()
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request())
        self.assertIn("empty", str(ctx.exception))
        self.assertNoRequests()

    def test_token_with_whitespace_or_control_characters_fails(self):
        for content in ("bad token\n", "bad\ntoken\n", "bad\ttoken\n", "bad\x01token\n", "bad\x7ftoken\n"):
            with self.subTest(content=repr(content)):
                self.set_token_file(content)
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError) as ctx:
                    forge.handle("pr.view", self.gh_request())
                self.assertIn("refusing to run", str(ctx.exception))
                self.assertNoRequests()

    # ---------------------------------------------------------------------
    # Redirects (blocking finding 2)
    # ---------------------------------------------------------------------

    def test_redirect_refused_and_token_never_forwarded(self):
        evil_url = f"http://127.0.0.1:{self.evil_port}/repos/{GH_REPO}/pulls/10"
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (302, {}, {"Location": evil_url}),
        }
        self.evil_server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {"state": "open"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request(required_checks=["ci"]))
        self.assertIn("redirect refused", str(ctx.exception))
        # The request went to the configured host exactly once, and the
        # redirect target (which never got the Authorization header) saw
        # nothing at all.
        self.assertEqual(len(self.server.get_requests()), 1)
        self.assertNoRequests(self.evil_server)

    def test_proxy_environment_variables_ignored(self):
        # The opener is built with an empty ProxyHandler, so http_proxy and
        # friends never route the request (and its Authorization header)
        # anywhere but the configured host.
        self.gh_routes_pr_view()
        saved = {}
        for var in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
                    "all_proxy", "ALL_PROXY"):
            saved[var] = os.environ.pop(var, None)
            os.environ[var] = f"http://127.0.0.1:{self.evil_port}"
        try:
            res = forge.handle("pr.view", self.gh_request(
                required_checks=["runtime (ubuntu-latest)"]))
        finally:
            for var, value in saved.items():
                if value is None:
                    os.environ.pop(var, None)
                else:
                    os.environ[var] = value
        self.assertTrue(res["ok"])
        self.assertNoRequests(self.evil_server)  # nothing reached the "proxy"
        self.assertEqual(
            self.server.get_requests()[0]["headers"].get("authorization"),
            f"Bearer {TOKEN}")

    # ---------------------------------------------------------------------
    # URL and field validation (blocking finding 3)
    # ---------------------------------------------------------------------

    def test_api_scheme_validation(self):
        for api, ok in [
            ("https://api.github.com", True),
            ("https://git.example.net/api/v1", True),
            ("http://127.0.0.1:8123", True),
            ("http://localhost:8123", True),
            ("http://[::1]:8123", True),
            ("http://forge.example.net", False),
            ("ftp://api.github.com", False),
            ("git.example.net", False),
            ("https://user:pass@api.github.com", False),
            ("https://api.github.com?x=1", False),
            ("https://api.github.com#frag", False),
        ]:
            with self.subTest(api=api):
                if ok:
                    self.assertEqual(forge.validate_api(api), api.rstrip("/"))
                else:
                    with self.assertRaises(forge.ForgeError):
                        forge.validate_api(api)

    def test_non_https_api_refused_no_request(self):
        self.gh_routes_pr_view()
        for api in ("http://forge.example.net", "ftp://api.github.com", "", None):
            with self.subTest(api=api):
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError):
                    forge.handle("pr.view", self.gh_request(api=api))
                self.assertNoRequests()

    def test_forge_kind_explicit_only(self):
        # Neither a missing forge nor one sniffed from the api host is allowed.
        self.gh_routes_pr_view()
        for overrides in ({"forge": None}, {"forge": "gitlab"}, {"forge": ""},
                          {"forge": "GITHUB-" + "ish"}):
            with self.subTest(overrides=overrides):
                self.server.clear_requests()
                request = self.gh_request(api="https://api.github.com")
                request.pop("forge")
                if overrides.get("forge") is not None:
                    request["forge"] = overrides["forge"]
                with self.assertRaises(forge.ForgeError):
                    forge.handle("pr.view", request)
                self.assertNoRequests()

    def test_bad_pr_refused(self):
        self.gh_routes_pr_view()
        for pr in (0, -3, "ten", 1.5, True, None):
            with self.subTest(pr=pr):
                self.server.clear_requests()
                request = self.gh_request()
                request.pop("pr")
                if pr is not None:
                    request["pr"] = pr
                with self.assertRaises(forge.ForgeError):
                    forge.handle("pr.view", request)
                self.assertNoRequests()

    def test_bad_repo_refused(self):
        self.gh_routes_pr_view()
        for repo in ("owner", "owner/repo/extra", "../repo", "owner/..",
                     "owner/repo/", "owner /repo", "github.com/owner/repo",
                     "owner/repo?x=1", "", None):
            with self.subTest(repo=repo):
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError):
                    forge.handle("pr.view", self.gh_request(repo=repo))
                self.assertNoRequests()

    def test_bad_sha_refused(self):
        self.gh_routes_review()
        for sha in ("short", "z" * 40, "a1b2c3d4e5f6", 123, None, HEAD_SHA.upper() + "!"):
            with self.subTest(sha=sha):
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError):
                    forge.handle("review.post", self.gh_request(
                        commit_id=sha,
                        body="ok <!-- privateer-review rev-1 -->",
                    ))
                self.assertNoRequests()

    def test_trailing_newline_in_repo_or_sha_refused(self):
        # A `$`-anchored match accepts a trailing newline; fullmatch must not.
        self.gh_routes_pr_view()
        self.gh_routes_review()
        with self.assertRaises(forge.ForgeError):
            forge.handle("pr.view", self.gh_request(repo=GH_REPO + "\n"))
        self.assertNoRequests()
        with self.assertRaises(forge.ForgeError):
            forge.handle("review.post", self.gh_request(
                commit_id=HEAD_SHA + "\n", body=self.review_body()))
        self.assertNoRequests()

    def test_branch_quoted_in_protection_path(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/feature%2Fx/protection"): (404, {
                "message": "Branch not protected"}),
        }
        res = forge.handle("protection.read", self.gh_request(branch="feature/x"))
        self.assertTrue(res["ok"])
        self.assertFalse(res["protected"])

    def test_branch_control_characters_refused(self):
        with self.assertRaises(forge.ForgeError):
            forge.handle("protection.read", self.gh_request(branch="main\nx-evil"))

    def test_pr_view_head_sha_from_server_validated(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": "not-a-real-sha"},
            }),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request(required_checks=["ci"]))
        self.assertIn("PR head sha", str(ctx.exception))

    # ---------------------------------------------------------------------
    # pr.view checks (blocking finding 5)
    # ---------------------------------------------------------------------

    def test_github_pr_view_success(self):
        self.gh_routes_pr_view()
        res = forge.handle("pr.view", self.gh_request(
            required_checks=["runtime (ubuntu-latest)", "runtime (macos-latest)"]))
        self.assertTrue(res["ok"])
        self.assertEqual(res["state"], "open")
        self.assertEqual(res["base"], "main")
        self.assertEqual(res["base_sha"], BASE_SHA)
        self.assertEqual(res["head_sha"], HEAD_SHA)
        self.assertTrue(res["mergeable"])
        self.assertFalse(res["merged"])
        self.assertEqual(res["checks"]["runtime (ubuntu-latest)"], "success")
        self.assertEqual(res["checks"]["runtime (macos-latest)"], "success")
        self.assertTrue(res["required_checks_ok"])
        self.assertEqual(res["checks_not_success"], [])

        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 3)
        self.assertEqual(reqs[0]["headers"].get("authorization"), f"Bearer {TOKEN}")
        self.assertEqual(reqs[0]["headers"].get("accept"), "application/vnd.github+json")
        self.assertEqual(reqs[0]["headers"].get("x-github-api-version"), "2022-11-28")

    def test_github_pr_view_required_check_missing_or_failing_not_ok(self):
        self.gh_routes_pr_view(
            statuses=[{"context": "runtime (ubuntu-latest)", "state": "failure"}],
            check_runs=[],
        )
        res = forge.handle("pr.view", self.gh_request(
            required_checks=["runtime (ubuntu-latest)", "runtime (macos-latest)"]))
        self.assertTrue(res["ok"])
        self.assertFalse(res["required_checks_ok"])
        self.assertEqual(res["checks_not_success"],
                         ["runtime (ubuntu-latest)", "runtime (macos-latest)"])

    def test_github_pr_view_failed_status_read_is_error(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
            }),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/status"): (500, {"message": "boom"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request(required_checks=["ci"]))
        self.assertIn("combined status", str(ctx.exception))
        self.assertEqual(ctx.exception.status_code, 500)

    def test_github_pr_view_failed_check_runs_read_is_error(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
            }),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/status"): (200, {"statuses": []}),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/check-runs"): (403, {"message": "no"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.gh_request(required_checks=["ci"]))
        self.assertIn("check runs", str(ctx.exception))
        self.assertEqual(ctx.exception.status_code, 403)

    def test_required_checks_must_be_non_empty_list(self):
        self.gh_routes_pr_view()
        for value in ([], None, "ci", [""], 7):
            with self.subTest(value=value):
                self.server.clear_requests()
                request = self.gh_request(required_checks=["ci"])
                if value is None:
                    request.pop("required_checks")
                else:
                    request["required_checks"] = value
                with self.assertRaises(forge.ForgeError) as ctx:
                    forge.handle("pr.view", request)
                self.assertIn("required_checks", str(ctx.exception))
                self.assertNoRequests()

    def test_github_pr_view_check_runs_pagination(self):
        filler = [{"name": f"check-{i}", "status": "completed", "conclusion": "success"}
                  for i in range(100)]

        def check_runs_page(query):
            page = query.get("page", ["1"])[0]
            if page == "1":
                return 200, {"total_count": 101, "check_runs": filler}
            return 200, {"total_count": 101, "check_runs": [
                {"name": "final-check", "status": "completed", "conclusion": "success"}]}

        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
            }),
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/status"): (200, {"statuses": []}),
        }
        self.server.page_routes = {
            ("GET", f"/repos/{GH_REPO}/commits/{HEAD_SHA}/check-runs"): check_runs_page,
        }
        res = forge.handle("pr.view", self.gh_request(required_checks=["ci"]))
        self.assertTrue(res["ok"])
        pages = [p for p in self.request_paths() if "/check-runs" in p]
        self.assertEqual(len(pages), 2)
        self.assertIn("per_page=100", self.server.get_requests()[-1]["path"])
        self.assertIn("page=2", self.server.get_requests()[-1]["path"])

    def test_github_worst_result_kept_for_shared_check_names(self):
        # When a status context and a check run (or two check runs) share a
        # name, the worst result wins regardless of which one was read last.
        cases = [
            ("status success, check run failure",
             [{"context": "shared-ci", "state": "success"}],
             [{"name": "shared-ci", "status": "completed", "conclusion": "failure"}],
             "failure"),
            ("check run failure then success",
             [],
             [{"name": "shared-ci", "status": "completed", "conclusion": "failure"},
              {"name": "shared-ci", "status": "completed", "conclusion": "success"}],
             "failure"),
            ("check run success then in-progress",
             [],
             [{"name": "shared-ci", "status": "completed", "conclusion": "success"},
              {"name": "shared-ci", "status": "in_progress"}],
             "in_progress"),
        ]
        for label, statuses, check_runs, expected in cases:
            with self.subTest(label=label):
                self.gh_routes_pr_view(statuses=statuses, check_runs=check_runs)
                self.server.clear_requests()
                res = forge.handle("pr.view", self.gh_request(
                    required_checks=["shared-ci"]))
                self.assertTrue(res["ok"])
                self.assertEqual(res["checks"]["shared-ci"], expected)
                self.assertFalse(res["required_checks_ok"])
                self.assertEqual(res["checks_not_success"], ["shared-ci"])

    def test_forgejo_pr_view_uses_combined_status_not_flat_list(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/pulls/5"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
                "mergeable": True,
                "merged": False,
            }),
            ("GET", f"/api/v1/repos/{FJ_REPO}/commits/{HEAD_SHA}/status"): (200, {
                "state": "success",
                "statuses": [
                    {"context": "continuous-integration", "status": "success"},
                    {"context": "lint", "status": "success"},
                ],
            }),
        }
        res = forge.handle("pr.view", self.fj_request(
            required_checks=["continuous-integration"]))
        self.assertTrue(res["ok"])
        self.assertEqual(res["head_sha"], HEAD_SHA)
        self.assertEqual(res["checks"]["continuous-integration"], "success")
        self.assertEqual(res["checks"]["lint"], "success")
        self.assertTrue(res["required_checks_ok"])
        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 2)
        self.assertEqual(reqs[0]["headers"].get("authorization"), f"token {TOKEN}")
        self.assertEqual(reqs[0]["headers"].get("accept"), "application/json")
        self.assertNotIn(f"/api/v1/repos/{FJ_REPO}/commits/{HEAD_SHA}/statuses",
                         self.request_paths())

    def test_forgejo_pr_view_failed_status_read_is_error(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/pulls/5"): (200, {
                "state": "open",
                "base": {"ref": "main", "sha": BASE_SHA},
                "head": {"sha": HEAD_SHA},
            }),
            ("GET", f"/api/v1/repos/{FJ_REPO}/commits/{HEAD_SHA}/status"): (404, {"message": "no"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.view", self.fj_request(required_checks=["ci"]))
        self.assertEqual(ctx.exception.status_code, 404)

    # ---------------------------------------------------------------------
    # review.post marker matching (blocking finding 4)
    # ---------------------------------------------------------------------

    def review_body(self, review_id="rev-uuid-123", text="Looks good!"):
        return f"{text}\n<!-- privateer-review {review_id} -->"

    def test_github_review_post_new(self):
        self.gh_routes_review(existing=[])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])
        self.assertFalse(res["already_exists"])
        self.assertEqual(res["review_id"], 9901)
        self.assertNotIn("id", res)

        reqs = self.server.get_requests()
        self.assertEqual([r["method"] for r in reqs], ["GET", "GET", "POST"])
        self.assertEqual(reqs[0]["path"], "/user")
        self.assertTrue(reqs[1]["path"].startswith(f"/repos/{GH_REPO}/pulls/10/reviews"))
        self.assertIn("per_page=100", reqs[1]["path"])
        self.assertTrue(reqs[2]["path"].startswith(f"/repos/{GH_REPO}/pulls/10/reviews"))
        self.assertEqual(reqs[2]["body"]["commit_id"], HEAD_SHA)
        self.assertEqual(reqs[2]["body"]["event"], "APPROVE")
        self.assertIn("<!-- privateer-review rev-uuid-123 -->", reqs[2]["body"]["body"])
        self.assertEqual(reqs[2]["headers"].get("authorization"), f"Bearer {TOKEN}")

    def test_pr_comment_posts_once_per_marker_on_both_forges(self):
        marker = "<!-- privateer-review 0123abcd -->"
        body = f"{marker}\n**Privateer review: APPROVE**"
        for forge_kind, request, prefix, identity in (
                ("github", self.gh_request, "", GH_IDENTITY),
                ("forgejo", self.fj_request, "/api/v1", FJ_IDENTITY)):
            repo, pr = (GH_REPO, 10) if forge_kind == "github" else (FJ_REPO, 5)
            path = f"{prefix}/repos/{repo}/issues/{pr}/comments"
            self.server.clear_requests()
            # Another login's comment with the marker does not count.
            self.server.routes = {
                ("GET", f"{prefix}/user"): (200, {"login": identity, "username": identity}),
                ("GET", path): (200, [{"id": 1, "user": {"login": "someone"}, "body": body}]),
                ("POST", path): (201, {"id": 77}),
            }
            res = forge.handle("pr.comment", request(marker=marker, body=body))
            self.assertEqual((res["posted"], res["comment_id"]), (True, 77), forge_kind)
            posted = [r for r in self.server.get_requests() if r["method"] == "POST"]
            self.assertEqual(posted[0]["path"], path)
            self.assertEqual(posted[0]["body"], {"body": body})
            # Its own earlier comment is returned, and nothing is posted.
            self.server.clear_requests()
            self.server.routes[("GET", path)] = (
                200, [{"id": 5, "user": {"login": identity, "username": identity}, "body": body}])
            res = forge.handle("pr.comment", request(marker=marker, body=body))
            self.assertEqual((res["posted"], res["comment_id"]), (False, 5), forge_kind)
            self.assertNotIn("POST", [r["method"] for r in self.server.get_requests()])
        with self.assertRaises(forge.ForgeError):
            forge.handle("pr.comment", self.gh_request(marker=marker, body="no marker here"))

    def test_github_review_post_idempotent_marker_match(self):
        self.gh_routes_review(existing=[{
            "id": 8801,
            "user": {"login": GH_IDENTITY},
            "body": self.review_body(text="Earlier run"),
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body(text="Replay")))
        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 8801)
        methods = [r["method"] for r in self.server.get_requests()]
        self.assertEqual(methods, ["GET", "GET"])  # no POST on a replay

    def test_marker_by_another_user_does_not_count(self):
        self.gh_routes_review(existing=[{
            "id": 8802,
            "user": {"login": "evil-impersonator"},
            "body": self.review_body(),
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])
        self.assertEqual([r["method"] for r in self.server.get_requests()],
                         ["GET", "GET", "POST"])

    def test_bare_id_marker_does_not_count(self):
        self.gh_routes_review(existing=[{
            "id": 8803,
            "user": {"login": GH_IDENTITY},
            "body": "Earlier run mentions rev-uuid-123 without the marker",
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])

    def test_marker_on_another_commit_does_not_count(self):
        self.gh_routes_review(existing=[{
            "id": 8804,
            "user": {"login": GH_IDENTITY},
            "body": self.review_body(),
            "commit_id": OTHER_SHA,
            "state": "APPROVED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])

    def test_dismissed_marker_review_does_not_count(self):
        self.gh_routes_review(existing=[{
            "id": 8805,
            "user": {"login": GH_IDENTITY},
            "body": self.review_body(),
            "commit_id": HEAD_SHA,
            "state": "DISMISSED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])

    def test_marker_review_with_other_state_does_not_count(self):
        self.gh_routes_review(existing=[{
            "id": 8806,
            "user": {"login": GH_IDENTITY},
            "body": self.review_body(),
            "commit_id": HEAD_SHA,
            "state": "COMMENTED",
        }])
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])

    def test_reviewer_claim_not_matching_token_identity_refused(self):
        self.gh_routes_review()
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("review.post", self.gh_request(
                commit_id=HEAD_SHA, body=self.review_body(),
                reviewer="someone-else"))
        self.assertIn("does not match", str(ctx.exception))
        # Only the identity read happened; nothing was posted.
        self.assertEqual([r["method"] for r in self.server.get_requests()], ["GET"])

    def test_review_body_without_marker_refused(self):
        self.gh_routes_review()
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("review.post", self.gh_request(
                commit_id=HEAD_SHA, body="no marker here"))
        self.assertIn("marker", str(ctx.exception))
        self.assertEqual([r["method"] for r in self.server.get_requests()], ["GET"])

    def test_unknown_review_event_refused(self):
        self.gh_routes_review()
        with self.assertRaises(forge.ForgeError):
            forge.handle("review.post", self.gh_request(
                commit_id=HEAD_SHA, body=self.review_body(), event="MAGIC"))
        self.assertNoRequests()

    def test_failed_identity_read_is_error(self):
        self.server.routes = {
            ("GET", "/user"): (401, {"message": "Bad credentials"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("review.post", self.gh_request(
                commit_id=HEAD_SHA, body=self.review_body()))
        self.assertEqual(ctx.exception.status_code, 401)

    def test_review_post_follows_review_pagination(self):
        filler = [{"id": 7000 + i, "user": {"login": GH_IDENTITY},
                   "body": "noise", "commit_id": HEAD_SHA, "state": "APPROVED"}
                  for i in range(100)]
        matching = {
            "id": 8807,
            "user": {"login": GH_IDENTITY},
            "body": self.review_body(text="Page two"),
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }

        def reviews_page(query):
            page = query.get("page", ["1"])[0]
            return (200, filler) if page == "1" else (200, [matching])

        self.server.routes = {
            ("GET", "/user"): (200, {"login": GH_IDENTITY}),
            ("POST", f"/repos/{GH_REPO}/pulls/10/reviews"): (200, {"id": 9901}),
        }
        self.server.page_routes = {
            ("GET", f"/repos/{GH_REPO}/pulls/10/reviews"): reviews_page,
        }
        res = forge.handle("review.post", self.gh_request(
            commit_id=HEAD_SHA, event="APPROVE", body=self.review_body()))
        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 8807)
        methods = [r["method"] for r in self.server.get_requests()]
        self.assertEqual(methods, ["GET", "GET", "GET"])  # identity, page 1, page 2; no POST

    def test_forgejo_review_post_approved_event_shape(self):
        self.fj_routes_review(existing=[])
        res = forge.handle("review.post", self.fj_request(
            commit_id=HEAD_SHA, event="APPROVE",  # caller sends APPROVE
            body=self.review_body("fj-rev-9", text="Verdict: approve"),
            reviewer=FJ_IDENTITY))
        self.assertTrue(res["ok"])
        self.assertTrue(res["posted"])
        self.assertEqual(res["review_id"], 4321)
        reqs = self.server.get_requests()
        self.assertEqual([r["method"] for r in reqs], ["GET", "GET", "POST"])
        self.assertEqual(reqs[0]["path"], "/api/v1/user")
        self.assertEqual(reqs[2]["body"]["event"], "APPROVED")  # Forgejo spelling
        self.assertEqual(reqs[2]["body"]["commit_id"], HEAD_SHA)
        self.assertEqual(reqs[2]["headers"].get("authorization"), f"token {TOKEN}")

    def test_forgejo_review_post_idempotent_marker_match(self):
        self.fj_routes_review(existing=[{
            "id": 4320,
            "user": {"username": FJ_IDENTITY},
            "body": self.review_body("fj-rev-9", text="Previous run"),
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }])
        res = forge.handle("review.post", self.fj_request(
            commit_id=HEAD_SHA, event="APPROVE",
            body=self.review_body("fj-rev-9", text="Replay")))
        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 4320)
        self.assertEqual([r["method"] for r in self.server.get_requests()],
                         ["GET", "GET"])

    def test_forgejo_dismissed_review_does_not_count(self):
        # Forgejo keeps a dismissed review's state APPROVED and flags it with
        # dismissed (and possibly stale) instead of changing the state.
        for flags in ({"dismissed": True}, {"stale": True},
                      {"dismissed": True, "stale": True},
                      {"dismissed": False, "stale": False}):
            with self.subTest(flags=flags):
                self.fj_routes_review(existing=[{
                    "id": 4330,
                    "user": {"username": FJ_IDENTITY},
                    "body": self.review_body("fj-rev-11", text="Dismissed run"),
                    "commit_id": HEAD_SHA,
                    "state": "APPROVED",
                    **flags,
                }])
                self.server.clear_requests()
                res = forge.handle("review.post", self.fj_request(
                    commit_id=HEAD_SHA, event="APPROVE",
                    body=self.review_body("fj-rev-11", text="Replay")))
                self.assertTrue(res["ok"])
                if any(flags.values()):
                    self.assertTrue(res["posted"])  # the dismissed review does not count
                    self.assertEqual(
                        [r["method"] for r in self.server.get_requests()],
                        ["GET", "GET", "POST"])
                else:
                    self.assertFalse(res["posted"])  # a plain APPROVED review still matches
                    self.assertEqual(
                        [r["method"] for r in self.server.get_requests()],
                        ["GET", "GET"])

    def test_forgejo_review_post_follows_limit_pagination(self):
        # Forgejo pages with `limit` (default 30, usual max 50), never with
        # per_page. The fixture honours limit and serves its 30-item default
        # page when it is absent, so the adapter must send limit=50 to reach
        # the matching review on page two.
        matching = {
            "id": 4331,
            "user": {"username": FJ_IDENTITY},
            "body": self.review_body("fj-rev-12", text="Page two"),
            "commit_id": HEAD_SHA,
            "state": "APPROVED",
        }

        def reviews_page(query):
            size = int(query.get("limit", ["30"])[0])  # Forgejo's default page
            page = int(query.get("page", ["1"])[0])
            if page == 1:
                return 200, [{"id": 6000 + i, "user": {"username": FJ_IDENTITY},
                              "body": "noise", "commit_id": HEAD_SHA,
                              "state": "APPROVED"}
                             for i in range(size)]
            return 200, [matching]

        self.server.routes = {
            ("GET", "/api/v1/user"): (200, {"username": FJ_IDENTITY}),
            ("POST", f"/api/v1/repos/{FJ_REPO}/pulls/5/reviews"): (200, {"id": 4321}),
        }
        self.server.page_routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/pulls/5/reviews"): reviews_page,
        }
        res = forge.handle("review.post", self.fj_request(
            commit_id=HEAD_SHA, event="APPROVE",
            body=self.review_body("fj-rev-12")))
        self.assertTrue(res["ok"])
        self.assertFalse(res["posted"])
        self.assertTrue(res["already_exists"])
        self.assertEqual(res["review_id"], 4331)

        review_reqs = [r for r in self.server.get_requests() if "/reviews" in r["path"]]
        self.assertEqual(len(review_reqs), 2)  # page two was short, no third page
        self.assertIn("limit=50", review_reqs[0]["path"])
        self.assertIn("page=1", review_reqs[0]["path"])
        self.assertIn("page=2", review_reqs[1]["path"])
        for req in review_reqs:
            self.assertNotIn("per_page", req["path"])

    def test_forgejo_pagination_keeps_page_cap(self):
        def reviews_page(query):
            size = int(query.get("limit", ["30"])[0])
            return 200, [{"id": 6000 + i, "user": {"username": FJ_IDENTITY},
                          "body": "noise", "commit_id": HEAD_SHA,
                          "state": "APPROVED"}
                         for i in range(size)]

        self.server.routes = {
            ("GET", "/api/v1/user"): (200, {"username": FJ_IDENTITY}),
        }
        self.server.page_routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/pulls/5/reviews"): reviews_page,
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("review.post", self.fj_request(
                commit_id=HEAD_SHA, event="APPROVE",
                body=self.review_body("fj-rev-13")))
        self.assertIn("pagination exceeded", str(ctx.exception))
        review_reqs = [r for r in self.server.get_requests() if "/reviews" in r["path"]]
        self.assertEqual(len(review_reqs), forge.MAX_PAGES)

    # ---------------------------------------------------------------------
    # pr.merge
    # ---------------------------------------------------------------------

    def test_github_merge_success(self):
        self.server.routes = {
            ("PUT", f"/repos/{GH_REPO}/pulls/10/merge"): (200, {
                "sha": OTHER_SHA, "merged": True,
                "message": "Pull Request successfully merged"}),
        }
        res = forge.handle("pr.merge", self.gh_request(
            head_sha=HEAD_SHA, merge_method="merge"))
        self.assertTrue(res["ok"])
        self.assertTrue(res["merged"])
        self.assertEqual(res["merge_sha"], OTHER_SHA)
        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 1)
        self.assertEqual(reqs[0]["method"], "PUT")
        self.assertEqual(reqs[0]["body"]["sha"], HEAD_SHA)
        self.assertEqual(reqs[0]["body"]["merge_method"], "merge")
        self.assertEqual(reqs[0]["headers"].get("authorization"), f"Bearer {TOKEN}")

    def test_github_merge_head_moved_is_error(self):
        self.server.routes = {
            ("PUT", f"/repos/{GH_REPO}/pulls/10/merge"): (409, {
                "message": "Head branch was modified. Review and try the merge again."}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.merge", self.gh_request(head_sha=HEAD_SHA))
        self.assertEqual(ctx.exception.status_code, 409)
        self.assertIn("Head branch was modified", str(ctx.exception))

    def test_merge_method_must_be_merge(self):
        for method in ("squash", "rebase", "fast-forward"):
            with self.subTest(method=method):
                self.server.clear_requests()
                with self.assertRaises(forge.ForgeError) as ctx:
                    forge.handle("pr.merge", self.gh_request(
                        head_sha=HEAD_SHA, merge_method=method))
                self.assertIn("only 'merge'", str(ctx.exception))
                self.assertNoRequests()

    def test_method_key_is_not_the_merge_method(self):
        # `method` names the operation (the CLI fallback in main), never the
        # merge method: pr.merge with method="pr.merge" must still merge.
        self.server.routes = {
            ("PUT", f"/repos/{GH_REPO}/pulls/10/merge"): (200, {
                "sha": OTHER_SHA, "merged": True, "message": "merged"}),
        }
        res = forge.handle("pr.merge", self.gh_request(
            head_sha=HEAD_SHA, method="pr.merge"))
        self.assertTrue(res["ok"])
        self.assertTrue(res["merged"])

    def test_forgejo_merge_success_shape(self):
        self.server.routes = {
            ("POST", f"/api/v1/repos/{FJ_REPO}/pulls/5/merge"): (200, {
                "sha": OTHER_SHA, "merged": True, "message": "merged"}),
        }
        res = forge.handle("pr.merge", self.fj_request(head_sha=HEAD_SHA))
        self.assertTrue(res["ok"])
        self.assertTrue(res["merged"])
        self.assertEqual(res["merge_sha"], OTHER_SHA)
        reqs = self.server.get_requests()
        self.assertEqual(len(reqs), 1)
        self.assertEqual(reqs[0]["method"], "POST")
        self.assertEqual(reqs[0]["body"]["head_commit_id"], HEAD_SHA)
        self.assertEqual(reqs[0]["body"]["Do"], "merge")
        self.assertEqual(reqs[0]["headers"].get("authorization"), f"token {TOKEN}")

    def test_forgejo_merge_refusal_is_error(self):
        self.server.routes = {
            ("POST", f"/api/v1/repos/{FJ_REPO}/pulls/5/merge"): (405, {
                "message": "Merge branch is not allowed for this pull request"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("pr.merge", self.fj_request(head_sha=HEAD_SHA))
        self.assertEqual(ctx.exception.status_code, 405)
        self.assertIn("not allowed", str(ctx.exception))

    # ---------------------------------------------------------------------
    # protection.read
    # ---------------------------------------------------------------------

    def gh_compliant_protection(self):
        return {
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
        }

    def test_github_protection_read_compliant(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (
                200, self.gh_compliant_protection()),
        }
        res = forge.handle("protection.read", self.gh_request(
            branch="main",
            required_checks=["runtime (ubuntu-latest)", "runtime (macos-latest)"]))
        self.assertTrue(res["ok"])
        self.assertTrue(res["protected"])
        self.assertTrue(res["enforce_admins"])
        self.assertEqual(res["required_approvals"], 1)
        self.assertTrue(res["require_code_owner_reviews"])
        self.assertTrue(res["dismiss_stale_approvals"])
        self.assertTrue(res["require_last_push_approval"])
        self.assertEqual(res["warnings"], [])

    def test_github_protection_unprotected_only_by_message(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (404, {
                "message": "Branch not protected"}),
        }
        res = forge.handle("protection.read", self.gh_request(branch="main"))
        self.assertTrue(res["ok"])
        self.assertFalse(res["protected"])

    def test_github_protection_other_404_is_error(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (404, {
                "message": "Not Found"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("protection.read", self.gh_request(branch="main"))
        self.assertEqual(ctx.exception.status_code, 404)

    def test_github_protection_403_is_error(self):
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (403, {
                "message": "Must have admin rights"}),
        }
        with self.assertRaises(forge.ForgeError) as ctx:
            forge.handle("protection.read", self.gh_request(branch="main"))
        self.assertEqual(ctx.exception.status_code, 403)
        self.assertIn("admin", str(ctx.exception))

    def test_github_protection_no_required_status_checks_warns(self):
        protection = self.gh_compliant_protection()
        protection["required_status_checks"] = {"contexts": [], "checks": []}
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (200, protection),
        }
        res = forge.handle("protection.read", self.gh_request(branch="main"))
        self.assertTrue(res["ok"])
        self.assertIn("no required status checks are configured", res["warnings"])

    def test_github_protection_missing_settings_warn(self):
        protection = self.gh_compliant_protection()
        protection["enforce_admins"] = {"enabled": False}
        protection["required_pull_request_reviews"] = {
            "required_approving_review_count": 0,
            "require_code_owner_reviews": False,
            "dismiss_stale_reviews": False,
            "require_last_push_approval": False,
        }
        protection["required_status_checks"] = {
            "contexts": ["runtime (ubuntu-latest)"], "checks": []}
        protection["allow_force_pushes"] = {"enabled": True}
        protection["allow_deletions"] = {"enabled": True}
        self.server.routes = {
            ("GET", f"/repos/{GH_REPO}/branches/main/protection"): (200, protection),
        }
        res = forge.handle("protection.read", self.gh_request(
            branch="main",
            required_checks=["runtime (ubuntu-latest)", "runtime (macos-latest)"]))
        self.assertTrue(res["ok"])
        self.assertIn("enforce_admins is not enabled", res["warnings"])
        self.assertIn("required approving review count is less than 1", res["warnings"])
        self.assertIn("require code owner reviews is not enabled", res["warnings"])
        self.assertIn("dismiss stale reviews is not enabled", res["warnings"])
        self.assertIn("require approval of most recent push is not enabled", res["warnings"])
        self.assertIn("force pushes are allowed", res["warnings"])
        self.assertIn("branch deletions are allowed", res["warnings"])
        self.assertTrue(any("runtime (macos-latest)" in w for w in res["warnings"]))

    def fj_compliant_protection(self):
        return {
            "branch_name": "main",
            "enable_push": False,
            "apply_to_admins": True,
            "required_approvals": 1,
            "enable_approvals_whitelist": True,
            "approvals_whitelist_usernames": [FJ_IDENTITY],
            "enable_merge_whitelist": True,
            "merge_whitelist_usernames": ["hermes-merger"],
            "dismiss_stale_approvals": True,
            "block_on_rejected_reviews": True,
            "enable_status_check": True,
            "status_check_contexts": ["ci-check"],
            "enable_force_push": False,
        }

    def test_forgejo_protection_read_compliant(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections/main"): (
                200, self.fj_compliant_protection()),
        }
        res = forge.handle("protection.read", self.fj_request(
            branch="main", reviewer=FJ_IDENTITY, merger="hermes-merger",
            required_checks=["ci-check"]))
        self.assertTrue(res["ok"])
        self.assertTrue(res["protected"])
        self.assertFalse(res["enable_push"])
        self.assertTrue(res["apply_to_admins"])
        self.assertEqual(res["approvals_whitelist"], [FJ_IDENTITY])
        self.assertEqual(res["merge_whitelist"], ["hermes-merger"])
        self.assertEqual(res["warnings"], [])

    def test_forgejo_protection_requires_reviewer_and_merger(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections/main"): (
                200, self.fj_compliant_protection()),
        }
        for overrides in ({"reviewer": None}, {"merger": None}):
            with self.subTest(overrides=overrides):
                self.server.clear_requests()
                request = self.fj_request(branch="main", reviewer=FJ_IDENTITY,
                                          merger="hermes-merger")
                for key, value in overrides.items():
                    if value is None:
                        request.pop(key)
                with self.assertRaises(forge.ForgeError) as ctx:
                    forge.handle("protection.read", request)
                self.assertIn("required", str(ctx.exception))
                self.assertNoRequests()

    def test_forgejo_protection_whitelist_others_and_force_push_warn(self):
        protection = self.fj_compliant_protection()
        protection["approvals_whitelist_usernames"] = [FJ_IDENTITY, "someone-else"]
        protection["merge_whitelist_usernames"] = ["hermes-merger", "intruder"]
        protection["enable_force_push"] = True
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections/main"): (
                200, protection),
        }
        res = forge.handle("protection.read", self.fj_request(
            branch="main", reviewer=FJ_IDENTITY, merger="hermes-merger"))
        self.assertTrue(res["ok"])
        self.assertIn("approvals whitelist contains a user other than the reviewer: "
                      "'someone-else'", res["warnings"])
        self.assertIn("merge whitelist contains a user other than the merger: "
                      "'intruder'", res["warnings"])
        self.assertIn("force pushes are enabled (enable_force_push)", res["warnings"])

    def test_forgejo_protection_missing_settings_warn(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections/main"): (200, {
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
        res = forge.handle("protection.read", self.fj_request(
            branch="main", reviewer=FJ_IDENTITY, merger="hermes-merger",
            required_checks=["ci-check"]))
        self.assertTrue(res["ok"])
        self.assertIn("enable_push is not false", res["warnings"])
        self.assertIn("apply_to_admins is not true", res["warnings"])
        self.assertIn("required_approvals is less than 1", res["warnings"])
        self.assertIn("enable_approvals_whitelist is not true", res["warnings"])
        self.assertIn(f"reviewer '{FJ_IDENTITY}' not in approvals_whitelist_usernames",
                      res["warnings"])
        self.assertIn("enable_merge_whitelist is not true", res["warnings"])
        self.assertIn("merger 'hermes-merger' not in merge_whitelist_usernames",
                      res["warnings"])
        self.assertIn("dismiss_stale_approvals is not true", res["warnings"])
        self.assertIn("block_on_rejected_reviews is not true", res["warnings"])
        self.assertIn("enable_status_check is not true", res["warnings"])
        self.assertTrue(any("ci-check" in w for w in res["warnings"]))

    def test_forgejo_protection_unprotected_via_list_fallback(self):
        self.server.routes = {
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections/main"): (404, {
                "message": "Not Found"}),
            ("GET", f"/api/v1/repos/{FJ_REPO}/branch_protections"): (200, []),
        }
        res = forge.handle("protection.read", self.fj_request(
            branch="main", reviewer=FJ_IDENTITY, merger="hermes-merger"))
        self.assertTrue(res["ok"])
        self.assertFalse(res["protected"])

    # ---------------------------------------------------------------------
    # CLI subprocess boundary
    # ---------------------------------------------------------------------

    def run_cli(self, argv_op, request, env=None):
        run_env = os.environ.copy()
        if env:
            run_env.update(env)
        return subprocess.run(
            [sys.executable, str(ROOT / "plugins/forge.py"), argv_op],
            input=json.dumps(request), capture_output=True, text=True, env=run_env,
        )

    def test_cli_subprocess_with_token_file(self):
        self.gh_routes_pr_view()
        proc = self.run_cli("pr.view", self.gh_request(
            id="cli-test-req-1", required_checks=["runtime (ubuntu-latest)"]))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        data = json.loads(proc.stdout)
        self.assertTrue(data["ok"])
        self.assertEqual(data["request_id"], "cli-test-req-1")
        self.assertNotIn("id", data)  # the request id never overwrites a result id
        self.assertEqual(data["head_sha"], HEAD_SHA)
        self.assertEqual(
            self.server.get_requests()[0]["headers"].get("authorization"),
            f"Bearer {TOKEN}")

    def test_cli_subprocess_invalid_json(self):
        run_env = os.environ.copy()
        proc = subprocess.run(
            [sys.executable, str(ROOT / "plugins/forge.py"), "pr.view"],
            input="not valid json", capture_output=True, text=True, env=run_env)
        self.assertEqual(proc.returncode, 0)
        data = json.loads(proc.stdout)
        self.assertFalse(data["ok"])
        self.assertIn("JSON", data["error"])

    def test_cli_error_reply_does_not_leak_the_token(self):
        secret = "SUPERSECRET-leakcheck-token"
        self.set_token_file(secret + "\n")
        proc = self.run_cli("pr.view", self.gh_request(
            api="https://127.0.0.1:9", required_checks=["ci"]))
        data = json.loads(proc.stdout)
        self.assertFalse(data["ok"])
        self.assertNotIn(secret, proc.stdout)
        self.assertNotIn(secret, proc.stderr)


if __name__ == "__main__":
    unittest.main()
