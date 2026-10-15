#!/usr/bin/env python3
"""One native ACP turn in a supervised job; no fleet state or credentials here."""
import argparse
import json
import math
import os
import queue
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

from native_launch import native_environment

MAX_LINE = 1024 * 1024


class ProtocolError(Exception):
    pass


def emit(kind, **fields):
    print(json.dumps({"type": kind, **fields}, separators=(",", ":")), flush=True)


def select_model(client, session, model):
    """Use advertised v1 configuration; never silently keep a default model."""
    configs = session.get("configOptions", [])
    if not isinstance(configs, list):
        raise ProtocolError("invalid-model-options")
    selected = next((option for option in configs if isinstance(option, dict)
                     and (option.get("category") == "model" or option.get("id") == "model")
                     and option.get("type") == "select"), None)
    if selected is not None:
        values = selected.get("options", [])
        if not isinstance(values, list):
            raise ProtocolError("invalid-model-options")
        flattened = []
        for option in values:
            if not isinstance(option, dict):
                raise ProtocolError("invalid-model-options")
            group = option.get("options", []) if "group" in option else [option]
            if not isinstance(group, list):
                raise ProtocolError("invalid-model-options")
            flattened.extend(group)
        if not isinstance(selected.get("id"), str) or not any(isinstance(v, dict) and v.get("value") == model for v in flattened):
            raise ProtocolError("model-unavailable")
        result = client.request("session/set_config_option", {"sessionId": client.session,
                                "configId": selected["id"], "value": model})
        returned = result.get("configOptions", [])
        if not isinstance(returned, list) or not any(isinstance(o, dict) and o.get("id") == selected["id"]
                                                    and o.get("currentValue") == model for o in returned):
            raise ProtocolError("model-unconfirmed")
        return
    # Older ACP v1 servers expose the unstable models field instead. Only
    # invoke its setter when that exact requested model was advertised.
    models = session.get("models", {})
    available = models.get("availableModels", []) if isinstance(models, dict) else []
    if not isinstance(available, list) or not any(isinstance(m, dict) and m.get("modelId") == model for m in available):
        raise ProtocolError("model-unavailable")
    client.request("session/set_model", {"sessionId": client.session, "modelId": model})


class Client:
    def __init__(self, binary, cwd, timeout, permissions, *, argv=None):
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or not 0 < timeout <= 3600:
            raise ProtocolError("invalid-deadline")
        self.end = time.monotonic() + timeout
        self.incoming = queue.Queue(8)
        self.outgoing = queue.Queue(1)
        # Inherit the owning job's process group. The runtime remains the sole
        # deadline/cancellation owner, including any native tool descendants.
        self.child = subprocess.Popen(argv if argv is not None else [binary, "acp"], cwd=cwd, env=native_environment(),
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, bufsize=0)
        self.permissions = permissions
        self.session = None
        self.creating = False
        self.pending_updates = []
        self.denied = False
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._write, daemon=True).start()
        threading.Thread(target=self._drain_errors, daemon=True).start()

    def _read(self):
        buffer = bytearray()
        try:
            while chunk := os.read(self.child.stdout.fileno(), 4096):
                buffer.extend(chunk)
                while b"\n" in buffer:
                    line, _, buffer = buffer.partition(b"\n")
                    if len(line) > MAX_LINE:
                        raise ProtocolError()
                    if line.strip():
                        self.incoming.put(json.loads(line))
                if len(buffer) > MAX_LINE:
                    raise ProtocolError()
        except Exception:
            self.incoming.put(ProtocolError())
        finally:
            self.incoming.put(None)

    def _drain_errors(self):
        # Diagnostics may contain account details. Drain without exporting them;
        # ACP errors are reported by code rather than copying vendor messages.
        try:
            while os.read(self.child.stderr.fileno(), 4096):
                pass
        except OSError:
            pass

    def _write(self):
        while True:
            raw, acknowledgement = self.outgoing.get()
            try:
                while raw:
                    written = os.write(self.child.stdin.fileno(), raw)
                    if written <= 0:
                        raise ProtocolError()
                    raw = raw[written:]
                acknowledgement.put(True)
            except Exception:
                acknowledgement.put(False)

    def remaining(self, deadline):
        remaining = min(deadline, self.end) - time.monotonic()
        if remaining <= 0:
            raise ProtocolError("deadline")
        return remaining

    def send(self, value, deadline):
        raw = (json.dumps(value, separators=(",", ":")) + "\n").encode()
        if len(raw) > MAX_LINE:
            raise ProtocolError("request-size")
        acknowledgement = queue.Queue(1)
        try:
            self.outgoing.put((raw, acknowledgement), timeout=self.remaining(deadline))
            if not acknowledgement.get(timeout=self.remaining(deadline)):
                raise ProtocolError("write")
        except queue.Empty as error:
            raise ProtocolError("write-deadline") from error
        except queue.Full as error:
            raise ProtocolError("write-deadline") from error

    def request(self, method, params, seconds=30):
        request_id = str(uuid.uuid4())
        deadline = min(self.end, time.monotonic() + seconds)
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}, deadline)
        while True:
            try:
                value = self.incoming.get(timeout=self.remaining(deadline))
            except queue.Empty as error:
                raise ProtocolError("response-deadline") from error
            if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
                raise ProtocolError("invalid-response")
            if "method" in value:
                self.inbound(value, deadline)
                continue
            if value.get("id") != request_id or ("result" in value) == ("error" in value):
                raise ProtocolError("response-identity")
            if "error" in value:
                raise ProtocolError("native-request-refused")
            if not isinstance(value["result"], dict):
                raise ProtocolError("invalid-result")
            return value["result"]

    def inbound(self, value, deadline):
        method, params = value["method"], value.get("params", {})
        if not isinstance(params, dict):
            raise ProtocolError("invalid-notification")
        if method == "session/update":
            if "id" in value:
                raise ProtocolError("invalid-notification")
            if self.session is None and self.creating:
                if len(self.pending_updates) >= 8:
                    raise ProtocolError("startup-update-limit")
                self.pending_updates.append(value)
                return
            if self.session is None or params.get("sessionId") != self.session:
                raise ProtocolError("wrong-session")
            update = params.get("update")
            if not isinstance(update, dict):
                raise ProtocolError("invalid-update")
            emit("acp.update", session_id=self.session, update=update)
        elif "id" in value:
            response = {"jsonrpc": "2.0", "id": value["id"]}
            if method == "session/request_permission":
                if self.session is None or params.get("sessionId") != self.session:
                    raise ProtocolError("wrong-session")
                options = params.get("options", [])
                if not isinstance(options, list) or len(options) > 32:
                    raise ProtocolError("invalid-permission-options")
                once = next((o for o in options if isinstance(o, dict) and o.get("kind") == "allow_once"
                             and isinstance(o.get("optionId"), str)), None)
                allowed = self.permissions == "auto" and once is not None
                if allowed:
                    response["result"] = {"outcome": {"outcome": "selected", "optionId": once["optionId"]}}
                else:
                    self.denied = True
                    response["result"] = {"outcome": {"outcome": "cancelled"}}
                emit("acp.permission", session_id=self.session, allowed=allowed)
            else:
                # Filesystem and terminal delegation were not advertised. Do
                # not turn an unexpected server request into a shell command.
                response["error"] = {"code": -32601, "message": "Client method unavailable"}
            self.send(response, deadline)

    def close(self):
        # Closing a pipe cannot depend on the native server still reading it.
        # Raw FileIO avoids a buffered-writer lock during timeout cleanup.
        try:
            self.child.stdin.close()
        except OSError:
            pass
        try:
            self.child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait(timeout=2)


