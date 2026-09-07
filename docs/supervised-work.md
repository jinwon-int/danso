# Supervised first use on a small real task

This is a practical walkthrough for a human supervisor running Danso once on a
small, low-impact coding task. It assumes a built binary
(`cargo build --release --locked` on the host), `/usr/bin/bwrap` available, and
that you will stay present for the whole run. It does not cover deployment,
unattended operation, or fleet use, and completing it does not prove that Danso
is ready to run without supervision.

## 1. Choose one bounded, low-impact task

Pick a single change that is small enough to review line by line and cheap to
discard, for example:

- fix one failing unit test,
- add one missing input validation with a test,
- update one stale comment or doc paragraph.

Before starting, write down the acceptance criteria in one or two sentences,
for example: "the new test fails before the change and passes after it; no
other test changes." If you cannot state the acceptance criteria before
running, the task is not bounded enough yet.

## 2. Prepare an isolated worktree and a session outside the workspace

Run against a dedicated branch in an isolated worktree so the main checkout is
untouched:

```sh
cd /path/to/your-project            # PLACEHOLDER: target repository
git worktree add ../project-supervised-danso -b danso-supervised-trial
cd ../project-supervised-danso
mkdir -p "$HOME/.danso/sessions"    # outside the workspace, parent must exist
```

The `--session` path must be outside the workspace and its parent directory
must already exist.

## 3. Choose the provider and model explicitly

This example uses the GLM adapter. `ZAI_API_KEY` must be supplied externally
through your existing credential mechanism; Danso performs no credential
discovery. Pick a concrete tool-capable GLM model available to your account —
the model is always explicit and never inferred from a name:

```sh
export ZAI_API_KEY='...'            # PLACEHOLDER: your external credential mechanism
MODEL=YOUR_GLM_MODEL                # PLACEHOLDER: choose a supported model
```

Keep the default bubblewrap sandbox. Do not pass `--unsafe-no-sandbox`.
`--trust-project` only allows reading the project's `AGENTS.md` and skill
metadata for this invocation; it is not a general permission grant.

## 4. Run with explicit finite budgets

Set turn and time budgets so the run cannot loop indefinitely
(`--max-turns` defaults to 16; per-request timeout defaults to 60 seconds):

```sh
target/release/danso \
  --provider glm --model "$MODEL" \
  --cwd . --trust-project \
  --session "$HOME/.danso/sessions/supervised-trial.jsonl" \
  --max-turns 8 \
  --timeout-seconds 300 \
  --tool-timeout-seconds 30 \
  -p 'Add input validation for VALUE in module X with a unit test. Acceptance: the new test fails before the change, passes after, and no other test changes.'
```

See `--help` for every documented option, including `--provider`,
`--reasoning-effort`, `--provider-timeout-seconds` and
`--compact-at-bytes`. Without `-p`/`--print`, output is JSONL.

## 5. Inspect the result yourself

Review the working tree and the journal — never trust the summary line alone:

```sh
git -C . status
git -C . diff
head -n 5 "$HOME/.danso/sessions/supervised-trial.jsonl"
```

Re-run the acceptance criteria you wrote in step 1 against the actual diff.

## 6. Run the checks in order: worker subset, then the full host gate

A worker-subset PASS is a subset only; it does not replace the host gate:

```sh
# Inside a Danso coding worker (Python subset only):
python3 scripts/dev_check.py --profile worker

# On the host, before merging, run the full gate (never a fallback subset):
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings
cargo test --locked && cargo build --locked
python3 scripts/dev_check.py --profile host
```

Repos without Danso's own scripts: substitute your project's real test suite
and treat its exit status as the gate.

## 7. Require independent review before merging

Another person reviews the `git diff` and the acceptance criteria before
merge. Do not automate the merge or any deployment in this workflow, and do
not add automation for it.

## Interrupted tools and unresolved operations

If a run is interrupted while a tool call is uncertain, do not automatically
replay it. Danso preserves the durable operation order and fails closed on an
unresolved call; recovery is a manual decision described in the
[v0 contract](v0.md#session-and-recovery). Inspect the journal, decide whether the effect
happened, and resolve it by hand before continuing the session.

## Scope of this guide

Following these steps neither deploys anything nor demonstrates unattended
readiness. Live provider acceptance is validated separately via the
[opt-in live acceptance workflow](live-acceptance.md); this guide's example run
is a supervised trial, not such acceptance.
