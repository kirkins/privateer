#!/usr/bin/env python3
"""Qualification-only Linux network boundary; no unguarded fallback.

python3 tests/pi_offline_netns.py -- COMMAND [ARG ...]
The whole command (including listeners/descendants) shares a fresh user/net ns.
This is not a filesystem, Unix-socket, credential or hostile-code sandbox.
"""
import errno
import json
import os
from pathlib import Path
import socket
import subprocess
import sys

MARKER = 'PRIVATEER_OFFLINE_NETNS'
IP = '/usr/bin/ip'
UNSHARE = '/usr/bin/unshare'


def identity(kind):
    return os.readlink(f'/proc/self/ns/{kind}')


def clean_env():
    # Preserve the real HOME, but never forward proxy, provider or auth env.
    return {'HOME': os.environ['HOME'], 'PATH': '/usr/bin:/bin'}


def ip_json(*args):
    run = subprocess.run([IP, '-json', *args], check=True, capture_output=True,
                         text=True, env=clean_env(), timeout=5)
    return json.loads(run.stdout)


def topology():
    links = ip_json('link', 'show')
    if len(links) != 1 or links[0]['ifname'] != 'lo' or links[0]['link_type'] != 'loopback':
        raise RuntimeError('offline boundary requires ONLY loopback')
    routes = []
    for family in ('-4', '-6'):
        current = ip_json(family, 'route', 'show', 'table', 'all')
        for route in current:
            # Only kernel local-loopback routes are permitted, never a default
            # or arbitrary unicast route (even one pointing at lo).
            if (route.get('dev') != 'lo' or (route.get('type'), route.get('dst')) not in (
                    ('local', '127.0.0.0/8'), ('local', '127.0.0.1'),
                    ('broadcast', '127.255.255.255'), ('local', '::1'))):
                raise RuntimeError(f'offline boundary refuses route: {route}')
        routes += current
    return links, routes


def external_refusal():
    # Topology must already be verified. These documentation-only destinations
    # cannot leave this namespace: connect must fail in the kernel with no route,
    # not timeout, ECONNREFUSED from a peer, or a response from any real service.
    refused = []
    for family, address in ((socket.AF_INET, '192.0.2.1'),
                            (socket.AF_INET6, '2001:db8::1')):
        for kind, label in ((socket.SOCK_STREAM, 'tcp'), (socket.SOCK_DGRAM, 'udp')):
            with socket.socket(family, kind) as sock:
                sock.settimeout(0.5)
                try:
                    sock.connect((address, 9))
                except OSError as error:
                    if error.errno != errno.ENETUNREACH:
                        raise RuntimeError('external probe did not fail with ENETUNREACH') from error
                else:
                    raise RuntimeError('external transport unexpectedly reachable')
            refused.append({'address': address, 'transport': label, 'errno': 'ENETUNREACH'})
    return refused


def require_boundary():
    if os.environ.get(MARKER) != identity('net'):
        raise RuntimeError('verified offline namespace required before native exec')
    links, routes = topology()
    if 'UP' not in links[0]['flags']:
        raise RuntimeError('offline loopback must be up for leakage traps')
    return links, routes, external_refusal()


def launch(command):
    """Return failure on unavailable namespaces/tools; NEVER retry on the host."""
    if not command:
        raise RuntimeError('explicit command required')
    args = [UNSHARE, '--user', '--map-root-user', '--net', '--',
            sys.executable, str(Path(__file__).resolve()), '--child',
            identity('net'), identity('user'), '--', *command]
    return subprocess.run(args, env=clean_env()).returncode


def enter():
    """Called before CLI parsing/native probes by the MCP test entry point."""
    if MARKER not in os.environ:
        raise SystemExit(launch([sys.executable, *sys.argv]))
    require_boundary()


def child(parent_net, parent_user, command):
    if identity('net') == parent_net or identity('user') == parent_user:
        raise RuntimeError('unshare did not create independent user AND network namespaces')
    # Check BEFORE altering lo or attempting any external socket operation.
    topology()
    subprocess.run([IP, 'link', 'set', 'lo', 'up'], check=True,
                   env=clean_env(), timeout=5)
    env = clean_env()
    env[MARKER] = identity('net')
    os.environ.clear()
    os.environ.update(env)
    links, routes, refused = require_boundary()
    print(json.dumps({'offline_boundary': 'linux-user-netns', 'net': identity('net'),
                      'user': identity('user'), 'parent_net': parent_net,
                      'parent_user': parent_user, 'links': links, 'routes': routes,
                      'external_refusal': refused, 'home_preserved': True}), flush=True)
    # No inherited host sockets are passed. Native children inherit the same net
    # namespace; all fake listeners are created by COMMAND after this exec.
    os.execvpe(command[0], command, env)


if __name__ == '__main__':
    try:
        if len(sys.argv) >= 6 and sys.argv[1] == '--child' and sys.argv[4] == '--':
            child(sys.argv[2], sys.argv[3], sys.argv[5:])
        elif len(sys.argv) >= 3 and sys.argv[1] == '--':
            raise SystemExit(launch(sys.argv[2:]))
        else:
            raise RuntimeError('usage: pi_offline_netns.py -- COMMAND [ARG ...]')
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f'offline boundary REFUSED (no host fallback): {error}', file=sys.stderr)
        raise SystemExit(1)
