# `danso doctor`

`danso doctor` emits one JSON object on stdout and performs a read-only
inspection of the live installation. It never acquires a file lock, creates or
changes state, reads a provider credential, calls a provider, or accesses the
network. Files are inspected with metadata and bounded reads. Fact records,
conversation contents, allowlists, tokens, and other message bodies are never
rendered.

## Report

The report is versioned and body-free:

```json
{
  "version": 1,
  "generated_at": "2026-09-15T12:00:00Z",
  "checks": [
    {"id": "config.parse", "status": "ok", "detail": "config valid; report=..."}
  ],
  "summary": {"ok": 1, "warn": 0, "fail": 0}
}
```

`status` is one of `ok`, `warn`, or `fail`. Details use a closed vocabulary;
where a check has a count or age, the suffix uses fixed `key=value` fields.
The existing body-free `config check` projection is carried in the
`config.parse` detail on success (`version`, `kind`, `path`, `valid`,
`set_keys`, `unread_keys`, `sources`, and `allowed_user_count`). `unread_keys`
lists the set keys that no command reads yet, so a value an operator filled
in and is waiting on is visible rather than silently inert. `sources` says,
for every key some command reads, which layer supplies its value right now:
`env:<NAME>` (the variable that is set — its name, never its contents; the
first alias in the service's order), `file` (only `config.toml` names it), or
`default` (neither does: the built-in default applies, or, for a required key
such as `telegram.token_file`, the service refuses to start). Unread keys
appear only under `unread_keys`; the report claims no source for them.
Absolute inspected paths may therefore appear, but values from the files and
the environment do not.

The nine checks are emitted in this order:

- `config.parse` resolves `$DANSO_HOME` exactly as `config check` does (or
  `$HOME/.danso`), then uses the existing config parser and validator. Missing
  config is the fixed `config file missing` failure category; a present file
  that cannot be parsed or validated is `config invalid`.
- `config.permissions` checks `config.toml` for mode `0600`. Group/world
  readable files warn with `config group/world readable`.
- `home.layout` reports whether the resolved Danso home and configured memory
  directory (or the existing default) are present.
- `telegram.data_dir` resolves `DANSO_TELEGRAM_DATA_DIR` exactly as the
  Telegram service does, including its `$HOME/.danso/telegram` default, and
  reports directory presence.
- `telegram.health` reads only `health.json` within a bounded limit and
  reports schema version, service-PID presence, and started/last-poll ages in
  seconds. A missing file is exactly `health file missing`; malformed or
  incomplete data is exactly `health file unparseable`; a last poll older than
  600 seconds is exactly the `health stale` category (with the bounded age
  fields retained in the detail).
- `telegram.token_lock` reports only whether `.telegram-token.lock` exists. It
  never opens or tests the kernel lock; both present and absent are normal
  observations.
- `telegram.token_file` checks only the configured `telegram.token_file`
  path's existence and owner-only permissions. It never reads the token.
- `telegram.conversations` counts `.json` conversation files and their total
  bytes under `data-dir/conversations` using a bounded directory scan.
- `memory.store` reports the memory layout, per-scope facts line counts and
  byte sizes from bounded reads, pending `.json` distill jobs when the queue
  directory is readable, and audit file sizes when present. Unreadable
  components produce fixed warning categories (`layout`, `facts`, `distill`,
  or `audit`); fact and audit contents are never included.

The check detail strings are intentionally category-oriented. Filesystem and
parser error text is discarded rather than copied into the report or logs.

## Exit codes

- `0`: the report ran and no check failed. Warnings do not change this code.
- `1`: the report ran and at least one check has status `fail`; the JSON
  report is still printed.
- `2`: the doctor itself could not run, such as when it cannot resolve
  `$DANSO_HOME`/`$HOME`. No partial diagnostic is substituted for the report.
