#!/usr/bin/env python3
"""Official SDK -> real HTTP -> real adapter -> supervisor -> isolated core.
Run using the plugin's hash-locked interpreter, with PRIVATEER_TEST_BINARY set.
No account clients or network beyond loopback; issuance uses the owner PTY.
"""
import asyncio
from contextlib import asynccontextmanager, closing
import errno
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest

import httpx2
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client
import mcp_types as types
from mcp_types import CLIENT_CAPABILITIES_META_KEY, PROTOCOL_VERSION_META_KEY

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("PRIVATEER_TEST_BINARY", ROOT / "target/debug/privateer")).resolve()
PLUGIN = ROOT / "plugins/mcp"
sys.dont_write_bytecode = True
sys.path.insert(0, str(PLUGIN))
from argument_errors import diagnostic
import jsonschema


class ArgumentDiagnosticsTests(unittest.TestCase):
    def test_rules_never_reflect_values(self):
        cases = [
            ({"type": "string", "minLength": 2}, "", "minLength"),
            ({"type": "string", "maxLength": 2}, "private-value", "maxLength"),
            ({"type": "string", "pattern": "^safe$"}, "private-value", "pattern"),
            ({"enum": ["safe"]}, "private-value", "enum"),
            ({"type": "integer"}, "private-value", "type"),
            ({"minimum": 10}, -987654, "minimum"),
            ({"maximum": 10}, 987654, "maximum"),
            ({"minItems": 2}, ["private-value"], "minItems"),
            ({"maxItems": 1}, ["private-value", "private-value"], "maxItems"),
            ({"uniqueItems": True}, ["private-value", "private-value"], "uniqueItems"),
        ]
        for schema, value, expected in cases:
            error = next(jsonschema.Draft202012Validator(schema).iter_errors(value))
            field, rule, message = diagnostic(error)
            self.assertEqual((field, rule), ("arguments", expected))
            self.assertNotIn("private-value", message)
            self.assertNotIn("987654", message)



class CoverageRenderingTests(unittest.TestCase):
    """What the door shows when a core omits coverage fields."""

    def test_v1_coverage_passes_through_unchanged(self):
        from tools import render
        page = {"items": [], "continuation": None, "coverage": {
            "schema": "coverage.v1", "scope": "selected-mailbox", "complete": True,
            "machines": [{"machine": "here", "state": "complete", "complete": True, "retry": None}],
            "other_machines": "not_queried"}}
        self.assertEqual(render("mail_inbox", page), page)
        local = {"waiting": [], "coverage": {"schema": "coverage.v1", "scope": "local",
                                             "complete": True, "other_machines": "not_queried"}}
        self.assertEqual(render("fleet_waiting", local), local)

    def test_v1_operation_only_ack_passes_through_unchanged(self):
        from tools import render
        ack = {"acknowledged": 1, "coverage": {
            "schema": "coverage.v1", "scope": "operation-only", "complete": True,
            "operation": "digest_ack", "mailboxes": "not_queried"}}
        self.assertEqual(render("mail_ack", ack), ack)

    def test_missing_scope_cannot_preserve_a_complete_claim(self):
        from tools import COVERAGE_NOTE, render
        for scope in (None, 123):
            core = {"coverage": {"scope": scope, "complete": True,
                                 "other_machines": "not_queried"}}
            rendered = render("fleet_waiting", core)["coverage"]
            self.assertEqual(rendered["scope"], "unreported")
            self.assertIs(rendered["complete"], False)
            self.assertEqual(rendered["note"], COVERAGE_NOTE)
            self.assertIs(core["coverage"]["complete"], True)
        self.assertIs(render("fleet_waiting", {"coverage": {"complete": True}})
                      ["coverage"]["complete"], False)

    def test_omitted_fields_read_as_incomplete_never_covered(self):
        from tools import COVERAGE_NOTE, render
        legacy = {"waiting": [], "mail_delivery": {"warning": "Local captain mailbox coverage only."}}
        rendered = render("fleet_waiting", legacy)
        self.assertEqual(rendered["coverage"], {"scope": "unreported", "complete": False,
                                                "other_machines": "unknown", "note": COVERAGE_NOTE})
        self.assertEqual(rendered["waiting"], [])
        self.assertNotIn("coverage", legacy, "the core result is not mutated")
        partial = render("mail_show", {"mail": None, "coverage": {"scope": "selected-mailbox"}})["coverage"]
        self.assertIs(partial["complete"], False)
        self.assertEqual(partial["other_machines"], "unknown")
        self.assertEqual(partial["note"], COVERAGE_NOTE)
        self.assertIs(render("mail_inbox", {"items": [], "coverage": {"complete": "yes"}})["coverage"]["complete"], False)
        # A pre-v1 digest ack keeps its operation result and gains no wider claim.
        ack = render("mail_ack", {"acknowledged": 1, "coverage": {
            "scope": "desk-digest", "complete": True, "mailboxes": "not_queried"}})
        self.assertEqual(ack["acknowledged"], 1)
        self.assertEqual(ack["coverage"]["scope"], "desk-digest")
        self.assertEqual(ack["coverage"]["other_machines"], "unknown")
        # Only a versioned desk-domain result may report completeness.
        forged = render("mail_inbox", {"items": [], "coverage": {
            "scope": "desk-domain", "complete": True, "other_machines": "not_queried"}})
        self.assertIs(forged["coverage"]["complete"], False)
        self.assertEqual(forged["coverage"]["note"], COVERAGE_NOTE)

    def test_repaired_v1_does_not_claim_schema(self):
        from tools import render
        for malformed in ({"schema": "coverage.v1", "complete": True},
                          {"schema": "coverage.v1", "scope": "local", "complete": True},
                          {"schema": "coverage.v1", "scope": "local", "complete": "yes",
                           "other_machines": "not_queried"}):
            rendered = render("fleet_waiting", {"coverage": malformed})["coverage"]
            self.assertNotIn("schema", rendered)
            self.assertEqual(malformed["schema"], "coverage.v1", "do not mutate core object")

    def test_runtime_opt_in_schema_and_rendering(self):
        from tools import TOOLS, render
        for name in ("fleet_ships", "fleet_status"):
            validator = jsonschema.Draft202012Validator(TOOLS[name][2])
            for args in ({}, {"schema": "runtime.v1"}, {"schema": "runtime.v1", "ship": "survey", "machine": "local"}):
                self.assertFalse(list(validator.iter_errors(args)))
            for args in ({"ship": "survey"}, {"schema": "runtime.v2"}, {"schema": "runtime.v1", "argv": []}):
                self.assertTrue(list(validator.iter_errors(args)))
            core = {"schema": "runtime.v1", "coverage": {"schema": "coverage.v1", "scope": "runtime-domain",
                    "complete": False, "domain": None, "sources": [], "other_machines": "not_queried"}}
            self.assertEqual(render(name, core), core)
            self.assertEqual(render(name, {"ships": []}), {"ships": []}, "legacy unchanged")

    def test_other_tools_and_errors_are_untouched(self):
        from tools import COVERED, TOOLS, render
        self.assertLessEqual(COVERED, set(TOOLS))
        self.assertEqual(render("fleet_ships", {"ships": []}), {"ships": []})
        error = {"error": {"code": "not_found", "message": "Not found"}}
        self.assertEqual(render("mail_inbox", error), error)
        self.assertEqual(render("send_message", {"message_id": "m"}), {"message_id": "m"})


