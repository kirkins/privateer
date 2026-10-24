#!/usr/bin/env python3
"""Isolated real HTTP OAuth flows: handwritten client and pinned MCP OAuth client."""
import asyncio
import base64
import errno
import html
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import sys
import time
import unittest
from urllib.parse import parse_qs, urlencode, urlsplit

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "plugins/mcp"))
from mcp_door_test import BIN, Fleet, session
from oauth_http import OAuthHTTP, SourceLimits, configuration, source_address
import httpx2
from mcp.client.auth import OAuthClientProvider
from mcp.shared.auth import AuthorizationCodeResult, OAuthClientInformationFull, OAuthClientMetadata

REDIRECT = "https://client.example/callback"
VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"


def owner(fleet, args, action):
    master, slave = os.openpty()
    bootstrap = 'import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv(sys.argv[1],sys.argv[1:])'
    child = subprocess.Popen([sys.executable, "-c", bootstrap, str(BIN), "door", "oauth-client", *args],
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
                    os.write(master, (action + "\n").encode())
                    confirmed = True
        child.wait(timeout=5)
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
                    {**cfg, "allowed_origins": []}, {"public_url": ""}, {"public_url": 42}):
            with self.assertRaises((ValueError, TypeError)):
                configuration(bad)
        headers = {b"x-forwarded-for": "198.51.100.3", b"x-forwarded-proto": "https"}
        self.assertEqual(source_address({"client": ("127.0.0.1", 1)}, headers, set()), "127.0.0.1")
        self.assertEqual(source_address({"client": ("192.0.2.1", 1)}, headers, {"192.0.2.1"}), "198.51.100.3")
        with self.assertRaises(ValueError):
            source_address({"client": ("192.0.2.1", 1)}, {**headers, b"x-forwarded-for": "a,b"}, {"192.0.2.1"})
        with self.assertRaises(ValueError):
            source_address({"client": ("192.0.2.1", 1)}, {**headers, b"x-forwarded-proto": "http"}, {"192.0.2.1"})

    def test_all_rate_limits_and_bound(self):
        for operation, limit in SourceLimits.VALUES.items():
            limiter = SourceLimits()
            for _ in range(limit):
                self.assertTrue(limiter.admit("192.0.2.1", operation))
            self.assertFalse(limiter.admit("192.0.2.1", operation))
            self.assertTrue(limiter.admit("192.0.2.2", operation))
        limiter = SourceLimits()
        limiter.rows = {(str(n), "metadata"): (time.monotonic(), 1) for n in range(4096)}
        self.assertFalse(limiter.admit("new", "metadata"))
        self.assertEqual(len(limiter.rows), 4096)


class MetadataTests(unittest.IsolatedAsyncioTestCase):
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


class OAuthDoorTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.secrets = []
        self.fleet = await asyncio.to_thread(Fleet)
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
        for credential in self.secrets + fleet.tokens:
            self.assertFalse(credential in logs, "credential leaked to diagnostic/audit")

    def authorization(self, **changes):
        return {"client_id": self.client_id, "response_type": "code", "redirect_uri": REDIRECT,
                "resource": self.fleet.url, "state": "client-state", "code_challenge_method": "S256",
                "code_challenge": CHALLENGE, "scope": "fleet:read", **changes}

    async def browser(self, url, negatives=False, deny=False):
        page = await self.http.get(url)
        self.assertEqual(page.status_code, 200)
        self.assertIn("Local-only", page.text)
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
        logged = await self.http.post(self.base + "/login", data={"csrf": token, "login_code": code})
        self.assertEqual(logged.status_code, 303)
        consent = await self.http.get(self.base + logged.headers["location"])
        self.assertEqual(consent.status_code, 200)
        self.secrets.append(csrf(consent.text))
        self.assertIn("&lt;untrusted application&gt;", consent.text)
        self.assertIsNone(re.search(r"<input\b[^>]*\schecked(?:\s|=|>)", consent.text))
        if negatives:
            self.assertEqual((await self.http.post(self.base + "/consent", data={"csrf": token, "decision": "approve"})).status_code, 403)
        response = await self.http.post(self.base + "/consent", data={"csrf": csrf(consent.text), "decision": "deny" if deny else "approve",
                                                                       "scopes": "fleet:read", "ships": "alpha"})
        self.assertEqual(response.status_code, 303)
        result = {k: v[0] for k, v in parse_qs(urlsplit(response.headers["location"]).query).items()}
        self.assertEqual(result["iss"], self.base)
        if "code" in result:
            self.secrets.append(result["code"])
        return result

    async def test_handwritten_flow_and_attacks(self):
        unauth = await self.http.post(self.fleet.url)
        self.assertEqual(unauth.status_code, 401)
        self.assertIn(self.base + "/.well-known/oauth-protected-resource/mcp", unauth.headers["www-authenticate"])
        for path in ("/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"):
            self.assertEqual((await self.http.get(self.base + path)).json()["resource"], self.fleet.url)
        metadata = (await self.http.get(self.base + "/.well-known/oauth-authorization-server")).json()
        self.assertEqual(metadata["issuer"], self.base)
        self.assertNotIn("registration_endpoint", metadata)
        self.assertEqual((await self.http.get(self.base + "/.well-known/openid-configuration")).status_code, 404)
        for changes in ({"redirect_uri": "https://evil.example/cb"}, {"resource": "https://wrong.example/mcp"}):
            bad = await self.http.get(self.base + "/authorize", params=self.authorization(**changes))
            self.assertEqual(bad.status_code, 400)
            self.assertNotIn("location", bad.headers)
        for headers in ({"Host": "evil.example"}, {"Origin": "https://evil.example"}):
            self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource", headers=headers)).status_code, 403)
        result = await self.browser(self.base + "/authorize?" + urlencode(self.authorization(scope=" ".join(metadata["scopes_supported"]))), negatives=True)
        self.assertEqual(result["state"], "client-state")
        data = {"client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
                "resource": self.fleet.url, "code": result["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT}
        wrong = await self.http.post(self.base + "/token", data={**data, "resource": "https://wrong.example/mcp"})
        self.assertEqual(wrong.json()["error"], "invalid_target")
        response = await self.http.post(self.base + "/token", data=data)
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
        refreshed = await self.http.post(self.base + "/token", headers={"Authorization": "Basic " + basic},
                                         data={"grant_type": "refresh_token", "resource": self.fleet.url, "refresh_token": tokens["refresh_token"]})
        self.assertEqual(refreshed.status_code, 200)
        current = refreshed.json()
        self.secrets.extend((current["access_token"], current["refresh_token"]))
        revoked = await self.http.post(self.base + "/revoke", data={"client_id": self.client_id, "client_secret": self.secret, "token": current["refresh_token"]})
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
            response = await self.http.get(self.base + "/.well-known/oauth-protected-resource", headers={"X-Forwarded-For": f"192.0.2.{n}"})
        self.assertEqual(response.status_code, 429)

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
                                     {"fleet_ships", "fleet_status", "fleet_waiting", "away_status"})
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
            if "MCP stopped; check configuration" in log.read_text():
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
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource", headers=headers)).json()["resource"],
                         "https://example.com/mcp")
        denied = await self.http.post(fleet.url, headers=headers)
        self.assertEqual(denied.status_code, 401)
        self.assertIn('resource_metadata="https://example.com/.well-known/oauth-protected-resource/mcp"', denied.headers["www-authenticate"])
        self.assertEqual((await self.http.get(self.base + "/.well-known/oauth-protected-resource", headers={"Host": "example.com"})).status_code, 403)
        self.assertIn("MCP remote HTTPS mode", log.read_text())


if __name__ == "__main__":
    unittest.main()
