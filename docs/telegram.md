# Telegram B3 service

`danso telegram` runs the Telegram Bot API long-poll service in the same
process as every agent turn. It never starts a Danso CLI subprocess per
message. The normal bounded tool workers remain the only subprocesses a turn
may use.

Configuration is resolved in one order — **environment, then
`$DANSO_HOME/config.toml`, then the default** — for every value the service
reads (docs/unified-design.md §6.1). A key the file declares is a key the
service honours; a variable that is set always wins over the file. A
`config.toml` that exists but does not parse or validate stops the service
from starting, the same as `update` and `backup`.

- `DANSO_TELEGRAM_BOT_TOKEN`, or `telegram.token_file` in `config.toml`: an
  owner-only (`0600`) regular file holding the token, read with every path
  component pinned. One of the two is required; the token itself never goes
  in the file.
- `DANSO_TELEGRAM_ALLOWED_USER_IDS` is a comma-separated numeric allowlist,
  falling back to `telegram.allowed_user_ids` when the variable is absent.
  A variable that is set but empty means every update is denied, whatever
  the file says; no variable and no file also denies everything.
- `DANSO_TELEGRAM_DATA_DIR` selects an absolute state directory. The default
  is `$HOME/.danso/telegram`.
- `DANSO_TELEGRAM_API_BASE_URL` is optional and is intended for a controlled
  HTTPS endpoint or loopback fake server.
- `DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS` selects the long-poll timeout
  (`0..=300`, default `25`), and `DANSO_TELEGRAM_RETRIES` selects bounded Bot
  API retries (`0..=5`, default `3`).
- `DANSO_TELEGRAM_HEARTBEAT_SECONDS` (or `telegram.heartbeat_seconds`)
  controls edits to the single progress message (default `60`; `0` disables
  heartbeat edits), and
  `DANSO_TELEGRAM_FOLLOWUP_CAP` bounds the durable per-chat follow-up queue
  (default `5`).
- `DANSO_TELEGRAM_WORKSPACE` (or `core.workspace`) selects the absolute
  workspace. If omitted, the current directory is used.
- `DANSO_TELEGRAM_MEMORY_SCOPE` (or `memory.scope`) selects the memory route
  used by turns and explicit memory commands (`global` by default; `shared`
  or `private-<32 lowercase hex>` are also valid). `DANSO_MEMORY_DIR` (or
  `memory.dir`) selects the absolute memory root; `doctor` and `backup`
  resolve it in the same order, so they inspect the root the service writes.
- Long-task limits use the same bounded values as the CLI. The task-specific
  environment names are `DANSO_TASK_WALL_SECONDS` (or
  `DANSO_TASK_TIMEOUT_SECONDS`), `DANSO_TASK_STAGE_REQUESTS`,
  `DANSO_TASK_MAX_REQUESTS`, `DANSO_TASK_MAX_TOKENS`, and
  `DANSO_TASK_REPEAT_LIMIT` (the wall-time default is six hours). Telegram-
  prefixed task aliases are accepted; the normal timeout environment is a
  wall-time fallback.

Provider and turn defaults use the normal Danso environment, with a
Telegram-specific value taking precedence where available:

- Provider: `DANSO_TELEGRAM_PROVIDER`, then `DANSO_PROVIDER`, then
  `provider.name` (default `anthropic`).
- Model: `DANSO_TELEGRAM_MODEL`, then `DANSO_MODEL`, then the provider-specific
  model variable (`DANSO_ANTHROPIC_MODEL`, `DANSO_OPENAI_MODEL`,
  `DANSO_OPENAI_CODEX_MODEL`, or `DANSO_GLM_MODEL`), then `provider.model`.
- Reasoning effort: `DANSO_TELEGRAM_EFFORT`, then
  `DANSO_REASONING_EFFORT`, then `provider.reasoning_effort`.
- Provider credentials and base URLs follow [the provider
  table](providers.md). Turn limits may be set with
  `DANSO_TELEGRAM_MAX_TURNS` (`core.max_turns`),
  `DANSO_TELEGRAM_TIMEOUT_SECONDS` (`core.timeout_seconds`),
  `DANSO_TELEGRAM_PROVIDER_TIMEOUT_SECONDS` (`provider.timeout_seconds`),
  `DANSO_TELEGRAM_TOOL_TIMEOUT_SECONDS` (`core.tool_timeout_seconds`),
  `DANSO_TELEGRAM_PROVIDER_RETRIES` (`provider.retries`) and
  `DANSO_TELEGRAM_MAX_OUTPUT_TOKENS` (`provider.max_output_tokens`); the
  un-prefixed `DANSO_*` names are accepted as the next alias in each case.
  An out-of-range value is refused with the name of the variable or file key
  that supplied it.
