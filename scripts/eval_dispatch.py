#!/usr/bin/env python3
"""Loopback-only Responses rehearsal gate. No live-provider credential support."""
import asyncio
import hashlib
import json
import os
import re
import secrets
import time

from eval_case import private_root, write_new
from harness_eval import require, rendered

BODY_CAP = 8 * 1024 * 1024
HEADER_CAP = 16384
CONNECTION_CAP = 8


async def close_writer(writer):
    writer.close()
    try:
        await asyncio.wait_for(writer.wait_closed(), .2)
    except (OSError, asyncio.TimeoutError):
        writer.transport.abort()
    except asyncio.CancelledError:
        writer.transport.abort()
        raise


def loads(raw):
    def pairs(items):
        result = {}
        for k, v in items:
            require(k not in result)
            result[k] = v
        return result
    return json.loads(raw, object_pairs_hook=pairs)


async def headers(reader):
    raw = await reader.readuntil(b'\r\n\r\n')
    require(len(raw) <= HEADER_CAP)
    lines = raw[:-4].decode('ascii').split('\r\n')
    fields = {}
    for line in lines[1:]:
        key, value = line.split(':', 1)
        key = key.lower()
        require(key and key not in fields and key.strip() == key)
        require(all(32 <= ord(c) <= 126 for c in value))
        fields[key] = value.strip()
    return lines[0], fields


async def body(reader, fields, *, response=False):
    transfer = fields.get('transfer-encoding')
    if transfer is not None:
        require(response and transfer.lower() == 'chunked' and 'content-length' not in fields)
        result = bytearray()
        while True:
            line = await reader.readuntil(b'\r\n')
            require(len(line) < 128)
            require(re.fullmatch(rb'[0-9a-fA-F]+\r\n', line) is not None)
            size = int(line.strip(), 16)
            require(0 <= size <= BODY_CAP - len(result))
            if not size:
                require(await reader.readexactly(2) == b'\r\n')  # no trailers
                return bytes(result)
            result.extend(await reader.readexactly(size))
            require(await reader.readexactly(2) == b'\r\n')
    length = fields.get('content-length')
    require(length is not None and length.isascii() and length.isdecimal())
    size = int(length)
    require(size <= BODY_CAP)
    return await reader.readexactly(size)


def usage(raw, content_type, status):
    if status != 200:
        return None
    try:
        if content_type.split(';')[0] == 'text/event-stream':
            text = raw.decode('utf-8').replace('\r\n', '\n')
            require(text.endswith('\n\n'))
            terminal = []
            for frame in text.split('\n\n'):
                data = '\n'.join(line[5:].lstrip(' ') for line in frame.split('\n') if line.startswith('data:'))
                if not data:
                    continue
                event = loads(data)
                require(type(event) is dict)
                if event.get('type') in ('response.completed', 'response.failed', 'response.incomplete'):
                    terminal.append(event.get('response'))
            require(len(terminal) == 1)
            result = terminal[0]
        else:
            require(content_type.split(';')[0] == 'application/json')
            result = loads(raw)
        require(type(result) is dict and result.get('status') == 'completed')
        u = result['usage']
        values = [u[k] for k in ('input_tokens', 'output_tokens', 'total_tokens')]
        require(all(type(n) is int and 0 <= n <= 10**12 for n in values))
        require(values[0] + values[1] == values[2])
        return values[2]  # cached inputs already belong to input_tokens
    except (ValueError, TypeError, KeyError, RecursionError):
        return None


