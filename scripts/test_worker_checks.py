#!/usr/bin/env python3
"""Worker-safe receipt and report comparison regressions; no real subprocesses."""
import contextlib
import copy
import io
import json
import unittest
from unittest.mock import patch

import worker_checks as checks


class Receipts(unittest.TestCase):
    def run_fixture(self, case):
        loader = unittest.TestLoader()
        with patch.object(loader, 'loadTestsFromName', side_effect=lambda _: loader.loadTestsFromTestCase(case)), contextlib.redirect_stderr(io.StringIO()):
            return checks.run_suites(['fixture'], loader)

    def test_actual_result_counters_preserve_subtest_failures(self):
        class Cases(unittest.TestCase):
            def test_print(self):
                print('test noise')
            def test_subtests(self):
                for n in range(2):
                    with self.subTest(n=n):
                        self.fail('fixture')
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            receipt = self.run_fixture(Cases)
        self.assertEqual(stdout.getvalue(), '')
        row = receipt['suites'][0]
        self.assertEqual(row['tests_run'], 2)
        self.assertEqual(row['failure_events'], 2)
        self.assertFalse(receipt['successful'])
        checks.validate_receipt(receipt)

    def test_setup_error_and_empty_suite_never_report_success(self):
        class Broken(unittest.TestCase):
            @classmethod
            def setUpClass(cls):
                raise RuntimeError('fixture')
            def test_unused(self):
                self.fail('not reached')
        class Empty(unittest.TestCase):
            pass
        for case, errors in ((Broken, 1), (Empty, 0)):
            receipt = self.run_fixture(case)
            self.assertEqual(receipt['tests_run'], 0)
            self.assertEqual(receipt['suites'][0]['error_events'], errors)
            self.assertFalse(receipt['successful'])
            checks.validate_receipt(receipt)

    def test_skips_expected_failures_and_unexpected_success(self):
        class Cases(unittest.TestCase):
            @unittest.skip('fixture')
            def test_skip(self):
                pass
            @unittest.expectedFailure
            def test_expected(self):
                self.fail('fixture')
            @unittest.expectedFailure
            def test_unexpected(self):
                pass
        receipt = self.run_fixture(Cases)
        row = receipt['suites'][0]
        self.assertEqual(row['tests_run'], 3)
        self.assertEqual([row[k] for k in ('skipped', 'expected_failures', 'unexpected_successes')], [1, 1, 1])
        self.assertFalse(row['successful'])
        checks.validate_receipt(receipt)

    def test_loader_exceptions_produce_failed_receipt_and_continue(self):
        for error in (SyntaxError('PRIVATE fixture'), RuntimeError('PRIVATE fixture'), SystemExit(0)):
            loader = unittest.TestLoader()
            with patch.object(loader, 'loadTestsFromName', side_effect=[error, unittest.TestSuite()]), contextlib.redirect_stderr(io.StringIO()):
                receipt = checks.run_suites(['broken', 'empty'], loader)
            self.assertEqual([row['selector'] for row in receipt['suites']], ['broken', 'empty'])
            self.assertEqual(receipt['tests_run'], 0)
            self.assertEqual(receipt['suites'][0]['error_events'], 1)
            self.assertFalse(receipt['successful'])
            self.assertNotIn('PRIVATE', json.dumps(receipt))
            checks.validate_receipt(receipt)

    def test_missing_test_is_error_not_missing_receipt(self):
        with contextlib.redirect_stderr(io.StringIO()):
            receipt = checks.run_suites(['nonexistent_danso_fixture_module'])
        self.assertFalse(receipt['successful'])
        self.assertEqual(receipt['suites'][0]['error_events'], 1)
        checks.validate_receipt(receipt)

    def example(self):
        rows = []
        for label, count in (('safety', 13), ('unit', 6)):
            rows.append({'selector': label, **{key: 0 for key in checks.COUNTERS},
                         'tests_run': count, 'successful': True})
        return {'version': 1, 'profile': 'worker', 'tests_run': 19,
                'successful': True, 'suites': rows}

    def test_comparison_detects_combined_total_misattribution_without_running(self):
        for claim, expected in (({'safety': 13, 'unit': 6}, 0), ({'safety': 13, 'unit': 19}, 1)):
            out = io.StringIO()
            with patch.object(checks, 'run_suites', side_effect=AssertionError('comparison ran tests')), patch.object(checks.sys, 'stdin', io.StringIO(json.dumps(self.example()))), contextlib.redirect_stdout(out):
                self.assertEqual(checks.main(['--compare-counts', json.dumps(claim)]), expected)
            result = json.loads(out.getvalue())
            self.assertEqual(result['counts_match'], expected == 0)
            if expected:
                self.assertEqual(result['differences'], [{'selector': 'unit', 'reported': 19, 'actual': 6}])

    def test_invalid_claims_and_inconsistent_receipts_rejected(self):
        receipt = self.example()
        for claims in ({'unit': 6}, {'safety': 13, 'unit': True}, {'safety': 13, 'unit': -1}, []):
            with self.assertRaises(ValueError):
                checks.compare_counts(receipt, claims)
        for mutate in (lambda r: r.update(tests_run=20),
                       lambda r: r.update(version=True),
                       lambda r: r['suites'].append(copy.deepcopy(r['suites'][0])),
                       lambda r: r['suites'][0].update(error_events=1)):
            bad = copy.deepcopy(receipt)
            mutate(bad)
            with self.assertRaises(ValueError):
                checks.compare_counts(bad, {'safety': 13, 'unit': 6})
        with self.assertRaises(ValueError):
            checks.parse_json('{"unit":6,"unit":19}')

    def test_matching_counts_cannot_promote_failed_checks(self):
        receipt = self.example()
        receipt['suites'][0].update(error_events=1, successful=False)
        receipt['successful'] = False
        out = io.StringIO()
        with patch.object(checks.sys, 'stdin', io.StringIO(json.dumps(receipt))), contextlib.redirect_stdout(out):
            self.assertEqual(checks.main(['--compare-counts', '{"safety":13,"unit":6}']), 1)
        result = json.loads(out.getvalue())
        self.assertTrue(result['counts_match'])
        self.assertFalse(result['checks_successful'])

    def test_json_mode_and_restricted_selectors(self):
        out = io.StringIO()
        with patch.object(checks, 'run_suites', return_value=self.example()) as run, contextlib.redirect_stdout(out):
            self.assertEqual(checks.main(['--json']), 0)
        run.assert_called_once_with(checks.WORKER_TESTS)
        self.assertEqual(json.loads(out.getvalue()), self.example())
        with patch.object(checks, 'run_suites') as run, contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            checks.main(['test_dev_check_host'])
        run.assert_not_called()


