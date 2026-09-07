#!/usr/bin/env python3
"""Real loopback HTTP gate tests; no external provider or credentials."""
import asyncio
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import eval_dispatch as e


def response():
    return {'id': 'resp_fixture', 'object': 'response', 'created_at': 0,
            'status': 'completed', 'error': None, 'model': 'gpt-6-astra',
            'output': [{'type': 'message', 'id': 'msg_fixture', 'status': 'completed', 'role': 'assistant',
                        'content': [{'type': 'output_text', 'text': 'done', 'annotations': []}]}],
            'usage': {'input_tokens': 3, 'output_tokens': 2, 'total_tokens': 5,
                      'input_tokens_details': {'cached_tokens': 1},
                      'output_tokens_details': {'reasoning_tokens': 0}}}


def request():
    return {'model': 'gpt-6-astra', 'reasoning': {'effort': 'medium'}, 'max_output_tokens': 4096,
            'store': False, 'input': 'PRIVATE_TASK', 'tools': []}


def sse_response():
    final = response()
    message = final['output'][0]
    events = [
        {'type':'response.created', 'response':{**final, 'status':'in_progress', 'output':[], 'usage':None}},
        {'type':'response.output_item.added', 'output_index':0, 'item':{**message,'content':[], 'status':'in_progress'}},
        {'type':'response.content_part.added', 'output_index':0, 'item_id':message['id'], 'content_index':0,
         'part':{'type':'output_text','text':'','annotations':[]}},
        {'type':'response.output_text.delta','output_index':0,'item_id':message['id'],'content_index':0,'delta':'done'},
        {'type':'response.output_text.done','output_index':0,'item_id':message['id'],'content_index':0,'text':'done'},
        {'type':'response.output_item.done','output_index':0,'item':message},
        {'type':'response.completed','response':final},
    ]
    return b''.join(b'data: '+json.dumps({**event,'sequence_number':i}).encode()+b'\n\n'
                    for i,event in enumerate(events))


class GateTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.root = Path(self.tmp.name)
        self.seen = []; self.reply_status = 200; self.delay = 0; self.streaming = False
        self.payload = json.dumps(response()).encode()
        self.trailing = b''
        self.server = await asyncio.start_server(self.upstream, '127.0.0.1', 0)
        self.endpoint = ('127.0.0.1', self.server.sockets[0].getsockname()[1])
        self.gates = []

    async def asyncTearDown(self):
        for gate in self.gates:
            if gate.fd >= 0: await gate.close()
        self.server.close(); await self.server.wait_closed()
        self.tmp.cleanup()

    async def upstream(self, reader, writer):
        try:
            line, fields = await e.headers(reader)
            raw = await e.body(reader, fields)
            self.seen.append((line, fields, json.loads(raw)))
            await asyncio.sleep(self.delay)
            if self.streaming:
                payload = sse_response()
                writer.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n')
                for offset in range(0, len(payload), 17):
                    chunk = payload[offset:offset+17]
                    writer.write(f'{len(chunk):x}\r\n'.encode()+chunk+b'\r\n')
                writer.write(b'0\r\n\r\n')
            else:
                writer.write((f'HTTP/1.1 {self.reply_status} Result\r\nContent-Type: application/json\r\n'
                              f'Content-Length: {len(self.payload)}\r\n\r\n').encode()+self.payload)
            writer.write(self.trailing)
            await writer.drain()
        except (OSError, asyncio.IncompleteReadError):
            pass
        finally:
            await e.close_writer(writer)

    async def gate(self, **kwargs):
        gate = e.DispatchGate(self.endpoint, self.root / f'job-{len(self.gates)}', model='gpt-6-astra', **kwargs)
        self.gates.append(gate)
        return await gate.start()

    async def call(self, gate, data=None, token=None):
        reader, writer = await asyncio.open_connection('127.0.0.1', gate.port)
        raw = json.dumps(request() if data is None else data).encode()
        auth = gate.token if token is None else token
        writer.write((f'POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {auth}\r\n'
                      f'Content-Length: {len(raw)}\r\n\r\n').encode()+raw)
        await writer.drain()
        try:
            line, fields = await e.headers(reader)
            return int(line.split()[1]), await e.body(reader, fields)
        finally:
            writer.close(); await writer.wait_closed()

    async def test_budget_is_shared_by_concurrent_calls_and_retries(self):
        gate = await self.gate(request_limit=3)
        self.delay = .1
        replies = await asyncio.gather(*(self.call(gate) for _ in range(8)))
        self.assertEqual(sorted(s for s,_ in replies), [200]*3+[429]*5)
        self.assertEqual((await self.call(gate))[0], 429)
        self.assertEqual(len(self.seen), 3)
        self.assertEqual(gate.summary()['total_tokens'], 15)
        self.assertEqual(gate.summary()['reserved_attempts'], 3)

    async def test_failures_consume_slots_and_unknown_usage_stays_unknown(self):
        gate = await self.gate(request_limit=2)
        self.reply_status = 500
        self.assertEqual((await self.call(gate))[0], 500)
        self.reply_status = 200
        self.assertEqual((await self.call(gate))[0], 200)
        self.assertEqual((await self.call(gate))[0], 429)
        self.assertEqual(len(self.seen), 2)
        self.assertIsNone(gate.summary()['total_tokens'])

    async def test_chunked_sse_is_forwarded_and_counted_once(self):
        gate = await self.gate(); self.streaming = True
        data = request(); data['stream'] = True
        status, raw = await self.call(gate, data)
        self.assertEqual(status, 200); self.assertIn(b'response.completed', raw)
        self.assertEqual(gate.summary()['total_tokens'], 5)

    async def test_policy_and_auth_fail_before_upstream(self):
        gate = await self.gate()
        for field, value in [('model','other'), ('store', True), ('max_output_tokens', True),
                             ('reasoning', {'effort':'high'}), ('background', True),
                             ('tools', [{'type':'web_search'}])]:
            data=request(); data[field]=value
            self.assertEqual((await self.call(gate, data))[0], 400)
        self.assertEqual((await self.call(gate, token='wrong'))[0], 400)
        self.assertEqual(self.seen, []); self.assertEqual(gate.rows, [])

    async def test_private_durable_journal_and_no_forwarded_client_auth(self):
        gate = await self.gate()
        self.assertEqual((await self.call(gate))[0], 200)
        raw = (gate.evidence/'dispatch.jsonl').read_text()
        rows = list(map(json.loads, raw.splitlines()))
        self.assertEqual([r['state'] for r in rows], ['started','settled'])
        self.assertNotIn('PRIVATE_TASK', raw); self.assertNotIn(gate.token, raw)
        self.assertNotIn('authorization', self.seen[0][1])
        self.assertEqual((gate.evidence/'dispatch.jsonl').stat().st_mode & 0o777, 0o600)
        with self.assertRaises(FileExistsError):
            e.DispatchGate(self.endpoint, gate.evidence, model='gpt-6-astra')

    async def test_fsync_failure_stops_dispatch_and_poisons_gate(self):
        gate = await self.gate()
        with patch.object(e.os, 'fsync', side_effect=OSError('fixture')):
            self.assertEqual((await self.call(gate))[0], 502)
        self.assertTrue(gate.closed); self.assertEqual(self.seen, [])
        self.assertFalse(gate.summary()['complete']); self.assertIsNone(gate.summary()['total_tokens'])

    async def test_timeout_and_close_leave_uncertain_attempt_without_replay(self):
        gate = await self.gate(request_seconds=1); self.delay = 2
        self.assertEqual((await self.call(gate))[0], 502)
        self.assertFalse(gate.summary()['complete']); self.assertEqual(len(self.seen), 1)
        self.assertIsNone(gate.summary()['total_tokens'])

    async def test_two_jobs_have_independent_budgets_and_tokens(self):
        first, second = await self.gate(request_limit=1), await self.gate(request_limit=1)
        self.assertEqual((await self.call(second, token=first.token))[0], 400)
        for gate in (first,second):
            self.assertEqual((await self.call(gate))[0], 200)
            self.assertEqual((await self.call(gate))[0], 429)
        self.assertEqual(len(self.seen), 2)

    async def test_close_cancels_inflight_request_and_retains_uncertainty(self):
        gate = await self.gate(); self.delay = 2
        caller = asyncio.create_task(self.call(gate))
        for _ in range(100):
            if self.seen: break
            await asyncio.sleep(.01)
        self.assertEqual(len(self.seen), 1)
        rows = list(map(json.loads,(gate.evidence/'dispatch.jsonl').read_text().splitlines()))
        self.assertEqual(rows[-1]['state'],'started')
        self.assertIsNone(rows[-1]['dispatch_attempted'])
        await asyncio.wait_for(gate.close(),1)
        await asyncio.gather(caller,return_exceptions=True)
        await gate.close()  # idempotent cleanup
        self.assertFalse(gate.summary()['complete']); self.assertIsNone(gate.summary()['total_tokens'])
        self.assertFalse(gate.tasks); self.assertEqual(gate.fd,-1)

    async def test_oversized_response_never_becomes_terminal_success(self):
        gate = await self.gate(); self.payload = b'x'*(e.BODY_CAP+1)
        self.assertEqual((await self.call(gate))[0],502)
        self.assertFalse(gate.summary()['complete']); self.assertIsNone(gate.summary()['total_tokens'])

    async def test_duplicate_and_oversized_request_lengths_rejected(self):
        gate = await self.gate()
        for lengths in (b'Content-Length: 2\r\nContent-Length: 2\r\n',
                        f'Content-Length: {e.BODY_CAP+1}\r\n'.encode()):
            reader,writer=await asyncio.open_connection('127.0.0.1',gate.port)
            writer.write(b'POST /v1/responses HTTP/1.1\r\nAuthorization: Bearer '+gate.token.encode()+b'\r\n'+lengths+b'\r\n{}')
            await writer.drain()
            line,_=await e.headers(reader)
            self.assertIn('400',line)
            await e.close_writer(writer)
        self.assertEqual(self.seen,[])

    def test_usage_rejects_missing_duplicate_terminal_and_invalid_counters(self):
        for values in ((True,2,3), (3,2,6), (3,-1,2)):
            data=response();data['usage']=dict(zip(('input_tokens','output_tokens','total_tokens'),values))
            self.assertIsNone(e.usage(json.dumps(data).encode(),'application/json',200))
        self.assertIsNone(e.usage(b'{}','application/json',200))
        frame=b'data: '+json.dumps({'type':'response.completed','response':response()}).encode()+b'\n\n'
        self.assertIsNone(e.usage(frame+frame,'text/event-stream',200))
        self.assertIsNone(e.usage(frame[:-1],'text/event-stream',200))

    async def test_empty_and_rejected_only_jobs_remain_incomplete(self):
        gate = await self.gate()
        self.assertFalse(gate.summary()['complete'])
        self.assertIsNone(gate.summary()['total_tokens'])
        self.assertEqual((await self.call(gate, token='wrong'))[0], 400)
        await gate.close()
        summary = json.loads((gate.evidence/'summary.json').read_text())
        self.assertFalse(summary['complete'])
        self.assertIsNone(summary['total_tokens'])

    async def test_concurrent_close_survives_cancelled_waiter(self):
        gate = await self.gate(); self.delay = 2
        caller = asyncio.create_task(self.call(gate))
        for _ in range(100):
            if self.seen: break
            await asyncio.sleep(.01)
        self.assertEqual(len(self.seen), 1)
        first = asyncio.create_task(gate.close())
        second = asyncio.create_task(gate.close())
        await asyncio.sleep(0)
        first.cancel()
        await asyncio.wait_for(second, 1)
        await asyncio.gather(first, caller, return_exceptions=True)
        await gate.close()
        self.assertEqual(gate.fd, -1)
        self.assertFalse(gate.tasks)
        self.assertTrue((gate.evidence/'summary.json').is_file())

    async def test_malformed_status_and_trailing_response_bytes_rejected(self):
        for status, trailing, streaming in [('0200', b'', False),
                                             (200, b'GARBAGE', False),
                                             (200, b'GARBAGE', True)]:
            gate = await self.gate()
            self.reply_status, self.trailing, self.streaming = status, trailing, streaming
            self.assertEqual((await self.call(gate))[0], 502)
            self.assertFalse(gate.summary()['complete'])
            self.assertIsNone(gate.summary()['total_tokens'])

    async def test_start_failure_closes_journal_and_prevents_restart(self):
        gate = e.DispatchGate(self.endpoint, self.root/'failed', model='gpt-6-astra')
        fd = gate.fd
        with patch.object(e.asyncio, 'start_server', side_effect=OSError('fixture')):
            with self.assertRaises(OSError):
                await gate.start()
        self.assertEqual(gate.fd, -1)
        with self.assertRaises(OSError): os.fstat(fd)
        with self.assertRaises(ValueError): await gate.start()
        await gate.close()
        self.assertFalse(gate.summary()['complete'])

    async def test_real_danso_cli_uses_gate(self):
        gate=await self.gate()
        workspace=self.root/'workspace';workspace.mkdir()
        binary=Path(os.environ.get('DANSO_BIN','target/debug/danso')).resolve()
        process=await asyncio.create_subprocess_exec(str(binary),'--sandbox','bubblewrap','--provider','openai','--model','gpt-6-astra',
            '--reasoning-effort','medium','--cwd',str(workspace),'--session',str(self.root/'session.jsonl'),'-p','do task',
            env={'PATH':'/usr/bin:/bin','HOME':str(self.root),'OPENAI_API_KEY':gate.token,
                 'DANSO_OPENAI_BASE_URL':f'http://127.0.0.1:{gate.port}/v1'},
            stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
        out,err=await asyncio.wait_for(process.communicate(),10)
        self.assertEqual(process.returncode,0,err); self.assertEqual(out.strip(),b'done')
        self.assertEqual(gate.summary()['dispatch_attempts'],1)


if __name__ == '__main__':
    unittest.main()
