# memory fixtures (issue #52 §9/§11)

Synthetic fixtures only — no real node data. Origin: the ccc-node memory
suites in `jinwon-int/ccc-node` at commit `93db3502` (the commit the #52
design lists as its reference):

- `scenario/` — the `ccc-memory-eval.sh --scenario` suite: the eight facts
  (new-editor, old-editor, volatile-pr, benchmark-adapter, standup-old,
  standup-new, freeze-future, secrets-constraint) plus the MEMORY.md/USER.md
  corpus, byte-identical to the ccc scenario heredoc.
- `golden/` — the `ccc-memory-eval.sh --golden` cases re-mapped onto the
  Danso document universe: the two cache documents (wiki.txt, honcho.txt)
  become `state/resume.md` and `state/working-state.md`, since Danso has no
  cache tier (§4.3).
- `ccc-sink.jsonl` — records as the ccc bridge sink writes them
  (`bridge/memory/distill_local_sink.py`): full superset fields.
- `ccc-hook.jsonl` — records as the ccc hook committer writes them
  (`claude/hooks/distill/local-memory-commit.py`): no `schema_version`,
  `durability`, or `source_rank`; the defaults-fill rules of §4.1 are pinned
  against this file.
- `danso.jsonl` — the Danso superset: `because`, `quote`, `supersedes`,
  explicit `valid_from`/`valid_until` windows.

All names, texts, and timestamps are synthetic.
