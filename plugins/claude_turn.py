#!/usr/bin/env python3
"""Confirm a headless Claude run and record its native permission denials for the runtime."""
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True  # a release holds only its own files
from native_launch import native_environment
from native_turn import MAX_LINE, Turn

STARTUP_TIMEOUT_SECS = 300
# The 1200 s default is twice the standard 10-minute maximum timeout for a Bash
# tool call, ensuring Claude is not killed while awaiting a long tool execution.
IDLE_TIMEOUT_SECS = 1200


def startup_bound():
    try:
        return max(0.1, float(os.environ.get("PRIVATEER_CLAUDE_STARTUP_SECS", STARTUP_TIMEOUT_SECS)))
    except ValueError:
        return STARTUP_TIMEOUT_SECS


def idle_bound():
    try:
        limit = float(os.environ.get("PRIVATEER_CLAUDE_IDLE_SECS", IDLE_TIMEOUT_SECS))
    except ValueError:
        limit = IDLE_TIMEOUT_SECS
    bash_max_ms = os.environ.get("BASH_MAX_TIMEOUT_MS")
    if bash_max_ms:
        try:
            limit = max(limit, (float(bash_max_ms) / 1000.0) * 2)
        except ValueError:
            pass
    return max(0.1, limit)


def is_mise_shim(path):
    if not path:
        return False
    p = Path(path)
    try:
        real = p.resolve()
        if real.name == "mise" or "mise/shims" in str(p) or "mise/shims" in str(real):
            return True
        target = real if real.is_file() else p
        with open(target, "rb") as stream:
            header = stream.read(512)
            if header.startswith(b"#!") and (b"mise " in header or b"mise\n" in header or b"mise\t" in header):
                return True
    except OSError:
        pass
    return False


def resolve_claude(binary="claude", env=None):
    """Resolve the claude executable once instead of going through a mise shim.
    Prefers a real binary on PATH over shims, falling back to the direct mise
    install path, and finally any shim found on PATH."""
    env = os.environ if env is None else env
    if os.path.sep in binary:
        path = Path(binary)
        if path.is_file() and os.access(path, os.X_OK) and not is_mise_shim(path):
            return str(path)

    path_val = env.get("PATH", "")
    first_shim = None
    for entry in path_val.split(os.pathsep):
        if not entry:
            continue
        candidate = Path(entry) / binary
        if candidate.is_file() and os.access(candidate, os.X_OK):
            if not is_mise_shim(candidate):
                return str(candidate)
            elif first_shim is None:
                first_shim = str(candidate)

    data_dir = env.get("MISE_DATA_DIR")
    if not data_dir:
        xdg_data = env.get("XDG_DATA_HOME")
        if xdg_data:
            data_dir = Path(xdg_data) / "mise"
        else:
            home = env.get("HOME")
            data_dir = (Path(home) if home else Path.home()) / ".local/share/mise"
    else:
        data_dir = Path(data_dir)
    installed = data_dir / "installs" / "claude" / "latest" / "claude"
    if installed.is_file() and os.access(installed, os.X_OK):
        return str(installed)

    if os.path.sep in binary and Path(binary).is_file() and os.access(binary, os.X_OK):
        return binary

    return first_shim or binary


def signal_group(child, signum):
    """Signal the child's process group. macOS refuses (EPERM) a group whose
    members have all exited but are not yet reaped; then only the leader is
    left, and send_signal reaps it instead of signalling it."""
    try:
        os.killpg(child.pid, signum)
    except ProcessLookupError:
        pass
    except PermissionError:
        child.send_signal(signum)


# How long the leader is given to exit on its own after its stream ends, and
# how long each stop signal is given to take effect.
STOP_GRACE_SECS = 5


