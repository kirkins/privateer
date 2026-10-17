#!/usr/bin/env python3
"""Opt-in actual installed Pi CLI inventories, no inference or credentials.

Usage: python3 tests/pi_bundles_native.py /absolute/native/pi
Only this test sets a disposable PI_CODING_AGENT_DIR. HOME is never replaced.
Production loader retains the original native config directory unchanged.
"""
import concurrent.futures
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
                                                          'PI_OFFLINE': '1'}, ROOT, 'Pi', pi_bundles.NEEDED)

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


if __name__ == '__main__':
    unittest.main()
