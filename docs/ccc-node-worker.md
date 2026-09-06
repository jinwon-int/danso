# Experimental ccc-node auxiliary worker

`integrations.ccc_node.DansoRuntime` implements ccc-node's `AgentRuntime` and
`AgentSession` Python contracts. It runs one bounded Danso CLI process per turn
and maps validated results to native ccc-node events. This is an opt-in auxiliary
worker integration, not a registered `CCC_AGENT_PROVIDER` or Telegram provider.
It does not replace a running ccc-node service or enable broker dispatch.

Use Python 3.11 or newer, matching ccc-node's supported minimum.
Install/use the normal ccc-node Python environment (`telegram_bot` package),
make this repository importable, and build Danso with `cargo build --locked`.
For example, from a ccc-node process with an explicitly supplied credential:

```python
import os
from integrations.ccc_node import DansoRuntime
from telegram_bot.core.agent_runtime import SessionRequest

runtime = DansoRuntime(
    binary='/srv/danso/target/debug/danso',
    state_directory='/srv/ccc/danso-journals',
    provider='glm', model='glm-5.3-flash',
    environment={'PATH': '/usr/bin:/bin', 'HOME': '/srv/ccc/danso-home',
                 'ZAI_API_KEY': os.environ['ZAI_API_KEY']},
    timeout_seconds=180, provider_timeout_seconds=60, max_turns=8,
)
session = await runtime.start_or_resume(SessionRequest(
    working_directory='/srv/ccc/tasks/task-001', effort='low'))
# Persist this opaque ID for explicit future resume; it names an adapter journal.
worker_session_id = session.session_id
async for event in session.send_turn('Fix add.sh and run bash test.sh.'):
    handle_worker_event(event)  # ccc-node caller owns presentation/routing
```

The state directory must be owner-controlled mode 0700, outside the workspace,
with no symlinks in its path. Journals are owner-only and preserved. The caller
must create the workspace and private HOME. Different tasks should use separate
workspaces; different session IDs do not isolate shared workspace files.
The environment accepts only PATH, HOME and the selected provider's credential
and optional endpoint variables. No ambient environment is inherited. Use
`DANSO_GLM_BASE_URL` explicitly if the operator selects the coding endpoint.
Credentials are not stored in journals or error events by the adapter.

Supported behavior:

- `start_or_resume`: one configured model, UUID journal ID, explicit resume.
  Danso remains authoritative for journal consistency, workspace match and
  uncertain operations. A missing stored ID is rejected; no guessed paths.
- `send_turn`: serialized on one session object, separate objects may run
  concurrently. Native journal locks reject competing writers of the same ID.
- Success: one `TextDeltaEvent`, `MessageCompletedEvent`, `ResultEvent` (text and
  numeric token usage), then `CompletionEvent`. Output is buffered, not streamed.
  A zero exit alone is insufficient: nonempty UTF-8 output and matching valid
  `DANSO_USAGE`/`PIRI_USAGE` counters are required. Unknown cost is omitted.
- Failure: one terminal, non-retryable `ErrorEvent`, with static sanitized text.
  No raw provider stderr or partial answer is forwarded. Private journals remain
  for diagnosis; uncertain tools are never automatically acknowledged or replayed.
- Interrupt: idle is a no-op. Active execution receives SIGTERM, with SIGKILL
  escalation; cancellation of the caller task also cleans up. Owned process
  groups and output readers are cleaned before terminal events are delivered.
- Bounds: each stdout/stderr stream is limited to 1 MiB; CLI run timeout is
  supervised with a 5-second adapter grace. Request/turn limits are explicit.
  The normal Linux bubblewrap sandbox stays enabled.

Memory routing, custom sandbox/approval policies, approval reviewers, provider
switching, model discovery and tool progress streaming are not
implemented by this initial adapter. Unsupported request policies and memory
routes fail before launch instead of being silently ignored. Project trust is
not enabled. The explicitly selected HOME may still contain normal Danso global
context; use an isolated HOME rather than an audience-scoped ccc memory route.
A successful smoke test does not establish long-task GLM reliability.

## Optional compaction

Set `compact_at_bytes=16384` on `DansoRuntime` to opt in to native checkpoint
compaction (integer range 8192..393216). The default `None` retains the normal
uncompressed request limit. Summary requests share the same request/turn budget,
provider timeout and whole-run deadline. Invalid summaries remain failures; the
adapter does not repair journals or retry failed turns.

Resume the saved adapter UUID with a new runtime/session object and the desired
threshold. Native Danso reads the stored checkpoint and keeps completed tool IDs
from the entire journal, so old effects remain forbidden after compaction.
Checkpoint creation is recorded in the private journal, not a new ccc-node event.

## Offline validation

`python3 scripts/test_ccc_node.py` exercises a real Danso binary and sandbox
against local synthetic provider responses. Tests use an unchanged pinned
ccc-node contract in `tests/fixtures/ccc_node/`; its README records provenance.
`CCC_NODE_SOURCE=/path/to/ccc-node python3 scripts/test_ccc_node.py` checks the
same suite against that checkout's real contract. No Telegram send, provider
API call, service restart or fleet change is part of this suite.
