#!/usr/bin/env python3
"""Exercise the opt-in workflow with synthetic credentials and real bubblewrap."""
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import live_acceptance as live
import test_e2e as e2e
import test_providers as providers
from test_e2e import BIN, reply, call


class Workflow(unittest.TestCase):
    setUp = e2e.Acceptance.setUp
    tearDown = e2e.Acceptance.tearDown
    final = e2e.Acceptance.final
    def test_workflow(self):
        actions = [call('read', {'path': 'add.sh'}, 'r'),
                   call('read', {'path': 'test.sh'}, 'r2'),
                   call('edit', {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}, 'e'),
                   call('bash', {'command': 'bash test.sh'}, 'b'),
                   call('write', {'path': 'report.md', 'content': live.REPORT}, 'w')]
        self.responses.append((200, reply(actions, 'tool_use')))
        self.final()
        token = 'abc123'
        self.responses.append((200, reply([{'type': 'text', 'text': token}])))
        with patch.object(live.secrets, 'token_hex', return_value=token):
            root = live.run(BIN, 'fixture-model', {
                'ANTHROPIC_API_KEY': self.env['ANTHROPIC_API_KEY'],
                'DANSO_ANTHROPIC_BASE_URL': self.env['DANSO_ANTHROPIC_BASE_URL']})
        self.assertEqual(json.loads((root / 'result.json').read_text())['status'], 'passed')
        self.assertEqual(len(self.requests), 3)
        self.assertIn(token, json.dumps(self.requests[-1]['messages']))
        for path in root.glob('*'):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)
        self.assertNotIn(self.env['ANTHROPIC_API_KEY'], ''.join(
            p.read_text() for p in root.glob('*') if p.is_file()))

    def test_failed_task_does_not_resume(self):
        self.final()
        with self.assertRaisesRegex(ValueError, 'incorrect source'):
            live.run(BIN, 'fixture-model', {
                'ANTHROPIC_API_KEY': self.env['ANTHROPIC_API_KEY'],
                'DANSO_ANTHROPIC_BASE_URL': self.env['DANSO_ANTHROPIC_BASE_URL']})
        self.assertEqual(len(self.requests), 1)


class MultiProviderWorkflow(providers.Fixture):
    def test_openai_and_glm_workflow(self):
        actions = [('read', {'path': 'add.sh'}), ('read', {'path': 'test.sh'}),
                   ('edit', {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}),
                   ('bash', {'command': 'bash test.sh'}),
                   ('write', {'path': 'report.md', 'content': live.REPORT})]
        for provider in ('openai', 'glm'):
            with self.subTest(provider=provider):
                self.responses.extend([(200, providers.response(provider, actions)),
                                       (200, providers.response(provider)),
                                       (200, providers.response(provider, text='abc123'))])
                with patch.object(live.secrets, 'token_hex', return_value='abc123'):
                    root = live.run(BIN, 'fixture', self.env(provider), provider, 'max')
                result = json.loads((root / 'result.json').read_text())
                self.assertEqual(result['status'], 'passed')
                self.assertEqual(result['provider'], provider)
                self.assertEqual(result['reasoning_effort'], 'max')
                for run in result['runs']:
                    self.assertEqual(run['models'], [f'{provider}/fixture'])


class Safety(unittest.TestCase):
    def test_symlinks_and_existing_files_rejected(self):
        root = Path(tempfile.mkdtemp(prefix='danso-acceptance-safety-'))
        target = root / 'target'
        live.save(target, 'original')
        link = root / 'link'
        link.symlink_to(target)
        with self.assertRaises(OSError):
            live.read_regular(link)
        with self.assertRaises(FileExistsError):
            live.save(link, 'changed')
        with self.assertRaises(FileExistsError):
            live.save(target, 'changed')
        self.assertEqual(target.read_text(), 'original')

    def test_opt_in_gate(self):
        result = subprocess.run(['python3', str(Path(live.__file__)), '--model', 'fixture'],
                                capture_output=True, text=True, env={'PATH': '/usr/bin:/bin'})
        self.assertEqual(result.returncode, 2)
        self.assertIn('--live is required', result.stderr)

    def test_rejects_report_and_temporary_test_replacement(self):
        calls = [
            {'name': 'read', 'arguments': {'path': 'add.sh'}},
            {'name': 'read', 'arguments': {'path': 'test.sh'}},
            {'name': 'edit', 'arguments': {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}},
            {'name': 'bash', 'arguments': {'command': 'bash test.sh'}},
            {'name': 'write', 'arguments': {'path': 'report.md', 'content': live.REPORT}},
        ]
        live.validate_calls(calls)
        calls[-1]['arguments']['content'] = 'x'
        with self.assertRaisesRegex(ValueError, 'unexpected edit, test command, or report'):
            live.validate_calls(calls)
        calls[-1]['arguments']['content'] = live.REPORT
        tamper = {'name': 'write', 'arguments': {'path': 'test.sh', 'content': 'echo ACCEPTANCE_TESTS_PASS'}}
        restore = {'name': 'write', 'arguments': {'path': 'test.sh', 'content': live.TEST}}
        with self.assertRaisesRegex(ValueError, 'unexpected tool sequence'):
            live.validate_calls(calls[:3] + [tamper, calls[3], restore, calls[4]])

    def test_ambient_endpoint_needs_explicit_selection(self):
        for provider, (key, base) in live.PROVIDERS.items():
            result = subprocess.run(['python3', str(Path(live.__file__)), '--live',
                                     '--provider', provider, '--model', 'fixture'],
                                    capture_output=True, text=True,
                                    env={'PATH': '/usr/bin:/bin', key: 'fake-key', base: 'https://example.com'})
            self.assertEqual(result.returncode, 2)
            self.assertIn('explicitly with --base-url', result.stderr)
            self.assertNotIn('fake-key', result.stderr)

    def test_usage_must_agree(self):
        with self.assertRaises(ValueError):
            live.usage('DANSO_USAGE={}\nPIRI_USAGE={"requests":1}\n')


