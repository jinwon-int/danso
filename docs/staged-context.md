# Staged context management — design note (danso#217 P2·P3)

Status: **design only, not implemented.** This note fixes the decisions that
danso#217 asked to settle before code: prompt-cache hysteresis for the soft
(B1) stage, the tail budget for the hard (B2) stage, and resume determinism
for both. Adoption of either stage is decided by a paired A/B with a
host-owned oracle ([harness-evaluation.md](harness-evaluation.md)), never by
the agent's own report.

Background: arXiv 2609.20804 found that the best cost profile came from a
two-threshold scheme — cheap elision of old tool observations at about 0.6 of
the usable window (B1), and an LLM summary of only the oldest middle span at
about 0.85 (B2) — with accuracy equal to other managed tiers, and that a
recall tool for elided content was almost never used.

## Current state (main, for reference)

| Piece | Today | Where |
|---|---|---|
| Trigger | One hard threshold: `before > limit` on the exact wire size | `src/runtime.rs:711-723` |
| Default `limit` | `floor(window × 0.70) × bytes_per_token − 32 KiB` | `src/compaction.rs:249`, `docs/providers.md` |
| Summarizer input | Older `toolResult` text reduced to 1.5 KiB head + tail (P1, #218) | `src/compaction.rs:20-125` |
| After compaction | `[latest user, checkpoint + ≤4 receipts]`; **no verbatim tail** | `src/compaction.rs:161-217` |
| Resume | Journal replay rebuilds the same projection at each `danso.compaction.v1` record | `src/session.rs:311-328` |
| Metrics | `compactions`, `compaction_failures`, `summary_*` in `DANSO_BUDGET` (P5) | `src/usage.rs` |

### Prompt-cache precondition (finding)

- **Anthropic**: cache marks sit only on the stable system block and the last
  tool definition (`src/provider/anthropic.rs:65-80`). Message history carries
  no mark, so rewriting old history never invalidates an Anthropic cache entry
  today.
- **GLM / OpenAI**: the adapters send `stable + "\n" + volatile` as one leading
  system message / `instructions` field (`src/provider/glm.rs:125`,
  `src/provider/openai.rs:90`). The volatile part carries `remaining=N`, which
  changes on every request. With automatic prefix caching, the matched prefix
  ends inside that system text, so **message history is most likely never a
  cache hit today**. This needs confirmation by measuring the
  `cacheReadTokens` share per request on a long session.

Consequence: the cache cost of P2 is only real once history is cacheable.
Moving the volatile guidance after the history, for example as the final
user-side block, is a separate change and should come first if measurement
confirms the finding. The hysteresis below is designed for that cache-friendly
state, so P2 does not need rework when it lands.

## P2 — soft stage (B1): request-only elision with hysteresis

### Rule

- Soft threshold `soft = floor(limit × ELIDE_RATIO)`, where `ELIDE_RATIO`
  defaults to **0.70**. This keeps the paper's B1/B2 proportion (0.6 / 0.85 ≈
  0.71) relative to Danso's existing hard threshold. It is configurable
  (`--elide-at-ratio`, 0 disables) and **off by default** until the A/B passes.
- Before each action request, if `before > soft` and no compaction is due
  (`before ≤ limit`), compute the *candidate watermark*. This is the last
  settled `toolResult` older than the `K = 4` most recent ones; K matches the
  receipts and the P1 policy.
- Every `toolResult` at or before the watermark is rendered with the P1
  `elide_text` head/tail excerpt (`EvidencePolicy::DEFAULT`). User, assistant
  and checkpoint messages, call ids, tool names and `isError` are unchanged.
- No recall tool is added. The marker text says the journal keeps the full
  output and that a targeted `read` with offset/limit can re-fetch a file. The
  A/B measures how often re-reads happen (below).

### Hysteresis (why a watermark, not a per-request rule)

A per-request rule that elides "everything older than the last K" would
rewrite a different message on almost every turn. Each rewrite moves the first
changed byte earlier and costs a full prefix-cache miss from that point. The
watermark avoids that:

1. **Monotonic**: the watermark only moves forward. A message once elided stays
   elided for the rest of the session, including after resume.
2. **Batched**: when the watermark advances, it jumps to the newest eligible
   result in one step, eliding every eligible message at once. The next
   advance can happen only after the context grows back past `soft`.
3. **Minimum gain**: an advance that would remove less than
   `MIN_ELIDE_GAIN = 16 KiB` is skipped. A cache break must buy a meaningful
   reduction.
4. **Bounded misses**: each advance costs at most one cache miss. The number of
   advances per session is bounded by roughly `growth / (soft − post-elision
   size)`. The count is reported (below), so the bound can be checked.

If an advance does not bring the request under `soft`, nothing else happens.
The hard stage still fires at `limit` exactly as today. P2 can delay a
compaction but never replaces the hard guarantee.

### Journal and resume determinism

- The decision depends on wire size, and wire size depends on the system text,
  memory refresh and volatile guidance. A resumed process cannot recompute it,
  so the decision is **recorded, not re-derived**. An advance appends
  `custom` / `danso.elision.v1` with `{version: 1, throughId, watermarkId}`.
  Here `watermarkId` is the journal entry id of the last elided `toolResult`.
- The record is fsynced before the next action request, like
  `danso.compaction.v1`. It may only be written between fully settled tool
  batches (`recovery.complete()`). It never rewrites or deletes original bytes.
- Journal replay (`session.rs` projection) applies the latest watermark that
  is still inside the active context window. A watermark that a later
  checkpoint has summarized away is ignored. The projection stays a pure
  function of the journal, so the existing `messages == compacted`-style check
  can assert that in-memory and replayed contexts are identical.
