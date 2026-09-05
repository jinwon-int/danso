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
from test_e2e import BIN, reply, call


class Workflow(unittest.TestCase):
    setUp = e2e.Acceptance.setUp
    tearDown = e2e.Acceptance.tearDown
    final = e2e.Acceptance.final
    def test_workflow(self):
        actions = [call('read', {'path': 'add.sh'}, 'r'),
                   call('edit', {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}, 'e'),
                   call('bash', {'command': 'bash test.sh'}, 'b'),
                   call('write', {'path': 'report.md', 'content': 'Addition fixed; tests pass.'}, 'w')]
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

    def test_usage_must_agree(self):
        with self.assertRaises(ValueError):
            live.usage('DANSO_USAGE={}\nPIRI_USAGE={"requests":1}\n')


if __name__ == '__main__':
    unittest.main()
