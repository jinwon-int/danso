# Context compaction and resume

Experimental, opt-in model checkpoints for a long task:

```sh
danso-glm --cwd /path/to/repo --session /path/to/sessions/task.jsonl \
  --compact-at-bytes 196608 --max-turns 48 -p 'Complete the task and verify it'
```

The session parent must already exist outside the workspace. Compaction is
opt-in. Without `--compact-at-bytes`, the original hard request limit remains.
A session containing Danso checkpoints can be resumed with or without this flag;
the flag enables creation of additional checkpoints, not interpretation of old
ones. Choose the same provider/model when resuming.

## What is preserved

Before an action request exceeds the configured size, the current provider
summarizes the active conversation into exactly five structured fields:

- `objective`: the task goal;
- `constraints`: instructions and restrictions still relevant to the task;
- `changes`: file changes and completed effects;
- `tests`: observed results, including failures;
- `pending`: remaining work and uncertainty.

The latest original user request is retained verbatim. Discovered system/project
instructions remain unchanged. Older text and tool evidence are summarized;
opaque provider reasoning is omitted. After compression, model context consists
of the latest user request, the checkpoint with recent tool receipts, then new messages. The checkpoint is
labeled historical, potentially lossy data and never grants authorization.

Summaries are model-generated and can omit or misinterpret details despite
schema validation. The original journal is the evidence source. This feature
cannot prove semantic completeness or exactly-once arbitrary shell effects.

## Durable journal and recovery

Compaction appends a `custom` record with `customType: danso.compaction.v1`:

```json
{
  "type": "custom",
  "customType": "danso.compaction.v1",
  "data": {
    "version": 1,
    "throughId": "immediately-preceding-entry-id",
    "userEntryId": "latest-original-user-entry-id",
    "summary": {
      "objective": "...",
      "constraints": [], "changes": [], "tests": [], "pending": []
    }
  }
}
```

Normal Pi v3 `id`, `parentId` and `timestamp` fields are added by the store. The
record is fsynced before another action request. No old record is rewritten,
truncated or deleted. Compaction must occur between fully settled tool batches.
All original call IDs and operation records remain part of recovery validation,
including the prefix hidden from the model. Duplicated IDs, orphan/duplicate
results, invalid operation order, corrupt checkpoint boundaries, branches and
unresolved operations fail closed before continuation.

This is a Danso extension. Pi readers that ignore custom records still see the
full original history; it does not implement Pi's native `compaction` entry
semantics. Native Pi compacted/branched sessions remain unsupported for runtime
resume. The existing Pi fixture continues to validate basic interchange.

If summarization, validation, checkpoint persistence or output rendering fails,
the run stops. A failed or interrupted summary never authorizes tools or creates
a partial checkpoint. Resume reads the original history or the last complete
checkpoint; there is no automatic ACK, tool replay or journal repair. A bounded checkpoint-format/size retry is described below.

## Limits and accounting

- `--compact-at-bytes`: 8192..393216 bytes. The threshold is measured against
  the exact serialized provider request, including JSON escaping, instructions,
  tool schemas and provider fields. It is a byte budget, not a token estimate.
- The current request and static instructions must leave room for a checkpoint.
  Impossible budgets fail before any summary call; they are not silently cut.
- Historical evidence is fed to the summarizer in consecutive UTF-8-safe
  fragments, each fitting the same wire budget. No unseen evidence is truncated.
  Every fragment updates the previous checkpoint; at most 32 fragments are
  allowed per compaction.
- Each intermediate/final checkpoint is limited to one eighth of the threshold,
  capped at 16 KiB. It must pass schema validation. The final action request must
  fit the threshold and be smaller than before, before the checkpoint is saved.
- Summary requests have no tools and consume the same `--max-turns` budget as
  action requests. At least one action request is reserved. Usage includes
  summary calls and their tokens; HTTP and whole-run timeouts still apply.
- The journal retains a 16 MiB capacity/import limit; compaction reduces model
  context, not disk usage. An append exceeding capacity stops before writing.
  If a tool has already executed but its result cannot be saved, unresolved-call
  recovery still applies. Imported journals are forced to mode 0600 under lock.

## Validation

```sh
cargo build --locked
cargo test --locked
python3 scripts/test_compaction.py
```

The offline suite forces multiple compactions with an 8 KiB budget across all
three providers, checks resume and global ID retention, and covers malformed
summaries, budget exhaustion, corrupt boundaries, unsettled prefixes, interruption
and disk-write failure. It uses local fake providers and real bubblewrap.
It also generates real structured worker test receipts for passing and failing
fixtures across all three providers, forces three checkpoints, and restarts the
session. The original receipt and its tool error status remain unchanged in the
journal even when the summary incorrectly claims success. A partial count audit
of that original receipt still rejects an unreported failed suite. Run markers
verify no test or effect is repeated during the scripted restart.

This checks persistence and auditing boundaries with local fake providers; it
does not prove a live model will recall exact counts or refrain from requesting
new tool calls. Bounded checkpoint excerpts are not complete test receipts: use
the original journal evidence when auditing counts after compaction.

