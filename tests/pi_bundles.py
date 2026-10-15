#!/usr/bin/env python3
"""Deterministic bundle validation/preflight tests; no Pi/model/network required."""
import copy
import hashlib
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
import pi_bundles as bundles
from native import handle


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
        return bundles.preflight(plan or self.plan(), args or ['pi', '--print', '--mode', 'json', '--', 'brief'],
                                 cwd=self.repo, env=self.env, **kwargs)

    def probe(self):
        return patch.object(bundles.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, b'1.2.0\n', b''))

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
        mutations = [lambda r: r.update(schema_version=2), lambda r: r.update(pi_version='1.2.1'),
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
        self.assertEqual(probe.call_count, 2)

    def test_only_explicit_approved_integrations_and_privateer_mail_load(self):
        ext = self.base / 'approved.mjs'
        ext.write_text('export default function(pi) {}\n')
        self.registry['integrations']['extensions'] = [{'path': str(ext), 'sha256': hashlib.sha256(ext.read_bytes()).hexdigest()}]
        mail = ROOT / 'plugins/attended/mail.ts'
        with self.probe():
            argv = self.preflight(args=['pi', '-e', str(mail), '--', 'brief'], mail_extension=mail)
        self.assertEqual(argv.count('-e'), 2)
        self.assertIn(str(ext), argv)
        self.assertIn(str(mail), argv)
        ext.write_text('throw new Error("changed")')
        with self.assertRaises(ValueError):
            self.plan()

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

    def test_wrong_version_and_activation_wrapper_refuse(self):
        with patch.object(bundles.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, b'1.3.0\n', b'')):
            with self.assertRaises(ValueError):
                self.preflight()
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
        run = subprocess.run([sys.executable, str(ROOT / 'plugins/pi_bundles.py')], input=json.dumps(request),
                             text=True, capture_output=True, timeout=5)
        self.assertFalse(json.loads(run.stdout)['ok'])
        self.assertNotIn(sentinel, run.stdout + run.stderr)
        run = subprocess.run([sys.executable, str(ROOT / 'plugins/native_launch.py'), '--tool-bundles', json.dumps({sentinel: sentinel}), 'pi', 'pi'],
                             capture_output=True, text=True, timeout=5)
        self.assertEqual(run.returncode, 2)
        self.assertNotIn(sentinel, run.stdout + run.stderr)


if __name__ == '__main__':
    unittest.main()
