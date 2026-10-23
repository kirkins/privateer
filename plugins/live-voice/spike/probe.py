#!/usr/bin/env python3
"""Go/no-go probe: can a managed Codex login start a realtime voice session?

Drives the official `codex app-server` over stdio JSON-RPC so Codex uses its
own managed login. The probe never reads Codex's credential files, never sets
or passes an API key, and never talks to an OpenAI endpoint itself. Before any
realtime call it reads the account through `account/read`; anything other than
a ChatGPT login is refused without calling realtime.

Sequence: initialize (experimental opt-in), initialized, account/read,
thread/start (ephemeral, read-only, empty scratch cwd), thread/realtime/start
(model gpt-live-1-codex, version v3, audio out, websocket transport), one
appendText turn, a bounded wait for realtime events, thread/realtime/stop and
a clean shutdown that kills the app-server's process group on timeout.

Prints one JSON object. Audio content and credentials are never printed;
--save-audio PATH writes the received audio as a WAV for a hand listen.

Usage: probe.py [--codex PATH] [--timeout SECS] [--save-audio PATH]
"""

import argparse
import base64
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import wave

MODEL = "gpt-live-1-codex"
VERSION = "v3"
PROMPT = "Say the word ready."
CLIENT = {"name": "privateer-live-voice-probe", "title": None, "version": "0.1.0"}
# Variables that would let Codex bill an API key instead of the managed login.
API_KEY_VARIABLES = ("OPENAI_API_KEY", "CODEX_API_KEY", "AZURE_OPENAI_API_KEY")
TRANSCRIPT_LIMIT = 2000
MESSAGE_LIMIT = 300
SECRET_PATTERNS = [
    re.compile(r"(?i)bearer\s+\S+"),
    re.compile(r"\bsk-[A-Za-z0-9_\-]{8,}"),
    re.compile(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]*"),
]


def redact(text, limit=MESSAGE_LIMIT):
    """Strip anything token-shaped and bound the length of server text."""
    text = str(text)
    for pattern in SECRET_PATTERNS:
        text = pattern.sub("[redacted]", text)
    return text if len(text) <= limit else text[:limit] + "..."


class Timeout(Exception):
    pass


