#!/usr/bin/env python3
"""Deterministic bundle validation/preflight tests; no Pi/model/network required."""
import copy
import hashlib
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
import pi_bundles
from native import handle

# Option rows as Pi's --help lists them, keyed by the flag each row declares.
PI_ROWS = {
    '--no-extensions': '  --no-extensions, -ne           Disable extension discovery and built-in extensions',
    '--no-mcp': '  --no-mcp                       Disable built-in MCP support',
    '--no-skills': '  --no-skills, -ns               Disable skills discovery and loading',
    '--no-prompt-templates': '  --no-prompt-templates, -np     Disable prompt template discovery and loading',
    '--no-context-files': '  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery',
    '--no-themes': '  --no-themes                    Disable theme discovery and loading',
    '--skill': '  --skill <path>                 Load a skill file or directory (can be used multiple times)',
    '-e': '  --extension, -e <path>         Load an extension file or builtin:<name>',
    '--provider': '  --provider <name>              Provider to search for --model (requires --model)',
}


def native_on_path(name):
    """The CLI on PATH, unless it is a #! wrapper: a wrapper can run a state-changing
    activation (for example `mise use -g`), so the installed-CLI checks never run one."""
    found = shutil.which(name)
    if found is None:
        return None
    with open(found, 'rb') as stream:
        return None if stream.read(2) == b'#!' else found


def pi_help(omit=(), extra=()):
    rows = [row for flag, row in PI_ROWS.items() if flag not in omit]
    return '\n'.join(['pi - fixture', 'Usage:', '  pi [options]', '', 'Options:', *rows, *extra]) + '\n'


def fake_pi(version=b'1.3.0\n', help_text=None):
    """Fake `pi`: answers --version and --help only, as the capability probe asks."""
    help_text = pi_help() if help_text is None else help_text
    def run(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 0, help_text.encode() if argv[-1] == '--help' else version, b'')
    return patch.object(bundles.subprocess, 'run', side_effect=run)


