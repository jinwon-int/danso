#!/usr/bin/env python3
"""Managed subscription refresh: synthetic credentials, loopback only."""
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import unittest
from test_providers import BIN, Fixture, response
from test_chatgpt import token, sse


class Refresh(Fixture):
    execution_args = ['--sandbox', 'host']

    def setUp(self):
        super().setUp()
        self.directory = self.root / 'auth'
        self.directory.mkdir(mode=0o700)
        self.source = self.directory / 'auth.json'
        self.managed = self.directory / 'danso-auth.json'
        self.original = {'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None,
                         'tokens': {'access_token': token(0), 'account_id': 'fixture-account', 'refresh_token': 'PRIVATE_REFRESH'}}
        self.source.write_text(json.dumps(self.original))
        self.source.chmod(0o600)

    def adopt(self):
        return subprocess.run([str(BIN), 'auth-adopt', '--source', str(self.source)],
                              capture_output=True, text=True, timeout=5)

    def adopted(self):
        p = self.adopt()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.source.exists())
        self.assertEqual(len(list(self.directory.glob('codex-auth-imported-*.json'))), 1)

    def env(self, provider):
        return {'PATH': '/usr/bin:/bin', 'HOME': str(self.home),
                'DANSO_CHATGPT_AUTH_FILE': str(self.managed),
                'DANSO_CHATGPT_BASE_URL': f'http://127.0.0.1:{self.server.server_port}/codex'}

    def invoke(self, *extra):
        return self.run_cli('openai-codex', *extra)

    def launch(self):
        return subprocess.Popen([str(BIN), '--sandbox', 'host', '--cwd', str(self.repo),
            '--session', str(self.session), '--provider', 'openai-codex', '--model', 'fixture', '-p', 'say done'],
            env=self.env(''), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def fresh(self, **extra):
        return dict(access_token=token(), refresh_token='PRIVATE_ROTATED', **extra)

    def assert_private(self, output):
        for value in ('PRIVATE_REFRESH', 'PRIVATE_ROTATED', self.original['tokens']['access_token']):
            self.assertNotIn(value, output)

    def test_adoption_is_local_private_and_preserves_original(self):
        original = self.source.read_bytes()
        self.adopted()
        self.assertEqual(self.requests, [])
        self.assertEqual(next(self.directory.glob('codex-auth-imported-*.json')).read_bytes(), original)
        self.assertEqual(json.loads(self.managed.read_text())['dansoManagedChatGPT'], 1)
        for path in self.directory.iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertNotEqual(self.adopt().returncode, 0)
        self.assertEqual(self.requests, [])

    def test_rotation_persisted_once_then_resume_without_refresh(self):
        self.adopted()
        updated = self.fresh()
        self.responses = [(200, updated), (200, sse(response('openai'))), (200, sse(response('openai')))]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.paths, ['/codex/oauth/token', '/codex/responses'])
        self.assertEqual(self.requests[0], {'client_id': 'app_EMoamEEZ73f0CkXaXp7hrann', 'grant_type': 'refresh_token', 'refresh_token': 'PRIVATE_REFRESH'})
        state = json.loads(self.managed.read_text())
        self.assertEqual(state['tokens']['refresh_token'], 'PRIVATE_ROTATED')
        self.assertFalse((self.directory / '.danso-refresh-pending').exists())
        self.assertEqual(len(list(self.directory.glob('danso-refresh-receipt-*.json'))), 1)
        self.assertEqual(len(list(self.directory.glob('danso-auth-archive-*.json'))), 1)
        self.assertEqual(self.invoke().returncode, 0)
        self.assertEqual(len(self.requests), 3)
        self.assert_private(p.stdout + p.stderr)

    def test_fresh_credential_does_not_refresh(self):
        self.original['tokens']['access_token'] = token()
        self.source.write_text(json.dumps(self.original))
        self.adopted()
        self.responses = [(200, sse(response('openai')))]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.paths, ['/codex/responses'])

    def test_optional_refresh_token_keeps_previous(self):
        self.adopted()
        self.responses = [(200, {'access_token': token()}), (200, sse(response('openai')))]
        self.assertEqual(self.invoke().returncode, 0)
        self.assertEqual(json.loads(self.managed.read_text())['tokens']['refresh_token'], 'PRIVATE_REFRESH')

    def test_failed_or_malformed_refresh_blocks_automatic_reuse(self):
        cases = [(400, {'error': 'invalid_grant', 'secret': 'PRIVATE_REFRESH'}),
                 (401, {'error': 'PRIVATE_REFRESH'}), (429, {}), (500, {}), (302, {}),
                 (200, b'PRIVATE_REFRESH'), (200, {}),
                 (200, {'access_token': token(0)}),
                 (200, {'access_token': token(account='other')}),
                 (200, {'access_token': token(), 'refresh_token': ''}),
                 (200, b'x' * 65537)]
        for index, reply in enumerate(cases):
            with self.subTest(index=index):
                # Each case owns a fresh store; uncertain stores are never repaired here.
                self.directory = self.root / f'auth{index}'
                self.directory.mkdir(mode=0o700)
                self.source = self.directory / 'auth.json'
                self.managed = self.directory / 'danso-auth.json'
                self.source.write_text(json.dumps(self.original)); self.source.chmod(0o600)
                self.session = self.root / f'case{index}.jsonl'
                self.adopted()
                self.responses = [reply]
                before = len(self.requests)
                p = self.invoke()
                self.assertNotEqual(p.returncode, 0)
                self.assertTrue((self.directory / '.danso-refresh-pending').exists())
                self.assertNotEqual(self.invoke().returncode, 0)
                self.assertEqual(len(self.requests), before + 1)
                self.assert_private(p.stdout + p.stderr)

    def test_second_process_cannot_read_or_rotate_locked_store(self):
        self.adopted()
        entered = threading.Event(); release = threading.Event()
        def hold(_):
            entered.set(); release.wait(8)
            return self.fresh()
        self.responses = [(200, hold), (200, sse(response('openai')))]
        first = self.launch()
        try:
            self.assertTrue(entered.wait(5))
            self.session = self.root / 'second.jsonl'
            second = self.invoke()
            self.assertNotEqual(second.returncode, 0)
            self.assertIn('busy', second.stderr)
            self.assertEqual(len(self.requests), 1)
        finally:
            release.set()
            out, err = first.communicate(timeout=8)
        self.assertEqual(first.returncode, 0, err)
        self.assertEqual(len(self.requests), 2)

    def test_cancel_during_refresh_preserves_uncertainty(self):
        self.adopted()
        entered = threading.Event(); release = threading.Event()
        def hold(_):
            entered.set(); release.wait(8)
            return self.fresh()
        self.responses = [(200, hold)]
        first = self.launch()
        try:
            self.assertTrue(entered.wait(5))
            first.terminate()
            first.communicate(timeout=5)
            self.assertNotEqual(first.returncode, 0)
            self.assertTrue((self.directory / '.danso-refresh-pending').exists())
            self.assertNotEqual(self.invoke().returncode, 0)
            self.assertEqual(len(self.requests), 1)
        finally:
            release.set()
            if first.poll() is None:
                first.kill(); first.communicate()

    def test_local_install_failure_preserves_uncertainty_and_recovery(self):
        self.adopted()
        def lose_current(_):
            self.managed.rename(self.directory / 'external-archive.json')
            return self.fresh()
        self.responses = [(200, lose_current)]
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertTrue((self.directory / '.danso-refresh-pending').exists())
        self.assertTrue((self.directory / 'external-archive.json').exists())
        self.assertTrue(list(self.directory.glob('.danso-auth-new-*.json')))
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertEqual(len(self.requests), 1)

    def test_codex_source_reappearance_blocks_managed_use(self):
        self.adopted()
        self.source.write_text(json.dumps(self.original)); self.source.chmod(0o600)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertEqual(self.requests, [])

    def test_refresh_timeout_preserves_pending_and_does_not_retry(self):
        self.adopted()
        release = threading.Event()
        def hold(_):
            release.wait(4)
            return self.fresh()
        self.responses = [(200, hold)]
        try:
            p = self.invoke('--provider-timeout-seconds', '1')
            self.assertNotEqual(p.returncode, 0)
            self.assertTrue((self.directory / '.danso-refresh-pending').exists())
            self.assertNotEqual(self.invoke().returncode, 0)
            self.assertEqual(len(self.requests), 1)
        finally:
            release.set()

    def test_source_symlink_and_oversize_adoption_fail_closed(self):
        saved = self.directory / 'saved'
        self.source.rename(saved)
        self.source.symlink_to(saved)
        self.assertNotEqual(self.adopt().returncode, 0)
        self.assertFalse(self.managed.exists())
        self.source.unlink(); saved.rename(self.source)
        self.source.write_bytes(b'x' * 65537)
        self.assertNotEqual(self.adopt().returncode, 0)
        self.assertTrue(self.source.exists())
        self.assertFalse(self.managed.exists())
        self.assertEqual(self.requests, [])

    def test_symlink_and_hardlink_lock_rejected_before_adoption(self):
        target = self.directory / 'target'
        target.write_text(''); target.chmod(0o600)
        lock = self.directory / '.danso-auth.lock'
        lock.symlink_to(target)
        self.assertNotEqual(self.adopt().returncode, 0)
        lock.unlink(); os.link(target, lock)
        self.assertNotEqual(self.adopt().returncode, 0)
        self.assertTrue(self.source.exists())
        self.assertFalse(self.managed.exists())
        self.assertEqual(self.requests, [])

    def test_managed_state_permissions_and_pending_entry_fail_closed(self):
        self.adopted()
        self.managed.chmod(0o644)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.managed.chmod(0o600)
        (self.directory / '.danso-refresh-pending').symlink_to('/nonexistent')
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertEqual(self.requests, [])


if __name__ == '__main__':
    unittest.main()
