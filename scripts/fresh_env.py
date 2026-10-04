#!/usr/bin/env python3
"""Fresh-environment reproduction for roadmap #33 stage C (issue #204).

The danso binary runs inside a clean, digest-pinned container that has no
Python, Node or ccc-node; this harness and its loopback fakes run outside it
on the CI host and reach each other over host networking. Every command run
inside the container is one README.md or docs/* names; where the docs are
silent the harness records a doc gap (docs/fresh-environment.md) rather than
guessing.

The steps are an ordered list: part 2 of #204 appends steps 5-10 to STEPS
and removes them from PENDING. Any failed row stops the run and fails it.

    python3 scripts/fresh_env.py --bin target/release/danso
    python3 scripts/fresh_env.py --self-test      # fakes and table only, no docker
"""
import argparse
import http.server
import json
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from test_e2e import anthropic_stream, reply  # noqa: E402  (the e2e fake provider's wire format)

# debian:stable-slim, linux/amd64 + others, pinned by manifest-list digest
# (Docker Hub, pushed 2026-09-19). Bump deliberately, never by tag.
IMAGE = ('debian:stable-slim@'
         'sha256:5bc3287b25407c965a30f38e32603dc253a3869e1b12a21ac09bfc27fd8b13ce')

HOME = '/root'
DANSO_HOME = f'{HOME}/.danso'
STATE_ROOT = f'{DANSO_HOME}/telegram'      # the documented default state root
WORKSPACE = '/srv/danso/workspace'          # docs/service-install.md example
TOKEN_FILE = '/srv/danso/telegram.token'    # docs/service-install.md example
DIST = '/tmp/danso-dist/danso'              # where the unreleased build is copied in
PROVIDER_KEY = 'fresh-env-fixture-key-not-a-secret'
BOT_TOKEN = '000000:FRESH-ENV-NOT-A-REAL-TOKEN'
FORBIDDEN = ('python3', 'python', 'node', 'nodejs', 'ccc-node')
REPLY_TEXT = 'fresh-env provider ok'

PENDING = [
    (5, 'Telegram module: single turn + token lock'),
    (6, 'memory: write, search, inject, survives restart'),
    (7, 'cron: one scheduled run + failure spool'),
    (8, 'backup, restore to a new target, start from it'),
    (9, 'kill -9, restart, orphan cleanup, no duplicate effects'),
    (10, 'update, then roll back'),
]

# Runs one command inside the container and reports, on its last stderr line,
# exit code, wall time and the danso process's peak RSS. Slim images have no
# ps or /usr/bin/time, so VmHWM is sampled from /proc every 20 ms while the
# process lives; a process that exits before the first sample reports none.
SAMPLER = r'''
start=$(date +%s%N)
"$@" &
pid=$!
hwm=
while [ -r /proc/$pid/status ]; do
  grep -q '^State:[[:space:]]*Z' /proc/$pid/status 2>/dev/null && break
  if grep -q '^Name:[[:space:]]*danso' /proc/$pid/status 2>/dev/null; then
    v=$(sed -n 's/^VmHWM:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' /proc/$pid/status 2>/dev/null)
    [ -n "$v" ] && hwm=$v
  fi
  sleep 0.02
done
wait $pid
rc=$?
end=$(date +%s%N)
printf 'FRESHENV rc=%s ns=%s rss_kb=%s\n' "$rc" "$((end - start))" "$hwm" >&2
exit $rc
'''

# docs/service-install.md "Running without systemd": start the supervisor,
# poll `service status` until it reports available, then read VmRSS of the
# service and its supervisor.
SERVICE_START = r'''
start=$(date +%s%N)
setsid danso service run --data-dir "$1" --supervise >/tmp/fresh-env-service.log 2>&1 </dev/null &
sup=$!
echo "$sup" >/tmp/fresh-env-supervisor.pid
tries=0
until danso service status --data-dir "$1" >/tmp/fresh-env-status.txt 2>&1; do
  if ! kill -0 "$sup" 2>/dev/null; then
    echo "supervisor exited before the service became available" >&2
    cat /tmp/fresh-env-status.txt /tmp/fresh-env-service.log >&2
    printf 'FRESHENV rc=1 ns=%s\n' "$(( $(date +%s%N) - start ))" >&2
    exit 1
  fi
  tries=$((tries + 1))
  if [ "$tries" -ge 300 ]; then
    echo "service not available within 30 s" >&2
    cat /tmp/fresh-env-status.txt /tmp/fresh-env-service.log >&2
    printf 'FRESHENV rc=1 ns=%s\n' "$(( $(date +%s%N) - start ))" >&2
    exit 1
  fi
  sleep 0.1
done
ready=$(date +%s%N)
cat /tmp/fresh-env-status.txt
svc=$(sed -n 's/.*"pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/service.pid")
rss=$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' "/proc/$svc/status")
sup_rss=$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' "/proc/$sup/status")
printf 'service pid=%s supervisor pid=%s\n' "$svc" "$sup"
printf 'FRESHENV rc=0 ns=%s ready_ns=%s rss_kb=%s supervisor_rss_kb=%s\n' \
  "$((ready - start))" "$((ready - start))" "$rss" "$sup_rss" >&2
'''

