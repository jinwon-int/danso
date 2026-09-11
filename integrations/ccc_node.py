"""Opt-in ccc-node AgentRuntime adapter for bounded Danso tasks."""
import asyncio
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import signal
import stat
import uuid

from telegram_bot.core.agent_runtime import (
    CompletionEvent, ErrorEvent, MessageCompletedEvent, ModelInfo,
    ResultEvent, TaskProgressEvent, TextDeltaEvent, deny_approval,
)

CAP = 1024 * 1024
# A checkpoint may be emitted at stage start and after each request, with a
# terminal record as well.  Keep a finite margin above the native 1024-request
# cumulative limit without buffering arbitrary stderr.
TASK_PROGRESS_CAP = 4096
TASK_PROGRESS_LINE_CAP = 64 * 1024
TASK_RESUME_CONTROL = "__CCC_DANSO_TASK_RESUME_V1__"
PROVIDERS = {
    'glm': ('ZAI_API_KEY', 'DANSO_GLM_BASE_URL'),
    'openai': ('OPENAI_API_KEY', 'DANSO_OPENAI_BASE_URL'),
    'openai-codex': ('DANSO_CHATGPT_AUTH_FILE', 'DANSO_CHATGPT_BASE_URL'),
    'anthropic': ('ANTHROPIC_API_KEY', 'DANSO_ANTHROPIC_BASE_URL'),
}
EFFORTS = ('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')


async def _read(stream):
    data = bytearray()
    while chunk := await stream.read(65536):
        if len(data) + len(chunk) > CAP:
            raise ValueError('output limit')
        data.extend(chunk)
    return bytes(data)


async def _read_limited(stream, limit):
    data = bytearray()
    while chunk := await stream.read(min(8192, limit + 1)):
        if len(data) + len(chunk) > limit:
            raise ValueError('output limit')
        data.extend(chunk)
    return bytes(data)


async def _stop(process):
    # Signal the owned process group even if the group leader has already exited.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        if sig == signal.SIGTERM:
            await asyncio.sleep(0.2)
    await process.wait()


async def _wait_owned(task):
    """Drain owned work despite repeated caller cancellation, then report it."""
    cancelled = False
    while True:
        try:
            return await asyncio.shield(task), cancelled
        except asyncio.CancelledError:
            if task.cancelled():
                raise
            cancelled = True


def _usage(stderr, *, allow_zero=False):
    groups = [[line.split('=', 1)[1] for line in stderr.splitlines()
               if line.startswith(prefix + '=')] for prefix in ('DANSO_USAGE', 'PIRI_USAGE')]
    if any(len(group) != 1 for group in groups):
        raise ValueError('usage missing')
    a, b = (json.loads(group[0]) for group in groups)
    if not isinstance(a, dict) or a != b:
        raise ValueError('usage mismatch')
    keys = ('requests', 'inputTokens', 'outputTokens', 'cacheReadTokens', 'cacheWriteTokens', 'totalTokens')
    if any(type(a.get(k)) is not int or not 0 <= a[k] <= 2**64 - 1 for k in keys):
        raise ValueError('invalid usage')
    if (a['requests'] < 1 and not allow_zero) or a['totalTokens'] != sum(a[k] for k in keys[1:-1]):
        raise ValueError('invalid usage totals')
    if a['requests'] == 0 and a['totalTokens'] != 0:
        raise ValueError('invalid zero-request usage')
    # Do not relay arbitrary fields, model strings, or the zero cost placeholder.
    return {k: a[k] for k in keys}


FAILURE_CATEGORIES = {
    'configuration', 'session', 'sandbox', 'provider', 'provider_timeout',
    'compaction', 'request_budget', 'output', 'runtime', 'run_timeout', 'interrupted',
}
TRANSPORT_PHASES = {'connect', 'before_response_headers', 'response_body'}
TRANSPORT_KEYS = {'version', 'phase', 'elapsed_ms', 'request_bytes', 'attempts'}
PROVIDER_REASONS = {
    'http_status', 'invalid_json', 'response_too_large', 'stream_ended',
    'invalid_stream', 'unsupported_stream_event', 'response_failed',
    'response_incomplete', 'response_error', 'max_tokens',
}
PROVIDER_KEYS = {'version', 'reason', 'http_status', 'output_tokens_max'}
TASK_PROGRESS_STATES = {'checkpoint', 'paused', 'completed', 'blocked'}
TASK_PROGRESS_KEYS = {
    'version', 'state', 'stage', 'requests', 'reported_tokens', 'elapsed_seconds',
}
TASK_STATUS_STATES = {
    'ready', 'pending_provider', 'pending_tools', 'final_pending',
    'paused', 'completed', 'failed', 'not_long_task',
}
TASK_STATUS_KEYS = {
    'version', 'kind', 'state', 'session_id', 'stage', 'elapsed_ms',
    'limits', 'usage', 'pending', 'resume_allowed',
}
TASK_STATUS_LIMIT_KEYS = {
    'wall_seconds', 'stage_requests', 'max_requests', 'max_tokens', 'repeat_limit',
}
TASK_STATUS_USAGE_KEYS = {'requests', 'reported_tokens'}


