#!/usr/bin/env python3
"""Manual spike: does a running Claude Code session re-read its `--settings` FILE?

Not wired into any test. It runs Claude Code against a fake Anthropic Messages API on
127.0.0.1 with a fake key and an isolated HOME, so no credential or real account is used.
Every request body is appended to `<case>/requests.jsonl`. The auto-mode classifier's
requests are the ones that carry `SPIKE-BASELINE-<nonce>` (an `autoMode.allow` entry);
after `s.json` is rewritten to add `SPIKE-LIVE-<nonce>`, a reload shows as a later
classifier request carrying it, from the same pid.

    python3 tests/fixtures/claude/settings_reload_spike.py headless
    python3 tests/fixtures/claude/settings_reload_spike.py tmux --write inplace
    python3 tests/fixtures/claude/settings_reload_spike.py tmux --write rename
    python3 tests/fixtures/claude/settings_reload_spike.py control
    python3 tests/fixtures/claude/settings_reload_spike.py headless --self-edit
    python3 tests/fixtures/claude/settings_reload_spike.py summary

Everything goes under `target/reload-spike/` (override with --root). Set CLAUDE_BIN to
the real binary when `claude` on PATH is a wrapper (mise, asdf) that needs the real HOME.
Only one case runs at a time; each kills only what it started.
"""

import argparse
import json
import os
import secrets
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
TMUX_SESSION = "reload-spike"


class FakeApi:
    """A minimal Anthropic Messages API. The auto-mode classifier (no tools, carries the
    baseline sentence) gets an allowing `<severity>5`; its stop sequence is `</severity>`.
    The main loop (offers the Bash tool) gets one Bash `curl` per user prompt, then text
    after its tool_result. Everything else gets a short text reply."""

    def __init__(self, case_dir):
        self.case_dir = case_dir
        self.log = open(case_dir / "requests.jsonl", "a", buffering=1)
        self.lock = threading.Lock()
        self.seq = 0
        self.probes = 0
        # Tool calls the main loop makes before any probe (the self-edit case).
        self.queued = []
        self.classifier_reply = os.environ.get("SPIKE_CLASSIFIER_REPLY", "<severity>5")
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.port = self.server.server_address[1]

    def start(self):
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def stop(self):
        self.server.shutdown()
        self.log.close()

    def record(self, path, body, kind):
        with self.lock:
            self.seq += 1
            self.log.write(json.dumps({"seq": self.seq, "t": time.time(), "path": path,
                                       "kind": kind, "body": body}) + "\n")

    def reply(self, body):
        text = json.dumps(body)
        tools = [t.get("name") for t in body.get("tools") or [] if isinstance(t, dict)]
        if "SPIKE-BASELINE" in text and not tools:
            return "classifier", [{"type": "text", "text": self.classifier_reply}]
        if "Bash" in tools and self.queued:
            with self.lock:
                self.probes += 1
                n = self.probes
                name, tool_input = self.queued.pop(0)
            return "main-tool", [{"type": "tool_use", "id": f"toolu_spike{n:04d}",
                                  "name": name, "input": tool_input}]
        if "Bash" in tools:
            turns = [m for m in body.get("messages") or [] if m.get("role") in ("user", "assistant")]
            content = (turns or [{}])[-1].get("content")
            if isinstance(content, list) and any(
                    isinstance(c, dict) and c.get("type") == "tool_result" for c in content):
                return "main-after-tool", [{"type": "text", "text": "Probe finished."}]
            with self.lock:
                self.probes += 1
                n = self.probes
            command = f"curl -s http://127.0.0.1:{self.port}/probe-{n}"
            return "main-tool", [{"type": "tool_use", "id": f"toolu_spike{n:04d}",
                                  "name": "Bash",
                                  "input": {"command": command, "description": f"Probe {n}"}}]
        return "other", [{"type": "text", "text": "ok"}]

    def _handler(self):
        api = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def send(self, code, payload, ctype="application/json"):
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(code)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_HEAD(self):
                self.send(200, b"")

            def do_GET(self):
                api.record(self.path, None, "get")
                self.send(200, b"probe-ok\n" if self.path.startswith("/probe-") else b"{}",
                          "text/plain" if self.path.startswith("/probe-") else "application/json")

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("content-length") or 0))
                try:
                    body = json.loads(raw or b"{}")
                except ValueError:
                    body = {"unparsed": raw.decode("utf-8", "replace")}
                if "count_tokens" in self.path:
                    api.record(self.path, body, "count_tokens")
                    return self.send(200, {"input_tokens": 10})
                if "/v1/messages" not in self.path:
                    api.record(self.path, body, "post-other")
                    return self.send(200, {})
                kind, content = api.reply(body)
                api.record(self.path, body, kind)
                model = body.get("model", "claude-spike")
                stop = "tool_use" if content[0]["type"] == "tool_use" else "end_turn"
                msg = {"id": f"msg_spike{api.seq}", "type": "message", "role": "assistant",
                       "model": model, "content": content, "stop_reason": stop,
                       "stop_sequence": None,
                       "usage": {"input_tokens": 10, "output_tokens": 5}}
                if not body.get("stream"):
                    return self.send(200, msg)
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("cache-control", "no-cache")
                self.send_header("connection", "close")
                self.end_headers()

                def event(name, data):
                    self.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())

                event("message_start", {"type": "message_start", "message": {
                    **msg, "content": [], "stop_reason": None,
                    "usage": {"input_tokens": 10, "output_tokens": 1}}})
                for i, block in enumerate(content):
                    if block["type"] == "text":
                        event("content_block_start", {"type": "content_block_start", "index": i,
                                                      "content_block": {"type": "text", "text": ""}})
                        event("content_block_delta", {"type": "content_block_delta", "index": i,
                                                      "delta": {"type": "text_delta",
                                                                "text": block["text"]}})
                    else:
                        event("content_block_start", {"type": "content_block_start", "index": i,
                                                      "content_block": {**block, "input": {}}})
                        event("content_block_delta", {"type": "content_block_delta", "index": i,
                                                      "delta": {"type": "input_json_delta",
                                                                "partial_json": json.dumps(block["input"])}})
                    event("content_block_stop", {"type": "content_block_stop", "index": i})
                event("message_delta", {"type": "message_delta",
                                        "delta": {"stop_reason": stop, "stop_sequence": None},
                                        "usage": {"output_tokens": 5}})
                event("message_stop", {"type": "message_stop"})
                self.wfile.flush()
                self.close_connection = True

        return Handler