- Pi readers ignore the custom record and still see the full history. Older
  Danso binaries reject unknown custom types only if they validate them. The
  current replay ignores unknown `custom` records (`session.rs:334-336`), so a
  downgrade would resume without elision. That is safe, and only costs bytes.

### Interaction with P1 and compaction

- `elide_text` is idempotent on its own output: an excerpt is at most
  `head + tail + marker`, below the elision bound. When a compaction later
  summarizes a context that already contains P2 stubs, P1 leaves them as they
  are. Tests must pin this.
- Compaction keeps its current trigger (`before > limit`) and semantics.
  Elision watermarks do not change what the summarizer is allowed to see. The
  journal stays the evidence source.

## P3 — hard stage (B2): summarize the oldest span, keep a verbatim tail

### Rule

- On compaction, split the active context at a *cut*. Messages before the cut,
  including any previous checkpoint, are summarized exactly as today
  (fragments, repair allowance, schema). Messages from the cut onward are kept
  **verbatim** after the checkpoint.
- New active context: `[latest original user request (if before the cut),
  checkpoint + receipts, tail…]`. Receipts are built only from results before
  the cut, so tail results are not duplicated.

### Tail budget

- `tail_budget = floor(limit × TAIL_RATIO)`, where `TAIL_RATIO` defaults to
  **0.25** and is configurable (`--compaction-tail-ratio`, 0 = today's
  behavior). It is off by default until the A/B passes.
- Cut selection is deterministic: walk backwards from the newest message,
  summing serialized message bytes, until the next message would exceed
  `tail_budget`. Then move the cut **forward** to the nearest safe boundary.
  - A safe boundary is the start of a user message or of an assistant message
    whose every tool call has its result inside the tail.
  - The tail never starts with a `toolResult` and never splits a call from its
    result.
- **Fit check**: the post-compaction request must satisfy
  `after ≤ soft` (or `≤ floor(limit × 0.60)` when P2 is disabled). This leaves
  room for progress before the next stage fires. If it does not fit, drop the
  oldest tail turn and retry. With an empty tail the behavior is identical to
  today, so P3 can never produce a larger request than the current
  implementation. The existing `after ≤ limit && after < before` check stays.
- Why a tail at all: today every compaction drops the working set, and the
  model re-reads files it had just read (#98). The tail keeps the last few
  turns of concrete evidence without growing the checkpoint schema.

### Journal and resume determinism

- The cut is recorded, not re-derived. The checkpoint record becomes
  `danso.compaction.v1` with `version: 2` and an added
  `keepFromId` (the journal id of the first tail entry, or `null` for no tail).
- Replay validates that `keepFromId` lies after the previous boundary and
  before `throughId`'s successor, and that it starts at a safe boundary.
  Otherwise replay fails closed, as with other corrupt boundaries.
- Older binaries check `version == 1` and reject version 2 boundaries
  (`session.rs:317-321`). A downgrade therefore fails closed instead of
  resuming with a silently different context. Version 1 records stay readable
  with today's projection.
- Recovery validation still runs over the whole journal, including the
  summarized prefix and the tail. Global call-id checks are unchanged.

## Metrics (extends P5 `DANSO_BUDGET`, appended at the end, order fixed)

- `elision_advances`: P2 watermark moves; each is at most one cache miss.
- `request_elided_bytes`: bytes removed from action requests by P2, summed
  per request sent.
- `compaction_tail_bytes`: verbatim tail bytes kept by P3, per compaction,
  summed.
- `post_compaction_rereads`: `read` calls for a path already read before the
  last boundary. This is the #98 symptom; the count comes from tool arguments,
  not content.

These are counts and sizes only, with no prompt, tool output or summary text.
The A/B also reads the existing `cacheReadTokens` / `inputTokens` from
`DANSO_USAGE` to compute the cache-hit share.

## A/B plan and adoption criteria

- Arms: **A** baseline (P1 only, today); **B** = A + P2; **C** = A + P3;
  **D** = A + P2 + P3. Same task corpus, model, request/time budget and frozen
  settings per [harness-evaluation.md](harness-evaluation.md). Success is
  judged by the host-owned acceptance result only.
- Run first on the small-window model, where the paper's gains concentrate
  (glm-5.3-flash, the default threshold).
- Adopt an arm only if all of the following hold:
  - success rate is not lower than A beyond run-to-run noise (document the
    noise from repeated A runs first);
  - total tokens (`DANSO_USAGE` input + output + summary) decrease;
  - overflow / `danso_compaction` failures do not increase;
  - for P2, the cache-hit share does not drop once history is cacheable (see
    the precondition above);
  - for P3, `post_compaction_rereads` decreases.
- Rollout follows the existing self-update and canary path. The defaults flip
  only after adoption, one stage at a time.

## Tests to add with the implementation

- Determinism: in-memory context equals journal replay after (a) an elision
  advance, (b) a compaction with a tail, (c) both, (d) restart between them.
- Hysteresis: a synthetic growing session produces the expected number of
  advances, each elided message stays elided, and no advance happens below
  `MIN_ELIDE_GAIN`.
- Idempotency: P1 on a context with P2 stubs removes 0 additional bytes from
  those stubs.
- Tail safety: the cut never splits a call from its result. Tail-less fallback
  equals today's projection byte for byte. A version 2 record is rejected by
  the version 1 check (downgrade fail-closed).
- Fit check: an oversized tail shrinks until `after ≤ soft`, or reaches zero.

## Out of scope

P4 (64 KiB head/tail truncation of a single observation), P6 (model-window
table for gpt-6-astra), recall tools, planning tools and bash-only mode.
Moving the volatile guidance for cacheability is a separate change, noted
above.
