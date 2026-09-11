#!/usr/bin/env python3
"""Offline ccc-node contract + real Danso/bubblewrap worker integration."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
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
from integrations.ccc_node import DansoRuntime, _failure, _transport


async def collect(session, message='do the bounded task'):
    return [event async for event in session.send_turn(message)]


class Worker(fixture.Fixture, unittest.IsolatedAsyncioTestCase):
    async def test_http_status_reaches_error_event_without_body_or_retry(self):
        self.responses = [(429, b'PRIVATE_BODY')]
        session = await self.runtime.start_or_resume(
            contract.SessionRequest(working_directory=str(self.repo)))
        events = await collect(session)
        errors = [event for event in events if event.kind == 'error']
        self.assertEqual(len(errors), 1)
        self.assertIn('reason=http_status, http_status=429', errors[0].message)
        self.assertNotIn('PRIVATE', errors[0].message)
        self.assertEqual(len(self.requests), 1)

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
        self.assertEqual(events[0].code, 'danso_provider')
        self.assertIn('reported_requests=0', events[0].message)
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
        self.assertEqual(events[0].code, 'danso_session')
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

    async def test_double_cancel_spawn(self):
        self.fake_binary('import time\ntime.sleep(20)\n')
        entered, release = asyncio.Event(), asyncio.Event()
        original, created = asyncio.create_subprocess_exec, []
        async def delayed(*args, **kwargs):
            p = await original(*args, **kwargs)
            created.append(p)
            entered.set()
            await release.wait()
            return p
        s = await self.new_session()
        try:
            with patch('asyncio.create_subprocess_exec', delayed):
                task = asyncio.create_task(collect(s))
                await entered.wait()
                task.cancel()
                await asyncio.sleep(.01)
                task.cancel()
                await asyncio.sleep(.01)
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await task
            self.assertIsNotNone(created[0].returncode, 'created process remains alive after double cancellation')
        finally:
            from integrations.ccc_node import _stop
            await _stop(created[0])
    async def test_repeated_cancel_waits_for_cleanup(self):
        from integrations import ccc_node
        self.fake_binary("import time\nfrom pathlib import Path\nPath('ready').write_text('yes')\ntime.sleep(20)\n")
        entered, release = asyncio.Event(), asyncio.Event()
        original, created = ccc_node._stop, []
        async def delayed_stop(process):
            created.append(process)
            entered.set()
            await release.wait()
            await original(process)
        s = await self.new_session()
        with patch('integrations.ccc_node._stop', delayed_stop):
            task = asyncio.create_task(collect(s))
            async with asyncio.timeout(3):
                while not (self.repo / 'ready').exists():
                    await asyncio.sleep(.01)
                task.cancel()
                await entered.wait()
                task.cancel()
                await asyncio.sleep(.01)
                self.assertFalse(task.done())
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await task
        self.assertIsNotNone(created[0].returncode)
        self.assertIsNone(s._process)
        self.assertFalse(s._active)

    async def test_weak_state_and_symlink_resume_rejected(self):
        weak = self.root / 'weak'
        weak.mkdir(mode=0o755)
        weak.chmod(0o755)  # mkdir mode is masked by umask (077 would yield 0700).
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

    async def test_compaction_setting_validation(self):
        self.assertIsNone(self.runtime.compact_at_bytes)
        for value in (True, False, 8191, 393217, '8192', 8192.0):
            with self.subTest(value=value), self.assertRaises(ValueError):
                DansoRuntime(binary=fixture.BIN, state_directory=self.root / 'state',
                             provider='glm', model='fixture', environment=self.env('glm'),
                             compact_at_bytes=value)
        self.assertEqual(len(self.requests), 0)

    async def test_compacted_session_resume_and_old_id_rejected(self):
        import test_compaction as cp
        self.runtime.compact_at_bytes = 8192
        state = {'actions': 0}
        def serve(body):
            if not body['tools']:
                return fixture.response('glm', text=json.dumps(cp.SUMMARY))
            state['actions'] += 1
            if state['actions'] == 1:
                return cp.set_call_id('glm', fixture.response('glm', [('bash', {
                    'command': "echo once >> effects.txt; printf '%09000d' 0"})]), 'before-compaction')
            return fixture.response('glm', text='finished')
        self.responses.extend([(200, serve)] * 20)
        s = await self.new_session()
        self.assertEqual((await collect(s))[-1].kind, 'completion')
        journal = self.runtime.root / (s.session_id + '.jsonl')
        records = [json.loads(line) for line in journal.read_text().splitlines()]
        self.assertTrue(any(r.get('customType') == 'danso.compaction.v1' for r in records))
        before = journal.read_bytes()
        self.responses.clear()
        self.responses.append((200, fixture.response('glm', text='recalled')))
        runtime = DansoRuntime(binary=fixture.BIN, state_directory=self.runtime.root,
                               provider='glm', model='fixture', environment=self.env('glm'),
                               compact_at_bytes=8192)
        resumed = await runtime.start_or_resume(contract.SessionRequest(
            working_directory=str(self.repo), session_id=s.session_id))
        count = len(self.requests)
        self.assertEqual((await collect(resumed))[-1].kind, 'completion')
        self.assertEqual(len(self.requests), count + 1)
        self.assertTrue(journal.read_bytes().startswith(before))
        self.assertEqual((self.repo / 'effects.txt').read_text(), 'once\n')
        self.responses.append((200, cp.set_call_id('glm', fixture.response('glm', [
            ('write', {'path': 'forbidden', 'content': 'bad'})]), 'before-compaction')))
        events = await collect(resumed)
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertFalse((self.repo / 'forbidden').exists())

    async def test_failed_compaction_emits_error_without_checkpoint(self):
        self.runtime.compact_at_bytes = 8192
        def serve(body):
            if not body['tools']:
                return fixture.response('glm', text='invalid checkpoint')
            return fixture.response('glm', [('bash', {'command': "printf '%09000d' 0"})])
        self.responses.extend([(200, serve)] * 5)
        s = await self.new_session()
        events = await collect(s)
        self.assertEqual([e.kind for e in events], ['error'])
        records = [json.loads(line) for line in
                   (self.runtime.root / (s.session_id + '.jsonl')).read_text().splitlines()]
        self.assertFalse(any(r.get('customType') == 'danso.compaction.v1' for r in records))
        self.assertEqual(events[0].code, 'danso_compaction')

    async def test_request_budget_reports_validated_usage(self):
        self.runtime.max_turns = 1
        self.responses.append((200, fixture.response('glm', [('write', {'path': 'once', 'content': 'ok'})])))
        events = await collect(await self.new_session())
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertEqual(events[0].code, 'danso_request_budget')
        self.assertIn('exit_code=3', events[0].message)
        self.assertIn('reported_requests=1', events[0].message)
        self.assertFalse(events[0].retryable)
        self.assertEqual((self.repo / 'once').read_text(), 'ok')

    async def test_compaction_budget_is_not_summary_validation(self):
        self.runtime.max_turns = 2
        self.runtime.compact_at_bytes = 8192
        self.responses.append((200, fixture.response('glm', [('bash', {'command': "printf '%09000d' 0"})])))
        events = await collect(await self.new_session())
        self.assertEqual(events[0].code, 'danso_request_budget')
        self.assertEqual(len(self.requests), 1)

    async def test_provider_timeout_and_compaction_provider_failure(self):
        import time
        def stalled(body):
            time.sleep(1.3)
            return fixture.response('glm', text='late')
        from integrations.ccc_node import PROVIDERS
        for provider in ('glm', 'openai', 'anthropic'):
            with self.subTest(provider=provider):
                key, endpoint = PROVIDERS[provider]
                environment = {'HOME': str(self.home), 'PATH': '/usr/bin:/bin',
                               key: 'synthetic-key', endpoint: self.env('glm')['DANSO_GLM_BASE_URL']}
                runtime = DansoRuntime(binary=fixture.BIN, state_directory=self.runtime.root,
                                       provider=provider, model='fixture', environment=environment,
                                       provider_timeout_seconds=1)
                self.responses.append((200, stalled))
                session = await runtime.start_or_resume(contract.SessionRequest(working_directory=str(self.repo)))
                events = await collect(session)
                self.assertEqual(events[0].code, 'danso_provider_timeout')
                self.assertIn('phase=before_response_headers', events[0].message)
                self.assertIn('elapsed_ms=', events[0].message)
                self.assertIn('request_bytes=', events[0].message)
                await asyncio.sleep(.4)
        self.runtime.compact_at_bytes = 8192
        self.runtime.provider_timeout = 3
        self.responses.extend([(200, fixture.response('glm', [('bash', {'command': "printf '%09000d' 0"})])),
                               (503, {'error': 'PRIVATE_SUMMARY_RESPONSE'})])
        events = await collect(await self.new_session())
        self.assertEqual(events[0].code, 'danso_provider')
        self.assertNotIn('PRIVATE', repr(events))


    async def test_configuration_failure_and_cli_validation(self):
        self.runtime.environment['DANSO_GLM_BASE_URL'] = 'bad-url'
        events = await collect(await self.new_session())
        self.assertEqual(events[0].code, 'danso_configuration')
        self.assertIn('exit_code=2', events[0].message)
        self.assertEqual(len(self.requests), 0)
        process = await asyncio.create_subprocess_exec(str(fixture.BIN), '--unknown',
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        stdout, stderr = await process.communicate()
        from integrations.ccc_node import _failure
        self.assertEqual(process.returncode, 2)
        self.assertEqual(_failure(stderr, process.returncode).code, 'danso_configuration')
        process = await asyncio.create_subprocess_exec(str(fixture.BIN), '--sandbox', 'bubblewrap', '--cwd', str(self.repo),
                    '--session', str(self.runtime.root / 'invalid.jsonl'), '--provider', 'glm',
                    '--model', 'fixture', '--compact-at-bytes', '1', '-p', 'hello',
                    env=self.env('glm'), stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        stdout, stderr = await process.communicate()
        self.assertEqual(process.returncode, 2)
        self.assertEqual(_failure(stderr, process.returncode).code, 'danso_configuration')
        self.assertEqual(len(self.requests), 0)

    async def test_optional_tool_home_reaches_child_without_replacing_private_home(self):
        tool_home = self.root / 'tool-home'
        tool_home.mkdir()
        self.runtime = DansoRuntime(
            binary=fixture.BIN, state_directory=self.runtime.root, provider='glm',
            model='fixture', environment=self.env('glm'), tool_home=tool_home,
        )
        self.fake_binary("""import json, os, sys
