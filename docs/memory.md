# Native memory (M1)

Danso's local long-term memory: an owner-only facts store with gated writes,
an in-process recall index with valid-time semantics, and injection-scanned
documents. It reimplements the audited ccc-node local-memory contract in Rust
with no Python, Node, Shell, or SQLite runtime dependency. Design source:
`jinwon-int/danso` issue #52 (§1–§13); the stage plan follows its §10.

## Status (stage plan, #52 §10)

| Stage | Scope | Status |
| --- | --- | --- |
| M1 | Storage + search core: `paths`, `scan`, `facts`, `recall`, `memory init/add/search/close/show/eval` | This PR |
| M2 | Snapshot assembly + run integration (`snapshot.rs`, `--memory read`, dynamic budgets) | **This PR** |
| M3 | Working-state + checkpoints (harness-written `working-state.md`) | Done (M1–M3) |
| M2½ | `--memory-refresh per-request` (runtime context hook) | **This PR** |
| M4 | Distill extraction + journal + transactions (`--memory read-write`, `distill/drain/rollback`) | **This PR** |
| M5 | Scope diagnostics (`check`, audit ledger, legacy read) | **This PR** (promotion deferred per §7) |

## On-disk layout (§3)

```
$DANSO_MEMORY_DIR (default ~/.danso/memory)
  <scope>/                            global | shared | private-<32 lowercase hex>
    memories/MEMORY.md, USER.md       stable, human-editable facts (0600)
    state/memory-facts.jsonl          fact records, one JSON object per line (0600)
    state/resume.md                   resume pointer (0600; written from M3)
    state/working-state.md            working state (0600; written from M3)
    state/.memory.lock                per-scope single-writer flock (0600;
                                      manual add/close, distill commits, and
                                      rollback all serialize on this one lock)
    state/wiki-candidates/            immutable per-job wiki queue (0600)
```

Every directory is 0700, every file 0600, owner == euid, `st_nlink == 1`,
symlinks refused. Violations fail open on read paths (the source is skipped)
and fail closed on write paths.

## Fact records (§4.1)

One line = one canonical JSON object: sorted keys, compact separators, raw
UTF-8, trailing newline. The record is a superset of both ccc-node writer
variants; missing fields are filled with fixed defaults on load
(`schema_version` 1, `durability` volatile for task-progress else durable,
`source_rank` 1, `review` auto-local, `audience` from `privacy` else
`private`). Unparseable lines are preserved opaquely, never dropped. The file
read bound is 8 MiB; appends roll to the last 1000 lines; an append that
changes nothing never touches the file.

## Write gates (§4.2, fixed order)

1. **Mutable-ops filter** — operational state (commit SHAs, systemd counters,
   unit states, "정상 동작" claims) in `observation|context` facts is never
   stored; it is measured live, not memorized.
2. **Rank validation** — an extraction claiming rank 2/3 must cite a quote of
   ≥ 8 characters found verbatim in the transcript; otherwise it is demoted
   to rank 1 and marked `review: needs-human`. Demotion limits closing
   authority: lower-rank facts never supersede higher-rank ones.
3. **Decision reason** — `kind=decision` requires a non-empty `because`.
4. **Auto-supersede** — a completion-vocabulary fact closes exactly one
   same-subject progress-vocabulary fact (token overlap ≥ 0.4, rank
   ordering respected) via `valid_until = now` + `review: superseded` and
   records `supersedes`.
5. **Conflict review** — same-kind open facts with overlap ≥ 0.6 leave both
   facts open; the newcomer is stored as `needs-human`. Never auto-resolved.
6. **Dedup** — normalized text (ccc charset `[0-9a-z가-힣]+`) is unique,
   batch-inclusive.

## Recall (§4.3)

The index is derived in-process on every call — no on-disk index, no
index/source drift. Documents: `memory` (MEMORY.md/USER.md, boost 3.0),
`structured` (one record = one document, path `<facts>#L<n>:<id>`, boost 2.5),
`state` (resume/working-state, boost 0.5). Scoring is the ccc formula
(`token_hits×4 + phrase×3 + boosts`), the fuzzy lane is character 3-gram
containment ≥ 0.34 scored `sim×8`, and the lanes fuse with RRF (k = 60). BM25
tiebreaks and usage boosts are deliberately not ported.

