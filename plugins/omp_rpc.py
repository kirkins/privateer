#!/usr/bin/env python3
"""One OMP RPC assignment, verified before admission and after quiescence."""
import argparse
import json
import queue
import sys
import time
import uuid
from pathlib import Path

from acp import Client as StdioClient, ProtocolError, emit
from native_turn import Turn


def exact_id(value):
    if not isinstance(value, str) or str(uuid.UUID(value)) != value.lower():
        raise ProtocolError("invalid-session")
    return value


def verify_state(state, request, identity=None):
    if not isinstance(state, dict):
        raise ProtocolError("invalid-state")
    selected = exact_id(state.get("sessionId"))
    if identity is not None and selected != identity:
        raise ProtocolError("session-changed")
    if (state.get("isSettled") is not True or state.get("isStreaming") is not False
            or state.get("isCompacting") is not False or state.get("hasPendingAsyncWork") is not False
            or state.get("queuedMessageCount") != 0 or state.get("goal") is not None):
        raise ProtocolError("session-not-settled")
    model = state.get("model") or {}
    configured = request.get("model")
    provider = request.get("provider")
    if configured:
        if "/" in configured:
            expected_provider, configured = configured.split("/", 1)
            if model.get("provider") != expected_provider:
                raise ProtocolError("model-provider-unconfirmed")
        if model.get("id") != configured:
            raise ProtocolError("model-unconfirmed")
    if provider and model.get("provider") != provider:
        raise ProtocolError("provider-unconfirmed")
    if request.get("effort") and state.get("thinkingLevel") != request["effort"]:
        raise ProtocolError("effort-unconfirmed")
    return selected


def resume_identity(request):
    selected = request.get("resume")
    if not selected:
        return None
    if Path(selected).is_absolute():
        return read_header(Path(selected), request["cwd"])
    return exact_id(selected)


def read_header(path, cwd):
    if not path.is_absolute() or not path.is_file():
        raise ProtocolError("session-file-unavailable")
    with path.open("rb") as stream:
        line = stream.readline(65537)
        if len(line) > 65536 or not line.endswith(b"\n"):
            raise ProtocolError("session-header-size")
        header = json.loads(line)
        # OMP 18.8.9 prepends a fixed title slot to its session header. Accept
        # that one native prefix, never scan arbitrary transcript records.
        if isinstance(header, dict) and header.get("type") == "title":
            line = stream.readline(65537)
            if len(line) > 65536 or not line.endswith(b"\n"):
                raise ProtocolError("session-header-size")
            header = json.loads(line)
    if (not isinstance(header, dict) or header.get("type") != "session"
            or not isinstance(header.get("cwd"), str)
            or not Path(header["cwd"]).is_absolute()
            or Path(header["cwd"]).resolve() != Path(cwd).resolve()):
        raise ProtocolError("resume-workspace-mismatch")
    return exact_id(header.get("id"))


class Client(StdioClient):
    def __init__(self, request):
        self.prompt_id = None
        self.prompt_result = None
        self.admitted = False
        self.quiet = False
        self.turn = Turn()
        super().__init__("omp", request["cwd"], request.get("timeout", 900), "deny", argv=request["argv"])

    def receive(self, deadline):
        try:
            value = self.incoming.get(timeout=self.remaining(deadline))
        except queue.Empty as error:
            raise ProtocolError("response-deadline") from error
        if not isinstance(value, dict) or not isinstance(value.get("type"), str):
            raise ProtocolError("invalid-frame")
        return value

    def request(self, method, params=None, seconds=30, identity=None):
        identity = identity or str(uuid.uuid4())
        deadline = min(self.end, time.monotonic() + seconds)
        self.send({"id": identity, "type": method, **(params or {})}, deadline)
        while True:
            value = self.receive(deadline)
            if value["type"] != "response":
                self.event(value)
                continue
            if value.get("id") != identity or value.get("command") != method:
                raise ProtocolError("response-identity")
            if value.get("success") is not True:
                raise ProtocolError("native-command-refused")
            result = value.get("data", {})
            if not isinstance(result, dict):
                raise ProtocolError("invalid-result")
            return result

    def event(self, value):
        kind = value["type"]
        if kind in ("available_commands_update", "advisor_cost_changed"):
            return
        if kind == "prompt_result":
            if (not self.admitted or self.prompt_id is None or value.get("id") != self.prompt_id
                    or self.prompt_result is not None or value.get("agentInvoked") is not True
                    or value.get("status") != "completed"
                    or not isinstance(value.get("sessionSettled"), bool)):
                raise ProtocolError("prompt-not-completed")
            self.prompt_result = value
            self.quiet = value["sessionSettled"]
        elif kind == "session_settled":
            if self.prompt_id is None or self.prompt_result is None:
                raise ProtocolError("unexpected-settle")
            self.quiet = True
        elif kind in ("rpc_chunk", "rpc_frame_error", "extension_ui_request", "host_tool_call", "host_uri_request", "extension_error"):
            # No delegated shell/fs/UI capabilities, no lossy reassembly or
            # invented extension responses. Native tools remain native.
            raise ProtocolError("unsupported-native-interaction")
        elif kind == "ready":
            raise ProtocolError("unexpected-ready")
        if kind in ("agent_start", "turn_start", "tool_execution_start", "auto_retry_start", "auto_compaction_start"):
            self.quiet = False
            self.turn.stop = None
        if kind in ("message_end", "compaction_end", "auto_compaction_end"):
            if kind == "auto_compaction_end":
                self.turn.add_usage((value.get("result") or {}).get("usage"))
            else:
                self.turn.observe(value)
                if value.get("message", {}).get("role") == "toolResult":
                    self.turn.stop = None
        if kind == "session_info_update" and value.get("sessionId", self.session) != self.session:
            raise ProtocolError("session-changed")
        # Nest native events so repeated message/agent snapshots cannot become
        # extra top-level usage or a forged runtime session/completion event.
        emit("omp.update", event=value)