from pathlib import Path
Path('argv.json').write_text(json.dumps(sys.argv))
Path('environment.json').write_text(json.dumps({key: os.environ.get(key) for key in ('HOME', 'PATH')}))
print('done')
usage = {'requests': 1, 'inputTokens': 1, 'outputTokens': 0,
         'cacheReadTokens': 0, 'cacheWriteTokens': 0, 'totalTokens': 1}
for prefix in ('DANSO_USAGE', 'PIRI_USAGE'):
    print(prefix + '=' + json.dumps(usage), file=sys.stderr)
""")
        events = await collect(await self.new_session())
        self.assertEqual(events[-1].kind, 'completion')
        argv = json.loads((self.repo / 'argv.json').read_text())
        self.assertEqual(argv[argv.index('--tool-home') + 1], str(tool_home))
        environment = json.loads((self.repo / 'environment.json').read_text())
        self.assertEqual(environment['HOME'], str(self.home))
        self.assertNotEqual(environment['HOME'], str(tool_home))

    def test_tool_home_is_host_only_and_absolute(self):
        for value, sandbox in (('relative-home', 'host'),
                               (str(self.root / 'tool:home'), 'host'),
                               ('/tool\x00home', 'host'),
                               (str(self.root / 'tool-home'), 'bubblewrap')):
            with self.subTest(value=value, sandbox=sandbox), self.assertRaises(ValueError):
                DansoRuntime(binary=fixture.BIN, state_directory=self.root / 'invalid-tool-home',
                             provider='glm', model='fixture', environment=self.env('glm'),
                             sandbox=sandbox, tool_home=value)

    async def test_truncated_provider_response_keeps_provider_category(self):
        import test_e2e
        runtime = DansoRuntime(binary=fixture.BIN, state_directory=self.runtime.root,
                provider='anthropic', model='fixture', environment={'HOME': str(self.home),
                'PATH': '/usr/bin:/bin', 'ANTHROPIC_API_KEY': 'synthetic',
                'DANSO_ANTHROPIC_BASE_URL': self.env('glm')['DANSO_GLM_BASE_URL']})
        self.responses.append((200, test_e2e.reply([{'type': 'text', 'text': 'PRIVATE_PARTIAL'}], stop='max_tokens')))
        session = await runtime.start_or_resume(contract.SessionRequest(working_directory=str(self.repo)))
        events = await collect(session)
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertEqual(events[0].code, 'danso_provider')
        self.assertNotIn('PRIVATE', repr(events))

    def test_malformed_diagnostics_fall_back_without_text_inference(self):
        from integrations.ccc_node import _failure
        good = json.dumps({'version': 1, 'category': 'request_budget', 'exit_code': 3})
        bad = [None, 'not json', '[]', '"PRIVATE"',
               good.replace('1', 'true', 1), good.replace('3', '2'),
               good.replace('request_budget', 'PRIVATE_CATEGORY'),
               good[:-1] + ', "private": "PRIVATE"}',
               good[:-1] + ', "category": "provider"}',
               '[' * 2000 + '0' + ']' * 2000]
        for payload in bad:
            text = 'PRIVATE provider timed out compaction budget exhausted\n'
            if payload is not None: text += 'DANSO_ERROR=' + payload
            result = _failure(text.encode(), 3)
            self.assertEqual(result.code, 'danso_failed')
            self.assertNotIn('PRIVATE', repr(result))
            self.assertFalse(result.retryable)
        duplicated = ('DANSO_ERROR=' + good + '\n') * 2
        self.assertEqual(_failure(duplicated.encode(), 3).code, 'danso_failed')
        self.assertEqual(_failure(('DANSO_ERROR=' + good).encode(), 124).code, 'danso_timeout')

    async def test_diagnostic_cannot_authorize_success(self):
        diag = json.dumps({'version': 1, 'category': 'provider', 'exit_code': 3})
        self.fake_binary("import sys\nprint('untrusted answer')\nprint(" + repr('DANSO_ERROR=' + diag) + ", file=sys.stderr)\n")
        events = await collect(await self.new_session())
        self.assertEqual([e.kind for e in events], ['error'])
        self.assertNotIn('untrusted answer', repr(events))

    async def test_no_ambient_credential_or_bootstrap_inheritance(self):
        self.assertNotIn('CCC_STATE_DIR', self.runtime.environment)
        self.assertNotIn('PIRI_BOOTSTRAP_CONTEXT_FILE', self.runtime.environment)
        self.assertEqual(set(self.runtime.environment), {'PATH', 'HOME', 'ZAI_API_KEY', 'DANSO_GLM_BASE_URL'})


class TransportDiagnostics(unittest.IsolatedAsyncioTestCase):
    def test_default_and_explicit_provider_timeout(self):
        root = Path(tempfile.mkdtemp(prefix='danso-diagnostic-'))
        try:
            binary = root / 'binary'
            binary.write_text('#!/bin/sh\nexit 0\n')
            binary.chmod(0o700)
            home = root / 'home'
            home.mkdir()
            runtime = DansoRuntime(binary=binary, state_directory=root / 'state', provider='glm',
                                   model='fixture', environment={
                                       'PATH': '/usr/bin:/bin', 'HOME': str(home),
                                       'ZAI_API_KEY': 'synthetic',
                                   })
            self.assertEqual(runtime.provider_timeout, 180)
            override = DansoRuntime(binary=binary, state_directory=root / 'override', provider='glm',
                                     model='fixture', environment={
                                         'PATH': '/usr/bin:/bin', 'HOME': str(home),
                                         'ZAI_API_KEY': 'synthetic',
                                     }, provider_timeout_seconds=37)
            self.assertEqual(override.provider_timeout, 37)
        finally:
            import shutil
            shutil.rmtree(root)

    def test_valid_transport_is_attached_only_to_matching_provider_failure(self):
        record = json.dumps({
            'version': 1, 'phase': 'response_body', 'elapsed_ms': 180001,
            'request_bytes': 30502, 'attempts': 1,
        }, separators=(',', ':'))
        stderr = (f'DANSO_ERROR={{"version":1,"category":"provider_timeout","exit_code":3}}\n'
                  f'DANSO_TRANSPORT={record}\n').encode()
        event = _failure(stderr, 3)
        self.assertEqual(event.code, 'danso_provider_timeout')
        self.assertIn('phase=response_body, elapsed_ms=180001, request_bytes=30502, attempts=1',
                      event.message)
        self.assertNotIn('DANSO_TRANSPORT', event.message)
        self.assertIsNone(_transport(stderr.decode(), 'provider_timeout', 2))

        provider_exit_two = (
            'DANSO_ERROR={"version":1,"category":"provider","exit_code":2}\n'
            f'DANSO_TRANSPORT={record}\n').encode()
        event = _failure(provider_exit_two, 2)
        self.assertEqual(event.code, 'danso_provider')
        self.assertNotIn('phase=', event.message)
        self.assertIsNone(_transport(provider_exit_two.decode(), 'provider', 2))

    def test_malformed_duplicate_unknown_bool_and_private_transport_are_ignored(self):
        base = 'DANSO_ERROR={"version":1,"category":"provider","exit_code":3}\n'
        records = [
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":1,"extra":"PRIVATE"}',
            '{"version":1,"phase":"PRIVATE","elapsed_ms":1,"request_bytes":1}',
            '{"version":1,"phase":"response_body","elapsed_ms":true,"request_bytes":1}',
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":524289}',
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":1,"request_bytes":2}',
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":1,"attempts":0}',
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":1,"attempts":"1"}',
            '{"version":1,"phase":"response_body","elapsed_ms":1,"request_bytes":1',

        ]
        for record in records:
            with self.subTest(record=record):
                event = _failure((base + 'DANSO_TRANSPORT=' + record + '\nPRIVATE_URL').encode(), 3)
                self.assertEqual(event.code, 'danso_provider')
                self.assertNotIn('phase=', event.message)
                self.assertNotIn('PRIVATE', event.message)

    async def test_provider_on_success_is_rejected(self):
        root = Path(tempfile.mkdtemp(prefix='danso-success-diagnostic-'))
        try:
            workspace = root / 'workspace'
            workspace.mkdir()
            home = root / 'home'
            home.mkdir()
            binary = root / 'binary'
            binary.write_text("""#!/usr/bin/python3