def stop_process(child):
    signal_group(child, signal.SIGTERM)
    deadline = time.monotonic() + STOP_GRACE_SECS
    if child.poll() is None:
        try:
            child.wait(timeout=STOP_GRACE_SECS)
        except subprocess.TimeoutExpired:
            pass
    while time.monotonic() < deadline:
        try:
            os.killpg(child.pid, 0)
        except (ProcessLookupError, PermissionError):
            break
        time.sleep(0.05)
    try:
        os.killpg(child.pid, 0)
        signal_group(child, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        child.send_signal(signal.SIGKILL)
    if child.poll() is None:
        try:
            child.wait(timeout=STOP_GRACE_SECS)
        except subprocess.TimeoutExpired:
            pass


USAGE_FIELDS = (("input_tokens", "input"), ("output_tokens", "output"),
                ("cache_read_input_tokens", "cacheRead"), ("cache_creation_input_tokens", "cacheWrite"))


def rising_result(previous, value):
    """A later result is accepted only when both carry an integer index and it
    rises. Without one on both, a repeated result stays an error."""
    before, after = previous.get("result_index"), value.get("result_index")
    return (isinstance(before, int) and not isinstance(before, bool)
            and isinstance(after, int) and not isinstance(after, bool)
            and after > before)


# Claude's own wording for a call its built-in safety checks stopped (a
# removal it cannot resolve, say): only a person at a prompt may approve one.
SAFETY_STOPS = ("cannot be auto-allowed by permission rules",
                "denied by a built-in Claude Code safety check")


def safety_stop(content):
    if isinstance(content, list):
        content = " ".join(b.get("text", "") for b in content if isinstance(b, dict) and isinstance(b.get("text"), str))
    return isinstance(content, str) and any(phrase in content for phrase in SAFETY_STOPS)


class ClaudeTurn(Turn):
    """Claude's init names the session; a run may end more than one turn, each
    with a result carrying a rising result_index, and the last settles the run."""

    def __init__(self, argv, servers=()):
        flags = argv[:argv.index("--")] if "--" in argv else argv
        super().__init__(flags[flags.index("--resume") + 1] if "--resume" in flags else None)
        self.result = None
        self.denied = False
        self.denials = []
        # Calls Claude's built-in safety checks stopped, by tool_use_id, and
        # how many of the results' denials were those: no rule, mode or
        # captain answer can allow one, so it is no ask for the captain.
        self.safety_stopped = set()
        self.safety_stops = 0
        # Why each refused call was refused, by tool_use_id, from Claude's
        # permission_denied events: the result's denials carry no reason.
        self.refusals = {}
        self.servers = list(servers)
        # The route's model, as the launch named it; the models Claude then
        # reports, from its init and each result's modelUsage.
        self.route_model = flags[flags.index("--model") + 1] if "--model" in flags[:-1] else None
        self.models = []

    def saw_model(self, name):
        if isinstance(name, str) and name and name not in self.models:
            self.models.append(name)

    def route_mismatch(self):
        """The models used that are not the route's, as a recorded fact for the
        job: a settings file or alias can send a run elsewhere than argv said.
        An alias (`opus`) names any model carrying it; `[1m]` is not a model."""
        if self.route_model is None:
            return None
        expected = self.route_model.lower().replace("[1m]", "")
        strays = [m for m in self.models if expected not in m.lower().replace("[1m]", "")]
        return {"expected": self.route_model, "used": strays} if strays else None

    def check_servers(self, reported, tools):
        """Every selected MCP server connected with tools, and no other: the Pi
        gate's guarantee, checked at init, before any tool runs. A run
        that selects none is not checked: unbounded runs keep the user's own
        servers, and a bounded one has --strict-mcp-config with no config."""
        if not self.servers:
            return
        reported = reported if isinstance(reported, list) else []
        tools = [tool for tool in tools if isinstance(tool, str)] if isinstance(tools, list) else []
        status = {}
        for item in reported:
            name = item.get("name") if isinstance(item, dict) else None
            if not (isinstance(name, str) and name in self.servers):
                shown = name if isinstance(name, str) and name.isascii() and name.isprintable() else "unknown"
                raise ValueError("mcp-foreign:" + shown[:32])
            status[name] = item.get("status")
        for name in self.servers:
            # Connected and listing tools, as the Pi gate requires.
            if status.get(name) != "connected" or not any(t.startswith(f"mcp__{name}__") for t in tools):
                raise ValueError("mcp-unavailable:" + name)

    def observe(self, value):
        if not isinstance(value, dict):
            raise ValueError("invalid-event")
        kind = value.get("type")
        if kind == "system" and value.get("subtype") == "init":
            super().observe({"type": "session", "id": value.get("session_id")})
            self.check_servers(value.get("mcp_servers"), value.get("tools"))
            self.saw_model(value.get("model"))
        elif kind == "system" and value.get("subtype") == "permission_denied":
            use, reason = value.get("tool_use_id"), value.get("decision_reason")
            if (isinstance(use, str) and isinstance(reason, str) and use not in self.refusals
                    and len(self.refusals) < MAX_REFUSALS):
                self.refusals[use] = {"reason": reason[:REASON_BYTES],
                                      "classifier": value.get("decision_reason_type") == "classifier"}
        elif kind == "user":
            content = (value.get("message") or {}).get("content")
            for block in content if isinstance(content, list) else ():
                if (isinstance(block, dict) and block.get("type") == "tool_result"
                        and block.get("is_error") is True and safety_stop(block.get("content"))):
                    self.safety_stopped.add(block.get("tool_use_id"))
        elif kind == "result":
            if value.get("session_id") != self.session:
                raise ValueError("changed-session")
            if self.result is not None and not rising_result(self.result, value):
                raise ValueError("repeated-result")
            self.result = value
            denials = value.get("permission_denials")
            if isinstance(denials, list):
                stopped = [d for d in denials if isinstance(d, dict) and d.get("tool_use_id") in self.safety_stopped]
                self.safety_stops += len(stopped)
                denials = [d for d in denials if d not in stopped]
                self.denials.extend(d for d in denials if isinstance(d, dict))
            self.denied |= bool(denials)
            # Each result reports the usage of its own turn, so a run with more
            # than one result sums them; Claude's modelUsage is cumulative.
            usage = value.get("usage")
            if isinstance(usage, dict):
                self.add_usage({new: usage[old] for old, new in USAGE_FIELDS if old in usage})
            if isinstance(value.get("modelUsage"), dict):
                for name in value["modelUsage"]:
                    self.saw_model(name)
            if self.usage:
                self.usage["totalTokens"] = sum(n for key, n in self.usage.items() if key != "totalTokens")

    def complete(self):
        result = self.result or {}
        return (self.session is not None and result.get("subtype") == "success"
                and result.get("is_error") is False)


# The runtime reads denials from this line at settlement, not from Claude's
# result line, which a long log can lose. It stays short: a few inputs, clipped.
MAX_DENIALS = 4
INPUT_BYTES = 4096
REASON_BYTES = 300
MAX_REFUSALS = 64


def clipped(value):
    """A tool input as it was, or with its long strings cut to about INPUT_BYTES."""
    if len(json.dumps(value, separators=(",", ":"))) <= INPUT_BYTES:
        return value
    if isinstance(value, dict):
        return {k: (v[:INPUT_BYTES] if isinstance(v, str) else v) for k, v in value.items()
                if isinstance(v, str) or len(json.dumps(v)) <= INPUT_BYTES}
    return json.dumps(value)[:INPUT_BYTES]


def report_denial(denials, refusals=None):
    """Each distinct denial once, with the reason Claude gave for the first of
    them when it gave one (the auto-mode classifier's category, say)."""
    seen, kept = set(), []
    for denial in denials:
        entry = {"tool_name": str(denial.get("tool_name", "")), "tool_input": clipped(denial.get("tool_input"))}
        key = json.dumps(entry, sort_keys=True)
        if entry["tool_name"] and key not in seen:
            seen.add(key)
            entry.update((refusals or {}).get(denial.get("tool_use_id"), {}))
            kept.append(entry)
    print(json.dumps({"type": "native.permission_blocked", "denials": kept[:MAX_DENIALS],
                      "denied": len(kept),
                      "reason": "A native tool was denied; the runtime asks the captain once."},
                     separators=(",", ":")), flush=True)


# Claude echoes a Read image twice in one user event: as the tool_result's
# base64 source and again in tool_use_result.file.base64. A 5 MB image is
# about 13 MiB of line. The model already has it, so the log keeps a short
# placeholder instead and never carries image bytes to transcripts or mail.
# MAX_LINE bounds what is forwarded; EVENT_CEILING bounds what is buffered.
EVENT_CEILING = 64 * 1024 * 1024
CONTENT_EVENTS = ("user", "assistant")
CONTENT_HEAD = re.compile(rb'\A\s*\{\s*"type"\s*:\s*"(user|assistant)"')
CLIP_CHARS = 8192


def omitted(text):
    return f"[privateer: {len(text)} bytes omitted]"


def without_payloads(value):
    """The value with each base64 payload replaced by a placeholder, and the
    bytes omitted."""
    if isinstance(value, dict):
        out, dropped = {}, 0
        encoded = value.get("type") == "base64"
        for key, item in value.items():
            if isinstance(item, str) and item and (key == "base64" or (encoded and key == "data")):
                out[key], dropped = omitted(item), dropped + len(item)
            else:
                out[key], more = without_payloads(item)
                dropped += more
        return out, dropped
    if isinstance(value, list):
        out, dropped = [], 0
        for item in value:
            item, more = without_payloads(item)
            out.append(item)
            dropped += more
        return out, dropped
    return value, 0


def clip_strings(value):
    if isinstance(value, dict):
        return {key: clip_strings(item) for key, item in value.items()}
    if isinstance(value, list):
        return [clip_strings(item) for item in value]
    if isinstance(value, str) and len(value) > CLIP_CHARS:
        return value[:CLIP_CHARS] + omitted(value[CLIP_CHARS:])
    return value


def forwarded(line, value):
    """The bytes to log for one event: the line as read, or a content event
    with its payloads omitted, clipped, or at last reduced to its identity.
    None when a non-content event is too large to forward."""
    content = isinstance(value, dict) and value.get("type") in CONTENT_EVENTS
    if not content or (b'"base64"' not in line and len(line) <= MAX_LINE):
        return line if len(line) <= MAX_LINE else None
    value, dropped = without_payloads(value)
    if not dropped and len(line) <= MAX_LINE:
        return line
    for shape in (lambda: value, lambda: clip_strings(value),
                  lambda: {**{key: value[key] for key in ("type", "subtype", "session_id", "uuid",
                                                          "parent_tool_use_id") if key in value},
                           "privateer_omitted_bytes": len(line)}):
        text = json.dumps(shape(), separators=(",", ":"))
        if len(text) <= MAX_LINE:
            break
    return text.encode() + b"\n"


def selected_servers(env):
    """Names of the run's selected MCP servers, from the launcher's records."""
    records = json.loads(env.get("PRIVATEER_MCP_SERVERS") or "[]")
    if not isinstance(records, list) or not all(isinstance(r, dict) and isinstance(r.get("name"), str)
                                                for r in records):
        raise ValueError("mcp-selection")
    return [record["name"] for record in records]


def run(argv, control):
    turn = ClaudeTurn(argv, selected_servers(os.environ))
    executable = resolve_claude(argv[0])
    command = [executable, *argv[1:]]
    child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             env=native_environment(), start_new_session=True)
    state = {
        "started": time.monotonic(),
        "last_event": None,
        "seen_lines": 0,
        "reason": None,
        "done": False,
    }
    lock = threading.Lock()

    def read_stdout():
        try:
            while line := child.stdout.readline(EVENT_CEILING + 1):
                if len(line) > EVENT_CEILING:
                    # Past the ceiling only a content event is skipped, unread:
                    # the model already has it. Any other event may settle the
                    # run, so it still fails it.
                    head = CONTENT_HEAD.match(line)
                    if not head:
                        with lock:
                            state["reason"] = "event-size"
                        break
                    kind, size = head.group(1).decode(), len(line)
                    while line and not line.endswith(b"\n"):
                        line = child.stdout.readline(MAX_LINE)
                        size += len(line)
                    if not line:
                        with lock:
                            state["reason"] = "truncated-stream"
                        break
                    sys.stdout.buffer.write(json.dumps(
                        {"type": "privateer.event_omitted", "reason": "event-over-ceiling",
                         "event_type": kind, "bytes": size}, separators=(",", ":")).encode() + b"\n")
                    sys.stdout.buffer.flush()
                elif not line.endswith(b"\n"):
                    with lock:
                        state["reason"] = "truncated-stream"
                    break
                elif line.strip():
                    value = None
                    try:
                        value = json.loads(line)
                        turn.observe(value)
                    except json.JSONDecodeError:
                        sys.stdout.buffer.write(line if len(line) <= MAX_LINE else line[:MAX_LINE] + b"\n")
                        with lock:
                            state["reason"] = "invalid-json"
                        break
                    except ValueError as error:
                        sys.stdout.buffer.write(forwarded(line, value) or b"")
                        with lock:
                            state["reason"] = str(error)
                        break
                    shown = forwarded(line, value)
                    if shown is None:
                        with lock:
                            state["reason"] = "event-size"
                        break
                    sys.stdout.buffer.write(shown)
                    sys.stdout.buffer.flush()
                else:
                    sys.stdout.buffer.write(line)
                    sys.stdout.buffer.flush()
                with lock:
                    state["seen_lines"] += 1
                    state["last_event"] = time.monotonic()
        except Exception as error:
            with lock:
                if not state["reason"]:
                    state["reason"] = str(error)
        finally:
            with lock:
                state["done"] = True

    def end(signum, _frame):
        stop_process(child)
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, end)
    reader = threading.Thread(target=read_stdout, daemon=True)
    reader.start()

    startup_limit = startup_bound()
    idle_limit = idle_bound()
    reason = None
    try:
        while child.poll() is None:
            with lock:
                seen = state["seen_lines"]
            limit = startup_limit if seen == 0 else idle_limit
            tick = min(1.0, limit / 4)
            # Wake as soon as claude exits, and give its reader up to a tick
            # to drain the closed pipe, rather than sleeping a whole tick.
            try:
                child.wait(timeout=tick)
                reader.join(timeout=tick)
            except subprocess.TimeoutExpired:
                pass
            now = time.monotonic()
            with lock:
                if state["done"] or state["reason"]:
                    break
                if state["seen_lines"] == 0:
                    if now - state["started"] >= startup_limit:
                        reason = "no-output"
                        break
                else:
                    if now - state["last_event"] >= idle_limit:
                        reason = "stalled"
                        break
        if not reason and child.poll() is not None and reader.is_alive():
            # The leader is gone but its stdout is still open: a grandchild
            # survived it. Stop the group so nothing leaks and the reader can
            # reach EOF instead of blocking on a pipe a grandchild holds.
            stop_process(child)
        if not reason:
            # The child exited or the reader ended. Let the reader finish before
            # choosing, so its specific reason never depends on which noticed
            # first. It has at most a pipe buffer left once the child is gone.
            reader.join(timeout=STOP_GRACE_SECS)
            with lock:
                reason = state["reason"]
        if reason:
            stop_process(child)
        reader.join(timeout=STOP_GRACE_SECS)
        with lock:
            reason = reason or state["reason"]
        if reader.is_alive():
            # A survivor still holds the pipe and the reader ended without a
            # reason: never block the turn on a stream nobody will close.
            stop_process(child)
            reader.join(timeout=STOP_GRACE_SECS)
            with lock:
                reason = reason or state["reason"]
            if reader.is_alive() or not (turn.complete() and not turn.denied):
                reason = reason or "stream-eof"
        try:
            status = child.wait(timeout=STOP_GRACE_SECS)
        except subprocess.TimeoutExpired:
            # The stream hit EOF with no reason while claude kept running. Bound
            # the wait, stop the whole group, and settle the turn as failed.
            reason = reason or "stream-eof"
            stop_process(child)
            try:
                status = child.wait(timeout=STOP_GRACE_SECS)
            except subprocess.TimeoutExpired:
                status = None
        success = reason is None and status == 0 and turn.complete() and not turn.denied
        summary = {"type": "turn.completed" if success else "native.turn.failed",
                   "harness": "claude", "session_id": turn.session, "native_exit_code": status}
        if reason:
            summary["reason"] = reason
        if turn.usage:
            summary["usage"] = turn.usage
        if turn.models:
            summary["models"] = turn.models
        if mismatch := turn.route_mismatch():
            summary["route_mismatch"] = mismatch
        if turn.safety_stops:
            summary["safety_stops"] = turn.safety_stops
        print(json.dumps(summary, separators=(",", ":")), flush=True)
        if turn.denied:
            report_denial(turn.denials, turn.refusals)
        return 0 if success else 2
    finally:
        if child.poll() is None:
            stop_process(child)
        if not reader.is_alive():
            try:
                child.stdout.close()
            except OSError:
                pass


def main(argv):
    try:
        raise SystemExit(run(argv[3:], json.loads(argv[2])))
    except Exception:
        print('{"type":"native.turn.failed","reason":"Claude stream could not be read."}', flush=True)
        raise SystemExit(2)


if __name__ == "__main__":
    main(sys.argv)
