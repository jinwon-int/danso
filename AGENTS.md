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
  `cargo test --locked`, and `python3 scripts/test_e2e.py` before a PR.
- Tests use synthetic credentials and local providers. Do not turn tests into
  real model calls or fleet changes.
