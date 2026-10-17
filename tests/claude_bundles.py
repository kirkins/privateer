#!/usr/bin/env python3
"""Deterministic bounded Claude launch tests with a fake claude. No model,
network, real HOME or real Claude settings: every test runs in a temporary
HOME, CLAUDE_CONFIG_DIR, XDG and PRIVATEER_FLEET."""
import json
import os
from pathlib import Path
import re
import shutil
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

# Option rows as Claude Code's --help lists them, keyed by the flag each declares.
CLAUDE_ROWS = {
    '--setting-sources': '  --setting-sources <sources>           Comma-separated list of setting sources',
    '--strict-mcp-config': '  --strict-mcp-config                   Only use MCP servers from --mcp-config',
    '--settings': '  --settings <file-or-json>             Path to a settings JSON file or a JSON string',
    '--plugin-dir': '  --plugin-dir <path>                   Load a plugin from a directory or .zip',
    '--print': '  -p, --print                           Print response and exit',
    '--add-dir': '  --add-dir <directories...>            Additional directories to allow tool access to',
    '--resume': '  -r, --resume [value]                  Resume a conversation by session ID',
}


def native_on_path(name):
    """The CLI on PATH, unless it is a #! wrapper: a wrapper can run a state-changing
    activation (for example `mise use -g`), so the installed-CLI checks never run one."""
    found = shutil.which(name)
    with open(found, 'rb') as stream:
        return None if found is None or stream.read(2) == b'#!' else found


def claude_help(omit=(), extra=()):
    rows = [row for flag, row in CLAUDE_ROWS.items() if flag not in omit]
    return '\n'.join(['Usage: claude [options] [command] [prompt]', '', 'Options:', *rows, *extra]) + '\n'


