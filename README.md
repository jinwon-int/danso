# danso

Minimal Rust coding-agent harness.

`piri` is the TypeScript Pi distribution for ccc-node. `danso` is a sibling instrument: a small native harness that reuses Pi's **portable contracts**, not Pi's engine.

Not a port of [piri](https://github.com/jinwon-int/piri), [earendil-works/pi](https://github.com/earendil-works/pi), or [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust).

## Build and run

The first v0 implementation is a Linux headless loop with Anthropic Messages, OpenAI Responses and Z.AI GLM
Chat Completions adapters, default host execution with optional bubblewrap isolation, durable sessions, and bounded context.

```sh
cargo build --release --locked
mkdir -p "$HOME/.danso/sessions"
# Supply ANTHROPIC_API_KEY through your existing credential mechanism.
target/release/danso --cwd /path/to/repo --trust-project \
  --session "$HOME/.danso/sessions/task.jsonl" \
  --model YOUR_ANTHROPIC_MODEL -p 'Explain this repository'
```

Requires Rust 1.98.1 to build. Running the binary requires Linux 5.3+ with
procfs/pidfd support and `/bin/bash`; no bubblewrap, Python, Node or Docker is
needed for the default CLI. Host tools use the current user’s
filesystem/network permissions with a cleared, native HOME-based development
environment; environment clearing is not protection from readable host
credentials. Host tool limits are intentionally larger for development work,
while bubblewrap keeps the original restrictive limits. Use `--sandbox
bubblewrap` to require `/usr/bin/bwrap` and usable user namespaces;
isolation failure never falls back to host execution. See [execution modes](docs/execution.md).
Without `-p`, stdout is JSONL. Reuse the session path to continue a completed
linear conversation. An uncertain interrupted tool requires manual recovery.

See [the v0 contract](docs/v0.md) for trust/discovery subsets, exit codes,
budgets, recovery behavior, fixture provenance and offline test commands.
The model adapter is mock-tested; live provider acceptance remains pending.
See [the opt-in live acceptance workflow](docs/live-acceptance.md) for the
scenario, offline verification and authorized execution command.
Choose GPT or GLM with `--provider openai` / `--provider glm`; see
[provider configuration and examples](docs/providers.md).

## Supervised first use

For a guided single-run walkthrough on one small real task — bounded scope,
predefined acceptance, isolated worktree, explicit budgets and review — see
[supervised first use](docs/supervised-work.md).

For Telegram/client tool lifecycle updates, see [durable progress](docs/progress.md).

## Long tasks

For work that needs multiple bounded invocations, opt in with `--long-task`.
It persists request/token budgets and settled tool-stage checkpoints, supports
an explicit `--resume-task`, and keeps uncertain provider/tool work
non-resumable. The mode is capped at six hours of active execution; ordinary
runs retain their short defaults. Use `--task-status` for a provider-free
read-only status projection and `--task-progress` for body-free checkpoint
notifications. See [the v0 long-task contract](docs/v0.md#optional-long-task-mode).

Context compaction remains separately opt-in with `--compact-at-bytes 196608`.
Compacted sessions retain the original journal and resume without replaying
completed tools. See [compaction and recovery](docs/compaction.md) for limits,
summary semantics and the offline/live stress workflow.

## Extending Danso

The agent loop uses replaceable provider, tool executor, session store and
output interfaces. Each builtin tool owns its schema and handler and is
registered once. See [the architecture and extension recipes](docs/architecture.md)
and [the executable extension example](tests/extensibility.rs) before adding
features. Production v0 still exposes exactly four tools.

## Share with Pi

v0 implements these portable contracts within the documented subset:

- Agent Skills (`SKILL.md`) and `AGENTS.md`
- Session JSONL v3
- `read` / `bash` / `edit` / `write`
- print-mode exit codes and a one-line usage summary for ccc-node

## Non-goals (v0)

- TypeScript `ExtensionAPI` / custom TUI extensions
- Syncing earendil Pi internals
- Replacing `piri` on the fleet

See [issues](https://github.com/jinwon-int/danso/issues) for the v0 slice.

## Development checks

For frozen Danso/Pi comparison plans and honest paired result accounting, see
[harness evaluation](docs/harness-evaluation.md). The offline tool does not run
agents or claim a measured performance advantage.

Inside a restricted Danso coding worker or bubblewrap tool, run
`python3 scripts/dev_check.py --profile worker` for the Python subset; it cannot
validate Rust or nested sandbox integration. On a configured host development
backend with the Rust toolchain, run `python3 scripts/dev_check.py --profile host`
for all required Rust and sandbox integration checks. Worker success does not
replace the host gate; see [check environments](docs/architecture.md#development-check-environments).
