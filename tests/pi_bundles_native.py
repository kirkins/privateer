#!/usr/bin/env python3
"""Opt-in actual installed Pi CLI inventories, no inference or credentials.

Usage: python3 tests/pi_bundles_native.py /absolute/native/pi
Only this test sets a disposable PI_CODING_AGENT_DIR. HOME is never replaced.
Production loader retains the original native config directory unchanged.
"""
import concurrent.futures
import copy
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'plugins'))
import bundles
import pi_bundles

NATIVE = Path(sys.argv.pop(1)).absolute() if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else None
SENTINEL = 'UNSELECTED_SECRET_SENTINEL_82d9'
SENTINEL_TOKEN = 'SENTINEL-' + '5f0c2d8e41b94a7c9e3a6d1b8f2e0c7a'


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.server.requests.append(self.path)
        self.send_response(500)
        self.end_headers()

    def do_GET(self):
        self.server.requests.append(self.path)
        if self.path == '/slow':
            time.sleep(0.7)
        status = {'/redirect': 302, '/error': 500}.get(self.path, 200)
        self.send_response(status)
        if status == 302:
            self.send_header('Location', '/must-not-follow')
        self.end_headers()
        data = {'count': 3, 'private': SENTINEL}
        try:
            self.wfile.write((b'x' * 17000) if self.path == '/large' else json.dumps(data).encode())
        except BrokenPipeError:
            pass


class FixtureHTTPServer(http.server.ThreadingHTTPServer):
    def __init__(self, *args):
        self.requests = []
        self.connections = 0
        super().__init__(*args)

    def get_request(self):
        accepted = super().get_request()
        self.connections += 1
        return accepted

# Selected-MCP fakes: an authless-by-default MCP endpoint that requires one Bearer
# token, and a model endpoint that only counts connections (a gate that passes
# reaches it; a gate that refuses never does). Both loopback, both per test.
sys.path.insert(0, str(ROOT / 'tests/fixtures/pi-mcp'))
import server as mcp_fixture
STDIO_SERVER = ROOT / 'tests/fixtures/pi-mcp/server.py'


class McpHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.seen.append((self.headers.get('Authorization') == 'Bearer ' + self.server.token, body.get('method')))
        if self.headers.get('Authorization') != 'Bearer ' + self.server.token:
            self.send_response(401)
            self.end_headers()
            return
        if 'id' not in body:
            self.send_response(202)
            self.end_headers()
            return
        result = {'tools': []} if body.get('method') == 'tools/list' and self.server.mode == 'no-tools' \
            else mcp_fixture.response(body, 'selected')
        data = json.dumps({'jsonrpc': '2.0', 'id': body['id'], 'result': result}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.send_response(405)
        self.end_headers()


class CountingServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, handler, **attributes):
        super().__init__(('127.0.0.1', 0), handler)
        self.connections = 0
        self.seen = []
        self.__dict__.update(attributes)
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def get_request(self):
        self.connections += 1
        return super().get_request()

    @property
    def url(self):
        return f'http://127.0.0.1:{self.server_port}'


class ModelHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', 0)))
        self.send_response(500)
        self.end_headers()

    do_GET = do_POST


