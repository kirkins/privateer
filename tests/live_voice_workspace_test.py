"""Offline scoped workspace acceptance: no provider, live fleet or credentials.

The existing scoped fixture is composed at setup time, never inherited, so its
voice acceptance suite runs only once in the explicit CI load_tests chain.
"""
import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
import uuid
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "plugins/live-voice"))
import scoped
import scoped_access as access
import scoped_runtime
import voice
import web


class WorkspaceTest(unittest.TestCase):
    def setUp(self):
        # Local import avoids a module-load cycle through scoped.load_tests.
        import live_voice_scoped_test as fixture
        self.fixture = fixture
        self.alpha, self.beta = fixture.ALPHA, fixture.BETA
        self.code = lambda device: fixture.ScopedTest.code(self, device)
        fixture.ScopedTest.setUp(self)

    def request(self, action, data, device=None):
        return self.door.request("/api/workspace/" + action,
                                 self.tokens[device or self.alpha], data)

    def open(self, device=None, **data):
        return self.request("open", data, device)

    def read(self, workspace, view="directory", args=None, device=None):
        data = dict(workspace_id=workspace["workspace_id"], view=view)
        if args is not None:
            data["args"] = args
        return self.request("read", data, device)

    def assert_error(self, status, operation, code=None):
        with self.assertRaises(web.ApiError) as caught:
            operation()
        self.assertEqual(caught.exception.status, status)
        if code:
            self.assertEqual(caught.exception.code, code)
        return caught.exception

    def restart(self):
        self.door.close()
        original = scoped.Enrollments
        with mock.patch.object(scoped, "Enrollments", side_effect=lambda *a:
                               original(*a, clock=lambda: self.ms[0])):
            self.door = scoped.Door(dict(door="alpha", state_dir=self.root), self.cfg,
                                    self.core, lambda _: None, clock=lambda: self.clock[0])
        self.addCleanup(self.door.close)

    def background_read(self, workspace, view="directory", device=None):
        result = {}
        def run():
            try:
                result["value"] = self.read(workspace, view, device=device)
            except BaseException as error:
                result["error"] = error
        worker = threading.Thread(target=run)
        worker.start()
        return worker, result

    def finish(self, worker, release):
        release.set()
        worker.join(5)
        self.assertFalse(worker.is_alive(), "workspace read did not finish")

    def assert_no_release(self, result, status=None):
        self.assertNotIn("value", result)
        self.assertIsInstance(result.get("error"), web.ApiError)
        if status is not None:
            self.assertEqual(result["error"].status, status)

    def server(self, pairing=None, api=None):
        server = web.serve(0, pairing or self.door, api or self.door)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        wire = self.fixture.Wire.__new__(self.fixture.Wire)
        wire.port = server.server_address[1]
        return wire

    def test_offline_directory_has_closed_projection_and_no_voice_dependency(self):
        self.assertIn("workspace_reads", self.door.features)
        with mock.patch.object(voice, "codex_version", side_effect=AssertionError("provider probe")), \
                mock.patch.object(voice, "VoiceSession", side_effect=AssertionError("provider launch")):
            workspace = self.open()
            self.assertEqual(set(workspace), {"device_id", "workspace_id", "views", "expires_in_secs"})
            self.assertEqual(workspace["device_id"], self.alpha)
            self.assertEqual(workspace["views"], ["directory", "runtime"])
            self.assertEqual(workspace["expires_in_secs"], 900)
            page = self.read(workspace)
            self.assertEqual(set(page), {"device_id", "workspace_id", "view", "data"})
            self.assertEqual(page["device_id"], self.alpha)
            self.assertEqual(page["workspace_id"], workspace["workspace_id"])
            self.assertEqual(page["view"], "directory")
            self.assertEqual(set(page["data"]), {"names", "coverage"})
            self.assertEqual(page["data"]["names"], ["alpha"])
            self.assertFalse(page["data"]["coverage"]["complete"])
            self.assertTrue(page["data"]["coverage"]["inventory_complete"])
            serialized = json.dumps(page)
            for hidden in ("worker-secret", "capacity-secret", "totals-secret", "machine-secret", "workers"):
                self.assertNotIn(hidden, serialized)
            before = list(self.core.calls)
            for _ in range(25):
                state = self.door.request("/api/state", self.tokens[self.alpha], {})
                self.assertEqual((state["phase"], state["session_id"]), ("idle", None))
            self.assertEqual(self.core.calls, before)
            self.assertIsNone(self.door.active)
            self.request("close", {"workspace_id": workspace["workspace_id"]})
        self.assertEqual({command for _, command in self.core.calls}, {"protocol", "capabilities", "ships"})
        self.assertEqual(self.core.envelopes, [])

    def test_optional_client_is_validated_without_extension_capabilities(self):
        workspace = self.open(client={"name": "Plain browser", "screen": True})
        self.assertEqual(self.read(workspace)["data"]["names"], ["alpha"])
        for client in (False, "extension", {"name": "\nunsafe"}, {"screen": "yes"}):
            with self.subTest(client=client):
                self.assert_error(400, lambda: self.open(client=client))
        # Invalid replacement attempts leave the existing workspace usable.
        self.assertEqual(self.read(workspace)["view"], "directory")

    def test_ungranted_workspace_opens_empty_and_cannot_read(self):
        for scopes in ([], ["ship:message"], ["mail:read", "work:manage", "fleet:admin"]):
            with self.subTest(scopes=scopes):
                self.core.scopes[self.beta] = scopes
                workspace = self.open(self.beta)
                self.assertEqual(workspace["views"], [])
                before = len(self.core.calls)
                for view in ("directory", "runtime"):
                    self.assert_error(403, lambda: self.read(workspace, view, device=self.beta), "access_denied")
                self.assertFalse(any(command in ("ships", "status", "ship_runtime")
                                     for _, command in self.core.calls[before:]))
        self.assertEqual(self.core.envelopes, [])

    def test_runtime_uses_fixed_selectors_safe_rendering_and_no_authored_fields(self):
        workspace = self.open()
        for selectors, command in (({}, "status"), ({"machine": "local"}, "status"),
                                   ({"ship": "alpha"}, "ship_runtime"),
                                   ({"machine": "local", "ship": "alpha"}, "ship_runtime")):
            with self.subTest(selectors=selectors):
                page = self.read(workspace, "runtime", selectors)
                self.assertEqual(set(page["data"]), {"available", "text"})
                self.assertIs(page["data"]["available"], True)
                self.assertIn("open task backlog: 40", page["data"]["text"])
                self.assertIn(scoped_runtime.MAIL, page["data"]["text"])
                self.assertNotIn("canary", json.dumps(page))
                expected = selectors if command == "ship_runtime" else dict(selectors, schema="runtime.v1")
                self.assertEqual(self.core.runtime_calls[-1], (command, expected))
        self.assertEqual(self.core.envelopes, [])

    def test_strict_shapes_and_views_cannot_escape_to_tools_or_writes(self):
        workspace = self.open()
        before = len(self.core.calls)
        for data in ({"command": "send"}, {"args": {}}, {"tools": []}, {"context": "secret"},
                     {"workspace_id": workspace["workspace_id"]}):
            with self.subTest(open=data):
                self.assert_error(400, lambda: self.request("open", data))
        for extra in ({"command": "send"}, {"args": {}, "to": "alpha"}, {"body": "hello"},
                      {"session_id": "voice"}):
            data = dict(workspace_id=workspace["workspace_id"], view="directory", **extra)
            with self.subTest(read=data):
                self.assert_error(400, lambda: self.request("read", data))
        for view in ("mail", "send", "work_enqueue", "", None, 1):
            with self.subTest(view=view):
                self.assert_error(400, lambda: self.read(workspace, view))
        for args in ({"ship": "alpha"}, {"machine": "local"}):
            with self.subTest(directory=args):
                self.assert_error(400, lambda: self.read(workspace, "directory", args))
        for args in ([], None, {"command": "send"}, {"schema": "runtime.v1"},
                     {"include_text": False}, {"ship": "../alpha"}, {"machine": "x\nINJECT"}):
            with self.subTest(runtime=args):
                data = dict(workspace_id=workspace["workspace_id"], view="runtime", args=args)
                self.assert_error(400, lambda: self.request("read", data))
        self.assert_error(400, lambda: self.request("close", dict(workspace_id=workspace["workspace_id"], command="send")))
        self.assertFalse(any(command != "capabilities" for _, command in self.core.calls[before:]))
        self.assertEqual(self.read(workspace)["data"]["names"], ["alpha"])
        self.assertEqual(self.core.envelopes, [])

    def test_runtime_failure_and_malformed_metadata_return_only_fixed_unavailable(self):
        workspace = self.open()
        original = self.core.call
        for problem in (web.ApiError(503, "unavailable", "diagnostic-canary"),
                        web.ApiError(403, "access_denied", "diagnostic-canary"),
                        {"schema": "runtime.v1", "authored": "diagnostic-canary"}):
            def failed(binding, command, args=None, **kwargs):
                if command in ("status", "ship_runtime"):
                    if isinstance(problem, Exception):
                        raise problem
                    return problem
                return original(binding, command, args, **kwargs)
            with self.subTest(problem=type(problem).__name__), \
                    mock.patch.object(self.core, "call", side_effect=failed):
                result = self.read(workspace, "runtime")
            self.assertEqual(result["data"], dict(available=False, text=scoped_runtime.UNAVAILABLE))
            self.assertNotIn("diagnostic-canary", json.dumps(result))
        self.assertTrue(self.read(workspace, "runtime")["data"]["available"])

    def test_directory_rejects_out_of_scope_names_and_malformed_projection(self):
        original = self.core.call
        for malformed in ({"ships": []}, dict(self.fixture.projection(), ships=[dict(name="beta")])):
            workspace = self.open()
            def failed(binding, command, args=None, **kwargs):
                return malformed if command == "ships" else original(binding, command, args, **kwargs)
            with self.subTest(malformed=malformed), mock.patch.object(self.core, "call", side_effect=failed):
                error = self.assert_error(503, lambda: self.read(workspace), "unavailable")
                self.assertNotIn("beta", str(error))

    def test_directory_transport_errors_do_not_release_diagnostics(self):
        original = self.core.call
        for status, code in ((403, "access_denied"), (503, "unavailable"), (409, "surprise_code")):
            workspace = self.open()
            def failed(binding, command, args=None, **kwargs):
                if command == "ships":
                    raise web.ApiError(status, code, "private-directory-diagnostic-canary")
                return original(binding, command, args, **kwargs)
            with self.subTest(code=code), mock.patch.object(self.core, "call", side_effect=failed):
                error = self.assert_error(403 if status == 403 else 503, lambda: self.read(workspace))
                self.assertNotIn("private-directory-diagnostic-canary", str(error))
                self.assertEqual(error.code, "access_denied" if status == 403 else "unavailable")

    def test_wrong_foreign_replaced_and_closed_workspace_never_release(self):
        workspace = self.open()
        beta = self.open(self.beta)
        self.assert_error(409, lambda: self.read(workspace, device=self.beta), "stale_state")
        self.assert_error(409, lambda: self.request("close", {"workspace_id": workspace["workspace_id"]}, self.beta), "stale_state")
        self.assert_error(409, lambda: self.read(dict(workspace_id="unknown")), "stale_state")
        replacement = self.open()
        self.assertNotEqual(workspace["workspace_id"], replacement["workspace_id"])
        self.assert_error(409, lambda: self.read(workspace), "stale_state")
        self.assert_error(409, lambda: self.request("close", {"workspace_id": workspace["workspace_id"]}), "stale_state")
        self.assertEqual(self.read(replacement)["data"]["names"], ["alpha"])
        self.request("close", {"workspace_id": replacement["workspace_id"]})
        self.assert_error(409, lambda: self.read(replacement), "stale_state")
        # Another device's workspace survives all same-device lifecycle changes.
        self.assert_error(403, lambda: self.read(beta, device=self.beta), "access_denied")

    def test_voice_start_stop_and_workspace_lifecycle_are_independent(self):
        workspace = self.open()
        ctx = self.fixture.ScopedTest.start(self)
        self.assertEqual(self.read(workspace)["data"]["names"], ["alpha"])
        self.request("close", {"workspace_id": workspace["workspace_id"]})
        self.assertIs(self.door.active, ctx)
        self.assertFalse(ctx.voice.closed)
        replacement = self.open()
        self.door.request("/api/session/stop", self.tokens[self.alpha], {"session_id": ctx.ident})
        self.assertIsNone(self.door.active)
        self.assertEqual(self.read(replacement)["data"]["names"], ["alpha"])
        self.assertEqual(self.door.state(ctx.binding)["mail_mode"], "disabled")

    def test_workspace_idle_ttl_uses_injected_clock_and_state_polls_do_not_refresh_it(self):
        workspace = self.open()
        self.clock[0] = 899
        self.read(workspace)
        self.clock[0] = 1798
        self.door.request("/api/state", self.tokens[self.alpha], {})
        self.clock[0] = 1799
        self.assert_error(409, lambda: self.read(workspace), "stale_state")
        replacement = self.open()
        self.assertEqual(self.read(replacement)["data"]["names"], ["alpha"])

    def test_restart_invalidates_workspace_without_replaying_reads(self):
        workspace = self.open()
        self.read(workspace)
        before = list(self.core.calls)
        self.restart()
        self.assert_error(409, lambda: self.read(workspace), "stale_state")
        self.assertFalse(any(command in ("ships", "status", "ship_runtime")
                             for _, command in self.core.calls[len(before):]))
        self.assertEqual(self.read(self.open())["data"]["names"], ["alpha"])

    def test_fresh_capability_fence_on_each_read_rejects_changed_full_snapshot(self):
        for change in ("remove_scope", "add_scope", "ships", "resource", "revoked"):
            with self.subTest(change=change):
                self.core.scopes[self.alpha] = ["fleet:read"]
                self.core.ships[self.alpha] = ["alpha"]
                self.core.resource = "door:alpha"
                self.core.denied.clear()
                workspace = self.open()
                before = len(self.core.calls)
                if change == "remove_scope":
                    self.core.scopes[self.alpha] = []
                elif change == "add_scope":
                    self.core.scopes[self.alpha].append("mail:read")
                elif change == "ships":
                    self.core.ships[self.alpha] = []
                elif change == "resource":
                    self.core.resource = "door:wrong"
                else:
                    self.core.denied.add(self.alpha)
                self.assert_error(403, lambda: self.read(workspace), "access_denied")
                self.assertTrue(any(command == "capabilities" for _, command in self.core.calls[before:]))

    def test_inflight_reads_drop_after_close_replacement_repair_rotation_or_restriction(self):
        for change in ("close", "replace", "repair", "rotate", "restrict", "expand", "resource"):
            with self.subTest(change=change):
                self.core.scopes[self.alpha] = ["fleet:read"]
                self.core.ships[self.alpha] = ["alpha"]
                self.core.resource = "door:alpha"
                # Rotation requires explicitly pairing again before a new workspace.
                if not self.door.check_token(self.tokens[self.alpha]):
                    self.tokens[self.alpha] = self.door.pair({"code": self.code(self.alpha)})["token"]
                workspace = self.open()
                entered, release = threading.Event(), threading.Event()
                self.core.barrier = entered, release
                worker, result = self.background_read(workspace)
                try:
                    self.assertTrue(entered.wait(5))
                    if change == "close":
                        self.request("close", {"workspace_id": workspace["workspace_id"]})
                    elif change == "replace":
                        self.open()
                    elif change == "repair":
                        self.tokens[self.alpha] = self.door.pair({"code": self.code(self.alpha)})["token"]
                    elif change == "rotate":
                        access.private_write(self.cfg["devices"][0]["credential_file"], "pv_local_rotated_secret")
                    elif change == "restrict":
                        self.core.ships[self.alpha] = []
                    elif change == "expand":
                        self.core.scopes[self.alpha].append("mail:read")
                    else:
                        self.core.resource = "door:wrong"
                finally:
                    self.finish(worker, release)
                    self.core.barrier = None
                self.assert_no_release(result)
                self.assertIsNone(self.door.active)

    def test_failed_runtime_read_also_fences_late_results(self):
        workspace = self.open()
        entered, release = threading.Event(), threading.Event()
        original = self.core.call
        def failed(binding, command, args=None, **kwargs):
            if command == "status":
                entered.set()
                if not release.wait(5):
                    raise AssertionError("barrier not released")
                raise web.ApiError(503, "unavailable", "late-diagnostic-canary")
            return original(binding, command, args, **kwargs)
        with mock.patch.object(self.core, "call", side_effect=failed):
            worker, result = self.background_read(workspace, "runtime")
            try:
                self.assertTrue(entered.wait(5))
                self.core.ships[self.alpha] = []
            finally:
                self.finish(worker, release)
        self.assert_no_release(result, 403)

    def test_post_capability_owner_check_drops_close_during_release_fence(self):
        workspace = self.open()
        entered, release = threading.Event(), threading.Event()
        original = self.core.call
        def blocked(binding, command, args=None, **kwargs):
            result = original(binding, command, args, **kwargs)
            if command == "capabilities":
                entered.set()
                if not release.wait(5):
                    raise AssertionError("barrier not released")
            return result
        with mock.patch.object(self.core, "call", side_effect=blocked):
            worker, result = self.background_read(workspace)
            try:
                self.assertTrue(entered.wait(5))
                self.request("close", {"workspace_id": workspace["workspace_id"]})
            finally:
                self.finish(worker, release)
        self.assert_no_release(result, 409)

    def test_per_device_inflight_bound_survives_replacement_and_other_device_can_read(self):
        self.core.scopes[self.beta] = ["fleet:read"]
        self.core.ships[self.beta] = ["alpha"]
        workspace, beta = self.open(), self.open(self.beta)
        entered, release = threading.Event(), threading.Event()
        original = self.core.call
        def blocked(binding, command, args=None, **kwargs):
            if binding and binding.device == self.alpha and command == "ships":
                entered.set()
                if not release.wait(5):
                    raise AssertionError("barrier not released")
            return original(binding, command, args, **kwargs)
        with mock.patch.object(self.core, "call", side_effect=blocked):
            worker, result = self.background_read(workspace)
            try:
                self.assertTrue(entered.wait(5))
                self.assert_error(409, lambda: self.read(workspace), "workspace_busy")
                replacement = self.open()
                self.assert_error(409, lambda: self.read(replacement), "workspace_busy")
                self.assertEqual(self.read(beta, device=self.beta)["data"]["names"], ["alpha"])
            finally:
                self.finish(worker, release)
        self.assert_no_release(result, 409)
        self.assertEqual(self.read(replacement)["data"]["names"], ["alpha"])

    def test_all_sixteen_configured_devices_have_independent_workspace_capacity(self):
        self.door.close()
        devices = [self.alpha, self.beta]
        for number in range(3, 17):
            device, grant = str(uuid.UUID(int=number)), str(uuid.UUID(int=100 + number))
            folder = os.path.join(self.root, "device" + str(number))
            os.mkdir(folder, 0o700)
            key = os.path.join(folder, "core-key")
            access.private_write(key, "pv_local_device_" + str(number) + "_secret")
            self.cfg["devices"].append(dict(id=device, label="device" + str(number),
                                             grant=grant, credential_file=key))
            devices.append(device)
        for device in devices:
            self.core.scopes[device], self.core.ships[device] = ["fleet:read"], ["alpha"]
        self.restart()
        workspaces = {}
        for device in devices:
            self.tokens[device] = self.door.pair({"code": self.code(device)})["token"]
            workspaces[device] = self.open(device)
        for device, workspace in workspaces.items():
            self.assertEqual(self.read(workspace, device=device)["device_id"], device)
        self.assertIsNone(self.door.active)
        self.assertEqual(self.core.envelopes, [])

    def test_real_http_guards_routes_and_offline_reads(self):
        wire = self.server()
        status, version, _ = wire.http("GET", "/api/version")
        self.assertEqual(status, 200)
        self.assertEqual(version["protocol"], 2)
        self.assertIn("workspace_reads", version["features"])
        path = "/api/workspace/open"
        before = list(self.core.calls)
        for headers, status, code in (({}, 401, "not_paired"),
                                      ({"token": self.tokens[self.alpha], "host": "attacker.invalid"}, 403, "wrong_host"),
                                      ({"token": self.tokens[self.alpha], "origin": "https://attacker.invalid"}, 403, "wrong_origin")):
            with self.subTest(headers=list(headers)):
                response = wire.http("POST", path, {}, **headers)
                self.assertEqual((response[0], response[1]["code"]), (status, code))
        self.assertEqual(self.core.calls, before)
        for action in ("open", "read", "close"):
            self.assertEqual(wire.http("GET", "/api/workspace/" + action, token=self.tokens[self.alpha])[0], 405)
        with mock.patch.object(voice, "codex_version", side_effect=AssertionError("provider probe")), \
                mock.patch.object(voice, "VoiceSession", side_effect=AssertionError("provider launch")):
            status, workspace, _ = wire.http("POST", path, {}, token=self.tokens[self.alpha])
            self.assertEqual(status, 200, workspace)
            data = dict(workspace_id=workspace["workspace_id"], view="runtime")
            status, page, _ = wire.http("POST", "/api/workspace/read", data, token=self.tokens[self.alpha])
            self.assertEqual(status, 200, page)
            self.assertTrue(page["data"]["available"])
            self.assertIn(scoped_runtime.MAIL, page["data"]["text"])
            self.assertEqual(wire.http("POST", "/api/workspace/read", data, token=self.tokens[self.beta])[0], 409)
            self.assertEqual(wire.http("POST", "/api/workspace/close", {"workspace_id": workspace["workspace_id"]},
                                       token=self.tokens[self.alpha])[0], 200)
            self.assertEqual(wire.http("POST", "/api/workspace/read", data, token=self.tokens[self.alpha])[0], 409)

    def test_trusted_v1_does_not_advertise_or_admit_workspace_routes(self):
        class Legacy:
            protocol = 1
        legacy = os.path.join(self.root, "legacy")
        os.mkdir(legacy, 0o700)
        pairing = web.Pairing(legacy)
        status, token = pairing.pair(pairing.code)
        self.assertEqual(status, 200)
        wire = self.server(pairing, Legacy())
        version = wire.http("GET", "/api/version")[1]
        self.assertNotIn("workspace_reads", version["features"])
        for action in ("open", "read", "close"):
            with self.subTest(action=action):
                self.assertEqual(wire.http("POST", "/api/workspace/" + action, {})[0], 404)
                self.assertEqual(wire.http("POST", "/api/workspace/" + action, {}, token=token)[0], 404)

    def test_real_adapter_starts_and_reads_without_installed_codex(self):
        from live_voice_door_test import free_port
        state = pathlib.Path(self.root, "offline-process-state")
        state.mkdir(mode=0o700)
        empty_bin = pathlib.Path(self.root, "empty-bin")
        empty_bin.mkdir()
        config = pathlib.Path(self.root, "offline-process.toml")
        row = self.cfg["devices"][0]
        fields = dict(access_mode="scoped", port=free_port(),
                      codex=[str(empty_bin / "codex-does-not-exist")])
        contents = "".join(f"{key} = {json.dumps(value)}\n" for key, value in fields.items())
        contents += "\n[[devices]]\n" + "".join(f"{key} = {json.dumps(value)}\n" for key, value in row.items())
        access.private_write(str(config), contents)
        log = pathlib.Path(self.root, "offline-process.log")
        errors, requests = [], []
        with log.open("w") as stderr:
            proc = subprocess.Popen([sys.executable, "-B", str(self.fixture.PLUGIN / "door.py")],
                cwd=self.fixture.PLUGIN, env={**os.environ, "PATH": str(empty_bin)},
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True)
            def close():
                if not proc.stdin.closed:
                    proc.stdin.close()
                try:
                    proc.wait(10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(5)
                proc.stdout.close()
            self.addCleanup(close)
            def write(value):
                proc.stdin.write(json.dumps(value) + "\n")
                proc.stdin.flush()
            def runtime():
                try:
                    for line in proc.stdout:
                        request = json.loads(line)
                        requests.append(request)
                        command = request["command"]
                        if request["type"] != "request":
                            raise AssertionError("unexpected adapter output")
                        if command == "protocol":
                            result = {"version": 2}
                        elif command == "capabilities":
                            result = dict(scopes=["fleet:read"], ships=["alpha"],
                                provenance=dict(client=row["grant"], grant=row["grant"], resource="door:alpha"))
                        elif command == "ships":
                            result = self.fixture.projection()
                        elif command == "status":
                            result = self.fixture.runtime_projection()
                        else:
                            raise AssertionError("unexpected core operation: " + command)
                        write(dict(type="response", id=request["id"], ok=True, result=result))
                except BaseException as error:
                    errors.append(error)
            worker = threading.Thread(target=runtime, daemon=True)
            worker.start()
            write(dict(type="hello", version=2, door="alpha", state_dir=str(state), config_path=str(config)))
            url = state / "url"
            self.fixture.wait_for("scoped adapter startup", lambda: url.exists() or proc.poll() is not None)
            self.assertIsNone(proc.poll(), log.read_text())
            wire = self.fixture.Wire.__new__(self.fixture.Wire)
            wire.port = int(url.read_text().strip().rstrip("/").rsplit(":", 1)[1])
            self.assertIn("workspace_reads", wire.http("GET", "/api/version")[1]["features"])
            code = (state / "scoped-devices" / self.alpha / "pairing-code").read_text().strip()
            status, paired, _ = wire.http("POST", "/api/pair", {"code": code})
            self.assertEqual(status, 200, paired)
            token = paired["token"]
            status, workspace, _ = wire.http("POST", "/api/workspace/open", {}, token=token)
            self.assertEqual(status, 200, workspace)
            data = dict(workspace_id=workspace["workspace_id"], view="directory")
            status, page, _ = wire.http("POST", "/api/workspace/read", data, token=token)
            self.assertEqual(status, 200, page)
            self.assertEqual(page["data"]["names"], ["alpha"])
            self.assertFalse((state / "app-server.pid").exists())
            status, failure, _ = wire.http("POST", "/api/session/start", {"offer": self.fixture.OFFER}, token=token)
            self.assertEqual((status, failure["code"]), (502, "codex_version"))
            self.assertNotIn("codex-does-not-exist", failure["error"])
            data["view"] = "runtime"
            status, page, _ = wire.http("POST", "/api/workspace/read", data, token=token)
            self.assertEqual(status, 200, page)
            self.assertTrue(page["data"]["available"])
            self.assertFalse((state / "app-server.pid").exists())
            close()
            worker.join(5)
            self.assertFalse(worker.is_alive())
            self.assertEqual(proc.returncode, 0, log.read_text())
            self.assertEqual(errors, [])
            self.assertEqual({request["command"] for request in requests},
                             {"protocol", "capabilities", "ships", "status"})


if __name__ == "__main__":
    unittest.main()
