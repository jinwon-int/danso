#!/usr/bin/env python3
"""Offline ccc-node contract + real Danso/bubblewrap worker integration."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import sys
import types
import unittest
from unittest.mock import patch

import test_providers as fixture

ROOT = Path(__file__).resolve().parent.parent
# Test the actual, pinned upstream contract without requiring the Telegram app.
for name in ('telegram_bot', 'telegram_bot.core'):
    sys.modules.setdefault(name, types.ModuleType(name))
source = (Path(os.environ['CCC_NODE_SOURCE']) / 'bridge/core/agent_runtime.py'
          if 'CCC_NODE_SOURCE' in os.environ else ROOT / 'tests/fixtures/ccc_node/agent_runtime.py')
spec = importlib.util.spec_from_file_location('telegram_bot.core.agent_runtime', source)
contract = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = contract
spec.loader.exec_module(contract)
sys.path.insert(0, str(ROOT))
from integrations.ccc_node import DansoRuntime


async def collect(session, message='do the bounded task'):
    return [event async for event in session.send_turn(message)]


class Worker(fixture.Fixture, unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        fixture.Fixture.setUp(self)
        self.runtime = DansoRuntime(binary=fixture.BIN, state_directory=self.root / 'state',
                                    provider='glm', model='fixture', environment=self.env('glm'),
                                    timeout_seconds=10, provider_timeout_seconds=3)

    async def new_session(self, **kwargs):
        return await self.runtime.start_or_resume(contract.SessionRequest(working_directory=str(self.repo), **kwargs))

    async def test_small_task_and_resume(self):
        s = await self.new_session()
        self.responses.extend([(200, fixture.response('glm', [('write', {'path': 'out.txt', 'content': 'done'})])),
                               (200, fixture.response('glm', text='finished'))])
        events = await collect(s)
        self.assertEqual([e.kind for e in events], ['text_delta', 'message_completed', 'result', 'completion'])
        self.assertEqual(events[2].result['usage']['requests'], 2)
        self.assertNotIn('costUsd', events[2].result['usage'])
        self.assertEqual((self.repo / 'out.txt').read_text(), 'done')
        journal = self.runtime.root / (s.session_id + '.jsonl')
        before = journal.read_bytes()
        resumed = await self.new_session(session_id=s.session_id)
        self.responses.append((200, fixture.response('glm', text='recalled')))
        events = await collect(resumed, 'Recall only, no tools')
        self.assertEqual(events[-1].kind, 'completion')
        self.assertTrue(journal.read_bytes().startswith(before))
        self.assertEqual(len(self.requests), 3)
        self.assertEqual(journal.stat().st_mode & 0o777, 0o600)

    async def test_provider_failure_is_terminal_and_private(self):
        s = await self.new_session()
        self.responses.append((503, {'error': 'PRIVATE_PROVIDER_MARKER'}))
        events = await collect(s)
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertFalse(events[0].retryable)
        self.assertNotIn('PRIVATE', repr(events))
        self.assertEqual(len(self.requests), 1)

    async def test_cancel_preserves_uncertain_operation_without_replay(self):
        s = await self.new_session()
        self.responses.append((200, fixture.response('glm', [('bash', {'command': 'echo once >> count; sleep 20'})])))
        task = asyncio.create_task(collect(s))
        async with asyncio.timeout(5):
            while not (self.repo / 'count').exists():
                await asyncio.sleep(.02)
        await s.interrupt()
        events = await task
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertEqual(events[0].code, 'danso_cancelled')
        self.assertIsNone(s._process)
        events = await collect(s, 'continue')
        self.assertEqual(events[0].kind, 'error')
        self.assertEqual(len(self.requests), 1)
        self.assertEqual((self.repo / 'count').read_text(), 'once\n')

    async def test_caller_task_cancellation_cleans_process(self):
        s = await self.new_session()
        self.responses.append((200, fixture.response('glm', [('bash', {'command': 'echo ready > ready; sleep 20'})])))
        task = asyncio.create_task(collect(s))
        async with asyncio.timeout(5):
            while not (self.repo / 'ready').exists():
                await asyncio.sleep(.02)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertIsNone(s._process)

    async def test_whole_run_timeout(self):
        self.runtime.timeout = 1
        s = await self.new_session()
        self.responses.append((200, fixture.response('glm', [('bash', {'command': 'sleep 10'})])))
        events = await collect(s)
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertEqual(events[0].code, 'danso_timeout')

    async def test_distinct_sessions_do_not_share_journal(self):
        a, b = await self.new_session(), await self.new_session()
        self.responses.extend([(200, fixture.response('glm', text='ok'))] * 2)
        ea, eb = await asyncio.gather(collect(a, 'alpha'), collect(b, 'beta'))
        self.assertEqual(ea[-1].kind, 'completion')
        self.assertEqual(eb[-1].kind, 'completion')
        self.assertNotEqual(a.session_id, b.session_id)
        for s in (a, b):
            self.assertTrue((self.runtime.root / (s.session_id + '.jsonl')).exists())

    async def test_unsupported_policy_and_bad_session_rejected_before_dispatch(self):
        for kwargs in ({'memory_environment': {'HOME': '/private'}}, {'approval_policy': 'on-request'},
                       {'sandbox_policy': {'type': 'dangerFullAccess'}}, {'model': 'different'},
                       {'session_id': '../escape'}, {'effort': 'unknown'}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                await self.new_session(**kwargs)
        self.assertEqual(len(self.requests), 0)
        self.assertEqual((await self.runtime.list_models())[0].id, 'fixture')

    def fake_binary(self, source):
        path = self.root / 'fake-danso'
        path.write_text('#!/usr/bin/python3\n' + source)
        path.chmod(0o700)
        self.runtime.binary = str(path)

    async def test_invalid_output_never_emits_success_or_private_error(self):
        self.fake_binary("print('unvalidated answer')\nimport sys\nprint('PRIVATE_BAD_USAGE', file=sys.stderr)\n")
        events = await collect(await self.new_session())
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertNotIn('PRIVATE', repr(events))
        self.assertNotIn('unvalidated answer', repr(events))

    async def test_output_cap_stops_child(self):
        self.fake_binary("import os,time\nos.write(1, b'x' * (1024 * 1024 + 1))\ntime.sleep(20)\n")
        s = await self.new_session()
        events = await asyncio.wait_for(collect(s), 3)
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertIsNone(s._process)

    async def test_interrupt_escalates_and_idle_interrupt_is_noop(self):
        self.fake_binary("import signal,time\nfrom pathlib import Path\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nPath('ready').write_text('yes')\ntime.sleep(20)\n")
        s = await self.new_session()
        await s.interrupt()
        task = asyncio.create_task(collect(s))
        async with asyncio.timeout(3):
            while not (self.repo / 'ready').exists():
                await asyncio.sleep(.02)
            await s.interrupt()
            events = await task
        self.assertEqual(events[0].code, 'danso_cancelled')
        self.assertIsNone(s._process)

    async def test_one_session_serializes_turns(self):
        s = await self.new_session()
        self.responses.extend([(200, fixture.response('glm', text='ok'))] * 2)
        a, b = await asyncio.gather(collect(s, 'first'), collect(s, 'second'))
        self.assertEqual(a[-1].kind, 'completion')
        self.assertEqual(b[-1].kind, 'completion')
        self.assertEqual(len(self.requests), 2)

    async def test_cancel_during_spawn_cleans_created_process(self):
        self.fake_binary("import time\ntime.sleep(20)\n")
        entered, release = asyncio.Event(), asyncio.Event()
        original = asyncio.create_subprocess_exec
        async def delayed_spawn(*args, **kwargs):
            process = await original(*args, **kwargs)
            entered.set()
            await release.wait()
            return process
        s = await self.new_session()
        with patch('asyncio.create_subprocess_exec', delayed_spawn):
            task = asyncio.create_task(collect(s))
            await asyncio.wait_for(entered.wait(), 3)
            task.cancel()
            await asyncio.sleep(0)
            release.set()
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(task, 3)
        self.assertIsNone(s._process)
        self.assertFalse(s._active)

    async def test_weak_state_and_symlink_resume_rejected(self):
        weak = self.root / 'weak'
        weak.mkdir(mode=0o755)
        with self.assertRaises(ValueError):
            DansoRuntime(binary=fixture.BIN, state_directory=weak, provider='glm',
                         model='fixture', environment=self.env('glm'))
        s = await self.new_session()
        target = self.root / 'target'
        target.write_text('private')
        target.chmod(0o600)
        (self.runtime.root / (s.session_id + '.jsonl')).symlink_to(target)
        with self.assertRaises(ValueError):
            await self.new_session(session_id=s.session_id)
        self.assertEqual(len(self.requests), 0)

    async def test_no_ambient_credential_or_bootstrap_inheritance(self):
        self.assertNotIn('CCC_STATE_DIR', self.runtime.environment)
        self.assertNotIn('PIRI_BOOTSTRAP_CONTEXT_FILE', self.runtime.environment)
        self.assertEqual(set(self.runtime.environment), {'PATH', 'HOME', 'ZAI_API_KEY', 'DANSO_GLM_BASE_URL'})


if __name__ == '__main__':
    unittest.main(verbosity=2)
