#!/usr/bin/env python3
"""Opt-in selected-only MCP qualification using installed native Pi, offline.

python3 tests/pi_mcp_native.py /absolute/installed/native/pi
No provider/model options, model prompts, installs, real config or credentials.
HOME stays unchanged; only a disposable PI_CODING_AGENT_DIR/project is used.
"""
import concurrent.futures
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / 'tests/fixtures/pi-mcp'
sys.path.insert(0, str(FIXTURE))
import server as fake

NATIVE = Path(sys.argv.pop(1)).resolve(strict=True) if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else None


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        size = int(self.headers['Content-Length'])
        if size > 65536 or len(self.server.requests) >= 128:
            self.send_error(413)
            return
        request = json.loads(self.rfile.read(size))
        self.server.requests.append(request)
        if 'id' not in request:
            self.send_response(202)
            self.end_headers()
            return
        if request.get('method') == 'tools/call' and request['params']['name'] == 'slow':
            time.sleep(2)
        data = json.dumps({'jsonrpc': '2.0', 'id': request['id'],
                           'result': fake.response(request, self.server.identity)}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        self.send_response(405)
        self.end_headers()

    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, identity):
        self.connections = 0
        self.requests = []
        self.identity = identity
        super().__init__(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    def get_request(self):
        connection = super().get_request()
        self.connections += 1
        return connection

    @property
    def url(self):
        return f'http://127.0.0.1:{self.server_port}/mcp'

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=2)
        assert not self.thread.is_alive()


def same_process(entry):
    try:
        stat = Path(f'/proc/{entry["pid"]}/stat').read_text().split()
        return stat[21] == entry['start']
    except FileNotFoundError:
        return False


