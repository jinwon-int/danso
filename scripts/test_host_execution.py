#!/usr/bin/env python3
"""Real default host execution; synthetic credentials and loopback providers only."""
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import time
import threading
import unittest

def system_text(body):
    """Anthropic renders the #69 E system split as text blocks."""
    system = body['system']
    if isinstance(system, list):
        return ''.join(block.get('text', '') for block in system)
    return system
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

    def test_host_limits_and_sanitized_development_environment(self):
        self.tool('bash', {'command': (
            "printf '%s\\n' \"$HOME\" \"$PATH\" \"${ANTHROPIC_API_KEY:-}\" "
            "\"${CARGO_BUILD_JOBS:-}\" "
            "\"$(ulimit -v)\" \"$(ulimit -f)\" \"$(ulimit -n)\" "
            "\"$(ulimit -t)\" > limits")})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(
            (self.repo / 'limits').read_text().splitlines(),
            [str(self.home), f'{self.home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin',
             '', '2', '33554432', '4194304', '4096', '900'],
        )

    def test_host_explicit_tool_timeout_override_reaches_host_maximum(self):
        self.tool('bash', {'command': 'true'})
        p = self.run_cli('--tool-timeout-seconds', '3600')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.results()[0]['isError'], self.results())

    def test_host_tool_home_changes_tools_only_and_not_context_discovery(self):
        (self.home / '.pi' / 'agent').mkdir(parents=True)
        (self.home / '.pi' / 'agent' / 'AGENTS.md').write_text('PRIVATE_NATIVE_HOME_SENTINEL')
        tool_home = self.root / 'real-tool-home'
        (tool_home / '.pi' / 'agent').mkdir(parents=True)
        (tool_home / '.pi' / 'agent' / 'AGENTS.md').write_text('TOOL_HOME_MUST_NOT_BE_DISCOVERED')
        self.tool('bash', {'command': 'printf "%s\n%s\n" "$HOME" "$PATH" > tool-home'})
        p = self.run_cli('--tool-home', str(tool_home))
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(
            (self.repo / 'tool-home').read_text().splitlines(),
            [str(tool_home), f'{tool_home}/.cargo/bin:/usr/local/bin:/usr/bin:/bin'],
        )
        self.assertIn('PRIVATE_NATIVE_HOME_SENTINEL', system_text(self.requests[0]))
        self.assertNotIn('TOOL_HOME_MUST_NOT_BE_DISCOVERED', system_text(self.requests[0]))

    def test_tool_home_rejects_invalid_path_and_bubblewrap_before_provider(self):
        invalid = self.run_cli('--tool-home', str(self.root / 'bad:home'))
        self.assertEqual(invalid.returncode, 2, invalid.stderr)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.session.exists())

        bubblewrap = self.run_cli('--sandbox', 'bubblewrap', '--tool-home', str(self.root / 'tool-home'))
        self.assertEqual(bubblewrap.returncode, 2, bubblewrap.stderr)
        self.assertEqual(self.requests, [])
        self.assertFalse(self.session.exists())

    def test_host_allows_large_write_and_loopback_download(self):
        self.download_size = 32 * 1024 * 1024
        port = self.server.server_port
        command = (
            'dd if=/dev/zero of=large.bin bs=1M count=32 status=none && '
            f'/usr/bin/curl --fail --silent http://127.0.0.1:{port}/download -o downloaded.bin && '
            'test "$(stat -c %s large.bin)" -eq 33554432 && '
            'test "$(stat -c %s downloaded.bin)" -eq 33554432'
        )
        self.tool('bash', {'command': command})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'large.bin').stat().st_size, self.download_size)
        self.assertEqual((self.repo / 'downloaded.bin').stat().st_size, self.download_size)

    def test_host_can_compile_large_artifact_when_rustc_is_available(self):
        rustc = shutil.which('rustc')
        if rustc is None:
            self.skipTest('rustc is not on the host development PATH')
        rustc_path = Path(rustc)
        if rustc_path.resolve().name == 'rustup':
            rustup = shutil.which('rustup')
            if rustup is None:
                self.skipTest('rustup proxy has no host resolver')
            resolved = subprocess.run([rustup, 'which', 'rustc'], capture_output=True,
                                      text=True, check=True).stdout.strip()
            rustc_path = Path(resolved)
        if not rustc_path.is_file():
            self.skipTest('host rustc target is unavailable')
        source = (
            '#[used] static DATA: [u8; 20 * 1024 * 1024] = *include_bytes!("payload.bin");\n'
            'fn main() { println!("{}", std::hint::black_box(&DATA).len()); }\n'
        )
        (self.repo / 'main.rs').write_text(source)
        (self.repo / 'payload.bin').write_bytes(b'x' * (20 * 1024 * 1024))
        self.tool('bash', {'command': f'{shlex.quote(str(rustc_path))} main.rs -O -o compiled && ./compiled'})
        # This tool call really compiles a binary carrying a 20 MiB static
        # array with -O. It takes ~3s on an idle machine, but it is memory- and
        # I/O-heavy, so a loaded CI runner has overrun the shared 15s deadline
        # (#95). The assertion here is that the host backend *can* build a
        # large artifact, never that it does so within a given time, so the
        # deadline only has to be generous enough to tell work from a hang.
        p = self.run_cli(timeout=120)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.results()[0]['isError'], self.results())
        self.assertGreater((self.repo / 'compiled').stat().st_size, 16 * 1024 * 1024)

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
            self.assertIn(value, system_text(self.requests[-1]))
            self.assertNotIn('UNTRUSTED_PROJECT_SENTINEL', system_text(self.requests[-1]))
            self.assertNotIn(value, self.session.read_text() + result.stdout + result.stderr)
        self.assertNotIn('MEMORY_SENTINEL_FIRST', system_text(self.requests[-1]))

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