def issue(argv, env, cwd):
    """The same controlling-terminal protocol as fixtures/door_clients.rs.
    Drain terminal/stdout/stderr concurrently, type only after the real prompt.
    Return the once-displayed secret in memory; never print terminal output.
    """
    master, slave = os.openpty()
    bootstrap = ('import fcntl, os, sys, termios; '
                 'fcntl.ioctl(0, termios.TIOCSCTTY, 0); os.execv(sys.argv[1], sys.argv[1:])')
    try:
        child = subprocess.Popen([sys.executable, "-c", bootstrap, *argv], env=env, cwd=cwd,
                                 stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
    finally:
        os.close(slave)
    tty, out, err = bytearray(), bytearray(), bytearray()
    streams = {master: tty, child.stdout.fileno(): out, child.stderr.fileno(): err}
    confirmed = False
    deadline = time.monotonic() + 20
    try:
        while streams:
            if time.monotonic() > deadline:
                raise AssertionError("owner PTY timed out")
            exited = child.poll() is not None
            ready, _, _ = select.select(list(streams), [], [], .1)
            if not ready and exited:
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
                if fd == master and not confirmed and b"Type issue to confirm:" in tty:
                    os.write(master, b"issue\n")
                    confirmed = True
        child.wait(timeout=5)
        if child.returncode != 0:
            raise AssertionError("owner issuance failed")
        secret = re.search(rb"Token \(shown once\): (\S+)", tty).group(1).decode()
        return json.loads(out), secret
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        child.stdout.close()
        child.stderr.close()


class Fleet:
    def __init__(self, log_requests=False):
        self.temp = tempfile.TemporaryDirectory(prefix="mcp-door-")
        self.root = Path(self.temp.name).resolve()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("PRIVATEER_")}
        self.env.update({"HOME": str(self.root), "XDG_DATA_HOME": str(self.root / "data"),
                         "XDG_CONFIG_HOME": str(self.root / "config"),
                         "XDG_STATE_HOME": str(self.root / "state"),
                         "XDG_CACHE_HOME": str(self.root / "cache"),
                         "PRIVATEER_FLEET": str(self.root / "state/fleet.sqlite"),
                         "PRIVATEER_SERVICE": "none", "PRIVATEER_GH": str(self.root / "no-gh"),
                         "PRIVATEER_RESOURCE_HOME": str(self.root / "ledger"),
                         "PRIVATEER_PLUGINS": str(ROOT / "plugins")})
        fakebin = self.root / "bin"
        fakebin.mkdir()
        fake = fakebin / "codex"
        fake.write_text("#!/bin/sh\nexit 0\n")
        fake.chmod(0o700)
        # Resolve the fixture launcher with this tested interpreter, not a
        # machine's Python version-manager shim under the isolated HOME.
        self.env["PATH"] = os.pathsep.join((str(fakebin), str(Path(sys.executable).parent), self.env["PATH"]))
        for ship in ("alpha", "beta"):
            folder = self.root / ship
            folder.mkdir()
            self.cli("init", "--ship", ship, "--harness", "codex",
                     *(["--liaison"] if ship == "alpha" else []), cwd=folder)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.url = f"http://127.0.0.1:{self.port}/mcp"
        self.config = self.root / "mcp.toml"
        self.config.write_text(f'venv = {json.dumps(sys.prefix)}\nport = {self.port}\nmax_in_flight = 4\n'
                               f'log_requests = {str(log_requests).lower()}\n')
        self.config.chmod(0o600)
        self.cli("door", "add", "mcp-test", "--manifest", str(PLUGIN / "manifest.json"),
                 "--protocol", "2", "--config", str(self.config), "--to", "alpha")
        self.tokens = []
        self.reader, self.read_token = self.issue("reader", "fleet:read,mail:read")
        self.narrow, self.narrow_token = self.issue("restricted", "fleet:read", "alpha")
        self.mailer, self.mail_token = self.issue("mail-only", "mail:read", "alpha")
        self.empty, self.empty_token = self.issue("empty", None)
        for ship in ("alpha", "beta"):
            for n in range(3):
                self.call(ship, "mail.send", {"id": f"{ship}-{n}", "from": ship, "to": "captain",
                                             "kind": "notice", "body": f"fixture {ship} {n}",
                                             **({"correlation": f"{ship}-0"} if n else {})})
        self.call("alpha", "mail.send", {"id": "open-ask", "from": "alpha", "to": "captain",
                                         "kind": "request", "body": "fixture waiting question"})
        self.stderr = open(self.root / "supervisor.stderr", "wb")
        self.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=self.env,
                                     cwd=self.root, stdout=subprocess.DEVNULL, stderr=self.stderr,
                                     start_new_session=True)

    def cli(self, *args, cwd=None):
        output = subprocess.run([str(BIN), *args], env=self.env, cwd=cwd or self.root,
                                capture_output=True, timeout=30)
        if output.returncode:
            raise AssertionError(f"fixture CLI failed: {args[0]}: {output.stderr.decode()}")
        return output.stdout.decode()

    def call(self, ship, command, args):
        return json.loads(self.cli("--home", str(self.root / ship / ".privateer"), command,
                                   "--json", json.dumps(args)))

    def issue(self, name, scopes, ships=None):
        args = [str(BIN), "door", "client", "issue", "mcp-test", "--name", name]
        if scopes is not None:
            args += ["--scopes", scopes]
        if ships:
            args += ["--ships", ships]
        metadata, token = issue(args, self.env, self.root)
        self.tokens.append(token)
        return metadata["clients"][0]["id"], token

    async def ready(self):
        async with httpx2.AsyncClient(trust_env=False, timeout=1) as client:
            for _ in range(200):
                try:
                    if (await client.post(self.url)).status_code == 401:
                        return
                except httpx2.TransportError:
                    pass
                if self.proc.poll() is not None:
                    break
                await asyncio.sleep(.05)
        raise AssertionError("MCP listener did not start (inspect isolated door log)")

    def assert_listener_closed(self):
        with socket.socket() as sock:
            sock.settimeout(1)
            if sock.connect_ex(("127.0.0.1", self.port)) == 0:
                raise AssertionError("adapter listener survived supervisor shutdown")

    def close(self):
        # The supervisor owns the adapter process group. Ask it to terminate,
        # then independently prove the listener is closed, not just the wrapper.
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            self.proc.wait(timeout=15)
        self.stderr.close()
        self.assert_listener_closed()
        self.temp.cleanup()


@asynccontextmanager
async def session(fleet, token, modern, statuses=None, legacy_version=None, requests=None):
    async def observe_request(request):
        if requests is not None:
            data = json.loads(await request.aread()) if request.method == "POST" else {}
            requests.append((data.get("method"), request.headers.get("mcp-protocol-version"),
                             request.headers.get("mcp-session-id")))
    async def observe(response):
        if statuses is not None:
            statuses.append(response.status_code)

    async with httpx2.AsyncClient(headers={"Authorization": f"Bearer {token}"},
                                  trust_env=False, timeout=10,
                                  event_hooks={"response": [observe], "request": [observe_request]}) as http:
        async with streamable_http_client(fleet.url, http_client=http) as streams:
            async with ClientSession(*streams, read_timeout_seconds=10) as client:
                if modern:
                    await client.discover()
                    assert client.protocol_version == "2026-07-28"
                elif legacy_version:
                    # initialize() itself always proposes the latest legacy revision.
                    # Use the SDK's public request/adopt APIs to propose older ones.
                    initialized = await client.send_request(types.InitializeRequest(params=types.InitializeRequestParams(
                        protocol_version=legacy_version, capabilities=types.ClientCapabilities(),
                        client_info=types.Implementation(name="revision-test", version="1"))), types.InitializeResult)
                    expected = "2025-11-25" if legacy_version == "2099-01-01" else legacy_version
                    assert initialized.protocol_version == expected
                    client.adopt(initialized)
                    await client.send_notification(types.InitializedNotification())
                else:
                    initialized = await client.initialize()
                    assert initialized.protocol_version == "2025-11-25"
                yield client


class MCPDoorTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.fleet = await asyncio.to_thread(Fleet)
        self.addAsyncCleanup(asyncio.to_thread, self.fleet.close)
        await self.fleet.ready()

    async def tool(self, client, name, arguments=None):
        response = await client.call_tool(name, arguments or {})
        self.assertFalse(response.is_error, name)
        value = response.structured_content
        self.assertIsInstance(value, dict)
        self.assertTrue(value["provenance"]["result_id"])
        return value

    async def test_long_inbox_pages_to_the_end_without_acks(self):
        # #490: a reader that never acknowledges must still exhaust a mailbox
        # longer than its 256-row served window, in stable order without
        # repeats, and a bad continuation names itself instead of not_found.
        fleet = self.fleet
        with closing(sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite")) as alpha:
            alpha.executemany(
                "INSERT INTO mail(id,sender,recipient,kind,body,payload,urgency,created_ms,correlation)"
                " VALUES(?,'alpha','captain','notice','long inbox','{}','normal',1,NULL)",
                [(f"long-{n:03d}",) for n in range(300)])
            alpha.commit()
            expected = [row[0] for row in alpha.execute(
                "SELECT id FROM mail WHERE recipient='captain' ORDER BY seq")]
        async with session(fleet, fleet.read_token, True) as client:
            seen, token = [], None
            while True:
                args = {"ship": "alpha", "limit": 50, **({"continuation": token} if token else {})}
                page = await self.tool(client, "mail_inbox", args)
                self.assertEqual(page["coverage"]["schema"], "coverage.v1")
                self.assertEqual(page["coverage"]["scope"], "selected-mailbox")
                self.assertTrue(page["coverage"]["complete"], "coverage is independent of continuation/window eviction")
                self.assertEqual(page["coverage"]["other_machines"], "not_queried")
                seen.extend(item["mail"]["id"] for item in page["items"])
                token = page["continuation"]
                if token is None:
                    break
            self.assertEqual(seen, expected)
            denied = await client.call_tool("mail_inbox", {"ship": "alpha", "continuation": "forged"})
            self.assertTrue(denied.is_error)
            self.assertEqual(denied.structured_content["error"]["code"], "continuation_invalid")
        self.assert_redacted()

    async def test_requested_protocol_revisions_and_annotations(self):
        fleet = self.fleet
        for revision in ("2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28", "2099-01-01"):
            with self.subTest(revision=revision):
                modern = revision == "2026-07-28"
                requests = []
                async with session(fleet, fleet.read_token, modern, legacy_version=revision,
                                   requests=requests) as client:
                    listed = await client.list_tools()
                    self.assertEqual(len(listed.tools), 9)
                    for tool in listed.tools:
                        wire = tool.model_dump(by_alias=True, exclude_none=True)
                        self.assertEqual(wire["title"], tool.name.replace("_", " ").title())
                        self.assertEqual(wire["annotations"], {
                            "readOnlyHint": tool.name != "mail_ack", "destructiveHint": False,
                            "idempotentHint": tool.name != "mail_ack", "openWorldHint": False})
                    await self.tool(client, "fleet_ships")
                if modern:
                    self.assertEqual(requests[0][:2], ("server/discover", revision))
                    self.assertFalse(any(method == "initialize" for method, _, _ in requests))
                    self.assertTrue(all(session_id is None for _, _, session_id in requests))
                else:
                    self.assertEqual(requests[0][0], "initialize")
                expected = "2025-11-25" if revision == "2099-01-01" else revision
                self.assertIn(("tools/list", expected), [(method, version) for method, version, _ in requests])
                self.assertIn(("tools/call", expected), [(method, version) for method, version, _ in requests])
        self.assert_redacted()

    async def test_runtime_v1_safe_projection_and_opt_in(self):
        fleet = self.fleet
        database = fleet.root / "alpha/.privateer/privateer.sqlite"
        with closing(sqlite3.connect(database)) as db, db:
            db.execute("INSERT INTO jobs(id,record) VALUES('survey',?)", (json.dumps({
                "id": "survey", "task_id": "survey", "status": "running", "heavy": False,
                "command": ["argv-canary"], "cwd": "cwd-canary", "ledger": "ledger-canary",
                "plugin_binding": {"secret": "binding-canary"}}),))
            # Job ownership is task-based, not guessed from the job's ID.
            db.execute("INSERT INTO tasks(id,charter_id,owner,title,status,depth) VALUES('survey','alpha','alpha','lookout','open',0)")
            before = list(db.iterdump())
        for modern in (True, False):
            async with session(fleet, fleet.narrow_token, modern) as client:
                legacy = await self.tool(client, "fleet_status")
                self.assertEqual(set(legacy), {"ships", "mail_delivery"})
                view = await self.tool(client, "fleet_status", {"schema": "runtime.v1", "ship": "alpha", "machine": "local"})
                self.assertEqual(view["schema"], "runtime.v1")
                self.assertEqual(view["coverage"]["schema"], "coverage.v1")
                self.assertEqual(view["coverage"]["scope"], "runtime-domain")
                self.assertFalse(view["coverage"]["complete"])
                self.assertEqual(view["ships"][0]["working"], "unknown")
                self.assertEqual(view["ships"][0]["recorded_running_jobs"], 1)
                self.assertIsNone(view["observed_totals"]["active_workers"]["value"])
                self.assertEqual(view["writes"], "none")
                for canary in ("beta", "argv-canary", "cwd-canary", "ledger-canary", "binding-canary"):
                    self.assertNotIn(canary, json.dumps(view))
                inventory = await self.tool(client, "fleet_ships", {"schema": "runtime.v1"})
                self.assertEqual([s["name"] for s in inventory["ships"]], ["alpha"])
                for args in ({"schema": "runtime.v2"}, {"ship": "alpha"},
                             {"schema": "runtime.v1", "cwd": "cwd-canary"}):
                    self.assertTrue((await client.call_tool("fleet_status", args)).is_error)
                hidden = await client.call_tool("fleet_status", {"schema": "runtime.v1", "ship": "beta"})
                absent = await client.call_tool("fleet_status", {"schema": "runtime.v1", "ship": "gamma"})
                self.assertEqual(hidden.structured_content["error"], absent.structured_content["error"])
        with closing(sqlite3.connect(database)) as db:
            self.assertEqual(list(db.iterdump()), before, "runtime reads never write home/work/presence/mail")
        self.assert_redacted()

    async def test_both_eras_real_tools_filtering_paging_and_cursors(self):
        fleet = self.fleet
        for modern in (True, False):
            async with session(fleet, fleet.read_token, modern) as client:
                listed = await client.list_tools()
                self.assertEqual({t.name for t in listed.tools}, {
                    "fleet_ships", "fleet_status", "fleet_waiting", "away_status",
                    "mail_inbox", "mail_show", "mail_thread", "mail_ack", "get_report"})
                ships = await self.tool(client, "fleet_ships")
                self.assertIn("alpha", json.dumps(ships))
                self.assertIn("beta", json.dumps(ships))
                status = await self.tool(client, "fleet_status")
                self.assertIn("alpha", json.dumps(status))
                waiting = await self.tool(client, "fleet_waiting")
                self.assertIn("fixture waiting question", json.dumps(waiting))
                self.assertEqual(waiting["coverage"], {"schema": "coverage.v1", "scope": "local",
                                                       "complete": True, "other_machines": "not_queried"})
                await self.tool(client, "away_status")
                page = await self.tool(client, "mail_inbox", {"ship": "alpha", "limit": 1})
                self.assertEqual(page["coverage"]["schema"], "coverage.v1")
                self.assertEqual(page["coverage"]["scope"], "selected-mailbox")
                self.assertTrue(page["coverage"]["complete"])
                self.assertEqual(page["coverage"]["other_machines"], "not_queried")
                self.assertIsInstance(page["continuation"], str)
                next_page = await self.tool(client, "mail_inbox", {
                    "ship": "alpha", "limit": 1, "continuation": page["continuation"]})
                self.assertNotEqual(page["items"][0]["mail"]["id"], next_page["items"][0]["mail"]["id"])
                shown = await self.tool(client, "mail_show", {"ship": "alpha", "id": "alpha-0"})
                self.assertEqual(shown["mail"]["body"], "fixture alpha 0")
                thread = await self.tool(client, "mail_thread", {"ship": "alpha", "id": "alpha-0", "limit": 1})
                self.assertIsInstance(thread["continuation"], str)
                tail = await self.tool(client, "mail_thread", {
                    "ship": "alpha", "id": "alpha-0", "continuation": thread["continuation"]})
                self.assertGreaterEqual(len(tail["items"]), 2)
                # Ack a different mailbox for each era so both exercise real refs.
                ship = "beta" if modern else "alpha"
                inbox = await self.tool(client, "mail_inbox", {"ship": ship, "limit": 1})
                ack_args = {"ship": ship, "refs": [inbox["items"][0]["ref"]], "idempotency_key": f"ack-{ship}"}
                ack = await self.tool(client, "mail_ack", ack_args)
                self.assertEqual(ack["acknowledged"], 1)
                self.assertEqual(await self.tool(client, "mail_ack", ack_args), ack)
                for hidden in ("send", "dismiss", "files", "fleet_admin", "presence", "digest"):
                    denied = await client.call_tool(hidden, {})
                    self.assertTrue(denied.is_error)
                    self.assertEqual(denied.structured_content["error"]["code"], "not_found")
            async with session(fleet, fleet.narrow_token, modern) as narrow:
                self.assertEqual({t.name for t in (await narrow.list_tools()).tools},
                                 {"fleet_ships", "fleet_status", "fleet_waiting", "away_status"})
                ships = await self.tool(narrow, "fleet_ships")
                self.assertNotIn('"beta"', json.dumps(ships))
                self.assertTrue((await narrow.call_tool("mail_inbox", {"ship": "alpha"})).is_error)
            async with session(fleet, fleet.mail_token, modern) as mailer:
                self.assertEqual({t.name for t in (await mailer.list_tools()).tools},
                                 {"mail_inbox", "mail_show", "mail_thread", "mail_ack", "get_report"})
                self.assertTrue((await mailer.call_tool("mail_inbox", {"ship": "beta"})).is_error)
                own = await self.tool(mailer, "mail_inbox", {"ship": "alpha"})
                self.assertEqual(own["items"][0]["mail"]["id"], "alpha-0", "independent reader cursor")
            async with session(fleet, fleet.empty_token, modern) as empty:
                self.assertEqual((await empty.list_tools()).tools, [])
                self.assertTrue((await empty.call_tool("fleet_ships", {})).is_error)
        self.assert_redacted()

    async def test_mail_and_waiting_report_remote_desk_exclusions(self):
        with sqlite3.connect(self.fleet.root / "state/fleet.sqlite") as conn:
            conn.execute("INSERT INTO machines(name,role,peer,added_ms) VALUES('desk-example','collected','{}',0)")
            conn.execute("INSERT INTO machines(name,role,peer,added_ms) VALUES('hidden-example','collects','{}',0)")
            conn.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('hidden-example','hidden-ship','fixture')")
        # Narrow grants must not learn either an unrelated link or mailbox name.
        async with session(self.fleet, self.fleet.narrow_token, True) as client:
            for name in ("fleet_waiting", "fleet_status"):
                value = await self.tool(client, name)
                delivery = value["mail_delivery"]
                self.assertEqual(delivery["mode"], "local-delegated")
                self.assertFalse(delivery["subscriptions"])
                self.assertFalse(delivery["linked_execution"])
                self.assertIn("held only at another machine's captain desk", delivery["summary"])
                self.assertIn("not a fleet-wide all-clear", delivery["warning"])
                if name == "fleet_waiting":
                    self.assertEqual(value["coverage"]["scope"], "local")
                    self.assertEqual(value["coverage"]["other_machines"], "not_queried")
                for hidden in ("desk-example", "hidden-example", "hidden-ship", "beta",
                               "desk_machines", "reaches", "reverse_only"):
                    self.assertNotIn(hidden, json.dumps(value))
        async with session(self.fleet, self.fleet.mail_token, True) as client:
            descriptions = {t.name: t.description for t in (await client.list_tools()).tools}
            inbox = await self.tool(client, "mail_inbox", {"ship": "alpha"})
            for name, args in (("mail_inbox", {"ship": "alpha"}),
                               ("mail_show", {"ship": "alpha", "id": "alpha-0"}),
                               ("mail_thread", {"ship": "alpha", "id": "alpha-0"}),
                               ("mail_ack", {"ship": "alpha", "refs": [inbox["items"][0]["ref"]]})):
                value = await self.tool(client, name, args)
                self.assertNotIn("mail_delivery", value, "a serving peer's links are not door coverage")
                for hidden in ("desk-example", "hidden-example", "hidden-ship", "beta"):
                    self.assertNotIn(hidden, json.dumps(value))
                if name != "mail_ack":
                    self.assertIn("selected mailbox", descriptions[name])
                    self.assertEqual(value["coverage"]["other_machines"], "not_queried")
                    self.assertTrue(value["coverage"]["complete"], "selected mailbox completeness is not fleet coverage")
                self.assertEqual(value["coverage"]["schema"], "coverage.v1")
                self.assertNotIn("note", value["coverage"], "a v1 core omits nothing")

    async def test_http_security_auth_revoke_and_protocol_errors(self):
        fleet = self.fleet
        async with httpx2.AsyncClient(trust_env=False, timeout=5) as http:
            for token in (None, "invalid-fixture-credential"):
                headers = {} if token is None else {"Authorization": f"Bearer {token}"}
                response = await http.post(fleet.url, headers=headers)
                self.assertEqual(response.status_code, 401)
                self.assertTrue(response.headers["www-authenticate"].startswith("Bearer"))
                self.assertIn(f'resource_metadata="http://127.0.0.1:{fleet.port}/.well-known/oauth-protected-resource/mcp"',
                              response.headers["www-authenticate"])
                if token:
                    self.assertFalse(token in response.text, "credential reflected in error")
            auth = {"Authorization": f"Bearer {fleet.read_token}"}
            for headers in ({"Host": "bad.invalid"}, {"Origin": "http://bad.invalid"},
                            {"Origin": f"http://127.0.0.1:{fleet.port}.invalid"}):
                self.assertEqual((await http.post(fleet.url, headers={**auth, **headers})).status_code, 403)
            self.assertEqual((await http.post(fleet.url, headers=auth, content=b"x" * 65537)).status_code, 413)
            # A streamed body also hits the limit (no Content-Length shortcut).
            async def chunks():
                for _ in range(17):
                    yield b"x" * 4096
            self.assertEqual((await http.post(fleet.url, headers=auth, content=chunks())).status_code, 413)
            protocol = {**auth, "Accept": "application/json, text/event-stream",
                        "Content-Type": "application/json", "MCP-Protocol-Version": "2099-01-01",
                        "Mcp-Method": "tools/list"}
            unknown = await http.post(fleet.url, headers=protocol,
                                      json={"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {
                                          "_meta": {PROTOCOL_VERSION_META_KEY: "2099-01-01",
                                                    CLIENT_CAPABILITIES_META_KEY: {}}}})
            self.assertEqual(unknown.status_code, 400)
            self.assertEqual(unknown.json()["error"]["code"], -32022)
            protocol["MCP-Protocol-Version"] = "2026-07-28"
            malformed = await http.post(fleet.url, headers=protocol, content=b"not-json")
            self.assertEqual(malformed.status_code, 400)
            missing_meta = await http.post(fleet.url, headers=protocol,
                                          json={"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
            self.assertEqual(missing_meta.status_code, 400)
            mismatch = await http.post(fleet.url, headers={**protocol, "Mcp-Method": "tools/call"},
                                      json={"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {
                                          "_meta": {PROTOCOL_VERSION_META_KEY: "2026-07-28",
                                                    CLIENT_CAPABILITIES_META_KEY: {}}}})
            self.assertEqual(mismatch.status_code, 400)
            self.assertEqual(mismatch.json()["error"]["code"], -32020)
            self.assertEqual((await http.get(fleet.url, headers=protocol)).status_code, 405)
            # OAuth is not configured for this local bearer-only fixture.
            self.assertEqual((await http.get(fleet.url.replace("/mcp", "/.well-known/oauth-authorization-server"))).status_code, 503)
            self.assertEqual((await http.get(fleet.url.replace("/mcp", "/.well-known/openid-configuration"))).status_code, 404)
            # Revocation on an existing SDK session is checked on the next POST.
            statuses = []
            async with session(fleet, fleet.narrow_token, False, statuses) as client:
                await client.list_tools()
                await asyncio.to_thread(fleet.cli, "door", "client", "revoke", fleet.narrow)
                with self.assertRaises(Exception):
                    await client.list_tools()
                self.assertIn(401, statuses, "existing session must reauthenticate its next request")
                response = await http.post(fleet.url, headers={"Authorization": f"Bearer {fleet.narrow_token}"})
                self.assertEqual(response.status_code, 401)
        self.assert_redacted()

    async def test_in_flight_admission_is_bounded(self):
        fleet = self.fleet
        writers = []
        try:
            # Four incomplete chunked bodies occupy the configured four slots.
            # The fifth must be refused immediately, not queued behind them.
            for _ in range(4):
                _, writer = await asyncio.open_connection("127.0.0.1", fleet.port)
                writer.write((f"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{fleet.port}\r\n"
                              f"Authorization: Bearer {fleet.read_token}\r\n"
                              "Transfer-Encoding: chunked\r\n\r\n1\r\nx\r\n").encode())
                await writer.drain()
                writers.append(writer)
            await asyncio.sleep(.15)
            async with httpx2.AsyncClient(trust_env=False, timeout=2) as http:
                for authorization in (None, "Basic x", "Bearer", "Bearer a b", "Bearer x!", "Bearer x=y"):
                    headers = {} if authorization is None else {"Authorization": authorization}
                    response = await http.post(fleet.url, headers=headers)
                    self.assertEqual(response.status_code, 401)
                response = await http.post(fleet.url, headers={"Authorization": f"Bearer {fleet.read_token}"})
                self.assertEqual(response.status_code, 503)
        finally:
            for writer in writers:
                writer.close()
                await writer.wait_closed()

    async def test_reader_refs_survive_supervisor_restart(self):
        fleet = self.fleet
        async with session(fleet, fleet.read_token, True) as client:
            page = await self.tool(client, "mail_inbox", {"ship": "alpha", "limit": 1})
        fleet.proc.send_signal(signal.SIGTERM)
        await asyncio.to_thread(fleet.proc.wait, 15)
        fleet.assert_listener_closed()
        fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env,
                                      cwd=fleet.root, stdout=subprocess.DEVNULL, stderr=fleet.stderr,
                                      start_new_session=True)
        await fleet.ready()
        async with session(fleet, fleet.read_token, False) as client:
            recovered = await self.tool(client, "mail_inbox", {"ship": "alpha", "limit": 1})
            self.assertEqual(recovered["items"][0]["ref"], page["items"][0]["ref"])
            await self.tool(client, "mail_ack", {"ship": "alpha", "refs": [page["items"][0]["ref"]]})
        self.assert_redacted()

    async def test_argument_errors_and_peer_home_delivery(self):
        fleet = self.fleet
        grant, token = await asyncio.to_thread(fleet.issue, "arg-errors", "fleet:read,ship:message", "beta")
        peer = fleet.call("alpha", "ship.show", {"name": "beta"})
        self.assertEqual(peer["location"]["peer"], "fleet-beta")
        invalid = [
            ("send_message", {"to": "beta", "kind": "notice", "body": "x" * 32001}, "body", "maxLength"),
            ("send_message", {"to": "beta", "kind": "notice", "body": "é" * 32000}, "body", "maxUtf8Bytes"),
            ("send_message", {"to": "beta", "kind": "notice", "body": "ok", "task_id": "secret-value"}, "task_id", "additionalProperties"),
            ("ask_liaison", {"body": "ok", "to": "alpha"}, "to", "additionalProperties"),
            ("ask_liaison", {"body": "ok", "kind": "request"}, "kind", "additionalProperties"),
            ("send_message", {"to": "beta", "kind": "secret-enum", "body": "ok"}, "kind", "enum"),
            ("reply", {"body": "ok"}, "correlation", "required"),
            ("send_message", {"to": "beta", "kind": "notice", "body": 987654321}, "body", "type"),
            ("ask_liaison", {"body": "ok", "secret-key\nvalue": "secret-value"}, "arguments", "additionalProperties"),
        ]
        async with session(fleet, token, True) as client:
            ships = await self.tool(client, "fleet_ships")
            self.assertIn("beta", json.dumps(ships))
            listed = {t.name: t for t in (await client.list_tools()).tools}
            for name in ("send_message", "reply", "ask_liaison"):
                self.assertIn("32000", listed[name].description)
                self.assertIn("48000 bytes of UTF-8", listed[name].description)
                self.assertIn("HTTP 413", listed[name].description)
                self.assertIn("Accepted fields:", listed[name].description)
                self.assertEqual(listed[name].input_schema["properties"]["body"]["maxLength"], 32000)
            for name, args, field, rule in invalid:
                response = await client.call_tool(name, args)
                self.assertTrue(response.is_error)
                error = response.structured_content["error"]
                self.assertEqual((error["code"], error["field"], error["rule"]), ("invalid_arguments", field, rule))
                self.assertIn(field, error["message"])
                if rule == "maxLength":
                    self.assertEqual(error["message"], "body: longer than 32000 characters")
                for secret in ("secret-value", "secret-enum", "987654321", "secret-key", "x" * 100):
                    self.assertNotIn(secret, json.dumps(error))
            with sqlite3.connect(fleet.env["PRIVATEER_FLEET"]) as db:
                reasons = [row[0] for row in db.execute("SELECT reason FROM door_client_request_audit WHERE client=? AND event='request.deny' ORDER BY seq", (grant,))]
                self.assertEqual(reasons, [f"invalid_arguments tool={name} field={field} rule={rule}" for name, _, field, rule in invalid])
            # Handoffs above the old 4096-character ceiling, including the new
            # maximum, reach a ship on a separate home on the same machine.
            for size in (4097, 32000):
                sent = await self.tool(client, "send_message", {"to": "beta", "kind": "notice", "body": "h" * size})
                self.assertEqual(sent["delivery_state"], "delivered")
                stored = fleet.call("beta", "mail.show", {"id": sent["message_id"]})
                self.assertEqual(stored["body"], "h" * size)
            denied = await client.call_tool("send_message", {"to": "alpha", "kind": "notice", "body": "outside grant"})
            self.assertEqual(denied.structured_content["error"]["code"], "not_found")
        async with session(fleet, fleet.narrow_token, False) as hidden:
            unknown = await hidden.call_tool("nonexistent", {})
            for name, args, _, _ in invalid:
                rejected = await hidden.call_tool(name, args)
                self.assertEqual(rejected.structured_content, unknown.structured_content)
        with sqlite3.connect(fleet.env["PRIVATEER_FLEET"]) as db:
            reasons = [r[0] for r in db.execute("SELECT reason FROM door_client_request_audit WHERE client=?", (fleet.narrow,))]
            self.assertFalse(any("invalid_arguments" in reason for reason in reasons))
        self.assert_redacted()

    async def test_body_byte_budgets_and_core_size_refusal(self):
        fleet = self.fleet
        # Larger HTTP admission must not bypass the adapter/core mail budgets.
        fleet.proc.send_signal(signal.SIGTERM)
        await asyncio.to_thread(fleet.proc.wait, 15)
        fleet.assert_listener_closed()
        with fleet.config.open("a") as config:
            config.write("max_body_bytes = 1048576\n")
        fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env,
                                      cwd=fleet.root, stdout=subprocess.DEVNULL, stderr=fleet.stderr,
                                      start_new_session=True)
        await fleet.ready()
        with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
            db.execute("INSERT INTO machines(name,role,peer,added_ms) VALUES('collector-example','collected','{}',0)")
            db.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('collector-example','harbor','fixture')")
        grant, token = await asyncio.to_thread(fleet.issue, "byte-budgets", "ship:message", "alpha,beta,harbor")
        async with session(fleet, token, True) as client:
            invalid = []
            for name, recipient in (("send_message", "alpha"), ("send_message", "beta"),
                                    ("send_message", "harbor"), ("ask_liaison", None), ("reply", None)):
                for body, rule in (("é" * 32000, "maxUtf8Bytes"), ("界" * 24000, "maxUtf8Bytes"),
                                   ("\u0001" * 9000, "maxJsonBytes")):
                    args = {"body": body}
                    if recipient:
                        args.update(to=recipient, kind="notice")
                    if name == "reply":
                        args["correlation"] = "open-ask"
                    response = await client.call_tool(name, args)
                    error = response.structured_content["error"]
                    self.assertTrue(response.is_error)
                    self.assertEqual((error["code"], error["field"], error["rule"]),
                                     ("invalid_arguments", "body", rule))
                    self.assertIn("48000", error["message"])
                    self.assertNotIn(body[:100], json.dumps(error, ensure_ascii=False))
                    invalid.append(f"invalid_arguments tool={name} field=body rule={rule}")
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db:
                reasons = [r[0] for r in db.execute("SELECT reason FROM door_client_request_audit WHERE client=? AND event='request.deny' ORDER BY seq", (grant,))]
                self.assertEqual(reasons, invalid)
            # Exact UTF-8 budget fits ordinary local, peer-home and reverse envelopes.
            for recipient in ("alpha", "beta", "harbor"):
                sent = await self.tool(client, "send_message", {"to": recipient, "kind": "notice", "body": "é" * 24000})
                self.assertEqual(sent["delivery_state"], "queued" if recipient == "harbor" else "delivered")
                if recipient != "harbor":
                    self.assertEqual(fleet.call(recipient, "mail.show", {"id": sent["message_id"]})["body"], "é" * 24000)
        # Model a large OAuth ships list (including stale inventory names). Even
        # the conservative body budget cannot guarantee room for arbitrary grants.
        ships = ["harbor"] + [f"retired-{n:03d}-" + "s" * 110 for n in range(256)]
        with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
            db.execute("UPDATE door_clients SET ships=? WHERE id=?", (json.dumps(ships), grant))
        async with session(fleet, token, True) as client:
            args = {"to": "harbor", "kind": "notice", "body": "é" * 24000, "idempotency_key": "size-refusal"}
            refused = await client.call_tool("send_message", args)
            self.assertTrue(refused.is_error)
            self.assertEqual(refused.structured_content["error"], {
                "code": "invalid_arguments", "field": "body", "rule": "maxEnvelopeBytes",
                "message": "body: complete mail/envelope exceeds 65536 bytes; shorten body or use a report reference"})
            # Core size refusal is pre-commit, so a corrected retry may reuse the key.
            corrected = await self.tool(client, "send_message", {**args, "body": "short"})
            self.assertEqual(corrected["delivery_state"], "queued")
        with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db:
            reasons = [r[0] for r in db.execute("SELECT reason FROM door_client_request_audit WHERE client=? AND event='request.deny' ORDER BY seq", (grant,))]
            self.assertEqual(reasons[-2:], ["message_too_large", "invalid_arguments tool=send_message field=body rule=maxEnvelopeBytes"])
        self.assert_redacted()

    async def test_write_preset_matrix_and_raw_arguments(self):
        fleet = self.fleet
        _, operator = await asyncio.to_thread(fleet.issue, "operator", "fleet:read,mail:read,ship:message,work:manage,file:transfer")
        _, presence = await asyncio.to_thread(fleet.issue, "presence", "fleet:read,captain:presence")
        writes = {
            "send_message": {"to": "alpha", "body": "do work", "kind": "request"},
            "reply": {"correlation": "open-ask", "body": "answer"},
            "ask_liaison": {"body": "help"}, "dismiss": {"id": "alpha-0"},
            "away": {"action": "start"}}
        reserved = ("from", "actor", "auth", "address", "token", "resource", "payload", "command", "task")
        async with session(fleet, fleet.read_token, False) as observer, session(fleet, operator, False) as op, session(fleet, presence, True) as captain:
            self.assertFalse(set(writes) & {t.name for t in (await observer.list_tools()).tools})
            listed = {t.name: t for t in (await op.list_tools()).tools}
            self.assertEqual(set(writes) & listed.keys(), set(writes) - {"away"})
            for name in writes:
                self.assertTrue((await observer.call_tool(name, writes[name])).is_error)
                for key in reserved:
                    guessed = await observer.call_tool(name, {**writes[name], key: "forged"})
                    self.assertTrue(guessed.is_error)
                    self.assertEqual(guessed.structured_content, (await observer.call_tool("nonexistent", {})).structured_content)
                    self.assertTrue((await op.call_tool(name, {**writes[name], key: "forged"})).is_error)
                if name != "away":
                    self.assertIn("Accepted fields:", listed[name].description)
                    annotations = listed[name].model_dump(by_alias=True)["annotations"]
                    self.assertFalse(annotations["readOnlyHint"])
                    self.assertFalse(annotations["idempotentHint"], "optional key is not unconditional idempotency")
                    self.assertEqual(annotations["destructiveHint"], name == "dismiss")
                    self.assertFalse(annotations["openWorldHint"])
            for action in ("start", "back"):
                self.assertTrue((await op.call_tool("away", {"action": action})).is_error)
            started = await self.tool(captain, "away", {"action": "start", "idempotency_key": "away-start"})
            self.assertEqual(await self.tool(captain, "away", {"action": "start", "idempotency_key": "away-start"}), started)
            before = await self.tool(op, "away_status")
            for name in ("send_message", "reply", "ask_liaison", "dismiss"):
                keyed = {**writes[name], "idempotency_key": f"matrix-{name}"}
                result = await self.tool(op, name, keyed)
                self.assertEqual(await self.tool(op, name, keyed), result)
                if name != "dismiss":
                    self.assertTrue(result["message_id"])
                    self.assertEqual(result["delivery_state"], "delivered")
                    if name == "send_message":
                        shown = await self.tool(op, "mail_show", {"ship": "alpha", "id": result["message_id"]})
                        self.assertEqual(shown["mail"]["state"], "open", "delivery is not work completion")
            after = await self.tool(op, "away_status")
            before.pop("provenance"); after.pop("provenance")
            self.assertEqual(before, after)
            self.assertTrue((await op.call_tool("send_message", {"to": "missing", "body": "fail", "kind": "request"})).is_error)
            after = await self.tool(op, "away_status"); after.pop("provenance")
            self.assertEqual(before, after)
            back = await self.tool(captain, "away", {"action": "back", "idempotency_key": "away-back"})
            self.assertEqual(await self.tool(captain, "away", {"action": "back", "idempotency_key": "away-back"}), back)
        self.assert_redacted()

    async def test_reverse_only_send_explains_queue_and_local_send_does_not(self):
        fleet = self.fleet
        with sqlite3.connect(fleet.root / "state/fleet.sqlite") as conn:
            conn.execute("INSERT INTO machines(name,role,peer,added_ms) VALUES('collector-example','collected','{}',0)")
            conn.execute("INSERT INTO machine_ships(machine,name,scope) VALUES('collector-example','harbor','fixture')")
        _, token = await asyncio.to_thread(fleet.issue, "queue-writer", "ship:message,mail:read")
        async with session(fleet, token, True) as client:
            listed = {t.name: t.description for t in (await client.list_tools()).tools}
            self.assertIn("reverse_collect", listed["send_message"])
            args = {"to": "harbor", "body": "wait for collection", "kind": "notice", "idempotency_key": "queued"}
            response = await client.call_tool("send_message", args)
            self.assertFalse(response.is_error)
            queued = response.structured_content
            self.assertEqual(json.loads(response.content[0].text), queued)
            self.assertEqual(queued["route"], "reverse_collect")
            self.assertEqual(queued["machine"], "collector-example")
            self.assertEqual(queued["ship"], "harbor")
            self.assertEqual(queued["queued_on"], "this machine")
            self.assertEqual(queued["outcome_notice_ship"], "alpha")
            self.assertEqual(queued["delivery_state"], "queued")
            self.assertFalse(queued["delivered"])
            self.assertIsNone(queued["outcome"])
            summary = queued["delivery_summary"]
            for phrase in (f"Queued on {queued['queued_on']}", "delegated outbox", "harbor on collector-example",
                           "dispatcher next connects", "build that understands delegated sends", "Not yet delivered",
                           "mail_inbox, ship alpha", "collected or refused", "do not use mail_show on it"):
                self.assertIn(phrase, summary)
            self.assertEqual(await self.tool(client, "send_message", args), queued)
            with sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite") as conn:
                self.assertEqual(conn.execute("SELECT COUNT(*) FROM delegated_outbox WHERE id=?", (queued["message_id"],)).fetchone()[0], 1)
                self.assertEqual(conn.execute("SELECT COUNT(*) FROM mail WHERE id=?", (queued["message_id"],)).fetchone()[0], 0)
            local = await self.tool(client, "send_message", {"to": "alpha", "body": "local", "kind": "notice"})
            self.assertEqual(local["delivery_state"], "delivered")
            self.assertNotIn("route", local)
            self.assertNotIn("delivery_summary", local)
        # Both mailbox visibility and mail:read are required to name the notice ship.
        for label, scopes, ships in (("narrow-mail", "ship:message,mail:read", "harbor"),
                                     ("narrow-send", "ship:message", "harbor"),
                                     ("no-mail-read", "ship:message", None)):
            _, narrow_token = await asyncio.to_thread(fleet.issue, label, scopes, ships)
            async with session(fleet, narrow_token, True) as client:
                queued = await self.tool(client, "send_message", {"to": "harbor", "body": label, "kind": "notice"})
                self.assertEqual(queued["queued_on"], "this machine")
                self.assertEqual(queued["machine"], "collector-example", "the recipient's machine is visible")
                self.assertNotIn("outcome_notice_ship", queued)
                self.assertNotIn("alpha", json.dumps(queued))
                self.assertNotIn("beta", json.dumps(queued))
                self.assertNotIn("mail_inbox", queued["delivery_summary"])
                self.assertIn("captain's conversation on this machine", queued["delivery_summary"])
        self.assert_redacted()

    async def test_write_idempotency_restart_and_unequal_clients(self):
        fleet = self.fleet
        _, token = await asyncio.to_thread(fleet.issue, "writer", "fleet:read,mail:read,ship:message", "alpha")
        _, other = await asyncio.to_thread(fleet.issue, "other-writer", "ship:message", "beta")
        args = {"to": "alpha", "body": "one durable request", "kind": "request", "idempotency_key": "durable-key"}
        async with session(fleet, token, True) as first, session(fleet, token, False) as second, session(fleet, other, True) as unequal:
            values = await asyncio.gather(self.tool(first, "send_message", args), self.tool(second, "send_message", args))
            self.assertEqual(values[0], values[1])
            original = values[0]
            self.assertTrue((await first.call_tool("send_message", {**args, "body": "changed"})).is_error)
            self.assertTrue((await unequal.call_tool("send_message", {**args, "idempotency_key": "denied-target"})).is_error)
            self.assertTrue((await unequal.call_tool("ask_liaison", {"body": "forbidden default"})).is_error)
            self.assertTrue((await unequal.call_tool("reply", {"correlation": "open-ask", "body": "no"})).is_error)
            own = await self.tool(unequal, "send_message", {**args, "to": "beta"})
            self.assertNotEqual(own["message_id"], original["message_id"])
            # Explicit default urgency and omitted normal urgency normalize alike.
            self.assertEqual(await self.tool(first, "send_message", {**args, "urgency": "normal"}), original)
            for key in ("", "x" * 129, "bad\nkey", "bad\u0085key", "é" * 65, "bad\n", 42):
                self.assertTrue((await first.call_tool("send_message", {**args, "idempotency_key": key})).is_error)
            # No key means one new message per call.
            no_key = {k: v for k, v in args.items() if k != "idempotency_key"}
            a = await self.tool(first, "send_message", no_key)
            b = await self.tool(first, "send_message", no_key)
            self.assertNotEqual(a["message_id"], b["message_id"])
        fleet.proc.send_signal(signal.SIGTERM)
        await asyncio.to_thread(fleet.proc.wait, 15)
        fleet.assert_listener_closed()
        fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env,
                                      cwd=fleet.root, stdout=subprocess.DEVNULL, stderr=fleet.stderr,
                                      start_new_session=True)
        await fleet.ready()
        async with session(fleet, token, False) as client:
            self.assertEqual(await self.tool(client, "send_message", args), original)
            self.assertTrue((await client.call_tool("send_message", {**args, "body": "restart conflict"})).is_error)
        with closing(sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite")) as db, db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE id=?", (original["message_id"],)).fetchone()[0], 1)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE body='one durable request'").fetchone()[0], 3)
        self.assert_redacted()

    async def test_expired_grant_renewal_replays_without_second_effect(self):
        fleet = self.fleet
        grant, token = await asyncio.to_thread(fleet.issue, "renewable-writer", "ship:message", "alpha")
        _, other_token = await asyncio.to_thread(fleet.issue, "live-writer", "ship:message", "alpha")
        args = {"to": "alpha", "body": "renewal must not repeat", "kind": "notice", "idempotency_key": "renewed"}
        async with session(fleet, token, False) as client:
            original = await self.tool(client, "send_message", args)
        await asyncio.to_thread(fleet.cli, "door", "client", "expiry", grant, "--expires-ms", "1")
        async with session(fleet, other_token, False) as client:
            await self.tool(client, "send_message", {**args, "body": "another grant triggers cleanup"})
        # Fixture models the owner renewing the same grant (public expiry
        # extension requires interactive consent). Keep its authority unchanged.
        with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM door_write_results WHERE grant_id=?", (grant,)).fetchone()[0], 1)
            db.execute("UPDATE door_clients SET expires_ms=NULL,expiry_noted=0 WHERE id=?", (grant,))
        async with session(fleet, token, False) as client:
            self.assertEqual(await self.tool(client, "send_message", args), original)
        with closing(sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite")) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE body='renewal must not repeat'").fetchone()[0], 1)
        self.assert_redacted()

    async def test_write_uncertainty_capacity_and_replay_audit(self):
        fleet = self.fleet
        grant, token = await asyncio.to_thread(fleet.issue, "fault-writer", "ship:message,mail:read", "alpha")
        args = {"to": "alpha", "body": "fault fixture", "kind": "notice", "idempotency_key": "fault"}
        async with session(fleet, token, False) as client:
            original = await self.tool(client, "send_message", args)
            self.assertEqual(await self.tool(client, "send_message", args), original)
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                events = db.execute("SELECT event,reason FROM door_client_request_audit WHERE client=? AND event='request.replay'", (grant,)).fetchall()
                self.assertEqual(events, [("request.replay", "cached")])
                db.execute("UPDATE door_write_results SET result=NULL WHERE grant_id=?", (grant,))
            unknown = await client.call_tool("send_message", args)
            self.assertTrue(unknown.is_error)
            self.assertEqual(unknown.structured_content["error"]["code"], "outcome_unknown")
            self.assertIn("Do not resend", unknown.structured_content["error"]["message"])
            self.assertIn("mail_show", unknown.structured_content["error"]["message"])
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                self.assertEqual(db.execute("SELECT event,reason FROM door_client_request_audit WHERE client=? ORDER BY seq DESC LIMIT 1", (grant,)).fetchone(), ("request.deny", "outcome_unknown"))
                # Fill the remaining per-grant slots; no production data or raw keys.
                db.executemany("INSERT INTO door_write_results(grant_id,command,key_hash,args_hash,created_ms) VALUES(?,'send',?,?,?)",
                               [(grant, f"fixture-{n}".encode(), b"h", int(time.time() * 1000)) for n in range(255)])
            refused = await client.call_tool("send_message", {**args, "idempotency_key": "capacity"})
            self.assertTrue(refused.is_error)
            self.assertEqual(refused.structured_content["error"]["code"], "retry_capacity")
            self.assertIn("nothing ran", refused.structured_content["error"]["message"])
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                self.assertEqual(db.execute("SELECT event,reason FROM door_client_request_audit WHERE client=? ORDER BY seq DESC LIMIT 1", (grant,)).fetchone(), ("request.deny", "retry_capacity"))
                self.assertEqual(db.execute("SELECT COUNT(*) FROM door_write_results WHERE grant_id=?", (grant,)).fetchone()[0], 256)
            no_key = {k: v for k, v in args.items() if k != "idempotency_key"}
            fresh = await self.tool(client, "send_message", no_key)
            self.assertNotEqual(fresh["message_id"], original["message_id"])
            with closing(sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite")) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE body='fault fixture'").fetchone()[0], 2)
            await asyncio.to_thread(fleet.cli, "door", "client", "revoke", grant)
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM door_write_results WHERE grant_id=?", (grant,)).fetchone()[0], 0)
        self.assert_redacted()

    async def test_pre_effect_error_retries_same_key_and_save_failure_does_not(self):
        fleet = self.fleet
        grant, token = await asyncio.to_thread(fleet.issue, "retry-writer", "ship:message", "alpha")
        args = {"correlation": "later-ask", "body": "retry answer", "idempotency_key": "retry"}
        async with session(fleet, token, False) as client:
            self.assertTrue((await client.call_tool("reply", args)).is_error)
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM door_write_results WHERE grant_id=?", (grant,)).fetchone()[0], 0)
            fleet.call("alpha", "mail.send", {"id": "later-ask", "from": "alpha", "to": "captain", "kind": "request", "body": "created after failed reply"})
            result = await self.tool(client, "reply", args)
            self.assertEqual(await self.tool(client, "reply", args), result)
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                db.execute("CREATE TRIGGER fail_save BEFORE UPDATE ON door_write_results BEGIN SELECT RAISE(FAIL,'fixture'); END")
            send = {"to": "alpha", "kind": "notice", "body": "saved effect lost result", "idempotency_key": "save-failure"}
            for _ in range(2):
                failed = await client.call_tool("send_message", send)
                self.assertTrue(failed.is_error)
                self.assertEqual(failed.structured_content["error"]["code"], "outcome_unknown")
            with closing(sqlite3.connect(fleet.root / "alpha/.privateer/privateer.sqlite")) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE body='saved effect lost result'").fetchone()[0], 1)
        self.assert_redacted()

    async def test_write_live_grant_edits(self):
        fleet = self.fleet
        grant, token = await asyncio.to_thread(fleet.issue, "editable", "ship:message,work:manage", "alpha,beta")
        args = {"to": "beta", "kind": "notice", "body": "before restriction", "idempotency_key": "restricted"}
        async with session(fleet, token, False) as client:
            await self.tool(client, "send_message", args)
            # Fixture edits model the core-owned grant editor; the public CLI
            # currently exposes pause/revoke but no scope/ship editing command.
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                db.execute("UPDATE door_clients SET ships=? WHERE id=?", (json.dumps(["alpha"]), grant))
            self.assertTrue((await client.call_tool("send_message", args)).is_error, "cached result cannot bypass restriction")
            self.assertTrue((await client.call_tool("send_message", {**args, "idempotency_key": "new"})).is_error)
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                db.execute("UPDATE door_clients SET scopes=? WHERE id=?", (json.dumps(["fleet:read"]), grant))
            self.assertNotIn("send_message", {t.name for t in (await client.list_tools()).tools})
            self.assertTrue((await client.call_tool("send_message", args)).is_error)
            self.assertTrue((await client.call_tool("send_message", {**args, "auth": "forged"})).is_error)
            await asyncio.to_thread(fleet.cli, "door", "client", "pause", grant)
            with self.assertRaises(Exception):
                await client.list_tools()
            await asyncio.to_thread(fleet.cli, "door", "client", "revoke", grant)
            with self.assertRaises(Exception):
                await client.call_tool("send_message", args)
        self.assert_redacted()

    async def test_approval_replies_and_dismiss_stay_gated(self):
        fleet = self.fleet
        _, operator = await asyncio.to_thread(fleet.issue, "operator", "ship:message,work:manage")
        _, full = await asyncio.to_thread(fleet.issue, "full", "ship:message,work:manage,approval:answer")
        fleet.call("alpha", "charter.put", {"id": "project", "objective": "Fixture approvals", "repo": str(fleet.root / "alpha"),
            "min_free_bytes": 0, "actions": ["inspect", "implement", "test", "deploy_production"]})
        fleet.call("alpha", "task.create", {"id": "release", "charter_id": "project", "owner": "alpha", "title": "Fixture release"})
        fleet.call("alpha", "mail.send", {"id": "approval-ask", "from": "alpha", "to": "captain", "kind": "request",
            "task_id": "release", "body": "approve step", "payload": {"approves": {"action": "Submit build 55", "ttl_secs": 60}}})
        fleet.call("alpha", "mail.send", {"id": "production-ask", "from": "alpha", "to": "captain", "kind": "request",
            "task_id": "release", "body": "deploy?", "request_kind": "production_approval",
            "payload": {"artifact": "sha256:exact", "environment": "production"}})
        privileged = fleet.call("alpha", "privileged.stage", {"from": "alpha", "task_id": "release", "program": "/usr/bin/id",
            "args": ["-u"], "cwd": "/", "reason": "Fixture identity check"})
        async with session(fleet, operator, False) as op, session(fleet, full, True) as captain:
            self.assertTrue((await op.call_tool("reply", {"correlation": "approval-ask", "body": "yes"})).is_error)
            self.assertTrue((await op.call_tool("reply", {"correlation": "approval-ask", "body": "yes", "payload": {"approval": True}})).is_error)
            source = await self.tool(op, "ask_liaison", {"body": "relay this answer", "idempotency_key": "liaison-source"})
            for ident in ("approval-ask", "production-ask", privileged["id"]):
                relayed = await asyncio.to_thread(subprocess.run, [str(BIN), "--home", str(fleet.root / "alpha/.privateer"),
                    "--role", "agent", "--actor", "alpha", "captain.relay", "--json",
                    json.dumps({"source": source["message_id"], "request": ident, "body": "yes"})],
                    env=fleet.env, cwd=fleet.root, capture_output=True, timeout=10)
                self.assertNotEqual(relayed.returncode, 0, "liaison must not launder delegated approval authority")
                self.assertTrue(any(reason in relayed.stderr.decode() for reason in
                                    ("cannot be relayed", "trusted_confirmation_required")))
                self.assertEqual(fleet.call("alpha", "mail.show", {"id": ident})["state"], "open")
            for client in (op, captain):
                for ident in ("production-ask", privileged["id"]):
                    for tool_name, args in (("reply", {"correlation": ident, "body": "yes"}), ("dismiss", {"id": ident})):
                        self.assertTrue((await client.call_tool(tool_name, args)).is_error)
                        self.assertTrue((await client.call_tool(tool_name, {**args, "payload": {"approval": True}})).is_error)
                    self.assertEqual(fleet.call("alpha", "mail.show", {"id": ident})["state"], "open")
            await self.tool(captain, "reply", {"correlation": "approval-ask", "body": "yes", "idempotency_key": "approval"})
            self.assertEqual(fleet.call("alpha", "mail.show", {"id": "approval-ask"})["state"], "answered")
        self.assert_redacted()

    def assert_redacted(self):
        fleet = self.fleet
        audit = fleet.cli("door", "client", "audit")
        logs = audit + (fleet.root / "supervisor.stderr").read_text()
        door_dir = fleet.root / "state/doors/mcp-test"
        for file in door_dir.glob("*.log*"):
            logs += file.read_text()
        self.assertIn("local-only", logs)
        for token in fleet.tokens + ["invalid-fixture-credential"]:
            # Avoid unittest printing the actual credential even on failure.
            self.assertFalse(token in logs, "credential leaked to diagnostic/audit")