class FailureReporting(unittest.TestCase):
    """Offline failure_stage / completed_runs reporting with mocked subprocess."""

    MARKER = 'confidential-token-marker'
    USAGE = ('DANSO_USAGE={"requests":1,"inputTokens":1,"outputTokens":1,'
             '"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":2}\n'
             'PIRI_USAGE={"requests":1,"inputTokens":1,"outputTokens":1,'
             '"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":2}\n')

    def fake_read_regular(self, path):
        name = Path(path).name
        actions = [
            ('read', {'path': 'add.sh'}), ('read', {'path': 'test.sh'}),
            ('edit', {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}),
            ('bash', {'command': 'bash test.sh'}),
            ('write', {'path': 'report.md', 'content': live.REPORT}),
        ]
        if name == 'session.jsonl':
            calls = [{'type': 'toolCall', 'id': str(i), 'name': tool, 'arguments': args}
                     for i, (tool, args) in enumerate(actions)]
            entries = [{'message': {'role': 'assistant', 'content': calls}}]
            entries.extend({'message': {'role': 'toolResult', 'toolCallId': str(i),
                            'toolName': tool, 'isError': False,
                            'content': 'ACCEPTANCE_TESTS_PASS' if tool == 'bash' else 'ok'}}
                           for i, (tool, _) in enumerate(actions))
            return '\n'.join(json.dumps(e) for e in entries) + '\n'
        if name == 'test.sh':
            return live.TEST + (f"printf '%0{self.threshold * 2}d\\n' 0\n" if self.threshold else '')
        if name == 'report.md':
            return live.REPORT
        if self.bad_source:
            return live.BROKEN
        return live.FIXED + ('#' + 'x' * (self.threshold * 2) + '\n' if self.threshold else '')

    def run_scenario(self, outcomes, *, threshold=None, bad_source=False):
        self.threshold, self.bad_source = threshold, bad_source
        base = Path(tempfile.mkdtemp(prefix='danso-failure-report-'))
        def fake_run(command, **kwargs):
            item = outcomes[runner.call_count - 1]
            if isinstance(item, Exception):
                raise item
            code, stdout, *stderr = item
            return subprocess.CompletedProcess(command, code, stdout=stdout,
                                               stderr=stderr[0] if stderr else self.USAGE)
        with patch.object(live.tempfile, 'mkdtemp', return_value=str(base)), \
             patch.object(live.subprocess, 'run', side_effect=fake_run) as runner, \
             patch.object(live, 'read_regular', self.fake_read_regular), \
             patch.object(live.secrets, 'token_hex', return_value=self.MARKER):
            with self.assertRaises(Exception) as caught:
                live.run('danso-fake', 'fixture-model', {'ANTHROPIC_API_KEY': self.MARKER},
                         compact_at_bytes=threshold)
        return base, runner.call_count, caught.exception

    def check(self, stage, count, invocations, outcomes, **kwargs):
        root, actual, error = self.run_scenario(outcomes, **kwargs)
        text = (root / 'result.json').read_text()
        self.assertEqual(json.loads(text), {'status': 'failed', 'completed_runs': count,
                                          'failure_stage': stage})
        self.assertEqual(actual, invocations)
        self.assertNotIn(self.MARKER, text)
        self.assertIn(stage, live.FAILURE_STAGES)
        return error

    def test_first_run_execution_failure(self):
        self.check('first_run_execution', 0, 1, [(1, self.MARKER, self.MARKER)])

    def test_first_run_validation_failure(self):
        self.check('first_run_validation', 0, 1, [(0, '')])

    def test_invalid_usage_is_validation(self):
        self.check('first_run_validation', 0, 1, [(0, 'answer', self.MARKER)])

    def test_failed_source_validation_does_not_count_usage_as_completion(self):
        self.check('first_run_validation', 0, 1, [(0, 'answer')], bad_source=True)

    def test_resume_execution_failure(self):
        self.check('resume_execution', 1, 2, [(0, 'answer'), (2, self.MARKER)])

    def test_resume_validation_failure(self):
        self.check('resume_validation', 1, 2, [(0, 'answer'), (0, 'wrong context')])

    def test_final_stress_validation_failure(self):
        self.check('final_stress_validation', 2, 2,
                   [(0, 'answer'), (0, self.MARKER)], threshold=8192)

    def test_original_exception_is_preserved_without_leaking_it(self):
        original = subprocess.TimeoutExpired(self.MARKER, 1, output=self.MARKER)
        error = self.check('first_run_execution', 0, 1, [original])
        self.assertIs(error, original)


if __name__ == '__main__':
    unittest.main()
