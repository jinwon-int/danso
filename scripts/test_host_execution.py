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

    def test_explicit_system_memory_refreshes_without_project_trust_or_journal_copy(self):
        context = self.root / 'memory.md'
        context.write_text('MEMORY_SENTINEL_FIRST')
        context.chmod(0o600)
        (self.repo / 'AGENTS.md').write_text('UNTRUSTED_PROJECT_SENTINEL')
        for value in ('MEMORY_SENTINEL_FIRST', 'MEMORY_SENTINEL_SECOND'):
            context.write_text(value)
            self.final()
            result = self.run_cli('--system-context-file', str(context))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(value, self.requests[-1]['system'])
            self.assertNotIn('UNTRUSTED_PROJECT_SENTINEL', self.requests[-1]['system'])
            self.assertNotIn(value, self.session.read_text() + result.stdout + result.stderr)
        self.assertNotIn('MEMORY_SENTINEL_FIRST', self.requests[-1]['system'])

    def test_explicit_system_memory_rejects_unsafe_inputs_before_provider_or_journal(self):
        private = self.root / 'private'; private.mkdir()
        context = private / 'memory.md'; context.write_text('PRIVATE_SENTINEL')
        context.chmod(0o600)
        symlink = self.root / 'linked'; symlink.symlink_to(private, target_is_directory=True)
        link = self.root / 'memory-link'; link.symlink_to(context)
        fifo = self.root / 'fifo'; os.mkfifo(fifo, 0o600)
        for path in (link, symlink / 'memory.md', fifo, private, self.repo / 'AGENTS.md'):
            result = self.run_cli('--system-context-file', str(path))
            self.assertEqual(result.returncode, 2, result.stderr)
        for data, mode in ((b'PRIVATE_SENTINEL', 0o644), (b'x'*32769, 0o600),
                           (b'\xff', 0o600), (b'  ', 0o600)):
            context.write_bytes(data); context.chmod(mode)
            self.assertEqual(self.run_cli('--system-context-file', str(context)).returncode, 2)
        context.write_text('PRIVATE_SENTINEL'); os.link(context, self.root/'hardlink')
        self.assertEqual(self.run_cli('--system-context-file', str(context)).returncode, 2)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.session.exists())

    def test_system_context_rejects_tool_mounts_and_discovery_duplicates(self):
        for path in ('/usr/local/private-memory.md', '/bin/memory.md', '/lib/memory.md', '/lib64/memory.md'):
            result = self.run_cli('--system-context-file', path)
            self.assertEqual(result.returncode, 2)
            self.assertIn('overlaps a tool mount', result.stderr)
        agents = self.home / '.pi' / 'agent' / 'AGENTS.md'
        agents.parent.mkdir(parents=True)
        agents.write_text('DISCOVERED_PRIVATE_SENTINEL')
        agents.chmod(0o600)
        result = self.run_cli('--trust-project', '--system-context-file', str(agents))
        self.assertEqual(result.returncode, 2)
        self.assertIn('overlaps a tool mount', result.stderr)
        self.assertNotIn('DISCOVERED_PRIVATE_SENTINEL', result.stderr)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.session.exists())

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

    def test_signal_immune_descendants_do_not_stall_the_tool_timeout(self):
        # The supervisor sends SIGKILL under PR_SET_CHILD_SUBREAPER, so even a
        # tree that ignores SIGTERM must converge. The post-timeout reap is
        # bounded, so a regression here shows up as a stalled run rather than a
        # leaked process.
        immune = ("setsid /bin/bash -c 'trap \"\" TERM INT HUP; echo $$ > detached.pid; "
                  "sleep 2; echo leaked > survivor; sleep 20' >/dev/null 2>&1 & sleep 20")
        self.tool('bash', {'command': immune})
        started = time.monotonic()
        p = self.run_cli('--tool-timeout-seconds', '1')
        elapsed = time.monotonic() - started
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(self.results()[0]['isError'])
        self.assertIn('timed out', str(self.results()[0]))
        # Generous, but far below the 5s reap grace and the 300s run timeout:
        # an unbounded reap would blow through this.
        self.assertLess(elapsed, 15, f'tool timeout took {elapsed:.2f}s')
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
