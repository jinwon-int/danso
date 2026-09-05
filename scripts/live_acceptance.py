#!/usr/bin/env python3
"""Opt-in provider acceptance. All artifacts are retained in a private temp tree."""
import argparse
import json
import os
from pathlib import Path
import secrets
import stat
import subprocess
import tempfile

PROVIDERS = {
    'anthropic': ('ANTHROPIC_API_KEY', 'DANSO_ANTHROPIC_BASE_URL'),
    'openai': ('OPENAI_API_KEY', 'DANSO_OPENAI_BASE_URL'),
    'glm': ('ZAI_API_KEY', 'DANSO_GLM_BASE_URL'),
}
EFFORTS = ('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')

BROKEN = '#!/bin/bash\nprintf "%s\\n" "$(($1 - $2))"\n'
FIXED = BROKEN.replace('$1 - $2', '$1 + $2')
REPORT = 'Fixed subtraction to addition; bash test.sh passed.\n'
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


def validate_calls(calls):
    require([c.get('name') for c in calls] == ['read', 'read', 'edit', 'bash', 'write'],
            'unexpected tool sequence')
    require(sorted(c.get('arguments', {}).get('path', '') for c in calls[:2])
            == ['add.sh', 'test.sh'], 'expected both source and test reads')
    require(all(set(c['arguments']) == {'path'} for c in calls[:2]), 'unexpected read arguments')
    expected = [
        {'path': 'add.sh', 'oldText': '$1 - $2', 'newText': '$1 + $2'},
        {'command': 'bash test.sh'},
        {'path': 'report.md', 'content': REPORT},
    ]
    require([c.get('arguments') for c in calls[2:]] == expected,
            'unexpected edit, test command, or report')


