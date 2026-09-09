#!/usr/bin/env python3
"""Memory read-write path acceptance (#52 §4.7, #65 §1.1-1.4).

The real CLI runs with `--memory read-write`: the run registers a distill
journal entry, the inline drain extracts through a fake HTTP provider, the
fact passes the gates into the local store, and `danso memory search` finds
it again. Also covers queue-mode drain via the memory CLI and the
HTTP-status-aware failure classification (429 → rate_limited)."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

BIN = Path(os.environ.get('DANSO_BIN', 'target/debug/danso')).resolve()

EXTRACTION_SYSTEM_PREFIX = 'Extract durable memory'
SUMMARIZER_SYSTEM_PREFIX = 'You are a checkpoint summarizer'

EXTRACTION_RESPONSE = {
    'schema_version': 1,
    'provenance': None,  # filled from the extraction input
    'honcho': [{'kind': 'preference', 'text': '에디터는 Helix', 'subject': 'user'}],
    'wiki_candidates': [{'title': '에디터 표준', 'suggested_path': 'pages/editor.md',
                         'summary': 'Helix 표준 정리',
                         'evidence_excerpt': '에디터는 Helix'}],
    'resume': {'last_activity': '작업 완료', 'pending_action': '',
               'awaiting_user': False, 'open_question': '', 'next_step': '',
               'evidence': []},
}


def reply(content, stop='end_turn'):
    return {'model': 'fixture-model', 'content': content, 'stop_reason': stop,
            'usage': {'input_tokens': 10, 'output_tokens': 5,
                      'cache_read_input_tokens': 2, 'cache_creation_input_tokens': 1}}


class Acceptance(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='danso-memory-e2e-')
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.memdir = self.root / 'memory'
        self.memdir.mkdir()
        self.session = self.root / 'session.jsonl'
        self.requests = []
        self.extraction_status = 200
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                owner.requests.append(body)
                system = body.get('system')
                if isinstance(system, list):
                    system = ''.join(block.get('text', '') for block in system)
                body['_system_text'] = system
                if system.startswith(EXTRACTION_SYSTEM_PREFIX):
                    status = owner.extraction_status
                    if status != 200:
                        data = b'{}'
                    else:
                        # Echo the input's provenance; the session id never
                        # appears in the extraction input, only its hash.
                        content = body['messages'][-1]['content']
                        if isinstance(content, list):
                            content = ''.join(b.get('text', '') for b in content)
                        payload = json.loads(content)
                        response = dict(EXTRACTION_RESPONSE)
                        response['provenance'] = {
                            'provider': payload['provider'],
                            'source_thread_hash': payload['source_thread_hash'],
                            'trigger': payload['trigger'],
                            'distilled_at': payload['captured_at'],
                        }
                        data = json.dumps(reply([{'type': 'text', 'text': json.dumps(response)}])).encode()
                    self.send_response(status)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                if system.startswith(SUMMARIZER_SYSTEM_PREFIX):
                    data = json.dumps(reply([{'type': 'text', 'text': '{}'}])).encode()
                else:
                    data = json.dumps(reply([{'type': 'text', 'text': 'done'}])).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.env = {'PATH': '/usr/bin:/bin', 'HOME': str(self.home),
                    'ANTHROPIC_API_KEY': 'fixture-secret-do-not-leak',
                    'DANSO_MEMORY_DIR': str(self.memdir),
                    'DANSO_ANTHROPIC_BASE_URL': f'http://127.0.0.1:{self.server.server_port}'}

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join() if hasattr(self, 'thread') else None
        self.tmp.cleanup()

    def run_danso(self, *args):
        return subprocess.run([str(BIN), *args], env=self.env, text=True,
                              capture_output=True, timeout=30)

    def run_task(self, prompt, *extra):
        return self.run_danso('--cwd', str(self.repo), '--session', str(self.session),
                              '--model', 'fixture-model', '--no-tools', *extra, prompt)

    def facts_file(self):
        return self.memdir / 'global' / 'state' / 'memory-facts.jsonl'

    def journal_files(self):
        journal = self.memdir / 'global' / 'state' / 'distill-journal'
        return sorted(journal.glob('*.json')) if journal.is_dir() else []

    def state_dir(self):
        return self.memdir / 'global' / 'state'

    def test_read_write_inline_run_extracts_commits_and_search_finds_it(self):
        # Run 1: two messages in the session — below the enqueue threshold.
        result = self.run_task('first task', '--memory', 'read-write',
                               '--memory-distill', 'inline')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.journal_files(), [], 'run 1 must not enqueue yet')
        # Run 2: four messages — enqueued and drained inline in-process.
        result = self.run_task('second task', '--memory', 'read-write',
                               '--memory-distill', 'inline')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.journal_files(), [], 'drained job must leave no pending entry')
        facts = self.facts_file().read_text()
        self.assertIn('에디터는 Helix', facts)
        # The store is searchable again (재검색).
        search = self.run_danso('memory', 'search', 'Helix', '--json')
        self.assertEqual(search.returncode, 0, search.stderr)
        self.assertIn('에디터는 Helix', search.stdout)
        # Exactly one extraction request, and the input is newest-first.
        extractions = [r for r in self.requests
                       if r['_system_text'].startswith(EXTRACTION_SYSTEM_PREFIX)]
        self.assertEqual(len(extractions), 1)
        content = extractions[0]['messages'][-1]['content']
        if isinstance(content, list):
            content = ''.join(b.get('text', '') for b in content)
        payload = json.loads(content)
        self.assertFalse(payload['truncated'])
        texts = [(m['role'], m['text'].split()[0]) for m in payload['messages']]
        self.assertEqual(texts, [('assistant', 'done'), ('user', 'second'),
                                 ('assistant', 'done'), ('user', 'first')])
        # The prompt text rides in the input, but the session id never does.
        self.assertNotIn(str(self.session), json.dumps(payload))

    def test_memory_read_mode_writes_nothing(self):
        """#65 §2: `--memory read` is injection-only — no working state,
        checkpoints, session archive, and no distill enqueueing."""
        for prompt in ('first task', 'second task'):
            result = self.run_task(prompt, '--memory', 'read')
            self.assertEqual(result.returncode, 0, result.stderr)
        state = self.state_dir()
        self.assertFalse((state / 'working-state.md').exists())
        self.assertFalse((state / 'checkpoints').exists())
        self.assertFalse((state / 'session-archive').exists())
        self.assertEqual(self.journal_files(), [], 'read mode never enqueues')

    def test_manual_add_close_undoable_and_audited(self):
        """#65 §1.5/§1.6: manual facts go through the rollback transaction —
        one scope lock, an undoable head, and an audit record."""
        init = self.run_danso('memory', 'init')
        self.assertEqual(init.returncode, 0, init.stderr)
        state = self.state_dir()

        def add():
            result = self.run_danso('memory', 'add', '--kind', 'preference',
                                    '--text', '에디터는 Helix', '--subject', 'user',
                                    '--id', 'editor-helix')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('"added": true', result.stdout)

        add()
        head = state / 'memory-rollback' / 'HEAD'
        self.assertTrue(head.exists(), 'manual add leaves an undoable head')
        action_id = head.read_text().strip()
        self.assertEqual(len(action_id), 32)
        # The single scope lock: no second transaction-only lock file.
        self.assertTrue((state / '.memory.lock').exists())
        self.assertFalse((state / 'memory-rollback' / '.transaction.lock').exists())
        audit = (state / 'audit.jsonl').read_text()
        self.assertIn(f'"action_id":"{action_id}"', audit)
        self.assertIn('"facts_added":1', audit)

        rollback = self.run_danso('memory', 'rollback', '--action', action_id)
        self.assertEqual(rollback.returncode, 0, rollback.stderr)
        # Back to the exact pre-add state — the facts file did not exist yet.
        self.assertFalse(self.facts_file().exists())

        # Close is transactional and audited too.
        add()
        close = self.run_danso('memory', 'close', '--fact', 'editor-helix')
        self.assertEqual(close.returncode, 0, close.stderr)
        self.assertIn('"closed": true', close.stdout)
        record = self.facts_file().read_text()
        self.assertIn('editor-helix', record)
        self.assertIn('valid_until', record)
        # A third audited commit with nothing added: the close (#65 §1.6).
        lines = (state / 'audit.jsonl').read_text().splitlines()
        self.assertEqual(len(lines), 3, 'add + add + close are all audited')
        self.assertIn('"facts_added":0', lines[-1])
        self.assertIn('"event":"MemoryCommit"', lines[-1])
        # The close head rolls back too (the fact reopens).
        new_head = (state / 'memory-rollback' / 'HEAD').read_text().strip()
        self.assertNotEqual(new_head, action_id)
        rollback = self.run_danso('memory', 'rollback', '--action', new_head)
        self.assertEqual(rollback.returncode, 0, rollback.stderr)
        record = self.facts_file().read_text()
        self.assertNotIn('valid_until', record)

    def test_drain_writes_wiki_queue_audit_and_resume_provenance(self):
        """#65 §1.6/§1.7/§2: the wiki queue is written, the audit ledger
        records the commit and the job, and resume.md carries the echoed
        provenance header."""
        import hashlib
        result = self.warmup_and_run('queued task')
        self.assertEqual(result.returncode, 0, result.stderr)
        job_record = json.loads(self.journal_files()[0].read_text())
        job_id = job_record['job_id']
        drain = self.run_danso('memory', 'drain', '--provider', 'anthropic',
                               '--model', 'fixture-model')
        self.assertEqual(drain.returncode, 0, drain.stderr)
        state = self.state_dir()

        queue_file = state / 'wiki-candidates' / f'{job_id}.json'
        self.assertTrue(queue_file.exists(), 'wiki candidates are queued')
        queue = json.loads(queue_file.read_text())
        self.assertEqual(queue['schema'], 'danso-wiki-candidate-v1')
        self.assertEqual(queue['job_id'], job_id)
        self.assertEqual(queue['candidates'][0]['title'], '에디터 표준')

        audit = (state / 'audit.jsonl').read_text()
        self.assertIn('"event":"MemoryCommit"', audit)
        self.assertIn('"event":"DistillJob"', audit)
        self.assertIn('"status":"extracted"', audit)

        resume = (state / 'resume.md').read_text()
        thread_hash = hashlib.sha256(
            job_record['session_id'].encode()).hexdigest()
        self.assertIn(f'provider=danso thread_hash={thread_hash}', resume)
        self.assertIn(f"trigger={job_record['trigger']}", resume)
        self.assertIn('distilled_at=', resume)
        self.assertTrue(
            resume.startswith('<!-- ccc-node:distill schema=1'),
            'resume starts with the audited provenance header')

        # The queue is immutable: a re-drain never rewrites the record.
        queue_file.write_text('{"schema":"tampered"}\n')
        result = self.warmup_and_run('second queued task')
        self.assertEqual(result.returncode, 0, result.stderr)
        # The second cycle enqueues two jobs (the warmup run crosses the
        # threshold too); only the job whose transcript hash matches the
        # LIVE session extracts — the stale older one dead-letters with
        # `transcript-changed` (§4.7), exactly as designed.
        import hashlib
        live_hash = hashlib.sha256(self.session.read_bytes()).hexdigest()
        records = [json.loads(f.read_text()) for f in self.journal_files()]
        self.assertEqual(len(records), 2)
        matching = [r for r in records if r['transcript_sha256'] == live_hash]
        self.assertEqual(len(matching), 1, 'exactly one job matches the live session')
        second_id = matching[0]['job_id']
        self.assertNotEqual(second_id, job_id)
        # --max-jobs 2 so both claim orders converge: whichever job claim
        # reaches first, the stale one ends dead-lettered and the live one
        # extracted, and the journal ends empty.
        drain = self.run_danso('memory', 'drain', '--provider', 'anthropic',
                               '--model', 'fixture-model', '--max-jobs', '2')
        self.assertEqual(drain.returncode, 0, drain.stderr + drain.stdout)
        self.assertEqual(queue_file.read_text(), '{"schema":"tampered"}\n',
                         'the first queue record is never rewritten')
        self.assertTrue((state / 'wiki-candidates' / f'{second_id}.json').exists())
        self.assertEqual(self.journal_files(), [], 'both jobs settled')
        dead_dir = state / 'distill-journal' / 'dead'
        dead_reasons = [json.loads(f.read_text()).get('dead_letter_reason')
                        for f in dead_dir.glob('*.json')] if dead_dir.exists() else []
        self.assertIn('transcript-changed', dead_reasons,
                      'the stale job dead-letters (§4.7)')

    def warmup_and_run(self, prompt, *extra):
        """First run seeds two session messages (below the enqueue threshold);
        the second run crosses it."""
        first = self.run_task('warmup', '--memory', 'read-write')
        self.assertEqual(first.returncode, 0, first.stderr)
        return self.run_task(prompt, '--memory', 'read-write', *extra)

    def test_queue_mode_leaves_job_and_memory_drain_completes_it(self):
        result = self.warmup_and_run('queued task')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.journal_files()), 1, 'queue mode keeps the job')
        drain = self.run_danso('memory', 'drain', '--provider', 'anthropic',
                               '--model', 'fixture-model')
        self.assertEqual(drain.returncode, 0, drain.stderr)
        self.assertIn('에디터는 Helix', self.facts_file().read_text())
        self.assertEqual(self.journal_files(), [], 'drain must complete the job')

    def test_drain_classifies_http_status_and_rate_limited_cooldown(self):
        self.extraction_status = 429
        result = self.warmup_and_run('failing task')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.journal_files()), 1)
        drain = self.run_danso('memory', 'drain', '--provider', 'anthropic',
                               '--model', 'fixture-model')
        self.assertEqual(drain.returncode, 0, drain.stderr)
        record = json.loads(self.journal_files()[0].read_text())
        self.assertEqual(record['last_error_class'], 'rate_limited',
                         '429 must classify as rate_limited (#65 §1.4)')
        self.assertNotEqual(record['retry_after'], None)
        # The failed job lands a DistillJob audit record with its class.
        audit = (self.state_dir() / 'audit.jsonl').read_text()
        self.assertIn('"status":"failed"', audit)
        self.assertIn('"error_class":"rate_limited"', audit)
        # After the cooldown passes the job drains successfully.
        record['retry_after'] = '2026-01-01T00:00:00+00:00'
        path = self.journal_files()[0]
        path.write_text(json.dumps(record) + '\n')
        self.extraction_status = 200
        drain = self.run_danso('memory', 'drain', '--provider', 'anthropic',
                               '--model', 'fixture-model')
        self.assertEqual(drain.returncode, 0, drain.stderr)
        self.assertIn('에디터는 Helix', self.facts_file().read_text())
        self.assertEqual(self.journal_files(), [])


if __name__ == '__main__':
    unittest.main()
