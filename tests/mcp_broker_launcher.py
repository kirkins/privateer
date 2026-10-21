#!/usr/bin/env python3
"""The broker's proxy as a crew bundle runs it: a pinned copy, started by the
step-1 stdio launcher (`python3 -I plugins/mcp_stdio.py SPEC`), completes a
tools/call against a broker serving the fake mcpbridge.

tests/mcp_broker.rs runs this with MCP_BROKER_BIN (the built broker) set."""
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'plugins'))
import bundles  # noqa: E402
import stdio_pins  # noqa: E402

FAKE = ROOT / 'tests/fixtures/mcp-broker/fake_mcpbridge.py'
LAUNCHER = ROOT / 'plugins/mcp_stdio.py'


def end(process, stop):
    stop()
    process.wait(10)
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream:
            stream.close()


def read_line(stream, deadline):
    while time.monotonic() < deadline:
        ready, _, _ = select.select([stream], [], [], 0.2)
        if ready:
            return stream.readline()
    raise AssertionError('no answer in time')


class Launcher(unittest.TestCase):
    def test_a_pinned_proxy_completes_a_tool_call_through_the_launcher(self):
        private = stdio_pins.private_directory(self)
        pinned = private / 'privateer-mcp-broker'
        shutil.copyfile(os.environ['MCP_BROKER_BIN'], pinned)
        pinned.chmod(0o755)
        try:
            digest = bundles.executable(str(pinned))
        except bundles.Refused:
            self.skipTest(f'the executable rule refuses the copy: {stdio_pins.refused_part(pinned)}')
        state = tempfile.mkdtemp(prefix='broker-launcher-')
        self.addCleanup(shutil.rmtree, state, True)
        socket = os.path.join(state, 's', 'xcode.sock')
        serve = subprocess.Popen(
            [str(pinned), 'serve', '--state-dir', os.path.join(state, 's'), '--name', 'xcode', '--',
             os.path.realpath(sys.executable), '-I', str(FAKE)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        self.addCleanup(end, serve, serve.terminate)
        deadline = time.monotonic() + 10
        while not os.path.exists(socket):
            self.assertIsNone(serve.poll(), serve.stderr.read() if serve.poll() is not None else '')
            self.assertLess(time.monotonic(), deadline, 'the broker did not listen')
            time.sleep(0.05)
        spec = json.dumps({'command': [str(pinned), 'proxy', '--socket', socket, '--connect-wait-secs', '10'],
                           'sha256': digest})
        # Unbuffered: select() sees every line, none hidden in a read buffer.
        client = subprocess.Popen([os.path.realpath(sys.executable), '-I', str(LAUNCHER), spec], bufsize=0,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  env={'HOME': os.environ.get('HOME', '/'), 'PATH': '/usr/bin:/bin'})
        self.addCleanup(end, client, client.kill)
        for message in (
            {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
                'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 't', 'version': '1'}}},
            {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
            {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call',
             'params': {'name': 'echo', 'arguments': {'text': 'through the launcher'}}},
        ):
            client.stdin.write((json.dumps(message) + '\n').encode())
        client.stdin.flush()
        deadline = time.monotonic() + 20
        answers = {}
        while 2 not in answers:
            line = read_line(client.stdout, deadline)
            self.assertTrue(line, client.stderr.read().decode() if client.poll() is not None else 'closed')
            message = json.loads(line)
            answers[message.get('id')] = message
        self.assertEqual(answers[1]['result']['serverInfo']['name'], 'fake-mcpbridge')
        self.assertEqual(answers[2]['result']['content'][0]['text'], 'through the launcher')
        self.assertFalse(answers[2]['result']['isError'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
