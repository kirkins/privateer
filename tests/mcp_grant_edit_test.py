#!/usr/bin/env python3
"""Real owner PTY -> live supervised MCP HTTP door, same open client session."""
import asyncio
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import sys
import unittest
from urllib.parse import urlencode

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_oauth_test as oauth
from mcp_oauth_test import owner, REDIRECT, VERIFIER
from mcp_door_test import session


class GrantEditHTTPTests(unittest.IsolatedAsyncioTestCase):
    # Reuse the isolated owner/browser fixture, not its unrelated test suite.
    asyncSetUp = oauth.OAuthDoorTests.asyncSetUp
    asyncTearDown = oauth.OAuthDoorTests.asyncTearDown
    authorization = oauth.OAuthDoorTests.authorization
    browser = oauth.OAuthDoorTests.browser

    async def edit(self, grant, *flags, **options):
        result, tty = await asyncio.to_thread(
            owner, self.fleet, ["edit", grant, *flags], "edit", group="client", **options)
        if not options.get("fails"):
            self.assertIn('"before"', tty)
            self.assertIn('"after"', tty)
        return result

    async def tool(self, client, name, args=None):
        result = await client.call_tool(name, args or {})
        self.assertFalse(result.is_error, name)
        return result.structured_content

    async def names(self, client):
        result = await self.tool(client, "fleet_ships")
        return {ship["name"] for ship in result["ships"]}

    async def test_live_local_and_oauth_grants_without_reconnect(self):
        fleet = self.fleet
        local_grant, local_token = await asyncio.to_thread(fleet.issue, "local-edit", "fleet:read", "alpha")
        # OAuth initially requests/approves only fleet:read. The terminal owner
        # can later widen it beyond the connector's immutable requested scopes.
        consent = await self.browser(self.base + "/authorize?" + urlencode(self.authorization()))
        response = await self.http.post(self.base + "/token", data={
            "client_id": self.client_id, "client_secret": self.secret, "grant_type": "authorization_code",
            "resource": fleet.url, "code": consent["code"], "code_verifier": VERIFIER, "redirect_uri": REDIRECT})
        self.assertEqual(response.status_code, 200)
        tokens = response.json()
        self.secrets.extend((tokens["access_token"], tokens["refresh_token"]))
        grants = json.loads(await asyncio.to_thread(fleet.cli, "door", "client", "list", "mcp-test"))["clients"]
        oauth_grant = next(g["id"] for g in grants if g.get("application_id") == self.client_id)
        for label, grant, token in (("local", local_grant, local_token), ("oauth", oauth_grant, tokens["access_token"])):
            with self.subTest(credential=label):
                async with session(fleet, token, False) as client:
                    self.assertEqual(await self.names(client), {"alpha"})
                    self.assertNotIn("send_message", {t.name for t in (await client.list_tools()).tools})
                    self.assertNotIn("job_retry", {t.name for t in (await client.list_tools()).tools})
                    await self.edit(grant, "--add-scopes", "work:manage")
                    self.assertIn("job_retry", {t.name for t in (await client.list_tools()).tools})
                    await self.edit(grant, "--remove-scopes", "work:manage")
                    self.assertNotIn("job_retry", {t.name for t in (await client.list_tools()).tools})
                    self.assertTrue((await client.call_tool("job_retry", {"ship": "alpha"})).is_error)
                    new_ship = "new-" + label
                    folder = fleet.root / new_ship
                    folder.mkdir()
                    await asyncio.to_thread(fleet.cli, "init", "--ship", new_ship, "--harness", "codex", cwd=folder)
                    self.assertEqual(await self.names(client), {"alpha"})
                    send = {"to": new_ship, "kind": "notice", "body": "edited " + label, "idempotency_key": "before-authority-change"}
                    self.assertTrue((await client.call_tool("send_message", send)).is_error)
                    await self.edit(grant, "--add-scopes", "ship:message", "--add-ships", new_ship,
                                    typeahead="revoke\n")
                    self.assertEqual(await self.names(client), {"alpha", new_ship})
                    self.assertIn("send_message", {t.name for t in (await client.list_tools()).tools})
                    sent = await self.tool(client, "send_message", send)
                    self.assertEqual(await self.tool(client, "send_message", send), sent)
                    # Target remains allowed. Only the authority hash changes:
                    # the stored keyed result must be refused, not reexecuted.
                    await self.edit(grant, "--remove-ships", "alpha")
                    retry = await client.call_tool("send_message", send)
                    self.assertTrue(retry.is_error)
                    self.assertEqual(retry.structured_content["error"]["code"], "not_found")
                    self.assertEqual(await self.names(client), {new_ship})
                    await self.tool(client, "send_message", {**send, "idempotency_key": "after-edit"})
                    await self.edit(grant, "--ships", "alpha")
                    self.assertEqual(await self.names(client), {"alpha"})
                    self.assertTrue((await client.call_tool("send_message", {**send, "idempotency_key": "removed-target"})).is_error)
                    await self.edit(grant, "--all-ships")
                    self.assertIn(new_ship, await self.names(client))
                    future = "future-" + label
                    folder = fleet.root / future
                    folder.mkdir()
                    await asyncio.to_thread(fleet.cli, "init", "--ship", future, "--harness", "codex", cwd=folder)
                    self.assertIn(future, await self.names(client), "all-ships includes ships created after the edit")
                    await self.edit(grant, "--named-ships", "--ships", "alpha", "--remove-scopes", "ship:message")
                    self.assertEqual(await self.names(client), {"alpha"})
                    self.assertNotIn("send_message", {t.name for t in (await client.list_tools()).tools})
                    self.assertTrue((await client.call_tool("send_message", {"to": "alpha", "kind": "notice", "body": "scope removed"})).is_error)
                    failure = await self.edit(grant, "--all-ships", response="issue", fails=True)
                    self.assertIn('expected "edit"', failure["error"])
                    self.assertEqual(await self.names(client), {"alpha"})
                    await self.edit(grant, "--scopes", "fleet:read,mail:read,ship:message")
                    self.assertIn("mail_inbox", {t.name for t in (await client.list_tools()).tools})
                    await self.tool(client, "send_message", {"to": "alpha", "kind": "notice", "body": "scope restored " + label})
                with closing(sqlite3.connect(fleet.root / new_ship / ".privateer/privateer.sqlite")) as db:
                    self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE body=?", ("edited " + label,)).fetchone()[0], 2,
                                     "stale-key retry must not create a third effect")
        response = await self.http.post(self.base + "/token", data={
            "client_id": self.client_id, "client_secret": self.secret, "grant_type": "refresh_token",
            "refresh_token": tokens["refresh_token"]})
        self.assertEqual(response.status_code, 200)
        refreshed = response.json()
        self.secrets.extend((refreshed["access_token"], refreshed["refresh_token"]))
        self.assertEqual(set(refreshed["scope"].split()), {"fleet:read", "mail:read", "ship:message"})
        # Even the original access token still works; refresh wasn't necessary.
        async with session(fleet, tokens["access_token"], True) as client:
            self.assertIn("send_message", {t.name for t in (await client.list_tools()).tools})
        audit = json.loads(await asyncio.to_thread(fleet.cli, "door", "client", "audit"))["events"]
        for grant in (local_grant, oauth_grant):
            edits = [e for e in audit if e["event"] == "edit" and e["grant"] == grant]
            self.assertEqual(len(edits), 6)
            self.assertTrue(all(e["retention"] == "lifecycle" and json.loads(e["reason"])["owner_consent"] for e in edits))
            self.assertEqual(json.loads(edits[0]["reason"])["before"], {"scopes": ["fleet:read"], "ships": ["alpha"]})


if __name__ == "__main__":
    unittest.main()