def run(request):
    if not isinstance(request, dict):
        raise ProtocolError("invalid-launch")
    argv = request.get("argv")
    if (not isinstance(argv, list) or not argv or argv[0] != "omp"
            or not all(isinstance(v, str) for v in argv)
            or not any(argv[i:i+2] == ["--mode", "rpc"] for i in range(len(argv)))):
        raise ProtocolError("invalid-host")
    cwd = Path(request["cwd"])
    if not cwd.is_absolute() or not cwd.is_dir():
        raise ProtocolError("invalid-workspace")
    if not isinstance(request.get("prompt"), str) or not 0 < len(request["prompt"]) <= 32768:
        raise ProtocolError("invalid-prompt")
    expected = resume_identity(request)
    client = Client(request)
    try:
        ready = client.receive(min(client.end, time.monotonic() + 30))
        if (ready.get("type") != "ready" or ready.get("protocolVersion") != 1
                or type(ready.get("maxFrameBytes")) is not int or not 0 < ready["maxFrameBytes"] <= 1024 * 1024):
            raise ProtocolError("unsupported-protocol")
        state = client.request("get_state")
        identity = verify_state(state, request, expected)
        if expected is not None:
            path = Path(state.get("sessionFile", ""))
            if read_header(path, request["cwd"]) != identity:
                raise ProtocolError("resume-file-identity")
            if Path(request["resume"]).is_absolute() and path.resolve() != Path(request["resume"]).resolve():
                raise ProtocolError("resume-file-changed")
        elif state.get("messageCount") != 0:
            raise ProtocolError("unexpected-existing-conversation")
        client.session = identity
        if not request.get("ephemeral"):
            emit("session", id=identity, source="omp.rpc.resume" if expected else "omp.rpc.new")
        client.prompt_id = str(uuid.uuid4())
        accepted = client.request("prompt", {"message": request["prompt"]}, identity=client.prompt_id)
        if accepted.get("agentInvoked") is False:
            raise ProtocolError("prompt-did-not-invoke-agent")
        client.admitted = True
        while client.prompt_result is None or not client.quiet:
            value = client.receive(client.end)
            if value["type"] == "response":
                # A later error for the accepted prompt is a failure, not a
                # second success acknowledgement or a new request to retry.
                raise ProtocolError("unexpected-late-response")
            client.event(value)
        state = client.request("get_state")
        verify_state(state, request, identity)
        if not request.get("ephemeral") and read_header(Path(state.get("sessionFile", "")), request["cwd"]) != identity:
            raise ProtocolError("session-durability-unconfirmed")
        if not client.quiet or client.turn.stop != "stop":
            raise ProtocolError("last-native-turn-not-completed")
        fields = {"harness": "omp", "session_id": identity, "stop_reason": "stop"}
        if client.turn.usage:
            fields["usage"] = client.turn.usage
    finally:
        client.close()
    if client.child.returncode != 0:
        raise ProtocolError("host-exit-unconfirmed")
    emit("turn.completed", **fields)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--request", required=True)
    args = parser.parse_args()
    try:
        run(json.loads(args.request))
        return 0
    except (Exception, KeyboardInterrupt):
        emit("turn.failed", reason="Native OMP RPC turn unavailable, refused, interrupted or unconfirmed; inspect local setup.")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