Valid time: `valid_from` inclusive, `valid_until` exclusive; current mode
excludes future facts and partitions expired below still-valid (demoted,
never deleted); `--as-of` keeps only facts valid at that instant; undated and
malformed-window facts are kept conservatively with body-free signals. With
no explicit as-of, a natural-language time reference (`어제`, `N일 전`,
`지난주`, `in March 2025`, …) estimates one — absolute beats relative,
periods resolve to their end, ambiguity gives up — and the estimate is
reported in `temporal.nl_as_of`.

Retention TTLs from the ccc policy (volatile 14d, session-only 2d, week-scale
45d, durable ∞) drop stale volatile records from the index;
`decision|procedure|constraint` never age-expire; `rejected|superseded`
records stay in the file but out of the index.

## Injection scanner (§6.2)

Every memory document passes a scanner before use: invisible/control unicode,
credential shapes (GitHub/slack/AWS/JWT/bearer/key=value tokens, PEM blocks),
and imperative prompt-injection phrases are redacted in place — the block is
never dropped — and byte caps reserve the truncation marker inside the limit,
cut on a UTF-8 boundary. Audit metadata carries category names and byte
counts only (§6.4).

## Snapshot assembly (§5, M2)

With `--memory read` the run injects a managed block into the system context,
assembled once per run (per-run refresh; per-request refresh lands with a
later stage). Block order and caps follow §5.1 — resume (2000 B, omitted when
absent) → status line → `## Built-in MEMORY + USER` (4000 B, placeholder
`(memory files unavailable)`) → `## Working-state checkpoint` (2048 B, with a
`> STALE: …` first line once the file is older than 14 days, 0 disables) →
`## Local hot memory` (dynamic: `alloc = max(3000, max_bytes − 1000 − used)`,
`limit = clamp(alloc/180, 5, 25)`, placeholder `(local hot memory disabled or
no hits)`).

The local-hot block follows §4.4: a counts-only review warning header, every
open constraint first (never budget-dropped), then query matches in ranking
order and newest-first fill, assembled with skip-then-fill where the first
fact line is always kept. Observations never enter the snapshot; needs-human
facts stay visible and marked; volatile facts carry `⟳` and a single
`⟳ live-check …` guidance line.

The managed block (§5.2) wraps the snapshot with the audited markers
(`ccc-node:codex-memory:begin/end`), the body SHA-256, `materialized-at`, the
`danso-native-v1` working-state policy, and the untrusted-data policy lines.
A stored file that itself contains the markers is a forgery and aborts the
injection with a `memory` failure. The block is capped at 32768 bytes, and
the combined system context (memory + caller `--system-context-file`) is
validated against 65536 bytes. Memory OFF is byte-identical: `--memory` off
never touches the context.

## Promotion (§7 follow-up)

`danso memory promote --from private-<32 hex> --fact distill-<12 hex>` copies
one private fact into the shared store as `review: "explicit-promotion"` with
`promoted`/`private-to-shared` tags, a deterministic promotion id
(`promotion-` + `sha256("ccc-memory-promotion-v1" ‧ NUL ‧ scope ‧ NUL ‧
fact_id)[:24]`), a destination id (`promoted-` + `sha256[:16]`), and an audit
trail under the shared state. Repeats verify and report `promoted: false` —
never duplicated, never automatic.

## Distill pipeline (§4.5–§4.7, M4)

