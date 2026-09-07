# Supervised first use on a small real task

Use this workflow for one small change in trusted code, with a supervisor
available to inspect the result. It does not deploy anything or establish
unattended readiness. Building Danso requires the toolchain described in the
[README](../README.md#build-and-run); running it requires functioning bubblewrap.

## Define acceptance first

Choose a small bug fix, regression test, or documentation update. Write down
the files in scope, expected behavior, checks to run, and what must stay
unchanged. For a bug fix, require a regression test that fails before the fix
and passes afterward. Include these criteria in the task prompt.

## Prepare the binary and an isolated worktree

Replace both `/path/to/...` placeholders below. Build in the trusted Danso
checkout and keep its absolute binary path before changing directories:

```sh
cd /path/to/danso
cargo build --release --locked
DANSO_BIN="$(pwd -P)/target/release/danso"
"$DANSO_BIN" --help

cd /path/to/your-project
git worktree add -b danso-supervised-trial ../project-supervised-danso
cd ../project-supervised-danso
```

Choose unused branch and worktree names. Check the target repository's
instructions and source before allowing `--trust-project`; this flag enables
project instruction/skill loading for the invocation, not additional tool access.
Keep the default sandbox and do not pass `--unsafe-no-sandbox`.

Create a fresh session directory outside the target workspace. The following
assumes `$HOME/.danso/sessions` is outside that workspace; choose another
external parent if necessary. Preserve this directory as run evidence.

```sh
mkdir -p "$HOME/.danso/sessions"
SESSION_DIR="$(mktemp -d "$HOME/.danso/sessions/supervised.XXXXXX")"
SESSION_PATH="$SESSION_DIR/session.jsonl"
```

A new task uses a new session path; reusing an existing path resumes its
conversation. Do not accidentally attach an unrelated task to an old session.

## Run one bounded task

Supply `ZAI_API_KEY` externally through your existing credential mechanism.
Do not put credentials in the prompt or repository. Replace the model and task
placeholders before running; use a tool-capable GLM model your account supports.

```sh
# ZAI_API_KEY is already supplied in the environment.
MODEL=YOUR_GLM_MODEL
TASK='REPLACE with the bounded task, allowed files, and acceptance criteria.'
"$DANSO_BIN" \
  --provider glm --model "$MODEL" \
  --cwd . --trust-project \
  --session "$SESSION_PATH" \
  --max-turns 32 --timeout-seconds 600 \
  --provider-timeout-seconds 120 --tool-timeout-seconds 30 \
  -p -- "$TASK"
```

These are finite request and time budgets, not a monetary cost limit or a
completion guarantee. `-p` prints the final answer; default output is JSONL.
See [provider options](providers.md) for model-specific reasoning settings.
Record elapsed time, usage, the final report, and the original journal. A final
success message alone does not establish that acceptance criteria were met.

## Inspect and validate before merging

```sh
git status --short
git diff --stat
git diff
```

Read new untracked files listed by `git status` too: ordinary `git diff` does
not show their contents. Check every changed file against the prewritten
scope and acceptance criteria, including tests and documentation.

When the target is Danso itself, use its two check profiles:

```sh
# Inside the coding worker: Python subset only.
python3 scripts/dev_check.py --profile worker

# On the host: full required gate, including Rust and sandbox integration.
python3 scripts/dev_check.py --profile host
```

The host needs Cargo/rustfmt/clippy and bubblewrap. A worker PASS is only a
subset; it does not cover Rust or the full integration gate. For another
project, use that project's documented checks and record anything omitted.
Inspect both exit status and actual test results, including the per-selector
receipt counts when available. Reproduce the original bug with the new test
before treating the fix as verified.

Require an independent reviewer to examine the diff, test evidence, and
remaining limitations before merging. A reviewer may be another person or a
separate review agent; the author must not be the sole reviewer. Merge remains
a separate decision after required repository checks and approvals. This guide
does not automate merge or deployment.

## Interrupted or unresolved operations

If a tool's effect is uncertain, stop. Do not automatically retry or replay it,
or edit the journal to manufacture a settled state. Inspect workspace effects
and preserve the original journal. For an unresolved operation, start a new
session only after deciding what completed, and include that explicit context
in the new task. A completed, fully settled session can resume normally.
See the [session and recovery contract](v0.md#session-and-recovery).

This supervised task workflow is separate from the narrow, opt-in
[live provider acceptance check](live-acceptance.md). Passing either does not
prove general coding quality or unattended readiness.
