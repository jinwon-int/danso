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
```

`memory/` is assembled from the configured `[memory] dir`, falling back to
the existing Danso memory default. Only valid scope directories and their
`state/` and `memories/` trees are captured. `conversations/` is present when
`DANSO_TELEGRAM_DATA_DIR` (or the Telegram resolver's $HOME/.danso/telegram
default) resolves; it contains that directory's `conversations` tree.

Only regular files are copied. Symlinks, unreadable entries, incomplete
layouts, and bounded-scan conditions are omitted and represented by fixed
warning categories in the manifest. Directory scans are limited to 4096
entries per directory and a fixed nesting bound. Backup files and directories
are owner-only. `config.toml` is required; an unavailable config is a backup
failure, while unreadable memory or conversation entries are recorded as
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
warning categories are `missing`, `unreadable`, `layout`, and `scan_bound`.

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
