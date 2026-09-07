#!/usr/bin/env python3
"""Host-only acceptance executor tests with real bubblewrap, no model calls."""
import asyncio
import json
import os
from pathlib import Path
import tempfile
import unittest

import eval_case as e

SOLUTIONS = {
    'slug': 'import json,sys,re\ns=json.load(sys.stdin)\nprint(json.dumps(re.sub("[^a-z0-9]+", "-", s.strip().lower()).strip("-")))\n',
    'csv_totals': 'import csv,io,json,sys\nout={}\nfor row in csv.DictReader(io.StringIO(json.load(sys.stdin))):\n k=row["category"];out[k]=out.get(k,0)+int(row["amount"])\nprint(json.dumps(out))\n',
    'paths': 'import json,sys\nfrom pathlib import Path\np=Path(json.load(sys.stdin))\nprint(json.dumps((p.parent/json.loads(p.read_text())["data"]).read_text()))\n',
}


class Cases(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_all_original_bugs_fail_and_correct_solutions_pass(self):
        for name, source in SOLUTIONS.items():
            with self.subTest(name=name):
                case = e.load_case(name)
                workspace = self.root / name
                descriptor = e.prepare(case, workspace)
                self.assertEqual(descriptor['input_sha256'], e.digest(e.snapshot(workspace)))
                failed = e.accept(case, workspace, self.root / (name + '-failed'))
                self.assertFalse(failed['passed'])
                (workspace / case['entrypoint']).write_text(source)
                passed = e.accept(case, workspace, self.root / (name + '-passed'))
                self.assertTrue(passed['passed'], passed)
                self.assertNotEqual(failed['candidate_sha256'], passed['candidate_sha256'])

    def test_existing_attempt_is_never_overwritten(self):
        case = e.load_case('slug'); workspace = self.root / 'work'
        e.prepare(case, workspace)
        before = e.snapshot(workspace)
        with self.assertRaises(FileExistsError):
            e.prepare(case, workspace)
        self.assertEqual(e.snapshot(workspace), before)
        evidence = self.root / 'evidence'
        e.accept(case, workspace, evidence)
        with self.assertRaises(FileExistsError):
            e.accept(case, workspace, evidence)

    def test_private_artifacts_under_permissive_umask(self):
        old = os.umask(0)
        try:
            case = e.load_case('paths'); workspace = self.root / 'work'
            e.prepare(case, workspace)
            e.accept(case, workspace, self.root / 'evidence')
        finally:
            os.umask(old)
        for p in self.root.rglob('*'):
            self.assertEqual(p.stat().st_mode & 0o777, 0o700 if p.is_dir() else 0o600)

    def test_symlinks_special_files_and_overlapping_evidence_rejected(self):
        case = e.load_case('slug'); workspace = self.root / 'work'
        e.prepare(case, workspace)
        link = workspace / 'secret'; link.symlink_to('/etc/passwd')
        with self.assertRaises(ValueError): e.snapshot(workspace)
        link.unlink()  # disposable fixture only
        os.mkfifo(link)
        with self.assertRaises(ValueError): e.snapshot(workspace)
        with self.assertRaises(ValueError): e.accept(case, workspace, workspace / 'evidence')

    def test_snapshot_size_and_path_caps(self):
        root = self.root / 'work'; root.mkdir()
        (root / 'large').write_bytes(b'x' * (e.FILE_CAP + 1))
        with self.assertRaises(ValueError): e.snapshot(root)
        for name in ('../escape', '/absolute', 'a/../b', '.', 'a//b'):
            with self.assertRaises(ValueError): e.relative(name)

    def test_timeout_and_output_limit_are_not_passes(self):
        workspace = self.root / 'work'; workspace.mkdir()
        script = workspace / 'bad.py'
        for source, expected in [('while True: pass', 'exited'),
                                 ('import time;time.sleep(30)', 'timeout'),
                                 ('print("x"*200000)', 'output_limit')]:
            script.write_text(source)
            state, code, _, _ = asyncio.run(e.execute(e.sandbox_command(workspace, 'bad.py'), b''))
            self.assertEqual(state, expected)
            self.assertNotEqual(code, 0)

    def test_oracle_and_host_environment_unavailable(self):
        workspace = self.root / 'work'; workspace.mkdir()
        secret = self.root / 'oracle'; secret.write_text('not-for-candidate')
        script = workspace / 'probe.py'
        script.write_text('import os,socket\nfrom pathlib import Path\n'
                          f'assert not Path({str(secret)!r}).exists()\n'
                          'assert "EVAL_TEST_SECRET" not in os.environ\n'
                          'try:\n socket.create_connection(("1.1.1.1", 443), timeout=.1)\n'
                          'except OSError: pass\nelse: raise Exception("network exposed")\n'
                          'try:\n Path("probe.py").write_text("tampered")\n'
                          'except OSError: pass\nelse: raise Exception("workspace writable")\n'
                          'print("ok")\n')
        os.environ['EVAL_TEST_SECRET'] = 'synthetic'
        try:
            state, code, out, _ = asyncio.run(e.execute(e.sandbox_command(workspace, 'probe.py'), b''))
        finally:
            os.environ.pop('EVAL_TEST_SECRET')
        self.assertEqual((state, code, out), ('exited', 0, b'ok\n'))


if __name__ == '__main__':
    unittest.main()
