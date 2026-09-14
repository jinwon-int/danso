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

With `--stream-requests`, the same stream also carries one body-free
`danso_request` frame per model request dispatch (issue #69 F), now including
the run-clock `elapsed_ms` stamp (issue #98 e):

```json
{"type":"danso_request","version":1,"sequence":1,"remaining":47,"elapsed_ms":12}
```

`elapsed_ms` counts milliseconds from run start to the dispatch of that
request; it is integer telemetry only and never identifies content.

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

## Interim assistant text at message boundaries (issue #98 a)

In text mode (`-p`) the stdout stream now carries the text of every interim
assistant message — one that is followed by tool calls — as soon as it is
durably journaled, before any tool starts. Each text block is one JSON record
closed by a completed record, with a stable key order:

```
{"type":"danso_text_delta","version":1,"text":"..."}
{"type":"danso_message_completed","version":1}
```

- Frames are render notifications, not journal entries; the journal keeps
  recording whole messages only, and frames never carry tool calls, tool
  output, or arguments.
- The terminal assistant message keeps its existing `FinalAnswer` rendering:
  the plain trailing stdout text after a successful exit and validated usage.
  It is never re-emitted as frames, so `-p` output stays backward compatible
  for single-response runs.
- JSON mode (`--print-json`, `--progress-jsonl`) is unchanged; it ignores the
  new boundary events and keeps rendering full transcript frames.
- A stream consumer must treat a stdout line as a frame only when it strictly
  matches the record shapes above; any other byte stays final-answer text.
  Frames share the adapter output bound with the retained answer text.
- `integrations/ccc_node.py` consumes the frames incrementally and forwards
  `TextDeltaEvent`/`MessageCompletedEvent` per interim message while the
  worker runs; the final answer is still wrapped only after process exit.

Run `python3 scripts/test_progress.py` for the real CLI timing of interim
frames, and `python3 scripts/test_ccc_node.py` for the adapter-level stream.

Run `python3 scripts/test_progress.py` on the host for actual CLI/bubblewrap
arrival timing, tool failure and opt-in compatibility checks. The Rust extension
suite verifies progress occurs after durable markers and that a failed start
notification cannot authorize tool execution. All tests use local fake providers.
