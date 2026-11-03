#!/usr/bin/env python3
"""Opt-in attended Codex binding only. No mailbox polling or queue mutation."""
import fcntl
import json
import os
from pathlib import Path
import selectors
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from contextlib import contextmanager

sys.dont_write_bytecode = True
from codex_socket import Daemon, LIMIT, load, process_identity, safe

PATH_ENV = ("HOME", "CODEX_HOME", "PRIVATEER_FLEET", "PRIVATEER_RESOURCE_HOME", "TMPDIR",
            "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME")
BEARINGS = "Read your charter and inbox, handle requests within your authority, reply with correlation IDs, and acknowledge only handled mail. Production deployment still needs explicit approval."


def atomic(path, value):
    fd, name = tempfile.mkstemp(prefix=".binding-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


@contextmanager
def locked(directory):
    safe(directory, stat.S_ISDIR, private=True)
    fd = os.open(directory / "lock", os.O_CREAT | os.O_NOFOLLOW | os.O_RDWR, 0o600)
    try:
        safe(directory / "lock", private=True)
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield
    finally:
        os.close(fd)


def live(record):
    try:
        return process_identity(record["pid"]) == record["process_identity"]
    except (OSError, ValueError, KeyError):
        return False


def plan(argv, cwd):
    """A deliberately narrow attended argv grammar; unknown plans fail closed."""
    value_flags = {"-C", "--cd", "--sandbox", "-s", "--add-dir", "--model", "-m",
                   "--ask-for-approval", "-a", "--enable", "--disable"}
    bare_flags = {"--no-alt-screen", "--dangerously-bypass-approvals-and-sandbox"}
    args = argv[1:]
    positional = []
    resume = False
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--":
            positional.extend(args[i + 1:])
            break
        if arg == "resume" and i == 0:
            resume = True
        elif arg in ("-c", "--config"):
            i += 1
            if i >= len(args):
                raise ValueError("missing Codex config override")
            key = args[i].split("=", 1)[0]
            if not (key.startswith("features.") or key == "tui.fullscreen_transcript"):
                raise ValueError("Codex -c override excludes the shared daemon (including effort/web_search)")
        elif arg in value_flags:
            i += 1
            if i >= len(args):
                raise ValueError("missing Codex option value")
            if arg in ("-C", "--cd") and Path(args[i]).resolve() != cwd:
                raise ValueError("Codex binding cwd override refused")
        elif arg in bare_flags:
            pass
        elif arg.startswith("-"):
            raise ValueError("Codex attended plan requires the shared daemon; unsupported flag")
        else:
            positional.append(arg)
        i += 1
    session = None
    if resume:
        if not positional:
            raise ValueError("Codex attended resume requires an exact UUID")
        session = str(uuid.UUID(positional.pop(0)))
    if len(positional) > 1:
        raise ValueError("ambiguous Codex positional prompt")
    return session, bool(positional)


def prepare(argv, env):
    if sys.platform != "linux":
        raise ValueError("Codex attended binding currently requires Linux peer/process identity")
    if env.get("CODEX_EXEC_SERVER_URL"):
        raise ValueError("Codex executor override excludes implicit shared-daemon attachment")
    if env.get("PRIVATEER_ROLE") != "agent":
        raise ValueError("Codex binding requires the first-mate launch context")
    cwd = Path.cwd().resolve()
    session, prompt = plan(argv, cwd)
    for key in ("PRIVATEER_HOME", "PRIVATEER_CONTROL_CLI", "PRIVATEER_ACTOR", "PRIVATEER_PRESENCE_HOLDER"):
        if not env.get(key):
            raise ValueError("missing Codex first-mate binding context")
    home = Path(env["PRIVATEER_HOME"]).resolve(strict=True)
    control = Path(env["PRIVATEER_CONTROL_CLI"])
    if not control.is_absolute():
        raise ValueError("Codex binding control CLI must be absolute")
    codex_home = Path(env.get("CODEX_HOME", str(Path(env["HOME"]) / ".codex"))).resolve(strict=True)
    project = cwd / ".codex"
    project.mkdir(mode=0o700, exist_ok=True)
    safe(project, stat.S_ISDIR)
    directory = project / "privateer-attended"
    directory.mkdir(mode=0o700, exist_ok=True)
    registry = directory / "launch.json"
    command = shlex.join([sys.executable, "-B", str(Path(__file__).resolve()), "--registry", str(registry)])
    with locked(directory):
        if registry.exists() and live(load(registry)):
            raise ValueError("another Codex launch owns this project binding; concurrent cwd is unbound")
        hook_path = project / "hooks.json"
        hooks = load(hook_path) if hook_path.exists() else {"hooks": {}}
        groups = hooks.setdefault("hooks", {}).setdefault("SessionStart", [])
        if not isinstance(groups, list):
            raise ValueError("invalid Codex project hook declarations")
        if not any(h.get("command") == command for group in groups for h in group.get("hooks", [])):
            groups.append({"hooks": [{"type": "command", "command": command, "timeout": 10}]})
            atomic(hook_path, hooks)
        planned = list(argv)
        if env.get("PRIVATEER_HOSTED") == "1" and not prompt:
            if "--" not in planned:
                planned.append("--")
            planned.append(f"You are the first mate of {env['PRIVATEER_ACTOR']}. {BEARINGS}")
        record = {"generation": str(uuid.uuid4()), "pid": os.getpid(),
                  "process_identity": process_identity(os.getpid()),
                  "executable": str(Path(shutil.which(argv[0]) or argv[0]).resolve(strict=True)),
                  "started_ms": time.time_ns() // 1_000_000, "session": session,
                  "home": str(home), "cwd": str(cwd), "codex_home": str(codex_home),
                  "ship": env["PRIVATEER_ACTOR"], "holder": env["PRIVATEER_PRESENCE_HOLDER"],
                  "control": str(control), "hosted": env.get("PRIVATEER_HOSTED") == "1",
                  "path_env": {key: env[key] for key in PATH_ENV if key in env},
                  "argv": planned, "hook_command": command, "state": "unbound",
                  "experimental": True, "delivery_enabled": False}
        atomic(registry, record)
    print("Codex attended binding: UNBOUND until trusted SessionStart; experimental queue API; delivery disabled", file=sys.stderr)
    return planned


def control(record, command, args):
    """Bounded CLI argv/JSON, with the launch's state paths, never daemon actor state."""
    env = os.environ.copy()
    for key in PATH_ENV:
        env.pop(key, None)
    env.update(record["path_env"])
    env.update(PRIVATEER_ROLE="agent", PRIVATEER_ACTOR=record["ship"], PRIVATEER_HOME=record["home"])
    argv = [record["control"], "--home", record["home"], command, "--json", json.dumps(args)]
    with subprocess.Popen(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        try:
            output = bytearray()
            total = 0
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                selector.register(child.stderr, selectors.EVENT_READ)
                deadline = time.monotonic() + 3
                while selector.get_map():
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("Privateer binding report timed out")
                    events = selector.select(remaining)
                    for key, _ in events:
                        chunk = os.read(key.fd, 65536)
                        total += len(chunk)
                        if total > LIMIT:
                            raise ValueError("Privateer binding report exceeds limit")
                        if not chunk:
                            selector.unregister(key.fileobj)
                        elif key.fileobj == child.stdout:
                            output.extend(chunk)
                child.wait(timeout=max(0.001, deadline - time.monotonic()))
            if child.returncode:
                raise ValueError("Privateer binding report refused")
            return json.loads(output)
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()


def validate(record, event, daemon):
    if not live(record) or Path(f"/proc/{record['pid']}/exe").resolve() != Path(record["executable"]):
        raise ValueError("Codex launch identity is stale")
    session = str(uuid.UUID(event["session_id"]))
    if (event.get("hook_event_name") != "SessionStart"
            or event.get("source") not in ("startup", "resume", "fork", "clear", "compact")
            or event.get("cwd") != record["cwd"]):
        raise ValueError("Codex startup callback does not match launch")
    if event["source"] == "resume":
        if session != (record.get("binding", {}).get("session") or record["session"]):
            raise ValueError("Codex resume UUID does not match launch")
    elif event["source"] in ("startup", "fork", "clear"):
        native_id = uuid.UUID(session)
        if native_id.version != 7 or native_id.int >> 80 < record["started_ms"]:
            raise ValueError("Codex startup session predates launch")
    else:
        if session != record.get("binding", {}).get("session"):
            raise ValueError("Codex compact callback has no matching binding")
    loaded = daemon.call("thread/loaded/list", {"limit": 32})
    if (not isinstance(loaded.get("data"), list) or "nextCursor" not in loaded
            or session not in loaded["data"] or loaded["nextCursor"] or len(loaded["data"]) > 32):
        raise ValueError("hook UUID is not unambiguously loaded on the verified daemon")
    threads = [daemon.call("thread/read", {"threadId": identity})["thread"] for identity in loaded["data"]]
    matches = [thread for thread in threads if thread.get("cwd") == record["cwd"]]
    if len(matches) != 1:
        raise ValueError("concurrent Codex threads in this cwd are ambiguous")
    thread = matches[0]
    if (thread["id"] != session or thread.get("sessionId") != session
            or thread["cwd"] != record["cwd"] or thread.get("ephemeral")
            or thread.get("path") != event.get("transcript_path")):
        raise ValueError("Codex native session metadata mismatch")
    transcript = Path(event["transcript_path"])
    if not transcript.is_relative_to(Path(record["codex_home"]) / "sessions") or transcript.resolve() != transcript:
        raise ValueError("Codex transcript provenance mismatch")
    transcript_identity = None
    if transcript.exists():
        info = safe(transcript)
        with transcript.open("rb") as stream:
            line = stream.readline(LIMIT + 1)
        if len(line) > LIMIT:
            raise ValueError("Codex transcript metadata exceeds limit")
        meta = json.loads(line)
        if meta.get("type") != "session_meta" or meta.get("payload", {}).get("id") != session or meta["payload"].get("cwd") != record["cwd"]:
            raise ValueError("Codex transcript session metadata mismatch")
        transcript_identity = {"device": info.st_dev, "inode": info.st_ino}
    # Paginated history may not materialize a JSONL rollout, even at the hook.
    # This binds native metadata only, not receipt/offset recovery (PR B).
    queue = daemon.call("thread/queue/list", {"threadId": session, "limit": 1})
    if not isinstance(queue.get("data"), list) or "nextCursor" not in queue:
        raise ValueError("unsupported experimental Codex queue schema")
    hooks = daemon.call("hooks/list", {"threadId": session})["data"]
    if not any(h.get("command") == record["hook_command"] and h.get("source") == "project"
               and h.get("trustStatus") == "trusted" and h.get("enabled") is True
               for entry in hooks for h in entry.get("hooks", [])):
        raise ValueError("stable Codex project hook is not trusted on this thread")
    return {"session": session, "transcript": str(transcript), "history_mode": thread.get("historyMode"),
            "daemon_pid": daemon.pid, "daemon_identity": daemon.identity, "socket": str(daemon.target),
            "rendezvous": str(daemon.rendezvous), "metadata_source": "daemon_thread_read",
            "transcript_identity": transcript_identity}


def bind(registry, event):
    registry = Path(registry)
    with locked(registry.parent):
        record = load(registry)
        with Daemon(record["codex_home"]) as daemon:
            binding = validate(record, event, daemon)
        record["state"] = "unbound"
        atomic(registry, record)
        presence = control(record, "ship.presence", {"ship": record["ship"]})
        if presence.get("attended") is not True or presence.get("lease", {}).get("holder") != record["holder"]:
            raise ValueError("Codex binding does not own current presence")
        # A failed/uncertain report remains unbound and may safely be repeated:
        # delivery_bind neither presents mail nor records a wake.
        if record["hosted"]:
            result = control(record, "wake.hosted", {"ship": record["ship"], "holder": record["holder"],
                             "session": binding["session"], "trigger": "delivery_bind"})
            if result.get("recorded") is not True:
                raise ValueError("hosted Codex binding report not recorded")
        record.update(binding=binding, state="bound")
        atomic(registry, record)
        return binding


def main():
    try:
        if len(sys.argv) != 3 or sys.argv[1] != "--registry":
            raise ValueError("invalid Codex binding hook invocation")
        data = sys.stdin.buffer.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise ValueError("Codex startup callback exceeds limit")
        bind(sys.argv[2], json.loads(data))
        print("{}")
    except Exception as error:
        # Only our local validation diagnostics, never daemon/provider values.
        reason = str(error)[:200] if type(error) in (ValueError, TimeoutError) else type(error).__name__
        print(json.dumps({"systemMessage": "Privateer Codex binding UNBOUND (" + reason + "): inspect the launch, trusted project hook, presence and shared daemon; automatic mail remains disabled."}))


if __name__ == "__main__":
    main()
