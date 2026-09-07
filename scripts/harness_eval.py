#!/usr/bin/env python3
"""Offline paired evaluation plans and receipt aggregation; never executes agents."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import statistics
import sys

LIMIT = 8 * 1024 * 1024
METRICS = ('elapsed_seconds', 'requests', 'total_tokens', 'compactions', 'repeated_tools')


def require(ok):
    if not ok:
        raise ValueError('invalid evaluation data')


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                     allow_nan=False).encode()).hexdigest()


def object_keys(value, keys):
    require(type(value) is dict and set(value) == set(keys))


def identifier(value):
    require(type(value) is str and re.fullmatch(r'[a-zA-Z0-9_-]{1,80}', value))


def sha(value):
    require(type(value) is str and re.fullmatch(r'[0-9a-f]{64}', value))


def integer(value, low, high):
    require(type(value) is int and low <= value <= high)


def text_field(value):
    require(type(value) is str and 0 < len(value) <= 256 and not any(ord(c) < 32 for c in value))


def make_plan(spec):
    object_keys(spec, ('schema', 'conditions', 'harnesses', 'cases', 'repetitions'))
    require(spec['schema'] == 'danso.eval.spec.v1')
    conditions = spec['conditions']
    object_keys(conditions, ('provider', 'model', 'effort', 'request_limit', 'wall_seconds',
                             'environment_sha256', 'cache_policy', 'budget_enforcement'))
    for key in ('provider', 'model', 'effort', 'cache_policy', 'budget_enforcement'):
        text_field(conditions[key])
    sha(conditions['environment_sha256'])
    integer(conditions['request_limit'], 1, 10000)
    integer(conditions['wall_seconds'], 1, 86400)
    integer(spec['repetitions'], 1, 100)
    require(type(spec['harnesses']) is list and len(spec['harnesses']) == 2)
    names = set()
    for harness in spec['harnesses']:
        object_keys(harness, ('id', 'revision', 'config_sha256'))
        identifier(harness['id'])
        require(harness['id'] not in names)
        names.add(harness['id'])
        require(type(harness['revision']) is str and re.fullmatch(r'[0-9a-f]{40}', harness['revision']))
        sha(harness['config_sha256'])
    require(type(spec['cases']) is list and 1 <= len(spec['cases']) <= 1000)
    names = set()
    for case in spec['cases']:
        object_keys(case, ('id', 'prompt_sha256', 'input_sha256', 'acceptance_sha256'))
        identifier(case['id'])
        require(case['id'] not in names)
        names.add(case['id'])
        for key in ('prompt_sha256', 'input_sha256', 'acceptance_sha256'):
            sha(case[key])
    require(len(spec['cases']) * spec['repetitions'] <= 10000)
    jobs = []
    for repeat in range(spec['repetitions']):
        for index, case in enumerate(spec['cases']):
            pair = f'{case["id"]}:{repeat + 1}'
            order = spec['harnesses'][::1 if (repeat + index) % 2 == 0 else -1]
            for harness in order:
                jobs.append({'id': f'{pair}:{harness["id"]}', 'pair': pair,
                             'harness': harness['id'], 'case': case['id'], 'repeat': repeat + 1})
    payload = {'schema': 'danso.eval.plan.v1', 'spec': spec, 'jobs': jobs}
    return {**payload, 'sha256': digest(payload)}


def stats(values):
    if not values:
        return {'n': 0, 'mean': None, 'median': None, 'min': None, 'max': None}
    return {'n': len(values), 'mean': statistics.mean(values),
            'median': statistics.median(values), 'min': min(values), 'max': max(values)}


def summarize(plan, receipts):
    object_keys(plan, ('schema', 'spec', 'jobs', 'sha256'))
    require(plan == make_plan(plan['spec']))
    require(type(receipts) is list and len(receipts) <= len(plan['jobs']))
    jobs = {job['id']: job for job in plan['jobs']}
    indexed = {}
    for row in receipts:
        object_keys(row, ('job', 'plan_sha256', 'status', 'exit_code', 'acceptance_passed',
                          'evidence_sha256', 'metrics'))
        require(type(row['job']) is str and row['job'] in jobs and row['job'] not in indexed)
        require(row['plan_sha256'] == plan['sha256'])
        sha(row['evidence_sha256'])
        require(row['status'] in ('passed', 'failed', 'timeout', 'error'))
        require(row['exit_code'] is None or type(row['exit_code']) is int)
        require(row['acceptance_passed'] is None or type(row['acceptance_passed']) is bool)
        if row['status'] == 'passed':
            require(row['exit_code'] == 0 and row['acceptance_passed'] is True)
        if row['status'] == 'failed':
            require(row['acceptance_passed'] is False or
                    (type(row['exit_code']) is int and row['exit_code'] != 0))
        object_keys(row['metrics'], METRICS)
        for metric, value in row['metrics'].items():
            require(value is None or
                    (type(value) in (int, float) and math.isfinite(value) and 0 <= value <= 10**12))
            if value is not None and metric != 'elapsed_seconds':
                require(type(value) is int)
        # A timeout/error is never promoted to success based on partial output.
        indexed[row['job']] = row
    harnesses = [h['id'] for h in plan['spec']['harnesses']]
    result = {'schema': 'danso.eval.report.v1', 'plan_sha256': plan['sha256'],
              'complete': len(indexed) == len(jobs), 'planned_runs': len(jobs),
              'submitted_runs': len(indexed),
              'missing_jobs': [key for key in jobs if key not in indexed],
              'measurement_trust': 'caller-supplied receipts; evidence digests are references, not verified attestations',
              'harnesses': {}, 'paired': {}}
    limits = plan['spec']['conditions']
    result['budget_violations'] = [key for key, row in indexed.items()
        if (row['metrics']['requests'] is not None and
            row['metrics']['requests'] > limits['request_limit']) or
           (row['metrics']['elapsed_seconds'] is not None and
            row['metrics']['elapsed_seconds'] > limits['wall_seconds'])]
    result['budget_observations_complete'] = all(
        row['metrics']['requests'] is not None and row['metrics']['elapsed_seconds'] is not None
        for row in indexed.values()) and result['complete']
    for harness in harnesses:
        rows = [row for key, row in indexed.items() if jobs[key]['harness'] == harness]
        planned = sum(job['harness'] == harness for job in jobs.values())
        counts = {s: sum(row['status'] == s for row in rows)
                  for s in ('passed', 'failed', 'timeout', 'error')}
        result['harnesses'][harness] = {
            'planned': planned, 'submitted': len(rows), 'statuses': counts,
            'pass_rate_submitted': counts['passed'] / len(rows) if rows else None,
            'pass_rate_planned_lower_bound': counts['passed'] / planned,
            'metrics_all_submitted': {m: stats([r['metrics'][m] for r in rows
                                              if r['metrics'][m] is not None]) for m in METRICS}}
    pairs = {}
    for job in jobs.values():
        pairs.setdefault(job['pair'], {})[job['harness']] = indexed.get(job['id'])
    result['paired']['direction'] = f'{harnesses[1]} minus {harnesses[0]}'
    result['paired']['planned_pairs'] = len(pairs)
    for cohort in ('all_submitted_pairs', 'both_passed_pairs'):
        rows = [p for p in pairs.values() if all(p.values()) and
                (cohort == 'all_submitted_pairs' or all(r['status'] == 'passed' for r in p.values()))]
        result['paired'][cohort] = {'pairs': len(rows), 'metric_deltas': {}}
        for metric in METRICS:
            deltas = [p[harnesses[1]]['metrics'][metric] - p[harnesses[0]]['metrics'][metric]
                      for p in rows if all(r['metrics'][metric] is not None for r in p.values())]
            result['paired'][cohort]['metric_deltas'][metric] = stats(deltas)
    return result


def read_json(path):
    # No writes, symlink traversal, special files, or unbounded input reads.
    path = Path(path).absolute()
    parent = os.open(path.anchor, os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
            os.close(parent)
            parent = child
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
    finally:
        os.close(parent)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_size <= LIMIT)
        raw = stream.read(LIMIT + 1)
        require(len(raw) <= LIMIT)
    def pairs(items):
        value = {}
        for key, item in items:
            require(key not in value)
            value[key] = item
        return value
    return json.loads(raw, object_pairs_hook=pairs)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('plan').add_argument('spec')
    report = sub.add_parser('report')
    report.add_argument('plan')
    report.add_argument('receipts')
    args = parser.parse_args(argv)
    try:
        result = make_plan(read_json(args.spec)) if args.command == 'plan' else \
            summarize(read_json(args.plan), read_json(args.receipts))
        print(json.dumps(result, indent=2, allow_nan=False))
        return 0
    except (OSError, ValueError, TypeError, KeyError, OverflowError, RecursionError):
        print('Invalid or unreadable evaluation input; no agents were executed.', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
