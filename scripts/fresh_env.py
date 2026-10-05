#!/usr/bin/env python3
"""Fresh-environment reproduction for roadmap #33 stage C (issue #204).

The danso binary runs inside a clean, digest-pinned container that has no
Python, Node or ccc-node; this harness and its loopback fakes run outside it
on the CI host and reach each other over host networking. Every command run
inside the container is one README.md or docs/* names; where the docs are
silent the harness records a doc gap (docs/fresh-environment.md) rather than
guessing.

The steps are an ordered list: part 1 of #204 (#205) wrote steps 1-4, part 2
appended steps 5-10 to STEPS and emptied PENDING. Any failed row stops the run
and fails it. A row tied to a documented code defect (KNOWN_GAPS) passes only
while that defect reproduces exactly as documented, so a fix cannot go
unnoticed and a different failure is still a failure. K1 (#209) and K2 (#210)
are fixed and their rows are ordinary checks now; the table is empty until
the next defect this reproduction finds.

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
# docs/release-signing.md "Rehearsing apply and rollback offline": the
# committed throwaway-key fixture, trusted only by a scratch home.
RELEASE_FIXTURE = ROOT / 'tests' / 'fixtures' / 'release'
FIXTURE_DIR = '/srv/danso-release-fixture'
FIXTURE_ARTIFACT = 'danso-9.9.9-ok.tar.gz'
REHEARSAL_HOME = '/srv/danso-update-rehearsal'
EXIT_VERIFICATION_FAILED = 13                # docs/unified-design.md §6.3
DIST = '/tmp/danso-dist/danso'              # where the unreleased build is copied in
PROVIDER_KEY = 'fresh-env-fixture-key-not-a-secret'
BOT_TOKEN = '000000:FRESH-ENV-NOT-A-REAL-TOKEN'
FORBIDDEN = ('python3', 'python', 'node', 'nodejs', 'ccc-node')
REPLY_TEXT = 'fresh-env provider ok'
CHAT_ID = 1                                  # allowed_user_ids = [1]; a private chat's id is its user's
MISSING_WORKSPACE = '/srv/danso/absent-workspace'
MEMORY_TOKEN = 'freshenvmarker'
MEMORY_FACT = f'Release notes on this node are written in Korean and English ({MEMORY_TOKEN}).'
MEMORY_MARKER = 'ccc-node:codex-memory:begin'  # docs/memory.md managed block
SPOOL_DIR = f'{STATE_ROOT}/spool'              # docs/agent-cron.md: $DANSO_HOME/telegram/spool
RESTORE_TARGET = '/srv/danso-restored'
HOLD_PROMPT = 'fresh-env: this turn is killed while in flight'
TURN_COMPLETE = 'Turn complete'
TURN_NOT_STARTED = 'Turn could not start'
RESTART_NOTICE = 'Service restarted. The prior turn did not complete'  # docs/telegram.md

# Every step is in STEPS now; kept so the table and JSON keep their shape.
PENDING = []

# Code defects the reproduction exposes; docs/fresh-environment.md "Known gaps".
# K1 (#209) and K2 (#210) were fixed; their rows now pass as ordinary checks.
KNOWN_GAPS = {}

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
setsid danso service run --data-dir "$1" --supervise >>/tmp/fresh-env-service.log 2>&1 </dev/null &
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

# Step 9: SIGKILL the supervised service, as the OOM killer would
# (docs/service-install.md), then wait until `service status` reports a
# replacement available under a new pid. The supervisor must survive it.
SERVICE_CRASH = r'''
pid_of() { sed -n 's/.*"pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/service.pid" 2>/dev/null; }
start=$(date +%s%N)
sup=$(cat /tmp/fresh-env-supervisor.pid)
old=$(pid_of "$1")
if [ -z "$old" ]; then
  echo "no service pid recorded" >&2
  printf 'FRESHENV rc=1 ns=0\n' >&2
  exit 1
fi
kill -9 "$old"
tries=0
while :; do
  new=$(pid_of "$1")
  if [ -n "$new" ] && [ "$new" != "$old" ] && danso service status --data-dir "$1" >/tmp/fresh-env-status.txt 2>&1; then
    break
  fi
  tries=$((tries + 1))
  if [ "$tries" -ge 300 ] || ! kill -0 "$sup" 2>/dev/null; then
    echo "no replacement available within 30 s, or the supervisor exited" >&2
    cat /tmp/fresh-env-status.txt /tmp/fresh-env-service.log >&2
    printf 'FRESHENV rc=1 ns=%s\n' "$(( $(date +%s%N) - start ))" >&2
    exit 1
  fi
  sleep 0.1
done
ready=$(date +%s%N)
cat /tmp/fresh-env-status.txt
rc=0
if kill -0 "$old" 2>/dev/null; then echo "killed service pid $old still exists" >&2; rc=1; fi
rss=$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' "/proc/$new/status")
sup_rss=$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' "/proc/$sup/status")
printf 'killed pid=%s replacement pid=%s supervisor pid=%s\n' "$old" "$new" "$sup"
printf 'FRESHENV rc=%s ns=%s ready_ns=%s rss_kb=%s supervisor_rss_kb=%s\n' \
  "$rc" "$((ready - start))" "$((ready - start))" "$rss" "$sup_rss" >&2
exit $rc
'''

# VmRSS of the running service, after a Telegram turn it served.
SERVICE_RSS = r'''
svc=$(sed -n 's/.*"pid":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/service.pid")
printf '%s %s\n' "$svc" "$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\) kB$/\1/p' "/proc/$svc/status")"
'''


class Fail(Exception):
    pass


class FakeProvider:
    """Loopback Anthropic Messages endpoint: scripts/test_e2e.py's fixture
    reply and SSE framing, answering every authenticated turn with one text
    block. A request without the fixture key gets 401, so a passing turn
    proves the documented credential variable reached the provider. A request
    whose latest prompt contains `hold_prompt` is held until `release` is
    set, so step 9 can kill the service with that turn in flight."""

    def __init__(self):
        self.requests = []
        self.unauthenticated = 0
        self.hold_prompt = None
        self.held = 0
        self.release = threading.Event()
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
                if owner.hold_prompt and owner.hold_prompt in last_user_text(body):
                    owner.held += 1
                    owner.release.wait(60)
                value = reply([{'type': 'text', 'text': REPLY_TEXT}])
                if body.get('stream') is True:
                    self.answer(200, 'text/event-stream', anthropic_stream(value))
                else:
                    self.answer(200, 'application/json', json.dumps(value).encode())

            def answer(self, status, kind, data):
                try:
                    self.send_response(status)
                    self.send_header('Content-Type', kind)
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # the caller was killed mid-request (step 9)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_port}'


class FakeBotApi:
    """Loopback Telegram Bot API, driven from outside the container. Part 1
    ported the tests/service_supervise.rs stub (`{"ok":true,"result":[]}`);
    part 2 adds what one real turn needs: getUpdates offers the updates the
    harness pushes until a later offset confirms them, as the Bot API does,
    and sendMessage/editMessageText answer with a Message and are recorded in
    `sent`. With nothing to offer, getUpdates holds for the requested
    long-poll timeout (capped at 1 s) after the first call, as an idle real
    Bot API would, so the service does not spin."""

    def __init__(self):
        self.calls = []
        self.sent = []      # {'method', 'chat_id', 'message_id', 'text'}
        self.updates = []   # offered until a getUpdates offset confirms them
        self.lock = threading.Lock()
        self.last_update_id = 1000
        self.last_message_id = 5000
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def handle_one(self):
                parsed = urllib.parse.urlsplit(self.path)
                method = parsed.path.rsplit('/', 1)[-1]
                length = int(self.headers.get('Content-Length') or 0)
                raw = self.rfile.read(length) if length else b''
                owner.calls.append(method)
                if method == 'getUpdates':
                    result = owner.poll(parsed.query, raw)
                elif method in ('sendMessage', 'editMessageText'):
                    result = owner.record(method, raw)
                else:
                    result = []
                data = json.dumps({'ok': True, 'result': result}).encode()
                try:
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # the poller was killed mid-request (step 9)

            do_GET = do_POST = handle_one

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_port}'

    def push(self, text, user_id=CHAT_ID):
        """Offer one private-chat text message from `user_id`."""
        with self.lock:
            self.last_update_id += 1
            self.updates.append({'update_id': self.last_update_id, 'message': {
                'message_id': self.last_update_id, 'date': int(time.time()), 'text': text,
                'from': {'id': user_id, 'is_bot': False, 'first_name': 'fresh-env'},
                'chat': {'id': user_id, 'type': 'private'}}})
            return self.last_update_id

    def poll(self, query, raw):
        offset = poll_param(query, raw, 'offset', None)
        first = self.calls.count('getUpdates') == 1
        deadline = time.monotonic() + min(poll_timeout(query, raw), 1.0)
        while True:
            with self.lock:
                if offset is not None:
                    self.updates = [u for u in self.updates if u['update_id'] >= offset]
                ready = list(self.updates)
            if ready or first or time.monotonic() >= deadline:
                return ready
            time.sleep(0.05)

    def record(self, method, raw):
        try:
            payload = json.loads(raw)
        except ValueError:
            payload = {}
        if not isinstance(payload, dict):
            payload = {}
        with self.lock:
            if method == 'sendMessage':
                self.last_message_id += 1
                message_id = self.last_message_id
            else:
                message_id = payload.get('message_id')
            entry = {'method': method, 'chat_id': payload.get('chat_id'),
                     'message_id': message_id, 'text': str(payload.get('text') or '')}
            self.sent.append(entry)
        return {'message_id': message_id, 'chat': {'id': payload.get('chat_id')},
                'date': int(time.time()), 'text': entry['text']}


def poll_param(query, raw, name, default):
    values = urllib.parse.parse_qs(query)
    try:
        values.update(json.loads(raw) if raw.startswith(b'{') else urllib.parse.parse_qs(raw.decode()))
    except (ValueError, UnicodeDecodeError):
        pass
    value = values.get(name, default)
    if isinstance(value, list):
        value = value[0] if value else default
    try:
        return default if value is None else float(value)
    except (TypeError, ValueError):
        return default


def poll_timeout(query, raw):
    return max(poll_param(query, raw, 'timeout', 1.0), 0.0)


def last_user_text(body):
    """The text of the newest message in an Anthropic request: the prompt of
    the turn being served, not the history the journal replays before it."""
    messages = body.get('messages') if isinstance(body, dict) else None
    if not messages or not isinstance(messages[-1], dict):
        return ''
    content = messages[-1].get('content')
    if isinstance(content, str):
        return content
    texts = [block.get('text', '') for block in content or []
             if isinstance(block, dict) and block.get('type') == 'text']
    return texts[-1] if texts else ''


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

    def copy_in(self, source, destination):
        docker('cp', str(source), f'{self.name}:{destination}')

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
        self.result = None   # overrides 'pass': a known gap, or a step that cannot run
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


def known_gap(row, gap, reproduced, what_changed, done=None):
    """Pass `row` as documented code defect `gap` only while it reproduces;
    once it does not, fail so docs/fresh-environment.md is corrected."""
    require(row, reproduced, f'known gap {gap} no longer reproduces ({what_changed}); '
            'update docs/fresh-environment.md', done)
    row.result = f'known gap {gap}'
    row.passed = True


def run_danso(ctx, row, args, env=None, timeout=120):
    done = ctx.container.sh(SAMPLER, 'danso', *args, env=env, timeout=timeout)
    measured(done, row)
    return done


def json_out(done):
    try:
        value = json.loads(done.stdout)
    except (json.JSONDecodeError, TypeError):
        return {}
    return value if isinstance(value, dict) else {}


def wait_for(predicate, timeout, interval=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return bool(predicate())


def service_env(ctx):
    """docs/telegram.md: loopback fake Bot API and a short long-poll; the
    provider per providers.md; plus the home a restore moved the node to."""
    return {
        'DANSO_TELEGRAM_API_BASE_URL': ctx.bot.url,
        'DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS': '1',
        'ANTHROPIC_API_KEY': PROVIDER_KEY,
        'DANSO_ANTHROPIC_BASE_URL': ctx.provider.url,
        **ctx.home_env,
    }


def start_service(ctx, row, env):
    ctx.service_started = True
    done = ctx.container.sh(SERVICE_START, ctx.state_root, env=env, timeout=60)
    measured(done, row)
    require(row, row.rc == 0, 'service did not become available', done)
    require(row, 'Bot status: available' in done.stdout, 'status did not say available', done)
    return done


def stop_service(ctx, row, env, measure=True):
    done = ctx.container.sh(SERVICE_STOP, ctx.state_root, env=env, timeout=60)
    if measure:
        measured(done, row)
    require(row, done.returncode == 0, 'service stop failed or supervision did not end', done)
    require(row, 'Bot stop: drained' in done.stdout, 'stop did not report drained', done)
    ctx.service_started = False
    return done


def service_rss(ctx, row):
    parts = ctx.container.sh(SERVICE_RSS, ctx.state_root).stdout.split()
    if len(parts) == 2 and parts[1].isdigit():
        row.rss_kb = int(parts[1])


def replies(sent):
    return [d for d in sent if d['method'] == 'sendMessage' and d['text'] == REPLY_TEXT]


def bot_trace(sent):
    return '; '.join(f'{d["method"]}({d["text"][:60]!r})' for d in sent) or 'no Bot API sends'


def telegram_turn(ctx, row, text, timeout=30):
    """Offer one authorized text update and wait for the final answer. Timed
    on the host, from the update being offered to the answer reaching the
    fake Bot API; returns (answered, sends, provider requests) since then."""
    mark, before = len(ctx.bot.sent), len(ctx.provider.requests)
    start = time.monotonic()
    ctx.bot.push(text)
    answered = wait_for(lambda: replies(ctx.bot.sent[mark:]), timeout)
    row.wall_s = time.monotonic() - start
    return answered, ctx.bot.sent[mark:], ctx.provider.requests[before:]


def memory_counts(ctx, env):
    """(fact records, pending distill jobs) from `danso memory check`."""
    report = json_out(ctx.container.exec(['danso', 'memory', 'check'], env=env))
    return (report.get('facts', {}).get('records'), report.get('journal', {}).get('pending'))


def memory_found(ctx, env):
    done = ctx.container.exec(['danso', 'memory', 'search', MEMORY_TOKEN, '--json'], env=env)
    results = json_out(done).get('results') or []
    return [r for r in results if MEMORY_TOKEN in json.dumps(r)], done


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
    # docs/telegram.md: `memory.mode = "read"` gives service turns the managed
    # memory block (#209); step 6 checks a turn's provider request for it.
    config = '\n'.join([
        '[telegram]', f'token_file = "{TOKEN_FILE}"', 'allowed_user_ids = [1]',
        '[provider]', 'model = "fixture-model"',
        '[core]', f'workspace = "{WORKSPACE}"',
        '[memory]', 'mode = "read"', ''])
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


def step_telegram(ctx):
    env = service_env(ctx)
    # Doc gap 4: docs/telegram.md lets the environment win over config.toml,
    # so the service can be pointed at an absent workspace without editing it.
    row = ctx.row(5, 'service with an absent `core.workspace` (doc gap 4)',
                  f'DANSO_TELEGRAM_WORKSPACE={MISSING_WORKSPACE} '
                  f'danso service run --data-dir {STATE_ROOT}')
    try:
        done = run_danso(ctx, row, ['service', 'run', '--data-dir', STATE_ROOT],
                         env={**env, 'DANSO_TELEGRAM_WORKSPACE': MISSING_WORKSPACE}, timeout=30)
    except subprocess.TimeoutExpired:
        require(row, False, 'the service started with an absent workspace (still running after 30 s)')
    require(row, row.rc == 1, f'expected exit 1, got {row.rc}', done)
    require(row, 'workspace does not exist' in done.stderr, 'the refusal did not name the workspace', done)
    absent = ctx.container.sh('test ! -e "$1"', MISSING_WORKSPACE)
    require(row, absent.returncode == 0, 'danso created the absent workspace', absent)
    row.note = 'exit 1 before polling: `Telegram workspace does not exist`; not created'
    row.passed = True

    row = ctx.row(5, '`service run --supervise` -> available',
                  f'danso service run --data-dir {STATE_ROOT} --supervise')
    start_service(ctx, row, env)
    row.note = 'ready = `danso service status` exit 0 (Bot status: available)'
    row.passed = True

    row = ctx.row(5, 'one Telegram turn end to end (fake Bot API)',
                  'getUpdates -> in-process turn -> sendMessage (host-timed)')
    answered, sent, requests = telegram_turn(ctx, row, 'Explain this repository')
    require(row, answered, 'no final answer reached the fake Bot API: ' + bot_trace(sent))
    progress = [d for d in sent if d['method'] == 'sendMessage' and d['text'] != REPLY_TEXT]
    edits = [d for d in sent if d['method'] == 'editMessageText']
    require(row, len(replies(sent)) == 1, 'expected exactly one answer: ' + bot_trace(sent))
    require(row, len(progress) == 1, 'expected one progress message: ' + bot_trace(sent))
    require(row, any(TURN_COMPLETE in d['text'] for d in edits),
            'the progress message was never marked complete: ' + bot_trace(sent))
    require(row, all(d['chat_id'] == CHAT_ID for d in sent), 'a send went to another chat: ' + bot_trace(sent))
    require(row, len(requests) == 1, f'expected one provider request, saw {len(requests)}')
    service_rss(ctx, row)
    row.note = f'1 progress message, {len(edits)} edit(s), 1 answer; 1 provider request'
    row.passed = True

    # docs/telegram.md: "A second consumer exits without polling." It gets
    # its own fake Bot API, so any poll it made would be visible there.
    row = ctx.row(5, 'token lock: second `danso telegram` refused', 'danso telegram (service running)')
    second = FakeBotApi()
    serve(second)
    try:
        done = run_danso(ctx, row, ['telegram'],
                         env={**env, 'DANSO_TELEGRAM_API_BASE_URL': second.url}, timeout=30)
    except subprocess.TimeoutExpired:
        require(row, False, 'a second consumer of the token was not refused (still running after 30 s)')
    finally:
        shutdown(second)
    require(row, row.rc == 1, f'expected exit 1, got {row.rc}', done)
    require(row, not second.calls, f'the second consumer called the Bot API: {second.calls}', done)
    lock = ctx.container.sh('test -f "$1/.telegram-token.lock"', STATE_ROOT)
    require(row, lock.returncode == 0, 'no .telegram-token.lock below the data directory', lock)
    row.note = 'exit 1 (`telegram service failed`), 0 Bot API calls; lock is per data directory (doc gap 5)'
    row.passed = True


def step_memory(ctx):
    env = service_env(ctx)
    row = ctx.row(6, '`danso memory init`', 'danso memory init')
    done = run_danso(ctx, row, ['memory', 'init'], env=env)
    require(row, row.rc == 0, 'memory init failed', done)
    row.note = f'scope {json_out(done).get("scope")}'
    row.passed = True

    row = ctx.row(6, '`danso memory add` (constraint fact)',
                  "danso memory add --kind constraint --text '...'")
    done = run_danso(ctx, row, ['memory', 'add', '--kind', 'constraint', '--text', MEMORY_FACT], env=env)
    require(row, row.rc == 0 and json_out(done).get('added') is True, 'the fact was not added', done)
    row.note = 'added through the write gates'
    row.passed = True

    row = ctx.row(6, '`danso memory search --json` finds it', f'danso memory search {MEMORY_TOKEN} --json')
    done = run_danso(ctx, row, ['memory', 'search', MEMORY_TOKEN, '--json'], env=env)
    hits = [r for r in json_out(done).get('results') or [] if MEMORY_TOKEN in json.dumps(r)]
    require(row, row.rc == 0 and hits, 'search did not return the fact', done)
    row.note = f'{len(hits)} matching result(s)'
    row.passed = True

    # docs/memory.md: the headless run's `--memory read` injects the managed
    # block (doc gap 6: the doc called this `danso run`). Constraints are
    # always in the local-hot block, so the prompt need not mention the fact.
    session = f'{DANSO_HOME}/sessions/fresh-env-memory.jsonl'
    row = ctx.row(6, 'injection: `--memory read` print-mode turn',
                  f'danso --cwd {WORKSPACE} --trust-project --session {session} '
                  "--model fixture-model --memory read -p '...'")
    before = len(ctx.provider.requests)
    done = run_danso(ctx, row, [
        '--cwd', WORKSPACE, '--trust-project', '--session', session, '--model', 'fixture-model',
        '--memory', 'read', '-p', 'What should I keep in mind on this node?'], env=env)
    require(row, row.rc == 0 and REPLY_TEXT in done.stdout, f'print-mode turn exited {row.rc}', done)
    requests = ctx.provider.requests[before:]
    require(row, requests, 'the fake provider saw no request', done)
    body = json.dumps(requests[0])
    require(row, MEMORY_MARKER in body and MEMORY_TOKEN in body,
            'the managed memory block with the fact is not in the provider request', done)
    row.note = 'managed block and the fact are in the provider request'
    row.passed = True

    # docs/telegram.md: `memory.mode` (set to "read" in step 2) turns injection
    # on for service turns; `memory.scope` picks the route (#209, was K1).
    row = ctx.row(6, 'injection: Telegram service turn (`memory.mode = "read"`)',
                  'getUpdates -> in-process turn; provider request inspected')
    answered, sent, requests = telegram_turn(ctx, row, 'What should I keep in mind on this node?')
    require(row, answered and len(requests) == 1, 'the Telegram turn did not complete: ' + bot_trace(sent))
    body = json.dumps(requests[0])
    require(row, MEMORY_MARKER in body and MEMORY_TOKEN in body,
            'the managed memory block with the fact is not in the Telegram turn\'s provider request')
    service_rss(ctx, row)
    row.note = 'turn answered; managed block and the fact are in its provider request'
    row.passed = True

    row = ctx.row(6, 'restart (stop, `--supervise`): fact still found',
                  f'danso service stop --data-dir {STATE_ROOT}; danso service run ... --supervise; '
                  f'danso memory search {MEMORY_TOKEN}')
    counts = memory_counts(ctx, env)
    require(row, counts[0] == 1, f'expected one fact record before the restart, got {counts}')
    stop_service(ctx, row, env, measure=False)
    start_service(ctx, row, env)
    hits, done = memory_found(ctx, env)
    require(row, hits, 'the fact is not found after the restart', done)
    after = memory_counts(ctx, env)
    require(row, after == counts, f'memory changed across the restart: {counts} -> {after}')
    row.note = f'records={counts[0]} before and after; ready = status available'
    row.passed = True


def step_cron(ctx):
    env = service_env(ctx)
    row = ctx.row(7, '`cron add` + `cron tick`: one scheduled run',
                  "danso cron add fresh-ok --schedule 'every 1m' --argv /usr/bin/true; danso cron tick --json")
    add = ctx.container.exec(['danso', 'cron', 'add', 'fresh-ok', '--schedule', 'every 1m',
                              '--prompt', 'fresh-env scheduled job', '--argv', '/usr/bin/true',
                              '--json'], env=env)
    require(row, add.returncode == 0, 'cron add failed', add)
    # docs/agent-cron.md: a never-run interval task with no anchor is due at once.
    done = run_danso(ctx, row, ['cron', 'tick', '--json'], env=env)
    tick = json_out(done)
    ran = [r for r in tick.get('results') or [] if r.get('taskId') == 'fresh-ok']
    require(row, row.rc == 0 and len(ran) == 1 and ran[0].get('status') == 'success',
            'the due task did not run once successfully', done)
    row.note = (f'executed={tick.get("executedActions")}; status=success, '
                f'payload exit {(ran[0].get("headless") or {}).get("exitCode")}')
    row.passed = True

    row = ctx.row(7, 'failing job -> alert spool entry',
                  'danso cron add fresh-fail ... --argv /usr/bin/false --notify telegram-owner-on-failure; '
                  'danso cron run fresh-fail --json')
    add = ctx.container.exec(['danso', 'cron', 'add', 'fresh-fail', '--schedule', 'every 1m',
                              '--prompt', 'fresh-env failing job', '--argv', '/usr/bin/false',
                              '--notify', 'telegram-owner-on-failure', '--json'], env=env)
    require(row, add.returncode == 0, 'cron add failed', add)
    done = run_danso(ctx, row, ['cron', 'run', 'fresh-fail', '--json'], env=env)
    result = json_out(done)
    require(row, row.rc == 1 and result.get('status') == 'failed',
            f'expected exit 1 and status failed, got {row.rc} and {result.get("status")}', done)
    notification = result.get('notification') or {}
    path = notification.get('spoolPath') or ''
    require(row, notification.get('delivery') == 'spooled' and path.startswith(SPOOL_DIR + '/'),
            f'no spool entry under {SPOOL_DIR} (doc gap 8)', done)
    entry = json_out(ctx.container.exec(['cat', path]))
    require(row, entry.get('status') == 'failed' and entry.get('taskId') == 'fresh-fail'
            and entry.get('send') is False, f'unexpected spool entry: {entry}')
    row.note = (f'exit 1, status=failed; {SPOOL_DIR}/{path.rsplit("/", 1)[-1]} '
                f'(event {entry.get("event")}, send=false)')
    row.passed = True


def step_backup(ctx):
    env = service_env(ctx)
    # Stopped first: the restored node below uses another data directory, so
    # the token lock could not keep the two from polling one token (doc gap 5).
    row = ctx.row(8, 'stop the service, `danso backup`', f'danso service stop --data-dir {STATE_ROOT}; danso backup')
    stop_service(ctx, row, env, measure=False)
    done = run_danso(ctx, row, ['backup'], env=env)
    require(row, row.rc == 0, 'backup failed', done)
    lines = done.stdout.strip().splitlines()
    snapshot = lines[-1] if lines else ''
    require(row, snapshot.startswith(f'{DANSO_HOME}/backups/backup-'), 'backup did not print its directory', done)
    manifest = json_out(ctx.container.exec(['cat', f'{snapshot}/manifest.json']))
    names = [c.get('name') for c in manifest.get('components') or []]
    require(row, sorted(names) == ['config', 'conversations', 'journals', 'memory'],
            f'unexpected components: {names}')
    leaked = ctx.container.exec(['grep', '-rqF', '-e', BOT_TOKEN, snapshot])
    require(row, leaked.returncode == 1, 'the bot token is in the snapshot', leaked)
    row.note = f'components: {", ".join(names)}; token not in the snapshot'
    row.passed = True

    row = ctx.row(8, '`danso restore --target <new dir>`', f'danso restore --target {RESTORE_TARGET} <backup>')
    done = run_danso(ctx, row, ['restore', '--target', RESTORE_TARGET, snapshot], env=env)
    report = json_out(done)
    require(row, row.rc == 0 and report.get('restored') is True, 'restore failed', done)
    row.note = 'restored: ' + ', '.join(f'{c.get("name")}={c.get("file_count")}'
                                        for c in report.get('components') or [])
    row.passed = True

    # docs/backup.md "Starting from a restored target" (doc gap 9): the target
    # is the home and, since it holds `conversations/`, the data directory.
    ctx.home_env = {'DANSO_HOME': RESTORE_TARGET, 'DANSO_TELEGRAM_DATA_DIR': RESTORE_TARGET}
    ctx.state_root = RESTORE_TARGET
    env = service_env(ctx)
    row = ctx.row(8, 'service from the restored home',
                  f'DANSO_HOME={RESTORE_TARGET} DANSO_TELEGRAM_DATA_DIR={RESTORE_TARGET} danso config check; '
                  f'danso service run --data-dir {RESTORE_TARGET} --supervise')
    check = ctx.container.exec(['danso', 'config', 'check'], env=env)
    require(row, check.returncode == 0 and json_out(check).get('valid') is True,
            'config check failed on the restored home', check)
    start_service(ctx, row, env)
    hits, done = memory_found(ctx, env)
    require(row, hits, 'the restored home does not have the memory fact', done)
    row.note = 'config valid; memory fact found; ready = status available'
    row.passed = True

    # docs/backup.md: the snapshot carries the session journals (#210, was
    # K2), so the restored chat continues its session instead of being
    # refused until /new. The step 6 prompt in the request is that history.
    row = ctx.row(8, 'restored chat: first turn continues the session',
                  'getUpdates -> turn on the restored session pointer')
    answered, sent, requests = telegram_turn(ctx, row, 'Explain this repository')
    require(row, not any(d['method'] == 'editMessageText' and TURN_NOT_STARTED in d['text'] for d in sent),
            'the restored chat was refused (`Turn could not start`): ' + bot_trace(sent))
    require(row, answered and len(replies(sent)) == 1 and len(requests) == 1,
            f'expected one answer and one provider request (saw {len(requests)}): ' + bot_trace(sent))
    require(row, 'What should I keep in mind on this node?' in json.dumps(requests[0]),
            'the restored turn did not carry the session history from the restored journal')
    service_rss(ctx, row)
    row.note = '1 answer, 1 provider request; the request carries the pre-backup history'
    row.passed = True

    row = ctx.row(8, '`/new`, then one turn on the restored home', '/new; getUpdates -> turn -> sendMessage')
    mark = len(ctx.bot.sent)
    ctx.bot.push('/new')
    require(row, wait_for(lambda: any(d['method'] == 'sendMessage' for d in ctx.bot.sent[mark:]), 30),
            '/new got no reply: ' + bot_trace(ctx.bot.sent[mark:]))
    answered, sent, requests = telegram_turn(ctx, row, 'Explain this repository')
    require(row, answered and len(replies(sent)) == 1 and len(requests) == 1,
            f'expected one answer and one provider request (saw {len(requests)}): ' + bot_trace(sent))
    service_rss(ctx, row)
    row.note = 'fresh session pointer; 1 answer, 1 provider request'
    row.passed = True


def step_crash(ctx):
    env = service_env(ctx)
    row = ctx.row(9, 'turn in flight, `kill -9` the service -> replacement available',
                  'kill -9 <service pid>; poll `danso service status` until a new pid is available')
    counts = memory_counts(ctx, env)
    mark, before = len(ctx.bot.sent), len(ctx.provider.requests)
    ctx.provider.hold_prompt = HOLD_PROMPT
    ctx.bot.push(HOLD_PROMPT)
    require(row, wait_for(lambda: ctx.provider.held, 30),
            'the turn never reached the provider: ' + bot_trace(ctx.bot.sent[mark:]))
    progress = [d for d in ctx.bot.sent[mark:] if d['method'] == 'sendMessage']
    require(row, len(progress) == 1, 'expected one progress message: ' + bot_trace(ctx.bot.sent[mark:]))
    done = ctx.container.sh(SERVICE_CRASH, ctx.state_root, env=env, timeout=60)
    measured(done, row)
    require(row, row.rc == 0, 'no replacement became available after kill -9', done)
    pids = re.search(r'killed pid=(\d+) replacement pid=(\d+)', done.stdout)
    row.note = (f'pid {pids.group(1)} -> {pids.group(2)} under the same supervisor'
                if pids else 'replacement available')
    row.passed = True

    # docs/telegram.md "Turns and persistence": a restart edits the saved
    # progress message, never replays the turn, and clears the active marker.
    row = ctx.row(9, 'orphan cleanup: restart notice, no replay, no duplicate effects',
                  'fake Bot API + fake provider observed; `danso memory check`')
    start = time.monotonic()
    noticed = wait_for(lambda: any(
        d['method'] == 'editMessageText' and d['message_id'] == progress[0]['message_id']
        and d['text'].startswith(RESTART_NOTICE) for d in ctx.bot.sent[mark:]), 15)
    row.wall_s = time.monotonic() - start
    try:
        require(row, noticed, 'the interrupted progress message did not get the restart notice: '
                + bot_trace(ctx.bot.sent[mark:]))
        time.sleep(2)  # a replay would reach the provider within this window
        replayed = len(ctx.provider.requests) - before - 1
        require(row, replayed == 0, f'the interrupted turn was replayed ({replayed} more request(s))')
        require(row, not replies(ctx.bot.sent[mark:]), 'the interrupted turn sent an answer')
        after = memory_counts(ctx, env)
        require(row, after == counts, f'memory changed across the crash: {counts} -> {after}')
    finally:
        ctx.provider.hold_prompt = None
        ctx.provider.release.set()
    row.note = (f'restart notice on the interrupted progress message; 1 provider request (no replay); '
                f'0 answers; memory records={counts[0]} unchanged')
    row.passed = True

    row = ctx.row(9, 'next turn after the crash: exactly one answer', 'getUpdates -> turn -> sendMessage')
    total = len(replies(ctx.bot.sent))
    answered, sent, requests = telegram_turn(ctx, row, 'Explain this repository')
    require(row, answered, 'no answer after the crash: ' + bot_trace(sent))
    require(row, len(replies(ctx.bot.sent)) == total + 1 and len(requests) == 1,
            f'expected one more answer and one provider request (saw {len(requests)}): ' + bot_trace(sent))
    service_rss(ctx, row)
    row.note = '1 answer, 1 provider request, same chat and session'
    row.passed = True

    row = ctx.row(9, '`danso service stop` -> supervisor exits', f'danso service stop --data-dir {ctx.state_root}')
    stop_service(ctx, row, env)
    row.note = 'Bot stop: drained; supervisor and service gone'
    row.passed = True


def step_update(ctx):
    env = service_env(ctx)
    row = ctx.row(10, '`danso update check` without a release source', 'danso update check')
    done = run_danso(ctx, row, ['update', 'check'], env=env)
    require(row, row.rc == 2 and 'source' in done.stderr,
            f'expected exit 2 (no source configured), got {row.rc}', done)
    row.note = 'exit 2, no source configured (docs/release-signing.md); nothing fetched'
    row.passed = True

    # docs/release-signing.md "Rehearsing apply and rollback offline" (doc gap
    # 10): the committed fixture is signed by a throwaway key whose private
    # half is destroyed. The embedded release key must refuse it; only a
    # scratch home that names the fixture key in `[update] public_key`
    # installs it, and rollback returns to the binary that home started with.
    ctx.container.copy_in(RELEASE_FIXTURE, FIXTURE_DIR)
    fixture_key = (RELEASE_FIXTURE / 'fixture-key.pub').read_text().splitlines()[1].strip()
    rehearsal = {'DANSO_HOME': REHEARSAL_HOME}
    apply_args = ['update', 'apply', '--artifact-dir', FIXTURE_DIR, '--artifact', FIXTURE_ARTIFACT,
                  '--data-dir', REHEARSAL_HOME, '--json']
    installed = f'{REHEARSAL_HOME}/bin/danso'

    def installed_version():
        return ctx.container.exec([installed, '--version']).stdout.strip()

    row = ctx.row(10, '`update apply`: the release key refuses the fixture',
                  f'DANSO_HOME={REHEARSAL_HOME} danso update apply --artifact-dir <fixture> '
                  f'--artifact {FIXTURE_ARTIFACT}')
    seed = ctx.container.sh('set -e; umask 077; mkdir -p "$1/bin"; install -m 0755 /usr/local/bin/danso "$1/bin/danso"',
                            REHEARSAL_HOME)
    require(row, seed.returncode == 0, 'seeding the rehearsal home failed', seed)
    serving = installed_version()
    require(row, serving.startswith('danso '), f'seeded binary did not report a version: {serving!r}')
    done = run_danso(ctx, row, apply_args, env=rehearsal)
    require(row, row.rc == EXIT_VERIFICATION_FAILED,
            f'expected exit {EXIT_VERIFICATION_FAILED} without the fixture key, got {row.rc}', done)
    require(row, installed_version() == serving, 'a refused apply changed the installed binary')
    row.note = f'exit {EXIT_VERIFICATION_FAILED}, installed binary unchanged ({serving})'
    row.passed = True

    row = ctx.row(10, '`update apply` + `update activate` with the fixture key',
                  f'[update] public_key = <fixture key> in {REHEARSAL_HOME}/config.toml; '
                  'danso update apply ...; danso update activate')
    config = ctx.container.sh('umask 077; printf \'[update]\\npublic_key = "%s"\\n\' "$2" > "$1/config.toml"',
                              REHEARSAL_HOME, fixture_key)
    require(row, config.returncode == 0, 'writing the rehearsal config failed', config)
    done = run_danso(ctx, row, apply_args, env=rehearsal)
    require(row, row.rc == 0, f'apply exited {row.rc}', done)
    require(row, installed_version() == 'danso 9.9.9', f'installed: {installed_version()!r}', done)
    activated = ctx.container.exec(['danso', 'update', 'activate', '--json'], env=rehearsal)
    require(row, activated.returncode == 0 and json_out(activated).get('result') == 'activated',
            'activate did not report activated', activated)
    row.note = 'exit 0; `bin/danso --version` = danso 9.9.9; activate: activated'
    row.passed = True

    row = ctx.row(10, '`update rollback` -> the seeded binary is back',
                  f'DANSO_HOME={REHEARSAL_HOME} danso update rollback; danso update activate')
    done = run_danso(ctx, row, ['update', 'rollback', '--json'], env=rehearsal)
    require(row, row.rc == 0, f'rollback exited {row.rc}', done)
    require(row, installed_version() == serving, f'after rollback: {installed_version()!r}', done)
    same = ctx.container.sh('cmp -s /usr/local/bin/danso "$1"', installed)
    require(row, same.returncode == 0, 'the rolled-back binary differs from the seeded one', same)
    activated = ctx.container.exec(['danso', 'update', 'activate', '--json'], env=rehearsal)
    require(row, activated.returncode == 0, 'activate after rollback failed', activated)
    row.note = f'exit 0; byte-identical to the seeded binary ({serving}); activate exit 0'
    row.passed = True


STEPS = [step_preflight, step_install, step_provider, step_service,
         step_telegram, step_memory, step_cron, step_backup, step_crash, step_update]


class Context:
    def __init__(self, container, provider, bot):
        self.container, self.provider, self.bot = container, provider, bot
        self.rows = []
        self.env = {}
        self.service_started = False
        # Where the node lives; step 8 moves it to the restored target.
        self.home_env = {}
        self.state_root = STATE_ROOT

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
        result = (row.result or 'pass') if row.passed else 'FAIL'
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
        provider.release.set()  # a turn held for step 9 must not outlive the run
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
             'pending': PENDING, 'known_gaps': KNOWN_GAPS, 'error': error}, indent=2) + '\n')
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
        assert poll_param('offset=7&timeout=1', b'', 'offset', None) == 7.0
        assert poll_param('', b'', 'offset', None) is None

        # Bot API: a pushed update is offered until a later offset confirms it.
        json_headers = {'Content-Type': 'application/json'}
        update_id = bot.push('hello')
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/getUpdates', {'timeout': 0}, json_headers)
        offered = json.loads(body)['result']
        assert [u['update_id'] for u in offered] == [update_id]
        assert offered[0]['message']['chat']['id'] == CHAT_ID == offered[0]['message']['from']['id']
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/getUpdates', {'offset': update_id, 'timeout': 0}, json_headers)
        assert [u['update_id'] for u in json.loads(body)['result']] == [update_id], 'unconfirmed is re-offered'
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/getUpdates', {'offset': update_id + 1, 'timeout': 0},
                       json_headers)
        assert json.loads(body)['result'] == [] and not bot.updates, 'a later offset confirms it'
        # sendMessage/editMessageText answer with a Message and are recorded.
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/sendMessage', {'chat_id': CHAT_ID, 'text': 'working'},
                       json_headers)
        message = json.loads(body)['result']
        assert message['chat']['id'] == CHAT_ID and message['message_id'] > 0
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/editMessageText',
                       {'chat_id': CHAT_ID, 'message_id': message['message_id'], 'text': 'done'}, json_headers)
        assert json.loads(body)['result']['message_id'] == message['message_id']
        _, body = post(f'{bot.url}/bot{BOT_TOKEN}/sendMessage', {'chat_id': CHAT_ID, 'text': REPLY_TEXT},
                       json_headers)
        assert [(d['method'], d['text']) for d in bot.sent] == [
            ('sendMessage', 'working'), ('editMessageText', 'done'), ('sendMessage', REPLY_TEXT)]
        assert len(replies(bot.sent)) == 1 and 'editMessageText' in bot_trace(bot.sent)

        # Provider: a request whose latest prompt is held waits for release.
        provider.hold_prompt = HOLD_PROMPT
        held = {}
        history = {'role': 'user', 'content': HOLD_PROMPT}
        worker = threading.Thread(target=lambda: held.update(answer=post(
            provider.url + '/v1/messages',
            {'messages': [history, {'role': 'user', 'content': [{'type': 'text', 'text': HOLD_PROMPT}]}]},
            {'x-api-key': PROVIDER_KEY, 'Content-Type': 'application/json'})))
        worker.start()
        assert wait_for(lambda: provider.held == 1, 5) and 'answer' not in held, 'held until released'
        status, body = post(provider.url + '/v1/messages', {'messages': [history, {'role': 'user', 'content': 'next'}]},
                            {'x-api-key': PROVIDER_KEY, 'Content-Type': 'application/json'})
        assert status == 200 and provider.held == 1, 'only the newest prompt decides the hold'
        provider.release.set()
        worker.join(5)
        assert held['answer'][0] == 200
    finally:
        provider.release.set()
        shutdown(provider, bot)
    assert last_user_text({'messages': [{'role': 'user', 'content': 'a'},
                                        {'role': 'user', 'content': [{'type': 'text', 'text': 'b'}]}]}) == 'b'
    assert last_user_text({}) == '' and last_user_text({'messages': [{'role': 'user', 'content': []}]}) == ''

    row = Row(1, 'x', 'y')
    done = subprocess.CompletedProcess([], 0, '', 'noise\nFRESHENV rc=0 ns=1500000000 rss_kb=2048\n')
    measured(done, row)
    assert (row.rc, row.wall_s, row.rss_kb) == (0, 1.5, 2048)
    gap = Row(6, 'g', 'c')
    known_gap(gap, 'K1', True, 'unused')
    assert gap.passed and gap.result == 'known gap K1'
    try:
        known_gap(Row(6, 'g', 'c'), 'K2', False, 'it was fixed')
    except Fail as caught:
        assert 'known gap K2 no longer reproduces (it was fixed)' in str(caught)
    else:
        raise AssertionError('a known gap that stops reproducing must fail the run')
    table = render_table([row, gap])
    assert '| 1 | x | 0 | 1.500 |  | 2.0 MiB | FAIL |' in table
    assert '| 6 | g |  |  |  |  | known gap K1 |' in table
    assert not PENDING and 'pending part 2' not in table
    assert KNOWN_GAPS == {}, 'K1/K2 are fixed; a new gap needs its docs row too'
    assert (RELEASE_FIXTURE / FIXTURE_ARTIFACT).is_file()
    assert (RELEASE_FIXTURE / 'fixture-key.pub').read_text().splitlines()[1].startswith('RW')
    assert [s.__name__ for s in STEPS] == ['step_preflight', 'step_install', 'step_provider',
                                           'step_service', 'step_telegram', 'step_memory',
                                           'step_cron', 'step_backup', 'step_crash', 'step_update']
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
