#!/usr/bin/env python3
"""One Muse MSP v1 turn in an owned stdio host, with exact durable resume."""
import argparse
import json
import queue
import secrets
import sys
import time
import uuid
from pathlib import Path

from acp import Client as StdioClient, ProtocolError, emit


def command_id():
    # MSP requires UUIDv7; retain Python versions without uuid.uuid7.
    return str(uuid.UUID(int=(int(time.time() * 1000) << 80) | (7 << 76)
                         | (secrets.randbits(12) << 64) | (2 << 62) | secrets.randbits(62)))


def exact_id(value):
    if not isinstance(value, str) or str(uuid.UUID(value)) != value.lower():
        raise ProtocolError("invalid-identity")
    return value


class Client(StdioClient):
    def __init__(self, request):
        self.startup = []
        self.approvals = []
        self.completion = None
        self.turn = None
        self.starting = False
        self.started_command = None
        self.started_turn = None
        super().__init__("muse", request["cwd"], request.get("timeout", 900),
                         request["permissions"], argv=request["argv"])

    def next_event(self):
        try:
            value = self.incoming.get(timeout=self.remaining(self.end))
        except queue.Empty as error:
            raise ProtocolError("turn-deadline") from error
        if not isinstance(value, dict) or value.get("jsonrpc") != "2.0" or "method" not in value:
            raise ProtocolError("invalid-turn-event")
        self.inbound(value, self.end)

    def inbound(self, value, deadline):
        method, params = value.get("method"), value.get("params", {})
        if not isinstance(method, str) or not isinstance(params, dict):
            raise ProtocolError("invalid-notification")
        if "id" not in value and method in ("usage/changed", "skill/changed", "session/listChanged"):
            return
        if self.session is None:
            if self.creating and "id" not in value and method == "session/started":
                if len(self.startup) >= 8:
                    raise ProtocolError("startup-limit")
                self.startup.append(value)
                return
            raise ProtocolError("unexpected-startup-event")
        if method == "session/started":
            if params.get("session", {}).get("sessionId") != self.session:
                raise ProtocolError("startup-identity")
        elif params.get("sessionId") != self.session:
            raise ProtocolError("wrong-session")
        if "id" in value:
            if method != "approval/request":
                self.send({"jsonrpc": "2.0", "id": value["id"],
                           "error": {"code": -32601, "message": "Client interaction unavailable"}}, deadline)
                raise ProtocolError("client-interaction-unavailable")
            # A receipt acknowledges presentation, never a decision. Decisions
            # are separate commands after the current RPC has been correlated.
            self.send({"jsonrpc": "2.0", "id": value["id"], "result": {}}, deadline)
            if len(self.approvals) >= 8:
                raise ProtocolError("approval-limit")
            self.approvals.append(params)
        elif method == "turn/completed":
            if not self.starting or self.completion is not None:
                raise ProtocolError("unexpected-completion")
            exact_id(params.get("turnId"))
            if self.turn is not None and params["turnId"] != self.turn:
                raise ProtocolError("wrong-turn")
            self.completion = params
        elif method == "turn/started":
            if not self.starting or params.get("commandId") != self.started_command:
                raise ProtocolError("unexpected-turn")
            exact_id(params.get("turnId"))
            if self.turn is not None and params["turnId"] != self.turn:
                raise ProtocolError("wrong-turn")
            self.started_turn = params["turnId"]
        elif method in ("view/gap", "session/closed", "session/modelRouteUnserved"):
            raise ProtocolError("native-state-unavailable")
        # Preserve bounded native session events, nesting usage so cumulative
        # counters and streaming snapshots cannot be summed as extra turns.
        emit("msp.update", method=method, params=params)

    def command(self, method, params):
        identity = command_id()
        result = self.request(method, {"commandId": identity, **params})
        if result.get("commandId") != identity or result.get("status") != "accepted":
            raise ProtocolError("command-unconfirmed")
        return result

    def decide(self):
        # A native approval stage can change while displayed. Send the exact
        # offered requirement reference; stale decisions fail, never retry.
        while self.approvals:
            params = self.approvals.pop(0)
            if params.get("turnId") != self.turn or params.get("subagentOrigin") is not None:
                raise ProtocolError("approval-turn-identity")
            options = params.get("availableChoices")
            requirement = params.get("currentRequirementId")
            if (not isinstance(options, list) or len(options) > 32
                    or not isinstance(params.get("approvalId"), str)
                    or not isinstance(requirement, dict)
                    or requirement.get("approvalId") != params["approvalId"]
                    or type(requirement.get("sourceIndex")) is not int):
                raise ProtocolError("invalid-approval")
            approved = self.permissions == "allow_once"
            choice = next((o for o in options if isinstance(o, dict) and o.get("scope") == "once"
                           and o.get("decision") == ("approved" if approved else "denied")
                           and isinstance(o.get("choiceId"), str)), None)
            if choice is None:
                raise ProtocolError("one-time-choice-unavailable")
            self.denied |= not approved
            self.command("approval/decide", {"sessionId": self.session, "approvalId": params["approvalId"],
                                           "requirementId": requirement, "choiceId": choice["choiceId"]})
            emit("msp.permission", session_id=self.session, allowed=approved)