@dataclass(frozen=True)
class _TaskStatus:
    state: str
    stage: int
    wall_seconds: int
    stage_requests: int
    max_requests: int
    max_tokens: int
    repeat_limit: int
    elapsed_ms: int
    requests: int
    reported_tokens: int
    resume_allowed: bool
    unknown_usage_requests: int = 0
    interruption_reason: str | None = None


def _task_status(data):  # noqa: C901 -- strict nested protocol validation
    """Parse the provider-free native status projection without relaying data."""
    if (type(data) is not dict or set(data) not in (TASK_STATUS_KEYS, TASK_STATUS_KEYS | {'recovery'})
            or type(data.get('version')) is not int or data['version'] != 1
            or data.get('kind') != 'long_task_status'
            or type(data.get('session_id')) is not str
            or not isinstance(data.get('state'), str)
            or data['state'] not in TASK_STATUS_STATES
            or type(data.get('stage')) is not int
            or not 0 <= data['stage'] <= 2**64 - 1
            or type(data.get('elapsed_ms')) is not int
            or not 0 <= data['elapsed_ms'] <= 2**64 - 1
            or type(data.get('resume_allowed')) is not bool):
        raise ValueError('invalid task status')
    try:
        if str(uuid.UUID(data['session_id'])) != data['session_id']:
            raise ValueError('invalid task status session id')
    except (ValueError, AttributeError, TypeError) as exc:
        raise ValueError('invalid task status session id') from exc
    limits = data['limits']
    usage = data['usage']
    if (type(limits) is not dict or set(limits) != TASK_STATUS_LIMIT_KEYS
            or type(usage) is not dict or set(usage) != (TASK_STATUS_USAGE_KEYS | ({'unknown_usage_requests'} if 'recovery' in data else set()))):
        raise ValueError('invalid task status')
    values = {}
    for key, maximum in (
        ('wall_seconds', 21600), ('stage_requests', 1024),
        ('max_requests', 2048), ('max_tokens', 25_000_000), ('repeat_limit', 8),
    ):
        value = limits[key]
        if type(value) is not int or not 1 <= value <= maximum:
            raise ValueError('invalid task status limits')
        values[key] = value
    usage_values = {}
    for key, maximum in (('requests', values['max_requests']),
                         ('reported_tokens', 2**64 - 1)):
        value = usage[key]
        if type(value) is not int or not 0 <= value <= maximum:
            raise ValueError('invalid task status usage')
        usage_values[key] = value
    unknown = 0
    reason = None
    if 'recovery' in data:
        recovery = data['recovery']
        unknown = usage['unknown_usage_requests']
        if (type(recovery) is not dict
                or set(recovery) != {'interruption_reason', 'interrupted_requests',
                                     'max_interrupted_requests', 'automatic_resume_allowed'}
                or type(unknown) is not int or not 0 <= unknown <= 3
                or unknown > usage_values['requests']
                or type(recovery['interrupted_requests']) is not int
                or recovery['interrupted_requests'] != unknown
                or type(recovery['max_interrupted_requests']) is not int
                or recovery['max_interrupted_requests'] != 3
                or recovery['automatic_resume_allowed'] is not False):
            raise ValueError('invalid task recovery assessment')
        reason = recovery['interruption_reason']
        if (reason is not None and (type(reason) is not str or reason not in
                {'user_stop', 'signal_termination', 'run_deadline', 'unknown'})):
            raise ValueError('invalid task interruption reason')
        if (unknown == 0) != (reason is None) or (unknown == 3 and data['resume_allowed']):
            raise ValueError('inconsistent task recovery assessment')
    pending = data['pending']
    if pending is not None:
        if type(pending) is not dict:
            raise ValueError('invalid task status pending')
        if set(pending) == {'kind'}:
            if pending['kind'] != 'tools':
                raise ValueError('invalid task status pending')
        elif set(pending) == {'kind', 'sequence'}:
            if (pending['kind'] != 'provider'
                    or type(pending['sequence']) is not int
                    or not 0 <= pending['sequence'] <= 2**64 - 1):
                raise ValueError('invalid task status pending')
        else:
            raise ValueError('invalid task status pending')
    if data['resume_allowed'] and (
            data['state'] not in {'ready', 'paused'} or pending is not None
            or data['elapsed_ms'] >= values['wall_seconds'] * 1000
            or usage_values['requests'] >= values['max_requests']
            or usage_values['reported_tokens'] >= values['max_tokens']):
        raise ValueError('inconsistent task resume assessment')
    return _TaskStatus(
        state=data['state'], stage=data['stage'], elapsed_ms=data['elapsed_ms'],
        resume_allowed=data['resume_allowed'], unknown_usage_requests=unknown,
        interruption_reason=reason, **values, **usage_values,
    )


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate diagnostic key')
        result[key] = value
    return result


