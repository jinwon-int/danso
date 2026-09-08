"""Opt-in ccc-node AgentRuntime adapter for bounded Danso tasks."""
import asyncio
import json
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
TRANSPORT_KEYS = {'version', 'phase', 'elapsed_ms', 'request_bytes'}
TASK_PROGRESS_STATES = {'checkpoint', 'paused', 'completed', 'blocked'}
TASK_PROGRESS_KEYS = {
    'version', 'state', 'stage', 'requests', 'reported_tokens', 'elapsed_seconds',
}


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate diagnostic key')
        result[key] = value
    return result


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
                or not 0 <= diagnostic['request_bytes'] <= 512 * 1024):
            raise ValueError('invalid transport diagnostic')
        return (diagnostic['phase'], diagnostic['elapsed_ms'], diagnostic['request_bytes'])
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
        f', phase={transport[0]}, elapsed_ms={transport[1]}, request_bytes={transport[2]}')
    label = 'timeout' if category == 'run_timeout' else category
    return ErrorEvent(code='danso_' + label, message=(
        f'Worker failed: category={category}, exit_code={code}{counts}{transport_detail}. '
        'Reported usage may omit failed requests; not a total attempt count. No automatic replay.'))


class DansoRuntime:
    """One configured model; explicit credentials and private journal directory."""
    def __init__(self, *, binary, state_directory, provider, model, environment,
                 timeout_seconds=300, provider_timeout_seconds=180, max_turns=16,
                 compact_at_bytes=None, sandbox="host", system_context_loader=None,
                 long_task=False, task_stage_requests=16, task_max_requests=1024,
                 task_max_tokens=10_000_000, task_repeat_limit=3,
                 task_pause_after_stage=None):
        if provider not in PROVIDERS or not model or not isinstance(model, str):
            raise ValueError('invalid provider/model')
        if type(long_task) is not bool:
            raise ValueError('invalid long-task setting')
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
        self.system_context_loader = system_context_loader
        self.sandbox = sandbox
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
            command = [r.binary, '--sandbox', r.sandbox, '--cwd', str(self.cwd), '--session', str(r.root / (self.session_id + '.jsonl')),
                       '--provider', r.provider, '--model', r.model, '--max-turns', str(r.max_turns),
                       '--timeout-seconds', str(r.timeout), '--provider-timeout-seconds', str(r.provider_timeout),
                       '-p']
            if r.long_task:
                command += [
                    '--long-task',
                    '--task-stage-requests', str(r.task_stage_requests),
                    '--task-max-requests', str(r.task_max_requests),
                    '--task-max-tokens', str(r.task_max_tokens),
                    '--task-repeat-limit', str(r.task_repeat_limit),
                    '--task-progress',
                ]
                if r.task_pause_after_stage is not None:
                    command += ['--task-pause-after-stage', str(r.task_pause_after_stage)]
            if self.effort is not None:
                command += ['--reasoning-effort', self.effort]
            if r.compact_at_bytes is not None:
                command += ['--compact-at-bytes', str(r.compact_at_bytes)]
            self._active, self._interrupted = True, False
            self._task_progress_ready = False
            self._task_pause_requested = False
            readers, events = [], []
            self._stop_task = None
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
                    async for event in self._execute(command, readers):
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

    async def _execute(self, command, readers):  # noqa: C901 -- bounded concurrent stdout/stderr/progress lifecycle
        r = self.runtime
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
            async with asyncio.timeout(r.timeout + 5):
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
                            if item.state == 'checkpoint':
                                # The first body-free checkpoint is the
                                # native-ready handshake.  Never signal a
                                # process before that record has been seen.
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
                    last_progress.requests < r.task_max_requests
                    and last_progress.reported_tokens < r.task_max_tokens
                    and last_progress.elapsed_seconds < r.timeout
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
                   for prefix in (b'DANSO_ERROR=', b'DANSO_TRANSPORT=')):
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
            self._task_pause_requested = False
