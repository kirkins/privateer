#!/usr/bin/env python3
"""Isolated real HTTP OAuth flows: handwritten client and pinned MCP OAuth client."""
import asyncio
import base64
from contextlib import closing
import errno
import hashlib
import html
import json
import os
from pathlib import Path
import re
import select
import signal
import sqlite3
import subprocess
import sys
import time
import unittest
from urllib.parse import parse_qs, urlencode, urlsplit

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "plugins/mcp"))
from mcp_door_test import BIN, Fleet, session
from oauth_http import (COMPLETED_SECONDS, CONSENT_FIELD_LIMIT, SUBMIT_HASH, SUBMIT_SCRIPT, TRANSACTION_SECONDS,
                        FormFieldLimit, OAuthHTTP, SourceLimits, configuration, form, proxy_networks, source_address)
import httpx2
from mcp.client.auth import OAuthClientProvider
from mcp.shared.auth import AuthorizationCodeResult, OAuthClientInformationFull, OAuthClientMetadata

REDIRECT = "https://chatgpt.com/connector_platform_oauth_redirect"
VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"


def owner(fleet, args, action, response=None, typeahead=None, fails=False, group="oauth-client"):
    master, slave = os.openpty()
    if typeahead:
        os.write(master, typeahead.encode())
    bootstrap = 'import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv(sys.argv[1],sys.argv[1:])'
    child = subprocess.Popen([sys.executable, "-c", bootstrap, str(BIN), "door", group, *args],
                             env=fleet.env, cwd=fleet.root, stdin=slave, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True)
    os.close(slave)
    tty, out, err = bytearray(), bytearray(), bytearray()
    streams = {master: tty, child.stdout.fileno(): out, child.stderr.fileno(): err}
    confirmed = False
    try:
        deadline = time.monotonic() + 20
        while streams:
            if time.monotonic() > deadline:
                raise AssertionError("owner fixture timeout")
            ready, _, _ = select.select(list(streams), [], [], .1)
            if not ready and child.poll() is not None:
                break
            for fd in ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError as error:
                    if fd != master or error.errno != errno.EIO:
                        raise
                    chunk = b""
                if not chunk:
                    del streams[fd]
                    continue
                streams[fd].extend(chunk)
                if not confirmed and f"Type {action} to confirm:".encode() in tty:
                    os.write(master, ((response if response is not None else action) + "\n").encode())
                    confirmed = True
        child.wait(timeout=5)
        if fails:
            if not child.returncode:
                raise AssertionError("owner fixture unexpectedly succeeded")
            return json.loads(err), tty.decode()
        if child.returncode:
            raise AssertionError("owner fixture failed (terminal output withheld)")
        return json.loads(out), tty.decode()
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        child.stdout.close()
        child.stderr.close()


def csrf(page):
    return html.unescape(re.search(r'name="csrf" value="([^"]+)"', page).group(1))


