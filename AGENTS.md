# Repository development rules

Read `docs/architecture.md` before adding a feature. Keep changes at the owning
module boundary; `app.rs` is the composition root and `runtime.rs` is agent policy.

- Add providers through `Provider`; keep HTTP formats and credentials in adapters.
- Add builtin tools through `Tool` and the single `tools::builtins()` registry.
  Definitions and execution must never have separate name lists.
- Keep production tool execution behind `ToolExecutor`, including sandbox
  preflight, cancellation and resource limits. Do not invoke handlers in the loop.
- Preserve the `SessionStore` durable operation order and unresolved-call gate.
  Do not silently replay effects, repair journals, or acknowledge uncertain work.
- Add renderers through `EventSink`; output must not authorize or perform effects.
- Keep CLI/env/process concerns out of shared contracts and the runtime loop.
- Preserve the documented v0 four-tool surface unless the feature explicitly
  changes that scope. Avoid speculative plugin runtimes or unused abstractions.
- For a new extension, test it through the common runtime and relevant failure
  gates. Keep the Pi fixture and real-sandbox E2E tests passing.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  `cargo test --locked`, `cargo build --locked`, `python3 scripts/test_e2e.py`,
  `python3 scripts/test_compaction.py`, `python3 scripts/test_providers.py`, and `python3 scripts/test_live_acceptance.py`
  before a PR. Also run `python3 scripts/test_dev_check.py` and `python3 scripts/test_dev_check_host.py` and `python3 scripts/test_ccc_node.py`.
- Inside a Danso worker, use `python3 scripts/dev_check.py --profile worker`
  for the Python subset; Rust and nested sandbox integration are host checks.
  Report omitted checks explicitly. On the host, `python3 scripts/dev_check.py --profile host` runs the full required gate list; it never falls back to a subset.
- Tests use synthetic credentials and local providers. Do not turn tests into
  real model calls or fleet changes.

- `test_dev_check` contains worker-safe unit tests. `test_dev_check_host` requires
  the host toolchain/sandbox; do not run it or broad test discovery in a worker.
- Bash uses `pipefail`. Inspect test results, not just the tool success flag;
  later commands or explicit status handling can still mask earlier failures.
