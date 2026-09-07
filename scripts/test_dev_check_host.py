#!/usr/bin/env python3
"""Host-only real-sandbox development profile integration checks."""
from pathlib import Path
import shutil
import json
import os
import subprocess
import sys
import unittest

import test_e2e as e2e
import test_dev_check as profile_tests


class PipedOutput(unittest.TestCase):
    SCRIPT = Path(__file__).with_name('dev_check.py')
    DRIVER = '''
import importlib.util
import sys
spec = importlib.util.spec_from_file_location('candidate', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
child = "import sys; print('child-stdout', flush=True); print('child-stderr', file=sys.stderr, flush=True); sys.exit(int(sys.argv[1]))"
module.commands = lambda profile: [[sys.executable, '-c', child, sys.argv[2]]]
raise SystemExit(module.main(sys.argv[3:]))
'''

    def test_real_children_keep_piped_streams_and_banner_order(self):
        # StringIO cannot reveal a buffered parent banner overtaken by a child
        # writing to the inherited pipe. Use real children, but never Cargo/API.
        for profile, structured in profile_tests.OutputContract.MODES:
            for child_exit in (0, 9):
                with self.subTest(profile=profile, structured=structured, child_exit=child_exit):
                    command = [sys.executable, '-I', '-B', '-c', self.DRIVER,
                               str(self.SCRIPT), str(child_exit), '--profile', profile]
                    if structured:
                        command.append('--json')
                    result = subprocess.run(command, capture_output=True, text=True, timeout=10,
                                            env={'PATH': os.defpath})
                    stdout = '' if structured else profile_tests.OutputContract.BANNERS[profile]
                    stdout += 'child-stdout\n'
                    stderr = 'child-stderr\n'
                    if child_exit:
                        stderr += 'FAIL: development check stopped; no remaining checks were run.\n'
                    elif not structured:
                        stdout += f'PASS: {profile} checks only.\n'
                    self.assertEqual(result.returncode, 1 if child_exit else 0, result.stderr)
                    self.assertEqual(result.stdout, stdout)
                    self.assertEqual(result.stderr, stderr)


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
                     'test_e2e.py', 'test_providers.py', 'test_dev_check.py', 'worker_checks.py', 'test_worker_checks.py'):
            shutil.copy2(Path(__file__).parent / name, scripts / name)
        self.tool('bash', {'command': 'python3 scripts/dev_check.py --profile worker && python3 scripts/dev_check.py --profile worker --json > receipt.json'})
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        tools = self.results()
        self.assertEqual(len(tools), 1)
        self.assertFalse(tools[0]['isError'], tools[0])
        receipt = json.loads((self.repo / 'receipt.json').read_text())
        self.assertTrue(receipt['successful'])
        self.assertEqual(receipt['tests_run'], sum(row['tests_run'] for row in receipt['suites']))
        self.assertEqual([row['selector'] for row in receipt['suites']],
                         ['test_live_acceptance.Safety', 'test_live_acceptance.FailureReporting', 'test_dev_check', 'test_worker_checks'])
        self.assertIn('PASS: worker checks only.', str(tools[0]['content']))
        self.assertIn('host checks remain required', str(tools[0]['content']))


if __name__ == '__main__':
    unittest.main()
