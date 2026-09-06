#!/usr/bin/env python3
"""Worker-safe development profile unit tests. Host integration is in test_dev_check_host."""
import contextlib
import io
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


if __name__ == '__main__':
    unittest.main()