class Bundles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='privateer-bundles-')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.repo = self.base / 'repo'
        self.repo.mkdir()
        self.agent = self.base / 'agent'
        self.agent.mkdir()
        self.env = {'HOME': os.environ['HOME'], 'PATH': str(self.base),
                    'PI_CODING_AGENT_DIR': str(self.agent)}
        self.binary = self.base / 'pi'
        self.binary.write_bytes(b'\x7fELFfixture')
        self.binary.chmod(0o700)
        self.a = self.make_skill('selected')
        self.b = self.make_skill('unselected')
        self.registry = {'schema_version': 1, 'pi_version': '1.2.0',
                         'integrations': {'reviewed': True, 'extensions': []},
                         'bundles': [{'ref': 'selected@1', 'roles': ['first_mate', 'crew'], 'skills': [self.a]},
                                     {'ref': 'unselected@1', 'roles': ['crew'], 'skills': [self.b]}]}

    def make_skill(self, name):
        path = self.base / name
        path.mkdir()
        (path / 'SKILL.md').write_text(f'---\nname: {name}\ndescription: Small fixture client.\n---\nRun client.py only when needed.\n')
        (path / 'client.py').write_text('print("fixture")\n')
        return {'name': name, 'root': str(path), 'sha256': bundles.tree_digest(path)}

    def plan(self, selected=None):
        return bundles.plan(self.registry, ['selected@1'] if selected is None else selected, 'crew')

    def preflight(self, plan=None, args=None, **kwargs):
        return pi_bundles.preflight(plan or self.plan(), args or ['pi', '--print', '--mode', 'json', '--', 'brief'],
                                 cwd=self.repo, env=self.env, **kwargs)

    def probe(self):
        return fake_pi()

    def test_selected_only_plan_and_explicit_empty(self):
        plan = self.plan()
        self.assertEqual([s['name'] for s in plan['skills']], ['selected'])
        self.assertNotIn('unselected', json.dumps(plan))
        self.assertEqual(self.plan([])['skills'], [])
        bundles.validate_plan(plan)

    def test_invalid_selection(self):
        for selected in (None, {}, '', ['missing@1'], ['selected@1', 'selected@1'], [1], ['-e'], ['selected']):
            with self.subTest(selected=selected), self.assertRaises((ValueError, TypeError)):
                bundles.plan(self.registry, selected, 'crew')
        with self.assertRaises(ValueError):
            bundles.plan(self.registry, ['unselected@1'], 'first_mate')
        with self.assertRaises(ValueError):
            bundles.plan(self.registry, [], 'nested')

    def test_invalid_registry_schema_and_unsupported_resources(self):
        mutations = [lambda r: r.update(schema_version=2), lambda r: r.update(pi_version='latest'),
                     lambda r: r['integrations'].pop('reviewed'), lambda r: r['integrations'].update(reviewed=False),
                     lambda r: r.update(credentials={'token': 'SENTINEL'}),
                     lambda r: r['bundles'][0].update(mcp={'url': 'http://invalid'}),
                     lambda r: r['bundles'][0].update(extensions=[]),
                     lambda r: r['bundles'][0].update(roles=['crew', 'crew']),
                     lambda r: r['bundles'].append(copy.deepcopy(r['bundles'][0])),
                     lambda r: r['bundles'][0]['skills'].append(copy.deepcopy(self.a))]
        for mutation in mutations:
            registry = copy.deepcopy(self.registry)
            mutation(registry)
            with self.subTest(registry=registry), self.assertRaises(ValueError):
                bundles.validate_registry(registry)

    def test_cross_bundle_skill_collision(self):
        self.registry['bundles'][1]['skills'] = [self.a]
        with self.assertRaises(ValueError):
            bundles.plan(self.registry, ['selected@1', 'unselected@1'], 'crew')

    def test_content_change_rejected_at_selection_and_execution(self):
        plan = self.plan()
        (Path(self.a['root']) / 'client.py').write_text('print("changed")\n')
        with self.assertRaises(ValueError):
            self.plan()
        with self.probe() as probe, self.assertRaises(ValueError):
            self.preflight(plan)
        probe.assert_not_called()

    def test_unsafe_missing_symlink_and_writable_paths(self):
        root = Path(self.a['root'])
        with self.assertRaises(ValueError):
            bundles.safe_path(str(root / '..' / root.name), directory=True)
        for path in (root / 'client.py', root):
            mode = path.stat().st_mode
            path.chmod(0o777)
            with self.assertRaises(ValueError):
                self.plan()
            path.chmod(mode)
        (root / 'escape').symlink_to(self.b['root'], target_is_directory=True)
        with self.assertRaises(ValueError):
            self.plan()
        (root / 'escape').unlink()
        (root / 'SKILL.md').unlink()
        with self.assertRaises(ValueError):
            self.plan()

    def test_sticky_ancestor_exception_is_root_owned_directory_only(self):
        from types import SimpleNamespace
        import stat
        path = Path('/controlled-temp/skill')
        def info(mode, uid):
            return SimpleNamespace(st_mode=mode, st_uid=uid)
        for mode, uid, allowed in (
                (stat.S_IFDIR | 0o1777, 0, True),
                (stat.S_IFDIR | 0o1777, 1, False),
                (stat.S_IFDIR | 0o0777, 0, False),
                (stat.S_IFDIR | 0o0775, 0, False),
                (stat.S_IFLNK | 0o1777, 0, False)):
            metadata = {Path('/'): info(stat.S_IFDIR | 0o755, 0),
                        path.parent: info(mode, uid),
                        path: info(stat.S_IFDIR | 0o700, os.geteuid())}
            with self.subTest(mode=mode, uid=uid), \
                    patch.object(Path, 'lstat', autospec=True, side_effect=lambda p: metadata[p]), \
                    patch.object(Path, 'stat', autospec=True, side_effect=lambda p: metadata[p]):
                if allowed:
                    self.assertEqual(bundles.safe_path(str(path), directory=True), path)
                else:
                    with self.assertRaises(ValueError):
                        bundles.safe_path(str(path), directory=True)

    def test_skill_frontmatter_cannot_silently_rename_or_skip(self):
        path = Path(self.a['root']) / 'SKILL.md'
        for text in ('bad', '---\nname: selected\n---\n', '---\nname: other\ndescription: demo\n---\n',
                     '---\nname: selected\ndescription: demo\nallowed-tools: bash\n---\n'):
            path.write_text(text)
            self.a['sha256'] = bundles.tree_digest(path.parent)
            with self.assertRaises(ValueError):
                self.plan()

    def test_yaml_forms_cannot_silently_change_description_type(self):
        path = Path(self.a['root']) / 'SKILL.md'
        for description in ('true', '[]', '{name: value}', '&alias value', '|', '123', 'bad: mapping'):
            path.write_text(f'---\nname: selected\ndescription: {description}\n---\n')
            self.a['sha256'] = bundles.tree_digest(path.parent)
            with self.subTest(description=description), self.assertRaises(ValueError):
                self.plan()
        path.write_text('---\nname: selected\ndescription: "Quoted, Unicode \\u03b1: supported."\n---\n')
        self.a['sha256'] = bundles.tree_digest(path.parent)
        self.plan()

    def test_tree_is_bounded(self):
        (Path(self.a['root']) / 'huge').write_bytes(b'x' * (bundles.MAX_BYTES + 1))
        with self.assertRaises(ValueError):
            self.plan()

    def test_preflight_preserves_route_prompt_environment_and_empty_mode(self):
        before = dict(self.env)
        with self.probe() as probe:
            argv = self.preflight(args=['pi', '--provider', 'chosen', '--model', 'exact', '--', 'brief -e not a flag'])
            empty = self.preflight(self.plan([]))
        self.assertEqual(self.env, before)
        self.assertEqual(argv[-6:], ['--provider', 'chosen', '--model', 'exact', '--', 'brief -e not a flag'])
        self.assertIn('--no-mcp', argv)
        self.assertIn('--skill', argv)
        self.assertNotIn('--skill', empty)
        self.assertEqual(probe.call_count, 3)  # --version twice, --help once (cached)
        # Help is asked with extensions off: Pi loads them to print help otherwise.
        helps = [call.args[0] for call in probe.call_args_list if call.args[0][-1] == '--help']
        self.assertEqual([argv[1:] for argv in helps], [['--no-extensions', '--help']])

    def test_only_explicit_approved_integrations_and_privateer_mail_load(self):
        ext = self.base / 'approved.mjs'
        ext.write_text('export default function(pi) {}\n')
        self.registry['integrations']['extensions'] = [{'path': str(ext), 'sha256': hashlib.sha256(ext.read_bytes()).hexdigest()}]
        mail = ROOT / 'plugins/attended/mail.ts'
        with self.probe():
            argv = self.preflight(args=['pi', '-e', str(mail), '--', 'brief'], runtime_extensions=[mail])
        self.assertEqual(argv.count('-e'), 2)
        self.assertIn(str(ext), argv)
        self.assertIn(str(mail), argv)
        ext.write_text('throw new Error("changed")')
        with self.assertRaises(ValueError):
            self.plan()

    def test_effective_hosted_crew_adapter_is_retained_but_not_inherited(self):
        import native_launch
        crew = str(ROOT / 'plugins/attended/crew.ts')
        env = dict(self.env, PRIVATEER_HOSTED_CREW='1')
        with patch.dict(os.environ, env, clear=True), self.probe(), patch.object(native_launch.os, 'execvpe') as execute:
            native_launch.launch('pi', ['pi', '-e', crew, '--', 'brief'], bundles=self.plan([]))
            argv = execute.call_args.args[1]
            self.assertIn(crew, argv)
            self.assertEqual(argv.count('-e'), 1)
            self.assertIn('--no-mcp', argv)
            execute.reset_mock()
            native_launch.launch('pi', ['pi', '--', 'brief'], bundles=self.plan([]))
            self.assertNotIn(crew, execute.call_args.args[1], 'a stale hosted marker cannot add resources')
        with patch.dict(os.environ, self.env, clear=True), self.probe() as probe, self.assertRaises(ValueError):
            native_launch.launch('pi', ['pi', '-e', crew, '--', 'brief'], bundles=self.plan([]))
        probe.assert_not_called()
        for flags in (['-e', crew, '-e', crew], ['-e', crew + '.spoof'], ['--extension', crew], ['-e' + crew]):
            with self.subTest(flags=flags), patch.dict(os.environ, env, clear=True), self.probe() as probe, self.assertRaises(ValueError):
                native_launch.launch('pi', ['pi', *flags, '--', 'brief'], bundles=self.plan([]))
            probe.assert_not_called()

    def test_bounded_pi_launch_gets_only_its_credential_file_path(self):
        import native_launch
        self.registry['bundles'][0]['credentials'] = [{'name': 'BRAVE_API_KEY', 'ref': 'brave-search'}]
        directory = self.base / 'fleet.credentials'
        directory.mkdir(mode=0o700)
        (directory / 'brave-search').write_bytes(b'SENTINEL-SECRET')
        (directory / 'brave-search').chmod(0o600)
        env = dict(self.env, PRIVATEER_FLEET=str(self.base / 'fleet.sqlite'))
        with patch.dict(os.environ, env, clear=True), self.probe(), patch.object(native_launch.os, 'execvpe') as execute:
            native_launch.launch('pi', ['pi', '--', 'brief'], bundles=self.plan())
        argv, launched = execute.call_args.args[1:]
        self.assertEqual(launched['PRIVATEER_CREDENTIAL_BRAVE_API_KEY_FILE'], str(directory / 'brave-search'))
        self.assertNotIn('SENTINEL-SECRET', json.dumps([argv, launched]))
        (directory / 'brave-search').unlink()
        with patch.dict(os.environ, env, clear=True), self.probe() as probe, self.assertRaises(ValueError):
            native_launch.launch('pi', ['pi', '--', 'brief'], bundles=self.plan())
        probe.assert_not_called()

    def test_calm_exact_runtime_adapter_in_legacy_and_bounded_modes(self):
        import native_launch
        mail = str(ROOT / 'plugins/attended/mail.ts')
        crew = str(ROOT / 'plugins/attended/crew.ts')
        calm = str(ROOT / 'plugins/attended/calm.ts')
        captain = str(ROOT / 'plugins/attended/captain.ts')
        before = dict(self.env, PRIVATEER_ROLE='agent', PRIVATEER_ACTOR='fixture',
                      PRIVATEER_HOME=str(self.base / '.privateer'),
                      PRIVATEER_CONTROL_CLI='/fixture/privateer',
                      PRIVATEER_ATTENDED_HARNESS='pi', PI_CODING_AGENT_DIR=str(self.agent))
        for bounded in (False, True):
            for mode, adapters in (('headless', []), ('attended', [mail, captain, calm]), ('hosted', [crew, calm])):
                env = dict(before)
                if mode == 'hosted':
                    env['PRIVATEER_HOSTED_CREW'] = '1'
                args = ['pi', *[arg for path in adapters for arg in ('-e', path)],
                        '--provider', 'chosen', '--model', 'exact', '--', 'brief']
                with self.subTest(bounded=bounded, mode=mode), patch.dict(os.environ, env, clear=True), \
                        self.probe(), patch.object(native_launch.os, 'execvpe') as execute:
                    native_launch.launch('pi', args, bundles=self.plan([]) if bounded else None,
                                         attended=mode == 'attended')
                    result = execute.call_args.args[1]
                    self.assertEqual(execute.call_args.args[2], dict(env, PWD=os.getcwd()))
                    self.assertEqual(result[-6:], args[-6:])
                    self.assertEqual(result.count('-e'), len(adapters))
                    self.assertEqual('--no-mcp' in result, bounded)
                    for path in adapters:
                        self.assertEqual(result.count(path), 1)
                    if mode == 'headless':
                        self.assertNotIn(calm, result)
        for attended, hosted, flags in (
                (False, False, ['-e', calm]), (False, True, ['-e', calm]),
                (True, False, ['-e', calm + '.spoof']), (True, False, ['--extension', calm]),
                (True, False, ['-e' + calm]), (True, False, ['-e', calm, '-e', calm]),
                (False, True, ['-e', crew, '-e', calm + '.spoof']),
                (False, False, ['-e', captain]), (False, True, ['-e', crew, '-e', captain])):
            env = dict(before, PRIVATEER_HOSTED_CREW='1' if hosted else '0')
            with self.subTest(flags=flags, attended=attended, hosted=hosted), \
                    patch.dict(os.environ, env, clear=True), self.probe() as probe, self.assertRaises(ValueError):
                native_launch.launch('pi', ['pi', *flags, '--', 'brief'], bundles=self.plan([]), attended=attended)
            probe.assert_not_called()

    def test_ambient_packages_and_prompt_files_refuse_before_native_probe(self):
        for directory in (self.agent, self.repo / '.pi'):
            directory.mkdir(exist_ok=True)
            path = directory / 'settings.json'
            for content in ('{"packages":["npm:never-install"]}', '{broken', '[]', '{"packages":null}', '{"packages":""}'):
                path.write_text(content)
                with self.probe() as probe, self.assertRaises((ValueError, json.JSONDecodeError)):
                    self.preflight()
                probe.assert_not_called()
            path.unlink()
            for name in ('SYSTEM.md', 'APPEND_SYSTEM.md'):
                path = directory / name
                path.write_text('ambient secret not read')
                with self.probe() as probe, self.assertRaises(ValueError):
                    self.preflight()
                probe.assert_not_called()
                path.unlink()
                path.symlink_to(directory / 'missing')
                with self.probe() as probe, self.assertRaises(ValueError):
                    self.preflight()
                probe.assert_not_called()
                path.unlink()

    def test_settings_symlink_is_not_followed(self):
        (self.agent / 'settings.json').symlink_to(self.agent / 'auth.json')
        (self.agent / 'auth.json').write_text('DO_NOT_READ_SECRET')
        with self.probe() as probe, self.assertRaises(ValueError):
            self.preflight()
        probe.assert_not_called()

    def test_no_credential_store_reads(self):
        for name in ('auth.json', 'models.json', 'mcp.json'):
            (self.agent / name).write_text('SECRET_NOT_JSON')
        with self.probe():
            self.preflight()

    def test_unsupported_resume_and_resource_overrides_rejected_before_probe(self):
        for args in (['--continue'], ['-c'], ['--session', 'id'], ['--fork=x'], ['--skill', str(self.base)],
                     ['-e', str(self.base)], ['-eattached'], ['--extension=x'], ['--system-prompt', 'x'],
                     ['--append-system-prompt=x'], ['--approve']):
            with self.subTest(args=args), self.probe() as probe, self.assertRaises(ValueError):
                self.preflight(args=['pi', *args, '--', 'brief'])
            probe.assert_not_called()

    def test_version_is_not_a_pin_flags_are(self):
        # Any reported version runs when the installed Pi declares every flag.
        for version in (b'1.2.0\n', b'1.3.0\n', b'1.4.2\n'):
            with self.subTest(version=version), fake_pi(version):
                self.assertIn('--no-mcp', self.preflight())

    def test_missing_flag_refuses_and_names_it(self):
        # One binary per case: capabilities are cached per path and version.
        for missing in ('--no-mcp', '-e', '--skill', '--no-themes'):
            with self.subTest(missing=missing):
                directory = self.base / f'without-{missing.strip("-")}'
                directory.mkdir()
                (directory / 'pi').write_bytes(b'\x7fELFfixture')
                (directory / 'pi').chmod(0o700)
                env = {**self.env, 'PATH': str(directory)}
                with fake_pi(help_text=pi_help(omit=(missing,))), \
                        self.assertRaisesRegex(ValueError, f'lacks bounded-loading flags: {re.escape(missing)}$'):
                    pi_bundles.preflight(self.plan(), ['pi', '--print', '--mode', 'json', '--', 'brief'],
                                         cwd=self.repo, env=env)

    def test_flag_named_only_in_a_description_does_not_count(self):
        help_text = pi_help(omit=('--no-mcp',), extra=['  --provider <name>  Provider, see --no-mcp in docs'])
        with fake_pi(help_text=help_text), self.assertRaisesRegex(ValueError, '--no-mcp'):
            self.preflight()

    def test_capabilities_cached_per_path_and_version(self):
        with self.probe() as probe:
            self.preflight()
            self.preflight()
        # --version each run, --help once for this path and version.
        self.assertEqual(probe.call_count, 3)

    def test_installed_pi_declares_bounded_flags(self):
        # Reads the real installed Pi's --version and --help only, offline. A #! wrapper skips.
        binary = native_on_path('pi')
        if binary is None:
            self.skipTest('no native Pi on PATH (a #! wrapper is never run)')
        version = bundles.require_flags(binary, {**os.environ, 'PI_OFFLINE': '1'}, self.repo, 'Pi', pi_bundles.NEEDED, help_args=pi_bundles.HELP)
        self.assertRegex(version, r'^[0-9]+\.[0-9]+\.[0-9]+$')
        print(f'verified installed Pi {version} at {binary}', file=sys.stderr)

    def test_activation_wrapper_refuses(self):
        self.binary.write_text('#!/bin/sh\nmise use pi\n')
        with self.probe() as probe, self.assertRaises(ValueError):
            self.preflight()
        probe.assert_not_called()

    def test_legacy_native_adapter_unchanged_and_bounded_not_an_option(self):
        request = {'method': 'command', 'context': {'repo': str(self.repo), 'title': 'brief', 'control': {}},
                   'options': {'provider': 'chosen', 'model': 'exact'}}
        legacy = handle('pi', request)
        self.assertNotIn('--tool-bundles', legacy['argv'])
        request['context']['tool_bundles'] = self.plan([])
        bounded = handle('pi', request)
        self.assertIn('--tool-bundles', bounded['argv'])
        request['options']['tool_bundles'] = []
        with self.assertRaises(ValueError):
            handle('pi', request)
        request['options'].pop('tool_bundles')
        request['options']['resume'] = '00000000-0000-4000-8000-000000000001'
        with self.assertRaises(ValueError):
            handle('pi', request)

    def test_protocol_redacts_malformed_registry_and_native_preflight_errors(self):
        sentinel = 'BUNDLE_SECRET_SENTINEL_9d0c'
        request = {'id': 'test', 'version': 1, 'method': 'validate', 'registry': {sentinel: sentinel}}
        run = subprocess.run([sys.executable, str(ROOT / 'plugins/bundles.py')], input=json.dumps(request),
                             text=True, capture_output=True, timeout=5)
        self.assertFalse(json.loads(run.stdout)['ok'])
        self.assertNotIn(sentinel, run.stdout + run.stderr)
        run = subprocess.run([sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles', json.dumps({sentinel: sentinel}), 'pi', 'pi'],
                             capture_output=True, text=True, timeout=5)
        self.assertEqual(run.returncode, 2)
        self.assertNotIn(sentinel, run.stdout + run.stderr)


    # Selected MCP servers: remote streamable-HTTP, Pi only, Bearer by credential ref.
    def mcp_record(self, **change):
        record = {'name': 'docs', 'url': 'https://mcp.example.test/mcp', 'credential': 'DOCS_TOKEN', 'timeout': 5}
        record.update(change)
        return {key: value for key, value in record.items() if value is not None}

    def mcp_bundle(self, ref='docs@1', servers=None, skills=(), credentials=None):
        bundle = {'ref': ref, 'roles': ['first_mate', 'crew'], 'skills': list(skills),
                  'credentials': [{'name': 'DOCS_TOKEN', 'ref': 'docs-search'}] if credentials is None else credentials,
                  'mcp_servers': [self.mcp_record()] if servers is None else servers}
        return bundle

    def registry_with(self, *bundle_list):
        return dict(self.registry, bundles=list(bundle_list))

    def test_selected_mcp_registry_refuses_every_unsafe_shape(self):
        bundles.validate_registry(self.registry_with(self.mcp_bundle(skills=[self.a])))
        cases = [
            {'command': 'npx'}, {'headers': {'Authorization': 'x'}}, {'oauth': True}, {'type': 'stdio'},
            {'url': 'https://mcp.example.test/$TOKEN'}, {'url': 'http://mcp.example.test/mcp'},
            {'url': 'https://user:pw@mcp.example.test/mcp'}, {'url': 'https://mcp.example.test/mcp#x'},
            {'url': 'ftp://mcp.example.test/mcp'}, {'url': 'https://mcp.example.test/' + 'a' * 2048},
            # Backslash, whitespace, host characters and ports the parser would read differently.
            {'url': 'https://evil.example\\.good.example/mcp'}, {'url': 'https://mcp.example.test\\/mcp'},
            {'url': 'https://mcp example.test/mcp'}, {'url': 'https://mcp.example.test/mcp\t'},
            {'url': 'https://mcp.example.test:99999/mcp'}, {'url': 'https://mcp.example.test:abc/mcp'},
            {'url': 'https://bad_host.example.test/mcp'}, {'url': 'https://[::1/mcp'},
            {'url': 'https://user@mcp.example.test/mcp'}, {'url': 'https://mcp.example.test:/mcp'},
            {'credential': 'UNDECLARED'}, {'timeout': 0}, {'timeout': 301}, {'timeout': True},
            {'name': 'x' * 33}, {'name': 'Docs'}, {'name': '$docs'}, {'name': 'docs_1'},
            {'description': '!run'}, {'description': 'x' * 201}, {'description': 'a\nb'}, {'description': 'a$b'},
        ]
        for change in cases:
            record = self.mcp_record()
            record.update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                bundles.validate_registry(self.registry_with(self.mcp_bundle(servers=[record], skills=[self.a])))
        # Declared credential must belong to the same bundle.
        with self.assertRaises(ValueError):
            bundles.validate_registry(self.registry_with(self.mcp_bundle(servers=[self.mcp_record(credential='OTHER')],
                                                                         skills=[self.a], credentials=[])))
        # Accepted forms: an explicit port on a plain host, and a bracketed IPv6 literal.
        for good in ('https://mcp.example.test:8443/mcp', 'https://mcp.example.test/mcp?x=1', 'http://[::1]:9000/mcp'):
            bundles.validate_registry(self.registry_with(self.mcp_bundle(servers=[self.mcp_record(url=good)], skills=[self.a])))
        # A bundle needs at least one skill or one server; an empty server list is not a server.
        with self.assertRaises(ValueError):
            bundles.validate_registry(self.registry_with(self.mcp_bundle(servers=[], skills=[])))
        with self.assertRaises(ValueError):
            bundles.validate_registry(self.registry_with(self.mcp_bundle(servers=[self.mcp_record()] * 5, skills=[self.a])))

    def test_selected_mcp_limits_and_collisions_are_per_plan(self):
        first = [self.mcp_record(name=f's{i}') for i in range(4)]
        second = [self.mcp_record(name=f't{i}') for i in range(4)]
        # Eight is the most in one plan; nine across three bundles refuses at selection.
        registry = self.registry_with(self.mcp_bundle('a@1', servers=first), self.mcp_bundle('b@1', servers=second),
                                      self.mcp_bundle('c@1', servers=[self.mcp_record(name='c0')]))
        bundles.validate_registry(registry)
        with self.assertRaises(ValueError):
            bundles.plan(registry, ['a@1', 'b@1', 'c@1'], 'crew')
        self.assertEqual(len(bundles.plan(registry, ['a@1', 'b@1'], 'crew')['mcp_servers']), 8)
        # One normalized name across two selected bundles refuses at selection.
        collide = self.registry_with(self.mcp_bundle('a@1', servers=[self.mcp_record(name='a-b')]),
                                     self.mcp_bundle('b@1', servers=[self.mcp_record(name='a-b')]))
        with self.assertRaises(ValueError):
            bundles.plan(collide, ['a@1', 'b@1'], 'crew')
        # Names that differ only before normalization (case) still collide after it.
        with self.assertRaises(ValueError):
            bundles.server_keys([{'name': 'A-b'}, {'name': 'a-b'}])

    def test_selected_mcp_plan_carries_only_selected_servers(self):
        registry = self.registry_with(self.mcp_bundle('docs@1', servers=[self.mcp_record()], skills=[self.a]),
                                      {'ref': 'other@1', 'roles': ['crew'], 'skills': [self.b],
                                       'mcp_servers': [self.mcp_record(name='hidden', url='https://hidden.example.test/mcp', credential='HIDDEN')],
                                       'credentials': [{'name': 'HIDDEN', 'ref': 'hidden-ref'}]})
        plan = bundles.plan(registry, ['docs@1'], 'crew')
        self.assertEqual([s['name'] for s in plan['mcp_servers']], ['docs'])
        self.assertNotIn('hidden', json.dumps(plan))
        bundles.validate_plan(plan)
        # Without servers, the plan is exactly the earlier shape: no key at all.
        self.assertNotIn('mcp_servers', self.plan())
        self.assertEqual(self.plan(), bundles.plan(self.registry, ['selected@1'], 'crew'))
        # A tampered record in a stored plan refuses at validation.
        tampered = copy.deepcopy(plan)
        tampered['mcp_servers'][0]['headers'] = {'Authorization': 'x'}
        with self.assertRaises(ValueError):
            bundles.validate_plan(tampered)
        # Claude has no bounded MCP loader: refuse, never drop the server.
        with self.assertRaises(ValueError):
            bundles.plan(registry, ['docs@1'], 'crew', harness='claude')
        # A forged Claude plan that carries servers refuses at validation.
        # A Claude plan reads user settings; keep the developer's own out of it.
        (self.base / 'claude-config').mkdir(exist_ok=True)
        with patch.dict(os.environ, {'CLAUDE_CONFIG_DIR': str(self.base / 'claude-config')}):
            claude = bundles.plan(dict(self.registry, claude_version='2.1.300'), ['selected@1'], 'crew', harness='claude')
        bundles.validate_plan(claude)
        with self.assertRaises(ValueError):
            bundles.validate_plan(dict(claude, mcp_servers=[self.mcp_record()]))

    def test_selected_mcp_preflight_adds_only_the_runtime_gate(self):
        import native_launch
        registry = self.registry_with(self.mcp_bundle(skills=[self.a]))
        directory = self.base / 'fleet.credentials'
        directory.mkdir(mode=0o700)
        (directory / 'docs-search').write_bytes(b'SENTINEL-MCP-TOKEN')
        (directory / 'docs-search').chmod(0o600)
        env = dict(self.env, PRIVATEER_FLEET=str(self.base / 'fleet.sqlite'), PRIVATEER_MCP_SERVERS='{"ambient":true}')
        with patch.dict(os.environ, env, clear=True), self.probe(), patch.object(native_launch.os, 'execvpe') as execute:
            native_launch.launch('pi', ['pi', '--print', '--', 'brief'], bundles=bundles.plan(registry, ['docs@1'], 'crew'))
        argv, launched = execute.call_args.args[1:]
        gate = str(ROOT / 'plugins/pi-mcp/selected.ts')
        self.assertIn(gate, argv)
        self.assertLess(argv.index(gate), argv.index('--'))
        recorded = json.loads(launched['PRIVATEER_MCP_SERVERS'])
        self.assertEqual([r['name'] for r in recorded], ['docs'])
        self.assertNotIn('SENTINEL-MCP-TOKEN', json.dumps([argv, launched]))
        self.assertEqual(launched['PRIVATEER_CREDENTIAL_DOCS_TOKEN_FILE'], str(directory / 'docs-search'))
        # No servers: no gate, no inherited selection, and the old argv shape.
        plain_env = dict(env)
        with patch.dict(os.environ, plain_env, clear=True), self.probe(), patch.object(native_launch.os, 'execvpe') as execute:
            native_launch.launch('pi', ['pi', '--print', '--', 'brief'], bundles=self.plan())
        argv, launched = execute.call_args.args[1:]
        self.assertNotIn(gate, argv)
        self.assertNotIn('PRIVATEER_MCP_SERVERS', launched)

    def test_selected_mcp_missing_credential_refuses_before_native_probe(self):
        import native_launch
        registry = self.registry_with(self.mcp_bundle(skills=[self.a]))
        env = dict(self.env, PRIVATEER_FLEET=str(self.base / 'fleet.sqlite'))
        with patch.dict(os.environ, env, clear=True), self.probe() as probe, self.assertRaises(ValueError):
            native_launch.launch('pi', ['pi', '--print', '--', 'brief'], bundles=bundles.plan(registry, ['docs@1'], 'crew'))
        probe.assert_not_called()


if __name__ == '__main__':
    unittest.main()
