#!/usr/bin/env python3
"""Authless stdio MCP fake. JSONL markers contain no environment/credential
values: the start marker lists the environment's names only, and PATH.

Modes: normal, disconnected, hang-init; stubborn starts a grandchild that
ignores SIGTERM in this process group, escaped one that also leaves it
(setsid). Each child's pid and start time are recorded, so a test can show
it is gone."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

# A grandchild that outlives a polite stop: SIGTERM ignored, closed stdio.
STUBBORN = 'import signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nwhile True: time.sleep(1)\n'


def tools():
    return [dict(name=name, description='Return a bounded fixture echo. SCHEMA_SENTINEL_9381',
                 inputSchema={'type': 'object', 'properties': {'value': {'type': 'string'}},
                              'required': ['value'], 'additionalProperties': False},
                 annotations={'readOnlyHint': True, 'openWorldHint': False})
            for name in ('echo', 'echo-item', 'echo_item', 'slow')]


def response(request, identity):
    method = request.get('method')
    if method == 'initialize':
        return {'protocolVersion': request['params']['protocolVersion'],
                'capabilities': {'tools': {}},
                'serverInfo': {'name': 'privateer-offline-fixture', 'version': '1'},
                'instructions': 'Authless qualification only.'}
    if method == 'tools/list':
        return {'tools': tools()}
    if method == 'tools/call':
        name = request['params']['name']
        value = request['params']['arguments']['value']
        return {'content': [{'type': 'text', 'text': json.dumps({'ok': True, 'identity': identity, 'value': value})}],
                'structuredContent': {'ok': True, 'identity': identity, 'value': value}}
    if method == 'ping':
        return {}
    raise ValueError('unsupported fixture method')


def started(pid):
    """A process's start time on Linux, which names it across pid reuse; None
    where there is no /proc (macOS)."""
    try:
        stat = Path(f'/proc/{pid}/stat').read_text()
    except FileNotFoundError:
        return None
    return stat[stat.rindex(')') + 2:].split()[19]


def main():
    marker, identity, mode = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
    def record(event, **fields):
        with marker.open('a') as stream:
            stream.write(json.dumps({'event': event, **fields}) + '\n')
    record('start', pid=os.getpid(), pgid=os.getpgrp(), start=started(os.getpid()), env=sorted(os.environ),
           path=os.environ.get('PATH'))
    if mode in ('stubborn', 'escaped'):
        child = subprocess.Popen([sys.executable, '-I', '-c', STUBBORN], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=mode == 'escaped')
        record('child', pid=child.pid, pgid=os.getpgid(child.pid), start=started(child.pid))
    def stop(_sig, _frame):
        raise SystemExit(0)
    signal.signal(signal.SIGTERM, stop)
    try:
        if mode == 'disconnected':
            return
        for _ in range(128):
            line = sys.stdin.readline(65537)
            if not line:
                break
            if len(line) > 65536:
                raise ValueError('oversized fixture request')
            request = json.loads(line)
            method = request.get('method')
            record('request', method=method, name=request.get('params', {}).get('name'))
            if mode == 'hang-init' or (method == 'tools/call' and request['params']['name'] == 'slow'):
                # Wait for Pi cancellation/termination; never launch an untracked child.
                if 'id' in request:
                    continue
            if 'id' not in request:
                continue
            result = response(request, identity)
            print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
    finally:
        record('stop')


def alive(event):
    """True while the recorded process runs and is not a zombie: the same
    start time too, where /proc records one."""
    state = subprocess.run(['ps', '-p', str(event['pid']), '-o', 'stat='], capture_output=True, text=True).stdout.strip()
    return bool(state) and not state.startswith('Z') and started(event['pid']) == event['start']


def outliving(events, grace=3):
    """The fake's recorded processes still running after `grace` seconds, then
    killed: a native probe runs outside a job, so nothing else would end them."""
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline and any(alive(e) for e in events if e['event'] in ('start', 'child')):
        time.sleep(0.1)
    found = [e for e in events if e['event'] in ('start', 'child') and alive(e)]
    for event in found:
        os.kill(event['pid'], 9)
    return found


if __name__ == '__main__':
    main()
