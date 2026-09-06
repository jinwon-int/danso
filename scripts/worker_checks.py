#!/usr/bin/env python3
"""Worker unittest receipts; compare a saved receipt with claimed suite counts."""
import argparse
import contextlib
import json
import sys
import unittest

from dev_check import WORKER_TESTS

COUNTERS = ('tests_run', 'failure_events', 'error_events', 'skipped',
            'expected_failures', 'unexpected_successes')


def run_suites(selectors, loader=None):
    loader = loader or unittest.TestLoader()
    rows = []
    # unittest diagnostics and test prints never enter the JSON stdout channel.
    with contextlib.redirect_stdout(sys.stderr):
        for selector in selectors:
            try:
                suite = loader.loadTestsFromName(selector)
            except (Exception, SystemExit):
                # Imports/load_tests may raise errors unittest's loader does not
                # wrap. Preserve the failed selector without inventing test runs.
                print('ERROR: could not load selected suite: ' + selector, file=sys.stderr)
                rows.append({'selector': selector, **{key: 0 for key in COUNTERS},
                             'error_events': 1, 'successful': False})
                continue
            result = unittest.TextTestRunner(stream=sys.stderr, verbosity=2).run(suite)
            rows.append({'selector': selector, 'tests_run': result.testsRun,
                         'failure_events': len(result.failures), 'error_events': len(result.errors),
                         'skipped': len(result.skipped), 'expected_failures': len(result.expectedFailures),
                         'unexpected_successes': len(result.unexpectedSuccesses),
                         'successful': result.wasSuccessful() and result.testsRun > 0})
    return {'version': 1, 'profile': 'worker', 'suites': rows,
            'tests_run': sum(row['tests_run'] for row in rows),
            'successful': bool(rows) and all(row['successful'] for row in rows)}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON key')
        result[key] = value
    return result


def reject_constant(_):
    raise ValueError('non-finite JSON value')


def parse_json(text):
    return json.loads(text, object_pairs_hook=unique_object, parse_constant=reject_constant)


def validate_receipt(receipt):
    if (type(receipt) is not dict or set(receipt) != {'version', 'profile', 'suites', 'tests_run', 'successful'}
            or type(receipt['version']) is not int or receipt['version'] != 1
            or receipt['profile'] != 'worker' or type(receipt['suites']) is not list
            or not receipt['suites']):
        raise ValueError('invalid receipt')
    selectors = set()
    for row in receipt['suites']:
        if (type(row) is not dict or set(row) != {'selector', 'successful', *COUNTERS}
                or type(row['selector']) is not str or not row['selector']
                or row['selector'] in selectors
                or any(type(row[key]) is not int or row[key] < 0 for key in COUNTERS)
                or type(row['successful']) is not bool):
            raise ValueError('invalid suite receipt')
        selectors.add(row['selector'])
        expected = row['tests_run'] > 0 and not any(row[key] for key in
                    ('failure_events', 'error_events', 'unexpected_successes'))
        if row['successful'] != expected:
            raise ValueError('inconsistent suite status')
    if (type(receipt['tests_run']) is not int
            or receipt['tests_run'] != sum(row['tests_run'] for row in receipt['suites'])
            or type(receipt['successful']) is not bool
            or receipt['successful'] != all(row['successful'] for row in receipt['suites'])):
        raise ValueError('inconsistent receipt totals')


def compare_counts(receipt, claims):
    validate_receipt(receipt)
    actual = {row['selector']: row['tests_run'] for row in receipt['suites']}
    if (type(claims) is not dict or set(claims) != set(actual)
            or any(type(count) is not int or count < 0 for count in claims.values())):
        raise ValueError('claims must include every suite exactly once with integer counts')
    differences = [{'selector': name, 'reported': claims[name], 'actual': count}
                   for name, count in actual.items() if claims[name] != count]
    return {'version': 1, 'counts_match': not differences,
            'checks_successful': receipt['successful'], 'differences': differences}


