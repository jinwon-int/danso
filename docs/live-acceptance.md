# Live provider acceptance

This opt-in check covers the real provider acceptance gap for Anthropic, GPT and GLM.
It does not deploy Danso or access an existing project. Passing offline tests
is not evidence that a real provider has accepted a request.

## Prepare and verify offline

```sh
cargo build --locked
python3 scripts/test_providers.py
python3 scripts/test_live_acceptance.py
```

The offline test uses a local fake HTTP provider, synthetic credentials and real
bubblewrap. It exercises the same workflow and retains its private artifacts.
Run it separately from the normal E2E suite; it never requires a live API key.

## Authorized live run

After the operator chooses a service/model and authorizes the call, supply its
credential through the existing environment mechanism, then run from the
repository root:

```sh
# OPENAI_API_KEY supplied externally
python3 scripts/live_acceptance.py --live --provider openai \
  --model gpt-5.6-luna --reasoning-effort max

# ZAI_API_KEY supplied externally
python3 scripts/live_acceptance.py --live --provider glm --model YOUR_GLM_MODEL

# ANTHROPIC_API_KEY supplied externally (the original default)
python3 scripts/live_acceptance.py --live --model YOUR_ANTHROPIC_MODEL
```

See [provider configuration](providers.md) for API bases and supported options.
By default the command uses the selected service's default endpoint. A custom
endpoint requires an explicit `--base-url`; an ambient endpoint override alone
is rejected. That URL receives the selected credential and synthetic task.
The command does not locate/copy credentials from another harness. Only PATH,
an empty private HOME, and the selected provider's credential and explicit base
reach the harness. No project instructions are loaded. The key remains outside
the tool sandbox. `--binary` must name a trusted Danso build, since that process
receives the credential.

There are two sequential harness invocations, each capped at eight model turns
and 180 seconds; each tool has a ten-second wall timeout. This is at most sixteen
model requests, not a monetary spending cap. There is no automatic retry.
The second invocation runs only after the first passes. `costUsd=0` in usage is
an unknown-cost compatibility placeholder, not a free-call guarantee.

## Pass criteria

1. Read a deliberately broken shell addition function, change only subtraction
   to addition with `edit`, run exactly `bash test.sh`, and write `report.md`.
2. Check the exact corrected source, unchanged test script, exact report,
   successful results for all four tools and sandbox test success evidence.
   Enforce exactly two reads, one exact edit, one exact test command and one
   report write, in that order. The report must contain the prescribed fix and
   test-result sentence. Extra calls, including temporary test replacement, fail.
   Generated code is never executed by the verifier outside the sandbox.
3. Both usage prefixes agree and token totals are internally consistent.
4. Resume the same session and recall a random token supplied only in the first
   request. The original journal prefix and workspace stay unchanged; no new
   tool results appear and resumed usage counts exactly one response.

A fresh 0700 directory under the system temporary directory is printed before
execution. The journal, per-run stdout/stderr and `result.json` remain there;
files created by the verifier are 0600. No automatic cleanup, journal repair,
ACK or replay occurs. No shared mutable state is used. A failure stops the run;
inspect the private artifacts before deciding on a new run. Do not publish raw
logs without inspection. A model ignoring the prescribed edit or test format
fails acceptance even if an alternative implementation could be correct.

This is a narrow happy-path provider canary. Rate-limit, cancellation and
interruption recovery remain covered by offline E2E tests, not induced live.

For forced repeated compaction, add `--compact-at-bytes 16384`; see
[the stress workflow and its larger budgets](compaction.md#validation).

## Failure report

A retained `result.json` with `status: "failed"` includes `completed_runs`, the
number of invocations that passed **all** their own acceptance checks (0..2),
and a fixed `failure_stage` value:

| Stage | Meaning | Completed runs |
| --- | --- | --- |
| `first_run_execution` | First process launch, capture, or nonzero exit | 0 |
| `first_run_validation` | First output/usage/journal/workspace validation | 0 |
| `resume_execution` | Resumed process launch, capture, or nonzero exit | 1 |
| `resume_validation` | Resumed output/usage/context/workspace validation | 1 |
| `final_stress_validation` | Final checkpoint-count check or report persistence | 2 |

Receiving valid usage alone does not count a run as complete. Failure stops
continuation without retry; the passed report schema stays unchanged. The new
fields use local constants and counters, not raw exception text, provider output,
or credentials. Programmatic callers still receive the original exception when
failure-report persistence succeeds. If storage itself fails, a report may not
be available. Preflight/setup errors before the run reporting block may likewise
produce no `result.json`; the CLI continues to print a generic failure message.
