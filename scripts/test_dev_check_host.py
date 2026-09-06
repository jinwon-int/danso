#!/usr/bin/env python3
"""Host-only real-sandbox development profile integration checks."""
from pathlib import Path
import shutil
import json
import unittest

import test_e2e as e2e


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