# Resolved before setUp isolates PATH to the fake claude.
INSTALLED_CLAUDE = native_on_path('claude')

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

    def probe(self, version=b'2.1.300 (Claude Code)\n', help_text=None):
        """Fake `claude` answering --version and --help, as the capability probe asks."""
        help_text = claude_help() if help_text is None else help_text
        def run(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 0, help_text.encode() if argv[-1] == '--help' else version, b'')
        return patch.object(bundles.subprocess, 'run', side_effect=run)

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
        self.assertEqual(probe.call_count, 3)  # --version twice, --help once (cached)
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

    def test_plan_kind_and_wrapper_refuse(self):
        del self.registry['claude_version']
        with self.assertRaises(ValueError):
            self.plan()
        self.assertIn('pi_version', bundles.plan(self.registry, [], 'crew'))
        pi_plan = bundles.plan(self.registry, [], 'crew')
        self.registry['claude_version'] = '2.1.300'
        self.refuses(lambda: self.preflight(pi_plan))
        self.binary.write_text('#!/bin/bash\nmise use -g claude\n')
        self.refuses(self.preflight)

    def test_version_is_a_record_not_a_pin_flags_are(self):
        # The registry's claude_version no longer has to match what is installed.
        for version in (b'2.1.300 (Claude Code)\n', b'2.1.301 (Claude Code)\n', b'3.0.0 (Claude Code)\n'):
            with self.subTest(version=version), self.probe(version):
                self.assertIn('--strict-mcp-config', self.preflight())

    def test_missing_flag_refuses_and_names_it(self):
        for missing in ('--strict-mcp-config', '--setting-sources', '--plugin-dir', '--print'):
            with self.subTest(missing=missing):
                directory = self.base / f'without-{missing.strip("-")}'
                directory.mkdir()
                (directory / 'claude').write_bytes(b'\x7fELFfixture')
                (directory / 'claude').chmod(0o700)
                env = {**os.environ, 'PATH': str(directory)}
                with self.probe(help_text=claude_help(omit=(missing,))), \
                        self.assertRaisesRegex(ValueError, f'lacks bounded-loading flags: {re.escape(missing)}$'):
                    claude_bundles.preflight(self.plan(), ['claude', '--print', '--', 'brief'], cwd=self.repo, env=env)

    def test_flag_named_only_in_a_description_does_not_count(self):
        help_text = claude_help(omit=('--settings',), extra=['  --model <model>  Model, not --settings'])
        with self.probe(help_text=help_text), self.assertRaisesRegex(ValueError, '--settings'):
            self.preflight()

    @unittest.skipUnless(INSTALLED_CLAUDE, 'no Claude Code on PATH')
    def test_installed_claude_declares_bounded_flags(self):
        # Reads the real installed Claude's --version and --help only.
        binary = INSTALLED_CLAUDE
        version = bundles.require_flags(binary, dict(os.environ), self.repo, 'Claude Code', claude_bundles.NEEDED)
        self.assertRegex(version, r'^[0-9]+\.[0-9]+\.[0-9]+$')
        print(f'verified installed Claude Code {version} at {binary}', file=sys.stderr)

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

    def store(self, ref, value=b'SENTINEL-SECRET'):
        directory = self.base / 'fleet.credentials'
        directory.mkdir(mode=0o700, exist_ok=True)
        path = directory / ref
        path.write_bytes(value)
        path.chmod(0o600)
        return path

    def test_credentials_reach_only_their_own_launch_as_file_paths(self):
        brave = {'name': 'BRAVE_API_KEY', 'ref': 'brave-search'}
        self.registry['bundles'][0]['credentials'] = [brave]
        self.registry['bundles'].append({'ref': 'other@1', 'roles': ['crew'], 'skills': [self.make_skill('other')],
                                         'credentials': [{'name': 'OTHER_KEY', 'ref': 'other'}]})
        # Names and refs only: an inline value or a value-shaped ref refuses.
        for item in ({**brave, 'value': 'SENTINEL-SECRET'}, {'name': 'BRAVE_API_KEY', 'ref': 'BSA0123456789abcdef'},
                     {'name': 'brave key', 'ref': 'brave-search'}, {'name': 'BRAVE_API_KEY', 'ref': 'x' * 41},
                     {'name': 'BRAVE_API_KEY'}):
            registry = json.loads(json.dumps(self.registry))
            registry['bundles'][0]['credentials'] = [item]
            with self.subTest(item=item), self.assertRaises(ValueError):
                bundles.validate_registry(registry)
        plan = self.plan()
        self.assertEqual(plan['credentials'], [brave])
        self.assertNotIn('credentials', self.plan([]))
        self.assertNotIn('OTHER_KEY', json.dumps(plan))
        path = self.store('brave-search')
        self.store('other')
        os.environ['PRIVATEER_CREDENTIAL_OTHER_KEY_FILE'] = str(self.base / 'fleet.credentials/other')

        def launch(plan):
            with patch.object(native_launch.os, 'execvpe') as execute, self.probe():
                native_launch.launch('claude', ['claude', '--print', '--', 'brief'], bundles=plan)
            return execute.call_args.args[1], execute.call_args.args[2]

        argv, env = launch(plan)
        self.assertEqual({k: v for k, v in env.items() if k.startswith('PRIVATEER_CREDENTIAL_')},
                         {'PRIVATEER_CREDENTIAL_BRAVE_API_KEY_FILE': str(path)})
        self.assertNotIn('SENTINEL-SECRET', json.dumps([argv, env, handle('claude', {
            'method': 'command', 'options': {},
            'context': {'repo': str(self.repo), 'title': 'brief', 'control': {}, 'tool_bundles': plan}})]))
        # A parent's credential paths are never inherited, bounded or not.
        _, env = launch(self.plan([]))
        self.assertFalse([k for k in env if k.startswith('PRIVATEER_CREDENTIAL_')])
        with patch.object(native_launch.os, 'execvpe') as execute:
            native_launch.launch('claude', ['claude', '--', 'brief'])
        self.assertFalse([k for k in execute.call_args.args[2] if k.startswith('PRIVATEER_CREDENTIAL_')])
        # Missing, wrongly moded, linked or exposed storage refuses before launch.
        for breach, undo in ((lambda: path.chmod(0o640), lambda: path.chmod(0o600)),
                             (lambda: path.rename(path.with_name('moved')), lambda: path.with_name('moved').rename(path)),
                             (lambda: path.parent.chmod(0o750), lambda: path.parent.chmod(0o700)),
                             (lambda: (path.rename(path.with_name('real')), path.symlink_to(path.with_name('real'))),
                              lambda: (path.unlink(), path.with_name('real').rename(path)))):
            breach()
            with self.subTest(breach=breach), patch.object(native_launch.os, 'execvpe') as execute:
                self.refuses(lambda: native_launch.launch('claude', ['claude', '--', 'brief'], bundles=plan))
                execute.assert_not_called()
            undo()
        self.assertEqual(launch(plan)[1]['PRIVATEER_CREDENTIAL_BRAVE_API_KEY_FILE'], str(path))


if __name__ == '__main__':
    unittest.main()
