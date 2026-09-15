# Telegram B1 foundation

The B1 module is a self-contained Bot API foundation. It does not yet select a
model, create agent sessions, or handle commands.

Configuration comes from the process environment:

- DANSO_TELEGRAM_BOT_TOKEN is required and supplies the Bot API token.
- DANSO_TELEGRAM_ALLOWED_USER_IDS is an optional comma-separated list of
  numeric Telegram user ids. If it is missing or empty, every update is denied.
- DANSO_TELEGRAM_DATA_DIR optionally selects an absolute state directory. The
  default is ~/.danso/telegram/.

One consumer owns the bot token at a time. Startup takes an exclusive kernel
lock on .telegram-token.lock below the data directory and holds it for the
process lifetime. A second consumer fails closed with a clear startup error;
the lock file itself may remain after shutdown because ownership is the live
file lock, not file deletion.

Updates are received with Bot API getUpdates long polling and answered with
sendMessage. HTTP 429 and 5xx responses use a bounded retry count and
exponential backoff. Per-chat records live in
data-dir/conversations/chat-id.json and contain the chat id, last update id,
and session pointer. Records are written through a same-directory
temporary file, fsync, and rename so they survive a restart without exposing
a partial JSON record.

Access is deny-by-default. An update is admitted only when its Telegram sender
id is present in the explicit allowlist. Rejected updates are logged and never
produce a Bot API reply.
