#!/usr/bin/env python3
"""Offline acceptance tests. Fake HTTP provider; real sandbox and process boundary."""
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
import unittest

BIN = Path(os.environ.get('DANSO_BIN', 'target/debug/danso')).resolve()


def reply(content, stop='end_turn'):
    return {'model': 'fixture-model', 'content': content, 'stop_reason': stop,
            'usage': {'input_tokens': 10, 'output_tokens': 5,
                      'cache_read_input_tokens': 2, 'cache_creation_input_tokens': 1}}


def call(name, args, id='call1'):
    return {'type': 'tool_use', 'id': id, 'name': name, 'input': args}


class Acceptance(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='danso-e2e-')
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.session = self.root / 'session.jsonl'
        self.requests = []
        self.responses = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                owner.requests.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                status, body = owner.responses.pop(0)
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.env = {'PATH': '/usr/bin:/bin', 'HOME': str(self.home),
                    'ANTHROPIC_API_KEY': 'fixture-secret-do-not-leak',
                    'DANSO_ANTHROPIC_BASE_URL': f'http://127.0.0.1:{self.server.server_port}'}

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.tmp.cleanup()

    def command(self, *extra):
        return [str(BIN), '--cwd', str(self.repo), '--session', str(self.session),
                '--model', 'fixture-model', *extra, 'do the task']

    def run_cli(self, *extra):
        return subprocess.run(self.command(*extra), env=self.env, text=True,
                              capture_output=True, timeout=15)

    def final(self):
        self.responses.append((200, reply([{'type': 'text', 'text': 'done'}])))

    def tool(self, name, args):
        self.responses.append((200, reply([call(name, args)], 'tool_use')))
        self.final()

    def usage(self, p):
        lines = [l for l in p.stderr.splitlines() if l.startswith('PIRI_USAGE=')]
        self.assertEqual(len(lines), 1, p.stderr)
        return json.loads(lines[0].split('=', 1)[1])

    def results(self):
        return [e['message'] for e in map(json.loads, self.session.read_text().splitlines())
                if e.get('message', {}).get('role') == 'toolResult']

    def test_bash_pipeline_failure_and_explicit_status_handling(self):
        commands = [
            ("python3 -c 'import sys; print(\"FAILED test\"); sys.exit(7)' | tail -n 1", True),
            ("printf 'ok\\n' | cat", False),
            ("false | cat; printf 'later command\\n'", False),
            ("false | cat || printf 'handled\\n'", False),
            ("printf x | (cat; exit 9)", True),
        ]
        self.responses.append((200, reply([
            call('bash', {'command': command}, f'pipeline{i}')
            for i, (command, _) in enumerate(commands)
        ], 'tool_use')))
        self.final()
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipts = self.results()
        self.assertEqual([r['isError'] for r in receipts], [expected for _, expected in commands])
        self.assertIn('FAILED test', str(receipts[0]['content']))
        self.assertIn('later command', str(receipts[2]['content']))
        self.assertIn('handled', str(receipts[3]['content']))
        # The next model request receives failed tool receipts, even though the
        # scripted model may subsequently produce a normal final response.
        wire_results = [block for message in self.requests[1]['messages']
                        for block in message['content'] if isinstance(block, dict)
                        and block.get('type') == 'tool_result']
        self.assertEqual([block['is_error'] for block in wire_results],
                         [expected for _, expected in commands])

    def test_four_tools_context_session_and_resume(self):
        (self.repo / 'AGENTS.md').write_text('PROJECT_SENTINEL')
        skills = self.repo / '.pi/skills/test'
        skills.mkdir(parents=True)
        (skills / 'SKILL.md').write_text('---\nname: fixture\ndescription: SKILL_SENTINEL\n---\nFULL_BODY_HIDDEN')
        actions = [call('write', {'path': 'hello.txt', 'content': 'hello'}, 'w'),
                   call('read', {'path': 'hello.txt'}, 'r'),
                   call('edit', {'path': 'hello.txt', 'oldText': 'hello', 'newText': 'world'}, 'e'),
                   call('bash', {'command': 'cat hello.txt'}, 'b')]
        self.responses.append((200, reply(actions, 'tool_use')))
        self.final()
        p = self.run_cli('--trust-project')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / 'hello.txt').read_text(), 'world')
        self.assertEqual(len(self.results()), 4)
        self.assertTrue(all(not r['isError'] for r in self.results()))
        self.assertEqual([d['name'] for d in self.requests[0]['tools']], ['read', 'bash', 'edit', 'write'])
        self.assertIn('PROJECT_SENTINEL', self.requests[0]['system'])
        self.assertIn('SKILL_SENTINEL', self.requests[0]['system'])
        self.assertNotIn('FULL_BODY_HIDDEN', self.requests[0]['system'])
        usage = self.usage(p)
        self.assertEqual(usage['requests'], 2)
        self.assertEqual(usage['totalTokens'], 36)
        # A settled write is not replayed while resuming; only new usage counts.
        (self.repo / 'hello.txt').write_text('manually changed')
        self.final()
        p = self.run_cli('-p')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout, 'done\n')
        self.assertEqual(self.usage(p)['requests'], 1)
        self.assertEqual((self.repo / 'hello.txt').read_text(), 'manually changed')
        export = os.environ.get('DANSO_TEST_SESSION_EXPORT')
        if export:
            Path(export).write_text(self.session.read_text())

    def test_sandbox_blocks_host_files_env_network_and_symlink(self):
        secret = self.root / 'outside-secret'
        secret.write_text('OUTSIDE_SECRET')
        (self.repo / 'escape').symlink_to(secret)
        command = (f'test -z "${{ANTHROPIC_API_KEY:-}}" && '
                   f'test ! -e {secret} && '
                   f'! (echo x > /dev/tcp/127.0.0.1/{self.server.server_port})')
        self.tool('bash', {'command': command})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertFalse(self.results()[0]['isError'], self.results())
        # New session avoids reusing the mock's call ID.
        self.session = self.root / 'second.jsonl'
        self.tool('write', {'path': 'escape', 'content': 'tampered'})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(self.results()[0]['isError'])
        self.assertEqual(secret.read_text(), 'OUTSIDE_SECRET')

    def test_ambiguous_edit_and_unknown_tool_are_errors(self):
        (self.repo / 'file').write_text('same same')
        self.responses.append((200, reply([
            call('edit', {'path': 'file', 'oldText': 'same', 'newText': 'oops'}, 'e'),
            call('delete', {'path': 'file'}, 'd')], 'tool_use')))
        self.final()
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(all(r['isError'] for r in self.results()))
        self.assertEqual((self.repo / 'file').read_text(), 'same same')

    def test_ranged_read_preserves_content_and_reports_navigation(self):
        text = 'first\r\n한글😀\r\nlast'
        (self.repo / 'lines.txt').write_bytes(text.encode())
        (self.repo / 'empty.txt').write_text('')
        (self.repo / 'many.txt').write_text('row\n' * 202)
        (self.repo / 'big-range.txt').write_text('source line\n' * 1000)
        cases = [({'path': 'lines.txt'}, text),
                 ({'path': 'lines.txt', 'offset': 2, 'limit': 1},
                  '[read: lines 2-2 of 3; next offset 3]\n한글😀\r\n'),
                 ({'path': 'lines.txt', 'limit': 1},
                  '[read: lines 1-1 of 3; next offset 2]\nfirst\r\n'),
                 ({'path': 'lines.txt', 'offset': 3}, '[read: lines 3-3 of 3; EOF]\nlast'),
                 ({'path': 'lines.txt', 'offset': 4}, '[read: no lines at offset 4; 3 total lines; EOF]\n'),
                 ({'path': 'empty.txt', 'limit': 1}, '[read: no lines at offset 1; 0 total lines; EOF]\n'),
                 ({'path': 'many.txt', 'offset': 2},
                  '[read: lines 2-201 of 202; next offset 202]\n' + 'row\n' * 200),
                 ({'path': 'big-range.txt', 'offset': 500, 'limit': 40},
                  '[read: lines 500-539 of 1000; next offset 540]\n' + 'source line\n' * 40)]
        self.responses.append((200, reply([call('read', args, f'r{i}') for i, (args, _) in enumerate(cases)], 'tool_use')))
        self.final()
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(len(self.results()), len(cases))
        for result, (_, expected) in zip(self.results(), cases):
            self.assertFalse(result['isError'])
            self.assertEqual(result['content'][0]['text'], expected)

    def test_ranged_read_rejects_invalid_ranges_and_preserves_file_gates(self):
        (self.repo / 'file').write_text('hello')
        (self.repo / 'large').write_bytes(b'x' * (256 * 1024 + 1))
        outside = self.root / 'outside-range-secret'
        outside.write_text('OUTSIDE_RANGE_SECRET')
        (self.repo / 'link').symlink_to(outside)
        invalid = [{'offset': v} for v in (0, -1, 1.5, None, '1', 2**64)]
        invalid += [{'limit': v} for v in (0, -1, 2001, True, None)]
        args = [{'path': 'file', **value} for value in invalid]
        args += [{'path': 'file', 'offset': 3}, {'path': 'large', 'limit': 1},
                 {'path': 'link', 'limit': 1}]
        self.responses.append((200, reply([call('read', arg, f'r{i}') for i, arg in enumerate(args)], 'tool_use')))
        self.final()
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(len(self.results()), len(args))
        self.assertTrue(all(result['isError'] for result in self.results()), self.results())
        self.assertNotIn('OUTSIDE_RANGE_SECRET', json.dumps(self.results()))
        self.assertEqual((self.repo / 'file').read_text(), 'hello')

    def test_output_limit_and_timeout(self):
        self.responses.append((200, reply([
            call('bash', {'command': 'yes x'}, 'big'),
            call('bash', {'command': 'sleep 10'}, 'slow')], 'tool_use')))
        self.final()
        p = self.run_cli('--tool-timeout-seconds', '1')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertTrue(all(r['isError'] for r in self.results()))
        self.assertIn('64 KiB', self.results()[0]['content'][0]['text'])
        self.assertIn('timed out', self.results()[1]['content'][0]['text'])

    def test_write_creates_nested_utf8_file_with_exact_host_bytes(self):
        content = 'héllo 한글 😀\nsecond line\n'
        self.tool('write', {'path': 'nested/deeper/dir/파일.txt', 'content': content})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        receipt = self.results()[0]
        self.assertFalse(receipt['isError'], receipt)
        target = self.repo / 'nested' / 'deeper' / 'dir' / '파일.txt'
        self.assertTrue(target.is_file())
        self.assertEqual(target.read_bytes(), content.encode('utf-8'))

    def test_write_exactly_256kib_ascii_succeeds(self):
        content = 'a' * (256 * 1024)
        self.tool('write', {'path': 'big.txt', 'content': content})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        receipt = self.results()[0]
        self.assertFalse(receipt['isError'], receipt)
        self.assertEqual((self.repo / 'big.txt').read_bytes(),
                         b'a' * (256 * 1024))

    def test_write_unicode_over_byte_limit_is_rejected_without_side_effects(self):
        # 257 KiB of bytes via 2-byte characters: below the 256 KiB character
        # count a naive length check would allow, but over the byte budget.
        content = 'é' * (257 * 1024 // 2)
        self.assertLess(len(content), 256 * 1024)
        self.assertGreater(len(content.encode('utf-8')), 256 * 1024)
        target = self.repo / 'existing.txt'
        target.write_text('PRE_EXISTING')
        self.session = self.root / 'over-limit.jsonl'  # fresh session: large payload stays within history limits
        self.tool('write', {'path': 'existing.txt', 'content': content})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        receipt = self.results()[0]
        self.assertTrue(receipt['isError'], receipt)
        self.assertIn('byte budget', str(receipt['content']))
        self.assertEqual(target.read_text(), 'PRE_EXISTING')

    def test_provider_error_and_config_error_contract(self):
        self.responses.append((429, {'error': 'do-not-print-provider-body'}))
        p = self.run_cli('-p')
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertEqual(p.stdout, '')
        self.assertNotIn('do-not-print-provider-body', p.stderr)
        self.usage(p)
        self.env['PIRI_BOOTSTRAP_CONTEXT_FILE'] = 'unused'
        p = self.run_cli()
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertEqual(len(self.requests), 1)

    def test_malformed_response_is_request_failure(self):
        self.responses.append((200, {'content': 'invalid'}))
        p = self.run_cli('-p')
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertEqual(p.stdout, '')
        self.usage(p)

    def test_run_timeout_contract(self):
        self.tool('bash', {'command': 'sleep 10'})
        p = self.run_cli('--timeout-seconds', '1')
        self.assertEqual(p.returncode, 124, p.stderr)
        self.usage(p)

    def test_interrupt_leaves_uncertain_operation_and_no_replay(self):
        self.tool('bash', {'command': 'echo once >> count; sleep 1; echo leaked > survivor; sleep 20'})
        p = subprocess.Popen(self.command(), env=self.env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while not (self.repo / 'count').exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue((self.repo / 'count').exists())
            p.send_signal(signal.SIGTERM)
            stdout, stderr = p.communicate(timeout=5)
            self.assertEqual(p.returncode, 143, stderr)
            self.assertEqual(len([l for l in stderr.splitlines() if l.startswith('PIRI_USAGE=')]), 1)
        finally:
            if p.poll() is None:
                p.kill()
                p.communicate()
        before = len(self.requests)
        p = self.run_cli()
        self.assertEqual(p.returncode, 2, p.stderr)
        self.assertIn('manual recovery', p.stderr)
        self.assertEqual(len(self.requests), before)
        self.assertEqual((self.repo / 'count').read_text(), 'once\n')
        time.sleep(1.1)
        self.assertFalse((self.repo / 'survivor').exists(), 'sandbox descendant survived SIGTERM')


if __name__ == '__main__':
    unittest.main(verbosity=2)
