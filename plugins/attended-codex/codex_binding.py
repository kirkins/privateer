#!/usr/bin/env python3
"""Opt-in attended Codex binding only. No mailbox polling or queue mutation."""
import fcntl
import hashlib
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

SYSTEM_PYTHON = "/usr/bin/python3"
# Exact bytes shipped by B0/PR 675; no other dispatcher is migrated.
B0_DISPATCHER_SHA256 = "13ebd926418ea7ee13ec7503fdf5d02ea54e13619ace2bc9413eb6e6d48c4d1f"

PATH_ENV = ("HOME", "CODEX_HOME", "PRIVATEER_FLEET", "PRIVATEER_RESOURCE_HOME", "TMPDIR",
            "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "PRIVATEER_SERVICE")

# Deliberately version-independent. The trusted command names a project-local
# dispatcher; the private launch record selects this launch's release handler.
DISPATCHER = '''import fcntl, json, os, pathlib, runpy, stat, sys, tempfile, time, uuid
sys.dont_write_bytecode = True
registry = pathlib.Path(__file__).with_name("launch.json")
for path in (registry.parent, registry):
    info = path.lstat()
    expected = stat.S_ISDIR if path == registry.parent else stat.S_ISREG
    if not expected(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("unsafe Codex binding dispatcher state")
# Publish before attempting the state lock. A timeout/crash cannot leave the old
# binding usable. This code stays self-contained even if the release is gone.
request = str(uuid.uuid4())
fence = registry.with_name("invalidate.json")
if fence.exists() or fence.is_symlink():
    info = fence.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("unsafe Codex invalidation request")
fd, name = tempfile.mkstemp(prefix=".poison-", dir=registry.parent)
try:
    with os.fdopen(fd, "w") as stream:
        json.dump({"token": request}, stream)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(name, fence)
    parent = os.open(registry.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(parent)
    finally:
        os.close(parent)
finally:
    if os.path.exists(name):
        os.unlink(name)
lock = os.open(registry.with_name("lock"), os.O_CREAT | os.O_NOFOLLOW | os.O_RDWR, 0o600)
try:
    info = os.fstat(lock)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("unsafe Codex binding dispatcher lock")
    deadline = time.monotonic() + 1
    while True:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            if time.monotonic() >= deadline:
                raise TimeoutError("Codex binding state lock timed out")
            time.sleep(0.01)
    with registry.open("rb") as stream:
        data = stream.read((1 << 20) + 1)
    if len(data) > 1 << 20:
        raise ValueError("Codex binding dispatcher record exceeds limit")
    previous = json.loads(data)
    record = dict(previous)
    token = str(uuid.uuid4())
    record.update(state="unbound", delivery_enabled=False, callback_generation=token)
    if "binding" in record:
        record["last_binding"] = record.pop("binding")
    fd, name = tempfile.mkstemp(prefix=".binding-", dir=registry.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(record, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, registry)
        parent = os.open(registry.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)
finally:
    os.close(lock)
# Invalidation is durable even when a release was pruned or is unsafe to load.
handler = pathlib.Path(record["handler"])
if not handler.is_absolute() or handler.name != "codex_binding.py":
    raise ValueError("Codex binding handler must be absolute and named codex_binding.py")
info = handler.lstat()
if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
    raise ValueError("unsafe Codex binding handler")
sys.path.insert(0, str(handler.parent))
sys.argv = [str(handler), "--registry", str(registry)]
runpy.run_path(str(handler), run_name="__main__",
              init_globals={"_DISPATCH_CONTEXT": (previous, token, request)})
'''


def hook_command(registry):
    # Keep the literal system path: resolving its symlink or using the launcher's
    # version-managed interpreter would change the native hook trust hash.
    if not Path(SYSTEM_PYTHON).is_file() or not os.access(SYSTEM_PYTHON, os.X_OK):
        raise ValueError("Codex attended hook requires executable /usr/bin/python3; no version-managed interpreter fallback")
    return shlex.join([SYSTEM_PYTHON, "-I", "-B", str(Path(registry).with_name("hook.py"))])


def check_system_python():
    # Probe the interpreter that actually runs the isolated hook, not the
    # launcher's Python. Refuse before modifying project state.
    if not Path(SYSTEM_PYTHON).is_file() or not os.access(SYSTEM_PYTHON, os.X_OK):
        raise ValueError("Codex attended hook requires executable /usr/bin/python3; no version-managed interpreter fallback")
    try:
        result = subprocess.run(
            [SYSTEM_PYTHON, "-I", "-B", "-c",
             "import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, timeout=3, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ValueError("Codex attended hook requires /usr/bin/python3 >= 3.9; interpreter probe failed") from error
    if result.returncode != 0:
        raise ValueError("Codex attended hook requires /usr/bin/python3 >= 3.9 (Path.is_relative_to)")


def install_dispatcher(directory):
    path = directory / "hook.py"
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except FileExistsError:
        safe(path, private=True)
        body = path.read_bytes()
        if body == DISPATCHER.encode():
            return
        if hashlib.sha256(body).hexdigest() != B0_DISPATCHER_SHA256:
            raise ValueError("Codex project dispatcher differs; inspect it before relaunch")
        # prepare holds the project lock and has refused any live owner. The
        # declaration/command stays unchanged; native trust is never rewritten.
        fd, name = tempfile.mkstemp(prefix=".dispatcher-", dir=directory)
        try:
            with os.fdopen(fd, "w") as stream:
                stream.write(DISPATCHER)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(name, path)
            sync_directory(directory)
        finally:
            if os.path.exists(name):
                os.unlink(name)
    else:
        with os.fdopen(fd, "w") as stream:
            stream.write(DISPATCHER)
            stream.flush()
            os.fsync(stream.fileno())
        sync_directory(directory)
BEARINGS = "Read your charter and inbox, handle requests within your authority, reply with correlation IDs, and acknowledge only handled mail. Production deployment still needs explicit approval."


def sync_directory(directory):
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, value):
    fd, name = tempfile.mkstemp(prefix=".binding-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        sync_directory(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)


@contextmanager
def locked(directory):
    safe(directory, stat.S_ISDIR, private=True)
    fd = os.open(directory / "lock", os.O_CREAT | os.O_NOFOLLOW | os.O_RDWR, 0o600)
    try:
        safe(directory / "lock", private=True)
        deadline = time.monotonic() + 1
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("Codex binding state lock timed out")
                time.sleep(0.01)
        yield
    finally:
        os.close(fd)


def publish_request(registry):
    """Sticky pre-lock poison, including when the callback cannot acquire lock."""
    safe(registry.parent, stat.S_ISDIR, private=True)
    path = registry.with_name("invalidate.json")
    if path.exists() or path.is_symlink():
        safe(path, private=True)
    request = str(uuid.uuid4())
    atomic(path, {"token": request})
    return request


def request_token(registry):
    path = registry.with_name("invalidate.json")
    safe(path, private=True)
    value = load(path)["token"]
    if not isinstance(value, str) or str(uuid.UUID(value)) != value:
        raise ValueError("invalid Codex invalidation request")
    return value


def require_fence(registry, record):
    """Poison prerequisite only, NOT full delivery authorization.

    Future admission must call this under the binding lock immediately before
    spawn, alongside generation/custody/UUID/daemon checks. B0 cannot pass even
    if its old record says bound/enabled. Missing/unreadable poison fails closed.
    """
    binding = record.get("binding", {})
    if (binding.get("fence_version") != 1 or not binding.get("invalidation_token")
            or request_token(registry) != binding["invalidation_token"]):
        raise ValueError("Codex binding invalidation fence missing or superseded")


def invalidate(registry):
    """Publish poison, then a short transaction; never lock across daemon calls."""
    request = publish_request(registry)
    with locked(registry.parent):
        previous = load(registry)
        record = dict(previous)
        token = str(uuid.uuid4())
        record.update(state="unbound", delivery_enabled=False, callback_generation=token)
        if "binding" in record:
            record["last_binding"] = record.pop("binding")
        atomic(registry, record)
    return previous, token, request


def own_dispatcher(hook, dispatcher):
    argv = shlex.split(hook["command"])
    return (hook.get("type") == "command" and len(argv) == 4
            and argv[1:] == ["-I", "-B", dispatcher])


def project_hooks(value, registry, cwd):
    """Validate shapes and refuse legacy callbacks before changing any declaration."""
    if not isinstance(value, dict) or not isinstance(value.get("hooks", {}), dict):
        raise ValueError("invalid Codex project hook declarations")
    for groups in value.get("hooks", {}).values():
        if not isinstance(groups, list):
            raise ValueError("invalid Codex project hook declarations")
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise ValueError("invalid Codex project hook declarations")
            for hook in group["hooks"]:
                if not isinstance(hook, dict) or not isinstance(hook.get("command"), str):
                    raise ValueError("invalid Codex project hook declarations")
                # shlex also catches malformed quoting. Conservative textual
                # matching covers shell wrappers and --registry=PATH as well.
                text = " ".join(shlex.split(hook["command"]))
                if "codex_binding.py" in text and any(path in text for path in
                        (str(registry), str(registry.relative_to(cwd)))):
                    raise ValueError("legacy codex_binding.py hook for this registry; review and remove it through native hook management before relaunch (not removed or re-trusted automatically)")
    # Replace only our exact earlier isolated dispatcher declarations, regardless
    # of interpreter. Preserve unrelated hooks/group metadata and all trust state.
    dispatcher = str(registry.with_name("hook.py"))
    command = hook_command(registry)
    # Trust keys contain group/hook positions. Preflight the whole edit before
    # mutating anything: only a removable suffix may be dropped. Removing a
    # duplicate before any retained declaration would shift its trust key.
    installed = False
    for event, groups in value.get("hooks", {}).items():
        removals = []
        for group in groups:
            for hook in group["hooks"]:
                own = own_dispatcher(hook, dispatcher)
                keep = not own or (event == "SessionStart" and not installed)
                if own and keep:
                    installed = True
                removals.append(not keep)
        retained_after = False
        for remove in reversed(removals):
            if remove and retained_after:
                raise ValueError("duplicate Codex dispatcher precedes another hook; review and remove it through native hook management before relaunch (trust positions must not shift)")
            retained_after = retained_after or not remove
    installed = False
    for event, groups in value.setdefault("hooks", {}).items():
        kept = []
        for group in groups:
            hooks = []
            for hook in group["hooks"]:
                own = own_dispatcher(hook, dispatcher)
                if not own:
                    hooks.append(hook)
                elif event == "SessionStart" and not installed:
                    # Hook keys include declaration position. Keep the first
                    # declaration in place so an unchanged command retains trust
                    # and unrelated hooks do not move unnecessarily.
                    hooks.append({**hook, "command": command})
                    installed = True
            if hooks or not group["hooks"]:
                kept.append({**group, "hooks": hooks})
        value["hooks"][event] = kept
    return value["hooks"].setdefault("SessionStart", [])


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
    if session is None:
        raise ValueError("Codex attended binding requires explicit codex resume <UUID>; fresh pane identity is unproven")
    for key in ("PRIVATEER_HOME", "PRIVATEER_CONTROL_CLI", "PRIVATEER_ACTOR", "PRIVATEER_PRESENCE_HOLDER"):
        if not env.get(key):
            raise ValueError("missing Codex first-mate binding context")
    home = Path(env["PRIVATEER_HOME"]).resolve(strict=True)
    control = Path(env["PRIVATEER_CONTROL_CLI"])
    if not control.is_absolute():
        raise ValueError("Codex binding control CLI must be absolute")
    codex_home = Path(env.get("CODEX_HOME", str(Path(env["HOME"]) / ".codex"))).resolve(strict=True)
    check_system_python()
    project = cwd / ".codex"
    project.mkdir(mode=0o700, exist_ok=True)
    safe(project, stat.S_ISDIR)
    directory = project / "privateer-attended"
    directory.mkdir(mode=0o700, exist_ok=True)
    registry = directory / "launch.json"
    command = hook_command(registry)
    with locked(directory):
        if registry.exists() and live(load(registry)):
            raise ValueError("another Codex launch owns this project binding; concurrent cwd is unbound")
        hook_path = project / "hooks.json"
        hooks = load(hook_path) if hook_path.exists() else {"hooks": {}}
        groups = project_hooks(hooks, registry, cwd)
        install_dispatcher(directory)
        if not any(h.get("command") == command for group in groups for h in group["hooks"]):
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
                  "argv": planned, "hook_command": command,
                  "handler": str(Path(__file__).resolve()), "state": "unbound",
                  "experimental": True, "delivery_enabled": False}
        atomic(registry, record)
    print("Codex attended binding: UNBOUND until trusted SessionStart; experimental queue API; delivery disabled", file=sys.stderr)
    return planned


def control(record, command, args):
    """Bounded CLI argv/JSON, with the launch's state paths, never daemon actor state."""
    # The callback runs in a shared daemon, whose inherited actor, credentials,
    # loader variables and state paths are NOT the pane's launch environment.
    env = {"PATH": os.defpath, "LANG": "C.UTF-8"}
    env.update({key: value for key, value in record["path_env"].items() if key in PATH_ENV})
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
    # A trusted project hook is shared by every TUI in the folder. UUID age,
    # hook trust and a unique loaded cwd cannot identify which pane started a
    # fresh thread: another pane can win that race before ours loads. Only an
    # immutable UUID explicitly present in this pane's launch argv is proof.
    if event["source"] in ("startup", "fork", "clear"):
        raise ValueError("fresh/fork/clear pane identity is unproven; relaunch with explicit codex resume <UUID>")
    if not record.get("session") or session != record["session"]:
        raise ValueError("Codex resume UUID does not match launch")
    if event["source"] == "compact" and session != record.get("binding", record.get("last_binding", {})).get("session"):
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


def bind(registry, event, context=None):
    registry = Path(registry)
    # The dispatcher has already invalidated before loading this handler. Direct
    # calls must do the same. last_binding retains compact continuity across
    # overlapping callbacks, but is never an active/delivery binding.
    previous, token, request = invalidate(registry) if context is None else context
    record = dict(previous)
    if isinstance(event, bytes):
        if len(event) > LIMIT:
            raise ValueError("Codex startup callback exceeds limit")
        event = json.loads(event)
    with Daemon(record["codex_home"]) as daemon:
        binding = validate(previous, event, daemon)
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
    with locked(registry.parent):
        current = load(registry)
        if (current.get("callback_generation") != token or current["generation"] != record["generation"]
                or request_token(registry) != request):
            raise ValueError("Codex binding superseded by a newer callback or launch")
        binding.update(fence_version=1, invalidation_token=request)
        current.update(binding=binding, last_binding=binding, state="bound", delivery_enabled=False)
        atomic(registry, current)
    return binding


def main():
    try:
        if len(sys.argv) != 3 or sys.argv[1] != "--registry":
            raise ValueError("invalid Codex binding hook invocation")
        data = sys.stdin.buffer.read(LIMIT + 1)
        bind(sys.argv[2], data, globals().get("_DISPATCH_CONTEXT"))
        print("{}")
    except Exception as error:
        # Only our local validation diagnostics, never daemon/provider values.
        reason = str(error)[:200] if type(error) in (ValueError, TimeoutError) else type(error).__name__
        print(json.dumps({"systemMessage": "Privateer Codex binding UNBOUND (" + reason + "): inspect the launch, trusted project hook, presence and shared daemon; automatic mail remains disabled."}))


if __name__ == "__main__":
    main()
