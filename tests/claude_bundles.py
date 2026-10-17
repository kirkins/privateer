#!/usr/bin/env python3
"""Deterministic bounded Claude launch tests with a fake claude. No model,
network, real HOME or real Claude settings: every test runs in a temporary
HOME, CLAUDE_CONFIG_DIR, XDG and PRIVATEER_FLEET."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'plugins'))
import bundles
import claude_bundles
import native_launch
from native import handle

SESSION = '00000000-0000-4000-8000-000000000001'
ATTENDED = str(ROOT / 'plugins/attended-claude')


class ClaudeBundles(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='privateer-claude-bundles-')
        self.addCleanup(temp.cleanup)
        self.base = Path(temp.name).resolve()
        self.config = self.base / 'home/.claude'
        self.config.mkdir(parents=True)
        self.repo = self.base / 'repo'
        self.ledger = self.base / 'ledger'
        for path in (self.repo, self.ledger, self.base / 'bin'):
            path.mkdir()
        self.binary = self.base / 'bin/claude'
        self.binary.write_bytes(b'\x7fELFfixture')
        self.binary.chmod(0o700)
        isolated = patch.dict(os.environ, {
            'HOME': str(self.base / 'home'), 'CLAUDE_CONFIG_DIR': str(self.config),
            'XDG_CONFIG_HOME': str(self.base / 'xdg'), 'XDG_STATE_HOME': str(self.base / 'xdg-state'),
            'PRIVATEER_FLEET': str(self.base / 'fleet.sqlite'), 'PATH': str(self.base / 'bin')}, clear=True)
        isolated.start()
        self.addCleanup(isolated.stop)
        # Personal hooks and plugins are dropped, never carried.
        self.settings({'model': 'exact', 'effort': 'high', 'hooks': {'SessionStart': []},
                       'enabledPlugins': {'personal@market': True}})
        self.skill = self.make_skill('selected')
        self.registry = {'schema_version': 1, 'pi_version': '1.2.0', 'claude_version': '2.1.300',
                         'integrations': {'reviewed': True, 'extensions': []},
                         'bundles': [{'ref': 'selected@1', 'roles': ['first_mate', 'crew'], 'skills': [self.skill]}]}

    def settings(self, value):
        (self.config / 'settings.json').write_text(json.dumps(value))

    def make_skill(self, name):
        path = self.base / name
        path.mkdir()
        (path / 'SKILL.md').write_text(f'---\nname: {name}\ndescription: Small fixture client.\n---\nRun client.py.\n')
        (path / 'client.py').write_text('print("fixture")\n')
        return {'name': name, 'root': str(path), 'sha256': bundles.tree_digest(path)}

    def plan(self, selected=('selected@1',)):
        return bundles.plan(self.registry, list(selected), 'crew', 'claude')

    def preflight(self, plan=None, args=('--print', '--', 'brief'), **kwargs):
        return claude_bundles.preflight(plan or self.plan(), ['claude', *args], cwd=self.repo, **kwargs)

    def probe(self, version=b'2.1.300 (Claude Code)\n'):
        return patch.object(claude_bundles.subprocess, 'run',
                            return_value=subprocess.CompletedProcess([], 0, version, b''))

    def refuses(self, call):
        with self.probe() as probe, self.assertRaises(ValueError):
            call()
        probe.assert_not_called()

    def test_bounded_flags_selected_skill_and_carried_allowlist(self):
        (self.repo / '.claude/skills/repo-skill').mkdir(parents=True)  # the project's own skills are allowed
        with self.probe() as probe:
            argv = self.preflight(args=['--print', '--model', 'exact', '--add-dir', str(self.ledger), '--', 'brief'])
            empty = self.preflight(self.plan([]))
        self.assertEqual(argv[:4], ['claude', *claude_bundles.FLAGS])
        settings = argv[argv.index('--settings') + 1]
        self.assertEqual(json.loads(settings), {'model': 'exact', 'effort': 'high'})
        self.assertNotIn('personal@market', ' '.join(argv))
        self.assertEqual([argv[i + 1] for i, a in enumerate(argv) if a == '--plugin-dir'], [self.skill['root']])
        self.assertEqual(argv[-7:], ['--print', '--model', 'exact', '--add-dir', str(self.ledger), '--', 'brief'])
        self.assertEqual(empty[:4], ['claude', *claude_bundles.FLAGS])
        self.assertNotIn('--plugin-dir', empty)
        self.assertEqual(probe.call_count, 2)
        self.settings({})
        with self.probe():
            self.assertNotIn('--settings', self.preflight(self.plan([])))

    def test_provider_route_settings_refuse(self):
        plan = self.plan()
        for key in ('env', 'apiKeyHelper', 'awsAuthRefresh', 'forceLoginMethod', 'otelHeadersHelper'):
            self.settings({'model': 'exact', key: 'SENTINEL'})
            with self.subTest(key=key):
                with self.assertRaises(ValueError):
                    self.plan()
                self.refuses(lambda: self.preflight(plan))
        # Known-harmless keys drop; restrictions refuse rather than drop, so a
        # bounded launch never gets more freedom than an unbounded one.
        self.settings({'model': 'exact', 'includeCoAuthoredBy': False, 'theme': 'dark'})
        with self.probe():
            argv = self.preflight(self.plan())
        self.assertEqual(json.loads(argv[argv.index('--settings') + 1]), {'model': 'exact'})
        for key, value in (('permissions', {'deny': ['Bash(rm:*)']}), ('sandbox', {'enabled': True}),
                           ('disableBypassPermissionsMode', 'disable'), ('unknownFutureKey', 1)):
            self.settings({'model': 'exact', key: value})
            with self.subTest(key=key):
                with self.assertRaises(ValueError):
                    self.plan()
                self.refuses(lambda: self.preflight(plan))
        # Carried settings that changed since planning refuse, not drift.
        self.settings({'model': 'other'})
        self.refuses(lambda: self.preflight(plan))
        (self.config / 'settings.json').unlink()
        (self.config / 'settings.json').symlink_to(self.base / 'elsewhere.json')
        self.refuses(lambda: self.preflight(plan))

    def test_added_folder_resources_and_skill_plugin_parts_refuse(self):
        for part in claude_bundles.FOLDER_PARTS:
            path = self.ledger / part
            path.parent.mkdir(parents=True, exist_ok=True)
            path.mkdir()
            for args in (['--add-dir', str(self.ledger)], [f'--add-dir={self.ledger}'],
                         ['--add-dir', str(self.repo), str(self.ledger)]):
                with self.subTest(part=part, args=args):
                    self.refuses(lambda: self.preflight(args=[*args, '--', 'brief']))
            path.rmdir()
        # Relative folders would resolve against Claude's cwd, not the check's.
        for args in (['--add-dir', 'ledger'], ['--add-dir=ledger'], ['--add-dir', str(self.repo), 'ledger']):
            with self.subTest(args=args):
                self.refuses(lambda: self.preflight(args=[*args, '--', 'brief']))
        root = Path(self.skill['root'])
        for part in ('hooks/hooks.json', '.mcp.json', 'bin/tool', 'commands/x.md', '.claude-plugin/plugin.json'):
            path = root / part
            path.parent.mkdir(exist_ok=True)
            path.write_text('{}')
            self.skill['sha256'] = bundles.tree_digest(root)
            with self.subTest(part=part), self.assertRaises(ValueError):
                self.plan()
            path.unlink()
            if path.parent != root:
                path.parent.rmdir()

    def test_caller_overrides_and_session_flags_refuse(self):
        for args in (['--plugin-dir', str(self.base)], ['--plugin-url', 'https://invalid'], ['--settings', '{}'],
                     ['--setting-sources=user'], ['--mcp-config', 'x.json'], ['--bare'], ['--safe-mode'],
                     ['--agents', '{}'], ['--continue'], ['-c'], ['-pc'], ['-r', SESSION], ['--resume', 'latest'],
                     ['--resume=' + SESSION], ['--session-id', SESSION], ['--fork-session'],
                     ['--plugin-dir', ATTENDED]):
            with self.subTest(args=args):
                self.refuses(lambda: self.preflight(args=[*args, '--', 'brief']))
        # Only the runtime's exact headless --resume; attended starts are fresh.
        with self.probe():
            self.assertIn(SESSION, self.preflight(args=['--resume', SESSION, '--', 'brief']))
        self.refuses(lambda: self.preflight(args=['--resume', SESSION, '--', 'brief'], attended=True))

    def test_version_pin_exact_binary_and_plan_kind(self):
        del self.registry['claude_version']
        with self.assertRaises(ValueError):
            self.plan()
        self.assertIn('pi_version', bundles.plan(self.registry, [], 'crew'))
        pi_plan = bundles.plan(self.registry, [], 'crew')
        self.registry['claude_version'] = '2.1.300'
        self.refuses(lambda: self.preflight(pi_plan))
        with self.probe(b'2.1.301 (Claude Code)\n'), self.assertRaises(ValueError):
            self.preflight()
        self.binary.write_text('#!/bin/bash\nmise use -g claude\n')
        self.refuses(self.preflight)

    def test_native_adapter_unbounded_unchanged_bounded_resume_and_attended_adapter(self):
        request = {'method': 'command', 'context': {'repo': str(self.repo), 'title': 'brief', 'control': {}},
                   'options': {'model': 'exact'}}
        legacy = handle('claude', request)['argv']
        self.assertNotIn('--tool-bundles', legacy)
        with patch.object(native_launch.os, 'execvpe') as execute, self.probe() as probe:
            native_launch.launch('claude', ['claude', '--model', 'exact', '--', 'brief'])
        probe.assert_not_called()
        self.assertEqual(execute.call_args.args[1], ['claude', '--model', 'exact', '--', 'brief'])
        request['context']['tool_bundles'] = self.plan()
        request['options']['resume'] = SESSION
        bounded = handle('claude', request)['argv']
        # native_launch.main reads the plan only ahead of --control.
        self.assertEqual([bounded[2], bounded[4], bounded[6]], ['--tool-bundles', '--control', 'claude'])
        self.assertIn(SESSION, bounded)
        with self.assertRaises(ValueError):
            handle('pi', request)
        with patch.object(native_launch.os, 'execvpe') as execute, self.probe():
            native_launch.launch('claude', ['claude', '--plugin-dir', ATTENDED, '--', 'brief'],
                                 bundles=self.plan(), attended=True)
        argv = execute.call_args.args[1]
        self.assertEqual(argv.count(ATTENDED), 1)
        self.assertEqual(argv.count('--plugin-dir'), 2)
        self.assertIn('--strict-mcp-config', argv)
        for attended, flags in ((False, ['--plugin-dir', ATTENDED]), (True, ['--plugin-dir', ATTENDED + '.spoof']),
                                (True, ['--plugin-dir', ATTENDED, '--plugin-dir', ATTENDED])):
            with self.subTest(attended=attended, flags=flags), patch.object(native_launch.os, 'execvpe'):
                self.refuses(lambda: native_launch.launch('claude', ['claude', *flags, '--', 'brief'],
                                                          bundles=self.plan(), attended=attended))


if __name__ == '__main__':
    unittest.main()