import json, sys
print('done')
usage = {'requests': 1, 'inputTokens': 1, 'outputTokens': 0,
         'cacheReadTokens': 0, 'cacheWriteTokens': 0, 'totalTokens': 1}
for prefix in ('DANSO_USAGE', 'PIRI_USAGE'):
    print(prefix + '=' + json.dumps(usage), file=sys.stderr)
print('DANSO_PROVIDER=' + json.dumps({'version': 1, 'phase': 'connect',
      'elapsed_ms': 1, 'request_bytes': 1}), file=sys.stderr)
""")
            binary.chmod(0o700)
            runtime = DansoRuntime(binary=binary, state_directory=root / 'state', provider='glm',
                                   model='fixture', environment={
                                       'PATH': '/usr/bin:/bin', 'HOME': str(home),
                                       'ZAI_API_KEY': 'synthetic',
                                   })
            session = await runtime.start_or_resume(
                contract.SessionRequest(working_directory=str(workspace)))
            events = await collect(session)
            self.assertEqual([event.kind for event in events], ['error'])
            self.assertEqual(events[0].code, 'danso_adapter_error')
        finally:
            import shutil
            shutil.rmtree(root)

    async def test_transport_on_success_is_rejected(self):
        root = Path(tempfile.mkdtemp(prefix='danso-success-diagnostic-'))
        try:
            workspace = root / 'workspace'
            workspace.mkdir()
            home = root / 'home'
            home.mkdir()
            binary = root / 'binary'
            binary.write_text("""#!/usr/bin/python3