# `danso service stop`, then wait for the supervisor to finish and the service
# process to be gone: a stop that the supervisor answers with a restart is a
# failure, not a pass.
SERVICE_STOP = r'''
start=$(date +%s%N)
sup=$(cat /tmp/fresh-env-supervisor.pid)
svc=$(sed -n 's/.*"pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/service.pid")
danso service stop --data-dir "$1"
rc=$?
tries=0
while kill -0 "$sup" 2>/dev/null || kill -0 "$svc" 2>/dev/null; do
  tries=$((tries + 1))
  if [ "$tries" -ge 300 ]; then
    echo "supervisor or service still running 30 s after stop" >&2
    cat /tmp/fresh-env-service.log >&2
    rc=1
    break
  fi
  sleep 0.1
done
end=$(date +%s%N)
printf 'FRESHENV rc=%s ns=%s\n' "$rc" "$((end - start))" >&2
exit $rc
'''


class Fail(Exception):
    pass


class FakeProvider:
    """Loopback Anthropic Messages endpoint: scripts/test_e2e.py's fixture
    reply and SSE framing, answering every authenticated turn with one text
    block. A request without the fixture key gets 401, so a passing turn
    proves the documented credential variable reached the provider."""

    def __init__(self):
        self.requests = []
        self.unauthenticated = 0
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                if self.headers.get('x-api-key') != PROVIDER_KEY:
                    owner.unauthenticated += 1
                    self.answer(401, 'application/json', json.dumps(
                        {'type': 'error', 'error': {'type': 'authentication_error',
                                                    'message': 'invalid x-api-key'}}).encode())
                    return
                owner.requests.append(body)
                value = reply([{'type': 'text', 'text': REPLY_TEXT}])
                if body.get('stream') is True:
                    self.answer(200, 'text/event-stream', anthropic_stream(value))
                else:
                    self.answer(200, 'application/json', json.dumps(value).encode())

            def answer(self, status, kind, data):
                self.send_response(status)
                self.send_header('Content-Type', kind)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_port}'


class FakeBotApi:
    """Loopback Telegram Bot API: the stub from tests/service_supervise.rs
    (`{"ok":true,"result":[]}` for every method), ported so the service can be
    driven from outside the container. getUpdates holds for the requested
    long-poll timeout (capped at 1 s) after the first call, as an idle real
    Bot API would, so the service does not spin."""

    def __init__(self):
        self.calls = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def handle_one(self):
                parsed = urllib.parse.urlsplit(self.path)
                method = parsed.path.rsplit('/', 1)[-1]
                length = int(self.headers.get('Content-Length') or 0)
                raw = self.rfile.read(length) if length else b''
                owner.calls.append(method)
                if method == 'getUpdates' and len([c for c in owner.calls if c == 'getUpdates']) > 1:
                    time.sleep(min(poll_timeout(parsed.query, raw), 1.0))
                data = b'{"ok":true,"result":[]}'
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = handle_one

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_port}'


def poll_timeout(query, raw):
    values = urllib.parse.parse_qs(query)
    try:
        values.update(json.loads(raw) if raw.startswith(b'{') else urllib.parse.parse_qs(raw.decode()))
    except (ValueError, UnicodeDecodeError):
        pass
    value = values.get('timeout', 1)
    if isinstance(value, list):
        value = value[0] if value else 1
    try:
        return max(float(value), 0.0)
    except (TypeError, ValueError):
        return 1.0


def serve(*fakes):
    for fake in fakes:
        threading.Thread(target=fake.server.serve_forever, daemon=True).start()