class ConfigurationTests(unittest.TestCase):
    def test_remote_config_and_trust(self):
        cfg = {"public_url": "https://example.com/door", "allowed_hosts": ["example.com"],
               "allowed_origins": ["https://example.com"], "trusted_proxies": ["192.0.2.1"], "bind": "192.0.2.2"}
        self.assertEqual(configuration(cfg)[1:3], ("https://example.com/door", "/door"))
        for bad in ({"bind": "0.0.0.0"}, {"trusted_proxies": ["*"]}, {"unknown": True},
                    {**cfg, "public_url": "http://example.com"}, {**cfg, "public_url": "https://example.com?x"},
                    {**cfg, "allowed_origins": []}, {"public_url": ""}, {"public_url": 42},
                    {**cfg, "bind": "0.0.0.0"}, {**cfg, "bind": "::"}, {"log_requests": "true"}, {"log_requests": 1}):
            with self.assertRaises((ValueError, TypeError)):
                configuration(bad)
        headers = {b"x-forwarded-for": "198.51.100.3", b"x-forwarded-proto": "https"}
        self.assertEqual(source_address({"client": ("127.0.0.1", 1)}, headers, set()), "127.0.0.1")
        proxies = proxy_networks(["192.0.2.1"])
        self.assertEqual(source_address({"client": ("192.0.2.1", 1)}, headers, proxies), "198.51.100.3")
        with self.assertRaises(ValueError):
            source_address({"client": ("192.0.2.1", 1)}, {**headers, b"x-forwarded-for": "a,b"}, proxies)
        with self.assertRaises(ValueError):
            source_address({"client": ("192.0.2.1", 1)}, {**headers, b"x-forwarded-proto": "http"}, proxies)

    def test_private_proxy_cidrs_and_specific_private_bind(self):
        cfg = {"public_url": "https://example.com", "allowed_hosts": ["example.com"],
               "allowed_origins": ["https://example.com"], "bind": "10.42.0.1",
               "trusted_proxies": ["10.42.0.0/16", "192.168.10.0/24", "fd00:42::/64", "127.0.0.1"]}
        bind, _, _, proxies = configuration(cfg)
        self.assertEqual(bind, "10.42.0.1")
        headers = {b"x-forwarded-for": "198.51.100.3", b"x-forwarded-proto": "https"}
        for peer in ("10.42.1.7", "10.42.255.254", "192.168.10.4", "fd00:42::7", "127.0.0.1"):
            self.assertEqual(source_address({"client": (peer, 1)}, headers, proxies, remote=True), "198.51.100.3")
        for peer in ("10.43.1.7", "192.168.11.4", "fd00:43::7"):
            self.assertEqual(source_address({"client": (peer, 1)}, headers, proxies), peer,
                             "untrusted forwarding headers are ignored")
            with self.assertRaises(ValueError):
                source_address({"client": (peer, 1)}, headers, proxies, remote=True)
        for value in ("0.0.0.0/0", "::/0", "10.0.0.0/7", "fc00::/7", "8.8.8.0/24",
                      "2001:4860::/32", "192.0.2.0/24", "10.42.0.1/16", "not-an-ip"):
            with self.subTest(proxy=value), self.assertRaises(ValueError):
                configuration({**cfg, "trusted_proxies": [value]})
        # A whole RFC 1918 /8 is the broadest permitted range; prefer much
        # narrower ranges in real configurations. Pinned exact IPs still work.
        configuration({**cfg, "trusted_proxies": ["10.0.0.0/8", "8.8.8.8"]})

    def test_all_rate_limits_and_bound(self):
        for operation, limit in SourceLimits.VALUES.items():
            limiter = SourceLimits()
            for _ in range(limit):
                self.assertTrue(limiter.admit("192.0.2.1", operation))
            self.assertFalse(limiter.admit("192.0.2.1", operation))
            self.assertTrue(limiter.admit("192.0.2.2", operation))
        limiter = SourceLimits()
        limiter.rows = {(f"10.0.{n // 256}.{n % 256}", "metadata"): (time.monotonic(), 1) for n in range(4096)}
        for operation, limit in SourceLimits.VALUES.items():
            for n in range(limit // 4):
                self.assertTrue(limiter.admit(f"198.51.100.{n}", operation))
            self.assertFalse(limiter.admit("198.51.100.250", operation))
            self.assertTrue(limiter.admit("203.0.113.1", operation), "a flooded prefix cannot spend another's budget")
        self.assertEqual(len(limiter.rows), 4096)
        self.assertEqual(len(limiter.overflow), 2 * len(SourceLimits.VALUES))
        self.assertTrue(limiter.admit("10.0.0.0", "metadata"), "existing buckets keep their budget")
        from unittest.mock import patch
        with patch("oauth_http.time.monotonic", return_value=time.monotonic() + 61):
            self.assertTrue(limiter.admit("203.0.113.1", "metadata"))
            self.assertEqual(len(limiter.overflow), 0, "expired overflow rows are reclaimed too")
        # Cycling host addresses inside one IPv6 allocation shares one budget.
        limiter = SourceLimits()
        for n in range(SourceLimits.VALUES["authorize"]):
            self.assertTrue(limiter.admit(f"2001:db8:1::{n:x}", "authorize"))
        self.assertFalse(limiter.admit("2001:db8:1::ffff", "authorize"))
        self.assertTrue(limiter.admit("2001:db8:2::1", "authorize"))
        self.assertEqual(len(limiter.rows), 2)
        # Mapped IPv4 must not collapse all IPv4 peers into ::ffff:0:0/64.
        limiter = SourceLimits()
        for _ in range(SourceLimits.VALUES["token"]):
            self.assertTrue(limiter.admit("::ffff:192.0.2.1", "token"))
        self.assertFalse(limiter.admit("192.0.2.1", "token"))
        self.assertTrue(limiter.admit("::ffff:192.0.2.2", "token"))
        self.assertEqual(len(limiter.rows), 2)

    def test_overflow_prefix_bound_and_expiry(self):
        from unittest.mock import patch
        limiter = SourceLimits()
        now = time.monotonic()
        limiter.rows = {(f"10.0.{n // 256}.{n % 256}", "metadata"): (now, 1) for n in range(4096)}
        for n in range(SourceLimits.VALUES["token"] // 4):
            self.assertTrue(limiter.admit(f"2001:db8:1:{n:x}::1", "token"))
        self.assertFalse(limiter.admit("2001:db8:1:ffff::1", "token"))
        self.assertTrue(limiter.admit("2001:db8:2::1", "token"))
        self.assertTrue(limiter.admit("::ffff:192.0.2.1", "token"))
        self.assertIn(("192.0.2.0/24", "token"), limiter.overflow)
        for n in range(1100):
            limiter.admit(f"2001:db8:{n + 3:x}::1", "token")
        self.assertEqual(len(limiter.overflow), 1024)
        self.assertFalse(limiter.admit("203.0.113.1", "token"), "new prefixes fail closed at the bound")
        self.assertTrue(limiter.admit("2001:db8:2::2", "token"), "live prefix budgets are not evicted")
        self.assertTrue(limiter.admit("10.0.0.0", "metadata"), "main table budgets are preserved")
        with patch("oauth_http.time.monotonic", return_value=now + 61):
            self.assertTrue(limiter.admit("203.0.113.1", "token"))
        self.assertEqual(len(limiter.rows), 1)
        self.assertEqual(len(limiter.overflow), 0)


class MetadataTests(unittest.IsolatedAsyncioTestCase):
    async def test_concurrent_consent_replays_only_same_session_and_csrf_for_one_minute(self):
        from unittest.mock import patch

        class Core:
            calls = 0

            async def call(self, _, command, args):
                self.calls += 1
                await asyncio.sleep(0)  # Force overlapping POSTs at the core boundary.
                return {"ok": True, "result": {"redirect_uri": "https://example.com/callback",
                                                 "code": "one-code", "state": "bound-state"}}

        core = Core()
        adapter = OAuthHTTP(core, {})
        adapter.sessions["browser"] = {"transaction": "fixture", "csrf": "fixture-csrf", "expires": 500,
                                       "absolute_expires": TRANSACTION_SECONDS,
                                       "summary": {"known_ships": [], "consent_token": "fixture-consent"}}
        adapter.sessions["other"] = {"csrf": "other-csrf", "expires": 500}

        async def post(sid="browser", token="fixture-csrf"):
            headers = {b"content-type": "application/x-www-form-urlencoded",
                       b"cookie": "privateer_oauth_browser=" + sid}
            scope = {"type": "http", "method": "POST", "path": "/consent", "query_string": b"",
                     "headers": [(k, v.encode()) for k, v in headers.items()]}
            return await adapter.handle(scope, None, urlencode({"csrf": token, "decision": "approve"}).encode(), headers, "127.0.0.1")

        with patch("oauth_http.time", monotonic=lambda: 100):
            first, second = await asyncio.gather(post(), post())
            self.assertEqual((first.status_code, second.status_code), (200, 200))
            self.assertEqual(first.body, second.body)
            self.assertEqual(core.calls, 1)
            self.assertNotIn("Max-Age=0", first.headers["set-cookie"])
            self.assertEqual((await post("other")).status_code, 403)
            self.assertEqual((await post(token="wrong")).status_code, 403)
        with patch("oauth_http.time", monotonic=lambda: 159.9):
            self.assertEqual((await post()).body, first.body)
        with patch("oauth_http.time", monotonic=lambda: 160):
            expired = await post()
            self.assertEqual(expired.status_code, 403)
            self.assertIn(b"Sign-in expired", expired.body)
            self.assertIn(b"Retry from the client", expired.body)
            self.assertNotIn("browser", adapter.sessions)
        self.assertEqual(core.calls, 1)

    async def test_consent_script_carries_clicked_decision_without_submitter(self):
        from unittest.mock import patch

        class Core:
            calls = []

            async def call(self, _, command, args):
                self.calls.append(args)
                return {"ok": True, "result": {"redirect_uri": "https://example.com/callback",
                                               "code": "fixture-code", "state": "bound-state"}}

        core = Core()
        adapter = OAuthHTTP(core, {})
        summary = {"known_ships": ["alpha"], "requested_scopes": ["fleet:read"], "display_name": "Example",
                   "client_id": "fake-id", "redirect_uri": "https://example.com/callback",
                   "resource": "https://example.com/mcp", "consent_token": "fixture-consent"}
        adapter.sessions["browser"] = {"transaction": "fixture", "csrf": "fixture-csrf", "expires": 500,
                                       "absolute_expires": TRANSACTION_SECONDS, "lock": asyncio.Lock(),
                                       "summary": summary}
        # A second session models a null-submitter submit with no recorded
        # click: a requestSubmit() from an extension or autofill, or a browser
        # whose script never ran (JS off or failed). Its POST carries no
        # decision at all, and the core must see deny, never approve.
        adapter.sessions["noscript"] = {**adapter.sessions["browser"], "csrf": "noscript-csrf"}

        async def request(method, sid, data):
            headers = {b"content-type": "application/x-www-form-urlencoded",
                       b"cookie": "privateer_oauth_browser=" + sid}
            scope = {"type": "http", "method": method, "path": "/consent", "query_string": b"",
                     "headers": [(k, v.encode()) for k, v in headers.items()]}
            return await adapter.handle(scope, None, urlencode(data).encode(), headers, "127.0.0.1")

        with patch("oauth_http.time", monotonic=lambda: 100):
            page = await request("GET", "browser", {})
            self.assertEqual(page.status_code, 200)
            script = re.search(rb"<script>(.*)</script>", page.body).group(1).decode()
            self.assertEqual(script, SUBMIT_SCRIPT)
            digest = base64.b64encode(hashlib.sha256(script.encode()).digest()).decode()
            self.assertEqual(digest, SUBMIT_HASH, "served script and CSP hash must stay in sync")
            self.assertIn(f"script-src 'sha256-{digest}'", page.headers["content-security-policy"])
            # A click is recorded before submit, so browsers without
            # SubmitEvent.submitter (Safari before 15.4) still bind the
            # clicked decision instead of silently defaulting to deny.
            self.assertIn("addEventListener('click'", script)
            self.assertIn("closest('button')", script)
            self.assertIn("e.submitter?e.submitter.value:c", script)
            # The hidden decision is added only behind the recorded decision
            # guard: no fallback to the form's default (first) button, so a
            # null-submitter submit with no recorded click sends no decision.
            self.assertIn("if(s){var i=document.createElement('input')", script)
            self.assertNotIn("querySelector('button')", script)
            self.assertNotIn("if(e.submitter)", script)
            self.assertIn("b.disabled=true", script)
            self.assertIn('name="decision" value="approve"', page.body.decode())
            self.assertIn('name="decision" value="deny"', page.body.decode())
            # A click on Approve recorded without e.submitter still approves.
            clicked = await request("POST", "browser", {"csrf": "fixture-csrf", "scopes": "fleet:read",
                                                          "decision": "approve"})
            self.assertEqual(clicked.status_code, 200)
            # A null-submitter submit with no recorded click adds no decision
            # field, and the core gets deny for it, never approve.
            bare = await request("POST", "noscript", {"csrf": "noscript-csrf", "scopes": "fleet:read"})
            self.assertEqual(bare.status_code, 200)
        self.assertEqual([args["decision"] for args in core.calls], ["approve", "deny"])

    async def test_completed_replay_window_never_outlives_the_absolute_cap(self):
        from unittest.mock import patch

        class Core:
            async def call(self, _, command, args):
                return {"ok": True, "result": {"redirect_uri": "https://example.com/callback",
                                               "code": "fixture-code", "state": "bound-state"}}

        adapter = OAuthHTTP(Core(), {})
        # The transaction began at t=0, so its absolute cap is
        # TRANSACTION_SECONDS; consent completes 30 s before that cap.
        adapter.sessions["browser"] = {"transaction": "fixture", "csrf": "fixture-csrf",
                                       "expires": TRANSACTION_SECONDS - 20,
                                       "absolute_expires": TRANSACTION_SECONDS, "lock": asyncio.Lock(),
                                       "summary": {"known_ships": [], "consent_token": "fixture-consent"}}

        async def post():
            headers = {b"content-type": "application/x-www-form-urlencoded",
                       b"cookie": "privateer_oauth_browser=browser"}
            scope = {"type": "http", "method": "POST", "path": "/consent", "query_string": b"",
                     "headers": [(k, v.encode()) for k, v in headers.items()]}
            return await adapter.handle(scope, None, urlencode({"csrf": "fixture-csrf",
                                                                 "decision": "approve"}).encode(), headers, "127.0.0.1")

        with patch("oauth_http.time", monotonic=lambda: TRANSACTION_SECONDS - 30):
            completed = await post()
            self.assertEqual(completed.status_code, 200)
            self.assertIn("Max-Age=30", completed.headers["set-cookie"])
            self.assertEqual(adapter.sessions["browser"]["expires"], TRANSACTION_SECONDS)
        with patch("oauth_http.time", monotonic=lambda: TRANSACTION_SECONDS - .1):
            self.assertEqual((await post()).status_code, 200, "replay works up to the absolute cap")
        with patch("oauth_http.time", monotonic=lambda: TRANSACTION_SECONDS + COMPLETED_SECONDS):
            expired = await post()
            self.assertEqual(expired.status_code, 403)
            self.assertIn(b"Sign-in expired", expired.body)
            self.assertNotIn("browser", adapter.sessions, "the completion must not extend the session past the cap")

    async def test_login_renews_session_cookie_and_consent_deadline(self):
        from unittest.mock import patch

        class Core:
            async def call(self, _, command, args):
                result = ({"transaction": "fixture", "csrf": "core-csrf", "expires_in": 300}
                          if command == "oauth.authorize-begin" else
                          {"expires_in": 600, "known_ships": [], "requested_scopes": [], "display_name": "Example",
                           "client_id": "fake-id", "redirect_uri": "https://example.com/callback", "resource": "https://example.com/mcp"})
                return {"ok": True, "result": result}

        adapter = OAuthHTTP(Core(), {})

        async def request(path, method="GET", sid="", data=None):
            headers = {b"content-type": "application/x-www-form-urlencoded", b"cookie": "privateer_oauth_browser=" + sid}
            scope = {"type": "http", "method": method, "path": path, "query_string": b"",
                     "headers": [(k, v.encode()) for k, v in headers.items()]}
            return await adapter.handle(scope, None, urlencode(data or {}).encode(), headers, "127.0.0.1")

        with patch("oauth_http.time", monotonic=lambda: 100):
            await request("/authorize")
            sid, session = next(iter(adapter.sessions.items()))
            token = session["csrf"]
            self.assertEqual(session["expires"], 400)
        with patch("oauth_http.time", monotonic=lambda: 399):
            response = await request("/login", "POST", sid, {"csrf": token, "login_code": "fixture"})
            self.assertEqual(response.status_code, 303)
            self.assertIn("Max-Age=600", response.headers["set-cookie"])
            self.assertNotEqual(session["csrf"], token)
            self.assertEqual(session["expires"], 999)
        with patch("oauth_http.time", monotonic=lambda: 998.9):
            consent = await request("/consent", sid=sid)
            self.assertEqual(consent.status_code, 200)
            self.assertIn("script-src 'sha256-", consent.headers["content-security-policy"])
            self.assertIn(b"b.disabled=true", consent.body)
        with patch("oauth_http.time", monotonic=lambda: 999):
            self.assertIn(b"Sign-in expired", (await request("/consent", sid=sid)).body)
        # Browsers may have already dropped the expired cookie.
        self.assertIn(b"Retry from the client", (await request("/consent")).body)

    async def test_consent_accepts_all_1024_listed_ships_and_bounds_extra_fields(self):
        class Core:
            def __init__(self):
                self.calls = []

            async def call(self, _, command, args):
                self.calls.append((command, args))
                return {"ok": True, "result": {"redirect_uri": "https://example.com/callback", "code": "fixture"}}

        core = Core()
        adapter = OAuthHTTP(core, {})
        ships = [f"ship-{n:04}" for n in range(1024)]
        scopes = [f"scope-{n}" for n in range(8)]
        adapter.sessions["browser"] = {"transaction": "fixture", "csrf": "fixture-csrf",
                                       "expires": time.monotonic() + 60, "absolute_expires": time.monotonic() + 600,
                                       "summary": {"known_ships": ships, "consent_token": "fixture-consent"}}
        fields = [("csrf", "fixture-csrf"), ("decision", "approve"), ("ships", "")]
        fields += [("scopes", scope) for scope in scopes]
        fields += [("selected_ships", ship) for ship in ships]
        # The actual browser form fits with every listed ship checked.
        self.assertLessEqual(len(fields), CONSENT_FIELD_LIMIT)
        headers = {b"content-type": "application/x-www-form-urlencoded",
                   b"cookie": "privateer_oauth_browser=browser"}
        scope = {"type": "http", "method": "POST", "path": "/consent", "query_string": b"",
                 "headers": [(k, v.encode()) for k, v in headers.items()]}

        async def receive():
            raise AssertionError("body already read")

        excessive = urlencode(fields + [("selected_ships", "ship-0000")] * (CONSENT_FIELD_LIMIT + 1 - len(fields)))
        response = await adapter.handle(scope, receive, excessive.encode(), headers, "127.0.0.1")
        self.assertEqual(response.status_code, 400)
        error = json.loads(response.body)
        self.assertEqual(error["error"], "invalid_request")
        self.assertIn(f"Too many form fields (maximum {CONSENT_FIELD_LIMIT})", error["error_description"])
        self.assertEqual(core.calls, [], "over-limit consent must not reach the core")
        self.assertIn("browser", adapter.sessions, "owner can retry a smaller selection")
        response = await adapter.handle(scope, receive, urlencode(fields).encode(), headers, "127.0.0.1")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(core.calls[0][0], "oauth.consent-submit")
        self.assertEqual(core.calls[0][1]["ships"], ships)
        self.assertEqual(core.calls[0][1]["scopes"], scopes)
        # Other OAuth forms retain their smaller bound and duplicate rejection.
        self.assertEqual(len(form("&".join(f"p{n}=x" for n in range(128)))), 128)
        with self.assertRaises(FormFieldLimit):
            form("&".join(f"p{n}=x" for n in range(129)))
        with self.assertRaisesRegex(ValueError, "duplicate parameter"):
            form("csrf=one&csrf=two", max_fields=CONSENT_FIELD_LIMIT)

    async def test_request_log_is_opt_in_and_allowlisted(self):
        from contextlib import redirect_stderr
        from io import StringIO
        from door import Ingress

        secret = "fixture-credential-must-never-be-logged"
        class Core:
            authenticated = False
            async def call(self, *args, **kwargs):
                return {"ok": self.authenticated, "result": {"scopes": [], "provenance": {"client": "fixture"}}}
        core = Core()
        async def app(scope, receive, send):
            self.assertTrue(core.authenticated, "invalid fixture credentials must not reach SDK")
            await send({"type": "http.response.start", "status": 200, "headers": []})
            await send({"type": "http.response.body", "body": b""})
        messages = []
        async def send(message):
            messages.append(message)
        async def receive():
            return {"type": "http.request", "body": json.dumps({"method": "tools/call", "params": {
                "secret": secret, "token": secret, "code": secret}}).encode(), "more_body": False}
        scope = {"type": "http", "method": "POST", "path": "/mcp", "query_string": b"",
                 "client": ("127.0.0.1", 1), "headers": [(b"host", b"127.0.0.1:8767"),
                 (b"authorization", ("Bearer " + secret).encode()), (b"cookie", secret.encode()),
                 (b"mcp-protocol-version", b"2026-07-28")]}
        output = StringIO()
        with redirect_stderr(output):
            await Ingress(app, core, {})(scope, receive, send)
        self.assertEqual(output.getvalue(), "", "logging defaults off")
        ingress = Ingress(app, core, {"log_requests": True})
        with redirect_stderr(output):
            await ingress(scope, receive, send)
            # Credential-shaped input in all ostensibly metadata fields, even
            # a request rejected before body receive, must never be reflected.
            await ingress({**scope, "method": secret, "path": "/" + secret,
                           "query_string": ("code=" + secret).encode(),
                           "headers": scope["headers"][:-1] + [(b"mcp-protocol-version", secret.encode())]}, receive, send)
            core.authenticated = True
            async def hostile_body():
                return {"type": "http.request", "body": json.dumps({"method": secret}).encode()}
            await ingress(scope, hostile_body, send)
            async def malformed_body():
                return {"type": "http.request", "body": b'{"method":' + secret.encode()}
            await ingress(scope, malformed_body, send)
            await ingress(scope, receive, send)
        records = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(len(records), 5)
        self.assertFalse(secret in output.getvalue())
        self.assertEqual(records[0], {"event": "mcp_http_request", "method": "POST", "path": "/mcp",
                         "mcp_method": None, "protocol_version": "2026-07-28", "authorization": "yes",
                         "status": 401, "duration_ms": records[0]["duration_ms"]})
        self.assertEqual(records[1]["path"], "[other]")
        self.assertEqual(records[1]["status"], 404)
        self.assertEqual(records[2]["mcp_method"], "[other]")
        self.assertIsNone(records[3]["mcp_method"])  # malformed JSON is refused before logging the body
        self.assertEqual(records[3]["status"], 400)
        self.assertEqual(records[4]["mcp_method"], "tools/call")
        self.assertEqual(records[4]["status"], 200)
        self.assertTrue(all(record["duration_ms"] >= 0 for record in records))

    async def test_remote_startup_binding_and_path_discovery(self):
        class Core:
            async def call(self, token, command, args):
                return {"ok": True, "result": {"issuer": "https://example.com/door", "protected_resource": {"resource": self.resource}}}
        core = Core()
        cfg = {"public_url": "https://example.com/door", "allowed_hosts": ["example.com"], "allowed_origins": ["https://example.com"]}
        adapter = OAuthHTTP(core, cfg)
        core.resource = "https://example.com/door/mcp"
        await adapter.startup()
        self.assertIn("/.well-known/oauth-authorization-server/door", adapter.routes)
        self.assertIn("/.well-known/oauth-protected-resource/door/mcp", adapter.routes)
        self.assertNotIn("/.well-known/openid-configuration", adapter.routes)
        self.assertNotIn("/.well-known/oauth-protected-resource", adapter.routes)
        self.assertNotIn("/door/.well-known/oauth-protected-resource/mcp", adapter.routes)
        self.assertNotIn("/.well-known/oauth-authorization-server", adapter.routes)
        self.assertIn('resource_metadata="https://example.com/.well-known/oauth-protected-resource/door/mcp"', adapter.challenge()["WWW-Authenticate"])
        response = adapter.page("test", "cookie")
        self.assertIn("Secure", response.headers["set-cookie"])
        self.assertIn("HttpOnly", response.headers["set-cookie"])
        self.assertIn("SameSite=lax", response.headers["set-cookie"])
        core.resource = "https://wrong.example/mcp"
        with self.assertRaises(ValueError):
            await adapter.startup()


    async def test_remote_ingress_resource_relay_and_unbuffered_stream(self):
        from door import Ingress

        class Core:
            calls = []
            async def call(self, token, command, args, resource=None):
                self.calls.append((token, resource))
                return {"ok": True, "result": {"scopes": [], "provenance": {"client": "safe-id"}}}

        released = asyncio.Event()
        paths, messages = [], []
        async def streaming(scope, receive, send):
            paths.append(scope["path"])
            await send({"type": "http.response.start", "status": 200, "headers": [(b"content-type", b"text/event-stream")]})
            await send({"type": "http.response.body", "body": b"data: first\n\n", "more_body": True})
            await asyncio.wait_for(released.wait(), 1)
            await send({"type": "http.response.body", "body": b"data: second\n\n", "more_body": False})
        async def send(message):
            messages.append(message)
            if message.get("more_body"):
                released.set()
        async def receive():
            return {"type": "http.request", "body": b"", "more_body": False}
        cfg = {"public_url": "https://example.com/door", "allowed_hosts": ["example.com"], "allowed_origins": ["https://example.com"],
               "trusted_proxies": ["192.0.2.1"]}
        core = Core()
        ingress = Ingress(streaming, core, cfg)
        scope = {"type": "http", "method": "POST", "path": "/door/mcp", "query_string": b"", "client": ("192.0.2.1", 10),
                 "headers": [(b"host", b"example.com"), (b"x-forwarded-proto", b"https"), (b"x-forwarded-for", b"198.51.100.2"),
                             (b"authorization", b"Bearer pv_access_opaque")]}
        await ingress(scope, receive, send)
        self.assertEqual(paths, ["/mcp"])
        self.assertEqual(core.calls[-1], ("pv_access_opaque", "https://example.com/door/mcp"))
        self.assertEqual([m.get("more_body") for m in messages[1:]], [True, False])
        scope["headers"][-1] = (b"authorization", b"Bearer local-owner-token")
        await ingress(scope, receive, send)
        self.assertEqual(core.calls[-1], ("local-owner-token", None))
        # A trusted peer must assert HTTPS, even when its Host is correct.
        scope["headers"][1] = (b"x-forwarded-proto", b"http")
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 403)
        # Direct non-loopback access in remote mode is refused, even with forged
        # forwarding headers. Loopback remains usable for local diagnostics.
        scope["headers"][1] = (b"x-forwarded-proto", b"https")
        scope["client"] = ("192.0.2.99", 10)
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 403)
        scope["client"] = ("127.0.0.1", 10)
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 200)

    async def test_rate_limit_before_slot_or_body_and_forwarded_buckets(self):
        from door import Ingress
        class Core:
            async def call(self, *args):
                return {"ok": True, "result": {"error": "invalid_request"}}
        cfg = {"public_url": "https://example.com", "allowed_hosts": ["example.com"],
               "allowed_origins": ["https://example.com"], "trusted_proxies": ["10.42.0.0/16"]}
        ingress = Ingress(None, Core(), cfg)
        scope = {"type": "http", "method": "POST", "path": "/token", "query_string": b"",
                 "client": ("10.42.1.7", 10), "headers": [(b"host", b"example.com"),
                 (b"x-forwarded-proto", b"https"), (b"x-forwarded-for", b"198.51.100.1")]}
        reads, messages = [], []
        async def receive():
            reads.append(True)
            return {"type": "http.request", "body": b"", "more_body": False}
        async def send(message):
            messages.append(message)
        for _ in range(SourceLimits.VALUES["token"]):
            await ingress(scope, receive, send)
            self.assertEqual(messages[0]["status"], 400)
            messages.clear()
        ingress.active = ingress.limit
        count = len(reads)
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 429, "limiter precedes the full in-flight gate")
        self.assertEqual(len(reads), count, "rate-limited requests must not read the body")
        ingress.active = 0
        scope["headers"][-1] = (b"x-forwarded-for", b"198.51.100.2")
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 400, "another forwarded client has its own budget")
        self.assertEqual(len(reads), count + 1)
        # Another pod in the trusted range, same forwarded source: same limit.
        scope["client"] = ("10.42.2.8", 10)
        scope["headers"][-1] = (b"x-forwarded-for", b"198.51.100.1")
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 429)
        self.assertEqual(len(reads), count + 1)
        scope["client"] = ("10.43.1.7", 10)
        messages.clear()
        await ingress(scope, receive, send)
        self.assertEqual(messages[0]["status"], 403)
        self.assertEqual(len(reads), count + 1)


