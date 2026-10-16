#!/usr/bin/env python3
"""Test-only, auth-free API script. Fixed loopback boundary and redacted failures.

This is a fixture, not a generic credential broker or a production API adapter.
"""
import http.client
import ipaddress
import json
import sys
from urllib.parse import urlsplit


def lookup(url):
    parsed = urlsplit(url)
    if (parsed.scheme != 'http' or not parsed.hostname or parsed.username or parsed.password
            or not ipaddress.ip_address(parsed.hostname).is_loopback or parsed.fragment):
        raise ValueError('unsupported endpoint')
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=0.4)
    try:
        connection.request('GET', parsed.path or '/')
        response = connection.getresponse()
        if response.status != 200:
            raise ValueError('request refused')  # Never follow redirects or echo server errors.
        body = response.read(16_385)
        if len(body) > 16_384:
            raise ValueError('response too large')
        data = json.loads(body)
        count = data['count']
        if type(count) is not int or not 0 <= count <= 100:
            raise ValueError('invalid response')
        return {'ok': True, 'count': count}  # Project only bounded typed facts.
    finally:
        connection.close()


if __name__ == '__main__':
    try:
        print(json.dumps(lookup(sys.argv[1]), separators=(',', ':')))
    except Exception:
        print('{"ok":false,"error":"lookup-unavailable"}')
        raise SystemExit(2)
