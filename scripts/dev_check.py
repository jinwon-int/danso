#!/usr/bin/env python3
"""Explicit worker subset or full host development checks. No live provider calls."""
import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
WORKER_TESTS = ('test_live_acceptance.Safety', 'test_live_acceptance.FailureReporting', 'test_dev_check', 'test_worker_checks')


def commands(profile):
    if profile == 'worker':
        return [[sys.executable, 'worker_checks.py', *WORKER_TESTS]]
    if profile != 'host':
        raise ValueError('unknown development check profile')
    return [['cargo', 'fmt', '--check'],
            ['cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings'],
            ['cargo', 'test', '--locked'], ['cargo', 'build', '--locked'],
            *[[sys.executable, f'scripts/{name}'] for name in
              ('test_e2e.py', 'test_compaction.py', 'test_providers.py',
               'test_live_acceptance.py', 'test_dev_check.py', 'test_worker_checks.py', 'test_dev_check_host.py', 'test_ccc_node.py')]]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=('worker', 'host'), required=True)
    parser.add_argument('--json', action='store_true', help='worker-only structured test receipt on stdout')
    args = parser.parse_args(argv)
    if args.json and args.profile != 'worker':
        parser.error('--json supports the worker profile only')
    if args.json:
        pass
    elif args.profile == 'worker':
        print('WORKER SUBSET ONLY: Python safety/failure-report/profile unit tests. '
              'Rust and sandbox integration checks are NOT run; host checks remain required.', flush=True)
    else:
        print('HOST CHECKS: Rust toolchain and functioning bubblewrap required. No fallback or live calls.', flush=True)
    for command in commands(args.profile):
        if args.json:
            command = [*command, '--json']
        try:
            result = subprocess.run(command, cwd=ROOT / 'scripts' if args.profile == 'worker' else ROOT)
        except OSError:
            print('FAIL: check could not start; verify the required host toolchain/environment.', file=sys.stderr)
            return 1
        if result.returncode:
            print('FAIL: development check stopped; no remaining checks were run.', file=sys.stderr)
            return 1
    if not args.json:
        print(f'PASS: {args.profile} checks only.', flush=True)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
