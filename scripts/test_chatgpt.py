#!/usr/bin/env python3
"""Subscription adapter: synthetic Codex credentials, loopback SSE, real Danso tools."""
import base64
import copy
import socket
import json
import os
import time
import unittest
from test_providers import Fixture, response


def token(exp=None, account='fixture-account'):
    claims = {'exp': int(time.time()) + 3600 if exp is None else exp,
              'https://api.openai.com/auth': {'chatgpt_account_id': account}}
    return 'test.' + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=') + '.synthetic'


def sse(value):
    return ('event: response.completed\ndata: ' + json.dumps({'type': 'response.completed', 'response': value}) + '\n\n').encode()


def item_events(value):
    return b''.join(('data: ' + json.dumps({'type': 'response.output_item.done',
                       'output_index': i, 'item': item}) + '\n\n').encode()
                    for i, item in enumerate(value['output']))


def streamed_items(value, omit_output=False):
    terminal = copy.deepcopy(value)
    terminal['output'] = []
    if omit_output:
        del terminal['output']
    return item_events(value) + sse(terminal)


class ChatGPT(Fixture):
    execution_args = ['--sandbox', 'host']

    def setUp(self):
        super().setUp()
        self.auth_dir = self.root / 'auth'
        self.auth_dir.mkdir(mode=0o700)
        self.auth = self.auth_dir / 'auth.json'
        self.set_auth()

    def set_auth(self, **changes):
        self.auth_data = {'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None,
                          'tokens': {'access_token': token(), 'account_id': 'fixture-account', 'refresh_token': 'REFRESH_SECRET'}}
        self.auth_data.update(changes)
        self.auth.write_text(json.dumps(self.auth_data))
        self.auth.chmod(0o600)

    def env(self, provider):
        return {'PATH': '/usr/bin:/bin', 'HOME': str(self.home),
                'DANSO_CHATGPT_AUTH_FILE': str(self.auth),
                'DANSO_CHATGPT_BASE_URL': f'http://127.0.0.1:{self.server.server_port}/codex'}

    def invoke(self, **kwargs):
        return self.run_cli('openai-codex', **kwargs)

    def assert_private(self, proc):
        output = proc.stdout + proc.stderr
        self.assertNotIn(self.auth_data['tokens']['access_token'], output)
        self.assertNotIn('REFRESH_SECRET', output)

    def test_tools_resume_wire_and_readonly_credentials(self):
        original = self.auth.read_bytes()
        self.responses = [(200, sse(response('openai', [('write', {'path': 'hello', 'content': 'ok'})]))),
                          (200, sse(response('openai'))), (200, sse(response('openai', text='resumed')))]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'hello').read_text(), 'ok')
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(len(self.requests), 3)
        self.assertTrue(all(x == '/codex/responses' for x in self.paths))
        for body, raw_headers in zip(self.requests, self.headers):
            headers = {k.lower(): v for k, v in raw_headers.items()}
            self.assertTrue(body['stream'])
            self.assertFalse(body['store'])
            self.assertNotIn('max_output_tokens', body)
            self.assertEqual(headers['chatgpt-account-id'], 'fixture-account')
            self.assertEqual(headers['originator'], 'danso')
            self.assertEqual(headers['authorization'], 'Bearer ' + self.auth_data['tokens']['access_token'])
        self.assertTrue(any(x.get('type') == 'reasoning' for x in self.requests[2]['input']))
        self.assertEqual(self.auth.read_bytes(), original)
        self.assert_private(p)
        self.assertIn('openai-codex', p.stdout + p.stderr)

    def test_streamed_items_survive_empty_terminal_tools_and_resume(self):
        self.exercise_streamed_items(False)

    def test_streamed_items_survive_omitted_terminal_output(self):
        self.exercise_streamed_items(True)

    def exercise_streamed_items(self, omit_output):
        values = [response('openai', [('write', {'path': 'streamed', 'content': 'ok'})]),
                  response('openai', text='done'), response('openai', text='resumed')]
        self.responses = [(200, streamed_items(v, omit_output)) for v in values]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'streamed').read_text(), 'ok')
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(len(self.requests), 3)
        for item in values[0]['output'] + values[1]['output']:
            self.assertIn(item, self.requests[2]['input'])
        self.assert_private(p)

    def test_streamed_item_conflicts_never_execute_tools(self):
        value = response('openai', [('write', {'path': 'unsafe-stream', 'content': 'bad'})])
        terminal = copy.deepcopy(value)
        terminal['output'] = []
        events = item_events(value)
        gap = ('data: ' + json.dumps({'type': 'response.output_item.done',
               'output_index': 2, 'item': value['output'][1]}) + '\n\n').encode()
        conflict = copy.deepcopy(value)
        conflict['output'][1]['arguments'] = '{"path":"other","content":"bad"}'
        for i, data in enumerate([events + events + sse(terminal), gap + sse(terminal),
                                  events + sse(conflict), events]):
            with self.subTest(i=i):
                self.session = self.root / f'item-failure{i}.jsonl'
                self.responses = [(200, data)]
                p = self.invoke()
                self.assertNotEqual(p.returncode, 0)
                self.assertFalse((self.repo / 'unsafe-stream').exists())
                self.assertFalse((self.repo / 'other').exists())
                self.assert_private(p)

    def test_invalid_credentials_fail_before_dispatch(self):
        for changes in ({'auth_mode': 'apikey'}, {'OPENAI_API_KEY': 'SECRET'}, {'tokens': {}},
                        {'tokens': {'access_token': token(0), 'account_id': 'fixture-account'}},
                        {'tokens': {'access_token': token(account='other'), 'account_id': 'fixture-account'}}):
            with self.subTest(changes=changes):
                self.set_auth(**changes)
                p = self.invoke()
                self.assertNotEqual(p.returncode, 0)
                self.assertEqual(self.requests, [])

    def test_symlink_file_and_parent_rejected(self):
        target = self.auth_dir / 'original'
        self.auth.rename(target)
        self.auth.symlink_to(target)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.auth.unlink()
        target.rename(self.auth)
        link = self.root / 'link'
        link.symlink_to(self.auth_dir)
        env = self.env('')
        env['DANSO_CHATGPT_AUTH_FILE'] = str(link / 'auth.json')
        self.assertNotEqual(self.invoke(env=env).returncode, 0)
        self.assertEqual(self.requests, [])

    def test_permissions_size_and_special_files_rejected(self):
        for mode in (0o644, 0o666):
            self.auth.chmod(mode)
            self.assertNotEqual(self.invoke().returncode, 0)
        self.auth.chmod(0o600)
        self.auth_dir.chmod(0o755)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.auth_dir.chmod(0o700)
        self.auth.write_bytes(b'x' * 65537)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.auth.unlink()
        os.mkfifo(self.auth, 0o600)
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertEqual(self.requests, [])

    def test_no_automatic_discovery_or_platform_fallback(self):
        env = self.env('')
        del env['DANSO_CHATGPT_AUTH_FILE']
        env['OPENAI_API_KEY'] = 'synthetic-platform-key'
        self.assertNotEqual(self.invoke(env=env).returncode, 0)
        self.assertEqual(self.requests, [])

    def test_destination_restricted(self):
        for base in ('https://example.com', 'http://localhost:1234', 'https://chatgpt.com/backend-api/codex?x=1'):
            env = self.env('')
            env['DANSO_CHATGPT_BASE_URL'] = base
            self.assertNotEqual(self.invoke(env=env).returncode, 0)
        self.assertEqual(self.requests, [])

    def test_stream_failures_never_execute_partial_tools(self):
        good = sse(response('openai', [('write', {'path': 'unsafe', 'content': 'bad'})]))
        cases = [good[:-2], b'data: [DONE]\n\n',
                 b'data: {"type":"response.failed","error":"REFRESH_SECRET"}\n\n',
                 b'data: {"type":"future.event"}\n\n',
                 b'event: response.failed\n' + good,
                 b'data: {"type":"response.output_item.done","item":{"type":"function_call"}}\n\n',
                 b'x' * (1024 * 1024 + 1)]
        for i, data in enumerate(cases):
            with self.subTest(i=i):
                self.session = self.root / f'failure{i}.jsonl'
                self.responses = [(200, data)]
                p = self.invoke()
                self.assertNotEqual(p.returncode, 0)
                self.assertFalse((self.repo / 'unsafe').exists())
                self.assert_private(p)

    def test_http_errors_no_retry_or_body_leak(self):
        for status in (401, 403, 429, 500, 302):
            self.session = self.root / f'http{status}.jsonl'
            self.responses = [(status, b'REFRESH_SECRET')]
            before = len(self.requests)
            p = self.invoke()
            self.assertNotEqual(p.returncode, 0)
            self.assertEqual(len(self.requests), before + 1)
            self.assert_private(p)

    def test_terminal_alias_and_incomplete_alias_rejected(self):
        self.responses = [(200, sse(response('openai')).replace(b'response.completed', b'response.done'))]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.session = self.root / 'incomplete-alias.jsonl'
        failed = response('openai', [('write', {'path': 'unsafe', 'content': 'bad'})])
        failed['status'] = 'incomplete'
        self.responses = [(200, sse(failed).replace(b'response.completed', b'response.done'))]
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertFalse((self.repo / 'unsafe').exists())

    def test_account_change_between_requests_stops_run(self):
        def rotate(_body):
            self.set_auth(tokens={'access_token': token(account='other'), 'account_id': 'other'})
            return sse(response('openai', [('read', {'path': 'existing'})]))
        (self.repo / 'existing').write_text('ok')
        self.responses = [(200, rotate)]
        p = self.invoke()
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(len(self.requests), 1)
        self.assertIn('account changed', p.stderr)

    def test_terminal_response_does_not_wait_for_connection_close(self):
        import threading
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen()
        release = threading.Event()
        errors = []
        def serve():
            try:
                conn, _ = listener.accept()
                with conn:
                    conn.settimeout(5)
                    stream = conn.makefile('rb')
                    length = 0
                    while True:
                        line = stream.readline()
                        if line == b'\r\n':
                            break
                        if line.lower().startswith(b'content-length:'):
                            length = int(line.split(b':', 1)[1])
                    stream.read(length)
                    conn.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n')
                    # Split the terminal frame delimiter across HTTP chunks.
                    data = sse(response('openai', text='complete'))
                    for part in (data[:-2], data[-2:-1], data[-1:]):
                        conn.sendall(f'{len(part):x}\r\n'.encode() + part + b'\r\n')
                    release.wait(5)  # Deliberately never send EOF or final HTTP chunk.
            except Exception as exc:
                errors.append(type(exc).__name__)
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            env = self.env('')
            env['DANSO_CHATGPT_BASE_URL'] = f'http://127.0.0.1:{listener.getsockname()[1]}/codex'
            p = self.run_cli('openai-codex', '--provider-timeout-seconds', '1', env=env)
            self.assertEqual(p.returncode, 0, p.stderr)
        finally:
            release.set()
            thread.join(6)
            listener.close()
        self.assertEqual(errors, [])

    def test_crlf_comments_and_done(self):
        self.responses = [(200, b': heartbeat\r\n\r\n' + sse(response('openai')).replace(b'\n', b'\r\n') + b'data: [DONE]\r\n\r\n')]
        p = self.invoke()
        self.assertEqual(p.returncode, 0, p.stderr)


if __name__ == '__main__':
    unittest.main()