def shutdown(*fakes):
    for fake in fakes:
        fake.server.shutdown()
        fake.server.server_close()


class Container:
    def __init__(self, image):
        self.image = image
        self.name = f'danso-fresh-env-{uuid.uuid4().hex[:12]}'
        self.base_env = {'HOME': HOME, 'PATH': '/usr/local/bin:/usr/bin:/bin'}

    def start(self, binary):
        docker('pull', '--quiet', self.image)
        # --init: tini as pid 1 reaps the supervisor once `service stop` ends it.
        docker('create', '--name', self.name, '--network', 'host', '--init',
               self.image, 'sleep', 'infinity')
        docker('cp', str(binary), f'{self.name}:/tmp/danso')
        docker('start', self.name)
        # docker cp cannot create parent directories; keep the copied build
        # outside PATH so step 2 is the install, not the copy.
        self.sh(f'mkdir -p {Path(DIST).parent} && mv /tmp/danso {DIST}')

    def remove(self):
        subprocess.run(['docker', 'rm', '-f', self.name], capture_output=True)

    def exec(self, argv, env=None, timeout=120):
        flags = []
        for key, value in {**self.base_env, **(env or {})}.items():
            flags += ['-e', f'{key}={value}']
        return subprocess.run(['docker', 'exec', *flags, self.name, *argv],
                              text=True, capture_output=True, timeout=timeout)

    def sh(self, script, *args, env=None, timeout=120):
        return self.exec(['sh', '-c', script, 'sh', *args], env=env, timeout=timeout)


def docker(*args):
    done = subprocess.run(['docker', *args], text=True, capture_output=True)
    if done.returncode != 0:
        raise Fail(f'docker {args[0]} failed ({done.returncode}): {done.stderr.strip()}')
    return done.stdout


class Row:
    def __init__(self, step, name, command):
        self.step, self.name, self.command = step, name, command
        self.rc = None
        self.wall_s = None
        self.ready_s = None
        self.rss_kb = None
        self.extra_rss = None
        self.passed = False
        self.note = ''

    def as_dict(self):
        return dict(vars(self))


def measured(done, row):
    """Fill `row` from the FRESHENV line the in-container script printed."""
    lines = [line for line in done.stderr.splitlines() if line.startswith('FRESHENV ')]
    fields = dict(part.split('=', 1) for part in lines[-1].split()[1:]) if lines else {}
    row.rc = int(fields['rc']) if fields.get('rc', '').lstrip('-').isdigit() else done.returncode
    if fields.get('ns', '').isdigit():
        row.wall_s = int(fields['ns']) / 1e9
    if fields.get('ready_ns', '').isdigit():
        row.ready_s = int(fields['ready_ns']) / 1e9
    if fields.get('rss_kb', '').isdigit():
        row.rss_kb = int(fields['rss_kb'])
    if fields.get('supervisor_rss_kb', '').isdigit():
        row.extra_rss = f'supervisor {int(fields["supervisor_rss_kb"]) / 1024:.1f} MiB'
    return row


def require(row, ok, why, done=None):
    if not ok:
        detail = ''
        if done is not None:
            detail = f'\n--- stdout ---\n{done.stdout}\n--- stderr ---\n{done.stderr}'
        row.note = (row.note + '; ' if row.note else '') + why
        raise Fail(f'step {row.step} {row.name}: {why}{detail}')


def run_danso(ctx, row, args, env=None):
    done = ctx.container.sh(SAMPLER, 'danso', *args, env=env)
    measured(done, row)
    return done


# ---- steps ---------------------------------------------------------------
# Each step appends its rows to ctx.rows before running them, so a failure
# leaves the failing row in the table.

def step_preflight(ctx):
    row = ctx.row(1, 'preflight: no python/node/ccc-node, ldd',
                  'command -v ' + ' '.join(FORBIDDEN) + f'; ldd {DIST}')
    probe = ' '.join(f'command -v {name} && echo "present: {name}";' for name in FORBIDDEN)
    start = time.monotonic()
    done = ctx.container.sh(probe + ' exit 0')
    row.rc = 0 if 'present:' not in done.stdout else 1
    present = re.findall(r'present: (\S+)', done.stdout)
    require(row, not present, f'forbidden runtime present: {", ".join(present)}', done)
    # README: Linux 5.3+, procfs/pidfd, /bin/bash.
    facts = ctx.container.sh(
        f'uname -r; cat /etc/debian_version; test -x /bin/bash && echo bash=yes; ldd {DIST}')
    row.wall_s = time.monotonic() - start
    require(row, facts.returncode == 0, 'ldd or environment probe failed', facts)
    lines = facts.stdout.splitlines()
    ctx.env['kernel'], ctx.env['debian_version'] = lines[0], lines[1]
    require(row, 'bash=yes' in lines, '/bin/bash missing (README requires it)', facts)
    ctx.env['ldd'] = [line.strip() for line in lines[3:]]
    row.note = 'absent: ' + ', '.join(FORBIDDEN) + f'; {len(ctx.env["ldd"])} ldd entries'
    row.passed = True


