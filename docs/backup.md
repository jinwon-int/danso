# `danso backup` and `danso restore`

These commands operate on durable Danso state without starting the agent
runtime, constructing a provider, acquiring a lock, or using the network.

## Backup layout

`danso backup` resolves $DANSO_HOME with the same `config::home` rules as
`danso config check` ($DANSO_HOME, or $HOME/.danso) and writes below
`$DANSO_HOME/backups/`. Set DANSO_BACKUP_DIR to an absolute alternate backup
root. The command creates one directory named
`backup-<UTC RFC3339 timestamp with Z>`, first staging it as the sibling
`.tmp-<same timestamp id>`. The final directory is exposed only by one atomic
rename; the temporary directory is removed on success and on failure paths.
An existing backup is never overwritten.

The snapshot contains:

```
backup-<timestamp>/
  manifest.json
  config.toml
  memory/<scope>/state/...
  memory/<scope>/memories/...
  conversations/...
  journals/...
```

`memory/` is assembled from `DANSO_MEMORY_DIR`, else the configured
`[memory] dir`, else `$DANSO_HOME/memory` — the service's own order under
the same `$DANSO_HOME`. A `config.toml` that is present but does
not parse or validate refuses the backup with the fixed `config_invalid`
category: running without it would silently drop the `telegram.token_file`
exclusion and the configured memory root. (A missing `config.toml` was
already refused as `config_unreadable`, since the config component is
required.) Only valid scope directories and their
`state/` and `memories/` trees are captured. `conversations/` and `journals/` are present when
`DANSO_TELEGRAM_DATA_DIR` (or the Telegram resolver's `$DANSO_HOME/telegram`
default) resolves; they contain that directory's `conversations` tree and the
session journals (`journals/<session-id>.jsonl`) its records point at, so a
restored chat continues its session (#210). Journals hold the conversation
text, as the memory component holds facts: treat a backup as private data.

Only regular files are copied. Symlinks, unreadable entries, incomplete
layouts, and bounded-scan conditions are omitted and represented by fixed
warning categories in the manifest. Directory scans are limited to 4096
entries per directory and a fixed nesting bound. Backup files and directories
are owner-only. `config.toml` is required; an unavailable config is a backup
failure, while unreadable memory, conversation or journal entries are recorded as
warnings.

The manifest is JSON and contains no state bodies:

```json
{
  "version": 1,
  "created_at": "2026-09-16T12:00:00.000000000Z",
  "source_home": "/absolute/state/home",
  "components": [
    {
      "name": "config",
      "file_count": 1,
      "byte_count": 123,
      "warnings": [],
      "mode": "0600"
    },
    {
      "name": "memory",
      "file_count": 2,
      "byte_count": 456,
      "warnings": []
    },
    {
      "name": "conversations",
      "file_count": 1,
      "byte_count": 789,
      "warnings": []
    },
    {
      "name": "journals",
      "file_count": 1,
      "byte_count": 2048,
      "warnings": []
    }
  ],
  "exclusions": [
    "telegram.token_file",
    "telegram.token_lock",
    "telegram.health"
  ]
}
```

The optional `mode` is the source octal mode of `config.toml`; it is recorded
as a string so the leading zero is retained. All other manifest data consists
of counts, modes, relative component layout, and fixed categories. The fixed
warning categories are `missing`, `unreadable`, `layout`, `scan_bound`, and
`legacy_state`.

A backup written before #210 has no `journals` component and restores as
before; a `danso` older than #210 refuses a backup that has one as
`invalid_backup`, like any other unknown component.

`legacy_state` on the `memory`, `conversations` or `journals` component means the
snapshot is complete for the root the service uses, but that root is the
`$DANSO_HOME` default and the pre-#136 location (`$HOME/.danso/memory` or
`$HOME/.danso/telegram`) still holds entries which are **not** in the
archive (#177). The backup neither reads nor copies the old tree; migrate
it first (`danso doctor` names both paths under `home.legacy_state`), then
back up again. A backup carrying this category restores like any other; a
`danso` older than the category refuses it as `invalid_backup`.

The same omission is named where the operator is looking (#185): a
successful `danso backup` prints one
`backup warning: <old> is not in the snapshot; the service uses <new>`
line per stranded tree on stderr — paths only, never contents. A node
without the `DANSO_HOME` split prints nothing, a deliberately chosen root
(`DANSO_TELEGRAM_DATA_DIR`, `DANSO_MEMORY_DIR`, `memory.dir`) is never
compared, and the manifest, its schema, and the exit status are unchanged.

The configured Telegram token file is never opened or copied. The
`.telegram-token.lock` file and `data-dir/health.json` are also excluded:
credentials must not enter a portable snapshot, and locks/health are runtime
artifacts that must not be replayed. Credential-shaped filenames are skipped
defensively as well. The backup operation does not lock, move, delete, or
alter source state; all writes are inside its backup staging directory.

## Restore safety

Use an explicit destination:

```sh
danso restore --target /srv/new-danso-state /path/to/backup-<timestamp>
```

`--target` is mandatory. A missing backup path or missing required CLI input
is usage exit `2`; an existing but malformed backup is exit `1`. Before any
target write, restore validates a version-1 manifest, its fixed exclusions,
its component names and counts, every listed component, every regular file,
and every file's readability. Symlinks, unexpected backup entries, count
mismatches, and scan-bound violations fail closed. A file with a Telegram
token-shaped name or a token-lock name is refused with the fixed
`credential_material` category, so restore cannot plant credentials.

An existing non-empty target is refused unless `--force` is supplied. Even
with `--force`, a target containing `config.toml` is refused to protect a live
installation. With `--force`, unrelated target files are retained and backed
up component files may be replaced. Restored files use mode `0600` and
restored directories use mode `0700`.

On success restore prints exactly one body-free JSON line:

```json
{"restored":true,"target":"/srv/new-danso-state","components":[{"name":"config","file_count":1,"byte_count":123}]}
```

Exit codes are:

- `0`: backup or restore completed.
- `1`: the command ran but a backup, preflight, safety, or filesystem
  operation failed. Stderr contains only a fixed category such as
  `backup failed: write_failed` or `restore failed: target_not_empty`.
- `2`: the command could not run, including an unresolvable home, missing
  `--target`, or a backup path that does not exist.

## Starting from a restored target

A restored target is laid out like a `$DANSO_HOME` (`config.toml`,
`memory/`), except that the conversation records and their journals sit at
`<target>/conversations/` and `<target>/journals/`: the layout of a Telegram
data directory, not of `$DANSO_HOME/telegram/`. To serve from it, make the target both roots:

```sh
export DANSO_HOME=/srv/new-danso-state
export DANSO_TELEGRAM_DATA_DIR=/srv/new-danso-state
danso config check
danso service run --data-dir /srv/new-danso-state --supervise   # or the systemd unit
```

Before starting, stop any service still using the same bot token elsewhere.
The token lock is per data directory, so it does not refuse a restored copy
([telegram.md](telegram.md)). The token file named by `telegram.token_file`
is not in the snapshot, so it must exist on the new host, owner-only, at that
path. The session journals are restored with the records, so a restored chat's
first turn continues the session it had at backup time; no `/new` is needed.
A backup taken while a turn is running can catch that turn's journal
mid-write; stop the service before `danso backup` for a consistent snapshot
(the fresh-environment run does).
