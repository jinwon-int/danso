#!/usr/bin/env python3
"""Real CLI/sandbox progress timing, privacy and opt-in compatibility."""
import json
import selectors
import subprocess
import unittest

import test_e2e as e2e


class Progress(unittest.TestCase):
    setUp = e2e.Acceptance.setUp
    tearDown = e2e.Acceptance.tearDown
    command = e2e.Acceptance.command
    run_cli = e2e.Acceptance.run_cli
    final = e2e.Acceptance.final
    tool = e2e.Acceptance.tool

    def test_native_start_arrives_before_tool_finishes(self):
        self.tool('bash', {'command': 'sleep 2; echo PRIVATE_TOOL_OUTPUT'})
        process = subprocess.Popen(self.command('--progress-jsonl'), env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        records = []
        buffer = b''
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        try:
            found = False
            while not found:
                self.assertTrue(selector.select(5), 'progress did not arrive')
                chunk = process.stdout.read1(65536)
                self.assertTrue(chunk, 'CLI exited before progress')
                buffer += chunk
                while b'\n' in buffer:
                    line, buffer = buffer.split(b'\n', 1)
                    record = json.loads(line); records.append(record)
                    if record.get('type') == 'danso_progress' and record['phase'] == 'started':
                        found = True
            self.assertIsNone(process.poll(), 'progress was buffered until exit')
            out, err = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, err)
            records.extend(map(json.loads, (buffer + out).splitlines()))
            progress = [r for r in records if r.get('type') == 'danso_progress']
            self.assertEqual([r['phase'] for r in progress], ['started', 'settled'])
            self.assertEqual([r['sequence'] for r in progress], [1, 1])
            self.assertTrue(progress[-1]['success'])
            self.assertNotIn('PRIVATE_TOOL_OUTPUT', json.dumps(progress))
            self.assertNotIn('sleep', json.dumps(progress))
            self.assertNotIn('danso_progress', self.session.read_text())
        finally:
            selector.close()
            if process.poll() is None:
                process.kill()
            process.communicate()

    def test_progress_reports_tool_failure_without_claiming_task_failure(self):
        self.tool('bash', {'command': 'exit 7'})
        p = self.run_cli('--progress-jsonl')
        self.assertEqual(p.returncode, 0, p.stderr)
        rows = [r for r in map(json.loads, p.stdout.splitlines()) if r['type'] == 'danso_progress']
        self.assertFalse(rows[-1]['success'])

    def test_closed_stdout_after_start_returns_output_error_without_replay(self):
        self.tool('bash', {'command': 'sleep 1; touch executed-after-progress'})
        process = subprocess.Popen(self.command('--progress-jsonl'), env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        pending = b''
        try:
            started = False
            while not started:
                self.assertTrue(selector.select(5), 'progress did not arrive')
                chunk = process.stdout.read1(65536)
                self.assertTrue(chunk, 'CLI exited before progress')
                pending += chunk
                while b'\n' in pending:
                    line, pending = pending.split(b'\n', 1)
                    row = json.loads(line)
                    started |= (row.get('type') == 'danso_progress'
                                and row.get('phase') == 'started')
            process.stdout.close()
            process.wait(timeout=10)
            error = process.stderr.read().decode()
            self.assertEqual(process.returncode, 3, error)
            self.assertNotIn('panicked', error)
            diagnostics = [json.loads(line.split('=', 1)[1]) for line in error.splitlines()
                           if line.startswith('DANSO_ERROR=')]
            self.assertEqual(diagnostics[0]['category'], 'output')
            self.assertIn('DANSO_USAGE=', error)
            self.assertTrue((self.repo / 'executed-after-progress').exists())
            rows = list(map(json.loads, self.session.read_text().splitlines()))
            states = [row['data']['state'] for row in rows
                      if row.get('customType') == 'danso.operation.v1']
            self.assertEqual(states, ['started'])
            before = self.session.read_bytes()
            requests = len(self.requests)
            resume = self.run_cli('--progress-jsonl')
            self.assertNotEqual(resume.returncode, 0)
            self.assertEqual(len(self.requests), requests, 'uncertain effect was replayed')
            self.assertEqual(self.session.read_bytes(), before)
        finally:
            selector.close()
            if process.poll() is None:
                process.kill()
            process.wait()
            process.stdout.close()
            process.stderr.close()

    def test_progress_is_opt_in_and_cannot_mix_with_print(self):
        self.tool('read', {'path': 'absent'})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertNotIn('danso_progress', p.stdout)
        p = self.run_cli('-p', '--progress-jsonl')
        self.assertEqual(p.returncode, 2)


if __name__ == '__main__':
    unittest.main()
