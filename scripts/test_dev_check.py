#!/usr/bin/env python3
"""Development profile boundaries, including execution inside the real sandbox."""
import contextlib
import io
from pathlib import Path
import shutil
import subprocess
import unittest
from unittest.mock import patch

import dev_check
import test_e2e as e2e


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


class WorkerSandbox(unittest.TestCase):
    setUp = e2e.Acceptance.setUp
    tearDown = e2e.Acceptance.tearDown
    command = e2e.Acceptance.command
    run_cli = e2e.Acceptance.run_cli
    final = e2e.Acceptance.final
    tool = e2e.Acceptance.tool
    results = e2e.Acceptance.results

    def test_worker_subset_runs_inside_real_danso_sandbox(self):
        scripts = self.repo / 'scripts'
        scripts.mkdir()
        for name in ('dev_check.py', 'live_acceptance.py', 'test_live_acceptance.py',
                     'test_e2e.py', 'test_providers.py'):
            shutil.copy2(Path(__file__).parent / name, scripts / name)
        self.tool('bash', {'command': 'python3 scripts/dev_check.py --profile worker'})
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        tools = self.results()
        self.assertEqual(len(tools), 1)
        self.assertFalse(tools[0]['isError'], tools[0])
        self.assertIn('PASS: worker checks only.', str(tools[0]['content']))
        self.assertIn('host checks remain required', str(tools[0]['content']))


if __name__ == '__main__':
    unittest.main()
