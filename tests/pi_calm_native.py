#!/usr/bin/env python3
"""Authless installed-class/session probe. Linux libseccomp is required.

Only this disposable fixture changes HOME/agentDir. Production is untouched.
No inherited credentials, config, preload hooks, proxies, sockets or model calls.
"""
import ctypes
import errno
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def refuse_transport():
    # Independent of Pi offline mode or extension event handling: refuse
    # network sockets/connect and io_uring before exec, including descendants.
    # Fail closed if this kernel/library cannot install the fixture boundary.
    lib = ctypes.CDLL("libseccomp.so.2", use_errno=True)
    lib.seccomp_init.argtypes = [ctypes.c_uint32]
    lib.seccomp_init.restype = ctypes.c_void_p
    lib.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    lib.seccomp_syscall_resolve_name.restype = ctypes.c_int
    lib.seccomp_rule_add.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint]
    lib.seccomp_load.argtypes = [ctypes.c_void_p]
    lib.seccomp_release.argtypes = [ctypes.c_void_p]
    context = lib.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
    if not context:
        raise RuntimeError("cannot create fixture transport guard")
    try:
        for name in (b"socket", b"connect", b"io_uring_setup"):
            number = lib.seccomp_syscall_resolve_name(name)
            if number < 0 or lib.seccomp_rule_add(context, 0x00050000 | errno.EPERM, number, 0):
                raise RuntimeError("cannot qualify fixture transport guard")
        if lib.seccomp_load(context):
            raise RuntimeError("cannot load fixture transport guard")
    finally:
        lib.seccomp_release(context)


def main():
    pi = str(Path(sys.argv[1]).resolve(strict=True))
    fixture = Path(__file__).with_name("pi-calm-native.ts").resolve()
    with tempfile.TemporaryDirectory(prefix="privateer-calm-native-") as scratch:
        agent = Path(scratch) / "agent"
        agent.mkdir()
        (agent / "auth.json").write_text("{}")
        (agent / "models.json").write_text("{}")
        (agent / "mcp.json").write_text('{"mcpServers":{}}')
        (agent / "settings.json").write_text(
            '{"packages":[],"cacheWarming":"off","enableInstallTelemetry":false,"enableAnalytics":false}')
        # An explicit clean allowlist, not a filtered copy of the host env.
        env = dict(PATH="/usr/bin:/bin", HOME=scratch, XDG_CONFIG_HOME=scratch,
                   XDG_CACHE_HOME=str(Path(scratch) / "cache"), TMPDIR=scratch,
                   PI_CODING_AGENT_DIR=str(agent), PI_OFFLINE="1",
                   PI_SKIP_VERSION_CHECK="1", PI_TELEMETRY="0",
                   PRIVATEER_HOME=str(Path(scratch) / ".privateer"),
                   PRIVATEER_ROLE="agent", PRIVATEER_ACTOR="fixture",
                   PRIVATEER_ATTENDED_HARNESS="pi")
        check = subprocess.run([sys.executable, "-c",
                                "import socket,errno\ntry: socket.socket()\nexcept OSError as e: "
                                "assert e.errno==errno.EPERM\nelse: raise SystemExit(1)"],
                               env=env, cwd=scratch, preexec_fn=refuse_transport, timeout=5,
                               capture_output=True, text=True)
        if check.returncode:
            raise RuntimeError("fixture external transport refusal failed")
        result = subprocess.run([pi, "--offline", "--no-extensions", "--no-mcp", "--no-skills",
                                 "--no-context-files", "--no-themes", "--no-session",
                                 "--no-prompt-templates", "--mode", "rpc", "-e", str(fixture)],
                                input="", text=True, capture_output=True, cwd=scratch,
                                env=env, timeout=30, preexec_fn=refuse_transport)
        print(result.stdout, end="")
        print(result.stderr, end="", file=sys.stderr)
        if result.returncode or "zero model requests; transport refused" not in result.stdout:
            raise SystemExit(result.returncode or 1)
        root = Path(__file__).resolve().parents[1]
        # The sole approved integration here is the test inspector itself, in
        # a synthetic test plan. No machine registry/approval is read or changed.
        plan = {'schema_version': 1, 'pi_version': '1.2.0', 'selection': [], 'skills': [],
                'integrations': {'reviewed': True, 'extensions': [
                    {'path': str(fixture), 'sha256': hashlib.sha256(fixture.read_bytes()).hexdigest()}]}}
        for bounded in (False, True):
            for mode, adapters in (('headless', []), ('attended', ['mail', 'calm']),
                                   ('hosted', ['crew', 'calm'])):
                launch_env = dict(env, PATH=f'{Path(pi).parent}:/usr/bin:/bin',
                                  PRIVATEER_CALM_LAUNCH=mode)
                if mode == 'hosted':
                    launch_env['PRIVATEER_HOSTED_CREW'] = '1'
                args = [sys.executable, str(root / 'plugins/native_launch.py')]
                if bounded:
                    args += ['--tool-bundles', json.dumps(plan)]
                if mode == 'attended':
                    args += ['--attended-policy', '{"mode":"standard","claude_allow":[]}']
                args += ['pi', 'pi', '--offline', '--no-extensions', '--no-mcp', '--no-skills',
                         '--no-context-files', '--no-prompt-templates', '--no-themes',
                         '--no-session', '--mode', 'rpc']
                if not bounded:
                    args += ['-e', str(fixture)]
                for adapter in adapters:
                    args += ['-e', str(root / f'plugins/attended/{adapter}.ts')]
                run = subprocess.run(args, input='', text=True, capture_output=True, cwd=scratch,
                                     env=launch_env, timeout=30, preexec_fn=refuse_transport)
                if run.returncode:
                    raise RuntimeError(f'{mode} bounded={bounded}: {run.stderr}')
                records = [json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
                record = next((r for r in records if r.get('launch') == mode), None)
                assert record == dict(launch=mode, calm=mode != 'headless', home=env['PRIVATEER_HOME'],
                                      actor='fixture', role='agent', agentDir=str(agent), cwd=scratch), run.stdout + run.stderr
                print(f'ok - installed Pi launch {mode}, bounded={bounded}: exact runtime inventory/identity/home/cwd; no inference/transport')


if __name__ == "__main__":
    main()
