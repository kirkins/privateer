#!/usr/bin/env python3
"""A fake `codex app-server` for the live voice door's tests.

Speaks the stdio JSON-RPC the door uses: initialize, account/read,
config/read, thread/start, thread/realtime/start with a webrtc transport (answered by
thread/realtime/started and thread/realtime/sdp), appendText/appendSpeech and
thread/realtime/stop. No network and no real Codex.

Driven through the directory in FAKE_VOICE_DIR:
- scenario.json: {"account": "chatgpt"|"apiKey"|null,
                  "realtime": "ok"|"refuse"|"silent"|"closed",
                  "config_read": "ok"|"refuse",
                  "config": the effective config config/read returns,
                  "codex_version": what `--version` prints
                    ("codex-cli 0.162.1", the version the door supports),
                  "app_server_error": {"method", "code", "message"} answered for
                    that method instead, to play a protocol mismatch}
- inject/NNN.json: a list of messages written to the door as they appear
  (notifications and server requests); {"exit": true} ends the fake.
- received.jsonl: every message the door sent, in order;
- version.jsonl: one line per `--version` run;
- started.jsonl: one line per start with the pid, process group and whether
  an API key variable reached this process.

Usage: fake_live_voice_app_server.py app-server
"""

import json
import os
import sys
import threading
import time

DIR = os.environ["FAKE_VOICE_DIR"]
ANSWER = ("v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n"
          "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n"
          "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n")
# A captain's own Codex config: an MCP server and a plugin the door must turn off.
CAPTAIN_CONFIG = {
    "model": None,
    "mcp_servers": {"captain-tools": {"command": "/bin/true", "enabled": True}},
    "plugins": {"captain-plugin@market": {"enabled": True}},
    "features": {"shell_tool": True},
}
lock = threading.Lock()


def scenario() -> dict:
    try:
        with open(os.path.join(DIR, "scenario.json"), encoding="utf-8") as f:
            return json.load(f)
    except OSError:
        return {}


def out(message: dict) -> None:
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def record(name: str, message: dict) -> None:
    with lock, open(os.path.join(DIR, name), "a", encoding="utf-8") as f:
        f.write(json.dumps(message) + "\n")


def injector() -> None:
    inject = os.path.join(DIR, "inject")
    os.makedirs(inject, exist_ok=True)
    while True:
        for name in sorted(os.listdir(inject)):
            if not name.endswith(".json"):
                continue
            path = os.path.join(inject, name)
            try:
                with open(path, encoding="utf-8") as f:
                    messages = json.load(f)
            except (OSError, ValueError):
                continue
            os.unlink(path)
            for message in messages:
                if message.get("exit"):
                    os._exit(0)
                out(message)
        time.sleep(0.02)


def main() -> int:
    if sys.argv[1:] == ["--version"]:
        # The door's version check: as cheap as it can be. No session, no
        # login, no credentials, and no app-server start is recorded.
        record("version.jsonl", {"argv": sys.argv[1:]})
        sys.stdout.write(scenario().get("codex_version", "codex-cli 0.162.1") + "\n")
        sys.stdout.flush()
        return 0
    assert sys.argv[1:] == ["app-server"], sys.argv
    plan = scenario()
    record("started.jsonl", {
        "pid": os.getpid(), "pgid": os.getpgid(0), "cwd": os.getcwd(),
        "api_key": any(k in os.environ for k in
                       ("OPENAI_API_KEY", "CODEX_API_KEY", "AZURE_OPENAI_API_KEY")),
    })
    threading.Thread(target=injector, daemon=True).start()
    threads = 0
    failure = plan.get("app_server_error") or {}
    for line in sys.stdin:
        message = json.loads(line)
        record("received.jsonl", message)
        method, ident = message.get("method"), message.get("id")
        params = message.get("params") or {}
        if method is None or ident is None:
            continue
        if method == failure.get("method"):
            out({"id": ident, "error": {"code": failure.get("code", -32601),
                                        "message": failure.get("message", "unknown method")}})
        elif method == "initialize":
            out({"id": ident, "result": {"userAgent": "fake"}})
        elif method == "account/read":
            mode = plan.get("account", "chatgpt")
            account = {"type": mode, "planType": "plus"} if mode else None
            out({"id": ident, "result": {"account": account, "requiresOpenaiAuth": True}})
        elif method == "config/read":
            if plan.get("config_read", "ok") == "refuse":
                out({"id": ident, "error": {"code": -32000, "message": "config unreadable"}})
            else:
                out({"id": ident, "result": {"config": plan.get("config", CAPTAIN_CONFIG),
                                             "origins": {}}})
        elif method == "thread/start":
            threads += 1
            out({"id": ident, "result": {"thread": {"id": f"thr-{threads}"}}})
        elif method == "thread/realtime/start":
            transport = params.get("transport") or {}
            realtime = plan.get("realtime", "ok")
            if transport.get("type") != "webrtc" or not str(transport.get("sdp", "")).startswith("v=0"):
                out({"id": ident, "error": {"code": -32600, "message": "webrtc offer required"}})
            elif realtime == "refuse":
                out({"id": ident, "error": {"code": -32000,
                                            "message": "realtime refused: Bearer abc.def secret"}})
            else:
                out({"id": ident, "result": {}})
                thread = params["threadId"]
                if realtime == "ok":
                    out({"method": "thread/realtime/started",
                         "params": {"threadId": thread, "version": "v3"}})
                    out({"method": "thread/realtime/sdp",
                         "params": {"threadId": thread, "sdp": ANSWER}})
                elif realtime == "closed":
                    out({"method": "thread/realtime/closed",
                         "params": {"threadId": thread, "reason": "boom"}})
        elif method in ("thread/realtime/appendText", "thread/realtime/appendSpeech"):
            out({"id": ident, "result": {}})
        elif method == "thread/realtime/stop":
            out({"id": ident, "result": {}})
            out({"method": "thread/realtime/closed",
                 "params": {"threadId": params.get("threadId"), "reason": None}})
        else:
            out({"id": ident, "error": {"code": -32601, "message": f"unknown {method}"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
