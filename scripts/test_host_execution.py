#!/usr/bin/env python3
"""Real default host execution; synthetic credentials and loopback providers only."""
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest
import test_e2e as e2e
import test_providers as providers

class HostProviders(providers.Providers):
    execution_args = []

class Host(unittest.TestCase):
    setUp = e2e.Acceptance.setUp
    tearDown = e2e.Acceptance.tearDown
    run_cli = e2e.Acceptance.run_cli
    final = e2e.Acceptance.final
    tool = e2e.Acceptance.tool
    results = e2e.Acceptance.results
    usage = e2e.Acceptance.usage
    test_four_tools_context_session_and_resume = e2e.Acceptance.test_four_tools_context_session_and_resume
    test_output_limit_and_timeout = e2e.Acceptance.test_output_limit_and_timeout
    test_interrupt_leaves_uncertain_operation_and_no_replay = e2e.Acceptance.test_interrupt_leaves_uncertain_operation_and_no_replay
    test_run_timeout_contract = e2e.Acceptance.test_run_timeout_contract

    def command(self, *extra):
        return [str(e2e.BIN), '--cwd', str(self.repo), '--session', str(self.session),
                '--model', 'fixture-model', *extra, 'do the task']

    def test_host_permissions_are_explicit_and_environment_is_cleared(self):
        outside = self.root/'outside'; outside.write_text('host-sentinel')
        self.tool('bash', {'command': f'test -z "${{ANTHROPIC_API_KEY:-}}" && cat {outside} && echo touched >> {outside}'})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.results()[0]['isError'])
        self.assertIn('touched', outside.read_text())
        description = next(d['description'] for d in self.requests[0]['tools'] if d['name']=='bash')
        self.assertIn('not sandboxed', description)

    def detached(self, tail):
        # setsid escapes a process-group-only implementation. PIDs and a delayed
        # marker prove termination and actual reaping, not merely closed pipes.
        return "setsid /bin/bash -c 'echo $$ > detached.pid; sleep 2; echo leaked > survivor; sleep 20' >/dev/null 2>&1 & " + tail

    def assert_reaped(self):
        pid = int((self.repo/'detached.pid').read_text())
        deadline = time.monotonic()+3
        while Path(f'/proc/{pid}').exists() and time.monotonic()<deadline:
            time.sleep(.02)
        self.assertFalse(Path(f'/proc/{pid}').exists(), 'detached child was not reaped')
        time.sleep(2.1)
        self.assertFalse((self.repo/'survivor').exists())

    def test_detached_descendants_removed_after_success(self):
        self.tool('bash', {'command': self.detached('while test ! -f detached.pid; do sleep .01; done')})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assert_reaped()

    def test_detached_descendants_removed_after_tool_timeout(self):
        self.tool('bash', {'command': self.detached('sleep 20')})
        p = self.run_cli('--tool-timeout-seconds', '1')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(self.results()[0]['isError'])
        self.assertIn('timed out', str(self.results()[0]))
        self.assert_reaped()

    def terminate_parent(self, sig):
        self.tool('bash', {'command': self.detached('sleep 20')})
        p = subprocess.Popen(self.command(), env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic()+5
            while not (self.repo/'detached.pid').exists() and time.monotonic()<deadline: time.sleep(.02)
            self.assertTrue((self.repo/'detached.pid').exists())
            p.send_signal(sig)
            p.communicate(timeout=5)
        finally:
            if p.poll() is None: p.kill(); p.communicate()
        self.assert_reaped()

    def test_detached_descendants_removed_after_sigterm(self):
        self.terminate_parent(signal.SIGTERM)

    def test_detached_descendants_removed_after_parent_sigkill(self):
        self.terminate_parent(signal.SIGKILL)

    def test_legacy_host_alias_and_conflicting_backend(self):
        self.final()
        p = self.run_cli('--unsafe-no-sandbox')
        self.assertEqual(p.returncode, 0, p.stderr)
        before = len(self.requests)
        p = self.run_cli('--unsafe-no-sandbox', '--sandbox', 'bubblewrap')
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertEqual(len(self.requests), before)

if __name__ == '__main__':
    unittest.main(verbosity=2)
