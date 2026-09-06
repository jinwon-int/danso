# danso

Minimal Rust coding-agent harness.

`piri` is the TypeScript Pi distribution for ccc-node. `danso` is a sibling instrument: a small native harness that reuses Pi's **portable contracts**, not Pi's engine.

Not a port of [piri](https://github.com/jinwon-int/piri), [earendil-works/pi](https://github.com/earendil-works/pi), or [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust).

## Build and run

The first v0 implementation is a Linux headless loop with Anthropic Messages, OpenAI Responses and Z.AI GLM
Chat Completions adapters, default bubblewrap isolation, durable sessions, and bounded context.

```sh
cargo build --release --locked
mkdir -p "$HOME/.danso/sessions"
# Supply ANTHROPIC_API_KEY through your existing credential mechanism.
target/release/danso --cwd /path/to/repo --trust-project \
  --session "$HOME/.danso/sessions/task.jsonl" \
  --model YOUR_ANTHROPIC_MODEL -p 'Explain this repository'
```

Requires Rust 1.98.1 to build and `/usr/bin/bwrap` with user namespaces to run.
Without `-p`, stdout is JSONL. Reuse the session path to continue a completed
linear conversation. An uncertain interrupted tool requires manual recovery.

See [the v0 contract](docs/v0.md) for trust/discovery subsets, exit codes,
budgets, recovery behavior, fixture provenance and offline test commands.
The model adapter is mock-tested; live provider acceptance remains pending.
See [the opt-in live acceptance workflow](docs/live-acceptance.md) for the
scenario, offline verification and authorized execution command.
Choose GPT or GLM with `--provider openai` / `--provider glm`; see
[provider configuration and examples](docs/providers.md).

## Long tasks

Opt in to automatic context checkpoints with `--compact-at-bytes 196608`.
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

Inside a Danso coding worker, run `python3 scripts/dev_check.py --profile worker`
for the Python subset. On the host, run `python3 scripts/dev_check.py --profile host`
for all required Rust and sandbox integration checks. Worker success does not
replace the host gate; see [check environments](docs/architecture.md#development-check-environments).
