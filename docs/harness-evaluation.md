# Paired harness evaluation

`scripts/harness_eval.py` creates a frozen comparison schedule and aggregates
external execution receipts. It is an **offline planning and accounting tool**.
It does not launch Danso/Pi, install either runtime, enforce their budgets,
verify referenced evidence, or call a model. No performance result is implied
by generating a plan or passing its tests.

## Create the plan before any run

```sh
umask 077
python3 scripts/harness_eval.py plan examples/harness-eval/spec.json > /tmp/example-plan.json
```

The checked-in specification is an illustrative schema example. Its repeated
hex digests and revisions are placeholders, not actual tasks or verified builds.
Replace them before an experiment. Keep raw prompts, transcripts, credentials,
and outputs out of public reports.

A real specification fixes:

- Two named harnesses, exact source revisions and configuration SHA-256s.
  Use `piri` rather than `pi` if the executable is actually the downstream Piri
  distribution. Record the executable/build provenance in the evidence bundle.
- Provider, exact model, effort, request and elapsed-time limits, environment
  digest, cache policy and how the external runner enforces budgets.
- Each case's unchanged user prompt, initial workspace and independent acceptance
  check, represented by SHA-256 digests of their archived bytes. Define a stable
  archive format for directory inputs; do not hash unordered directory listings.
- Repetition count. Each case/repetition is a pair. The schedule alternates which
  harness goes first across cases and repetitions. Repeated cases are not
  independent task categories, and this is not a randomized statistical trial.

The plan SHA-256 binds all these fields and the schedule. A changed condition
requires a new plan and a new experiment. Preserve every scheduled attempt,
including failed and interrupted work. Do not rerun failures under their old job
ID or omit them. A separately declared retry experiment is a different dataset.

## External execution contract

For each job, start from a fresh copy of the input, isolated HOME/session state,
and the frozen settings. The harness's own system prompt/tools are part of the
intervention; the user task and acceptance criteria stay the same. Preserve
stdout/stderr, session journal, actual configuration/build provenance and the
host-owned acceptance result in a private evidence bundle. Do not let the agent
edit the acceptance oracle or treat its final answer as proof of success.

A paired live runner is **not implemented yet**. Before live measurement, it must:

- Verify pinned hashes against the real artifacts and enforce the common
  wall/request limits. Pi's controls are not assumed equivalent to Danso's.
- Account for every provider dispatch, retry and summary request. Count failed
  requests separately when their token usage is unavailable. Never substitute
  assistant-message count for provider request count.
- Supervise process groups and bound output, disk, descendants and cleanup;
  preserve uncertain journals without automatically replaying effects.
- Use the same authorized provider/model and document cache/routing differences.
  A cold local HOME does not prove a cold remote prompt cache.

Real model calls require an authorized bounded execution plan and credentials
through their existing private channel. Never place a key in the specification.

## Receipt format

`report` consumes a JSON array, one record per submitted job:

```json
{
  "job": "edit:1:danso",
  "plan_sha256": "<64 lowercase hex characters from the plan>",
  "status": "passed",
  "exit_code": 0,
  "acceptance_passed": true,
  "evidence_sha256": "<SHA-256 of the private evidence bundle>",
  "metrics": {
    "elapsed_seconds": 12.5,
    "requests": 3,
    "total_tokens": 1200,
    "compactions": 0,
    "repeated_tools": 0
  }
}
```

Statuses are `passed`, `failed`, `timeout`, `error`. Passing requires both exit
zero and an independent acceptance pass. A timeout/error remains unsuccessful
even if partial output looks correct. `null` is permitted for unknown exit,
acceptance and metrics; unknown measurements are never replaced with zero.
Use `failed` when a known nonzero exit or failed acceptance is present.

Metric definitions:

- `elapsed_seconds`: wall time from process launch to final reaping, including
  startup and termination. Use the same boundary for both harnesses.
- `requests`: all observed provider dispatches, including summaries and retries.
  Unknown counts must be null, not derived from the final transcript.
- `total_tokens`: total provider-reported input (including cached input) plus
  output, including summarization and reasoning where the provider counts it.
  If any request's usage is unavailable, use null for the whole run. Cache reads
  are already part of input in many APIs; do not double-count them.
- `compactions`: completed persisted checkpoints, not failed summary attempts.
- `repeated_tools`: externally audited redundant calls under a predeclared rule,
  such as rereading an unchanged file after compaction. Legitimate repeated tests
  are not automatically redundant. This tool does not infer intent from commands.