def _provider_detail(text, category, code):
    """Accept only one native, bounded diagnostic; never relay provider text."""
    if category != 'provider' or code != 3:
        return ''
    lines = [line[len('DANSO_PROVIDER='):] for line in text.splitlines()
             if line.startswith('DANSO_PROVIDER=')]
    if len(lines) != 1:
        return ''
    try:
        value = json.loads(lines[0], object_pairs_hook=_unique_object)
        if (type(value) is not dict or set(value) != PROVIDER_KEYS
                or type(value['version']) is not int or value['version'] != 1
                or type(value['reason']) is not str or value['reason'] not in PROVIDER_REASONS):
            return ''
        status = value['http_status']
        cap = value['output_tokens_max']
        if cap is not None and (type(cap) is not int or cap < 0):
            return ''
        if value['reason'] == 'http_status':
            # reqwest StatusCode accepts all three-digit codes, including extensions.
            if type(status) is not int or not 100 <= status <= 999 or 200 <= status <= 299:
                return ''
            if cap is not None:
                return ''
            return f", reason=http_status, http_status={status}"
        if value['reason'] == 'max_tokens':
            # Output-token cap stop (issue #69 B): a positive cap rides with
            # the reason; without one the record is not relayed.
            if status is not None or type(cap) is not int or cap <= 0:
                return ''
            return f", reason=max_tokens, output_tokens_max={cap}"
        if status is not None or cap is not None:
            return ''
        return f", reason={value['reason']}"
    except (ValueError, TypeError, RecursionError):
        return ''


def _transport(text, category, code):
    """Return trusted native transport facts, or ignore the optional record."""
    if category not in {'provider', 'provider_timeout'}:
        return None
    # A native HTTP transport failure has already marked the request attempted,
    # so both provider categories must carry the normal attempted exit code.
    if code != 3:
        return None
    lines = [line[len('DANSO_TRANSPORT='):] for line in text.splitlines()
             if line.startswith('DANSO_TRANSPORT=')]
    if len(lines) != 1:
        return None
    try:
        diagnostic = json.loads(lines[0], object_pairs_hook=_unique_object)
        if (type(diagnostic) is not dict or set(diagnostic) != TRANSPORT_KEYS
                or type(diagnostic['version']) is not int or diagnostic['version'] != 1
                or not isinstance(diagnostic['phase'], str)
                or diagnostic['phase'] not in TRANSPORT_PHASES
                or type(diagnostic['elapsed_ms']) is not int
                or not 0 <= diagnostic['elapsed_ms'] <= 2**64 - 1
                or type(diagnostic['request_bytes']) is not int
                or not 0 <= diagnostic['request_bytes'] <= 512 * 1024
                or type(diagnostic['attempts']) is not int
                or not 1 <= diagnostic['attempts'] <= 8):
            raise ValueError('invalid transport diagnostic')
        return (diagnostic['phase'], diagnostic['elapsed_ms'], diagnostic['request_bytes'],
                diagnostic['attempts'])
    except (ValueError, TypeError, RecursionError):
        return None


def _task_progress(line):
    """Parse one strict, body-free native long-task progress record."""
    if not line.startswith('DANSO_TASK='):
        return None
    try:
        diagnostic = json.loads(line[len('DANSO_TASK='):], object_pairs_hook=_unique_object)
        if (type(diagnostic) is not dict or set(diagnostic) != TASK_PROGRESS_KEYS
                or type(diagnostic['version']) is not int or diagnostic['version'] != 1
                or not isinstance(diagnostic['state'], str)
                or diagnostic['state'] not in TASK_PROGRESS_STATES):
            raise ValueError('invalid task progress')
        values = {}
        for key in ('stage', 'requests', 'reported_tokens', 'elapsed_seconds'):
            value = diagnostic[key]
            if type(value) is not int or not 0 <= value <= 2**64 - 1:
                raise ValueError('invalid task progress')
            values[key] = value
        return TaskProgressEvent(state=diagnostic['state'], **values)
    except (ValueError, TypeError, RecursionError):
        return None


