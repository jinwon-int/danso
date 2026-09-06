#!/usr/bin/env python3
"""Worker-safe development profile unit tests. Host integration is in test_dev_check_host."""
import contextlib
import io
import json
import subprocess
import unittest
from unittest.mock import patch

import dev_check


class Profiles(unittest.TestCase):
    def test_worker_does_not_invoke_rust_or_integration_and_requires_selection(self):
        with patch.object(dev_check.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(dev_check.main(['--profile', 'worker']), 0)
            command = run.call_args.args[0]
            self.assertEqual(run.call_count, 1)
            self.assertIn('test_live_acceptance.FailureReporting', command)
            self.assertNotIn('test_live_acceptance.Workflow', command)
            self.assertIn('host checks remain required', output.getvalue())
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                dev_check.main([])
            self.assertEqual(run.call_count, 1)

    def test_host_profile_keeps_separate_integration_gate(self):
        host = dev_check.commands('host')
        self.assertTrue(any(command[-1] == 'scripts/test_dev_check_host.py' for command in host))
        worker = dev_check.commands('worker')
        self.assertIn('test_dev_check', worker[0])
        self.assertNotIn('test_dev_check_host', str(worker))

    def test_json_profile_has_no_wrapper_stdout_and_rejects_host(self):
        with patch.object(dev_check.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(dev_check.main(['--profile', 'worker', '--json']), 0)
            self.assertEqual(output.getvalue(), '')
            self.assertEqual(run.call_args.args[0][-1], '--json')
        with patch.object(dev_check.subprocess, 'run') as run, contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            dev_check.main(['--profile', 'host', '--json'])
        run.assert_not_called()

    def test_host_failure_stops_without_worker_fallback(self):
        for result in (subprocess.CompletedProcess([], 2), OSError('private-marker')):
            with self.subTest(result=type(result).__name__):
                with patch.object(dev_check.subprocess, 'run', side_effect=result if isinstance(result, OSError) else None,
                                  return_value=result) as run:
                    output = io.StringIO()
                    with contextlib.redirect_stderr(output), contextlib.redirect_stdout(output):
                        self.assertEqual(dev_check.main(['--profile', 'host']), 1)
                    self.assertEqual(run.call_count, 1)
                    self.assertNotIn('private-marker', output.getvalue())
                    self.assertNotIn('PASS', output.getvalue())


class PlanOutput(unittest.TestCase):
    def test_list_prints_strict_json_manifest_for_worker(self):
        with patch.object(dev_check.subprocess, 'run') as run:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(dev_check.main(['--profile', 'worker', '--list']), 0)
            run.assert_not_called()
            manifest = json.loads(output.getvalue())
        self.assertEqual(list(manifest), ['profile', 'executed', 'commands'])
        self.assertEqual(manifest['profile'], 'worker')
        self.assertIs(manifest['executed'], False)
        self.assertEqual(manifest['commands'],
                         [{'argv': command, 'cwd': str(dev_check.ROOT / 'scripts')}
                          for command in dev_check.commands('worker')])
        self.assertTrue(all(isinstance(part, str) for command in manifest['commands'] for part in command['argv']))
        self.assertIn('test_live_acceptance.FailureReporting', manifest['commands'][0]['argv'])

    def test_list_prints_strict_json_manifest_for_host(self):
        with patch.object(dev_check.subprocess, 'run') as run:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(dev_check.main(['--profile', 'host', '--list']), 0)
            run.assert_not_called()
            manifest = json.loads(output.getvalue())
        self.assertEqual(list(manifest), ['profile', 'executed', 'commands'])
        self.assertEqual(manifest['profile'], 'host')
        self.assertIs(manifest['executed'], False)
        self.assertEqual(manifest['commands'],
                         [{'argv': command, 'cwd': str(dev_check.ROOT)}
                          for command in dev_check.commands('host')])
        self.assertTrue(any(command['argv'][0] == 'cargo' for command in manifest['commands']))

    def test_list_emits_single_json_object_without_banner_or_pass(self):
        output = io.StringIO()
        with patch.object(dev_check.subprocess, 'run') as run, contextlib.redirect_stdout(output):
            dev_check.main(['--profile', 'worker', '--list'])
        run.assert_not_called()
        text = output.getvalue()
        self.assertEqual(json.JSONDecoder().raw_decode(text)[1], len(text.rstrip()))
        self.assertNotIn('PASS', text)
        self.assertNotIn('WORKER SUBSET ONLY', text)

    def test_list_rejects_json_flag_for_both_profiles_without_stdout(self):
        for profile in ('worker', 'host'):
            with self.subTest(profile=profile):
                with patch.object(dev_check.subprocess, 'run') as run:
                    output = io.StringIO()
                    with contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()), \
                            self.assertRaises(SystemExit) as raised:
                        dev_check.main(['--profile', profile, '--list', '--json'])
                self.assertEqual(raised.exception.code, 2)
                self.assertEqual(output.getvalue(), '')
                run.assert_not_called()

    def test_list_preserves_required_profile_validation(self):
        for argv in ([], ['--list'], ['--profile', 'other'], ['--profile', 'other', '--list']):
            with self.subTest(argv=argv):
                with patch.object(dev_check.subprocess, 'run') as run, \
                        contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()), \
                        self.assertRaises(SystemExit) as raised:
                    dev_check.main(argv)
                self.assertEqual(raised.exception.code, 2)
                run.assert_not_called()

    def test_without_list_existing_execution_paths_are_unchanged(self):
        with patch.object(dev_check.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(dev_check.main(['--profile', 'worker']), 0)
            self.assertEqual(run.call_count, 1)
            self.assertNotIn('--list', run.call_args.args[0])
            worker_cwd = run.call_args.kwargs.get('cwd')
            expected = dev_check.commands('host')
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(dev_check.main(['--profile', 'host']), 0)
            self.assertEqual(run.call_count, 1 + len(expected))
            self.assertNotEqual(worker_cwd, run.call_args.kwargs.get('cwd'))


if __name__ == '__main__':
    unittest.main()
