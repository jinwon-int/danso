# Merge queue rollout

The `contracts` job runs for every pull request and `merge_group: checks_requested`,
without path or PR-only job filters. Queue builds run the same Rust, sandbox and
release-artifact tests as PR builds. `scripts/test_merge_queue.py` guards this
wiring offline; it does not replace a GitHub queue run.

## Activation (separate settings action)

Merge this CI preparation before enabling a `main` merge-queue ruleset. Preserve
all existing classic protection, including strict `contracts`, linear history
and conversation resolution. Do not add bypass actors or relax checks to unblock
the queue. Start with `ALLGREEN`, `SQUASH`, one entry per build/merge and a
60-minute check timeout. These are rollout targets, not proof settings are active.

Record the full reviewed PR head, then use
`gh pr merge NUMBER --squash --match-head-commit REVIEWED_FULL_SHA` once required
checks pass to request queue admission. Add `--auto` only if repository auto-merge
is enabled and admission must wait. A head change requires re-review; do not
silently refresh the expected SHA. GitHub owns eligibility and the final merge;
do not use `--admin`. Verify a `merge_group` run
on `gh-readonly-queue/main/...` succeeds before declaring rollout complete.
Branch cleanup happens after the PR actually merges, not merely after enqueue.

Read back both protection surfaces:

```sh
gh api repos/jinwon-int/danso/branches/main/protection
gh api repos/jinwon-int/danso/rules/branches/main
```

If queue checks stall, retain run evidence and remove only the newly added queue
ruleset after confirming its contents are unchanged. Never remove or overwrite
classic protection as a rollback. No runtime deployment, provider request or
release is part of this rollout.
