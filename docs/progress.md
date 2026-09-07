# Durable tool progress over JSONL

`--progress-jsonl` selects JSON output and adds versioned, body-free tool
notifications between existing session/message/checkpoint frames. It conflicts
with `-p`; explicit `--mode text` is overridden by this JSONL selector. The
ordinary JSON and text modes remain unchanged when the flag is absent.

```json
{"type":"danso_progress","version":1,"sequence":1,"phase":"started","tool":"bash"}
{"type":"danso_progress","version":1,"sequence":1,"phase":"settled","tool":"bash","success":true}
```

A sequence starts at 1 for each invocation and increments for each new tool.
There is at most one active tool. These records are notifications, not journal
entries, tool calls, approvals, or input instructions. Do not import the mixed
stream as a session file; retain the actual `--session` journal instead.

- `started` is emitted and flushed after the native `started` operation marker
  is durably saved and before dispatching the executor.
- `settled` follows result persistence and the durable `settled` marker.
  `success` is the tool outcome, not proof the user task passed.
- Tool names are restricted to `read`, `write`, `edit`, `bash`, or `other` for
  custom executors. Provider call IDs, arguments, output, paths and checkpoint
  bodies are never included in progress notifications.
- An interrupted, failed, or output-blocked run may leave a start without a
  settlement notification. It must never be interpreted as success or as an
  instruction to replay an operation. A failed progress write before execution
  conservatively leaves the existing uncertain-operation recovery gate in place.
- Notifications are emitted on the existing `EventSink` boundary. They cannot
  mutate the journal, approve a tool, or replace existing operation validation.
  With the standard CLI, progress frames are flushed to stdout as they occur.

The rest of the stream still contains full transcript frames and may be
sensitive. A Telegram/client adapter must parse those locally and forward only
the allowed progress fields. Emit a final answer only after a successful process
exit and validated usage; progress alone does not establish terminal success.
The existing one-line stderr usage/error contracts remain unchanged.

This is tool lifecycle streaming, not token streaming, steering, a bidirectional
RPC protocol or memory reinjection. Long model reasoning before the first tool
can still be quiet; whole-run and provider deadlines remain in force.

Run `python3 scripts/test_progress.py` on the host for actual CLI/bubblewrap
arrival timing, tool failure and opt-in compatibility checks. The Rust extension
suite verifies progress occurs after durable markers and that a failed start
notification cannot authorize tool execution. All tests use local fake providers.