class Native(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if NATIVE is None:
            raise unittest.SkipTest('pass an exact installed native Pi binary to opt in; not inventory evidence')
        if NATIVE.name != 'pi' or not NATIVE.is_file():
            raise RuntimeError('exact native Pi required')
        # The version this run actually qualifies: the native binary's own report,
        # after it declares every bounded-loading flag.
        cls.version = bundles.require_flags(str(NATIVE), {'HOME': os.environ['HOME'], 'PATH': os.environ['PATH'],
                                                          'PI_OFFLINE': '1'}, ROOT, 'Pi', pi_bundles.NEEDED,
                                                 help_args=pi_bundles.HELP)

    def setUp(self):
        # Keep the fixture below an owned, non-sticky ancestor. In an unprivileged
        # user namespace host-root /tmp has an unmapped UID; production safe_path
        # correctly refuses it. Do not weaken that check for qualification.
        self.temp = tempfile.TemporaryDirectory(prefix='privateer-pi-inventory-', dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.repo = self.base / 'repo'
        self.repo.mkdir()
        self.agent = self.base / 'agent'
        self.agent.mkdir()
        self.server = FixtureHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.url = f'http://127.0.0.1:{self.server.server_port}'
        self.client = ROOT / 'tests/fixtures/pi-bundles/client.py'
        self.env = {'HOME': os.environ['HOME'], 'PATH': f'{NATIVE.parent}:/usr/bin:/bin',
                    'PI_CODING_AGENT_DIR': str(self.agent), 'PI_OFFLINE': '1', 'PI_SKIP_VERSION_CHECK': '1',
                    'PI_TELEMETRY': '0', 'PRIVATEER_FIXTURE_ENDPOINT': self.url + '/lookup'}
        self.selected = self.skill(self.base / 'selected', 'selected-fixture')
        self.unselected = self.skill(self.base / 'unselected', 'unselected-fixture')
        shutil.copyfile(self.client, Path(self.selected['root']) / 'client.py')
        self.selected['sha256'] = bundles.tree_digest(self.selected['root'])
        inspector = ROOT / 'tests/fixtures/pi-bundles/inventory.mjs'
        self.registry = {'schema_version': 1, 'pi_version': self.version,
                         'integrations': {'reviewed': True, 'extensions': [
                             {'path': str(inspector), 'sha256': hashlib.sha256(inspector.read_bytes()).hexdigest()}]},
                         'bundles': [{'ref': 'selected@1', 'roles': ['first_mate', 'crew'], 'skills': [self.selected]},
                                     {'ref': 'unselected@1', 'roles': ['crew'], 'skills': [self.unselected]}]}
        self.seed_ambient(self.agent)
        self.seed_ambient(self.repo / '.pi')
        # Synthetic auth only: never inspect/copy any machine credential store.
        (self.agent / 'auth.json').write_text('{}')
        (self.agent / 'settings.json').write_text(json.dumps({'defaultProjectTrust': 'always', 'packages': []}))

    def skill(self, root, name):
        root.mkdir(parents=True)
        (root / 'SKILL.md').write_text(f'---\nname: {name}\ndescription: Small auth-free fixture API client.\n---\nRead client.py and run it only when needed.\n')
        return {'name': name, 'root': str(root), 'sha256': bundles.tree_digest(root)}

    def seed_ambient(self, directory):
        directory.mkdir(exist_ok=True)
        self.skill(directory / 'skills/ambient', 'ambient-fixture')
        (directory / 'AGENTS.md').write_text('AMBIENT_CONTEXT_SENTINEL')
        (directory / 'prompts').mkdir()
        (directory / 'prompts/ambient.md').write_text('AMBIENT_CONTEXT_SENTINEL')
        extensions = directory / 'extensions'
        extensions.mkdir()
        marker = directory / 'extension-connected'
        (extensions / 'ambient.mjs').write_text(
            'import fs from "node:fs"; export default function(pi) { fs.writeFileSync('
            + json.dumps(str(marker)) + ', "connected"); pi.registerCommand("ambient-command", {description:"ambient",handler:async()=>{}}); }')
        mcp_marker = directory / 'mcp-connected'
        server = directory / 'ambient-server.py'
        server.write_text('from pathlib import Path\nPath(' + repr(str(mcp_marker)) + ').write_text("connected")\n')
        (directory / 'mcp.json').write_text(json.dumps({'mcpServers': {
            'ambient-stdio': {'command': 'python3', 'args': [str(server)]},
            'ambient-http': {'url': self.url + '/mcp', 'headers': {'Authorization': SENTINEL}}}}))

    def run_inventory(self, selection, attended=False, hosted_crew=False):
        plan = bundles.plan(self.registry, selection, 'first_mate' if attended else 'crew')
        env = dict(self.env)
        if hosted_crew:
            env['PRIVATEER_HOSTED_CREW'] = '1'
        if selection:
            env['PRIVATEER_FIXTURE_CLIENT'] = str(Path(self.selected['root']) / 'client.py')
        args = [sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles', json.dumps(plan)]
        if attended:
            args += ['--attended-policy', '{"mode":"standard","claude_allow":[]}']
        args += ['pi', 'pi']
        if attended:
            args += ['-e', str(ROOT / 'plugins/attended/mail.ts')]
        if hosted_crew:
            args += ['-e', str(ROOT / 'plugins/attended/crew.ts')]
        # --print executes the extension command, not a user/model prompt.
        # No JSON-mode completion wrapper: an inventory command has no model turn.
        args += ['--offline', '--no-session', '--print', '--', '/bundle-inventory']
        run = subprocess.run(args, cwd=self.repo, env=env, text=True, capture_output=True, timeout=15)
        self.assertEqual(run.returncode, 0, run.stderr[:1000])
        self.assertNotIn(SENTINEL, run.stdout + run.stderr)
        records = [json.loads(line) for line in run.stdout.splitlines() if line.startswith('{')]
        result = next(value for value in records if value.get('fixture') == 'bundle-inventory')
        self.assertFalse(result['ambient'])
        self.assertFalse(result['unselected'])
        self.assertFalse(result['instructionsEager'])
        self.assertFalse(any(name.startswith('mcp__') for name in result['tools']))
        self.assertNotIn('ambient-command', result['commands'])
        self.assertLess(result['promptChars'], 20_000)
        for directory in (self.agent, self.repo / '.pi'):
            self.assertFalse((directory / 'extension-connected').exists())
            self.assertFalse((directory / 'mcp-connected').exists())
        self.assertNotIn('/mcp', self.server.requests)
        return result

    def test_selected_empty_attended_and_concurrent_actual_inventory(self):
        selected = self.run_inventory(['selected@1'])
        self.assertTrue(selected['selected'])
        self.assertEqual(json.loads(selected['script']['stdout']), {'ok': True, 'count': 3})
        self.assertEqual(selected['script']['code'], 0)
        empty = self.run_inventory([])
        self.assertFalse(empty['selected'])
        self.assertIsNone(empty['script'])
        attended = self.run_inventory(['selected@1'], attended=True)
        self.assertTrue(attended['selected'])
        self.assertEqual(selected['tools'], attended['tools'])
        self.assertEqual(selected['commands'], attended['commands'])
        hosted = self.run_inventory(['selected@1'], hosted_crew=True)
        self.assertTrue(hosted['selected'])
        self.assertEqual(selected['tools'], hosted['tools'])
        self.assertEqual(selected['commands'], hosted['commands'])
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            a = pool.submit(self.run_inventory, ['selected@1'])
            b = pool.submit(self.run_inventory, [])
            self.assertTrue(a.result()['selected'])
            self.assertFalse(b.result()['selected'])
        self.assertEqual(self.server.requests.count('/lookup'), 4)
        self.assertEqual(self.server.connections, 4, 'no additional/idle ambient MCP TCP connections')
        print(json.dumps({'native_pi': self.version, 'inventory': 'selected/empty/attended/hosted-crew/concurrent',
                          'ambient_mcp_connections': 0, 'api_calls': self.server.requests.count('/lookup')}))

    def test_required_extension_factory_failure_refuses_without_agent_turn(self):
        broken = self.base / 'broken.mjs'
        broken.write_text('export default function(pi) { throw new Error("required-integration-unavailable"); }')
        self.registry['integrations']['extensions'].append({
            'path': str(broken), 'sha256': hashlib.sha256(broken.read_bytes()).hexdigest()})
        plan = bundles.plan(self.registry, [], 'crew')
        run = subprocess.run([sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles',
                              json.dumps(plan), 'pi', 'pi', '--offline', '--no-session', '--print', '--', '/bundle-inventory'],
                             cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=15)
        self.assertNotEqual(run.returncode, 0)
        self.assertNotIn('"fixture":"bundle-inventory"', run.stdout.replace(' ', ''))
        self.assertIn('required-integration-unavailable', run.stderr)
        self.assertEqual(self.server.requests, [])
        self.assertEqual(self.server.connections, 0)

    def test_api_script_errors_timeouts_redirects_and_redaction(self):
        for path in ('/redirect', '/error', '/slow', '/large'):
            run = subprocess.run([sys.executable, str(self.client), self.url + path], text=True,
                                 capture_output=True, env={'PATH': '/usr/bin:/bin'}, timeout=2)
            self.assertEqual(run.returncode, 2)
            self.assertEqual(json.loads(run.stdout), {'ok': False, 'error': 'lookup-unavailable'})
            self.assertNotIn(SENTINEL, run.stdout + run.stderr)
        self.assertNotIn('/must-not-follow', self.server.requests)


    def run_selected(self, token, mode='normal', rogue_mode=None, timeout_seconds=1):
        """One native Pi run that selects the docs MCP server. The gate either
        passes (the model endpoint is reached) or refuses with exit 78 first."""
        mcp = CountingServer(McpHandler, token=SENTINEL_TOKEN, mode=mode)
        self.addCleanup(mcp.server_close)
        self.addCleanup(mcp.shutdown)
        model = CountingServer(ModelHandler)
        self.addCleanup(model.server_close)
        self.addCleanup(model.shutdown)
        # One fleet (and credential directory) per run: a test may run several.
        self.runs = getattr(self, 'runs', 0) + 1
        fleet = self.base / f'run{self.runs}' / 'fleet.sqlite'
        directory = fleet.with_suffix('.credentials')
        fleet.parent.mkdir()
        directory.mkdir(mode=0o700)
        (directory / 'docs-search').write_text(token + '\n')
        (directory / 'docs-search').chmod(0o600)
        (self.agent / 'models.json').write_text(json.dumps({'providers': {'spike': {
            'baseUrl': f'{model.url}/v1', 'api': 'openai-completions', 'apiKey': 'unused',
            'models': [{'id': 'm'}]}}}))
        registry = copy.deepcopy(self.registry)
        # The inventory integration exits at agent_start: it would end a run before its model turn.
        registry['integrations']['extensions'] = []
        registry['bundles'].append({'ref': 'docs@1', 'roles': ['crew'], 'skills': [],
                                    'credentials': [{'name': 'DOCS_TOKEN', 'ref': 'docs-search'}],
                                    'mcp_servers': [{'name': 'docs', 'url': f'{mcp.url}/mcp', 'credential': 'DOCS_TOKEN',
                                                     'timeout': timeout_seconds, 'description': 'Fixture docs'}]})
        if rogue_mode:
            # An operator-approved integration that registers a server of its own: at
            # load time ('startup') or when the first prompt starts ('first-prompt').
            rogue = self.base / 'rogue.mjs'
            register = 'pi.registerMcpServer("rogue", {url: "http://127.0.0.1:9/mcp", timeout: 1, exposure: "direct"});'
            body = f'pi.on("before_agent_start", () => {{ {register} }});' if rogue_mode == 'first-prompt' else register
            rogue.write_text(f'export default function(pi) {{ {body} }}')
            registry['integrations']['extensions'].append({
                'path': str(rogue), 'sha256': hashlib.sha256(rogue.read_bytes()).hexdigest()})
        plan = bundles.plan(registry, ['docs@1'], 'crew')
        env = dict(self.env, PRIVATEER_FLEET=str(fleet))
        args = [sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles', json.dumps(plan),
                'pi', 'pi', '--offline', '--no-session', '--model', 'spike/m', '--print', '--', 'hello']
        run = subprocess.run(args, cwd=self.repo, env=env, text=True, capture_output=True, stdin=subprocess.DEVNULL,
                             timeout=60)
        leaks = [p for p in self.base.rglob('*') if p.is_file() and p.name != 'docs-search'
                 and SENTINEL_TOKEN.encode() in p.read_bytes()]
        self.assertEqual(leaks, [], 'token must never reach a log, config or registry file')
        self.assertNotIn(SENTINEL_TOKEN, run.stdout + run.stderr)
        return run, mcp, model

    def test_selected_mcp_gate_passes_only_with_a_valid_bearer(self):
        run, mcp, model = self.run_selected(SENTINEL_TOKEN)
        # The gate passed: the run reached a model turn, after the server listed its tools.
        self.assertGreater(model.connections, 0, (run.returncode, run.stdout[-400:], run.stderr[-600:]))
        self.assertIn('tools/list', [method for ok, method in mcp.seen if ok])
        self.assertTrue(all(ok for ok, _method in mcp.seen))
        self.assertGreater(mcp.connections, 0)
        print(json.dumps({'case': 'selected-valid-bearer', 'native_pi': self.version, 'mcp_requests': len(mcp.seen),
                          'model_connections': model.connections, 'exit': run.returncode}), flush=True)

    def test_selected_mcp_unready_servers_exit_78_before_any_model_turn(self):
        cases = {
            'bad-token': (f'WRONG-{SENTINEL_TOKEN}', 'normal', None),
            'no-tools': (SENTINEL_TOKEN, 'no-tools', None),
            'extension-registered': (SENTINEL_TOKEN, 'normal', 'startup'),
            'registered-at-first-prompt': (SENTINEL_TOKEN, 'normal', 'first-prompt'),
        }
        for label, (token, mode, rogue_mode) in cases.items():
            with self.subTest(case=label):
                run, mcp, model = self.run_selected(token, mode=mode, rogue_mode=rogue_mode)
                self.assertEqual(run.returncode, 78, run.stderr[-600:])
                self.assertEqual(model.connections, 0, 'a refused gate must not reach a model')
                self.assertIn('MCP server "', run.stderr)
                if rogue_mode:
                    self.assertIn('"rogue"', run.stderr)
                else:
                    self.assertIn('"docs"', run.stderr)

    def test_selected_mcp_token_with_expansion_marker_refuses_before_any_request(self):
        run, mcp, model = self.run_selected(f'{SENTINEL_TOKEN}$HOME')
        self.assertEqual(run.returncode, 78, run.stderr[-600:])
        self.assertEqual(model.connections, 0)
        self.assertEqual(mcp.seen, [], 'a token Pi could expand must never be sent')


    def test_selected_stdio_server_env_ambient_and_lifetime(self):
        """A pinned stdio server: the gate passes once its tools list, ambient
        stdio servers never start, and the fake records the environment names
        Pi passes it. Reports which of its processes outlive Pi; the job's
        teardown ends those (tests/tool_bundles.rs proves it)."""
        model = CountingServer(ModelHandler)
        self.addCleanup(model.server_close)
        self.addCleanup(model.shutdown)
        fleet = self.base / 'stdio/fleet.sqlite'
        directory = fleet.with_suffix('.credentials')
        fleet.parent.mkdir()
        directory.mkdir(mode=0o700)
        (directory / 'stdio-token').write_text(SENTINEL_TOKEN + '\n')
        (directory / 'stdio-token').chmod(0o600)
        (self.agent / 'models.json').write_text(json.dumps({'providers': {'spike': {
            'baseUrl': f'{model.url}/v1', 'api': 'openai-completions', 'apiKey': 'unused',
            'models': [{'id': 'm'}]}}}))
        # Claude's project file too: neither harness may start it.
        ambient = self.base / 'ambient-project.jsonl'
        (self.repo / '.mcp.json').write_text(json.dumps({'mcpServers': {'ambient-project': {
            'command': sys.executable, 'args': [str(STDIO_SERVER), str(ambient), 'ambient', 'normal']}}}))
        python = os.path.realpath(sys.executable)
        marker = self.base / 'stdio.jsonl'
        registry = copy.deepcopy(self.registry)
        registry['integrations']['extensions'] = []
        registry['bundles'].append({'ref': 'stdio@1', 'roles': ['crew'],
                                    'credentials': [{'name': 'STDIO_TOKEN', 'ref': 'stdio-token'}],
                                    'mcp_servers': [{'name': 'fixture', 'credential': 'STDIO_TOKEN', 'timeout': 10,
                                                     'command': [python, '-I', str(STDIO_SERVER), str(marker),
                                                                 'selected', 'stubborn'],
                                                     'sha256': hashlib.sha256(Path(python).read_bytes()).hexdigest()}]})
        plan = bundles.plan(registry, ['stdio@1'], 'crew')
        env = dict(self.env, PRIVATEER_FLEET=str(fleet), PRIVATEER_PROBE_AMBIENT='1')
        args = [sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles', json.dumps(plan),
                'pi', 'pi', '--offline', '--no-session', '--model', 'spike/m', '--print', '--', 'hello']
        run = subprocess.run(args, cwd=self.repo, env=env, text=True, capture_output=True, stdin=subprocess.DEVNULL,
                             timeout=60)
        events = [json.loads(line) for line in marker.read_text().splitlines()] if marker.exists() else []
        survivors = mcp_fixture.outliving(events)
        self.assertGreater(model.connections, 0, (run.returncode, run.stdout[-400:], run.stderr[-600:]))
        starts = [e for e in events if e['event'] == 'start']
        self.assertTrue(starts)
        self.assertIn('tools/list', [e.get('method') for e in events if e['event'] == 'request'])
        for directory in (self.agent, self.repo / '.pi'):
            self.assertFalse((directory / 'mcp-connected').exists())
        self.assertFalse(ambient.exists())
        names = starts[0]['env']
        self.assertIn('PRIVATEER_CREDENTIAL_STDIO_TOKEN_FILE', names)
        self.assertNotIn(SENTINEL_TOKEN, marker.read_text() + run.stdout + run.stderr)
        print(json.dumps({'case': 'selected-stdio', 'native_pi': self.version, 'exit': run.returncode,
                          'starts': len(starts), 'env_names': names,
                          'outliving_pi': [e['event'] for e in survivors]}), flush=True)


if __name__ == '__main__':
    unittest.main()