def claude_bin():
    found = os.environ.get("CLAUDE_BIN") or shutil.which("claude")
    if not found:
        sys.exit("claude not found; set CLAUDE_BIN")
    return str(Path(found).resolve())


def settings(nonces):
    return json.dumps({"autoMode": {"allow": ["$defaults"] + nonces}})


def write_inplace(path, text):
    with open(path, "r+") as f:
        f.truncate(0)
        f.write(text)


def write_rename(path, text):
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text)
    os.rename(tmp, path)


def new_case(root, name):
    case = root / f"{name}-{time.strftime('%Y%m%d-%H%M%S')}"
    home = case / "home"
    repo = case / "repo"
    for d in (home / ".claude", repo, case / "xdg" / "config", case / "xdg" / "state",
              case / "xdg" / "cache"):
        d.mkdir(parents=True, exist_ok=True)
    git = ["git", "-c", "user.name=spike", "-c", "user.email=spike@example.invalid"]
    subprocess.run(git + ["init", "-q", str(repo)], check=True,
                   env={"PATH": "/usr/bin:/bin", "HOME": str(home)})
    (repo / "README.md").write_text("scratch\n")
    subprocess.run(git + ["-C", str(repo), "add", "."], check=True,
                   env={"PATH": "/usr/bin:/bin", "HOME": str(home)})
    subprocess.run(git + ["-C", str(repo), "commit", "-qm", "scratch"], check=True,
                   env={"PATH": "/usr/bin:/bin", "HOME": str(home)})
    key = "sk-ant-spike-fake"
    (home / ".claude.json").write_text(json.dumps({
        "hasCompletedOnboarding": True,
        "lastOnboardingVersion": "2.1.300",
        "theme": "dark",
        "customApiKeyResponses": {"approved": [key[-20:]], "rejected": []},
        "projects": {str(repo): {"hasTrustDialogAccepted": True,
                                 "hasCompletedProjectOnboarding": True,
                                 "allowedTools": []}},
    }))
    (home / ".claude" / "settings.json").write_text(json.dumps({"skipAutoPermissionPrompt": True}))
    return case, home, repo, key