def run(binary, model, provider_env, provider='anthropic', reasoning_effort=None, compact_at_bytes=None):
    require(compact_at_bytes is None or 8192 <= compact_at_bytes <= 24576,
            'acceptance compaction threshold must be 8192..24576')
    key_name, base_name = PROVIDERS[provider]
    require(reasoning_effort is None or (provider != 'anthropic' and reasoning_effort in EFFORTS),
            'unsupported reasoning effort')
    root = Path(tempfile.mkdtemp(prefix='danso-acceptance-'))
    print(f'Artifacts retained: {root}', flush=True)
    repo = root / 'repo'
    repo.mkdir(mode=0o700)
    home = root / 'home'
    home.mkdir(mode=0o700)
    session = root / 'session.jsonl'
    source = BROKEN
    test_file = TEST
    if compact_at_bytes is not None:
        source += '#' + 'x' * (compact_at_bytes * 2) + '\n'
        test_file += f"printf '%0{compact_at_bytes * 2}d\\n' 0\n"
    save(repo / 'add.sh', source)
    save(repo / 'test.sh', test_file)
    token = secrets.token_hex(12)
    prompts = [
        'Make exactly five tool calls in this order: read add.sh, read test.sh, edit, bash, write. '
        'For each read supply only path. Fix add.sh using edit by changing only '
        'oldText `$1 - $2` to newText `$1 + $2`. Do not change test.sh. Run exactly '
        '`bash test.sh` with bash. Use write with path report.md and exactly this content: '
        f'{REPORT!r} (including the trailing newline). Finish with a short explanation. '
        'Remember this conversation '
        f'token for my next request: {token}',
        'Without calling any tools, return only the conversation token I gave you '
        'in my previous request.'
    ]
    if compact_at_bytes is not None:
        prompts[0] += ' Make one tool call per response and wait for its result before choosing the next call.'
    env = {'PATH': '/usr/bin:/bin', 'HOME': str(home)}
    env.update({k: provider_env[k] for k in (key_name, base_name) if k in provider_env})
    summaries = []
    try:
        first_bytes = None
        for index, prompt in enumerate(prompts, 1):
            command = [str(binary), '--cwd', str(repo), '--session', str(session),
                       '--provider', provider, '--model', model, '--max-turns', '24' if compact_at_bytes else '8',
                       '--timeout-seconds', '300' if compact_at_bytes else '180',
                       '--tool-timeout-seconds', '10', '-p', '--', prompt]
            if compact_at_bytes is not None:
                command[1:1] = ['--compact-at-bytes', str(compact_at_bytes)]
            if reasoning_effort is not None:
                command[1:1] = ['--reasoning-effort', reasoning_effort]
            # Danso supervises its workers; allow its own timeout to finish first.
            result = subprocess.run(command, env=env, text=True, capture_output=True, timeout=315 if compact_at_bytes else 195)
            save(root / f'run-{index}.stdout', result.stdout)
            save(root / f'run-{index}.stderr', result.stderr)
            require(result.returncode == 0, f'run {index} failed (exit {result.returncode})')
            require(bool(result.stdout.strip()), 'empty final answer')
            summaries.append(usage(result.stderr))
            transcript = read_regular(session)
            entries = [json.loads(line) for line in transcript.splitlines()]
            results = [e['message'] for e in entries if e.get('message', {}).get('role') == 'toolResult']
            if index == 1:
                require(read_regular(repo / 'add.sh') == source.replace('$1 - $2', '$1 + $2'), 'incorrect source change')
                require(read_regular(repo / 'test.sh') == test_file, 'test file changed')
                require(read_regular(repo / 'report.md') == REPORT, 'incorrect report')
                require(all(not r.get('isError', True) for r in results), 'tool failure')
                require({r.get('toolName') for r in results} == {'read', 'edit', 'bash', 'write'},
                        'four-tool round trip missing')
                calls = [c for e in entries for c in e.get('message', {}).get('content', [])
                         if isinstance(c, dict) and c.get('type') == 'toolCall']
                validate_calls(calls)
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
        compactions = sum(e.get('customType') == 'danso.compaction.v1' for e in entries)
        if compact_at_bytes is not None:
            require(compactions >= 2, 'expected multiple compactions')
        save(root / 'result.json', json.dumps({'status': 'passed', 'provider': provider, 'model': model,
             'reasoning_effort': reasoning_effort, 'compact_at_bytes': compact_at_bytes, 'compactions': compactions,
             'runs': summaries, 'cost': 'unknown; costUsd=0 is a compatibility placeholder'}, indent=2) + '\n')
        return root
    except Exception:
        save(root / 'result.json', json.dumps({'status': 'failed', 'completed_runs': len(summaries)}) + '\n')
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--live', action='store_true', help='authorize two real provider invocations')
    parser.add_argument('--model', required=True)
    parser.add_argument('--provider', choices=PROVIDERS, default='anthropic')
    parser.add_argument('--base-url', help='explicitly trusted API base; receives the selected provider key')
    parser.add_argument('--reasoning-effort', choices=EFFORTS)
    parser.add_argument('--compact-at-bytes', type=int, help='stress compaction with large read/test output (8192..24576)')
    parser.add_argument('--binary', type=Path, default=Path('target/debug/danso'))
    args = parser.parse_args()
    if not args.live:
        parser.error('--live is required; this sends requests and may incur charges')
    key_name, base_name = PROVIDERS[args.provider]
    if args.provider == 'anthropic' and args.reasoning_effort is not None:
        parser.error('reasoning-effort is unsupported by the Anthropic adapter')
    if os.environ.get(base_name) and not args.base_url:
        parser.error('an ambient endpoint override exists; select it explicitly with --base-url')
    key = os.environ.get(key_name)
    if not key:
        parser.error(f'{key_name} must be supplied by the operator')
    env = {key_name: key}
    if args.base_url:
        env[base_name] = args.base_url
    try:
        run(args.binary.resolve(strict=True), args.model, env, args.provider, args.reasoning_effort, args.compact_at_bytes)
    except Exception:
        # Raw exceptions/provider output may contain confidential material.
        print('FAIL: inspect the retained private artifacts; no automatic retry.')
        return 1
    print('PASS: coding task, four tools, sandbox test, usage, and session resume')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
