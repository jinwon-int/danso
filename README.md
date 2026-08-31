# danso

Minimal Rust coding-agent harness.

`piri` is the TypeScript Pi distribution for ccc-node. `danso` is a sibling instrument: a small native harness that reuses Pi's **portable contracts**, not Pi's engine.

Not a port of [piri](https://github.com/jinwon-int/piri), [earendil-works/pi](https://github.com/earendil-works/pi), or [pi_agent_rust](https://github.com/Dicklesworthstone/pi_agent_rust).

## Share with Pi

v0 aims to speak these contracts:

- Agent Skills (`SKILL.md`) and `AGENTS.md`
- Session JSONL v3
- `read` / `bash` / `edit` / `write`
- print-mode exit codes and a one-line usage summary for ccc-node

## Non-goals (v0)

- TypeScript `ExtensionAPI` / custom TUI extensions
- Syncing earendil Pi internals
- Replacing `piri` on the fleet

See [issues](https://github.com/jinwon-int/danso/issues) for the v0 slice.