def run(request):
    if not isinstance(request, dict) or request.get("permissions") not in ("deny", "allow_once"):
        raise ProtocolError("invalid-launch")
    argv = request.get("argv")
    if not isinstance(argv, list) or argv[:2] != ["muse", "serve"] or not all(isinstance(v, str) for v in argv):
        raise ProtocolError("invalid-host")
    if not isinstance(request.get("prompt"), str) or not 0 < len(request["prompt"]) <= 32768:
        raise ProtocolError("invalid-prompt")
    cwd = Path(request["cwd"])
    roots = request.get("roots")
    if (not cwd.is_absolute() or not cwd.is_dir() or not isinstance(roots, list)
            or not 0 < len(roots) <= 32 or roots[0] != str(cwd)
            or any(not isinstance(p, str) or not Path(p).is_absolute() or not Path(p).is_dir() for p in roots)):
        raise ProtocolError("invalid-workspace")
    resume = request.get("resume")
    if resume:
        exact_id(resume)
    client = Client(request)
    try:
        initialized = client.request("initialize", {"clientInfo": {"name": "privateer", "version": "1"},
                                                   "capabilities": {"userInputDialogs": False}})
        if initialized.get("schema", {}).get("version") != 1 or initialized.get("sessionDurability", "durable") != "durable":
            raise ProtocolError("unsupported-protocol-or-durability")
        client.send({"jsonrpc": "2.0", "method": "initialized"}, client.end)
        if resume:
            client.session = resume
            result = client.request("session/resume", {"commandId": command_id(), "sessionId": resume, "excludeItems": True})
        else:
            client.creating = True
            setup = {"commandId": command_id(), "workspaceRoots": roots}
            if request.get("model"):
                setup["modelId"] = request["model"]
            if request.get("provider"):
                setup["providerId"] = request["provider"]
            if request.get("approval_mode"):
                setup["approvalMode"] = request["approval_mode"]
            result = client.request("session/start", setup)
            client.creating = False
        state = result.get("session", {})
        identity = exact_id(state.get("sessionId"))
        if ((resume and identity != resume) or state.get("status") != "idle"
                or state.get("activeTurnId") is not None or result.get("pendingRequests", [])):
            raise ProtocolError("session-unavailable")
        if state.get("workspaceRoot") != request["cwd"]:
            raise ProtocolError("resume-workspace-mismatch")
        client.session = identity
        for value in client.startup:
            client.inbound(value, client.end)
        client.startup.clear()
        # Resume restores native settings. Explicit choices must be reselected
        # and confirmed, not silently overridden by the stored conversation.
        if resume and request.get("model"):
            model = {"modelId": request["model"]}
            if request.get("provider"):
                model["providerId"] = request["provider"]
            client.command("session/setModel", {"sessionId": identity, "model": model})
            state = client.request("session/read", {"sessionId": identity, "excludeItems": True})["session"]
        if request.get("model") and state.get("modelId") != request["model"]:
            raise ProtocolError("model-unconfirmed")
        if request.get("provider") and state.get("providerId") != request["provider"]:
            raise ProtocolError("provider-unconfirmed")
        if resume and request.get("approval_mode"):
            changed = client.command("session/setApprovalMode", {"sessionId": identity, "mode": request["approval_mode"]})
            state["approvalMode"] = changed.get("effectiveMode")
        if request.get("approval_mode") and (state.get("approvalMode") or {}).get("mode") != request["approval_mode"]:
            raise ProtocolError("approval-mode-unconfirmed")
        emit("session.started", harness="muse", session_id=identity, source="msp.resume" if resume else "msp.start")
        client.started_command = command_id()
        client.starting = True
        params = {"commandId": client.started_command, "sessionId": identity,
                  "workspaceRoots": roots, "input": [{"type": "text", "text": request["prompt"]}]}
        if request.get("effort"):
            params["reasoningEffort"] = request["effort"]
        accepted = client.request("turn/start", params)
        if (accepted.get("commandId") != client.started_command or accepted.get("status") != "accepted"
                or accepted.get("disposition") != "started" or accepted.get("startedNewTurn") is not True):
            raise ProtocolError("turn-not-started")
        client.turn = exact_id(accepted.get("turnId"))
        if client.started_turn is not None and client.started_turn != client.turn:
            raise ProtocolError("wrong-started-turn")
        if client.completion and client.completion["turnId"] != client.turn:
            raise ProtocolError("wrong-completed-turn")
        while client.completion is None or client.approvals:
            client.decide()
            if client.completion is None:
                client.next_event()
        completed = client.completion
        if completed["terminal"] != "completed" or client.denied:
            raise ProtocolError("turn-not-completed")
        fields = {"session_id": identity, "turn_id": client.turn, "stop_reason": "completed"}
        if isinstance(completed.get("usage"), dict):
            fields["usage"] = completed["usage"]
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
        emit("turn.failed", reason="Native MSP turn unavailable, refused, interrupted or unconfirmed; inspect local setup.")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
