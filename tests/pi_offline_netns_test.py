#!/usr/bin/env python3
"""Fail-closed boundary unit tests: no native exec or real network operations."""
import errno
import os
import socket
import subprocess
import sys
from pathlib import Path
import unittest
from unittest.mock import patch, MagicMock

import pi_offline_netns as guard

LO = {'ifname': 'lo', 'link_type': 'loopback', 'flags': ['LOOPBACK', 'UP']}
LOCAL = {'type': 'local', 'dst': '127.0.0.0/8', 'dev': 'lo'}


class Boundary(unittest.TestCase):
    def test_missing_or_stale_marker_refuses_before_network_probe(self):
        for env in ({}, {guard.MARKER: 'stale'}):
            with self.subTest(env=env), patch.dict(os.environ, env, clear=True), \
                    patch.object(guard, 'topology') as topology:
                with self.assertRaisesRegex(RuntimeError, 'namespace required'):
                    guard.require_boundary()
                topology.assert_not_called()

    def test_extra_interface_refuses_before_route_or_socket_probe(self):
        with patch.object(guard, 'ip_json', return_value=[LO, {'ifname': 'eth0'}]) as ip, \
                patch.object(socket, 'socket') as sock:
            with self.assertRaisesRegex(RuntimeError, 'ONLY loopback'):
                guard.topology()
            self.assertEqual(ip.call_count, 1)
            sock.assert_not_called()

    def test_nonlocal_routes_refuse_even_on_loopback(self):
        for route in ({'dev': 'lo', 'dst': 'default'},
                      {'dev': 'lo', 'type': 'local', 'dst': '192.0.2.1'},
                      {'dev': 'eth0', **{k: v for k, v in LOCAL.items() if k != 'dev'}}):
            with self.subTest(route=route), \
                    patch.object(guard, 'ip_json', side_effect=[[LO], [route]]):
                with self.assertRaisesRegex(RuntimeError, 'refuses route'):
                    guard.topology()

    def test_loopback_down_refuses_before_external_probe(self):
        with patch.dict(os.environ, {guard.MARKER: guard.identity('net')}), \
                patch.object(guard, 'topology', return_value=([dict(LO, flags=[])], [])), \
                patch.object(guard, 'external_refusal') as probe:
            with self.assertRaisesRegex(RuntimeError, 'must be up'):
                guard.require_boundary()
            probe.assert_not_called()

    def test_external_proof_requires_no_route_for_both_families_and_protocols(self):
        sock = MagicMock()
        sock.__enter__.return_value = sock
        sock.connect.side_effect = OSError(errno.ENETUNREACH, 'mock no route')
        with patch.object(socket, 'socket', return_value=sock) as factory:
            evidence = guard.external_refusal()
        self.assertEqual(len(evidence), 4)
        self.assertEqual(factory.call_args_list, [
            unittest.mock.call(socket.AF_INET, socket.SOCK_STREAM),
            unittest.mock.call(socket.AF_INET, socket.SOCK_DGRAM),
            unittest.mock.call(socket.AF_INET6, socket.SOCK_STREAM),
            unittest.mock.call(socket.AF_INET6, socket.SOCK_DGRAM)])
        for error in (None, OSError(errno.ECONNREFUSED, 'mock peer'), socket.timeout()):
            sock.connect.side_effect = error
            with self.subTest(error=error), patch.object(socket, 'socket', return_value=sock):
                with self.assertRaises(RuntimeError):
                    guard.external_refusal()

    def test_unavailable_unshare_never_executes_command_directly(self):
        for failure in (OSError('mock unavailable'),
                        subprocess.CompletedProcess([], 1)):
            with self.subTest(failure=failure), patch.object(guard.subprocess, 'run') as run:
                if isinstance(failure, OSError):
                    run.side_effect = failure
                    with self.assertRaises(OSError):
                        guard.launch(['/never-execute-native'])
                else:
                    run.return_value = failure
                    self.assertEqual(guard.launch(['/never-execute-native']), 1)
                self.assertEqual(run.call_count, 1)
                self.assertEqual(run.call_args.args[0][0], guard.UNSHARE)
                self.assertNotIn(guard.MARKER, run.call_args.kwargs['env'])

    def test_unchanged_namespace_refuses_before_link_change_or_exec(self):
        for net, user in ((guard.identity('net'), 'different'),
                          ('different', guard.identity('user'))):
            with self.subTest(net=net, user=user), \
                    patch.object(guard.subprocess, 'run') as run, \
                    patch.object(os, 'execvpe') as execute:
                with self.assertRaisesRegex(RuntimeError, 'independent user AND network'):
                    guard.child(net, user, ['/never-execute-native'])
                run.assert_not_called()
                execute.assert_not_called()

    def test_opt_in_skip_needs_no_namespace_or_home(self):
        # Missing HOME makes any attempt to launch the boundary refuse before
        # exec. An ordinary no-binary invocation must still skip successfully.
        run = subprocess.run(
            [sys.executable, str(Path(__file__).with_name('pi_mcp_native.py'))],
            env={'PATH': '/usr/bin:/bin'}, capture_output=True, text=True, timeout=5)
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn('skipped=1', run.stderr)
        self.assertNotIn('offline_boundary', run.stdout)

    def test_environment_preserves_only_real_home_and_fixed_path(self):
        with patch.dict(os.environ, {'HOME': '/real-home', 'HTTPS_PROXY': 'sentinel',
                                    'PROVIDER_KEY': 'sentinel', guard.MARKER: 'stale'}, clear=True):
            self.assertEqual(guard.clean_env(), {'HOME': '/real-home', 'PATH': '/usr/bin:/bin'})


if __name__ == '__main__':
    unittest.main()
