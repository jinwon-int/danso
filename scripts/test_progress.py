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

    def test_progress_is_opt_in_and_cannot_mix_with_print(self):
        self.tool('read', {'path': 'absent'})
        p = self.run_cli()
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertNotIn('danso_progress', p.stdout)
        p = self.run_cli('-p', '--progress-jsonl')
        self.assertEqual(p.returncode, 2)


if __name__ == '__main__':
    unittest.main()
