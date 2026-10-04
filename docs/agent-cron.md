# Agent-cron operations (danso)

`danso cron` is the local durable task scheduler surface over the ccc-compatible
`agent-cron` store at `$DANSO_HOME/cron/tasks.json` (§6.5). The store file is
structurally identical to ccc's schema v1 — either implementation can read,
validate, and mutate a store written by the other. Everything is local: no
scheduler install, no crontab/systemd edits, no Telegram delivery, no provider
calls. Status and planning modes are read-only; `tick --execute`/`run` touch
locks, payload execution, run history, retry state, and the notify spool only;
the CRUD commands below mutate only the validated task store through the same
atomic private write path.

## Commands

- `danso cron list [--json]` — configured tasks, prompt-free.
- `danso cron describe <id> [--json]` — one task plus its live schedule, lock,
  and due view.
- `danso cron due [--at ISO8601] [--json]` — read-only due/retry resolver.
- `danso cron lock <id> [--action probe|acquire|release] [--run-id ID] [--json]`
  — the run-lock surface (probe/acquire/release).
- `danso cron tick [--dry-run] [--max-runs N] [--at ISO8601]` — the timer-facing
  planner; without `--dry-run` it executes due tasks.
- `danso cron run <id> [--dry-run] [--at ISO8601]` — one manual run of the
  payload with its spool notification and durable state commit.

### Store mutations (exit codes 0/1/2)

The CRUD commands port ccc `agent-cron add/edit/remove/enable/disable` exactly —
same flags, same check ordering, same exit codes (0 ok, 1
not-found/duplicate/validation/store errors, 2 usage errors). Every mutation
takes the store flock (`cron/locks/store.lock`), re-reads the store **inside**
the lock, validates the full candidate store fail-closed, and replaces the file
atomically (0600, previous content snapshotted to `tasks.json.bak`).

- `danso cron add <id> --schedule EXPR --prompt TEXT [flags] [--json]` — create
  a task (never executes). Duplicate id → rc 1; missing `--schedule`/`--prompt`
  → rc 2; invalid schedule/`--not-before` bounds → rc 2; candidate validation →
  rc 1 with the error list. For command payloads `--prompt` is the human
  description.
- `danso cron edit <id> [same flags as add] [--json]` — set-only partial
  update; schedule/timezone bounds are re-validated; payload flags **merge**
  (`--argv` replaces the whole argv and flips the kind to `command`); a payload
  that has no argv defaults to kind `prompt`. There are no clear semantics —
  unset by remove+add, like ccc. `--keep-after-run` only ever sets true;
  `--disabled` only ever clears `enabled` (re-enabling is `enable`).
- `danso cron remove|enable|disable <id> [--json]` — store-only mutations.
  `enable`/`disable` report `changed` and write only on an actual flip.
- Result documents carry the task **without `prompt`** plus
  `mutations.taskStoreWrite`. Unlike ccc's sparse dicts, danso materializes
  schema defaults explicitly in the stored task (`maxRunHistory: 20`,
  `notify: "none"`, ...); keys that are absent in ccc stores (optional fields
  with no value) are omitted, never written as `null`.

Flags: `--schedule --prompt --name --timezone --notify --notify-chat-id
--permission-mode --catch-up-policy --anchor-at --not-before --redact-profile
--allowed-tools CSV --success-exit-codes INT-CSV --max-catchup
--lock-timeout-sec --max-run-history --max-runs`, payload buckets `--cwd
--model --timeout-sec --output-max-bytes`, `--argv WORD` (repeatable — each
occurrence appends one command word; hyphen-leading words like `-c` are
consumed as values, matching ccc), and the booleans `--keep-after-run
--disabled --json`. Enum-valued flags (`--notify`, `--permission-mode`,
`--catch-up-policy`) are plain strings that fail validation (rc 1) exactly
where ccc's fail.

### Import

- `danso cron import --from PATH [--json]` — danso addition (ccc has no
  import). Loads a ccc-compatible `tasks.json` (schema v1, fail-closed),
  skips ids already present in the store (`skipped` with the reason), adds the
  rest with defaults materialized, and writes only when at least one task was
  added. An invalid or unreadable source is refused before any mutation
  (rc 1); conflicting ids are resolved by remove+add, not by overwrite.

## Schedule forms

- **Cron:** 5-field expression or `@hourly|@daily|@weekly|@monthly|@yearly`,
  matched in the task's `timezone` (IANA name, default UTC). Fields accept
  `*`, values, ranges, comma lists, and `/S` steps; numeric equivalents only
  (day-of-week `0`/`7` = Sunday).
- **Interval:** `every <N>m|h|d` (1 minute..366 days), free-running from
  `lastRunAt`; `anchorAt` phase-anchors occurrences. A never-run interval task
  with no anchor is due immediately once.
- **One-shot:** `at <ISO8601>` or a bare ISO8601 timestamp; naive stamps anchor
  to the task timezone. Successful one-shots auto-disable unless
  `keepAfterRun: true`.
- Unknown timezones and malformed expressions fail closed as
  `invalid-schedule` in the planners.
- **No system tzdata required.** IANA names resolve against the tz database
  compiled into the binary (`chrono-tz`), never `/usr/share/zoneinfo`, so a
  Termux or minimal-container host resolves `Asia/Seoul` exactly like any other
  node — the failure mode behind ccc's `unknown timezone: Asia/Seoul` on Termux
  Python. `import` keeps each task's `timezone` verbatim (no conversion to a
  fixed offset), so DST zones round-trip losslessly. Operator decision
  2026-09-16 (#120, #33).

## Payload kinds

- **prompt** (default when a payload exists without argv): runs the prompt
  through danso's own subprocess harness (danso is the harness — there is no
  external headless runner to install). Optional `payload.model` passes
  through; wall-clock timeout defaults to 3600s.
