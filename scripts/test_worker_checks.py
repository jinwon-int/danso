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

    def test_stdin_read_error_is_invalid_input(self):
        class FailingStdin:
            def read(self, size):
                raise OSError('synthetic stdin failure')
        for mode in ('--compare-partial-counts', '--compare-counts'):
            for claims in ('{"z":1}', '{"z":1,"a":2,"m":3}'):
                with self.subTest(mode=mode, claims=claims):
                    out, err = io.StringIO(), io.StringIO()
                    with patch.object(checks, 'run_suites',
                                      side_effect=AssertionError('comparison ran tests')), \
                            patch.object(checks.sys, 'stdin', FailingStdin()), \
                            contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                        with self.assertRaises(SystemExit) as caught:
                            checks.main([mode, claims])
                    self.assertEqual(caught.exception.code, 2)
                    self.assertEqual(out.getvalue(), '')
                    self.assertIn('invalid receipt or claimed counts', err.getvalue())
                    self.assertNotIn('OSError', err.getvalue())
                    self.assertNotIn('synthetic', err.getvalue())

    def test_strict_comparison_unchanged(self):
        self.assertEqual(self.invoke('{"z":1}', mode='--compare-counts'), (2, ''))
        code, out = self.invoke('{"z":1,"a":2,"m":3}', mode='--compare-counts')
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out), {'version': 1, 'counts_match': True,
                                        'checks_successful': True, 'differences': []})


class JournalReceipts(unittest.TestCase):
    def entry(self, ident='entry1', call='call1', failed=False, tool_error=None):
        receipt = Receipts().example()
        if failed:
            receipt['suites'][0].update(error_events=1, successful=False)
            receipt['successful'] = False
        return {'type': 'message', 'id': ident, 'message': {
            'role': 'toolResult', 'toolCallId': call,
            'isError': failed if tool_error is None else tool_error,
            'content': [{'type': 'text', 'text': 'noise\nDANSO_CHECK_RESULTS=' + json.dumps(receipt)}]}}

    def raw(self, *entries):
        return ('\n'.join(json.dumps(e, ensure_ascii=False) for e in entries) + '\n').encode()

    def invoke(self, raw, *extra):
        out = io.StringIO()
        with patch.object(checks, 'run_suites', side_effect=AssertionError('extraction ran tests')), \
                patch.object(checks.sys, 'stdin', io.TextIOWrapper(io.BytesIO(raw), encoding='utf-8')), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = checks.main(['--extract-receipts', *extra])
            except SystemExit as exc:
                code = exc.code
        return code, out.getvalue()

    def test_original_results_only_and_failed_receipt_preserved(self):
        entry = self.entry(failed=True, tool_error=False)  # shell can mask failed tests
        marker = entry['message']['content'][0]['text']
        user = {'type': 'message', 'message': {'role': 'user', 'content': marker}}
        summary = {'type': 'custom', 'customType': 'danso.compaction.v1', 'data': {'text': marker}}
        code, out = self.invoke(self.raw(user, summary, entry))
        self.assertEqual(code, 0)  # extraction success is not check success
        rows = json.loads(out)['receipts']
        self.assertEqual(len(rows), 1)
        self.assertEqual({k: v for k, v in rows[0].items() if k != 'receipt'}, {
            'entry_id': 'entry1', 'tool_call_id': 'call1', 'receipt_index': 1, 'tool_is_error': False})
        self.assertFalse(rows[0]['receipt']['successful'])
        result = checks.compare_partial_counts(rows[0]['receipt'], {'unit': 6})
        self.assertTrue(result['counts_match'])
        self.assertFalse(result['checks_successful'])

    def test_multiple_invocations_and_blocks_keep_order(self):
        first = self.entry()
        first['message']['content'].append(copy.deepcopy(first['message']['content'][0]))
        second = self.entry('entry2', 'call2', failed=True)
        code, out = self.invoke(self.raw(first, second))
        self.assertEqual(code, 0)
        self.assertEqual([(r['tool_call_id'], r['receipt_index']) for r in json.loads(out)['receipts']],
                         [('call1', 1), ('call1', 2), ('call2', 1)])
        self.assertTrue(json.loads(out)['receipts'][-1]['tool_is_error'])

    def test_no_marker_is_explicit_empty_result(self):
        for raw in (b'', b'\n', self.raw({'type': 'custom', 'data': 'DANSO_CHECK_RESULTS=bad'})):
            self.assertEqual(self.invoke(raw), (1, '{"version": 1, "receipts": []}\n'))

    def test_bad_tail_never_emits_partial_output(self):
        valid = self.raw(self.entry())
        for tail in (b'{', b'[]', b'{"x":1,"x":2}', b'{"x":NaN}', b'\xff'):
            with self.subTest(tail=tail):
                self.assertEqual(self.invoke(valid + tail), (2, ''))
        bad = self.entry('entry2', 'call2')
        bad['message']['content'][0]['text'] = 'DANSO_CHECK_RESULTS={}'
        self.assertEqual(self.invoke(valid + self.raw(bad)), (2, ''))

    def test_numeric_overflow_rejected_even_in_ignored_metadata(self):
        for number in ('NaN', 'Infinity', '-Infinity', '1e309', '-1e309'):
            with self.subTest(number=number):
                with self.assertRaises(ValueError):
                    checks.parse_json(number)
                raw = self.raw(self.entry()).rstrip()[:-1] + (
                    ',"timestamp":' + number + '}\n').encode()
                self.assertEqual(self.invoke(raw), (2, ''))
                self.assertEqual(self.invoke(self.raw(self.entry()) +
                                            ('{"ignored":[' + number + ']}').encode()), (2, ''))
        entry = self.entry()
        entry['timestamp'] = 1.5
        self.assertEqual(self.invoke(self.raw(entry))[0], 0)

    def test_ambiguous_or_incomplete_tool_records_rejected(self):
        for ident, call in (('entry1', 'other'), ('other', 'call1')):
            self.assertEqual(self.invoke(self.raw(self.entry(), self.entry(ident, call))), (2, ''))
        for mutate in (lambda e: e['message'].pop('isError'),
                       lambda e: e['message'].update(isError=0),
                       lambda e: e['message'].update(content='DANSO_CHECK_RESULTS={}'),
                       lambda e: e.update(id=''),
                       lambda e: e['message'].update(content=[{'type': 'text', 'text': None}])):
            entry = self.entry()
            mutate(entry)
            self.assertEqual(self.invoke(self.raw(entry)), (2, ''))

    def test_byte_limit_and_mode_conflicts(self):
        with patch.object(checks, 'JOURNAL_LIMIT', 16):
            self.assertEqual(self.invoke(b' ' * 17), (2, ''))
        for extra in (('test_worker_checks',), ('--json',), ('--compare-counts', '{}'),
                      ('--compare-partial-counts', '{}')):
            self.assertEqual(self.invoke(self.raw(self.entry()), *extra), (2, ''))

    def test_unicode_line_separators_are_not_jsonl_boundaries(self):
        entry = self.entry(ident='entry\u2028one', call='call\u0085one')
        code, out = self.invoke(self.raw(entry))
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)['receipts'][0]['entry_id'], entry['id'])


if __name__ == '__main__':
    unittest.main()
