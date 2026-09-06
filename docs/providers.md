# Model providers

Select the service with `--provider`; `--model` is always explicit. The original
Anthropic path remains the default. These adapters use the same runtime,
four builtin tools, sandbox, budgets, usage output and durable operation gates.
All three paths are tested against local HTTP fixtures. Real account/model
acceptance remains pending; no API access is inferred from a model name.

| Provider | Wire API | Credential environment variable | Base URL environment variable | Default base (suffix appended) |
| --- | --- | --- | --- | --- |
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` | `DANSO_ANTHROPIC_BASE_URL` | `https://api.anthropic.com` (`/v1/messages`) |
| `openai` | OpenAI Responses | `OPENAI_API_KEY` | `DANSO_OPENAI_BASE_URL` | `https://api.openai.com/v1` (`/responses`) |
| `glm` | Z.AI Chat Completions | `ZAI_API_KEY` | `DANSO_GLM_BASE_URL` | `https://api.z.ai/api/paas/v4` (`/chat/completions`) |

Supply credentials through your existing environment mechanism. Danso does not
read Codex/Claude login files, reuse plan credentials, or infer OAuth flows.
Changing a base URL authorizes that destination to receive the corresponding
key and task content. Use an API base, not the complete method URL. GPT/GLM bases
reject URL credentials, query strings and fragments; only HTTPS or literal
loopback HTTP is accepted. Redirects and automatic retries are disabled.
A GLM Coding Plan or regional service may require a different documented base
and credential; choose them explicitly, never infer them from the model name.

Example invocations (session parent must already exist and be outside the
workspace):

```sh
# OPENAI_API_KEY supplied externally; model must be available to the account.
target/debug/danso --provider openai --model gpt-5.6-luna \
  --reasoning-effort max --cwd /path/to/repo --trust-project \
  --session /path/to/sessions/gpt.jsonl -p 'Explain this repository'

# ZAI_API_KEY supplied externally; choose a supported tool-capable GLM model.
target/debug/danso --provider glm --model YOUR_GLM_MODEL \
  --cwd /path/to/repo --trust-project \
  --session /path/to/sessions/glm.jsonl -p 'Explain this repository'
```

## Protocol details and limits

- OpenAI uses non-streaming Responses with `store: false`, a 4096 output-token
  budget, and `include: ["reasoning.encrypted_content"]`. It exposes only local
  function tools. Tool schemas use `strict: false` to preserve optional builtin
  parameters. Hosted tools are not enabled.
- The complete supported OpenAI output sequence (message, function call,
  encrypted reasoning) is kept in the assistant message's `dansoOpenAIOutput`
  field. The adapter verifies its visible content/calls match the Pi transcript
  before resending it. Responses must be completed; unsupported output items,
  missing opaque reasoning, malformed calls and incomplete batches fail before
  any tool executes. An OpenAI session missing its preserved output fails closed.
- GLM uses non-streaming Chat Completions with a 4096 output-token budget and
  `thinking: {type: "enabled", clear_thinking: false}`. This targets the GLM-4.5+
  thinking/tool-capable protocol. Returned `reasoning_content` is stored in
  `dansoGlmReasoning` and forwarded verbatim. It is not rendered as final text.
  Tool arguments accept both JSON strings and the object form shown in Z.AI's
  API reference. Only consistent `stop`/`tool_calls` finishes are accepted.
- `--reasoning-effort` is optional for OpenAI/GLM and otherwise leaves the service
  default intact. Accepted spellings are `none`, `minimal`, `low`, `medium`,
  `high`, `xhigh`, `max`; actual support depends on the chosen model/service.
  An unsupported combination fails without fallback or retry. Anthropic rejects
  this option because its current adapter does not implement thinking.
- Resume with the same service/model to retain provider-specific reasoning
  semantics. Other providers receive only the portable text/tool context;
  cross-provider reasoning migration and automatic failover are not implemented.
  The new metadata is a Danso extension; this is not a claim of complete Pi
  reasoning interchange compatibility.
- Input usage includes cached tokens in the upstream APIs. Danso subtracts the
  cache portion from normalized `inputTokens`, counts it in `cacheReadTokens`,
  and keeps `totalTokens` free of double counting. Per-response and cumulative
  arithmetic are checked; overflow fails without changing the last valid summary. Cost remains unknown (zero
  solely for Piri schema compatibility).
- Requests are capped at 512 KiB, responses at 1 MiB, and HTTP transport at 60s by default.
  The CLI run/turn/tool limits still apply. Large reasoning histories can reach
  the byte cap; opt-in [context compaction](compaction.md) can summarize portable evidence
  and start a fresh provider reasoning context while preserving the journal.

## Offline verification

```sh
cargo build --locked
python3 scripts/test_e2e.py
python3 scripts/test_providers.py
python3 scripts/test_live_acceptance.py
```

Provider fixtures cover auth/paths, all four tools, preserved reasoning, usage,
resume without replay, malformed/incomplete batches, duplicate call IDs,
response byte caps, HTTP failures, redirect rejection and pre-dispatch config/
history errors. See [live acceptance](live-acceptance.md) for the separate
operator-invoked canary.

Protocol references checked for this implementation:

- [OpenAI official function calling guide](https://developers.openai.com/api/docs/guides/function-calling)
- [OpenAI Responses create reference](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Z.AI Chat Completion reference](https://docs.z.ai/api-reference/llm/chat-completion)

## OpenAI/GLM transport diagnostics

The shared Bearer transport uses a default 60-second total request deadline and a
separate 10-second connection deadline. The latter includes establishing the
connection (DNS/TCP/TLS); it is not a reason to retry automatically. Anthropic
uses its separate adapter and is not changed by this policy.

Transport failures now include only static labels and measured numbers:

```text
provider request timed out: phase=before_response_headers elapsed_ms=60001 request_bytes=30502
```

- `connect`: reqwest classified the failure as connection establishment.
- `before_response_headers`: failure before complete headers without a
  connection classification; this label alone does not prove server slowness.
- `response_body`: headers arrived, but reading the body failed or timed out.

`elapsed_ms` is measured from dispatch with a monotonic clock; `request_bytes`
is the serialized JSON body length. Errors contain no URL, key, request/response
body, or underlying exception text. Redirects, byte limits, usage accounting
and the no-retry policy remain unchanged.

On 2026-09-06, two direct diagnostic requests returned HTTP 200: a small control
request in 1.96s and a 30,502-byte action request reconstructed from the saved task
context in 10.24s. DNS/TCP/TLS each took under 0.06s; header waits were 1.85s and 10.15s.
The timed probes used HTTPS/1.1 and a 180s upper bound, and executed no returned
tool calls. They did not reproduce the earlier 60s timeout, so its cause remains
unconfirmed; these observations do not justify increasing the default deadline.

## Request timeout override

`--provider-timeout-seconds 120` sets the total time for each provider HTTP
request, including response-body reads. It applies to Anthropic, OpenAI and GLM,
including checkpoint summarization and its one permitted format repair. The
default remains 60 seconds; accepted values are 1..300. The OpenAI/GLM connection
limit remains 10 seconds, capped by the shorter total when applicable.

This is separate from `--timeout-seconds`, which bounds the entire CLI run,
and `--tool-timeout-seconds`, which bounds each tool. A longer provider timeout
does not extend either limit or add retries. The option must be passed again
on resume; sessions do not persist runtime timeout configuration.

Example for a bounded GLM experiment: add `--provider-timeout-seconds 120
--timeout-seconds 600` to the normal invocation. This enables comparison, not
a claim that extending the timeout fixes service latency or task completion.