async def _read_stderr(stream, progress_queue, *, parse_progress):
    """Retain only bounded diagnostics while yielding native progress records."""
    retained = bytearray()
    pending = bytearray()
    progress_count = 0

    async def consume(line):
        nonlocal progress_count
        if parse_progress and line.startswith(b'DANSO_TASK='):
            if len(line) > TASK_PROGRESS_LINE_CAP:
                # Reserved protocol lines are never allowed to bypass the
                # adapter's bounded stderr contract by being discarded as
                # malformed progress.  Fail closed so the owned process is
                # reaped immediately.
                raise ValueError('stderr line limit')
            if len(retained) + len(line) + 1 > CAP:
                # Malformed reserved records are still untrusted stderr bytes;
                # counting them prevents an arbitrary stream of records from
                # bypassing the retained-output bound.
                raise ValueError('output limit')
            retained.extend(line)
            retained.extend(b'\n')
            if progress_count < TASK_PROGRESS_CAP:
                event = _task_progress(line.decode('utf-8', errors='replace'))
                if event is not None:
                    progress_count += 1
                    await progress_queue.put(event)
            return
        if len(retained) + len(line) + 1 > CAP:
            raise ValueError('output limit')
        retained.extend(line)
        retained.extend(b'\n')

    try:
        while chunk := await stream.read(65536):
            pending.extend(chunk)
            if len(pending) > TASK_PROGRESS_LINE_CAP and b'\n' not in pending:
                raise ValueError('stderr line limit')
            while b'\n' in pending:
                line, _, pending = pending.partition(b'\n')
                if line.endswith(b'\r'):
                    line = line[:-1]
                await consume(line)
                if len(pending) > TASK_PROGRESS_LINE_CAP and b'\n' not in pending:
                    raise ValueError('stderr line limit')
        if pending:
            await consume(bytes(pending))
        return bytes(retained)
    finally:
        await progress_queue.put(None)


def _failure(stderr, code):
    # Native enums only: error text, URLs and provider response bodies are never
    # used to guess a category or included in the event.
    text = stderr.decode('utf-8', errors='replace')
    lines = [line[len('DANSO_ERROR='):] for line in text.splitlines()
             if line.startswith('DANSO_ERROR=')]
    category = 'run_timeout' if code == 124 else 'failed'
    if len(lines) == 1:
        try:
            diagnostic = json.loads(lines[0], object_pairs_hook=_unique_object)
            if (type(diagnostic) is not dict or set(diagnostic) != {'version', 'category', 'exit_code'}
                    or type(diagnostic['version']) is not int or diagnostic['version'] != 1
                    or type(diagnostic['exit_code']) is not int or diagnostic['exit_code'] != code
                    or not isinstance(diagnostic['category'], str)
                    or diagnostic['category'] not in FAILURE_CATEGORIES):
                raise ValueError('invalid diagnostic')
            candidate = diagnostic['category']
            expected = (124,) if candidate == 'run_timeout' else (
                (129, 130, 143) if candidate == 'interrupted' else (2, 3))
            if code not in expected:
                raise ValueError('inconsistent diagnostic')
            category = candidate
        except (ValueError, TypeError, RecursionError):
            pass
    counts = ''
    try:
        usage = _usage(text, allow_zero=True)
        counts = f", reported_requests={usage['requests']}, reported_tokens={usage['totalTokens']}"
    except (ValueError, TypeError, RecursionError):
        pass
    transport = _transport(text, category, code)
    transport_detail = '' if transport is None else (
        f', phase={transport[0]}, elapsed_ms={transport[1]}, request_bytes={transport[2]}, '
        f'attempts={transport[3]}')
    provider_detail = _provider_detail(text, category, code)
    label = 'timeout' if category == 'run_timeout' else category
    return ErrorEvent(code='danso_' + label, message=(
        f'Worker failed: category={category}, exit_code={code}{counts}{transport_detail}{provider_detail}. '
        'Reported usage may omit failed requests; not a total attempt count. No automatic replay.'))


