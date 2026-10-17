#!/usr/bin/env python3
"""Opt-in bounded Claude inventory probe using the installed native binary.

Usage: python3 tests/claude_bundles_native.py /absolute/native/claude
Offline: ANTHROPIC_BASE_URL is a dead loopback port and the API key is a
fixture string, so no model call can succeed. Reads the system/init event,
then kills the process. A disposable HOME/CLAUDE_CONFIG_DIR/XDG holds seeded
personal resources which must not load; real user state is never touched.
Without an explicit binary the suite skips, and a skip is not evidence.
"""
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'plugins'))
import bundles
import claude_bundles

NATIVE = Path(sys.argv.pop(1)).absolute() if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else None


def skill(path, name):
    path.mkdir(parents=True)
    (path / 'SKILL.md').write_text(f'---\nname: {name}\ndescription: Fixture skill {name}.\n---\nFixture.\n')


class NativeInventory(unittest.TestCase):
    def setUp(self):
        if NATIVE is None:
            raise unittest.SkipTest('explicit installed native Claude required; skip is not evidence')
        temp = tempfile.TemporaryDirectory(prefix='privateer-claude-native-')
        self.addCleanup(temp.cleanup)
        base = Path(temp.name).resolve()
        config = base / 'home/.claude'
        self.marker = base / 'ambient-hook-ran'
        # Seeded personal resources: a skill, a hook and an MCP server.
        skill(config / 'skills/personal-seed', 'personal-seed')
        (config / 'settings.json').write_text(json.dumps({'model': 'sonnet', 'hooks': {'SessionStart': [
            {'hooks': [{'type': 'command', 'command': f'touch {self.marker}'}]}]}}))
        (config / '.claude.json').write_text(json.dumps({'mcpServers': {
            'ambient-seed': {'type': 'stdio', 'command': '/bin/false'}}}))
        self.repo = base / 'repo'
        skill(self.repo / '.claude/skills/repo-seed', 'repo-seed')
        root = base / 'selected'
        skill(root, 'selected')
        self.env = {'HOME': str(base / 'home'), 'CLAUDE_CONFIG_DIR': str(config),
                    'XDG_CONFIG_HOME': str(base / 'xdg'), 'XDG_STATE_HOME': str(base / 'xdg-state'),
                    'XDG_CACHE_HOME': str(base / 'xdg-cache'), 'PRIVATEER_FLEET': str(base / 'fleet.sqlite'),
                    'PATH': f'{NATIVE.parent}:/usr/bin:/bin', 'ANTHROPIC_BASE_URL': 'http://127.0.0.1:9',
                    'ANTHROPIC_API_KEY': 'offline-fixture-not-a-key',
                    'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_AUTOUPDATER': '1'}
        version = subprocess.run([str(NATIVE), '--version'], env=self.env, capture_output=True, timeout=30)
        registry = {'schema_version': 1, 'pi_version': '1.2.0', 'claude_version': version.stdout.split()[0].decode(),
                    'integrations': {'reviewed': True, 'extensions': []},
                    'bundles': [{'ref': 'selected@1', 'roles': ['crew'], 'skills': [
                        {'name': 'selected', 'root': str(root), 'sha256': bundles.tree_digest(root)}]}]}
        with patch.dict(os.environ, self.env, clear=True):
            self.plan = bundles.plan(registry, ['selected@1'], 'crew', 'claude')

    def init(self):
        argv = claude_bundles.preflight(self.plan, [
            'claude', '--print', '--verbose', '--output-format', 'stream-json', '--include-hook-events',
            '--no-session-persistence', '--', 'Reply with one word.'], cwd=self.repo, env=self.env)
        process = subprocess.Popen(argv, cwd=self.repo, env=self.env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        events, init = [], None
        try:
            reader = selectors.DefaultSelector()
            reader.register(process.stdout, selectors.EVENT_READ)
            deadline = time.monotonic() + 60
            # Read to init, then briefly on for any ambient hook events.
            while time.monotonic() < deadline and reader.select(timeout=1) is not None:
                line = process.stdout.readline()
                if not line:
                    break
                event = json.loads(line)
                events.append(event)
                if event.get('type') == 'system' and event.get('subtype') == 'init':
                    init = event
                    deadline = min(deadline, time.monotonic() + 3)
        finally:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        self.assertIsNotNone(init, 'no system/init event')
        return init, events

    def test_selected_only_inventory(self):
        init, events = self.init()
        observed = {key: init.get(key) for key in ('skills', 'plugins', 'mcp_servers', 'model')}
        print(json.dumps(observed, indent=1), file=sys.stderr)
        skills = [name.split(':')[-1] for name in init['skills']]
        self.assertIn('selected', skills)
        self.assertIn('repo-seed', skills, "the project's own skills are allowed")
        self.assertNotIn('personal-seed', skills)
        # Claude's built-in plugins and skills load in every mode.
        self.assertEqual([p['name'] for p in init['plugins'] if p.get('path') != 'builtin'], ['selected'])
        self.assertEqual(init['mcp_servers'], [])
        self.assertEqual(init.get('model', '').split('-')[1:2], ['sonnet'], 'carried model setting')
        self.assertFalse([e for e in events if 'hook' in str(e.get('subtype', ''))])
        self.assertFalse(self.marker.exists())


if __name__ == '__main__':
    unittest.main()