- **command**: `payload.argv` runs directly (no shell interpolation, no token
  spend). Optional `cwd`, `timeoutSec` (default 600s), `outputMaxBytes`
  (default 64 KiB). `model` is rejected for command payloads; `argv`/`cwd` are
  rejected for prompt payloads — both fail closed at CRUD time and on load.

By default only exit 0 is success. `--success-exit-codes 0,1` marks watch-type
tasks whose exit 1 means "ran fine, found something"; only codes outside the
set count as failed. A task with **no** `retryPolicy` has no retry concept and
is never labelled retry-exhausted. `maxRuns` makes bounded tasks safe for
one-time jobs (reaching the limit disables the task and cancels pending
retries); pair an annual one-shot with `notBefore` so it cannot catch up a
year-old occurrence.

## Notify modes

`none` (default), `telegram-owner`, `telegram-owner-on-failure`,
`telegram-chat`, `telegram-chat-on-failure` (+ required `--notify-chat-id`).
Delivery goes through the notify spool write path (PR3): redacted,
display-capped entries under the spool directory, delivered by the bridge, not
by danso. The spool directory is `DANSO_AGENT_CRON_PUSH_SPOOL`, else
`DANSO_PUSH_SPOOL`, else `$DANSO_HOME/telegram/spool`; each run that notifies
writes one owner-only `<ts>-<task id>-<run id>.json` there, and `run`/`tick
--json` report its path as `notification.spoolPath`. An `-on-failure` mode
writes nothing for a successful run. Divergence from ccc, carried since PR3: the redaction pipeline is
static, so the ccc `blocked-redaction-unavailable` delivery state is
unreachable here.

### Fleet alert titles

A fleet command payload (`adapter-fleet-watch`, `fleet-doctor-sweep`) prints
one diagnostic row per affected node. For a **non-success** run, the redacted
stdout/stderr are scanned for the exact line-start tokens `DOWN`,
`UNREACHABLE`, `DRIFT`, `BOOTPATH`, `DUALDOMAIN`, `NONCANONICAL`, `DEGRADED`,
and `UNVERIFIED` (token followed by a space, a tab, or the end of the line).
If any are present the first line becomes

```
danso cron fleet alert for task <id>: DOWN=2 UNREACHABLE=1
```

with tokens in that fixed order and each count capped at 999. Only the
validated task id and the counts reach the title — node names, paths, and the
rest of each row never do. The scan covers the whole captured output (already
bounded by `outputMaxBytes`), not the 900-character body excerpt, so a row late
in a long watch matrix still raises the alert. Successful runs, outputs without
a recognized token, and ids outside `^[A-Za-z0-9_.-]{1,96}$` keep the generic
`danso cron task <id> finished with status=<status>` line. This is ccc
`fleet_diagnostic_title` (#829) with the danso title prefix; ccc's precomputed
`fleetDiagnosticTitle` headless field is not needed because danso's headless
document keeps the capped output instead of re-cutting it to 4000 characters.

## SIGPIPE (documented divergence)

Rust ignores `SIGPIPE` process-wide and spawned children inherit the ignore,
unlike ccc's Python children which die silently on a closed pipe. Two visible
symptoms, both accepted and documented rather than "fixed": (1) `danso cron
list | head` and friends hit a write error instead of dying quietly (Rust
panics on stdout write failure once the pipe closes); (2) command-payload
children get `EPIPE` write errors instead of `SIGPIPE` death. `run`/`tick`
execution itself is unaffected — payload output is captured through pipes that
the parent holds open and drains (`exec.rs`), then the child is waited on.
Restoring the default disposition (`pre_exec`/`SIG_DFL`) is a deliberate
non-goal for #120; revisit only if a real payload misbehaves.

## Safety boundaries

Read-only modes (`list`, `describe`, `due`, `lock` probe, `tick --dry-run`,
`run --dry-run`) never acquire locks, execute payloads, write spools, or touch
state. `add`/`edit`/`remove`/`enable`/`disable`/`import` mutate only the
validated task store under the store flock via the atomic private write path.
Execution modes may write history, retry state, and owner-redacted spool
entries, but never install timers, edit crontab/systemd, or deliver Telegram
directly. Every mutation names its effect in `mutations` (`taskStoreWrite`,
`spoolWrite`, `execute` — danso's names for ccc's `taskStoreWrite`/
`pushSpoolWrite`/`headlessExecute`).

## Source map

- `src/cron/store.rs` — schema v1 types + fail-closed load/validate (the port
  of `agent_cron_schema.validate_store` plus duplicate ids and cross-field
  payload rules).
- `src/cron/crud.rs` — the mutation core (clap-free, directly tested): ccc
  `parse_crud_args`/`_crud_add`/`_crud_edit`/`crud_command` semantics with the
  flock re-load and the danso write path.
- `src/cron/locks.rs` / `src/cron/commit.rs` — store flock, `.bak`-snapshotted
  atomic writes, history archive, retry/run-limit transitions.
- `docs/agent-cron.md` — this document; the ccc reference lives at
  `/opt/ccc-node/docs/agent-cron.md`.
