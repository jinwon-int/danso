#!/usr/bin/env python3
"""Offline GPT/GLM wire and runtime acceptance; real bubblewrap, fake keys."""
import copy
import http.server
import itertools
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

IDS = itertools.count()
BIN = Path(os.environ.get('DANSO_BIN', 'target/debug/danso')).resolve()


def response(provider, actions=(), text='done', reasoning=True):
    if provider == 'openai':
        serial = next(IDS)
        output = []
        if reasoning:
            output.append({'type': 'reasoning', 'id': f'rs_{serial}', 'summary': [], 'encrypted_content': 'opaque-fixture'})
        for i, (name, args) in enumerate(actions):
            output.append({'type': 'function_call', 'id': f'fc_{i}', 'call_id': f'call{i}',
                           'name': name, 'arguments': json.dumps(args), 'status': 'completed'})
        if not actions:
            output.append({'type': 'message', 'id': f'msg_{serial}', 'role': 'assistant', 'status': 'completed',
                           'content': [{'type': 'output_text', 'text': text, 'annotations': []}]})
        return {'model': 'fixture', 'status': 'completed', 'output': output,
                'usage': {'input_tokens': 10, 'output_tokens': 5, 'input_tokens_details': {'cached_tokens': 2}, 'total_tokens': 15}}
    message = {'role': 'assistant', 'content': None if actions else text}
    if reasoning:
        message['reasoning_content'] = 'fixture-reasoning'
    if actions:
        message['tool_calls'] = [{'type': 'function', 'id': f'call{i}',
                                 'function': {'name': name, 'arguments': json.dumps(args)}}
                                for i, (name, args) in enumerate(actions)]
    return {'model': 'fixture', 'choices': [{'index': 0, 'message': message,
             'finish_reason': 'tool_calls' if actions else 'stop'}],
            'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'prompt_tokens_details': {'cached_tokens': 2}, 'total_tokens': 15}}


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='danso-providers-')
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.session = self.root / 'session.jsonl'
        self.requests = []
        self.responses = []
        self.headers = []
        self.paths = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                owner.requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                owner.headers.append(dict(self.headers))
                owner.paths.append(self.path)
                status, body = owner.responses.pop(0) if owner.responses else (500, {})
                if callable(body):
                    body = body(owner.requests[-1])
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                if status == 302:
                    self.send_header('Location', f'http://127.0.0.1:{owner.server.server_port}/redirected')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.tmp.cleanup()

    def env(self, provider):
        prefix = 'OPENAI' if provider == 'openai' else 'GLM'
        key_name = 'OPENAI_API_KEY' if provider == 'openai' else 'ZAI_API_KEY'
        return {'PATH': '/usr/bin:/bin', 'HOME': str(self.home), key_name: 'synthetic-key',
                f'DANSO_{prefix}_BASE_URL': f'http://127.0.0.1:{self.server.server_port}/api'}

    def run_cli(self, provider, *extra, env=None):
        return subprocess.run([str(BIN), '--cwd', str(self.repo), '--session', str(self.session),
                               '--provider', provider, '--model', 'fixture', *extra, '-p', 'do task'],
                              capture_output=True, text=True, timeout=15, env=env or self.env(provider))

    def usage(self, p):
        a = [json.loads(l.split('=', 1)[1]) for l in p.stderr.splitlines() if l.startswith('DANSO_USAGE=')]
        b = [json.loads(l.split('=', 1)[1]) for l in p.stderr.splitlines() if l.startswith('PIRI_USAGE=')]
        self.assertEqual(len(a), 1, p.stderr)
        self.assertEqual(a, b)
        return a[0]


