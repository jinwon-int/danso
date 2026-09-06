"""Opt-in ccc-node AgentRuntime adapter; not a registered Telegram provider."""
import asyncio
import json
import os
from pathlib import Path
import signal
import stat
import uuid

from telegram_bot.core.agent_runtime import (
    CompletionEvent, ErrorEvent, MessageCompletedEvent, ModelInfo,
    ResultEvent, TextDeltaEvent, deny_approval,
)

CAP = 1024 * 1024
PROVIDERS = {
    'glm': ('ZAI_API_KEY', 'DANSO_GLM_BASE_URL'),
    'openai': ('OPENAI_API_KEY', 'DANSO_OPENAI_BASE_URL'),
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


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate diagnostic key')
        result[key] = value
    return result


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
    label = 'timeout' if category == 'run_timeout' else category
    return ErrorEvent(code='danso_' + label, message=(
        f'Worker failed: category={category}, exit_code={code}{counts}. '
        'Reported usage may omit failed requests; not a total attempt count. No automatic replay.'))


class DansoRuntime:
    """One configured model; explicit credentials and private journal directory."""
    def __init__(self, *, binary, state_directory, provider, model, environment,
                 timeout_seconds=300, provider_timeout_seconds=60, max_turns=16, compact_at_bytes=None):
        if provider not in PROVIDERS or not model or not isinstance(model, str):
            raise ValueError('invalid provider/model')
        for value, maximum in ((timeout_seconds, 3600), (provider_timeout_seconds, 300), (max_turns, 128)):
            if type(value) is not int or not 1 <= value <= maximum:
                raise ValueError('invalid worker limit')
        if compact_at_bytes is not None and (type(compact_at_bytes) is not int
                or not 8192 <= compact_at_bytes <= 393216):
            raise ValueError('invalid compaction threshold')
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

    async def interrupt(self):
        if self._active:
            self._interrupted = True
            if self._process is not None:
                await self._terminate()

    async def _terminate(self):
        if self._stop_task is None:
            self._stop_task = asyncio.create_task(_stop(self._process))
        await asyncio.shield(self._stop_task)

    async def send_turn(self, message, *, approval_handler=deny_approval):
        async with self._lock:
            if not isinstance(message, str) or not message.strip() or len(message.encode()) > 65536:
                yield ErrorEvent(code='danso_input', message='Invalid worker input.')
                return
            r = self.runtime
            command = [r.binary, '--cwd', str(self.cwd), '--session', str(r.root / (self.session_id + '.jsonl')),
                       '--provider', r.provider, '--model', r.model, '--max-turns', str(r.max_turns),
                       '--timeout-seconds', str(r.timeout), '--provider-timeout-seconds', str(r.provider_timeout),
                       '-p']
            if self.effort is not None:
                command += ['--reasoning-effort', self.effort]
            if r.compact_at_bytes is not None:
                command += ['--compact-at-bytes', str(r.compact_at_bytes)]
            command += ['--', message]
            self._active, self._interrupted = True, False
            readers, events = [], []
            self._stop_task = None
            try:
                spawn = asyncio.create_task(asyncio.create_subprocess_exec(
                    *command, cwd=self.cwd, env=r.environment, stdin=asyncio.subprocess.DEVNULL,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True))
                self._process, cancelled = await _wait_owned(spawn)
                if cancelled:
                    raise asyncio.CancelledError
                if self._interrupted:
                    await self.interrupt()
                readers = [asyncio.create_task(_read(self._process.stdout)),
                           asyncio.create_task(_read(self._process.stderr))]
                async with asyncio.timeout(r.timeout + 5):
                    stdout, stderr = await asyncio.gather(*readers)
                    code = await self._process.wait()
                if self._interrupted:
                    events.append(ErrorEvent(code='danso_cancelled', message='Worker interrupted; journal retained. No automatic replay.'))
                elif code != 0:
                    events.append(_failure(stderr, code))
                else:
                    if any(line.startswith(b'DANSO_ERROR=') for line in stderr.splitlines()):
                        raise ValueError('failure diagnostic on successful exit')
                    text = stdout.decode('utf-8').strip()
                    usage = _usage(stderr.decode('utf-8'))
                    if not text:
                        raise ValueError('empty result')
                    events.append(TextDeltaEvent(text=text))
                    events.append(MessageCompletedEvent())
                    events.append(ResultEvent(result={'text': text, 'usage': usage}))
                    events.append(CompletionEvent(stop_reason='stop'))
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

    async def _cleanup(self, readers):
        try:
            if self._process is not None:
                await self._terminate()
        finally:
            for task in readers:
                task.cancel()
            await asyncio.gather(*readers, return_exceptions=True)
            self._process, self._active = None, False
