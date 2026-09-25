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

The ten checks are emitted in this order:

- `config.parse` resolves `$DANSO_HOME` exactly as `config check` does (or
  `$HOME/.danso`), then uses the existing config parser and validator. Missing
  config is the fixed `config file missing` failure category; a present file
  that cannot be parsed or validated is `config invalid`.
- `config.permissions` checks `config.toml` for mode `0600`. Group/world
  readable files warn with `config group/world readable`.
- `home.layout` reports whether the resolved Danso home and configured memory
  directory (`DANSO_MEMORY_DIR`, `memory.dir`, or the `$DANSO_HOME/memory`
  default) are present.
- `home.legacy_state` is the migration signal for #136 (see the state-root
  section of [architecture.md](architecture.md)). `one state root` when
  `DANSO_HOME` is unset or is `$HOME/.danso` itself. Otherwise, for each of
  the Telegram state root and the memory root that is the `DANSO_HOME`
  default (`$DANSO_HOME/telegram`, `$DANSO_HOME/memory`), the check looks at
  the pre-#136 location (`$HOME/.danso/telegram`, `$HOME/.danso/memory`):
  whenever that one has entries the detail is a warning, both paths spelled
  out. While the root in use is missing or empty it is `legacy state
  present; move it: mv <old> <new>[; mv <old> <new>] while the service is
  stopped`; once the root in use holds entries as well — the service ran
  after the switch, so a plain `mv` would clobber it (#176) — that root is
  listed instead as `both roots hold entries, reconcile by hand: <old> into
  <new>`, and a report may carry both parts. Otherwise `legacy state
  absent`. A root chosen by `DANSO_TELEGRAM_DATA_DIR`, `DANSO_MEMORY_DIR`
  or `memory.dir` is never compared. The doctor moves nothing. A backup
  taken on such a node records the same omission as the `legacy_state`
  warning on the component (see [backup.md](backup.md)).
- `telegram.data_dir` resolves `DANSO_TELEGRAM_DATA_DIR` exactly as the
  Telegram service does, including its `$DANSO_HOME/telegram` default
  (`$HOME/.danso/telegram` when `DANSO_HOME` is unset), and reports
  directory presence.
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