class DispatchGate:
    """One fresh job, one budget, fixed loopback fixture endpoint, no retries.

    Callers must isolate the harness and prevent bypassing this route. This
    component cannot claim that an arbitrary process used only this gateway.
    """
    def __init__(self, upstream, evidence, *, model, effort='medium', request_limit=4,
                 wall_seconds=30, request_seconds=10, output_tokens=4096):
        require(type(upstream) is tuple and len(upstream) == 2 and upstream[0] == '127.0.0.1')
        require(type(upstream[1]) is int and 1 <= upstream[1] <= 65535)
        require(type(model) is str and 0 < len(model) <= 128)
        require(effort in ('low', 'medium', 'high'))
        for n, low, high in ((request_limit, 1, 128), (wall_seconds, 1, 3600),
                              (request_seconds, 1, 120), (output_tokens, 1, 65536)):
            require(type(n) is int and low <= n <= high)
        self.upstream, self.model, self.effort = upstream, model, effort
        self.request_limit, self.request_seconds = request_limit, request_seconds
        self.output_tokens = output_tokens
        self.deadline = time.monotonic() + wall_seconds
        self.token = secrets.token_hex(32)
        self.evidence = private_root(evidence)
        self.fd = os.open(self.evidence / 'dispatch.jsonl', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            os.fchmod(self.fd, 0o600)
        except BaseException:
            os.close(self.fd)
            raise
        self.rows = []
        self.tasks = set()
        self.writers = set()
        self.rejected = 0
        self.server = None
        self.closed = False
        self.journal_ok = True
        self.close_task = None

    def record(self, row):
        if row['state'] == 'started':
            row = {**row, 'dispatch_attempted': None}  # crash here means unknown, never safe to replay
        raw = json.dumps(row, sort_keys=True, separators=(',', ':')).encode() + b'\n'
        try:
            while raw:
                written = os.write(self.fd, raw)
                if written <= 0:
                    raise OSError('short journal write')
                raw = raw[written:]
            os.fsync(self.fd)
        except OSError:
            self.closed = True
            self.journal_ok = False
            raise

    async def start(self):
        require(self.server is None and not self.closed)
        try:
            self.server = await asyncio.start_server(self.admit, '127.0.0.1', 0, limit=HEADER_CAP)
        except BaseException:
            await self.close()
            raise
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    def admit(self, reader, writer):
        if self.closed or len(self.tasks) >= CONNECTION_CAP:
            writer.close()
            return
        task = asyncio.create_task(self.handle(reader, writer))
        self.tasks.add(task)
        self.writers.add(writer)
        def finished(task):
            self.tasks.discard(task)
            self.writers.discard(writer)
            writer.close()  # also covers cancellation before handle first runs
        task.add_done_callback(finished)

    def validate(self, raw):
        data = loads(raw)
        allowed = {'model', 'reasoning', 'max_output_tokens', 'input', 'instructions',
                   'tools', 'tool_choice', 'store', 'include', 'stream', 'parallel_tool_calls',
                   'temperature', 'top_p', 'text', 'truncation', 'prompt_cache_key'}
        require(type(data) is dict and set(data) <= allowed)
        require(data.get('model') == self.model and data.get('store') is False)
        require(type(data.get('max_output_tokens')) is int and data['max_output_tokens'] == self.output_tokens)
        require(type(data.get('reasoning')) is dict and data['reasoning'].get('effort') == self.effort)
        require('stream' not in data or type(data['stream']) is bool)
        require(type(data.get('tools', [])) is list)
        for tool in data.get('tools', []):
            require(type(tool) is dict and tool.get('type') == 'function'
                    and tool.get('name') in ('read', 'write', 'edit', 'bash'))
        return data

    async def upstream_call(self, raw, row):
        reader, writer = await asyncio.open_connection(*self.upstream, limit=HEADER_CAP)
        try:
            packet = (f'POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{self.upstream[1]}\r\n'
                      f'Content-Type: application/json\r\nContent-Length: {len(raw)}\r\nConnection: close\r\n\r\n').encode() + raw
            row['dispatch_attempted'] = True
            writer.write(packet)
            await writer.drain()
            line, fields = await headers(reader)
            protocol, status, _ = line.split(' ', 2)
            require(protocol in ('HTTP/1.0', 'HTTP/1.1') and re.fullmatch(r'[0-9]{3}', status) is not None)
            status = int(status)
            require(200 <= status <= 599)
            content_type = fields.get('content-type', '')
            require(content_type.split(';')[0] in ('application/json', 'text/event-stream'))
            require(fields.get('content-encoding', 'identity') == 'identity')
            result = await body(reader, fields, response=True)
            require(await reader.read(1) == b'')  # fixture must close; deadline still applies
            return status, content_type, result
        finally:
            await close_writer(writer)

    async def reply(self, writer, status, content_type, raw):
        writer.write((f'HTTP/1.1 {status} Result\r\nContent-Type: {content_type}\r\n'
                      f'Content-Length: {len(raw)}\r\nConnection: close\r\n\r\n').encode() + raw)
        await writer.drain()

    async def handle(self, reader, writer):
        row = None
        status, content_type, result = 400, 'application/json', b'{"error":"invalid_request"}'
        try:
            async with asyncio.timeout(min(self.request_seconds, max(0, self.deadline - time.monotonic()))):
                line, fields = await headers(reader)
                require(line == 'POST /v1/responses HTTP/1.1')
                require(secrets.compare_digest(fields.get('authorization', ''), 'Bearer ' + self.token))
                raw = await body(reader, fields)
                self.validate(raw)
                require(not self.closed)
                if len(self.rows) >= self.request_limit:
                    self.rejected += 1
                    status, result = 429, b'{"error":"evaluation_request_budget"}'
                else:
                    row = {'sequence': len(self.rows) + 1, 'state': 'started',
                           'request_sha256': hashlib.sha256(raw).hexdigest(),
                           'dispatch_attempted': False, 'status': None, 'total_tokens': None}
                    self.record(row)  # durable reservation before any upstream operation
                    self.rows.append(row)
                    status, content_type, result = await self.upstream_call(raw, row)
                    row.update(state='settled', status=status,
                               total_tokens=usage(result, content_type, status))
                    self.record(row)
                await self.reply(writer, status, content_type, result)
        except asyncio.CancelledError:
            raise
        except (OSError, ValueError, TypeError, KeyError, RecursionError,
                asyncio.IncompleteReadError, asyncio.LimitOverrunError, asyncio.TimeoutError):
            if row is not None:
                row.update(state='error', total_tokens=None)
                try:
                    self.record(row)
                except OSError:
                    self.closed = True
            try:
                await asyncio.wait_for(self.reply(writer, 502 if row else 400,
                    'application/json', b'{"error":"evaluation_gate_failure"}'), .2)
            except (OSError, asyncio.TimeoutError):
                pass
        finally:
            await close_writer(writer)

    def summary(self):
        settled = bool(self.rows) and self.journal_ok and all(r['state'] == 'settled' for r in self.rows)
        return {'schema': 'danso.eval.dispatch.v1', 'reserved_attempts': len(self.rows),
                'dispatch_attempts': sum(r['dispatch_attempted'] for r in self.rows),
                'rejected_budget': self.rejected, 'complete': settled, 'journal_ok': self.journal_ok,
                'total_tokens': sum(r['total_tokens'] for r in self.rows)
                if settled and all(r['total_tokens'] is not None for r in self.rows) else None}

    async def close(self):
        if self.close_task is None:
            self.close_task = asyncio.create_task(self._close())
        await asyncio.shield(self.close_task)

    async def _close(self):
        if self.fd < 0:
            return
        self.closed = True
        if self.server is not None:
            self.server.close()
        for writer in list(self.writers):
            writer.close()
        tasks = list(self.tasks)
        for task in tasks:
            task.cancel()
        try:
            await asyncio.gather(*tasks, return_exceptions=True)
            if self.server is not None:
                await self.server.wait_closed()
            write_new(self.evidence / 'summary.json', rendered(self.summary()).encode())
        finally:
            fd, self.fd = self.fd, -1
            os.close(fd)