class StartupDoorTests(unittest.IsolatedAsyncioTestCase):
    async def test_missing_remote_oauth_is_actionable_in_start_and_log(self):
        fleet = await asyncio.to_thread(Fleet)
        self.addAsyncCleanup(asyncio.to_thread, fleet.close)
        await fleet.ready()
        local = await asyncio.to_thread(subprocess.run, [str(BIN), "door", "start", "mcp-test"],
                                        env=fleet.env, cwd=fleet.root, capture_output=True, timeout=20)
        self.assertEqual(local.returncode, 0)
        warning = "OAuth is not configured for this door; this is fine for local-only use, but remote mode (public_url) will refuse to start"
        self.assertIn(warning.encode(), local.stderr)
        log = fleet.root / "state/doors/mcp-test/door.log"
        self.assertIn(warning, log.read_text())
        fleet.proc.send_signal(signal.SIGTERM)
        await asyncio.to_thread(fleet.proc.wait, 15)
        fleet.config.write_text(f'venv = {json.dumps(sys.prefix)}\nport = {fleet.port}\npublic_url = "https://example.com"\n'
                                'allowed_hosts = ["example.com"]\nallowed_origins = ["https://example.com"]\n')
        output = await asyncio.to_thread(subprocess.run, [str(BIN), "door", "start", "mcp-test"],
                                         env=fleet.env, cwd=fleet.root, capture_output=True, timeout=20)
        self.assertEqual(output.returncode, 0)
        self.assertIn(warning.encode(), output.stderr)
        self.assertIn(b"door oauth-client configure", output.stderr)
        self.assertIn(b"Run each confirmation command on its own", output.stderr)
        fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env, cwd=fleet.root,
                                      stdout=subprocess.DEVNULL, stderr=fleet.stderr, start_new_session=True)
        log = fleet.root / "state/doors/mcp-test/door.log"
        for _ in range(100):
            if "MCP stopped: remote OAuth configuration unavailable" in log.read_text():
                break
            await asyncio.sleep(.05)
        else:
            self.fail("missing remote OAuth startup error was swallowed")
        fleet.assert_listener_closed()

    async def test_startup_diagnostics_never_reflect_arbitrary_exception_data(self):
        from door import CoreUnavailable, startup_error
        for error in (ValueError("pv_secret_do-not-log"), RuntimeError("Bearer do-not-log"),
                      OSError("/secret/path")):
            self.assertNotIn(str(error), startup_error(error))
            self.assertLess(len(startup_error(error)), 256)
        self.assertEqual(startup_error(CoreUnavailable()), "core unavailable during startup")


class OAuthDoorTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.secrets = []
        self.fleet = await asyncio.to_thread(Fleet, log_requests=True)
        self.addAsyncCleanup(asyncio.to_thread, self.fleet.close)
        await self.fleet.ready()
        self.base = self.fleet.url.removesuffix("/mcp")
        await asyncio.to_thread(owner, self.fleet, ["configure", "mcp-test", "--issuer", self.base,
                                                  "--resource", self.fleet.url], "configure")
        app, terminal = await asyncio.to_thread(owner, self.fleet, ["add", "mcp-test", "--name", "<untrusted application>",
                                                                  "--redirect-uri", REDIRECT, "--type", "confidential"], "add")
        self.client_id = app["client_id"]
        self.secret = re.search(r"Client secret \(shown once\): (\S+)", terminal).group(1)
        self.secrets.append(self.secret)
        self.http = httpx2.AsyncClient(trust_env=False, timeout=10, follow_redirects=False)
        self.addAsyncCleanup(self.http.aclose)

    async def asyncTearDown(self):
        fleet = self.fleet
        audit = await asyncio.to_thread(fleet.cli, "door", "client", "audit")
        logs = audit + (fleet.root / "supervisor.stderr").read_text()
        for path in (fleet.root / "state/doors/mcp-test").glob("*.log*"):
            logs += path.read_text()
        self.assertIn('"event":"mcp_http_request"', logs, "OAuth fixture must exercise request logging")
        for credential in self.secrets + fleet.tokens:
            self.assertFalse(credential in logs, "credential leaked to diagnostic/audit")

    def authorization(self, **changes):
        return {"client_id": self.client_id, "response_type": "code", "redirect_uri": REDIRECT,
                "resource": self.fleet.url, "state": "client-state", "code_challenge_method": "S256",
                "code_challenge": CHALLENGE, "scope": "fleet:read", **changes}

    async def browser(self, url, negatives=False, deny=False, approved_scopes="fleet:read", ship_form=None,
                      check_ship_overflow=False):
        page = await self.http.get(url)
        self.assertEqual(page.status_code, 200)
        self.assertIn("Local-only", page.text)
        self.assertNotIn('name="selected_ships"', page.text)
        self.assertNotIn('value="alpha"', page.text)
        self.assertEqual((await self.http.get(self.base + "/consent")).status_code, 400)
        self.assertEqual(page.headers["referrer-policy"], "same-origin")
        origin = {"Origin": self.base}
        self.assertIn("frame-ancestors 'none'", page.headers["content-security-policy"])
        transaction = re.search(r"privateer door oauth-client login-code ([a-f0-9-]+)", page.text).group(1)
        _, terminal = await asyncio.to_thread(owner, self.fleet, ["login-code", transaction], "login-code")
        code = re.search(r"Login code \(shown once\): (\S+)", terminal).group(1)
        token = csrf(page.text)
        self.secrets.extend((code, token))
        if negatives:
            for data in ({"login_code": code}, {"csrf": "forged", "login_code": code}, {"csrf": "forged-雪", "login_code": code}):
                self.assertEqual((await self.http.post(self.base + "/login", data=data)).status_code, 403)
            async with httpx2.AsyncClient(trust_env=False) as stranger:
                self.assertEqual((await stranger.post(self.base + "/login", data={"csrf": token, "login_code": code})).status_code, 403)
        self.assertEqual((await self.http.post(self.base + "/login", headers={"Origin": "null"},
                                               data={"csrf": token, "login_code": code})).status_code, 403)
        logged = await self.http.post(self.base + "/login", headers=origin, data={"csrf": token, "login_code": code})
        self.assertEqual(logged.status_code, 303)
        consent = await self.http.get(self.base + logged.headers["location"])
        self.assertEqual(consent.status_code, 200)
        self.secrets.append(csrf(consent.text))
        self.assertIn("&lt;untrusted application&gt;", consent.text)
        self.assertIn('name="selected_ships" value="alpha"', consent.text)
        self.assertIn('name="selected_ships" value="beta"', consent.text)
        self.assertIn("All ships (current and future)", consent.text)
        async with httpx2.AsyncClient(trust_env=False) as stranger:
            self.assertEqual((await stranger.get(self.base + "/consent")).status_code, 403)
        self.assertIsNone(re.search(r"<input\b[^>]*\schecked(?:\s|=|>)", consent.text))
        if negatives:
            self.assertEqual((await self.http.post(self.base + "/consent", data={"csrf": token, "decision": "approve"})).status_code, 403)
        self.assertEqual(consent.headers["referrer-policy"], "same-origin")
        self.assertEqual((await self.http.post(self.base + "/consent", headers={"Origin": "null"},
                                               data={"csrf": csrf(consent.text), "decision": "approve"})).status_code, 403)
        data = {"csrf": csrf(consent.text), "decision": "deny" if deny else "approve",
                "scopes": approved_scopes.split(), **({"ships": "alpha"} if ship_form is None else ship_form)}
        if check_ship_overflow:
            rejected = await self.http.post(self.base + "/consent", headers=origin,
                                            data={**data, "ships": "one-too-many"})
            self.assertEqual(rejected.status_code, 400)
            self.assertEqual(rejected.json()["error"], "invalid_request")
            self.assertIn("1024 named ships", rejected.json()["error_description"])
            self.assertIn("All ships", rejected.json()["error_description"])
        if negatives:
            response, duplicate = await asyncio.gather(
                self.http.post(self.base + "/consent", headers=origin, data=data),
                self.http.post(self.base + "/consent", headers=origin, data=data))
            self.assertEqual(duplicate.status_code, 200)
            self.assertEqual(response.text, duplicate.text)
            async with httpx2.AsyncClient(trust_env=False) as stranger:
                replay = await stranger.post(self.base + "/consent", headers=origin, data=data)
                self.assertEqual(replay.status_code, 403)
        else:
            response = await self.http.post(self.base + "/consent", headers=origin, data=data)
        # The callback is reached by a fresh navigation, not a redirect of the
        # form POST, so form-action never governs callbacks that redirect again.
        self.assertEqual(response.status_code, 200)
        self.assertNotIn("location", response.headers)
        self.assertIn("form-action 'self';", response.headers["content-security-policy"])
        self.assertEqual(response.headers["cache-control"], "no-store")
        refresh = html.unescape(re.search(r'<meta http-equiv="refresh" content="0;url=([^"]+)">', response.text).group(1))
        self.assertIn(f'href="{html.escape(refresh)}"', response.text)
        target = urlsplit(refresh)
        self.assertEqual(target._replace(query="").geturl(), REDIRECT)
        result = {k: v[0] for k, v in parse_qs(target.query).items()}
        self.assertEqual(result["iss"], self.base)
        expected_state = parse_qs(urlsplit(url).query)["state"][0]
        self.assertEqual(result["state"], expected_state)
        if "code" in result:
            self.secrets.append(result["code"])
        return result

    async def test_handwritten_flow_and_attacks(self):
        unauth = await self.http.post(self.fleet.url)
        self.assertEqual(unauth.status_code, 401)
        self.assertIn(self.base + "/.well-known/oauth-protected-resource/mcp", unauth.headers["www-authenticate"])
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource")).status_code, 404)
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp")).json()["resource"], self.fleet.url)
        metadata = (await self.http.get(self.base + "/.well-known/oauth-authorization-server")).json()
        self.assertEqual(metadata["issuer"], self.base)
        for field in ("token_endpoint_auth_methods_supported", "revocation_endpoint_auth_methods_supported"):
            self.assertEqual(metadata[field], ["client_secret_post", "client_secret_basic", "none"])
        self.assertEqual(metadata["code_challenge_methods_supported"], ["S256"])
        self.assertTrue(metadata["authorization_response_iss_parameter_supported"])
        self.assertNotIn("registration_endpoint", metadata)
        self.assertEqual((await self.http.get(self.base + "/.well-known/openid-configuration")).status_code, 404)
        for changes in ({"redirect_uri": "https://evil.example/cb"}, {"resource": "https://wrong.example/mcp"}):
            bad = await self.http.get(self.base + "/authorize", params=self.authorization(**changes))
            self.assertEqual(bad.status_code, 400)
            self.assertNotIn("location", bad.headers)
        for headers in ({"Host": "evil.example"}, {"Origin": "https://evil.example"}):
            self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource", headers=headers)).status_code, 403)
        method_error = await self.http.post(self.base + "/authorize")
        self.assertEqual(method_error.status_code, 405)
        self.assertEqual(method_error.headers["allow"], "GET")
        for query in (urlencode(self.authorization()) + "&prompt=login&prompt=consent",
                      urlencode(self.authorization()) + "&client_id=duplicate"):
            bad = await self.http.get(self.base + "/authorize?" + query)
            self.assertEqual(bad.status_code, 400)
            self.assertNotIn("location", bad.headers)
        missing = self.authorization()
        del missing["resource"]
        self.assertEqual((await self.http.get(self.base + "/authorize", params=missing)).status_code, 400)
        result = await self.browser(self.base + "/authorize?" + urlencode(self.authorization(scope=" ".join(metadata["scopes_supported"]), prompt="consent")), negatives=True)
        self.assertEqual(result["state"], "client-state")
        data = {"client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
                "resource": self.fleet.url, "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT}
        wrong = await self.http.post(self.base + "/token", data={**data, "resource": "https://wrong.example/mcp"})
        self.assertEqual(wrong.json()["error"], "invalid_target")
        missing = {k: v for k, v in data.items() if k != "resource"}
        duplicate = await self.http.post(self.base + "/token", content=urlencode(data) + "&extra=x&extra=y",
                                         headers={"Content-Type": "application/x-www-form-urlencoded"})
        self.assertEqual(duplicate.status_code, 400)
        audit = json.loads(await asyncio.to_thread(self.fleet.cli, "door", "client", "audit"))
        failures = [row for row in audit["events"] if row["event"] == "oauth.exchange_failure"]
        self.assertTrue(any(row["client"] == self.client_id and json.loads(row["reason"])["error"] == "invalid_target"
                            and json.loads(row["reason"])["parameter"] == "resource" for row in failures))
        self.assertTrue(any(json.loads(row["reason"])["parameter"] == "form" for row in failures))
        response = await self.http.post(self.base + "/token", data={**missing, "extension": "ignored"})
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.headers["cache-control"], "no-store")
        tokens = response.json()
        self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
        self.assertEqual(tokens["scope"], "fleet:read")
        async with session(self.fleet, tokens["access_token"], True) as mcp:
            value = (await mcp.call_tool("fleet_ships", {})).structured_content
            self.assertIn("alpha", json.dumps(value))
            self.assertNotIn("beta", json.dumps(value))
        basic = base64.b64encode(f"{self.client_id}:{self.secret}".encode()).decode()
        refresh = {"grant_type": "refresh_token", "refresh_token": tokens["refresh_token"], "scope": "fleet:read"}
        wrong = await self.http.post(self.base + "/token", headers={"Authorization": "Basic " + basic},
                                     data={**refresh, "resource": "https://wrong.example/mcp"})
        self.assertEqual(wrong.json()["error"], "invalid_target")
        refreshed = await self.http.post(self.base + "/token", headers={"Authorization": "Basic " + basic}, data=refresh)
        self.assertEqual(refreshed.status_code, 200)
        current = refreshed.json()
        self.assertEqual(current["scope"], "fleet:read", "narrower refresh scope is ignored, not silently applied")
        self.secrets.extend((current["access_token"], current["refresh_token"]))
        duplicate = await self.http.post(self.base + "/revoke", content="token=a&token=b",
                                         headers={"Content-Type": "application/x-www-form-urlencoded"})
        self.assertEqual(duplicate.status_code, 400)
        revoked = await self.http.post(self.base + "/revoke", data={"client_id": self.client_id, "client_secret": self.secret,
                                                                   "token": current["refresh_token"], "extension": "ignored"})
        self.assertEqual(revoked.status_code, 200)
        denied = await self.http.post(self.fleet.url, headers={"Authorization": "Bearer " + current["access_token"]})
        self.assertEqual(denied.status_code, 401)
        self.assertEqual((await self.http.post(self.base + "/token", data=data)).json()["error"], "invalid_grant")
        denied = await self.browser(self.base + "/authorize?" + urlencode(self.authorization()), deny=True)
        self.assertEqual(denied["error"], "access_denied")
        self.assertNotIn("code", denied)
        oversized = await self.http.post(self.base + "/token", content=b"x" * 65537)
        self.assertEqual(oversized.status_code, 413)
        self.assertEqual(oversized.headers["cache-control"], "no-store")
        # Spoofing a fresh source on every request cannot bypass loopback's limiter.
        for n in range(125):
            response = await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp", headers={"X-Forwarded-For": f"192.0.2.{n}"})
        self.assertEqual(response.status_code, 429)

    async def test_ship_picker_all_ships_and_empty_defaults(self):
        for fields, expected in (({"all_ships": "yes"}, None),
                                 ({"selected_ships": ["beta"]}, ["beta"]),
                                 ({"selected_ships": ["alpha"], "ships": "beta alpha"}, ["alpha", "beta"]),
                                 ({}, [])):
            result = await self.browser(self.base + "/authorize?" + urlencode(self.authorization()), ship_form=fields)
            response = await self.http.post(self.base + "/token", data={
                "client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
                "resource": self.fleet.url, "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT})
            self.assertEqual(response.status_code, 200)
            tokens = response.json()
            self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
            grants = json.loads(await asyncio.to_thread(self.fleet.cli, "door", "client", "list"))["clients"]
            matching = [g for g in grants if g.get("application_id") == self.client_id]
            self.assertEqual(matching[-1]["ships"], expected)

    async def test_all_1024_picker_ships_through_http_and_real_core(self):
        ships = ["alpha", "beta"] + [f"inventory-ship-{n:04}" for n in range(1022)]
        # Add inventory only; these homes are never opened by the OAuth flow.
        with closing(sqlite3.connect(self.fleet.env["PRIVATEER_FLEET"])) as conn, conn:
            conn.executemany("INSERT INTO ships(name,home,created_by,created_ms) VALUES(?,?,'fixture',0)",
                             [(ship, str(self.fleet.root / ship)) for ship in ships[2:]])
        result = await self.browser(self.base + "/authorize?" + urlencode(self.authorization()),
                                    ship_form={"selected_ships": ships}, check_ship_overflow=True)
        response = await self.http.post(self.base + "/token", data={
            "client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
            "resource": self.fleet.url, "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT})
        self.assertEqual(response.status_code, 200)
        tokens = response.json()
        self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
        grants = json.loads(await asyncio.to_thread(self.fleet.cli, "door", "client", "list"))["clients"]
        matching = [g for g in grants if g.get("application_id") == self.client_id]
        self.assertEqual(matching[-1]["ships"], sorted(ships))

    async def test_owner_confirmation_flush_and_safe_mismatch(self):
        # Queued input before exec must not answer the fresh confirmation.
        await asyncio.to_thread(owner, self.fleet, ["configure", "mcp-test", "--issuer", self.base,
                              "--resource", self.fleet.url], "configure", typeahead="add\n")
        args = ["configure", "mcp-test", "--issuer", self.base, "--resource", self.fleet.url]
        result, _ = await asyncio.to_thread(owner, self.fleet, args, "configure", response="add", fails=True)
        self.assertIn('expected "configure", received "add"', result["error"])
        secret = "pv_secret_not-for-diagnostics"
        result, _ = await asyncio.to_thread(owner, self.fleet, args, "configure", response=secret, fails=True)
        self.assertNotIn(secret, result["error"])
        self.assertIn("withheld to protect secrets", result["error"])

    async def test_redirect_edit_preserves_id_and_grants_invalidates_pending(self):
        url = self.base + "/authorize?" + urlencode(self.authorization())
        result = await self.browser(url)
        args = {"client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
                "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT}
        tokens = (await self.http.post(self.base + "/token", data=args)).json()
        self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
        unexchanged = await self.browser(url)
        pending = await self.http.get(url)
        transaction = re.search(r"login-code ([a-f0-9-]+)", pending.text).group(1)
        redirect = "https://example.com/user-bound-callback"
        edit = ["edit", self.client_id, "--redirect-uri", redirect]
        for marker in ("PRIVATEER_ROLE", "PRIVATEER_JOB"):
            denied = await asyncio.to_thread(subprocess.run, [str(BIN), "door", "oauth-client", *edit],
                                             env={**self.fleet.env, marker: "agent", "PRIVATEER_ACTOR": "crew"}, cwd=self.fleet.root,
                                             capture_output=True, timeout=10)
            self.assertNotEqual(denied.returncode, 0)
            expected = b"operator command; agents cannot run it" if marker == "PRIVATEER_ROLE" else b"captain-only"
            self.assertIn(expected, denied.stderr)
        no_tty = await asyncio.to_thread(subprocess.run, [str(BIN), "door", "oauth-client", *edit],
                                         env=self.fleet.env, cwd=self.fleet.root, capture_output=True, timeout=10,
                                         start_new_session=True)
        self.assertNotEqual(no_tty.returncode, 0)
        await asyncio.to_thread(owner, self.fleet, ["edit", self.client_id, "--redirect-uri", "https://example.com/#fragment"], "edit", fails=True)
        await asyncio.to_thread(owner, self.fleet, edit, "edit", response="remove", fails=True)
        registered = json.loads(await asyncio.to_thread(self.fleet.cli, "door", "oauth-client", "inspect", self.client_id))
        self.assertEqual(registered["applications"][0]["redirect_uris"], [REDIRECT])
        edited, terminal = await asyncio.to_thread(owner, self.fleet, edit, "edit")
        self.assertEqual(edited["client_id"], self.client_id)
        self.assertEqual(edited["redirect_uris"], [redirect])
        self.assertIn("preserved", terminal)
        await asyncio.to_thread(owner, self.fleet, ["login-code", transaction], "login-code", fails=True)
        refused = await self.http.post(self.base + "/token", data={**args, "code": unexchanged["code"]})
        self.assertEqual(refused.json()["error"], "invalid_grant")
        self.assertEqual((await self.http.get(url)).status_code, 400)
        self.assertEqual((await self.http.get(self.base + "/authorize", params=self.authorization(redirect_uri=redirect))).status_code, 200)
        rotated = await self.http.post(self.base + "/token", data={"client_id": self.client_id, "client_secret": self.secret,
                                      "grant_type": "refresh_token", "refresh_token": tokens["refresh_token"]})
        self.assertEqual(rotated.status_code, 200)
        self.secrets.extend((rotated.json()["access_token"], rotated.json()["refresh_token"]))
        # Retain old consumed-code evidence: replay still revokes its family.
        self.assertEqual((await self.http.post(self.base + "/token", data=args)).json()["error"], "invalid_grant")
        refused = await self.http.post(self.fleet.url, headers={"Authorization": "Bearer " + rotated.json()["access_token"]})
        self.assertEqual(refused.status_code, 401)

    async def test_refresh_narrow_scope_retains_approved_scope(self):
        scopes = "fleet:read mail:read"
        result = await self.browser(self.base + "/authorize?" + urlencode(self.authorization(scope=scopes)),
                                    approved_scopes=scopes)
        response = await self.http.post(self.base + "/token", data={
            "client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
            "resource": self.fleet.url, "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT})
        self.assertEqual(response.status_code, 200)
        tokens = response.json()
        self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
        self.assertEqual(set(tokens["scope"].split()), set(scopes.split()))
        response = await self.http.post(self.base + "/token", data={
            "client_id": self.client_id, "client_secret": self.secret, "grant_type": "refresh_token",
            "refresh_token": tokens["refresh_token"], "scope": "fleet:read"})
        self.assertEqual(response.status_code, 200)
        refreshed = response.json()
        self.secrets.extend((refreshed["access_token"], refreshed["refresh_token"]))
        self.assertEqual(set(refreshed["scope"].split()), set(scopes.split()))
        async with session(self.fleet, refreshed["access_token"], True) as client:
            self.assertIn("mail_inbox", {t.name for t in (await client.list_tools()).tools})

    async def test_official_sdk_oauth_client(self):
        fixture = self

        class Storage:
            tokens = None
            info = OAuthClientInformationFull(client_id=fixture.client_id, client_secret=fixture.secret,
                                               redirect_uris=[REDIRECT], token_endpoint_auth_method="client_secret_post", issuer=fixture.base)
            async def get_tokens(self): return self.tokens
            async def set_tokens(self, value): self.tokens = value
            async def get_client_info(self): return self.info
            async def set_client_info(self, value): self.info = value

        callback = {}
        async def redirect(url):
            callback.update(await self.browser(url))
        async def receive_callback():
            return AuthorizationCodeResult(code=callback["code"], state=callback["state"], iss=callback["iss"])
        storage = Storage()
        provider = OAuthClientProvider(self.fleet.url, OAuthClientMetadata(redirect_uris=[REDIRECT], scope="fleet:read",
                                                                          token_endpoint_auth_method="client_secret_post"),
                                       storage, redirect_handler=redirect, callback_handler=receive_callback)
        from mcp.client.streamable_http import streamable_http_client
        from mcp import ClientSession
        async with httpx2.AsyncClient(auth=provider, trust_env=False, timeout=15) as http:
            async with streamable_http_client(self.fleet.url, http_client=http) as streams:
                async with ClientSession(*streams, read_timeout_seconds=15) as client:
                    await client.discover()
                    self.assertEqual({t.name for t in (await client.list_tools()).tools},
                                     {"fleet_ships", "fleet_status", "fleet_waiting", "away_status",
                                      "ship_runtime", "ship_jobs", "job_inspect", "ship_work", "machine_capacity", "runtime_needs",
                                      "resource_recovery_reference"})
                    self.assertFalse({"work_rank", "work_enqueue", "work_defer", "job_cancel", "job_retry"} &
                                     {t.name for t in (await client.list_tools()).tools},
                                     "OAuth fleet:read never implies scheduling writes")
                    self.assertFalse((await client.call_tool("fleet_ships", {})).is_error)
        self.assertIsNotNone(storage.tokens)
        # Force expiry to exercise the SDK's refresh implementation on a new request.
        previous_refresh = storage.tokens.refresh_token
        self.secrets.extend((storage.tokens.access_token, previous_refresh))
        provider.context.token_expiry_time = time.time() - 10
        async with httpx2.AsyncClient(auth=provider, trust_env=False, timeout=15) as http:
            self.assertNotEqual((await http.post(self.fleet.url)).status_code, 401)
        self.assertNotEqual(storage.tokens.refresh_token, previous_refresh, "SDK must really rotate the refresh token")
        self.secrets.extend((storage.tokens.access_token, storage.tokens.refresh_token))
        response = await self.http.post(self.base + "/revoke", data={"client_id": self.client_id, "client_secret": self.secret,
                                                                     "token": storage.tokens.refresh_token})
        self.assertEqual(response.status_code, 200)

    async def test_real_remote_startup_mismatch_and_proxy_metadata(self):
        fleet = self.fleet
        async def stop():
            fleet.proc.send_signal(signal.SIGTERM)
            await asyncio.to_thread(fleet.proc.wait, 15)
        def start():
            fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env, cwd=fleet.root,
                                          stdout=subprocess.DEVNULL, stderr=fleet.stderr, start_new_session=True)
        await stop()
        fleet.config.write_text(f'venv = {json.dumps(sys.prefix)}\nport = {fleet.port}\npublic_url = "https://example.com"\n'
                                'trusted_proxies = ["127.0.0.1"]\nallowed_hosts = ["example.com"]\nallowed_origins = ["https://example.com"]\n')
        start()
        log = fleet.root / "state/doors/mcp-test/door.log"
        for _ in range(100):
            if "MCP stopped: core issuer/resource mismatch" in log.read_text():
                break
            await asyncio.sleep(.05)
        else:
            self.fail("remote startup did not refuse mismatching core issuer/resource")
        with self.assertRaises(httpx2.TransportError):
            await self.http.get(self.base + "/.well-known/oauth-authorization-server")
        await stop()
        await asyncio.to_thread(owner, fleet, ["remove", self.client_id], "remove")
        await asyncio.to_thread(owner, fleet, ["configure", "mcp-test", "--issuer", "https://example.com",
                                               "--resource", "https://example.com/mcp"], "configure")
        start()
        headers = {"Host": "example.com", "X-Forwarded-For": "192.0.2.5", "X-Forwarded-Proto": "https"}
        for _ in range(100):
            try:
                response = await self.http.get(self.base + "/.well-known/oauth-authorization-server", headers=headers)
                if response.status_code == 200:
                    break
            except httpx2.TransportError:
                pass
            await asyncio.sleep(.05)
        else:
            self.fail("matching remote configuration did not start")
        self.assertEqual(response.json()["issuer"], "https://example.com")
        self.assertEqual(response.json()["token_endpoint"], "https://example.com/token")
        self.assertNotIn(self.base, response.text)
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp", headers=headers)).json()["resource"],
                         "https://example.com/mcp")
        denied = await self.http.post(fleet.url, headers=headers)
        self.assertEqual(denied.status_code, 401)
        self.assertIn('resource_metadata="https://example.com/.well-known/oauth-protected-resource/mcp"', denied.headers["www-authenticate"])
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp", headers={"Host": "example.com"})).status_code, 403)
        for _ in range(125):
            response = await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp", headers=headers)
        self.assertEqual(response.status_code, 429)
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource/mcp",
                                             headers={**headers, "X-Forwarded-For": "192.0.2.6"})).status_code, 200)
        self.assertIn("MCP remote HTTPS mode", log.read_text())


if __name__ == "__main__":
    unittest.main()