class DansoRuntime:
    """One configured model; explicit credentials and private journal directory."""
    def __init__(self, *, binary, state_directory, provider, model, environment,
                 timeout_seconds=300, provider_timeout_seconds=180, max_turns=16,
                 compact_at_bytes=None, sandbox="host", system_context_loader=None,
                 outer_timeout_seconds=None,
                 tool_home=None,
                 long_task=False, task_stage_requests=16, task_max_requests=1024,
                 task_max_tokens=10_000_000, task_repeat_limit=3,
                 task_pause_after_stage=None):
        if provider not in PROVIDERS or not model or not isinstance(model, str):
            raise ValueError('invalid provider/model')
        if type(long_task) is not bool:
            raise ValueError('invalid long-task setting')
        if (outer_timeout_seconds is not None
                and (type(outer_timeout_seconds) not in (int, float)
                     or not math.isfinite(outer_timeout_seconds)
                     or outer_timeout_seconds <= 0)):
            raise ValueError('invalid outer timeout')
        timeout_maximum = 21600 if long_task else 3600
        for value, maximum in ((timeout_seconds, timeout_maximum), (provider_timeout_seconds, 300), (max_turns, 128),
                               (task_stage_requests, 1024), (task_max_requests, 2048),
                               (task_max_tokens, 25_000_000), (task_repeat_limit, 8)):
            if type(value) is not int or not 1 <= value <= maximum:
                raise ValueError('invalid worker limit')
        if task_stage_requests > task_max_requests:
            raise ValueError('invalid task request budgets')
        if task_pause_after_stage is not None and (
                type(task_pause_after_stage) is not int or not 1 <= task_pause_after_stage <= 2048):
            raise ValueError('invalid task pause point')
        if task_pause_after_stage is not None and task_pause_after_stage > task_max_requests:
            raise ValueError('invalid task pause point')
        if compact_at_bytes is not None and (type(compact_at_bytes) is not int
                or not 8192 <= compact_at_bytes <= 393216):
            raise ValueError('invalid compaction threshold')
        if sandbox not in {"host", "bubblewrap"}:
            raise ValueError("invalid execution backend")
        if tool_home is not None:
            if sandbox != "host":
                raise ValueError("tool HOME requires the host execution backend")
            raw_tool_home = str(tool_home)
            if not isinstance(tool_home, (str, Path)) or not Path(raw_tool_home).is_absolute():
                raise ValueError("tool HOME must be an absolute path")
            if "\x00" in raw_tool_home or os.pathsep in raw_tool_home:
                raise ValueError("tool HOME contains an invalid PATH component")
        self.system_context_loader = system_context_loader
        self.sandbox = sandbox
        self.tool_home = None if tool_home is None else str(tool_home)
        self.compact_at_bytes = compact_at_bytes
        self.binary = str(Path(binary).resolve(strict=True))
        root = Path(state_directory).absolute()
        if root.resolve() != root:
            raise ValueError('state path must not contain symlinks')
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        st = root.lstat()
        if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or stat.S_IMODE(st.st_mode) != 0o700:
            raise ValueError('state directory must be private and owner-controlled')
        self.root, self.provider, self.model = root, provider, model
        names = ('PATH', 'HOME', *PROVIDERS[provider])
        self.environment = {k: environment[k] for k in names if k in environment}
        if not self.environment.get('HOME') or not self.environment.get(PROVIDERS[provider][0]):
            raise ValueError('explicit HOME and provider credential required')
        self.timeout, self.provider_timeout, self.max_turns = timeout_seconds, provider_timeout_seconds, max_turns
        self.outer_timeout = outer_timeout_seconds
        self.long_task = long_task
        self.task_stage_requests = task_stage_requests
        self.task_max_requests = task_max_requests
        self.task_max_tokens = task_max_tokens
        self.task_repeat_limit = task_repeat_limit
        self.task_pause_after_stage = task_pause_after_stage

    async def list_models(self):
        return [ModelInfo(id=self.model, display_name=self.model, is_default=True,
                          supported_reasoning_efforts=() if self.provider == 'anthropic' else EFFORTS)]

    async def start_or_resume(self, request):
        if (request.memory_environment is not None or request.sandbox_policy is not None
                or request.approvals_reviewer is not None or request.approval_policy not in (None, 'never')):
            raise ValueError('unsupported worker policy or memory route')
        if request.model not in (None, self.model) or (request.effort is not None and
                (self.provider == 'anthropic' or request.effort not in EFFORTS)):
            raise ValueError('unsupported worker model/effort')
        cwd = Path(request.working_directory).resolve(strict=True)
        if not cwd.is_dir() or self.root.is_relative_to(cwd):
            raise ValueError('journal directory must be outside workspace')
        ident = request.session_id or str(uuid.uuid4())
        if str(uuid.UUID(ident)) != ident:
            raise ValueError('invalid session id')
        journal = self.root / (ident + '.jsonl')
        if request.session_id:
            st = journal.lstat()
            if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or stat.S_IMODE(st.st_mode) != 0o600:
                raise ValueError('invalid stored session')
        return DansoSession(self, ident, cwd, request.effort)


