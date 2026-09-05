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


if __name__ == '__main__':
    unittest.main()