For a separately authorized live stress test, the existing acceptance runner can
pad source reads and test output to force at least two compactions:

```sh
# Supply ZAI_API_KEY through the existing credential mechanism.
python3 scripts/live_acceptance.py --live --provider glm --model glm-5.3-flash \
  --base-url https://api.z.ai/api/coding/paas/v4 --compact-at-bytes 16384
```

The stress runner accepts thresholds 8192..24576 so generated output stays below
the existing tool cap. It allows at most 24 model requests and 300 seconds per
invocation (two invocations, at most 48 requests overall), including summaries.
It checks all five prescribed tool calls, source/test/report content, at least two
checkpoints, and same-session recall without replay. The standard non-stress
acceptance budgets remain eight requests and 180 seconds per invocation.

Live GLM stress evidence (2026-09-06): the 16 KiB run stopped after one
checkpoint with an unclassified transport failure. One retry with
`--reasoning-effort low` saved five checkpoints but repeated reads under new
call IDs and exhausted the shared turn budget (23 completed requests); it did
not finish the edit/test/report workflow or reach resumed-session validation.
These were the initial implementation results; see the progress-receipt
validation below for the subsequent successful run. Global ID checks prevent replay of existing calls; they do
not prevent a model from requesting the same operation with a new ID.

## Progress receipts

Following Piri's separation of summaries and retained evidence, Danso rebuilds
up to four recent settled tool receipts from the journal at each checkpoint.
Each contains the tool, a bounded path/command, explicit success/error/unknown
status, and a short leading output excerpt. Combined receipt JSON is at most
1024 bytes; older entries are removed first and shortened strings are marked.
Receipts survive subsequent model summaries and are rebuilt identically on
resume. They remain untrusted historical data, never native tool calls. An error
or absent status cannot become success merely because the summary says so.

The original request now precedes the checkpoint and continuation guidance,
so the last context message describes current progress rather than reissuing
the original task. This reduces a restart cue but cannot guarantee model behavior
or prevent a model from issuing the same operation under a fresh ID. Old custom
checkpoint records remain readable without rewriting them. Whole-journal
recovery still gates projection; no uncertain operation is acknowledged.

Reference designs inspected: Piri `84859e40`
(`packages/agent/src/harness/session/context.ts`, compaction retained tail and
file-operation details), and ccc-node `164907ec`
(`bridge/core/session_resume.py`, transcript-backed resume). Danso uses bounded
receipts instead of copying potentially oversized native tool results or adding
provider-specific replay behavior.

Progress-receipt live validation (2026-09-06): one GLM-5.3-Flash run with
`--reasoning-effort low --compact-at-bytes 16384` passed. Two checkpoints
preceded completion of exactly `read`, `read`, `edit`, `bash`, `write`; the
resumed invocation recalled prior context without executing tools or changing
workspace files. Coding used 12 completed requests/27,266 tokens; resume used
one request/1,447 tokens. This is one bounded fixture, not evidence that every
long task or provider will avoid repetition. Compaction remains opt-in and
experimental. Original failed-run evidence above is retained.

## Checkpoint format and size recovery

A terminal text response with invalid JSON, an invalid five-field schema, or an
oversize checkpoint gets at most one repair request **per compaction**, shared
across all fragments and all three failure kinds. The retry uses the same
original evidence and previous valid checkpoint with stricter JSON instructions
and a target of one quarter of the hard summary allowance. Normal summarization
targets half that allowance, leaving room for later progress. It never feeds back the invalid response, strips
markdown fences, guesses missing fields, or truncates evidence. This improves
format recovery without guaranteeing that a model will return valid JSON.

The longer repair prompt is included when choosing wire-sized fragments. Both
attempts consume the existing turn/time budgets, with one action turn reserved.
Only a fully validated summary can be committed. Tool calls, nonterminal replies,
invalid response envelopes and transport errors still stop immediately. A second
format/schema/size failure anywhere in the same compaction stops and retains the
original journal; this is not a general provider retry policy.

## Targeted reads and concise progress

The `read` tool accepts optional `offset` (1-based line) and `limit` (1..2000
lines). Providing either selects a range: omitted offset means 1, omitted limit
means 200. Ranged output includes line bounds, total lines and the next offset
or EOF. Content retains original line endings. An offset one past the last
line returns an explicit empty EOF result; farther offsets fail. With neither
option, existing full-file behavior is unchanged. The 256 KiB file limit,
sandbox filesystem boundary and 64 KiB tool-output limit still apply. A range
does not grant access to paths outside that boundary.

For example, `read` arguments `{"path":"src/runtime.rs","offset":70,"limit":40}`
read only the relevant section. The worker is encouraged to use ranges/searches
and continue from checkpoints instead of repeating whole-file dumps. This does
not block rereads or cache file contents; changed files remain readable.

The summarizer is reminded that the latest original request is preserved
verbatim alongside the checkpoint. It should keep a short objective and focus
on additional constraints, changed paths, actual test results, uncertainty and
remaining work instead of repeating the original requirements or copying code.
These are generation instructions, not guarantees of model behavior. Hard
validation, original journal retention and the single repair allowance remain
unchanged.
