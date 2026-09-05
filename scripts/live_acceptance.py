#!/usr/bin/env python3
"""Opt-in Anthropic acceptance. All artifacts are retained in a private temp tree."""
import argparse
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess
import tempfile

BROKEN = '#!/bin/bash\nprintf "%s\\n" "$(($1 - $2))"\n'
FIXED = BROKEN.replace('$1 - $2', '$1 + $2')
TEST = '''#!/bin/bash
set -eu
test "$(bash add.sh 2 3)" = 5
test "$(bash add.sh -2 3)" = 1
test "$(bash add.sh 0 0)" = 0
echo ACCEPTANCE_TESTS_PASS
'''


def save(path, content):
    # Exclusive creation: never follow or overwrite an existing path.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        stream.write(content)


def read_regular(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'r') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024:
            raise ValueError('invalid acceptance artifact')
        return stream.read(1024 * 1024 + 1)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def usage(stderr):
    values = []
    for prefix in ('DANSO_USAGE=', 'PIRI_USAGE='):
        lines = [line[len(prefix):] for line in stderr.splitlines() if line.startswith(prefix)]
        require(len(lines) == 1, 'missing or duplicate usage summary')
        values.append(json.loads(lines[0]))
    require(values[0] == values[1], 'usage prefixes disagree')
    result = values[0]
    fields = ('inputTokens', 'outputTokens', 'cacheReadTokens', 'cacheWriteTokens')
    require(result['requests'] > 0 and result['outputTokens'] > 0, 'no response usage')
    require(result['totalTokens'] == sum(result[k] for k in fields), 'invalid token total')
    return result


def run(binary, model, provider_env):
    root = Path(tempfile.mkdtemp(prefix='danso-acceptance-'))
    print(f'Artifacts retained: {root}', flush=True)
    repo = root / 'repo'
    repo.mkdir(mode=0o700)
    home = root / 'home'
    home.mkdir(mode=0o700)
    session = root / 'session.jsonl'
    save(repo / 'add.sh', BROKEN)
    save(repo / 'test.sh', TEST)
    token = secrets.token_hex(12)
    prompts = [
        'Read add.sh and test.sh with read. Fix add.sh using edit by changing only '
        'the subtraction operator to addition. Do not change test.sh. Run exactly '
        '`bash test.sh` with bash. Use write to create report.md describing the fix '
        'and test result. Finish with a short explanation. Remember this conversation '
        f'token for my next request: {token}',
        'Without calling any tools, return only the conversation token I gave you '
        'in my previous request.'
    ]
    env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), **provider_env}
    summaries = []
    try:
        first_bytes = None
        for index, prompt in enumerate(prompts, 1):
            command = [str(binary), '--cwd', str(repo), '--session', str(session),
                       '--model', model, '--max-turns', '8', '--timeout-seconds', '180',
                       '--tool-timeout-seconds', '10', '-p', '--', prompt]
            # Danso supervises its workers; allow its own timeout to finish first.
            result = subprocess.run(command, env=env, text=True, capture_output=True, timeout=195)
            save(root / f'run-{index}.stdout', result.stdout)
            save(root / f'run-{index}.stderr', result.stderr)
            require(result.returncode == 0, f'run {index} failed (exit {result.returncode})')
            require(bool(result.stdout.strip()), 'empty final answer')
            summaries.append(usage(result.stderr))
            transcript = read_regular(session)
            entries = [json.loads(line) for line in transcript.splitlines()]
            results = [e['message'] for e in entries if e.get('message', {}).get('role') == 'toolResult']
            if index == 1:
                require(read_regular(repo / 'add.sh') == FIXED, 'incorrect source change')
                require(read_regular(repo / 'test.sh') == TEST, 'test file changed')
                require(bool(read_regular(repo / 'report.md').strip()), 'missing report')
                require(all(not r.get('isError', True) for r in results), 'tool failure')
                require({r.get('toolName') for r in results} == {'read', 'edit', 'bash', 'write'},
                        'four-tool round trip missing')
                calls = [c for e in entries for c in e.get('message', {}).get('content', [])
                         if isinstance(c, dict) and c.get('type') == 'toolCall']
                test_ids = {c['id'] for c in calls if c.get('name') == 'bash'
                            and c.get('arguments') == {'command': 'bash test.sh'}}
                require(any(r.get('toolCallId') in test_ids and
                            'ACCEPTANCE_TESTS_PASS' in json.dumps(r.get('content')) for r in results),
                        'no successful sandbox test evidence')
                first_bytes = transcript
                first_count = len(results)
                snapshot = {name: read_regular(repo / name) for name in ('add.sh', 'test.sh', 'report.md')}
            else:
                require(transcript.startswith(first_bytes), 'resume rewrote journal')
                require(len(results) == first_count, 'resume unexpectedly executed tools')
                require(result.stdout.strip() == token, 'resume lost conversation context')
                require(summaries[-1]['requests'] == 1, 'resume usage includes extra responses')
                require(all(read_regular(repo / name) == body for name, body in snapshot.items()),
                        'resume changed workspace')
        save(root / 'result.json', json.dumps({'status': 'passed', 'model': model,
             'runs': summaries, 'cost': 'unknown; costUsd=0 is a compatibility placeholder'}, indent=2) + '\n')
        return root
    except Exception:
        save(root / 'result.json', json.dumps({'status': 'failed', 'completed_runs': len(summaries)}) + '\n')
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--live', action='store_true', help='authorize two real provider invocations')
    parser.add_argument('--model', required=True)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/danso'))
    args = parser.parse_args()
    if not args.live:
        parser.error('--live is required; this sends requests and may incur charges')
    if os.environ.get('DANSO_ANTHROPIC_BASE_URL'):
        parser.error('this acceptance command supports only the default Anthropic endpoint')
    key = os.environ.get('ANTHROPIC_API_KEY')
    if not key:
        parser.error('ANTHROPIC_API_KEY must be supplied by the operator')
    try:
        run(args.binary.resolve(strict=True), args.model, {'ANTHROPIC_API_KEY': key})
    except Exception:
        # Raw exceptions/provider output may contain confidential material.
        print('FAIL: inspect the retained private artifacts; no automatic retry.')
        return 1
    print('PASS: coding task, four tools, sandbox test, usage, and session resume')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