import json, sys
print('done')
usage = {'requests': 1, 'inputTokens': 1, 'outputTokens': 0,
         'cacheReadTokens': 0, 'cacheWriteTokens': 0, 'totalTokens': 1}
for prefix in ('DANSO_USAGE', 'PIRI_USAGE'):
    print(prefix + '=' + json.dumps(usage), file=sys.stderr)
print('DANSO_TRANSPORT=' + json.dumps({'version': 1, 'phase': 'connect',
      'elapsed_ms': 1, 'request_bytes': 1}), file=sys.stderr)
""")
            binary.chmod(0o700)
            runtime = DansoRuntime(binary=binary, state_directory=root / 'state', provider='glm',
                                   model='fixture', environment={
                                       'PATH': '/usr/bin:/bin', 'HOME': str(home),
                                       'ZAI_API_KEY': 'synthetic',
                                   })
            session = await runtime.start_or_resume(
                contract.SessionRequest(working_directory=str(workspace)))
            events = await collect(session)
            self.assertEqual([event.kind for event in events], ['error'])
            self.assertEqual(events[0].code, 'danso_adapter_error')
        finally:
            import shutil
            shutil.rmtree(root)


class ProviderDiagnostics(unittest.TestCase):
    def failure(self, record, category='provider', code=3):
        base = 'DANSO_ERROR=' + json.dumps({
            'version': 1, 'category': category, 'exit_code': code}) + '\n'
        return _failure((base + record + '\nPRIVATE_BODY https://private.invalid').encode(), code)

    def test_valid_closed_reasons_and_http_status(self):
        for reason in ('invalid_json', 'response_too_large', 'stream_ended',
                       'invalid_stream', 'unsupported_stream_event', 'response_failed',
                       'response_incomplete', 'response_error'):
            record = 'DANSO_PROVIDER=' + json.dumps({
                'version': 1, 'reason': reason, 'http_status': None,
                'output_tokens_max': None})
            event = self.failure(record)
            self.assertIn('reason=' + reason, event.message)
            self.assertNotIn('http_status=', event.message)
            self.assertNotIn('output_tokens_max=', event.message)
            self.assertNotIn('PRIVATE', event.message)
        for status in (101, 302, 401, 403, 429, 500, 999):
            event = self.failure('DANSO_PROVIDER=' + json.dumps({
                'version': 1, 'reason': 'http_status', 'http_status': status,
                'output_tokens_max': None}))
            self.assertIn(f'reason=http_status, http_status={status}', event.message)
            self.assertIn('No automatic replay.', event.message)

    def test_max_tokens_reason_relays_only_positive_caps(self):
        event = self.failure('DANSO_PROVIDER=' + json.dumps({
            'version': 1, 'reason': 'max_tokens', 'http_status': None,
            'output_tokens_max': 16384}))
        self.assertIn('reason=max_tokens, output_tokens_max=16384', event.message)
        self.assertIn('No automatic replay.', event.message)
        for bad in ({'output_tokens_max': 0}, {'output_tokens_max': None},
                    {'output_tokens_max': -1}, {'output_tokens_max': '16384'},
                    {'http_status': 429, 'output_tokens_max': 16384}):
            record = 'DANSO_PROVIDER=' + json.dumps({
                'version': 1, 'reason': 'max_tokens', **bad})
            event = self.failure(record)
            self.assertNotIn('max_tokens', event.message, bad)

    def test_malformed_records_and_wrong_categories_do_not_leak(self):
        valid = {'version': 1, 'reason': 'http_status', 'http_status': 429,
                 'output_tokens_max': None}
        malformed = [None, [], {}, {**valid, 'version': True},
                     {**valid, 'reason': []}, {**valid, 'reason': 'PRIVATE'},
                     {**valid, 'extra': 'PRIVATE'}, {**valid, 'http_status': True},
                     {**valid, 'http_status': 'PRIVATE'}, {**valid, 'http_status': 99},
                     {**valid, 'http_status': 1000}, {**valid, 'http_status': 200},
                     {**valid, 'http_status': 299}, {**valid, 'http_status': None},
                     {**valid, 'reason': 'invalid_stream'}]
        records = ['DANSO_PROVIDER=' + json.dumps(value) for value in malformed]
        line = 'DANSO_PROVIDER=' + json.dumps(valid)
        records += [line + '\n' + line, 'DANSO_PROVIDER={',
                    'DANSO_PROVIDER={"version":1,"reason":"http_status","http_status":401,"http_status":429}']
        for record in records:
            with self.subTest(record=record):
                event = self.failure(record)
                self.assertEqual(event.code, 'danso_provider')
                self.assertNotIn('reason=', event.message)
                self.assertNotIn('PRIVATE', event.message)
        for category, code in [('provider', 2), ('provider_timeout', 3), ('session', 3),
                               ('run_timeout', 124), ('UNKNOWN', 3)]:
            self.assertNotIn('reason=', self.failure(line, category, code).message)
        self.assertNotIn('reason=', self.failure('').message)


class InterruptedTaskStatus(unittest.TestCase):
    def status(self):
        return dict(version=1, kind='long_task_status', state='paused',
                    session_id='12345678-1234-1234-1234-123456789abc', stage=0, elapsed_ms=12,
                    limits=dict(wall_seconds=60, stage_requests=16, max_requests=10,
                                max_tokens=1000, repeat_limit=3),
                    usage=dict(requests=1, reported_tokens=0, unknown_usage_requests=1),
                    pending=None, resume_allowed=True,
                    recovery=dict(interruption_reason='unknown', interrupted_requests=1,
                                  max_interrupted_requests=3, automatic_resume_allowed=False))

    def test_legacy_and_interruption_status(self):
        from integrations.ccc_node import _task_status
        data = self.status()
        self.assertEqual(_task_status(data).unknown_usage_requests, 1)
        del data['recovery']
        del data['usage']['unknown_usage_requests']
        self.assertEqual(_task_status(data).unknown_usage_requests, 0)

    def test_inconsistent_resume_and_unknown_usage_are_rejected(self):
        from integrations.ccc_node import _task_status
        for patch in ({'state': 'pending_provider'}, {'elapsed_ms': 60000}):
            with self.subTest(patch=patch), self.assertRaises(ValueError):
                _task_status(self.status() | patch)
        data = self.status()
        data['recovery']['automatic_resume_allowed'] = True
        with self.assertRaises(ValueError):
            _task_status(data)
        data = self.status()
        data['usage']['unknown_usage_requests'] = True
        with self.assertRaises(ValueError):
            _task_status(data)

    def test_token_overshoot_is_valid_status_but_cannot_authorize_resume(self):
        from integrations.ccc_node import _task_status
        data = self.status()
        data['usage']['reported_tokens'] = 1001
        with self.assertRaises(ValueError):
            _task_status(data)
        data.update(state='failed', resume_allowed=False)
        self.assertEqual(_task_status(data).reported_tokens, 1001)


if __name__ == '__main__':
    unittest.main(verbosity=2)