class DansoSession:
    def __init__(self, runtime, ident, cwd, effort):
        self.runtime, self.session_id, self.cwd, self.effort = runtime, ident, cwd, effort
        self._lock = asyncio.Lock()
        self._process = None
        self._active = False
        self._stop_task = None
        self._interrupted = False
        self._bootstrap_task = None
        self._resume_task_authorized = False
        self._task_progress_ready = False
        self._task_progress_seen = False
        self._task_pause_requested = False

    def authorize_task_resume(self):
        """Arm exactly one bridge-authorized no-prompt resume dispatch."""
        self._resume_task_authorized = True

    def clear_task_resume_authorization(self):
        self._resume_task_authorized = False

    def request_task_pause(self) -> bool:
        """Ask only the native parent to pause at its next settled boundary."""
        if (
            not self.runtime.long_task
            or not self._active
            or not self._task_progress_ready
            or self._task_pause_requested
            or self._process is None
            or self._process.returncode is not None
        ):
            return False
        try:
            os.kill(self._process.pid, signal.SIGUSR1)
        except ProcessLookupError:
            return False
        self._task_pause_requested = True
        return True

    async def interrupt(self):
        if self._active:
            self._interrupted = True
            if self._bootstrap_task is not None:
                self._bootstrap_task.cancel()
                await asyncio.gather(self._bootstrap_task, return_exceptions=True)
            if self._process is not None:
                await self._terminate()

    async def _terminate(self):
        if self._stop_task is None:
            self._stop_task = asyncio.create_task(_stop(self._process))
        await asyncio.shield(self._stop_task)

    async def _read_task_status(self):
        """Read the saved native task state without credentials or mutation."""
        r = self.runtime
        journal = r.root / (self.session_id + '.jsonl')
        command = [r.binary, '--task-status', '--session', str(journal)]
        spawn = asyncio.create_task(asyncio.create_subprocess_exec(
            *command, cwd=self.cwd,
            env={'PATH': os.defpath, 'HOME': str(Path.home())},
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            start_new_session=True,
        ))
        process, cancelled = await _wait_owned(spawn)
        if cancelled:
            stop_task = asyncio.create_task(_stop(process))
            await _wait_owned(stop_task)
            raise asyncio.CancelledError
        stdout_task = asyncio.create_task(_read_limited(process.stdout, 64 * 1024))
        stderr_task = asyncio.create_task(_read_limited(process.stderr, 64 * 1024))
        try:
            async with asyncio.timeout(2):
                stdout, stderr = await asyncio.gather(stdout_task, stderr_task)
                code = await process.wait()
            stop_task = asyncio.create_task(_stop(process))
            _, stop_cancelled = await _wait_owned(stop_task)
            if stop_cancelled:
                raise asyncio.CancelledError
        except BaseException:
            stop_task = asyncio.create_task(_stop(process))
            _, stop_cancelled = await _wait_owned(stop_task)
            for task in (stdout_task, stderr_task):
                task.cancel()
            await asyncio.gather(stdout_task, stderr_task, return_exceptions=True)
            if stop_cancelled:
                raise asyncio.CancelledError
            raise
        if code != 0 or stderr.strip() or len(stdout) > 64 * 1024:
            raise ValueError('task status unavailable')
        try:
            data = json.loads(stdout.decode('utf-8'), object_pairs_hook=_unique_object)
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError, RecursionError) as exc:
            raise ValueError('invalid task status') from exc
        return _task_status(data)

    async def send_turn(self, message, *, approval_handler=deny_approval):  # noqa: C901 -- subprocess lifecycle and terminal event mapping
        async with self._lock:
            if not isinstance(message, str) or not message.strip() or len(message.encode()) > 65536:
                yield ErrorEvent(code='danso_input', message='Invalid worker input.')
                return
            r = self.runtime
            resume_task = message == TASK_RESUME_CONTROL and self._resume_task_authorized
            self._resume_task_authorized = False
            if message == TASK_RESUME_CONTROL and not resume_task:
                yield ErrorEvent(code='danso_input', message='Invalid worker input.')
                return
            if resume_task and not r.long_task:
                yield ErrorEvent(code='danso_task_resume_disabled', message='Long-task resume is disabled.')
                return
            self._active, self._interrupted = True, False
            self._task_progress_ready = False
            self._task_progress_seen = False
            self._task_pause_requested = False
            readers, events = [], []
            self._stop_task = None
            effective_timeout = r.timeout
            effective_wall = r.timeout
            effective_max_requests = r.task_max_requests
            effective_max_tokens = r.task_max_tokens
            resume_stage = None
            if resume_task:
                status_task = asyncio.create_task(self._read_task_status())
                self._bootstrap_task = status_task
                try:
                    status = await status_task
                except asyncio.CancelledError:
                    if not self._interrupted:
                        self._active = False
                        self._bootstrap_task = None
                        raise
                    events.append(ErrorEvent(
                        code='danso_cancelled',
                        message='Worker interrupted before dispatch.',
                    ))
                    status = None
                except (OSError, asyncio.TimeoutError, ValueError):
                    if self._interrupted:
                        events.append(ErrorEvent(
                            code='danso_cancelled',
                            message='Worker interrupted before dispatch.',
                        ))
                    else:
                        events.append(ErrorEvent(
                            code='danso_task_resume_unavailable',
                            message=(
                                'Saved Danso task cannot be resumed safely; its checkpoint '
                                'or remaining deadline is unavailable.'
                            ),
                        ))
                    status = None
                finally:
                    self._bootstrap_task = None
                if status is None:
                    self._active = False
                    self._task_progress_ready = False
                    self._task_progress_seen = False
                    self._task_pause_requested = False
                    for event in events:
                        yield event
                    return
                try:
                    if status.state not in {'ready', 'paused'} or not status.resume_allowed:
                        raise ValueError('task is not resumable')
                    remaining = status.wall_seconds - (status.elapsed_ms / 1000)
                    if remaining <= 0:
                        raise ValueError('task has no remaining wall time')
                    if (r.outer_timeout is not None
                            and r.outer_timeout < remaining + 10):
                        raise ValueError('outer deadline is too short for saved task')
                except (OSError, asyncio.TimeoutError, ValueError):
                    self._active = False
                    events.append(ErrorEvent(
                        code='danso_task_resume_unavailable',
                        message=(
                            'Saved Danso task cannot be resumed safely; its checkpoint '
                            'or remaining deadline is unavailable.'
                        ),
                    ))
                    for event in events:
                        yield event
                    return
                effective_timeout = remaining
                effective_wall = status.wall_seconds
                effective_max_requests = status.max_requests
                effective_max_tokens = status.max_tokens
                resume_stage = status.stage
            # The bridge owns the replay policy ("No automatic replay"), so the
            # native wire-level retry budget stays off for this lane.
            command = [r.binary, '--sandbox', r.sandbox, '--cwd', str(self.cwd), '--session', str(r.root / (self.session_id + '.jsonl')),
                       '--provider', r.provider, '--model', r.model, '--max-turns', str(r.max_turns),
                       '--provider-retries', '0',
                       '--provider-timeout-seconds', str(r.provider_timeout), '-p']
            if r.tool_home is not None:
                command += ['--tool-home', r.tool_home]
            if not resume_task:
                command += ['--timeout-seconds', str(r.timeout)]
            if r.long_task:
                command += [
                    '--long-task',
                    '--task-progress',
                ]
                if not resume_task:
                    command += [
                        '--task-stage-requests', str(r.task_stage_requests),
                        '--task-max-requests', str(r.task_max_requests),
                        '--task-max-tokens', str(r.task_max_tokens),
                        '--task-repeat-limit', str(r.task_repeat_limit),
                    ]
                if not resume_task and r.task_pause_after_stage is not None:
                    command += ['--task-pause-after-stage', str(r.task_pause_after_stage)]
            if self.effort is not None:
                command += ['--reasoning-effort', self.effort]
            if r.compact_at_bytes is not None:
                command += ['--compact-at-bytes', str(r.compact_at_bytes)]
            try:
                if r.system_context_loader is not None:
                    self._bootstrap_task = asyncio.create_task(r.system_context_loader())
                    context_file = await self._bootstrap_task
                    self._bootstrap_task = None
                    command += ['--system-context-file', str(context_file)]
                if self._interrupted:
                    events.append(ErrorEvent(code='danso_cancelled', message='Worker interrupted before dispatch.'))
                else:
                    if resume_task:
                        command += ['--resume-task']
                    else:
                        command += ['--', message]
                    async for event in self._execute(
                        command, readers, resume_task=resume_task,
                        resume_stage=resume_stage,
                        timeout_seconds=effective_timeout,
                        wall_seconds=effective_wall,
                        max_requests=effective_max_requests,
                        max_tokens=effective_max_tokens,
                    ):
                        if isinstance(event, TaskProgressEvent):
                            # Progress is consumed incrementally and never
                            # retained alongside the final answer.
                            yield event
                        else:
                            events.append(event)
            except asyncio.CancelledError:
                if not self._interrupted or asyncio.current_task().cancelling():
                    raise
                events.append(ErrorEvent(code='danso_cancelled', message='Worker interrupted before dispatch.'))
            except asyncio.TimeoutError:
                events.append(ErrorEvent(code='danso_timeout', message='Worker deadline exceeded; journal retained.'))
            except (OSError, ValueError):
                events.append(ErrorEvent(code='danso_adapter_error', message='Worker output or startup failed validation.'))
            finally:
                _, cancelled = await _wait_owned(asyncio.create_task(self._cleanup(readers)))
                if cancelled:
                    raise asyncio.CancelledError

            for event in events:
                yield event

    async def _execute(self, command, readers, *, resume_task=False,  # noqa: C901 -- bounded concurrent stdout/stderr/progress lifecycle
                       resume_stage=None,
                       timeout_seconds=None, wall_seconds=None,
                       max_requests=None, max_tokens=None):
        r = self.runtime
        timeout_seconds = r.timeout if timeout_seconds is None else timeout_seconds
        wall_seconds = r.timeout if wall_seconds is None else wall_seconds
        max_requests = r.task_max_requests if max_requests is None else max_requests
        max_tokens = r.task_max_tokens if max_tokens is None else max_tokens
        events = []
        spawn = asyncio.create_task(asyncio.create_subprocess_exec(
            *command, cwd=self.cwd, env=r.environment, stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True))
        self._process, cancelled = await _wait_owned(spawn)
        if cancelled:
            raise asyncio.CancelledError
        if self._interrupted:
            await self.interrupt()
        progress_queue = asyncio.Queue()
        stdout_task = asyncio.create_task(_read(self._process.stdout))
        if r.long_task:
            stderr_task = asyncio.create_task(
                _read_stderr(self._process.stderr, progress_queue, parse_progress=True))
        else:
            # Preserve the established normal-mode stderr contract exactly;
            # long-task progress records are only a long-mode protocol.
            stderr_task = asyncio.create_task(_read(self._process.stderr))
        readers.extend([stdout_task, stderr_task])
        progress_task = (asyncio.create_task(progress_queue.get()) if r.long_task else None)
        stdout = stderr = None
        stdout_done = stderr_done = False
        last_progress = None
        try:
            async with asyncio.timeout(timeout_seconds + 5):
                while True:
                    # A caller can pause an async-generator consumer after a
                    # progress event.  Drain every task that finished during
                    # that pause before constructing the next wait set; a
                    # completed future must never be silently dropped.
                    if not stdout_done and stdout_task.done():
                        stdout = stdout_task.result()
                        stdout_done = True
                    if not stderr_done and stderr_task.done():
                        stderr = stderr_task.result()
                        stderr_done = True
                    if progress_task is not None and progress_task.done():
                        item = progress_task.result()
                        if item is None:
                            progress_task = None
                        else:
                            last_progress = item
                            if item.state == 'checkpoint' and not self._task_progress_seen:
                                self._task_progress_seen = True
                                # New tasks must prove native readiness with
                                # the stage-zero record.  A resumed task's
                                # first record legitimately carries its
                                # persisted nonzero stage/cumulative usage.
                                if ((resume_task and (
                                        resume_stage is None or item.stage >= resume_stage))
                                        or (
                                        item.stage == 0 and item.requests == 0
                                        and item.reported_tokens == 0
                                        and item.elapsed_seconds == 0)):
                                    self._task_progress_ready = True
                            progress_task = asyncio.create_task(progress_queue.get())
                            yield item
                        continue
                    wait_for = {task for task in (stdout_task, stderr_task, progress_task)
                                if task is not None and not task.done()}
                    if not wait_for:
                        break
                    # Waiting only on unfinished tasks avoids the old
                    # completed-reader busy loop.  Results are propagated on
                    # the next loop iteration, before any further wait.
                    await asyncio.wait(wait_for, return_when=asyncio.FIRST_COMPLETED)
                if not stdout_done:
                    stdout = stdout_task.result()
                if not stderr_done:
                    stderr = stderr_task.result()
                code = await self._process.wait()
        finally:
            if progress_task is not None and not progress_task.done():
                progress_task.cancel()
            if progress_task is not None:
                await asyncio.gather(progress_task, return_exceptions=True)
        if self._interrupted:
            events.append(ErrorEvent(code='danso_cancelled', message='Worker interrupted; journal retained. No automatic replay.'))
        elif code != 0:
            failure = _failure(stderr, code)
            if (code in (2, 3) and failure.code == 'danso_request_budget'
                    and last_progress is not None and last_progress.state == 'paused'):
                has_remaining_budget = (
                    last_progress.requests < max_requests
                    and last_progress.reported_tokens < max_tokens
                    and last_progress.elapsed_seconds < wall_seconds
                )
                failure = ErrorEvent(
                    code='danso_task_paused',
                    message=(
                        'Paused at a saved checkpoint. Use /task_resume to continue.'
                        if has_remaining_budget
                        else 'Checkpoint saved; resume requires remaining budget.'
                    ),
                )
            events.append(failure)
        else:
            if any(line.startswith(prefix) for line in stderr.splitlines()
                   for prefix in (b'DANSO_ERROR=', b'DANSO_TRANSPORT=', b'DANSO_PROVIDER=')):
                raise ValueError('failure diagnostic on successful exit')
            text = stdout.decode('utf-8').strip()
            usage = _usage(stderr.decode('utf-8'))
            if not text:
                raise ValueError('empty result')
            events.append(TextDeltaEvent(text=text))
            events.append(MessageCompletedEvent())
            events.append(ResultEvent(result={'text': text, 'usage': usage}))
            events.append(CompletionEvent(stop_reason='stop'))
        for event in events:
            yield event

    async def _cleanup(self, readers):
        try:
            if self._process is not None:
                await self._terminate()
        finally:
            for task in readers:
                task.cancel()
            await asyncio.gather(*readers, return_exceptions=True)
            self._process, self._active, self._bootstrap_task = None, False, None
            self._task_progress_ready = False
            self._task_progress_seen = False
            self._task_pause_requested = False