def compare_partial_counts(receipt, claims):
    validate_receipt(receipt)
    if type(claims) is not dict or not claims:
        raise ValueError('claims must be a nonempty JSON object')
    actual = {row['selector']: row['tests_run'] for row in receipt['suites']}
    unknown = set(claims) - set(actual)
    if unknown or any(type(count) is not int or count < 0 for count in claims.values()):
        # bool counts fail here: type(True) is bool, not int.
        raise ValueError('claims must use receipt selectors with nonnegative integer counts')
    differences = [{'selector': row['selector'], 'reported': claims[row['selector']],
                    'actual': row['tests_run']}
                   for row in receipt['suites'] if row['selector'] in claims
                   and claims[row['selector']] != row['tests_run']]
    unreported = [row['selector'] for row in receipt['suites'] if row['selector'] not in claims]
    return {'version': 1, 'counts_match': not differences,
            'checks_successful': receipt['successful'], 'differences': differences,
            'unreported_selectors': unreported}


JOURNAL_LIMIT = 16 * 1024 * 1024


def extract_receipts(raw):
    """Extract original tool-result markers, not summaries or journal authority."""
    if type(raw) is not bytes or len(raw) > JOURNAL_LIMIT:
        raise ValueError('invalid journal input')
    text = raw.decode('utf-8')
    rows = []
    entry_ids = set()
    call_ids = set()
    for line in text.split('\n'):
        if not line.strip():
            continue
        entry = parse_json(line)
        if type(entry) is not dict:
            raise ValueError('invalid journal entry')
        if entry.get('type') != 'message':
            continue
        message = entry.get('message')
        if type(message) is not dict:
            raise ValueError('invalid journal message')
        if message.get('role') != 'toolResult':
            continue
        entry_id, call_id = entry.get('id'), message.get('toolCallId')
        if (type(entry_id) is not str or not entry_id or entry_id in entry_ids
                or type(call_id) is not str or not call_id or call_id in call_ids
                or type(message.get('content')) is not list):
            raise ValueError('invalid or ambiguous tool result')
        entry_ids.add(entry_id)
        call_ids.add(call_id)
        index = 0
        for block in message['content']:
            if type(block) is not dict:
                raise ValueError('invalid content block')
            if block.get('type') != 'text':
                continue
            if type(block.get('text')) is not str:
                raise ValueError('invalid text block')
            for output_line in block['text'].split('\n'):
                if not output_line.startswith('DANSO_CHECK_RESULTS='):
                    continue
                receipt = parse_json(output_line.split('=', 1)[1])
                validate_receipt(receipt)
                if type(message.get('isError')) is not bool:
                    raise ValueError('missing tool result status')
                index += 1
                rows.append({'entry_id': entry_id, 'tool_call_id': call_id,
                             'receipt_index': index, 'tool_is_error': message['isError'],
                             'receipt': receipt})
    return {'version': 1, 'receipts': rows}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--json', action='store_true')
    mode.add_argument('--extract-receipts', action='store_true',
                      help='extract original worker receipts from journal JSONL on stdin; never run tests')
    mode.add_argument('--compare-counts', metavar='JSON', help='compare claimed counts with a receipt on stdin; never run tests')
    mode.add_argument('--compare-partial-counts', metavar='JSON',
                      help='compare a subset of claimed counts with a receipt on stdin; never run tests')
    parser.add_argument('selectors', nargs='*')
    args = parser.parse_args(argv)
    if args.extract_receipts:
        if args.selectors:
            parser.error('extraction does not accept test selectors')
        try:
            result = extract_receipts(sys.stdin.buffer.read(JOURNAL_LIMIT + 1))
        except (ValueError, TypeError, RecursionError, OSError):
            parser.error('invalid journal or worker receipt')
        print(json.dumps(result), flush=True)
        return 0 if result['receipts'] else 1
    if args.compare_counts is not None or args.compare_partial_counts is not None:
        if args.selectors:
            parser.error('comparison does not accept test selectors')
        try:
            raw = sys.stdin.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise ValueError('receipt too large')
            if args.compare_counts is not None:
                result = compare_counts(parse_json(raw), parse_json(args.compare_counts))
            else:
                result = compare_partial_counts(parse_json(raw), parse_json(args.compare_partial_counts))
        except (ValueError, TypeError, RecursionError):
            parser.error('invalid receipt or claimed counts')
        print(json.dumps(result), flush=True)
        return 0 if result['counts_match'] and result['checks_successful'] else 1
    selectors = tuple(args.selectors) or WORKER_TESTS
    if selectors != WORKER_TESTS:
        parser.error('only the configured worker subset is supported')
    receipt = run_suites(selectors)
    print(('' if args.json else 'DANSO_CHECK_RESULTS=') + json.dumps(receipt), flush=True)
    return 0 if receipt['successful'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
