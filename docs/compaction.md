# Context compaction and resume

Enable automatic model checkpoints for a long task:

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
of the checkpoint, the latest user request, then new messages. The checkpoint is
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
checkpoint; there is no automatic ACK, replay, repair or retry.

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