`danso run --memory read-write` registers a pending extraction job on a
final answer or turn-budget exhaustion (`--memory-distill queue|inline|off`;
inline drains one job after the run). `danso memory distill --session
<path>` enqueues explicitly; `danso memory drain` claims pending jobs and
runs the one-turn tool-free extraction over the production provider
(env-selected, one STRICT retry whose request carries an explicit strict
instruction), validates the output against the ccc
`codex-distill-extraction-v1` contract plus the Danso `source`/`quote`
extensions (bounds, duplicate keys, NaN/Infinity, credential and directive
patterns, provenance identity, decision reasons, source ranks 3
user-stated / 2 measured / 1 inferred), applies the §4.2 write gates, and
commits both targets through the crash-recoverable
`ccc.local-memory-rollback.v1` transaction: prepared → committed with the
pre-image retained as the single undoable head, recovery completing forward
or restoring atomically, `danso memory rollback --action <32hex>` restoring
the newest head after a full post-image CAS check. Failures classify from
the provider failure kind and HTTP status only (auth 6h, quota until
01:00 UTC, rate-limit 30 min, model 6h, exponential otherwise, capped at
4 h, dead-letter after five failures, 48 h age limit, transcript-change
dead-letter) with a scope-wide cooldown for hard classes. Validated wiki
candidates land in the immutable local queue
`state/wiki-candidates/<job_id>.json` (§4.6): the first record for a job
is kept and never rewritten; consumption stays a human/agent review step.

## Diagnostics (§6.4, M5)

`danso memory check --json` reports body-free scope state: record counts
(open/closed/needs-human/constraints), journal pending/dead counters, the
cooldown state, rollback head/action counts, and file presence. The audit
ledger `state/audit.jsonl` records `MemoryCommit` and `DistillJob` events
(category names and counts only, 1 MiB rotation) — every transaction
commit (manual add/close and distill) is audited. Manual facts go through
the same rollback transaction as distill commits and serialize on the
single per-scope lock (`state/.memory.lock`), so concurrent writers can
never interleave on one facts file. The session id is hashed
wherever it must appear; fact bodies and raw session ids never enter
diagnostics.

## Working state (§5.3, M3)

The harness — not the model — records `state/working-state.md`:
- on every compaction, the checkpoint's five fields (objective / constraints
  / changes / tests / pending) are rendered into the file, after the
  previous state is preserved as a PreCompact copy under
  `state/checkpoints/working-state-YYYYMMDD_HHMMSS.md` (newest 30 kept by
  mtime);
- when a run finishes, the first 2048 bytes of the final answer are recorded
  in a `## last final answer` section, the pending list is resolved, and an
  archived copy lands in `state/session-archive/working-state-<sha256[:24]>.md`.

Writes are fail-closed (an unwritable tree fails the run), the rendered text
passes the injection scanner, and the session journal is never touched. The
STALE read-side warning lives in the snapshot (§5.1).

## CLI (§8 M1 subset)

```
danso memory init                              # create the scope tree + templates
danso memory add --kind K --text T [--because B] [--subject S] [--valid-from F] [--valid-until U]
danso memory search <query> [--as-of ISO] [--limit N] [--json]
danso memory close --fact <id>                 # valid_until = now (reversible, idempotent)
danso memory show                              # M1 snapshot preview (fixed caps)
danso memory eval --golden | --scenario        # built-in fixture suites, pinned clock
```

Root: `--memory-dir` or `$DANSO_MEMORY_DIR` (default `~/.danso/memory`);
scope: `--scope global|shared|private-<32 hex>`. Configuration errors exit 2;
runtime refusals exit 1. Read rules (§7): a private scope reads its own tree
plus `shared`; `shared` and `global` never open another tree.

`danso run` gains `--memory off|read|read-write` (read = injection-only,
§8: nothing is written; read-write registers pending distill jobs and
writes the working state; extraction runs via drain), `--memory-distill
queue|inline|off`, `--memory-refresh per-run|per-request` (per-request
re-assembles the context right after each compaction through a
memory-agnostic runtime hook), `--memory-dir`, `--memory-scope`,
`--memory-query` (default: task + cwd + git branch/changed paths, capped at
1400 bytes), `--memory-max-bytes` (1..=24576, default 12000) and
`--memory-as-of`. Memory configuration failures are configuration errors
(exit 2, category `memory` for assembly failures such as marker forgery).

## Evaluation

`danso memory eval --scenario` (9 cases from the ccc suite) gates on
recall@5 ≥ 0.8, p@1 ≥ 0.6, and temporal-current, volatile-exclusion and
temporal-semantic accuracies of exactly 1.0; `--golden` (5 cases) gates on
recall@5 ≥ 0.8 and p@1 ≥ 0.6. Fixtures are synthetic and copied from ccc-node
(`tests/fixtures/memory/README.md` documents the origin); the evaluation
clock is pinned so the gates are deterministic.