## Report without selection bias

```sh
python3 scripts/harness_eval.py report /tmp/example-plan.json /path/to/receipts.json
```

Output includes every planned job, missing IDs, submitted success rates and a
planned-run success lower bound. With no submissions, the submitted success
rate is null. Unknown metrics reduce each metric's own sample count. Failures
remain in all-submitted summaries; both-passed pairs are an explicitly separate,
selected cohort and cannot support an unconditional efficiency claim.

Pair deltas are **second harness minus first harness**, only for matched jobs
with both measurements. The report does not choose a winner, compute a claimed
speedup, establish causality, or perform significance testing. Incomplete plans,
missing measurements and observed budget overruns stay visible. Exit zero means
valid input/report generation, not that tasks passed or an experiment is complete.

The digests and receipts are caller claims, not cryptographic attestations of
execution. This tool validates internal consistency, not the authenticity of
measurements. All source evidence must remain available for independent audit.
Inputs are bounded regular JSON files (8 MiB each); duplicate keys and symlinked
path components are rejected. Plans whose actual rendered JSON would exceed
that same reader limit are rejected before emitting output, even if their case
and repetition counts fit the individual schema limits.
Output goes to stdout; use owner-only storage for
real evaluation artifacts.

## Verification

```sh
python3 scripts/test_harness_eval.py
```

Tests cover schedule integrity, missing/duplicate attempts, mixed outcomes,
unknown measurements, budget overruns, matched pair accounting and bounded
input handling. They run without an installed Pi or live credentials and are
part of the host and GitHub CI checks. They are not model-quality evaluations.


## Concrete task corpus and acceptance executor

`examples/harness-eval/cases/` now contains three small, real coding tasks:
ASCII slug normalization, quoted CSV integer aggregation, and resolving data
paths relative to configuration files. Each original implementation fails at
least one behavioral check. These cover short fixes only; they do not establish
long-context, resumption, or repository-scale performance.

```sh
python3 scripts/eval_case.py catalog
python3 scripts/eval_case.py prepare paths /private/experiment/workspace
# Run the separately supervised harness on that workspace using the case prompt.
python3 scripts/eval_case.py accept paths /private/experiment/workspace /private/experiment/acceptance-001
```

`catalog` produces real case digests suitable for `spec.cases`. The input digest
is SHA-256 of the UTF-8-file mapping serialized using `harness_eval.digest`:
JSON sorted keys, compact separators, default ASCII escaping, no trailing newline.
Prompt digests cover the exact UTF-8 string bytes. Acceptance digests bind the
entrypoint, vectors, runner source and limits; they must be regenerated when
those change. The task prompt and oracle stay outside the prepared workspace.
Tests are public benchmark vectors, not secret test security or a guarantee
against benchmark contamination.

The acceptance executor snapshots the stopped candidate into a fresh private
attempt directory, rejects symlinks/special files and caps file count/bytes,
then executes the Python CLI in a read-only bubblewrap workspace with networking,
capabilities and inherited environment disabled. Expected results remain in the
host process; candidate code cannot alter the oracle or its output decision.
Each check starts a fresh process with a 3-second wall bound, 64-KiB output caps,
CPU/address-space/file limits, and process-group cleanup. Bubblewrap's PID
namespace owns descendants; these are not aggregate cgroup memory/disk quotas.
Sandbox preflight failure is an infrastructure error, not a failed model task.

Exit codes are 0 (all checks pass), 1 (a behavioral check fails), 2 (input,
snapshot or infrastructure failure). Reports include the candidate digest,
check states and stdout/stderr hashes. Logs and immutable input copies remain
in the owner-only evidence directory; rerunning into an existing destination
is rejected. Interrupted attempts may be incomplete and must be retained and
classified by the experiment supervisor, never silently retried. Stop the
candidate harness before snapshotting; this is not a concurrent live-workspace
snapshot or a sandbox for a hostile local host user.

This is an **acceptance execution component**, not yet the paired live harness
runner. The next component must launch pinned Danso/Pi builds, supply matching
prompts/settings, enforce provider dispatch budgets including retries, and
produce job receipts linked to these acceptance artifacts. `accept` does not
call a model, calculate performance metrics or generate harness success claims.
Use the existing PR #27 planner/reporter only with genuine execution receipts.

Run `python3 scripts/test_eval_case.py` on a host with bubblewrap. Tests prove
all seeded defects fail, corrected solutions pass, and timeout/output/privacy/
path/permission gates work. They are included in CI and the full host checks.