class Native(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if NATIVE is None:
            raise unittest.SkipTest('explicit installed native Pi required; skip is not evidence')
        with NATIVE.open('rb') as stream:
            wrapper = stream.read(2) == b'#!'
        if NATIVE.name != 'pi' or wrapper:
            raise RuntimeError('exact native Pi, not an activation wrapper, required')
        probe = subprocess.run([str(NATIVE), '--version'], text=True, capture_output=True,
                               env={'PATH': '/usr/bin:/bin', 'PI_OFFLINE': '1', 'PI_TELEMETRY': '0'}, timeout=5)
        if probe.returncode or probe.stdout.strip() != '1.2.0':
            raise RuntimeError('qualification requires exactly Pi1.2.0')
        with NATIVE.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        print(json.dumps({'native': str(NATIVE), 'version': '1.2.0', 'sha256': digest,
                          'import': '@earendil-works/pi-coding-agent',
                          'api': 'createMcpExtension({loadConfig(ctx): {servers, errors}})'}), flush=True)

    def fixture(self, transport='stdio', operation='normal', identity='selected'):
        with tempfile.TemporaryDirectory(prefix='privateer-pi-mcp-') as tmp:
            base = Path(tmp).resolve()
            repo, agent = base / 'repo', base / 'agent'
            repo.mkdir()
            agent.mkdir()
            selected, ambient = Server(identity), Server('must-not-connect')
            markers = []
            try:
                def stdio(name, mode='normal'):
                    marker = base / (name + '.jsonl')
                    markers.append(marker)
                    return {'command': '/usr/bin/python3', 'args': [str(FIXTURE / 'server.py'), str(marker), identity, mode],
                            'timeout': 0.4, 'exposure': 'codemode'}

                # Both levels are trusted in this disposable fixture. Suppression
                # must come from selected loadConfig, not denied project trust.
                for directory in (agent, repo / '.pi'):
                    directory.mkdir(exist_ok=True)
                    (directory / 'settings.json').write_text(json.dumps({'packages': [], 'defaultProjectTrust': 'always'}))
                    (directory / 'mcp.json').write_text(json.dumps({'mcpServers': {
                        'ambient-stdio': stdio(directory.name + '-ambient'),
                        'ambient-http': {'url': ambient.url}}}))
                    extension = directory / 'extensions'
                    extension.mkdir()
                    (extension / 'ambient.ts').write_text('throw new Error("ambient-extension-loaded");')
                unselected = stdio('unselected')
                # Unselected exists only in this fixture's candidate set, never in
                # the serialized selected callback config or inherited environment.
                candidates = {'selected': stdio('selected', operation if operation in ('disconnected', 'hang-init') else 'normal'),
                              'unselected': unselected}
                config = candidates['selected'] if transport == 'stdio' else {
                    'url': selected.url, 'timeout': 0.4, 'exposure': 'codemode'}
                empty_operations = ('empty', 'load-failure', 'factory-failure',
                                    'server-conflict', 'factory-server-conflict')
                servers = [] if operation in empty_operations else [
                    {'name': 'selected', 'config': config, 'source': '<offline-selected>', 'scope': 'extension'}]
                if operation == 'shutdown-starting':
                    servers[0]['config'] = stdio('starting', 'hang-init')
                (agent / 'auth.json').write_text('{}')
                (agent / 'models.json').write_text('{}')
                input_path = base / 'input.json'
                input_path.write_text(json.dumps({'servers': servers, 'operation': operation, 'identity': identity,
                                                 'conflictConfig': config,
                                                 'authPath': str(agent / 'auth.json'), 'modelsPath': str(agent / 'models.json'),
                                                 'logPath': str(base / 'mcp.log')}))
                env = {'HOME': os.environ['HOME'], 'PATH': '/usr/bin:/bin',
                       'PI_CODING_AGENT_DIR': str(agent), 'PI_OFFLINE': '1',
                       'PI_SKIP_VERSION_CHECK': '1', 'PI_TELEMETRY': '0',
                       'PRIVATEER_MCP_INPUT': str(input_path)}
                args = [str(NATIVE), '--offline', '--no-session', '--no-extensions', '--no-mcp',
                        '--no-skills', '--no-context-files', '--no-prompt-templates', '--no-themes',
                        '-e', str(FIXTURE / 'qualify.ts')]
                if operation == 'factory-failure':
                    broken = base / 'broken.ts'
                    broken.write_text('export default function() { throw new Error("fixture-factory-failed"); }')
                    args += ['-e', str(broken)]
                args += ['--print', '--', '/mcp-qualify']
                before = time.monotonic()
                run = subprocess.run(args, cwd=repo, env=env, text=True,
                                     capture_output=True, timeout=20)
                duration = time.monotonic() - before
                if operation == 'factory-failure':
                    self.assertNotEqual(run.returncode, 0)
                    self.assertIn('fixture-factory-failed', run.stderr)
                    self.assertNotIn('"fixture":"mcp-qualify"', run.stdout)
                    result = {'factoryFailedClosed': True}
                else:
                    self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
                    records = [json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
                    result = next((r for r in records if r.get('fixture') == 'mcp-qualify'), None)
                    self.assertIsNotNone(result, run.stdout + run.stderr)
                    self.assertEqual(result['loads'], 1)
                    self.assertTrue(result['cleanupTwice'])
                    if operation != 'load-failure':
                        self.assertEqual(result['errors'], [])
                self.assertEqual(ambient.connections, 0, 'ambient/unselected accepted TCP connections')
                for marker in markers:
                    if 'ambient' in marker.name or 'unselected' in marker.name:
                        self.assertFalse(marker.exists(), marker.name)
                starts = []
                requests = []
                for marker in markers:
                    if marker.exists():
                        events = [json.loads(line) for line in marker.read_text().splitlines()]
                        starts += [e for e in events if e['event'] == 'start']
                        requests += [e for e in events if e['event'] == 'request']
                deadline = time.monotonic() + 2
                while any(same_process(e) for e in starts) and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertFalse(any(same_process(e) for e in starts), 'payload still alive after native shutdown')
                if operation in empty_operations:
                    self.assertEqual(starts, [])
                    if operation == 'server-conflict':
                        self.assertTrue(result['conflictRejected'])
                    if operation == 'factory-server-conflict':
                        self.assertTrue(result['factoryAliasesAccepted'])
                    self.assertEqual(selected.connections, 0)
                elif transport == 'http':
                    self.assertGreater(selected.connections, 0)
                    self.assertEqual(sum(r.get('method') == 'tools/call' for r in selected.requests), 1)
                else:
                    self.assertEqual(len(starts), 1)
                    if operation in ('normal', 'timeout', 'cancel'):
                        self.assertEqual(sum(r.get('method') == 'tools/call' for r in requests),
                                         1 if operation == 'normal' else 2)
                    if operation == 'cancel':
                        self.assertTrue(any(r['method'] == 'notifications/cancelled' for r in requests))
                print(json.dumps({'case': operation, 'transport': transport, 'identity': identity,
                                  'seconds': round(duration, 3), 'ambient_tcp': ambient.connections,
                                  'selected_tcp': selected.connections, 'stdio_starts': len(starts),
                                  'methods': [r.get('method') for r in (selected.requests if transport == 'http' else requests)],
                                  'payloads_remaining': 0, 'result': result}), flush=True)
                return result
            finally:
                # Failure recovery targets only fixture-owned processes with matching
                # kernel start identity, never the Pi wrapper or unrelated services.
                recovered = []
                for marker in markers:
                    if marker.exists():
                        for line in marker.read_text().splitlines():
                            entry = json.loads(line)
                            if entry['event'] == 'start' and same_process(entry):
                                recovered.append(entry)
                                try:
                                    if entry['pgid'] == entry['pid']:
                                        os.killpg(entry['pgid'], signal.SIGKILL)
                                    else:
                                        os.kill(entry['pid'], signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                deadline = time.monotonic() + 2
                while any(same_process(e) for e in recovered) and time.monotonic() < deadline:
                    time.sleep(0.02)
                selected.close()
                ambient.close()

    def test_stdio_selected_lazy_pipeline_and_colliding_tool_names(self):
        self.fixture()

    def test_streamable_http_selected(self):
        self.fixture('http')

    def test_empty_factory_and_load_failures(self):
        for operation in ('empty', 'factory-failure', 'load-failure', 'server-conflict', 'factory-server-conflict'):
            with self.subTest(operation=operation):
                self.fixture(operation=operation)

    def test_disconnected_timeout_cancel_and_starting_shutdown(self):
        for operation in ('disconnected', 'timeout', 'cancel', 'shutdown-starting'):
            with self.subTest(operation=operation):
                self.fixture(operation=operation)

    def test_concurrent_selection_isolation(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            selected = pool.submit(self.fixture, identity='concurrent-A')
            empty = pool.submit(self.fixture, operation='empty', identity='concurrent-B')
            self.assertTrue(selected.result()['echo']['schema'])
            self.assertTrue(empty.result()['empty'])


if __name__ == '__main__':
    unittest.main()