class LongTask(providers.Fixture):
    """Cross-process long-task checks against the real native CLI."""

    def command(self, *extra, prompt=None):
        args = [str(providers.BIN), '--sandbox', 'host', '--cwd', str(self.repo),
                '--session', str(self.session), '--provider', 'glm', '--model',
                'fixture', '--long-task', *extra]
        if prompt is not None:
            args += ['-p', '--', prompt]
        return args

    def run_long(self, *extra, prompt=None, env=None, timeout=15):
        return subprocess.run(self.command(*extra, prompt=prompt),
                              env=env or self.env('glm'), capture_output=True,
                              text=True, timeout=timeout)

    def users(self):
        return [record for record in map(json.loads, self.session.read_text().splitlines())
                if record.get('message', {}).get('role') == 'user']

    def tool_response(self, command):
        return providers.response('glm', [('bash', {'command': command})])

    def status(self):
        return subprocess.run(
            [str(providers.BIN), '--task-status', '--session', str(self.session)],
            env={'PATH': '/usr/bin:/bin', 'HOME': str(self.home)},
            capture_output=True, text=True, timeout=5,
        )

    def assert_recovery(self, result, state, reason, allowed):
        records = [json.loads(line.split('=', 1)[1])
                   for line in result.stderr.splitlines()
                   if line.startswith('DANSO_RECOVERY=')]
        self.assertEqual(records, [{
            'version': 1, 'state': state, 'reason': reason,
            'resume_allowed': allowed,
            'action': 'resume_task' if allowed else 'new_session',
        }], result.stderr)
        errors = [json.loads(line.split('=', 1)[1])
                  for line in result.stderr.splitlines()
                  if line.startswith('DANSO_ERROR=')]
        self.assertEqual(len(errors), 1, result.stderr)
        self.assertEqual(set(errors[0]), {'version', 'category', 'exit_code'})
        self.assertEqual(errors[0]['exit_code'], result.returncode)
        if not allowed:
            self.assertNotIn('requires explicit --resume-task', result.stderr)
            self.assertIn('different --session path', result.stderr)

    def test_cancelled_provider_requires_explicit_resume(self):
        received, release = threading.Event(), threading.Event()

        def delayed(_request):
            received.set()
            release.wait(10)
            return providers.response('glm', text='unobserved result')

        self.responses.append((200, delayed))
        process = subprocess.Popen(
            self.command('--timeout-seconds', '60', prompt='Synthetic interrupted request'),
            env=self.env('glm'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            self.assertTrue(received.wait(5))
            process.send_signal(signal.SIGTERM)
            _, stderr = process.communicate(timeout=5)
            self.assertEqual(process.returncode, 143, stderr)
        finally:
            release.set()
            if process.poll() is None:
                process.kill()
                process.communicate()
        before = self.session.read_bytes()
        status = json.loads(self.status().stdout)
        self.assertEqual(status['state'], 'paused')
        self.assertTrue(status['resume_allowed'])
        self.assertEqual(status['usage']['unknown_usage_requests'], 1)
        self.assertEqual(status['recovery']['interruption_reason'], 'signal_termination')
        refused = self.run_long(prompt='An unrelated new objective')
        self.assertEqual(refused.returncode, 2, refused.stderr)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.session.read_bytes(), before)
        self.responses.append((200, providers.response('glm', text='RESUMED')))
        resumed = self.run_long('--resume-task', '-p')
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(resumed.stdout.strip(), 'RESUMED')
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(len(self.users()), 1)
        status = json.loads(self.status().stdout)
        self.assertEqual(status['usage']['requests'], 2)
        self.assertEqual(status['usage']['unknown_usage_requests'], 1)

    def test_killed_provider_reports_recovery_without_replay_or_journal_change(self):
        received, release = threading.Event(), threading.Event()

        def delayed(_request):
            received.set()
            release.wait(10)
            return providers.response('glm', text='unobserved result')

        self.responses.append((200, delayed))
        process = subprocess.Popen(
            self.command('--timeout-seconds', '60', prompt='Synthetic interrupted request'),
            env=self.env('glm'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            self.assertTrue(received.wait(5))
            process.send_signal(signal.SIGKILL)
            _, stderr = process.communicate(timeout=5)
            self.assertEqual(process.returncode, -9, stderr)
        finally:
            release.set()
            if process.poll() is None:
                process.kill()
                process.communicate()
        before = self.session.read_bytes()
        status = self.status()
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertEqual(json.loads(status.stdout)['state'], 'pending_provider')
        self.assertFalse(json.loads(status.stdout)['resume_allowed'])
        for args, prompt in [((), 'A new message'), (('--resume-task', '-p'), None)]:
            with self.subTest(args=args):
                refused = self.run_long(*args, prompt=prompt)
                self.assertEqual(refused.returncode, 2, refused.stderr)
                self.assert_recovery(refused, 'pending_provider', 'uncertain_work', False)
                self.assertEqual(len(self.requests), 1)
                self.assertEqual(self.session.read_bytes(), before)
        # Omitting --long-task must not bypass either the gate or its advice.
        command = self.command(prompt='Short mode must not bypass the gate')
        command.remove('--long-task')
        refused = subprocess.run(command, env=self.env('glm'), capture_output=True,
                                 text=True, timeout=5)
        self.assertEqual(refused.returncode, 2, refused.stderr)
        self.assert_recovery(refused, 'pending_provider', 'uncertain_work', False)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.session.read_bytes(), before)

    def test_cli_resume_inherits_limits_and_does_not_replay_tools(self):
        self.responses.append((200, self.tool_response('echo once >> effects')))
        first = self.run_long(
            '--timeout-seconds', '60', '--task-stage-requests', '1',
            '--task-max-requests', '5', '--task-max-tokens', '500',
            '--task-repeat-limit', '3', '--task-pause-after-stage', '1',
            '--task-progress', prompt='Perform the task once',
        )
        self.assertEqual(first.returncode, 3, first.stderr)
        self.assertEqual((self.repo / 'effects').read_text(), 'once\n')
        self.assertEqual(len(self.users()), 1)
        progress = [json.loads(line.split('=', 1)[1])
                    for line in first.stderr.splitlines()
                    if line.startswith('DANSO_TASK=')]
        self.assertTrue(any(item['state'] == 'paused' for item in progress), first.stderr)

        before = self.session.read_bytes()
        status = self.status()
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertTrue(json.loads(status.stdout)['resume_allowed'])
        self.assertEqual(self.session.read_bytes(), before)

        refused = self.run_long('--timeout-seconds', '60',
                                prompt='Start over without permission')
        self.assertNotEqual(refused.returncode, 0)
        self.assert_recovery(refused, 'paused', 'explicit_resume_required', True)
        self.assertEqual(self.session.read_bytes(), before)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(len(self.users()), 1)

        self.responses.append((200, providers.response('glm', text='FINISHED')))
        resumed = self.run_long('--resume-task', '-p', '--task-progress')
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(resumed.stdout.strip(), 'FINISHED')
        self.assertEqual(len(self.requests), 2)
        self.assertEqual(len(self.users()), 1)
        self.assertEqual((self.repo / 'effects').read_text(), 'once\n')

        completed = self.run_long('--resume-task', '-p')
        self.assertNotEqual(completed.returncode, 0)
        self.assert_recovery(completed, 'completed', 'terminal_task', False)
        self.assertEqual(len(self.requests), 2)

    def test_sigusr1_pauses_after_settled_tool_and_resume_is_explicit(self):
        self.responses.append((200, self.tool_response(
            'echo started > started; sleep 0.3; echo once >> effects')))
        process = subprocess.Popen(
            self.command('--timeout-seconds', '60', '--task-stage-requests', '4',
                         '--task-max-requests', '5', '--task-max-tokens', '500',
                         '--task-repeat-limit', '3', '--task-progress',
                         prompt='Perform the synthetic task once'),
            env=self.env('glm'), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 5
            while not (self.repo / 'started').exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue((self.repo / 'started').exists())
            process.send_signal(signal.SIGUSR1)
            stdout, stderr = process.communicate(timeout=10)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
        self.assertEqual(process.returncode, 3, stderr)
        progress = [json.loads(line.split('=', 1)[1])
                    for line in stderr.splitlines()
                    if line.startswith('DANSO_TASK=')]
        self.assertTrue(any(item['state'] == 'paused' for item in progress), stderr)
        self.assertEqual((self.repo / 'effects').read_text(), 'once\n')
        self.assertTrue(json.loads(self.status().stdout)['resume_allowed'])

        self.responses.append((200, providers.response('glm', text='FINISHED')))
        resumed = self.run_long('--resume-task', '-p')
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(resumed.stdout.strip(), 'FINISHED')
        self.assertEqual(len(self.requests), 2)
        self.assertEqual((self.repo / 'effects').read_text(), 'once\n')

    def test_sigterm_keeps_uncertain_long_task_non_resumable(self):
        self.responses.append((200, self.tool_response(
            'echo started > started; sleep 1; echo once >> effects')))
        process = subprocess.Popen(
            self.command('--timeout-seconds', '60', '--task-progress',
                         prompt='Perform the synthetic task once'),
            env=self.env('glm'), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        try:
            deadline = time.monotonic() + 5
            while not (self.repo / 'started').exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue((self.repo / 'started').exists())
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=10)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
        self.assertEqual(process.returncode, 143, stderr)
        self.assertFalse((self.repo / 'effects').exists())
        status = self.status()
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertFalse(json.loads(status.stdout)['resume_allowed'])
        before = self.session.read_bytes()
        for args, prompt in [((), 'New message'), (('--resume-task', '-p'), None)]:
            refused = self.run_long(*args, prompt=prompt)
            self.assertEqual(refused.returncode, 2, refused.stderr)
            self.assert_recovery(refused, 'pending_tools', 'uncertain_work', False)
            self.assertEqual(len(self.requests), 1)
            self.assertEqual(self.session.read_bytes(), before)
            self.assertFalse((self.repo / 'effects').exists())

    def test_resume_uses_saved_active_deadline(self):
        self.responses.append((200, self.tool_response(
            'sleep 1.2; echo once > effects')))
        first = self.run_long(
            '--timeout-seconds', '3', '--task-stage-requests', '1',
            '--task-max-requests', '5', '--task-max-tokens', '500',
            '--task-pause-after-stage', '1', prompt='Perform synthetic work',
        )
        self.assertEqual(first.returncode, 3, first.stderr)

        def delayed(_request):
            time.sleep(2.5)
            return providers.response('glm', text='LATE')

        self.responses.append((200, delayed))
        resumed = self.run_long('--resume-task', '-p', timeout=6)
        self.assertEqual(resumed.returncode, 124, resumed.stderr)
        self.assertIn('"category":"run_timeout"', resumed.stderr)
        self.assertEqual(len(self.requests), 2)


if __name__ == '__main__':
    unittest.main(verbosity=2)