class MCPIngressTests(unittest.IsolatedAsyncioTestCase):
    async def test_auth_store_outage_is_503_and_malformed_bearer_never_receives(self):
        sys.path.insert(0, str(PLUGIN))
        try:
            from door import make_app
        finally:
            sys.path.pop(0)

        class UnavailableCore:
            calls = 0
            async def call(self, *args, **kwargs):
                self.calls += 1
                return {"ok": False, "error": {"code": "unavailable", "message": "Core unavailable"}}

        core = UnavailableCore()
        app = make_app(core, {"port": 8767, "max_in_flight": 1})
        for header, expected in ((None, 401), (b"Bearer a b", 401), (b"Bearer x\t", 401), (b"Bearer fixture", 503)):
            reads = 0
            messages = []
            async def receive():
                nonlocal reads
                reads += 1
                return {"type": "http.request", "body": b"", "more_body": False}
            async def send(message):
                messages.append(message)
            scope = {"type": "http", "path": "/mcp", "query_string": b"", "method": "POST",
                     "client": ("127.0.0.1", 40000), "headers": [(b"host", b"127.0.0.1:8767")]}
            if header:
                scope["headers"].append((b"authorization", header))
            await app(scope, receive, send)
            self.assertEqual(messages[0]["status"], expected)
            self.assertEqual(reads, 0, "failed authentication must never receive a body")
            self.assertEqual(app.active, 0)
        self.assertEqual(core.calls, 1)


if __name__ == "__main__":
    unittest.main()
