#!/usr/bin/env python3
"""Checkpoint compaction across real sandboxes and local fake providers only."""
import json
import os
from pathlib import Path
import subprocess
import threading
import signal
from unittest.mock import patch
import live_acceptance as live
import unittest

import test_providers as fixture
import test_e2e as anthropic

SUMMARY = {'objective': 'Keep ORIGINAL_GOAL and finish the task', 'constraints': ['Do not repeat completed effects'],
           'changes': ['effects.txt records completed bash effects'], 'tests': ['prior tool calls succeeded'],
           'pending': ['continue remaining steps']}


def reply(provider, actions=(), text='done'):
    if provider == 'anthropic':
        content = [anthropic.call(n, a, f'call{i}') for i, (n, a) in enumerate(actions)]
        return anthropic.reply(content if actions else [{'type': 'text', 'text': text}],
                               'tool_use' if actions else 'end_turn')
    return fixture.response(provider, actions, text)


def set_call_id(provider, r, ident):
    if provider == 'anthropic':
        r['content'][0]['id'] = ident
    elif provider == 'openai':
        r['output'][-1]['call_id'] = ident
    else:
        r['choices'][0]['message']['tool_calls'][0]['id'] = ident
    return r


class Compaction(fixture.Fixture):
    def env(self, provider):
        if provider == 'anthropic':
            return {'PATH': '/usr/bin:/bin', 'HOME': str(self.home), 'ANTHROPIC_API_KEY': 'synthetic-key',
                    'DANSO_ANTHROPIC_BASE_URL': f'http://127.0.0.1:{self.server.server_port}'}
        return super().env(provider)

    def run_cli(self, provider, *extra, env=None):
        return subprocess.run([str(fixture.BIN), '--cwd', str(self.repo), '--session', str(self.session),
                               '--provider', provider, '--model', 'fixture', '--compact-at-bytes', '8192',
                               '--max-turns', '32', *extra, '-p', 'ORIGINAL_GOAL finish without repeated effects'],
                              capture_output=True, text=True, timeout=25, env=env or self.env(provider))

    def records(self):
        return [json.loads(l) for l in self.session.read_text().splitlines()]

    def checkpoints(self):
        return [e for e in self.records() if e.get('customType') == 'danso.compaction.v1']

    def queue_task(self, provider, rounds=3, bad_summary=None, duplicate=False):
        state = {'actions': 0, 'summaries': 0, 'action_requests': [], 'summary_requests': []}
        def serve(body):
            if not body['tools']:
                state['summaries'] += 1
                state['summary_requests'].append(body)
                return bad_summary if bad_summary is not None else reply(provider, text=json.dumps(SUMMARY))
            state['action_requests'].append(body)
            step = state['actions']
            state['actions'] += 1
            if step >= rounds:
                return reply(provider)
            action = [('bash', {'command': f"echo step{step} >> effects.txt; printf '%09000d' 0"})]
            return set_call_id(provider, reply(provider, action), 'effect0' if duplicate else f'effect{step}')
        self.responses.extend([(200, serve)] * 50)
        return state

    def test_multiple_compactions_and_resume_for_all_providers(self):
        for provider in ('anthropic', 'openai', 'glm'):
            with self.subTest(provider=provider):
                self.session = self.root / f'multi-{provider}.jsonl'
                (self.repo / 'effects.txt').write_text('')
                self.responses.clear()
                state = self.queue_task(provider)
                p = self.run_cli(provider)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual((self.repo / 'effects.txt').read_text(), 'step0\nstep1\nstep2\n')
                self.assertEqual(len(self.checkpoints()), 3)
                self.assertGreaterEqual(state['summaries'], 3)
                for body in state['summary_requests'] + state['action_requests']:
                    # Parsed JSON canonical separators match serde for these ASCII fixtures.
                    self.assertLessEqual(len(json.dumps(body, separators=(',', ':'), ensure_ascii=False).encode()), 8192)
                for body in state['action_requests'][1:]:
                    rendered = json.dumps(body)
                    self.assertIn('Historical checkpoint', rendered)
                    self.assertIn('ORIGINAL_GOAL', rendered)
                    self.assertNotIn('opaque-fixture', rendered)
                    self.assertNotIn('fixture-reasoning', rendered)
                before = self.session.read_bytes()
                count = len(self.requests)
                self.responses.clear()
                self.responses.append((200, reply(provider, text='resumed')))
                p = self.run_cli(provider)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(len(self.requests), count + 1)
                self.assertEqual(self.usage(p)['requests'], 1)
                self.assertTrue(self.session.read_bytes().startswith(before))
                self.assertEqual((self.repo / 'effects.txt').read_text(), 'step0\nstep1\nstep2\n')
                # IDs compacted out of model context must remain forbidden after restart.
                r = set_call_id(provider, reply(provider, [('write', {'path': 'forbidden', 'content': 'x'})]), 'effect0')
                self.responses.append((200, r))
                p = self.run_cli(provider)
                self.assertEqual(p.returncode, 3, p.stderr)
                self.assertIn('duplicate tool call id', p.stderr)
                self.assertFalse((self.repo / 'forbidden').exists())

    def test_live_acceptance_stress_path_offline(self):
        actions = [('read', {'path': 'add.sh'}), ('read', {'path': 'test.sh'}),
                   ('edit', {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'}),
                   ('bash', {'command': 'bash test.sh'}),
                   ('write', {'path': 'report.md', 'content': live.REPORT})]
        step = 0
        def serve(body):
            nonlocal step
            if not body['tools']:
                return reply('glm', text=json.dumps(SUMMARY))
            current = step
            step += 1
            if current < len(actions):
                return set_call_id('glm', reply('glm', [actions[current]]), f'stress{current}')
            return reply('glm', text='done' if current == len(actions) else 'abc123')
        self.responses.extend([(200, serve)] * 50)
        with patch.object(live.secrets, 'token_hex', return_value='abc123'):
            root = live.run(fixture.BIN, 'fixture', self.env('glm'), 'glm', compact_at_bytes=16384)
        result = json.loads((root / 'result.json').read_text())
        self.assertEqual(result['status'], 'passed')
        self.assertGreaterEqual(result['compactions'], 2)
        self.assertEqual(result['runs'][1]['requests'], 1)

    def test_duplicate_id_in_same_run_after_compaction_is_blocked(self):
        self.queue_task('glm', rounds=2, duplicate=True)
        p = self.run_cli('glm')
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertIn('duplicate tool call id', p.stderr)
        self.assertEqual((self.repo / 'effects.txt').read_text(), 'step0\n')
        self.assertEqual(len(self.checkpoints()), 1)

    def test_bad_summary_never_executes_tools_or_commits_checkpoint(self):
        variants = [reply('glm', text='not JSON'),
                    reply('glm', text=json.dumps({'objective': 'missing fields'})),
                    reply('glm', text=json.dumps({**SUMMARY, 'pending': ['x' * 2000]})),
                    reply('glm', [('write', {'path': 'forbidden', 'content': 'x'})])]
        for i, bad in enumerate(variants):
            with self.subTest(variant=i):
                self.session = self.root / f'bad-{i}.jsonl'
                (self.repo / 'effects.txt').write_text('')
                self.responses.clear()
                state = self.queue_task('glm', bad_summary=bad)
                p = self.run_cli('glm')
                self.assertEqual(p.returncode, 3, p.stderr)
                self.assertEqual(len(self.checkpoints()), 0)
                self.assertEqual(state['actions'], 1)
                self.assertFalse((self.repo / 'forbidden').exists())
                self.assertEqual((self.repo / 'effects.txt').read_text(), 'step0\n')

    def test_summary_calls_consume_turn_budget_and_preserve_source(self):
        state = self.queue_task('glm', rounds=3)
        # The overridden run avoids duplicate clap flags.
        p = subprocess.run([str(fixture.BIN), '--cwd', str(self.repo), '--session', str(self.session),
                            '--provider', 'glm', '--model', 'fixture', '--compact-at-bytes', '8192',
                            '--max-turns', '2', '-p', 'ORIGINAL_GOAL'], env=self.env('glm'),
                           capture_output=True, text=True, timeout=20)
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertEqual(state['actions'], 1)
        self.assertEqual(state['summaries'], 0)
        self.assertEqual(len(self.checkpoints()), 0)
        self.assertEqual(self.usage(p)['requests'], 1)

    def test_corrupt_checkpoint_is_rejected_before_dispatch(self):
        for field, value in (('throughId', 'wrong'), ('userEntryId', 'wrong'), ('version', 2), ('summary', {})):
            self.session = self.root / f'corrupt-{field}.jsonl'
            self.responses.clear()
            self.queue_task('glm', rounds=1)
            p = self.run_cli('glm')
            self.assertEqual(p.returncode, 0, p.stderr)
            entries = self.records()
            cp = next(e for e in entries if e.get('customType') == 'danso.compaction.v1')
            cp['data'][field] = value
            self.session.write_text(''.join(json.dumps(e) + '\n' for e in entries))
            count = len(self.requests)
            p = self.run_cli('glm')
            self.assertEqual(p.returncode, 2, p.stderr)
            self.assertEqual(len(self.requests), count)

    def test_utf8_escaped_evidence_is_chunked_without_loss(self):
        output = ('한글"\\' * 3000) + '\n'
        fragments = []
        def serve(body):
            if not body['tools']:
                data = json.loads(body['messages'][-1]['content'])
                fragments.append(data['history_fragment'])
                return reply('glm', text=json.dumps(SUMMARY))
            return reply('glm')
        command = "python3 -c " + __import__('shlex').quote('print(' + repr(output[:-1]) + ')')
        self.responses.append((200, reply('glm', [('bash', {'command': command})])))
        self.responses.extend([(200, serve)] * 30)
        p = self.run_cli('glm')
        self.assertEqual(p.returncode, 0, p.stderr)
        evidence = json.loads(''.join(fragments))
        actual = next(e for e in evidence if e['role'] == 'toolResult')['content'][0]['text']
        self.assertEqual(actual, output)
        self.assertGreater(len(fragments), 1)

    def test_unshrinkable_latest_request_fails_before_summary(self):
        p = subprocess.run([str(fixture.BIN), '--cwd', str(self.repo), '--session', str(self.session),
                            '--provider', 'glm', '--model', 'fixture', '--compact-at-bytes', '8192',
                            '-p', 'x' * 10000], env=self.env('glm'), capture_output=True, text=True, timeout=10)
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertIn('leave no compaction budget', p.stderr)
        self.assertEqual(len(self.requests), 0)
        self.assertEqual(len(self.checkpoints()), 0)

    def test_interrupt_during_summary_leaves_original_resumable(self):
        summarizing = threading.Event()
        release = threading.Event()
        def blocked(body):
            summarizing.set()
            release.wait(10)
            return reply('glm', text=json.dumps(SUMMARY))
        self.responses.extend([(200, reply('glm', [('bash', {'command': "echo done >> effects.txt; printf '%09000d' 0"})])),
                               (200, blocked)])
        p = subprocess.Popen([str(fixture.BIN), '--cwd', str(self.repo), '--session', str(self.session),
                              '--provider', 'glm', '--model', 'fixture', '--compact-at-bytes', '8192',
                              '-p', 'continue'], env=self.env('glm'), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertTrue(summarizing.wait(10))
            p.send_signal(signal.SIGTERM)
            p.communicate(timeout=5)
            self.assertEqual(p.returncode, 143)
        finally:
            release.set()
            if p.poll() is None:
                p.kill(); p.communicate()
        self.assertEqual(len(self.checkpoints()), 0)
        self.responses.clear()
        self.responses.append((200, reply('glm')))
        # Disable new compaction: verify the untouched old history is resumable.
        p = fixture.Fixture.run_cli(self, 'glm')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'effects.txt').read_text(), 'done\n')

    def test_checkpoint_cannot_cover_an_unsettled_prefix(self):
        # The final journal is settled, but this checkpoint interrupts the batch.
        entries = [{'type': 'session', 'version': 3, 'id': 's', 'timestamp': 't', 'cwd': str(self.repo)},
                   {'type': 'message', 'id': 'u', 'message': {'role': 'user', 'content': 'goal'}},
                   {'type': 'message', 'id': 'a', 'message': {'role': 'assistant', 'content': [
                       {'type': 'toolCall', 'id': 'c', 'name': 'read', 'arguments': {'path': 'x'}}]}},
                   {'type': 'custom', 'id': 'cp', 'customType': 'danso.compaction.v1', 'data': {
                       'version': 1, 'throughId': 'a', 'userEntryId': 'u', 'summary': SUMMARY}},
                   {'type': 'custom', 'id': 'st', 'customType': 'danso.operation.v1', 'data': {'toolCallId': 'c', 'state': 'started'}},
                   {'type': 'message', 'id': 'r', 'message': {'role': 'toolResult', 'toolCallId': 'c', 'content': []}},
                   {'type': 'custom', 'id': 'done', 'customType': 'danso.operation.v1', 'data': {'toolCallId': 'c', 'state': 'settled'}}]
        for i, e in enumerate(entries[1:], 1):
            e['parentId'] = entries[i-1]['id'] if i > 1 else None
        self.session.write_text(''.join(json.dumps(e) + '\n' for e in entries))
        p = self.run_cli('glm')
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertIn('unresolved tool operation', p.stderr)
        self.assertEqual(len(self.requests), 0)


if __name__ == '__main__':
    unittest.main(verbosity=2)