- Environment-only (no file key): `DANSO_TELEGRAM_DATA_DIR`,
  `DANSO_TELEGRAM_API_BASE_URL`, `DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS`,
  `DANSO_TELEGRAM_RETRIES`, `DANSO_TELEGRAM_FOLLOWUP_CAP`,
  `DANSO_[TELEGRAM_]COMPACT_AT_BYTES`, `DANSO_[TELEGRAM_]TRUST_PROJECT`,
  `DANSO_[TELEGRAM_]NO_TOOLS`, and the `DANSO_[TELEGRAM_]TASK_*` limits.

Example:

```sh
export DANSO_TELEGRAM_BOT_TOKEN='...'
export DANSO_TELEGRAM_ALLOWED_USER_IDS='123456789'
export DANSO_TELEGRAM_DATA_DIR='/var/lib/danso/telegram'
export DANSO_TELEGRAM_WORKSPACE='/srv/project'
export DANSO_PROVIDER='glm'
export DANSO_GLM_MODEL='glm-5.3-flash'
export ZAI_API_KEY='...'
danso telegram
```

One consumer owns a bot token at a time. Startup takes an exclusive kernel
lock on `.telegram-token.lock` below the data directory and holds it for the
service lifetime. A second consumer exits without polling. The lock file may
remain after shutdown; ownership is the live kernel lock, not file deletion.

## Turns and persistence

An authorized text update creates or resumes the chat's session pointer and
starts exactly one in-process turn. The pointer is durably written before the
turn starts. The final answer is sent only after the core has durably
completed the assistant message and returned successfully. A cancellation,
provider failure, journal failure, or Bot API response-body failure never
replays the turn or sends a partial answer.

Per-chat records live in
`data-dir/conversations/chat-id.json`. They retain the last consumed update,
session pointer, up to five displaced session pointers, provider, model and
effort, the last completed turn's usage, aggregate usage, active-turn state,
progress-message id, queued follow-up text, and body-free metadata for one
resumable long task. New fields have tolerant defaults, so records written by
the foundation version remain readable. The next polling offset is also
persisted in `data-dir/poll-offset.json`.

While a turn runs, the service sends one progress message and edits that same
message for heartbeat and body-free tool completion updates. A restart edits
the saved progress message (or sends a notice if it cannot be edited), states
that the prior turn did not complete, preserves its journal without replay,
and clears the stale active marker. Queued follow-ups remain durable and run
sequentially after the active turn reaches its terminal boundary. `/stop`
cancels the active turn and clears the queue; `/new` is refused while the
queue is non-empty.

`data-dir/health.json` is atomically replaced each poll cycle and on state
changes. It is mode `0600` and contains only schema/timing, active-turn and
queue counts, and the service PID; it never contains message content.

The service supports:

- `/start` — greeting and access information.
- `/new` — allocate a fresh session pointer for the chat.
- `/stop` — cancel the active in-process turn; the journal is retained and
  never replayed.
- `/model` — show the current model; `/model <name>` persists a per-chat
  override.
- `/effort` — show the current effort; `/effort <value>` persists an override,
  and `/effort default` returns to the service default.
- `/usage` — show last-turn and aggregate local usage; it never calls a model.
- `/task <prompt>` — start a bounded long task. Everything after the command
  word is the prompt.
- `/task_pause` — request a cooperative pause at the next safe checkpoint.
- `/task_resume` — resume the paused task without appending another prompt.
- `/distill` — enqueue the current session journal for memory distillation.
- `/memory_promote <fact-id>` — explicitly promote one private distill fact;
  replies contain only destination and promotion identifiers.
- `/resume` — swap the current session with the newest previous session;
  repeated calls toggle between them.
- `/history` — show a bounded, body-free session timeline with short ids,
  state categories, and timestamps.

Every command is authorized before it reads or writes chat state. Unauthorized
users are logged and never receive a Bot API reply.