def run(request):
    if not isinstance(request, dict) or request.get("binary") not in ("rovo", "kimi", "devin") or request.get("permissions") not in ("ask", "auto"):
        raise ProtocolError("invalid-launch")
    if not isinstance(request.get("prompt"), str) or not 0 < len(request["prompt"]) <= 32768:
        raise ProtocolError("invalid-prompt")
    cwd = Path(request["cwd"])
    if not cwd.is_absolute() or not cwd.is_dir():
        raise ProtocolError("invalid-directory")
    client = Client(request["binary"], request["cwd"], request.get("timeout", 900), request["permissions"])
    try:
        initialized = client.request("initialize", {"protocolVersion": 1, "clientInfo": {"name": "privateer", "version": "1"},
                                                    "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False}})
        if initialized.get("protocolVersion") != 1:
            raise ProtocolError("unsupported-protocol")
        resume = request.get("resume")
        setup = {"cwd": request["cwd"], "mcpServers": []}
        if resume:
            if initialized.get("agentCapabilities", {}).get("loadSession") is not True:
                raise ProtocolError("resume-unavailable")
            # History updates may arrive while loading. They remain attached
            # to this explicit session and can never redirect the next prompt.
            client.session = resume
            result = client.request("session/load", {**setup, "sessionId": resume})
            if result.get("sessionId", resume) != resume:
                raise ProtocolError("resume-identity")
        else:
            client.creating = True
            result = client.request("session/new", setup)
            client.session = result.get("sessionId")
            client.creating = False
        if not isinstance(client.session, str) or not 0 < len(client.session) <= 512 or any(c.isspace() or ord(c) < 32 for c in client.session):
            raise ProtocolError("invalid-session")
        for update in client.pending_updates:
            client.inbound(update, min(client.end, time.monotonic() + 5))
        client.pending_updates.clear()
        emit("session.started", harness=request["binary"], session_id=client.session, source="acp.load" if resume else "acp.new")
        if request.get("model"):
            select_model(client, result, request["model"])
        completed = client.request("session/prompt", {"sessionId": client.session,
                                                      "prompt": [{"type": "text", "text": request["prompt"]}]}, seconds=request.get("timeout", 900))
        if completed.get("stopReason") != "end_turn" or client.denied:
            raise ProtocolError("turn-not-completed")
        emit("turn.completed", session_id=client.session, stop_reason=completed["stopReason"])
    finally:
        client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--request", required=True)
    args = parser.parse_args()
    try:
        run(json.loads(args.request))
        return 0
    except (Exception, KeyboardInterrupt):
        emit("turn.failed", reason="Native ACP turn unavailable, refused, interrupted or unconfirmed; inspect local setup.")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