class PartialReports(unittest.TestCase):
    def receipt(self, failed=False):
        rows = []
        # Deliberately not alphabetical: output order belongs to the receipt.
        for selector, count in (('z', 1), ('a', 2), ('m', 3)):
            rows.append({'selector': selector, **{key: 0 for key in checks.COUNTERS},
                         'tests_run': count, 'successful': True})
        if failed:
            rows[2].update(error_events=1, successful=False)
        return {'version': 1, 'profile': 'worker', 'suites': rows,
                'tests_run': 6, 'successful': not failed}

    def invoke(self, claims, receipt=None, extra=(), mode='--compare-partial-counts', raw=None):
        if raw is None:
            raw = json.dumps(self.receipt() if receipt is None else receipt)
        out = io.StringIO()
        with patch.object(checks, 'run_suites', side_effect=AssertionError('comparison ran tests')), \
                patch.object(checks.sys, 'stdin', io.StringIO(raw)), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = checks.main([mode, claims, *extra])
            except SystemExit as exc:
                code = exc.code
        return code, out.getvalue()

    def test_subset_and_exact_schema(self):
        code, out = self.invoke('{"a":2}')
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out), {
            'version': 1, 'counts_match': True, 'checks_successful': True,
            'differences': [], 'unreported_selectors': ['z', 'm']})

    def test_order_and_complete_claims(self):
        code, out = self.invoke('{"m":8,"z":9}')
        self.assertEqual(code, 1)
        result = json.loads(out)
        self.assertEqual(result['differences'], [
            {'selector': 'z', 'reported': 9, 'actual': 1},
            {'selector': 'm', 'reported': 8, 'actual': 3}])
        self.assertEqual(result['unreported_selectors'], ['a'])
        code, out = self.invoke('{"m":3,"a":2,"z":1}')
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)['unreported_selectors'], [])

    def test_unreported_failure_prevents_success(self):
        code, out = self.invoke('{"z":1}', self.receipt(failed=True))
        self.assertEqual(code, 1)
        result = json.loads(out)
        self.assertTrue(result['counts_match'])
        self.assertFalse(result['checks_successful'])

    def test_invalid_claims_have_no_stdout(self):
        for claims in ('{}', '[]', 'null', '{', '{"z":true}', '{"z":-1}',
                       '{"unknown":1}', '{"z":1,"z":1}', '{"z":1.0}', '{"z":NaN}'):
            with self.subTest(claims=claims):
                self.assertEqual(self.invoke(claims), (2, ''))

    def test_invalid_receipts_and_modes(self):
        bad = self.receipt()
        bad['tests_run'] = 7
        self.assertEqual(self.invoke('{"z":1}', bad), (2, ''))
        for extra in (('test_worker_checks',), ('--json',),
                      ('--compare-counts', '{"z":1}')):
            with self.subTest(extra=extra):
                self.assertEqual(self.invoke('{"z":1}', extra=extra), (2, ''))

    def test_size_limit_and_duplicate_receipt_keys(self):
        for raw in (' ' * (1024 * 1024 + 1),
                    json.dumps(self.receipt())[:-1] + ',"version":1}'):
            self.assertEqual(self.invoke('{"z":1}', raw=raw), (2, ''))

    def test_strict_comparison_unchanged(self):
        self.assertEqual(self.invoke('{"z":1}', mode='--compare-counts'), (2, ''))
        code, out = self.invoke('{"z":1,"a":2,"m":3}', mode='--compare-counts')
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out), {'version': 1, 'counts_match': True,
                                        'checks_successful': True, 'differences': []})


if __name__ == '__main__':
    unittest.main()