def step_install(ctx):
    row = ctx.row(2, 'install binary (signature: n/a, unreleased PR build)',
                  f'install -m 0755 {DIST} /usr/local/bin/danso')
    done = ctx.container.sh(SAMPLER, 'install', '-m', '0755', DIST, '/usr/local/bin/danso')
    measured(done, row)  # no RSS: the sampler reads danso processes only
    require(row, row.rc == 0, 'install failed', done)
    where = ctx.container.sh('command -v danso')
    require(row, where.stdout.strip() == '/usr/local/bin/danso', 'danso not on PATH after install', where)
    row.note = ('no signed manifest exists for a PR build; minisign -V -H '
                '(docs/release-signing.md) applies to Release artifacts only')
    row.passed = True

    row = ctx.row(2, 'config.toml per docs + `danso config check`', 'danso config check')
    # docs/service-install.md "Installing": owner-only token file named from
    # $DANSO_HOME/config.toml; the state root itself is left for danso to create.
    config = '\n'.join([
        '[telegram]', f'token_file = "{TOKEN_FILE}"', 'allowed_user_ids = [1]',
        '[provider]', 'model = "fixture-model"',
        '[core]', f'workspace = "{WORKSPACE}"', ''])
    setup = ctx.container.sh(
        'set -e; umask 077; mkdir -p "$1" "$2" "$(dirname "$3")";'
        ' printf "%s\\n" "$4" > "$3"; printf "%s" "$5" > "$2/config.toml"',
        WORKSPACE, DANSO_HOME, TOKEN_FILE, BOT_TOKEN, config)
    require(row, setup.returncode == 0, 'writing config.toml/token file failed', setup)
    done = run_danso(ctx, row, ['config', 'check'])
    require(row, row.rc == 0, 'danso config check failed', done)
    try:
        report = json.loads(done.stdout)
    except json.JSONDecodeError:
        report = {}
    require(row, report.get('valid') is True, 'config check did not report valid', done)
    row.note = f'valid; set_keys={len(report.get("set_keys", []))}'
    row.passed = True


def step_provider(ctx):
    session = f'{DANSO_HOME}/sessions/fresh-env.jsonl'
    row = ctx.row(3, 'provider auth: print-mode turn on loopback fake',
                  f'danso --cwd {WORKSPACE} --trust-project --session {session} '
                  "--model fixture-model -p '...'")
    mk = ctx.container.sh(f'mkdir -p {DANSO_HOME}/sessions')  # README "Build and run"
    require(row, mk.returncode == 0, 'mkdir sessions failed', mk)
    before = len(ctx.provider.requests)
    done = run_danso(ctx, row, [
        '--cwd', WORKSPACE, '--trust-project', '--session', session,
        '--model', 'fixture-model', '-p', 'Explain this repository'],
        env={'ANTHROPIC_API_KEY': PROVIDER_KEY, 'DANSO_ANTHROPIC_BASE_URL': ctx.provider.url})
    require(row, row.rc == 0, f'print-mode turn exited {row.rc}', done)
    require(row, REPLY_TEXT in done.stdout, 'final answer missing from stdout', done)
    usage = [line for line in done.stderr.splitlines() if line.startswith('PIRI_USAGE=')]
    require(row, len(usage) == 1, 'expected exactly one PIRI_USAGE line', done)
    require(row, len(ctx.provider.requests) > before and not ctx.provider.unauthenticated,
            'fake provider saw no authenticated request', done)
    totals = json.loads(usage[0].split('=', 1)[1])
    row.note = (f'usage line present: requests={totals.get("requests")} '
                f'totalTokens={totals.get("totalTokens")}')
    row.passed = True


