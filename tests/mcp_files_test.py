#!/usr/bin/env python3
"""Isolated real supervised MCP uploads/reports plus deterministic staging races."""
import sys
sys.dont_write_bytecode = True
import asyncio
import base64
from contextlib import ExitStack, closing
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from mcp_door_test import Fleet, BIN, session
import httpx2
from file_transfer import Staging, UPLOAD_BYTES, UPLOAD_ENVELOPE, decode, valid_name
from door import Ingress, JSON_MAX_DEPTH


class StagingTests(unittest.TestCase):
    def test_canonical_base64_names_and_caps(self):
        for bad in ["Zg=", "Zh==", "Zg==\n", "!!!!", "é"]:
            with self.assertRaises(ValueError):
                decode(bad)
        self.assertEqual(decode("Zg=="), b"f")
        self.assertEqual(decode(""), b"")
        for name in ["", ".hidden", "a/b", "a\\b", "nul\0", "x\x85", "é" * 65]:
            self.assertFalse(valid_name(name))
        self.assertTrue(valid_name("é" * 64))
        with self.assertRaises(ValueError):
            decode(base64.b64encode(b"x" * (UPLOAD_BYTES + 1)).decode())

    def test_exclusive_leaf_links_quotas_cleanup_and_restart(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root) / "state"
            state.mkdir()
            staging = Staging(state)
            self.addCleanup(staging.close)
            with staging.stage("grant", b"bytes") as path:
                self.assertEqual(Path(path).stat().st_mode & 0o777, 0o600)
                with staging.stage("grant", b"bytes"):
                    with self.assertRaises(OSError):
                        with staging.stage("grant", b""):
                            pass
            self.assertEqual(list(state.iterdir()), [])
            self.assertEqual(staging.total, 0)
            with ExitStack() as stack:
                for i in range(8):
                    stack.enter_context(staging.stage(str(i), b"x" * UPLOAD_BYTES))
                with self.assertRaises(OSError):
                    with staging.stage("extra", b"x"):
                        pass
            target = Path(root) / "secret"
            target.write_bytes(b"secret")
            ident = "0" * 32
            leaf = state / (".mcp-upload-" + ident)
            for hardlink in (False, True):
                if hardlink:
                    os.link(target, leaf)
                else:
                    leaf.symlink_to(target)
                with patch("file_transfer.uuid.uuid4") as mocked:
                    mocked.return_value.hex = ident
                    with self.assertRaises(OSError):
                        with staging.stage("grant", b"replacement"):
                            pass
                self.assertEqual(target.read_bytes(), b"secret")
                leaf.unlink()
            # Crash leftovers are removed on adapter restart, never followed.
            leaf.symlink_to(target)
            restarted = Staging(state)
            restarted.close()
            self.assertFalse(leaf.exists())
            self.assertEqual(target.read_bytes(), b"secret")
            self.assertEqual(staging.active, {})

    def test_restart_skips_reserved_directories_and_special_files(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            directory = state / (".mcp-upload-" + "0" * 32)
            directory.mkdir()
            (directory / "retained").write_bytes(b"keep")
            fifo = state / (".mcp-upload-" + "1" * 32)
            os.mkfifo(fifo)
            regular = state / (".mcp-upload-" + "2" * 32)
            regular.write_bytes(b"abandoned")
            link = state / (".mcp-upload-" + "3" * 32)
            link.symlink_to(directory, target_is_directory=True)
            staging = Staging(state)
            staging.close()
            self.assertTrue(directory.is_dir())
            self.assertEqual((directory / "retained").read_bytes(), b"keep")
            self.assertTrue(fifo.is_fifo())
            self.assertFalse(regular.exists())
            self.assertFalse(link.is_symlink())


class IngressFileTests(unittest.IsolatedAsyncioTestCase):
    async def request(self, payload, scopes=None, authenticated=True, body_limit=1024, chunk_size=512):
        reads, calls, relayed, messages = [], [], [], []
        class Core:
            async def call(self, token, command, args, resource=None):
                calls.append(command)
                if not authenticated:
                    return {"ok": False, "error": {"code": "not_found"}}
                return {"ok": True, "result": {"scopes": scopes or [], "provenance": {"client": "fixture"}}}
        async def app(scope, receive, send):
            relayed.append((await receive())["body"])
            await send({"type": "http.response.start", "status": 200, "headers": []})
            await send({"type": "http.response.body", "body": b""})
        async def receive():
            # Authentication must precede even the first bounded receive.
            self.assertEqual(calls, ["capabilities"])
            offset = sum(reads)
            chunk = payload[offset:offset + chunk_size]
            reads.append(len(chunk))
            return {"type": "http.request", "body": chunk, "more_body": offset + len(chunk) < len(payload)}
        async def send(message):
            messages.append(message)
        scope = {"type": "http", "method": "POST", "path": "/mcp", "query_string": b"",
                 "client": ("127.0.0.1", 40000), "headers": [(b"host", b"127.0.0.1:8767"),
                 (b"authorization", b"Bearer syntactically-valid-fixture")]}
        ingress = Ingress(app, Core(), {"max_body_bytes": body_limit})
        await ingress(scope, receive, send)
        self.assertEqual(ingress.active, 0)
        return messages[0]["status"], sum(reads), relayed

    async def test_large_bodies_authenticate_before_receive_or_json(self):
        upload = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": "send_file", "arguments": {"content": "x" * 70000}}}).encode()
        with patch("door.json.loads", side_effect=AssertionError("must not parse before admission")):
            huge = b"x" * UPLOAD_ENVELOPE
            status, received, relayed = await self.request(huge, authenticated=False)
            self.assertEqual((status, received, relayed), (401, 0, []))
            status, received, relayed = await self.request(huge, scopes=["mail:read"])
            self.assertEqual(status, 413)
            self.assertLessEqual(received, 1024 + 512)  # first excess chunk is refused, not copied
            self.assertEqual(relayed, [])
        status, received, relayed = await self.request(upload, scopes=["file:transfer"])
        self.assertEqual((status, received, relayed), (200, len(upload), [upload]))
        other_tool = upload.replace(b'"send_file"', b'"get_report"')
        status, _, relayed = await self.request(other_tool, scopes=["file:transfer"])
        self.assertEqual((status, relayed), (413, []))

    async def test_oversized_single_chunk_never_enters_body_buffer(self):
        class BoundedBody(bytearray):
            def extend(self, chunk):
                if len(self) + len(chunk) > 1024:
                    raise AssertionError("oversized ASGI chunk was copied into body buffer")
                super().extend(chunk)
        with patch("door.bytearray", BoundedBody, create=True):
            status, _, relayed = await self.request(b"x" * 70000, scopes=["mail:read"], chunk_size=70000)
        self.assertEqual((status, relayed), (413, []))

    async def test_json_depth_boundary_and_quoted_delimiters(self):
        at_limit = b"[" * JSON_MAX_DEPTH + b"0" + b"]" * JSON_MAX_DEPTH
        status, _, relayed = await self.request(at_limit)
        self.assertEqual((status, relayed), (200, [at_limit]))
        over_limit = b"[" + at_limit + b"]"
        status, _, relayed = await self.request(over_limit)
        self.assertEqual((status, relayed), (400, []))
        quoted = json.dumps({"text": '[{\\"\\\\' * 100 + ']}é' * 100}, ensure_ascii=False).encode()
        status, _, relayed = await self.request(quoted, body_limit=4096)
        self.assertEqual((status, relayed), (200, [quoted]))
        # Keep the defensive catch even if the interpreter raises below our cap.
        with patch("door.json.loads", side_effect=RecursionError):
            status, _, relayed = await self.request(b'{}')
        self.assertEqual((status, relayed), (400, []))

    async def test_recursive_json_fails_closed_under_both_limits(self):
        for payload, expected in [(b"[" * 1500 + b"]" * 1500, 400),
                                  (b"[" * 1500 + b"]" * 1500 + b" " * 70000, 413)]:
            status, _, relayed = await self.request(payload, scopes=["file:transfer"], body_limit=65536)
            self.assertEqual((status, relayed), (expected, []))
            status, received, relayed = await self.request(payload, authenticated=False)
            self.assertEqual((status, received, relayed), (401, 0, []))


class MCPFilesTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.fleet = await asyncio.to_thread(Fleet)
        self.addAsyncCleanup(asyncio.to_thread, self.fleet.close)
        self.grant, self.token = await asyncio.to_thread(self.fleet.issue, "files", "file:transfer,mail:read", "alpha")
        await self.fleet.ready()

    async def call(self, client, name, args):
        response = await client.call_tool(name, args)
        self.assertFalse(response.is_error, response.structured_content)
        return response.structured_content

    def db(self, ship="alpha"):
        return sqlite3.connect(self.fleet.root / ship / ".privateer/privateer.sqlite")

    async def test_upload_retries_bounds_discovery_and_restart(self):
        fleet = self.fleet
        args = {"to":"alpha", "name":"receipt.txt", "content":"aGVsbG8=", "idempotency_key":"upload-one"}
        async with session(fleet, self.token, True) as client:
            tools = {t.name:t for t in (await client.list_tools()).tools}
            self.assertIn("send_file", tools)
            self.assertFalse(tools["send_file"].annotations.read_only_hint)
            self.assertNotIn("get_file", tools)
            self.assertNotIn("list_files", tools)
            first = await self.call(client, "send_file", args)
            self.assertEqual(await self.call(client, "send_file", args), first)
            concurrent = await asyncio.gather(*(client.call_tool("send_file", args) for _ in range(2)))
            self.assertTrue(all(not r.is_error and r.structured_content == first for r in concurrent))
            self.assertEqual(first["delivery_state"], "stored")
            self.assertFalse(first["notified"])
            self.assertNotIn("path", json.dumps(first))
            for changed in [{"content":"d29ybGQ="}, {"to":"beta"}, {"name":"../secret"},
                            {"name":"é" * 65}, {"content":"Zh=="}, {"body":"extra"},
                            {"content_file":"/never/accept"}]:
                response = await client.call_tool("send_file", {**args, **changed})
                self.assertTrue(response.is_error)
            with closing(self.db()) as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM files").fetchone()[0], 1)
                self.assertEqual(db.execute("SELECT COUNT(*) FROM mail WHERE sender='captain'").fetchone()[0], 0)
            self.assertEqual(list((fleet.root / "state/doors/mcp-test/state").glob(".mcp-upload-*")), [])
            # Recipient discovers the bytes through the real agent command.
            found = json.loads(await asyncio.to_thread(fleet.cli, "--home", str(fleet.root / "alpha/.privateer"),
                "--role", "agent", "--actor", "alpha", "file.describe", "--json",
                json.dumps({"file_id":first["file_id"]})))
            self.assertEqual(Path(found["path"]).read_bytes(), b"hello")
            self.assertEqual(found["sender"], "captain")
            # Exercise the approved two-step protocol (separate message grant).
            _, messenger = await asyncio.to_thread(fleet.issue, "notifier", "ship:message", "alpha")
            async with session(fleet, messenger, True) as notify:
                await self.call(notify, "send_message", {"to":"alpha", "kind":"notice", "body":"Read " + first["file_id"]})
            # Exactly the decoded cap traverses HTTP and core; no newline wire
            # carries the encoded payload (only a short private staged path).
            large = {"to":"alpha", "name":"max.bin", "content":base64.b64encode(b"x" * UPLOAD_BYTES).decode()}
            maximum = await self.call(client, "send_file", large)
            self.assertEqual(maximum["bytes"], UPLOAD_BYTES)
            response = await client.call_tool("send_file", {**large, "content":base64.b64encode(b"x" * (UPLOAD_BYTES + 1)).decode()})
            self.assertTrue(response.is_error)
        fleet.proc.send_signal(signal.SIGTERM)
        await asyncio.to_thread(fleet.proc.wait, 15)
        fleet.assert_listener_closed()
        fleet.proc = subprocess.Popen([str(BIN), "door", "run", "mcp-test"], env=fleet.env,
            cwd=fleet.root, stdout=subprocess.DEVNULL, stderr=fleet.stderr, start_new_session=True)
        await fleet.ready()
        async with session(fleet, self.token, True) as client:
            self.assertEqual(await self.call(client, "send_file", args), first)
        self.assertEqual(list((fleet.root / "state/doors/mcp-test/state").glob(".mcp-upload-*")), [])

    async def test_report_publication_ship_scopes_cursors_and_operator_gate(self):
        fleet = self.fleet
        reports = {}
        for ship in ("alpha", "beta"):
            reports[ship] = fleet.call(ship, "report.put", {"from":ship, "title":"Report", "format":"markdown", "content":"safe " + ship})["report_id"]
            fleet.call(ship, "mail.send", {"from":ship, "to":"captain", "kind":"notice", "body":"Report attached",
                "payload":{"report":{"report_id":reports[ship]}}})
        unpublished = fleet.call("alpha", "report.put", {"from":"alpha", "title":"Private", "format":"html", "content":"not published"})["report_id"]
        # A forged foreign-author reference cannot publish the local report.
        with closing(self.db()) as db, db:
            db.execute("UPDATE mail SET payload=? WHERE id='alpha-0'", (json.dumps({"report":{"report_id":reports["beta"]}}),))
            before = db.execute("SELECT * FROM mail_cursor").fetchall()
        report_source = fleet.root / "bounded-report.txt"
        report_source.write_text(chr(1) * (256 * 1024))
        maximum = fleet.call("alpha", "report.put", {"from":"alpha", "title":"Bounded", "format":"html", "content_file":str(report_source)})["report_id"]
        fleet.call("alpha", "mail.send", {"from":"alpha", "to":"captain", "kind":"notice", "body":"Bounded report", "payload":{"report":{"report_id":maximum}}})
        async with session(fleet, self.token, True) as client:
            report = await self.call(client, "get_report", {"report_id":reports["alpha"]})
            self.assertEqual(report["content"], "safe alpha")
            self.assertEqual(report["coverage"]["remote"], "unavailable")
            self.assertEqual(report["coverage"]["schema"], "coverage.v1")
            self.assertEqual(report["coverage"]["scope"], "local")
            bounded = await self.call(client, "get_report", {"report_id":maximum})
            self.assertEqual(bounded["bytes"], 256 * 1024)
            for id in [reports["beta"], unpublished, "report-unknown"]:
                denied = await client.call_tool("get_report", {"report_id":id})
                self.assertEqual(denied.structured_content["error"]["code"], "not_found")
            # A live grant narrowing applies to an already connected session.
            with closing(sqlite3.connect(fleet.env["PRIVATEER_FLEET"])) as db, db:
                db.execute("UPDATE door_clients SET ships='[\"beta\"]' WHERE id=?", (self.grant,))
            denied = await client.call_tool("get_report", {"report_id":reports["alpha"]})
            self.assertTrue(denied.is_error)
        with closing(self.db()) as db:
            self.assertEqual(db.execute("SELECT * FROM mail_cursor").fetchall(), before)
        _, file_only = await asyncio.to_thread(fleet.issue, "upload-only", "file:transfer", "alpha")
        async with session(fleet, file_only, True) as client:
            self.assertNotIn("get_report", {t.name for t in (await client.list_tools()).tools})
            self.assertTrue((await client.call_tool("get_report", {"report_id":reports["alpha"]})).is_error)
        output = subprocess.run([str(BIN), "--home", str(fleet.root / "alpha/.privateer"),
            "--role", "agent", "--actor", "alpha", "report.get", "--json", json.dumps({"id":reports["alpha"]})],
            env=fleet.env, cwd=fleet.root, capture_output=True, timeout=10)
        self.assertNotEqual(output.returncode, 0)

    async def test_large_body_exception_is_only_one_upload_call(self):
        fleet = self.fleet
        headers = {"Authorization":"Bearer " + self.token, "Content-Type":"application/json"}
        async with httpx2.AsyncClient(trust_env=False, timeout=15) as http:
            for payload in [
                {"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"get_report", "arguments":{"report_id":"x" * 70000}}},
                [{"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"send_file", "arguments":{"content":"x" * 70000}}}],
            ]:
                response = await http.post(fleet.url, headers=headers, content=json.dumps(payload))
                self.assertEqual(response.status_code, 413)
            for token, payload, expected in [
                ("invalid-fixture-token", b"x" * 70000, 401),
                (fleet.read_token, b"x" * 70000, 413),
                (self.token, b"[" * 1500 + b"]" * 1500, 400),
                (self.token, b"[" * 1500 + b"]" * 1500 + b" " * 70000, 413),
                ("invalid-fixture-token", b"[" * 1500 + b"]" * 1500, 401),
            ]:
                response = await http.post(fleet.url, headers={**headers, "Authorization": "Bearer " + token}, content=payload)
                self.assertEqual(response.status_code, expected)
            response = await http.post(fleet.url, headers=headers, content=b"{" + b"x" * 70000)
            self.assertEqual(response.status_code, 413)
            response = await http.post(fleet.url, headers=headers, content=b"x" * (UPLOAD_ENVELOPE + 1))
            self.assertEqual(response.status_code, 413)
            response = await http.post(fleet.url.replace("/mcp", "/token"), headers=headers, content=b"x" * 70000)
            self.assertEqual(response.status_code, 413)
        # Denied tools remain absent/nonrevealing, including deferred downloads.
        async with session(fleet, fleet.empty_token, True) as client:
            for name in ["send_file", "get_report", "get_file", "list_files"]:
                denied = await client.call_tool(name, {"path":"/not/exposed"})
                self.assertEqual(denied.structured_content["error"]["code"], "not_found")


if __name__ == "__main__":
    unittest.main()