class AppServer:
    """One `codex app-server` child on stdio, its own process group."""

    def __init__(self, codex, cwd):
        env = dict(os.environ)
        for name in API_KEY_VARIABLES:
            env.pop(name, None)
        options = {}
        if os.name == "posix":
            options["start_new_session"] = True
        else:
            options["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        self.proc = subprocess.Popen(
            [codex, "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Server logs are not read: they are not part of the verdict and
            # the probe keeps anything credential-adjacent out of its output.
            stderr=subprocess.DEVNULL,
            cwd=cwd,
            env=env,
            **options,
        )
        self.inbox = queue.Queue()
        self.next_id = 0
        self.non_json_lines = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                self.non_json_lines += 1
                continue
            if isinstance(message, dict):
                self.inbox.put(message)
        self.inbox.put(None)

    def send(self, message):
        self.proc.stdin.write(json.dumps(message).encode() + b"\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        self.send({"id": self.next_id, "method": method, "params": params})
        return self.next_id

    def notify(self, method, params=None):
        message = {"method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def receive(self, deadline):
        """The next message, None at end of stream; Timeout at the deadline."""
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Timeout()
        try:
            return self.inbox.get(timeout=remaining)
        except queue.Empty:
            raise Timeout() from None

    def shutdown(self, grace):
        """Close stdin and wait; terminate, then kill the whole group."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        outcome = "clean"
        try:
            self.proc.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            outcome = "terminated"
            self._signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                outcome = "killed"
                self._signal(getattr(signal, "SIGKILL", signal.SIGTERM))
                self.proc.wait(timeout=grace)
        # The app-server may have started helpers (the voice host). Its exit
        # is not proof they stopped, so the group is swept and checked.
        if os.name == "posix":
            self._signal(signal.SIGKILL)
            return outcome, not self._group_alive()
        return outcome, None

    def _signal(self, sig):
        try:
            if os.name == "posix":
                os.killpg(self.proc.pid, sig)
            elif sig == signal.SIGTERM:
                self.proc.terminate()
            else:
                self.proc.kill()
        except (ProcessLookupError, PermissionError, OSError):
            pass

    def _group_alive(self):
        for _ in range(20):
            try:
                os.killpg(self.proc.pid, 0)
            except (ProcessLookupError, PermissionError):
                return False
            time.sleep(0.05)
        return True


class Probe:
    def __init__(self, args):
        self.args = args
        self.steps = []
        self.events = {}
        self.item_types = {}
        self.errors = []
        self.audio_bytes = 0
        self.audio_chunks = []
        self.audio_formats = {}
        self.transcript_deltas = []
        self.transcript_done = []
        self.thread_id = None
        self.started = False
        self.closed = False
        self.turns_interrupted = 0
        self.server = None

    # -- JSON-RPC ------------------------------------------------------------

    def call(self, method, params, timeout=None):
        """One request; records a step and returns (result, error)."""
        began = time.monotonic()
        wanted = self.server.request(method, params)
        deadline = began + (timeout or self.args.rpc_timeout)
        step = {"method": method, "ok": False}
        try:
            while True:
                message = self.server.receive(deadline)
                if message is None:
                    step["error"] = {"code": None, "message": "app-server exited"}
                    break
                if message.get("id") == wanted and "method" not in message:
                    if "error" in message:
                        error = message["error"] or {}
                        step["error"] = {
                            "code": error.get("code"),
                            "message": redact(error.get("message", "")),
                        }
                    else:
                        step["ok"] = True
                    break
                self.handle(message)
        except Timeout:
            step["error"] = {"code": None, "message": "timed out"}
        step["ms"] = int((time.monotonic() - began) * 1000)
        self.steps.append(step)
        if step["ok"]:
            return message.get("result"), None
        return None, step["error"]

    def handle(self, message):
        """A notification or server request arriving between responses."""
        method = message.get("method")
        if not isinstance(method, str):
            return
        if "id" in message:
            # The probe approves nothing: every server request (command or
            # file approvals, tool calls, token refresh) is declined.
            self.events[f"request:{method}"] = self.events.get(f"request:{method}", 0) + 1
            self.server.send({
                "id": message["id"],
                "error": {"code": -32000, "message": "declined by the live voice probe"},
            })
            return
        self.events[method] = self.events.get(method, 0) + 1
        params = message.get("params") or {}
        if method == "thread/realtime/started":
            self.started = True
        elif method == "thread/realtime/outputAudio/delta":
            self.audio(params.get("audio") or {})
        elif method == "thread/realtime/transcript/delta":
            if params.get("role") == "assistant":
                self.transcript_deltas.append(str(params.get("delta", "")))
        elif method == "thread/realtime/transcript/done":
            if params.get("role") == "assistant":
                self.transcript_done.append(str(params.get("text", "")))
        elif method in ("thread/realtime/itemAdded", "thread/realtime/item/started",
                        "thread/realtime/item/completed"):
            item = params.get("item")
            kind = item.get("type") if isinstance(item, dict) else None
            if isinstance(kind, str):
                key = f"{method}:{kind[:60]}"
                self.item_types[key] = self.item_types.get(key, 0) + 1
        elif method == "thread/realtime/error":
            self.errors.append(redact(params.get("message", "")))
        elif method == "thread/realtime/closed":
            self.closed = True
            if params.get("reason"):
                self.errors.append("closed: " + redact(params["reason"]))
        elif method == "turn/started" and self.thread_id:
            # A realtime session may delegate to a Codex turn. The probe is
            # only about voice, so any such turn is interrupted at once.
            turn = params.get("turn") or {}
            if turn.get("id"):
                self.turns_interrupted += 1
                self.server.request("turn/interrupt",
                                    {"threadId": self.thread_id, "turnId": turn["id"]})

    def audio(self, chunk):
        try:
            data = base64.b64decode(chunk.get("data", ""), validate=True)
        except (ValueError, TypeError):
            self.errors.append("undecodable audio chunk")
            return
        self.audio_bytes += len(data)
        shape = (chunk.get("sampleRate"), chunk.get("numChannels"))
        self.audio_formats[shape] = self.audio_formats.get(shape, 0) + 1
        if self.args.save_audio:
            self.audio_chunks.append((shape, data))

    # -- The probe ------------------------------------------------------------

    def run(self):
        report = {"verdict": "error", "reason": None, "codex_version": None,
                  "auth_mode": None, "plan_type": None}
        codex = self.args.codex or shutil.which("codex")
        if not codex:
            report["reason"] = "codex not found; pass --codex PATH"
            return report
        report["codex_version"] = codex_version(codex)
        with tempfile.TemporaryDirectory(prefix="live-voice-probe-") as scratch:
            try:
                self.server = AppServer(codex, scratch)
            except OSError as error:
                report["reason"] = f"could not start codex app-server: {redact(error)}"
                return report
            try:
                self.converse(report, scratch)
            except OSError:
                # A broken pipe: the app-server went away mid-conversation.
                report["verdict"] = "error"
                report["reason"] = "app-server closed its stdin early"
            finally:
                outcome, group_gone = self.server.shutdown(self.args.grace)
                report["shutdown"] = outcome
                report["process_group_gone"] = group_gone
                if self.server.non_json_lines:
                    report["non_json_lines"] = self.server.non_json_lines
        return report

    def converse(self, report, scratch):
        _, error = self.call("initialize", {
            "clientInfo": CLIENT,
            "capabilities": {"experimentalApi": True, "requestAttestation": False},
        })
        if error:
            report["reason"] = "initialize failed"
            return
        self.server.notify("initialized")

        account, error = self.call("account/read", {"refreshToken": False})
        if error:
            report["reason"] = "account/read failed"
            return
        login = (account or {}).get("account")
        mode = login.get("type") if isinstance(login, dict) else None
        report["auth_mode"] = mode or "none"
        if isinstance(login, dict) and isinstance(login.get("planType"), str):
            report["plan_type"] = login["planType"]
        if mode != "chatgpt":
            # An API key bills per use; the captain has not approved that.
            report["reason"] = (f"refused: the Codex login is {report['auth_mode']!r}, "
                                "not a ChatGPT login; realtime was not called")
            return

        thread, error = self.call("thread/start", {
            "cwd": scratch,
            "ephemeral": True,
            "sandbox": "read-only",
            "approvalPolicy": "untrusted",
        })
        self.thread_id = ((thread or {}).get("thread") or {}).get("id")
        if error or not self.thread_id:
            report["reason"] = "thread/start failed"
            return

        _, error = self.call("thread/realtime/start", {
            "threadId": self.thread_id,
            "model": self.args.model,
            "version": self.args.version,
            "outputModality": "audio",
            "transport": {"type": "websocket"},
            "includeStartupContext": False,
            "clientManagedHandoffs": True,
        })
        if error:
            report["verdict"] = "no-go" if error["message"] != "timed out" else "error"
            report["reason"] = "thread/realtime/start refused"
            return

        try:
            self.wait(lambda: self.started or self.closed or self.errors,
                      time.monotonic() + self.args.rpc_timeout)
        except Timeout:
            pass
        if not self.started:
            if self.errors or self.closed:
                report["verdict"] = "no-go"
                report["reason"] = "realtime session failed to start"
            else:
                report["reason"] = "no thread/realtime/started within the RPC timeout"
            self.stop()
            return

        method = ("thread/realtime/appendSpeech" if self.args.append == "speech"
                  else "thread/realtime/appendText")
        params = {"threadId": self.thread_id, "text": self.args.prompt}
        if self.args.append == "text":
            params["role"] = "user"
        _, error = self.call(method, params)
        if error:
            report["verdict"] = "no-go" if error["message"] != "timed out" else "error"
            report["reason"] = f"{method} refused"
            self.stop()
            return

        self.listen()
        self.stop()
        if self.audio_bytes > 0:
            report["verdict"] = "go"
            report["reason"] = "realtime session started and returned audio"
        elif self.errors or self.closed:
            report["verdict"] = "no-go"
            report["reason"] = "realtime session ended with an error before any audio"
        else:
            report["reason"] = f"no audio within {self.args.timeout:g}s"

    def wait(self, done, deadline):
        while not done():
            message = self.server.receive(deadline)
            if message is None:
                self.closed = True
                self.errors.append("app-server exited")
                return
            self.handle(message)

    def listen(self):
        """Collect events until the answer settles, fails, or time runs out."""
        deadline = time.monotonic() + self.args.timeout
        last_audio = None
        while time.monotonic() < deadline:
            if self.closed or (self.errors and not self.audio_bytes):
                return
            if self.audio_bytes and self.transcript_done:
                return
            # Once audio has flowed, a quiet spell ends the answer.
            step = deadline
            if last_audio is not None:
                step = min(deadline, last_audio + self.args.settle)
            try:
                message = self.server.receive(step)
            except Timeout:
                if last_audio is not None:
                    return
                continue
            if message is None:
                self.closed = True
                return
            before = self.audio_bytes
            self.handle(message)
            if self.audio_bytes > before:
                last_audio = time.monotonic()

    def stop(self):
        if self.server.proc.poll() is None and not self.closed:
            self.call("thread/realtime/stop", {"threadId": self.thread_id})

    def finish(self, report):
        transcript = "".join(self.transcript_done) or "".join(self.transcript_deltas)
        report["steps"] = self.steps
        report["audio_bytes"] = self.audio_bytes
        report["audio_format"] = [
            {"sample_rate": rate, "channels": channels, "chunks": count}
            for (rate, channels), count in self.audio_formats.items()
        ]
        report["transcript"] = redact(transcript, TRANSCRIPT_LIMIT)
        report["events_seen"] = dict(sorted(self.events.items()))
        report["realtime_item_types"] = dict(sorted(self.item_types.items()))
        report["realtime_errors"] = self.errors
        if self.turns_interrupted:
            report["codex_turns_interrupted"] = self.turns_interrupted
        if self.args.save_audio and self.audio_chunks:
            report["saved_audio"] = save_wav(self.args.save_audio, self.audio_chunks)
        return report


def codex_version(codex):
    try:
        out = subprocess.run([codex, "--version"], capture_output=True, text=True,
                             timeout=15, env=without_api_keys()).stdout
    except (OSError, subprocess.SubprocessError):
        return None
    return redact(out.strip(), 80) or None


def without_api_keys():
    env = dict(os.environ)
    for name in API_KEY_VARIABLES:
        env.pop(name, None)
    return env


def save_wav(path, chunks):
    """16-bit PCM, the realtime session's audio/pcm; one format per file."""
    (rate, channels), _ = chunks[0]
    data = b"".join(chunk for shape, chunk in chunks if shape == (rate, channels))
    with wave.open(path, "wb") as out:
        out.setnchannels(channels or 1)
        out.setsampwidth(2)
        out.setframerate(rate or 24000)
        out.writeframes(data)
    return {"path": path, "bytes": len(data)}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--codex", help="codex binary (default: codex on PATH)")
    parser.add_argument("--timeout", type=float, default=30.0,
                        help="seconds to wait for the spoken answer (default 30)")
    parser.add_argument("--rpc-timeout", type=float, default=20.0,
                        help="seconds to wait for each JSON-RPC response (default 20)")
    parser.add_argument("--settle", type=float, default=3.0,
                        help="quiet seconds after audio that end the answer (default 3)")
    parser.add_argument("--grace", type=float, default=5.0,
                        help="seconds per shutdown stage before escalating (default 5)")
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--version", default=VERSION)
    parser.add_argument("--prompt", default=PROMPT)
    parser.add_argument("--append", choices=("text", "speech"), default="text",
                        help="send the prompt with appendText (default) or appendSpeech")
    parser.add_argument("--save-audio", metavar="PATH",
                        help="write received audio to PATH as WAV (off by default)")
    args = parser.parse_args(argv)
    probe = Probe(args)
    report = probe.finish(probe.run())
    print(json.dumps(report))
    return 0 if report["verdict"] in ("go", "no-go") else 1


if __name__ == "__main__":
    sys.exit(main())