def step_service(ctx):
    env = {
        # docs/telegram.md: loopback fake Bot API and a short long-poll so a
        # stop is not held by an idle 25 s poll. Token, allowlist, model and
        # workspace come from config.toml (step 2); provider per providers.md.
        'DANSO_TELEGRAM_API_BASE_URL': ctx.bot.url,
        'DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS': '1',
        'ANTHROPIC_API_KEY': PROVIDER_KEY,
        'DANSO_ANTHROPIC_BASE_URL': ctx.provider.url,
    }
    row = ctx.row(4, '`danso service install` on a host without systemd', 'danso service install')
    done = run_danso(ctx, row, ['service', 'install'], env=env)
    require(row, row.rc == 0, 'service install failed', done)
    require(row, 'systemd not present' in done.stdout, 'expected the no-systemd guidance', done)
    row.note = 'prints the Termux:Boot equivalent, installs nothing'
    row.passed = True

    row = ctx.row(4, '`service run --supervise` -> available',
                  f'danso service run --data-dir {STATE_ROOT} --supervise')
    ctx.service_started = True
    done = ctx.container.sh(SERVICE_START, STATE_ROOT, env=env, timeout=60)
    measured(done, row)
    require(row, row.rc == 0, 'service did not become available', done)
    require(row, 'Bot status: available' in done.stdout, 'status did not say available', done)
    require(row, 'getUpdates' in ctx.bot.calls, 'fake Bot API was never polled', done)
    row.note = 'ready = `danso service status` exit 0 (Bot status: available)'
    row.passed = True

    row = ctx.row(4, '`danso doctor` against the running service', 'danso doctor')
    done = run_danso(ctx, row, ['doctor'], env=env)
    try:
        summary = json.loads(done.stdout).get('summary', {})
    except json.JSONDecodeError:
        summary = {}
    require(row, row.rc == 0, f'doctor exited {row.rc} (1 = a check failed)', done)
    row.note = f'ok={summary.get("ok")} warn={summary.get("warn")} fail={summary.get("fail")}'
    if summary.get('warn'):
        warned = [c['id'] for c in json.loads(done.stdout).get('checks', []) if c.get('status') == 'warn']
        row.note += ' (' + ', '.join(warned) + ')'
    row.passed = True

    row = ctx.row(4, '`danso service stop` -> supervisor exits',
                  f'danso service stop --data-dir {STATE_ROOT}')
    done = ctx.container.sh(SERVICE_STOP, STATE_ROOT, env=env, timeout=60)
    measured(done, row)
    require(row, row.rc == 0, 'service stop failed or supervision did not end', done)
    require(row, 'Bot stop: drained' in done.stdout, 'stop did not report drained', done)
    ctx.service_started = False
    row.note = 'Bot stop: drained; supervisor and service gone'
    row.passed = True


STEPS = [step_preflight, step_install, step_provider, step_service]


class Context:
    def __init__(self, container, provider, bot):
        self.container, self.provider, self.bot = container, provider, bot
        self.rows = []
        self.env = {}
        self.service_started = False

    def row(self, step, name, command):
        row = Row(step, name, command)
        self.rows.append(row)
        return row


def mib(kb):
    return '' if kb is None else f'{kb / 1024:.1f} MiB'


def secs(value):
    return '' if value is None else f'{value:.3f}'


def render_table(rows, error=None):
    out = ['| Step | Check | Exit | Wall s | Ready s | RSS (danso) | Result | Notes |',
           '| --- | --- | ---: | ---: | ---: | ---: | --- | --- |']
    for row in rows:
        rss = mib(row.rss_kb)
        if row.extra_rss:
            rss = f'{rss} ({row.extra_rss})'
        result = 'pass' if row.passed else 'FAIL'
        out.append(f'| {row.step} | {row.name} | {"" if row.rc is None else row.rc} | '
                   f'{secs(row.wall_s)} | {secs(row.ready_s)} | {rss} | {result} | '
                   f'{row.note.replace("|", "/")} |')
    for number, name in PENDING:
        out.append(f'| {number} | {name} |  |  |  |  | pending part 2 (#204) |  |')
    if error:
        out.append('')
        out.append(f'Stopped: {error.splitlines()[0]}')
    return '\n'.join(out)


def render_environment(env, binary):
    lines = [f'- image: `{IMAGE}` (host networking, `--init`)',
             f'- kernel: `{env.get("kernel", "?")}`; debian `{env.get("debian_version", "?")}`',
             f'- binary: `{binary}` copied in alone, {binary.stat().st_size} bytes']
    if env.get('ldd'):
        lines.append('- `ldd`:')
        lines += [f'  - `{entry}`' for entry in env['ldd']]
    return '\n'.join(lines)


