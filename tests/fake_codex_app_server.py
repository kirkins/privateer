#!/usr/bin/env python3
"""A fake `codex app-server` for the live voice probe tests.

Speaks the app-server's stdio JSON-RPC (no "jsonrpc" field, one JSON object
per line) for the methods the probe uses. FAKE_CODEX_SCENARIO picks the
behaviour; FAKE_CODEX_LOG names a JSON-lines file recording what the probe
sent and whether an API key reached the environment. No network.

Scenarios (the transport the probe asks for picks the realtime behaviour):
  go            ChatGPT login; over websocket realtime starts and speaks
                "ready" with audio, over webrtc it answers the SDP offer;
                exits on stdin EOF but leaves a helper child in its group
  unauthorized  ChatGPT login; thread/realtime/start answers a 401 error
  closed        ChatGPT login; realtime starts, then errors and closes
  needs-api-key ChatGPT login; the realtime start fails, before any session
                is announced, the way Codex 0.162.1 fails websocket realtime
                without an API key
  apikey        API key login; realtime must never be called
  hang          ChatGPT login; realtime starts but no audio or answer ever
                comes, and the server ignores stdin EOF and SIGTERM, with a
                helper child in its process group
"""

import base64
import json
import os
import signal
import subprocess
import sys
import time

SCENARIO = os.environ.get("FAKE_CODEX_SCENARIO", "go")
LOG = os.environ.get("FAKE_CODEX_LOG")
THREAD = "thr_fake"
# 20 ms of 24 kHz mono PCM16 silence-ish ramp per chunk.
CHUNK = bytes(range(256)) * 3 + bytes(192)
# An answer carrying a session identifier the probe must never print.
ANSWER_SESSION = "rtc_fake_session_identifier"
ANSWER = "\r\n".join([
    "v=0", f"o=- 42 2 IN IP4 0.0.0.0", f"s={ANSWER_SESSION}", "t=0 0",
    "a=group:BUNDLE 0 1",
    "m=audio 9 UDP/TLS/RTP/SAVPF 111", "c=IN IP4 0.0.0.0", "a=mid:0",
    "a=setup:active", "a=rtcp-mux", "a=rtpmap:111 opus/48000/2",
    "m=application 9 UDP/DTLS/SCTP webrtc-datachannel", "c=IN IP4 0.0.0.0",
    "a=mid:1", "a=setup:active", "a=sctp-port:5000",
]) + "\r\n"


def log(entry):
    if LOG:
        with open(LOG, "a") as out:
            out.write(json.dumps(entry) + "\n")


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def notify(method, params):
    send({"method": method, "params": params})


def result(request_id, value=None):
    send({"id": request_id, "result": {} if value is None else value})


def error(request_id, code, message):
    send({"id": request_id, "error": {"code": code, "message": message}})


def speak():
    send({"id": "srv-1", "method": "item/commandExecution/requestApproval",
          "params": {"threadId": THREAD, "turnId": "t1", "itemId": "i1"}})
    notify("turn/started", {"threadId": THREAD, "turn": {"id": "turn_fake"}})
    notify("thread/realtime/itemAdded",
           {"threadId": THREAD, "item": {"type": "response.created", "secret": "sk-notreal12345"}})
    notify("thread/realtime/transcript/delta",
           {"threadId": THREAD, "role": "assistant", "delta": "Rea"})
    for _ in range(3):
        notify("thread/realtime/outputAudio/delta", {"threadId": THREAD, "audio": {
            "data": base64.b64encode(CHUNK).decode(), "sampleRate": 24000,
            "numChannels": 1, "samplesPerChannel": len(CHUNK) // 2, "itemId": "it1"}})
    notify("thread/realtime/transcript/delta",
           {"threadId": THREAD, "role": "assistant", "delta": "dy."})
    notify("thread/realtime/transcript/done",
           {"threadId": THREAD, "role": "assistant", "text": "Ready."})


def main():
    if sys.argv[1:] == ["--version"]:
        print("codex-cli 0.0.0-fake")
        return
    assert sys.argv[1:] == ["app-server"], sys.argv
    log({"start": True, "api_key_in_env": any(
        name in os.environ for name in ("OPENAI_API_KEY", "CODEX_API_KEY"))})
    if SCENARIO == "hang":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if SCENARIO in ("go", "hang"):
        # A helper in the server's process group, left behind on exit the way
        # a voice host could be.
        helper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
        log({"helper": helper.pid})
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        log({"method": method, "id": message.get("id"),
             "error": message.get("error"), "params": message.get("params")})
        request_id = message.get("id")
        if method == "initialize":
            caps = (message["params"] or {}).get("capabilities") or {}
            if not caps.get("experimentalApi"):
                error(request_id, -32600, "experimental API not enabled")
                continue
            result(request_id, {"userAgent": "fake/0.0.0", "codexHome": "/nonexistent",
                                "platformFamily": "unix", "platformOs": "linux"})
        elif method == "account/read":
            account = ({"type": "apiKey"} if SCENARIO == "apikey" else
                       {"type": "chatgpt", "email": "someone@example.com", "planType": "plus"})
            result(request_id, {"account": account, "requiresOpenaiAuth": True,
                                "workspaceRouting": None})
        elif method == "thread/start":
            result(request_id, {"thread": {"id": THREAD}, "model": "fake"})
            notify("thread/started", {"thread": {"id": THREAD}})
        elif method == "thread/realtime/start":
            if SCENARIO == "unauthorized":
                error(request_id, -32603,
                      "realtime start failed: 401 Unauthorized no_matching_rule "
                      "Bearer eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl")
                continue
            transport = ((message["params"] or {}).get("transport") or {}).get("type")
            result(request_id)
            if SCENARIO == "needs-api-key":
                # Codex fails the start before announcing a session.
                notify("thread/realtime/error", {
                    "threadId": THREAD,
                    "message": "realtime conversation requires API key auth"})
                notify("thread/realtime/closed", {"threadId": THREAD, "reason": "error"})
                continue
            # Codex announces the session before the WebRTC answer.
            notify("thread/realtime/started",
                   {"threadId": THREAD, "realtimeSessionId": "rs1", "version": "v3"})
            if SCENARIO == "closed":
                notify("thread/realtime/error",
                       {"threadId": THREAD, "message": "401 no_matching_rule"})
                notify("thread/realtime/closed", {"threadId": THREAD, "reason": "error"})
            elif SCENARIO == "go" and transport == "webrtc":
                notify("thread/realtime/sdp", {"threadId": THREAD, "sdp": ANSWER})
        elif method in ("thread/realtime/appendText", "thread/realtime/appendSpeech"):
            result(request_id)
            if SCENARIO == "go":
                speak()
        elif method == "thread/realtime/stop":
            result(request_id)
            notify("thread/realtime/closed", {"threadId": THREAD, "reason": None})
        elif method == "turn/interrupt":
            result(request_id)
        elif request_id is not None and method is not None:
            error(request_id, -32601, f"unknown method {method}")
    if SCENARIO == "hang":
        while True:
            time.sleep(1)


if __name__ == "__main__":
    main()