def env_for(case, home, key, port):
    return {
        "PATH": "/usr/bin:/bin",
        "TERM": "xterm-256color",
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(case / "xdg" / "config"),
        "XDG_STATE_HOME": str(case / "xdg" / "state"),
        "XDG_CACHE_HOME": str(case / "xdg" / "cache"),
        "ANTHROPIC_API_KEY": key,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    }


def classifier_requests(case):
    out = []
    path = case / "requests.jsonl"
    if not path.exists():
        return out
    for line in path.read_text().splitlines():
        rec = json.loads(line)
        if rec["kind"] == "classifier" or rec["kind"].startswith("main"):
            text = json.dumps(rec["body"])
            # `live` looks only at the system prompt, where the classifier lists the
            # allow rules; the transcript can quote the new text (the self-edit case).
            rules = json.dumps(rec["body"].get("system"))
            out.append({"seq": rec["seq"], "t": rec["t"], "kind": rec["kind"],
                        "baseline": "SPIKE-BASELINE" in text, "live": "SPIKE-LIVE" in rules,
                        "live_anywhere": "SPIKE-LIVE" in text})
    return out


def wait_for(pred, timeout, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(step)
    return False


def meta(case, data):
    (case / "meta.json").write_text(json.dumps(data, indent=1))
    print(json.dumps(data, indent=1))


def run_headless(args):
    case, home, repo, key = new_case(args.root, "self-edit" if args.self_edit else "headless")
    base, live = f"SPIKE-BASELINE-{secrets.token_hex(4)}", f"SPIKE-LIVE-{secrets.token_hex(4)}"
    sfile = case / "s.json"
    sfile.write_text(settings([base]))
    api = FakeApi(case).start()
    if args.self_edit:
        # The model itself asks to widen its own allow list; no outside rewrite happens.
        api.queued = [("Read", {"file_path": str(sfile)}),
                      ("Write", {"file_path": str(sfile), "content": settings([base, live])})]
    argv = [claude_bin(), "-p", "--input-format", "stream-json", "--output-format",
            "stream-json", "--verbose", "--permission-mode", "auto", "--settings", str(sfile)]
    out = open(case / "stdout.jsonl", "w")
    proc = subprocess.Popen(argv, cwd=repo, env=env_for(case, home, key, api.port),
                            stdin=subprocess.PIPE, stdout=out, stderr=open(case / "stderr.txt", "w"),
                            text=True)

    def results():
        return sum(1 for line in (case / "stdout.jsonl").read_text().splitlines()
                   if '"type":"result"' in line.replace(" ", ""))

    def send(text):
        proc.stdin.write(json.dumps({"type": "user", "message": {"role": "user",
                                                                 "content": text}}) + "\n")
        proc.stdin.flush()

    send("Run probe 1.")
    first = wait_for(lambda: results() >= 1, 120)
    if args.self_edit:
        rewrote = time.time()
    else:
        write = write_inplace if args.write == "inplace" else write_rename
        write(sfile, settings([base, live]))
        rewrote = time.time()
        time.sleep(10)
    send("Run probe 2.")
    second = wait_for(lambda: results() >= 2, 120)
    pid = proc.pid
    proc.stdin.close()
    try:
        proc.wait(30)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(10)
    api.stop()
    reqs = classifier_requests(case)
    meta(case, {"case": "self-edit" if args.self_edit else "headless",
                "write": "model-write" if args.self_edit else args.write, "pid": pid,
                "s_json_after": json.loads(sfile.read_text()), "first_result": first,
                "second_result": second, "rewrote_at": rewrote,
                "classifier": [{**r, "after_rewrite": r["t"] > rewrote} for r in reqs]})


def tmux(*argv, check=True):
    return subprocess.run(["tmux", *argv], check=check, capture_output=True, text=True)


def capture():
    return tmux("capture-pane", "-p", "-t", TMUX_SESSION, check=False).stdout


def run_tmux(args, case_name=None, preset_live=False):
    if tmux("has-session", "-t", TMUX_SESSION, check=False).returncode == 0:
        sys.exit(f"tmux session {TMUX_SESSION} already exists; kill it first")
    case, home, repo, key = new_case(args.root, case_name or f"tmux-{args.write}")
    base, live = f"SPIKE-BASELINE-{secrets.token_hex(4)}", f"SPIKE-LIVE-{secrets.token_hex(4)}"
    sfile = case / "s.json"
    sfile.write_text(settings([base, live] if preset_live else [base]))
    api = FakeApi(case).start()
    env = env_for(case, home, key, api.port)
    inner = ["env", "-i", *[f"{k}={v}" for k, v in env.items()],
             claude_bin(), "--permission-mode", "auto", "--settings", str(sfile)]
    tmux("new-session", "-d", "-s", TMUX_SESSION, "-x", "160", "-y", "50", "-c", str(repo),
         "--", *inner)
    pane_pid = tmux("display-message", "-p", "-t", TMUX_SESSION, "#{pane_pid}").stdout.strip()
    screens = []

    def snap(label):
        screens.append(f"===== {label} =====\n{capture()}")

    def prompt(text):
        tmux("send-keys", "-t", TMUX_SESSION, "-l", text)
        time.sleep(0.5)
        tmux("send-keys", "-t", TMUX_SESSION, "Enter")

    def until(pred, timeout):
        # One-time notices (the classifier billing notice for a non-Anthropic base URL)
        # hold the turn until dismissed.
        def step():
            if "Enter to continue" in capture():
                snap("notice")
                tmux("send-keys", "-t", TMUX_SESSION, "Enter")
                time.sleep(1)
            return pred()
        return wait_for(step, timeout)

    try:
        ready = wait_for(lambda: ">" in capture() or "❯" in capture(), 60)
        time.sleep(3)
        snap("ready")
        prompt("Run probe 1.")
        got1 = until(lambda: "Probe finished." in capture(), 120)
        snap("after prompt 1")
        rewrote = None
        got2 = None
        if not preset_live:
            write = write_inplace if args.write == "inplace" else write_rename
            write(sfile, settings([base, live]))
            rewrote = time.time()
            time.sleep(10)
            snap("after rewrite")
            prompt("Run probe 2.")
            got2 = until(lambda: capture().count("Probe finished.") >= 2, 120)
            snap("after prompt 2")
        pid_after = tmux("display-message", "-p", "-t", TMUX_SESSION, "#{pane_pid}",
                         check=False).stdout.strip()
    finally:
        (case / "screens.txt").write_text("\n".join(screens))
        tmux("kill-session", "-t", TMUX_SESSION, check=False)
        api.stop()
    reqs = classifier_requests(case)
    meta(case, {"case": case_name or "tmux", "write": args.write, "ready": ready,
                "first_result": got1, "second_result": got2, "pane_pid": pane_pid, "pane_pid_after": pid_after,
                "rewrote_at": rewrote, "preset_live": preset_live,
                "classifier": [{**r, "after_rewrite": bool(rewrote) and r["t"] > rewrote}
                               for r in reqs]})


def run_control(args):
    run_tmux(args, case_name="control", preset_live=True)


def run_summary(args):
    for m in sorted(args.root.glob("*/meta.json")):
        data = json.loads(m.read_text())
        line = [m.parent.name, f"write={data.get('write')}"]
        for kind in ("classifier", "main"):
            reqs = [c for c in data.get("classifier", []) if c["kind"].startswith(kind)]
            after = [c for c in reqs if c.get("after_rewrite")]
            line.append(f"{kind}: total={len(reqs)} live_any={sum(c['live'] for c in reqs)}"
                        f" after_rewrite={len(after)} live_after={sum(c['live'] for c in after)}")
        print("  ".join(line))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--root", type=Path, default=REPO / "target" / "reload-spike")
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("headless", "tmux", "control"):
        s = sub.add_parser(name)
        s.add_argument("--write", choices=("inplace", "rename"), default="inplace")
        if name == "headless":
            s.add_argument("--self-edit", action="store_true",
                           help="the fake model asks to Write s.json itself")
    sub.add_parser("summary")
    args = p.parse_args()
    args.root = args.root.resolve()
    args.root.mkdir(parents=True, exist_ok=True)
    {"headless": run_headless, "tmux": run_tmux, "control": run_control,
     "summary": run_summary}[args.cmd](args)


if __name__ == "__main__":
    main()
