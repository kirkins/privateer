#!/usr/bin/env python3
"""Authless stdio MCP fake. JSONL markers contain no environment/credential values."""
import json
import os
from pathlib import Path
import signal
import sys
import time


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


def main():
    marker, identity, mode = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
    def record(event, **fields):
        with marker.open('a') as stream:
            stream.write(json.dumps({'event': event, **fields}) + '\n')
    record('start', pid=os.getpid(), pgid=os.getpgrp(),
           start=Path('/proc/self/stat').read_text().split()[21])
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


if __name__ == '__main__':
    main()