class Providers(Fixture):
    def test_roundtrip_resume_reasoning_auth_usage_and_sandbox(self):
        for provider in ('openai', 'glm'):
            with self.subTest(provider=provider):
                self.session = self.root / f'{provider}.jsonl'
                self.requests.clear()
                self.responses.extend([(200, response(provider, [
                    ('write', {'path': 'hello.txt', 'content': 'hello'}),
                    ('read', {'path': 'hello.txt'}),
                    ('edit', {'path': 'hello.txt', 'oldText': 'hello', 'newText': 'world'}),
                    ('bash', {'command': 'test -z "${OPENAI_API_KEY:-}${ZAI_API_KEY:-}" && cat hello.txt'})])),
                    (200, response(provider))])
                p = self.run_cli(provider, '--reasoning-effort', 'max')
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual((self.repo / 'hello.txt').read_text(), 'world')
                results = [e['message'] for e in map(json.loads, self.session.read_text().splitlines())
                           if e.get('message', {}).get('role') == 'toolResult']
                self.assertEqual(len(results), 4)
                self.assertTrue(all(not r['isError'] for r in results))
                usage = self.usage(p)
                self.assertEqual((usage['requests'], usage['inputTokens'], usage['cacheReadTokens'], usage['totalTokens']), (2, 16, 4, 30))
                self.assertEqual(usage['models'], [f'{provider}/fixture'])
                self.assertEqual({k.lower(): v for k, v in self.headers[-1].items()}['authorization'], 'Bearer synthetic-key')
                self.assertNotIn('x-api-key', {k.lower() for k in self.headers[-1]})
                self.assertEqual(self.paths[-1], '/api/responses' if provider == 'openai' else '/api/chat/completions')
                self.assertEqual(len(self.requests[0]['tools']), 4)
                if provider == 'openai':
                    self.assertFalse(self.requests[0]['store'])
                    self.assertEqual(self.requests[0]['reasoning'], {'effort': 'max'})
                    self.assertIn('reasoning.encrypted_content', self.requests[0]['include'])
                    self.assertIn('opaque-fixture', json.dumps(self.requests[1]['input']))
                    outputs = [i for i in self.requests[1]['input'] if i.get('type') == 'function_call_output']
                    self.assertEqual([i['call_id'] for i in outputs], ['call0', 'call1', 'call2', 'call3'])
                else:
                    self.assertEqual(self.requests[0]['thinking'], {'type': 'enabled', 'clear_thinking': False})
                    self.assertEqual(self.requests[0]['reasoning_effort'], 'max')
                    self.assertIn('fixture-reasoning', json.dumps(self.requests[1]['messages']))
                    outputs = [i for i in self.requests[1]['messages'] if i['role'] == 'tool']
                    self.assertEqual([i['tool_call_id'] for i in outputs], ['call0', 'call1', 'call2', 'call3'])
                before = self.session.read_bytes()
                (self.repo / 'hello.txt').write_text('resume sentinel')
                self.responses.append((200, response(provider, text='resumed')))
                p = self.run_cli(provider)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(p.stdout, 'resumed\n')
                self.assertEqual(self.usage(p)['requests'], 1)
                self.assertTrue(self.session.read_bytes().startswith(before))
                self.assertEqual((self.repo / 'hello.txt').read_text(), 'resume sentinel')
                self.assertNotIn('synthetic-key', self.session.read_text() + p.stderr + p.stdout)

    def test_invalid_responses_never_execute_partial_batch(self):
        for provider in ('openai', 'glm'):
            base = response(provider, [('write', {'path': 'must-not-exist', 'content': 'bad'})])
            variants = []
            if provider == 'openai':
                for status in ('incomplete', 'failed', 'in_progress'):
                    r = copy.deepcopy(base); r['status'] = status; variants.append(r)
                r = copy.deepcopy(base); r['output'].append({'type': 'unknown'}); variants.append(r)
                r = copy.deepcopy(base); r['output'][-1]['arguments'] = '{'; variants.append(r)
                r = copy.deepcopy(base); r['output'][0].pop('encrypted_content'); variants.append(r)
                r = copy.deepcopy(base); r['output'].append(r['output'][-1]); variants.append(r)
                r = copy.deepcopy(base); r['output'][-1]['call_id'] = ''; variants.append(r)
            else:
                for reason in ('length', 'sensitive', 'network_error', 'stop'):
                    r = copy.deepcopy(base); r['choices'][0]['finish_reason'] = reason; variants.append(r)
                r = copy.deepcopy(base); r['choices'][0]['message']['tool_calls'][0]['function']['arguments'] = '[]'; variants.append(r)
                r = copy.deepcopy(base); r['choices'][0]['message']['tool_calls'] *= 2; variants.append(r)
                r = copy.deepcopy(base); r['choices'] *= 2; variants.append(r)
            for i, r in enumerate(variants):
                with self.subTest(provider=provider, variant=i):
                    self.session = self.root / f'bad-{provider}-{i}.jsonl'
                    self.responses.append((200, r))
                    p = self.run_cli(provider)
                    self.assertEqual(p.returncode, 3, p.stderr)
                    self.assertFalse((self.repo / 'must-not-exist').exists())

    def test_http_errors_bounds_and_no_redirect_or_retry(self):
        for provider in ('openai', 'glm'):
            for status, body in ((429, b'SENSITIVE_BODY'), (302, b'SENSITIVE_BODY'), (200, b'{' + b'x' * (1024 * 1024)), (200, b'not json')):
                with self.subTest(provider=provider, status=status, size=len(body)):
                    self.session = self.root / f'http-{len(self.requests)}.jsonl'
                    count = len(self.requests)
                    self.responses.append((status, body))
                    p = self.run_cli(provider)
                    self.assertEqual(p.returncode, 3, p.stderr)
                    self.assertEqual(len(self.requests), count + 1)
                    self.assertNotIn('SENSITIVE_BODY', p.stderr)
                    self.assertNotIn('synthetic-key', p.stderr)
                    self.assertEqual(self.usage(p)['requests'], 0)

    def test_config_and_history_errors_before_dispatch(self):
        for provider in ('openai', 'glm'):
            env = self.env(provider)
            base_var = next(k for k in env if k.endswith('BASE_URL'))
            for i, url in enumerate(('http://example.com', 'https://user:secret@example.com',
                                     'https://example.com?key=secret', 'https://example.com#secret')):
                self.session = self.root / f'config-{provider}-{i}.jsonl'
                env[base_var] = url
                p = self.run_cli(provider, env=env)
                self.assertEqual(p.returncode, 2, p.stderr)
                self.assertNotIn('secret', p.stderr)
            self.assertEqual(len(self.requests), 0)
            self.session = self.root / f'history-{provider}.jsonl'
            self.responses.append((200, response(provider)))
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 0, p.stderr)
            entries = list(map(json.loads, self.session.read_text().splitlines()))
            entries[-1]['message']['content'] = [{'type': 'image'}]
            self.session.write_text(''.join(json.dumps(e) + '\n' for e in entries))
            count = len(self.requests)
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 2, p.stderr)
            self.assertEqual(len(self.requests), count)
            # Clear captures before next provider's config checks.
            self.requests.clear()

    def test_request_cap_and_unresolved_recovery_before_dispatch(self):
        for provider in ('openai', 'glm'):
            self.session = self.root / f'large-{provider}.jsonl'
            self.responses.append((200, response(provider, text='x' * (530 * 1024))))
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 0, p.stderr)
            count = len(self.requests)
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 2, p.stderr)
            self.assertIn('512 KiB', p.stderr)
            self.assertEqual(len(self.requests), count)
            self.session = self.root / f'unresolved-{provider}.jsonl'
            self.responses.append((200, response(provider)))
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 0, p.stderr)
            entries = list(map(json.loads, self.session.read_text().splitlines()))
            entries[-1]['message']['content'] = [{'type': 'toolCall', 'id': 'orphan',
                                                  'name': 'write', 'arguments': {'path': 'x', 'content': 'x'}}]
            self.session.write_text(''.join(json.dumps(e) + '\n' for e in entries))
            count = len(self.requests)
            p = self.run_cli(provider)
            self.assertEqual(p.returncode, 2, p.stderr)
            self.assertIn('unresolved tool operation', p.stderr)
            self.assertEqual(len(self.requests), count)

    def test_aggregate_usage_overflow_is_normal_failure(self):
        for provider in ('openai', 'glm'):
            for extra in (1, 2):
                with self.subTest(provider=provider, extra=extra):
                    self.session = self.root / f'overflow-{provider}-{extra}.jsonl'
                    first = response(provider, [('read', {'path': 'hello'})])
                    second = response(provider)
                    key_in, key_out, details = (('input_tokens', 'output_tokens', 'input_tokens_details')
                                               if provider == 'openai' else
                                               ('prompt_tokens', 'completion_tokens', 'prompt_tokens_details'))
                    first['usage'] = {key_in: 2**64 - 2, key_out: 1, details: {'cached_tokens': 0}}
                    second['usage'] = {key_in: extra, key_out: 0, details: {'cached_tokens': 0}}
                    (self.repo / 'hello').write_text('hello')
                    self.responses.extend([(200, first), (200, second)])
                    p = self.run_cli(provider)
                    self.assertEqual(p.returncode, 3, p.stderr)
                    self.assertIn('usage overflow', p.stderr)
                    self.assertNotIn('panicked', p.stderr)
                    usage = self.usage(p)
                    self.assertEqual(usage['requests'], 1)
                    self.assertEqual(usage['totalTokens'], 2**64 - 1)

    def test_glm_accepts_documented_object_arguments(self):
        r = response('glm', [('write', {'path': 'object-args', 'content': 'ok'})])
        r['choices'][0]['message']['tool_calls'][0]['function']['arguments'] = {'path': 'object-args', 'content': 'ok'}
        self.responses.extend([(200, r), (200, response('glm'))])
        p = self.run_cli('glm')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'object-args').read_text(), 'ok')

    def test_openai_saved_output_cannot_disagree_with_history(self):
        self.responses.append((200, response('openai')))
        p = self.run_cli('openai')
        self.assertEqual(p.returncode, 0, p.stderr)
        entries = list(map(json.loads, self.session.read_text().splitlines()))
        entries[-1]['message']['dansoOpenAIOutput'][-1]['content'][0]['text'] = 'tampered'
        self.session.write_text(''.join(json.dumps(e) + '\n' for e in entries))
        p = self.run_cli('openai')
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertEqual(len(self.requests), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
