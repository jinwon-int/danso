#!/usr/bin/env python3
"""Explicit worker subset or full host development checks. No live provider calls."""
import argparse
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
WORKER_TESTS = ('test_live_acceptance.Safety', 'test_live_acceptance.FailureReporting')


def commands(profile):
    if profile == 'worker':
        return [[sys.executable, '-m', 'unittest', *WORKER_TESTS, '-v']]
    if profile != 'host':
        raise ValueError('unknown development check profile')
    return [['cargo', 'fmt', '--check'],
            ['cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings'],
            ['cargo', 'test', '--locked'], ['cargo', 'build', '--locked'],
            *[[sys.executable, f'scripts/{name}'] for name in
              ('test_e2e.py', 'test_compaction.py', 'test_providers.py',
               'test_live_acceptance.py', 'test_dev_check.py')]]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=('worker', 'host'), required=True)
    args = parser.parse_args(argv)
    if args.profile == 'worker':
        print('WORKER SUBSET ONLY: Python safety/failure-report tests. '
              'Rust and sandbox integration checks are NOT run; host checks remain required.', flush=True)
    else:
        print('HOST CHECKS: Rust toolchain and functioning bubblewrap required. No fallback or live calls.', flush=True)
    for command in commands(args.profile):
        try:
            result = subprocess.run(command, cwd=ROOT / 'scripts' if args.profile == 'worker' else ROOT)
        except OSError:
            print('FAIL: check could not start; verify the required host toolchain/environment.', file=sys.stderr)
            return 1
        if result.returncode:
            print('FAIL: development check stopped; no remaining checks were run.', file=sys.stderr)
            return 1
    print(f'PASS: {args.profile} checks only.', flush=True)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