def run(binary, image, summary=None, json_out=None):
    provider, bot = FakeProvider(), FakeBotApi()
    serve(provider, bot)
    container = Container(image)
    ctx = Context(container, provider, bot)
    error = None
    try:
        container.start(binary)
        for step in STEPS:
            step(ctx)
    except (Fail, subprocess.TimeoutExpired) as caught:
        error = str(caught)
    finally:
        if ctx.service_started:
            log = container.sh('cat /tmp/fresh-env-service.log 2>/dev/null')
            if log.stdout:
                print('--- service log ---\n' + log.stdout, file=sys.stderr)
        container.remove()
        shutdown(provider, bot)
    report = ('## Fresh environment (#204)\n\n' + render_environment(ctx.env, binary)
              + '\n\n' + render_table(ctx.rows, error) + '\n')
    print(report)
    if summary:
        with open(summary, 'a', encoding='utf-8') as handle:
            handle.write(report + '\n')
    if json_out:
        Path(json_out).write_text(json.dumps(
            {'image': image, 'environment': ctx.env, 'rows': [r.as_dict() for r in ctx.rows],
             'pending': PENDING, 'error': error}, indent=2) + '\n')
    if error:
        print(error, file=sys.stderr)
        return 1
    return 0


def self_test():
    """Exercise the fakes and the renderer without docker or a danso binary."""
    provider, bot = FakeProvider(), FakeBotApi()
    serve(provider, bot)
    try:
        def post(url, body, headers):
            request = urllib.request.Request(url, json.dumps(body).encode(), headers, method='POST')
            try:
                with urllib.request.urlopen(request, timeout=5) as response:
                    return response.status, response.read()
            except urllib.error.HTTPError as caught:
                return caught.code, caught.read()

        status, _ = post(provider.url + '/v1/messages', {'stream': True}, {'x-api-key': 'wrong'})
        assert status == 401 and provider.unauthenticated == 1
        status, body = post(provider.url + '/v1/messages', {'stream': True},
                            {'x-api-key': PROVIDER_KEY, 'Content-Type': 'application/json'})
        assert status == 200 and REPLY_TEXT.encode() in body and b'message_stop' in body
        status, body = post(provider.url + '/v1/messages', {},
                            {'x-api-key': PROVIDER_KEY, 'Content-Type': 'application/json'})
        assert status == 200 and json.loads(body)['content'][0]['text'] == REPLY_TEXT
        for _ in range(2):
            started = time.monotonic()
            status, body = post(f'{bot.url}/bot{BOT_TOKEN}/getUpdates', {'timeout': 5},
                                {'Content-Type': 'application/json'})
            assert status == 200 and json.loads(body) == {'ok': True, 'result': []}
        assert 0.9 <= time.monotonic() - started < 3, 'second poll holds about 1 s'
        assert poll_timeout('timeout=3', b'') == 3.0 and poll_timeout('', b'') == 1.0
    finally:
        shutdown(provider, bot)

    row = Row(1, 'x', 'y')
    done = subprocess.CompletedProcess([], 0, '', 'noise\nFRESHENV rc=0 ns=1500000000 rss_kb=2048\n')
    measured(done, row)
    assert (row.rc, row.wall_s, row.rss_kb) == (0, 1.5, 2048)
    table = render_table([row])
    assert '| 1 | x | 0 | 1.500 |  | 2.0 MiB | FAIL |' in table
    assert all(f'| {n} |' in table and 'pending part 2 (#204)' in table for n, _ in PENDING)
    assert [s.__name__ for s in STEPS] == ['step_preflight', 'step_install',
                                           'step_provider', 'step_service']
    print('fresh_env self-test: ok')
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--bin', default='target/release/danso', type=Path)
    parser.add_argument('--image', default=IMAGE)
    parser.add_argument('--summary', help='append the report here (e.g. $GITHUB_STEP_SUMMARY)')
    parser.add_argument('--json', dest='json_out', help='write the measured rows as JSON')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    binary = args.bin.resolve()
    if not binary.is_file():
        print(f'no binary at {binary}; build it with cargo build --release --locked', file=sys.stderr)
        return 2
    return run(binary, args.image, args.summary, args.json_out)


if __name__ == '__main__':
    sys.exit(main())
