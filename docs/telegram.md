# Telegram B1 service

`danso telegram` runs the Telegram Bot API long-poll service in the same
process as every agent turn. It never starts a Danso CLI subprocess per
message. The normal bounded tool workers remain the only subprocesses a turn
may use.

Configuration is environment-only:

- `DANSO_TELEGRAM_BOT_TOKEN` is required.
- `DANSO_TELEGRAM_ALLOWED_USER_IDS` is a comma-separated numeric allowlist.
  Missing or empty means every update is denied.
- `DANSO_TELEGRAM_DATA_DIR` selects an absolute state directory. The default
  is `$HOME/.danso/telegram`.
- `DANSO_TELEGRAM_API_BASE_URL` is optional and is intended for a controlled
  HTTPS endpoint or loopback fake server.
- `DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS` selects the long-poll timeout
  (`0..=300`, default `25`), and `DANSO_TELEGRAM_RETRIES` selects bounded Bot
  API retries (`0..=5`, default `3`).
- `DANSO_TELEGRAM_WORKSPACE` selects the absolute workspace. If omitted, the
  current directory is used.

Provider and turn defaults use the normal Danso environment, with a
Telegram-specific value taking precedence where available:

- Provider: `DANSO_TELEGRAM_PROVIDER`, then `DANSO_PROVIDER` (default
  `anthropic`).
- Model: `DANSO_TELEGRAM_MODEL`, then `DANSO_MODEL`, then the provider-specific
  model variable (`DANSO_ANTHROPIC_MODEL`, `DANSO_OPENAI_MODEL`,
  `DANSO_OPENAI_CODEX_MODEL`, or `DANSO_GLM_MODEL`).
- Reasoning effort: `DANSO_TELEGRAM_EFFORT`, then
  `DANSO_REASONING_EFFORT`.
- Provider credentials and base URLs follow [the provider
  table](providers.md). Turn limits may be set with
  `DANSO_TELEGRAM_MAX_TURNS`, `DANSO_TELEGRAM_TIMEOUT_SECONDS`,
  `DANSO_TELEGRAM_PROVIDER_TIMEOUT_SECONDS`, and
  `DANSO_TELEGRAM_TOOL_TIMEOUT_SECONDS`.

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
session pointer, provider, model and effort, the last completed turn's usage,
and aggregate usage. New fields have tolerant defaults, so records written by
the foundation version remain readable.

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

Every command is authorized before it reads or writes chat state. Unauthorized
users are logged and never receive a Bot API reply.
