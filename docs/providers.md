# Model providers

Select the service with `--provider`; `--model` is always explicit. The original
Anthropic path remains the default. These adapters use the same runtime,
four builtin tools, sandbox, budgets, usage output and durable operation gates.
The original three paths are tested against local HTTP fixtures. Real account/model
acceptance remains pending; no API access is inferred from a model name.

| Provider | Wire API | Credential environment variable | Base URL environment variable | Default base (suffix appended) |
| --- | --- | --- | --- | --- |
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` | `DANSO_ANTHROPIC_BASE_URL` | `https://api.anthropic.com` (`/v1/messages`) |
| `openai` | OpenAI Responses | `OPENAI_API_KEY` | `DANSO_OPENAI_BASE_URL` | `https://api.openai.com/v1` (`/responses`) |
| `glm` | Z.AI Chat Completions | `ZAI_API_KEY` | `DANSO_GLM_BASE_URL` | preset, see "GLM profiles" below (`/chat/completions`) |

Supply credentials through your existing environment mechanism. The original API-key adapters do not read login files. The separate opt-in
`openai-codex` adapter below reads an explicitly selected Codex file; no adapter
automatically discovers credentials or infers OAuth flows.
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

## GLM profiles (issue #70 C)

`--glm-endpoint <general|coding>` (env `DANSO_GLM_ENDPOINT`, default
`general`) selects a documented preset: `general` →
`https://api.z.ai/api/paas/v4`, `coding` → `https://api.z.ai/api/coding/paas/v4`.
An explicit `DANSO_GLM_BASE_URL` wins, but when both are set and disagree the
invocation fails closed as a configuration error. `--glm-thinking
<enabled|disabled>` (env `DANSO_GLM_THINKING`, default `enabled`) toggles the
GLM `thinking` body field.

| Profile | Recommended invocation | Notes |
| --- | --- | --- |
| `glm-5.3-flash` | `scripts/danso-glm` (≡ `--provider glm --model glm-5.3-flash --glm-endpoint coding --reasoning-effort low --max-turns 48 --compact-at-bytes 131072 --provider-timeout-seconds 120`) | Fast lane used for the 2026-09-06 compaction measurement (docs/compaction.md). Apply `--repeat-limit 3` when it repeats identical reads. |
| `glm-5.3` | `--provider glm --model glm-5.3 --glm-endpoint general --reasoning-effort medium` | Thinking-enabled default profile. |

Context window and maximum output tokens are model/service claims that Danso
does not verify: out-of-range output caps surface as the provider's own 400
without automatic adjustment (issue #69 A). Confirm current values against
Z.AI's model documentation before sizing requests; the 2026-09-09 measurement
environment could not reach docs.z.ai to pin them here. The coding-plan quota
windows (5h/weekly) are operator-managed upstream. A 429 is retryable under
the bounded wire retry (issue #67 B); once retries are exhausted the run ends
with exit code 3 and the `DANSO_PROVIDER http_status=429` record is available
to the bridge for quota bookkeeping. Pass `--provider-retries 0` where a
single attempt is preferred.

`scripts/danso-glm` execs danso with the flash profile above; `ZAI_API_KEY`
must exist in the environment and `DANSO_GLM_MODEL` optionally swaps the
model. Extra arguments are forwarded, so a later `--model` in argv wins.

## Protocol details and limits

- OpenAI uses non-streaming Responses with `store: false`, the configured
  output-token cap (default 16384, `--max-output-tokens`; issue #69 A), and
  `include: ["reasoning.encrypted_content"]`. It exposes only local
  function tools. Tool schemas use `strict: false` to preserve optional builtin
  parameters. Hosted tools are not enabled.
- The complete supported OpenAI output sequence (message, function call,
  encrypted reasoning) is kept in the assistant message's `dansoOpenAIOutput`
  field. The adapter verifies its visible content/calls match the Pi transcript
  before resending it. Responses must be completed; unsupported output items,
  missing opaque reasoning, malformed calls and incomplete batches fail before
  any tool executes. An OpenAI session missing its preserved output fails closed.
- GLM uses non-streaming Chat Completions with the configured output-token cap
  (default 16384, `--max-output-tokens`) and
  `thinking: {type: "enabled"|"disabled", clear_thinking: false}`
  (`--glm-thinking`; issue #70 A). This targets the GLM-4.5+
  thinking/tool-capable protocol. Returned `reasoning_content` is stored in
  `dansoGlmReasoning` and forwarded verbatim. It is not rendered as final text.
  Tool arguments accept both JSON strings and the object form shown in Z.AI's
  API reference. Consistent `stop`/`tool_calls`/`length` finishes are accepted;
  a terminal `length` finish maps to the shared `max_tokens` diagnosis
  (issue #69 B).
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
- Requests are capped at 512 KiB, responses at 1 MiB, and HTTP transport at 180s by default.
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

## Provider transport diagnostics

All three API-key adapters use the bounded shared HTTP transport with a default
180-second total request deadline and a separate 10-second connection deadline.
The latter includes establishing the connection (DNS/TCP/TLS); it is not a
reason to retry automatically. The ChatGPT subscription adapter keeps its
explicit authentication flow while using the same selected request timeout.

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
body, or underlying exception text. Redirects, byte limits and usage accounting
remain unchanged; retryable failures follow the bounded wire retry
(issue #67 B, see docs/v0.md).

For a typed HTTP transport failure, the CLI also emits one optional body-free
record alongside the unchanged `DANSO_ERROR` record:

```text
DANSO_TRANSPORT={"version":1,"phase":"response_body","elapsed_ms":60001,"request_bytes":30502,"attempts":1}
```

Its exact keys are `version`, `phase`, `elapsed_ms`, `request_bytes`, and
`attempts` (HTTP attempts including the first; the integrations adapter
validates 1..8).
`phase` is one of `connect`, `before_response_headers`, or `response_body`;
the numeric fields are nonnegative bounded integers. The record is emitted only
for typed native HTTP transport failures, never for HTTP status or response
validation errors. The ccc-node adapter treats it as optional and ignores a
missing or invalid record while preserving the terminal category and no-replay
behavior.

On 2026-09-06, two direct diagnostic requests returned HTTP 200: a small control
request in 1.96s and a 30,502-byte action request reconstructed from the saved task
context in 10.24s. DNS/TCP/TLS each took under 0.06s; header waits were 1.85s and 10.15s.
The timed probes used HTTPS/1.1 and a 180s upper bound, and executed no returned
tool calls. They did not reproduce the earlier 60s timeout, so its cause remains
unconfirmed. The current 180s default is an owner-selected usability budget;
it is not a claim that those probes established a service-side latency fix.

## Request timeout override

`--provider-timeout-seconds 120` sets the total time for each provider HTTP
request, including response-body reads. It applies to Anthropic, OpenAI and GLM,
including checkpoint summarization and its one permitted format repair. The
default is 180 seconds; accepted values are 1..300. The connection limit
remains 10 seconds, capped by the shorter total when applicable.

This is separate from `--timeout-seconds`, which bounds the entire CLI run,
and `--tool-timeout-seconds`, which bounds each tool. A longer provider timeout
does not extend either limit or add retries. The option must be passed again
on resume; sessions do not persist runtime timeout configuration.

Example for a bounded GLM experiment: add `--provider-timeout-seconds 120
--timeout-seconds 600` to the normal invocation. This enables comparison, not
a claim that extending the timeout fixes service latency or task completion.

## ChatGPT subscription

`--provider openai-codex` keeps Danso's runtime, four tools, journal and execution
backend. It does not invoke Codex or Pi to perform tasks. No additional runtime
package is required. Initial login uses the official Codex device flow; the
operator must provide `DANSO_CHATGPT_AUTH_FILE` explicitly. The ordinary Codex file path stays read-only with no refresh. Explicitly adopted
Danso stores support bounded renewal as described below. No API-key fallback,
credential discovery, token printing or model-request retry occurs. Telegram
authentication selection remains a separate integration step.

```sh
# Existing isolated Codex login completed by the operator; use its auth.json.
export DANSO_CHATGPT_AUTH_FILE=/private/codex-login/auth.json
target/debug/danso --provider openai-codex --model gpt-6-astra \
  --reasoning-effort medium --cwd /path/to/repo --trust-project \
  --session /outside/repo/session.jsonl -p 'Explain this repository'
```

The file must be a regular, single-link, current-user-owned file up to 64 KiB;
its immediate parent and file must be owner-only. All path components reject
symlinks. The adapter reads only the explicitly selected credential source (and managed
store coordination files when explicitly adopted). For read-only mode its access token
must contain an account ID matching the saved account and an expiry more than
60 seconds in the future. Decoding JWT metadata is not signature validation;
the service authenticates the bearer. An expired token stops before model
requests: renew using Codex login in that isolated home and retry explicitly.
Authentication is re-read before every model request; a changed account stops
an in-progress run. Read-only mode sends no refresh token. Keep auth outside the
workspace; host-mode tools retain the invoking user's host permissions.

Requests go to `https://chatgpt.com/backend-api/codex/responses`, using the
subscription access token and account header, with `store:false` and SSE.
`DANSO_CHATGPT_BASE_URL` permits that exact production base or literal-loopback
HTTP test fixtures only; no arbitrary HTTPS token destinations. An explicit
loopback override authorizes the local fixture to receive credentials/content.
Never use a real login in fixture tests. Ordinary `OPENAI_API_KEY` and
`DANSO_OPENAI_BASE_URL` are ignored for this adapter.

Bytes through the terminal event are bounded to 1 MiB and the normal provider
timeout. The first validated `response.completed` or `response.done` response
ends the request, without waiting for the SSE connection to close. Failed,
incomplete, truncated or unknown events before completion fail before any
returned tool executes. Duplicate terminal events already in the received
buffer fail; future bytes after a terminal response are not consumed.
Completed `response.output_item.done` items are retained by output index. When
the terminal response has an empty or omitted output list, these complete items supply the
answer and opaque reasoning history. Reconstruction requires contiguous indices
from zero, and duplicate indices always fail. A nonempty terminal output list
remains authoritative and must agree with every retained completed item; it does
not require a separate done event for every terminal item.
The existing OpenAI output and opaque-reasoning validation still applies.
No streamed delta is executed. Usage reports `openai-codex` and
`openai-codex-responses`; terminal usage must be valid. The service's subscription
request does not use the Platform adapter's `max_output_tokens:4096`; this
adapter enforces byte, request deadline and run/turn limits, not a 4096-token
server output cap. Token-based dispatch evaluation must account for this
separately. Subscription model access/quality has not been verified by these
synthetic tests; an authenticated login alone proves neither inference access
nor availability of a particular model.

Implementation evidence (public source, not a general Platform API guarantee):
- OpenAI authentication: https://learn.chatgpt.com/docs/auth
- Codex file schema and auth management at
  https://github.com/openai/codex/tree/5ecb3afd1bf405149e2159bfda50093b0c1b5fab/codex-rs/login/src/auth
- Pi subscription transport reference at
  https://github.com/earendil-works/pi/blob/9767ba275f3e9a5ee0f5c5342249b629ab1b2282/packages/ai/src/api/openai-codex-responses.ts

Offline gate: `python3 scripts/test_chatgpt.py` (fake tokens, loopback SSE,
real native host tool execution and resume; no live requests).


### Optional managed token renewal

A dedicated isolated Codex login can be transferred to Danso with an explicit
local command. Stop every Codex CLI, IDE or app-server process using that login
home first. The command cannot revoke tokens already cached in another process;
quiescence is an operator precondition. Do not adopt a shared interactive login.
No login, token exchange or inference occurs during adoption.

```sh
# SOURCE is the isolated login's auth.json, not the normal shared Codex home.
target/debug/danso auth-adopt --source /private/isolated-codex/auth.json
export DANSO_CHATGPT_AUTH_FILE=/private/isolated-codex/danso-auth.json
# The usual --provider openai-codex run now uses managed renewal.
```

Adoption requires the private owner-only directory rules above, a file with
exact0600 permissions, and a refresh token. Managed store files and locks also
require exact0600 permissions. Under an exclusive lock it stages `danso-auth.json`, parks the
original as `codex-auth-imported-<uuid>.json`, and installs the managed file with
no overwrite. Both credentials are preserved locally; the original `auth.json`
name is retired so Codex no longer discovers it. Re-running adoption never
overwrites an existing managed store. The `danso-auth.json` basename is reserved
for adopted stores; a moved managed file is rejected. If `auth.json` is detected at inspection or after a refresh exchange,
Danso stops until ownership is resolved. The post-exchange check runs before
installing or using refreshed credentials; the pending marker remains on drift.
These checks cannot make a non-cooperating external writer obey the lock, so
the quiescent isolated-home precondition remains necessary. This prevents normal shared-file use,
not a malicious process running as the same OS user.

Each managed inspection and renewal reads under `.danso-auth.lock`. A concurrent
process fails with a body-free busy message rather than observing in-flight
credentials or rotating the token again. The caller may retry the run explicitly;
no provider request is automatically retried. Credentials with at most 60 seconds
remaining are renewed before the next model request. Fresh credentials do not
trigger auth traffic. Revoked tokens receiving HTTP401 during inference are not
blindly refreshed and replayed; explicit reauthentication is required.

The refresh endpoint is fixed to `https://auth.openai.com/oauth/token`. Loopback
fixtures use `<DANSO_CHATGPT_BASE_URL>/oauth/token`; arbitrary issuers and redirects
are forbidden. JSON refresh responses are bounded to64KiB; request/connect
limits follow `--provider-timeout-seconds` (connect max10seconds), within the
existing whole-run deadline. Auth exchange and model inference are separate
bounded requests; the per-request timeout is not a combined two-request budget.
Model usage counters exclude auth exchanges. A refreshed access token must retain
the same account and a usable expiry. A missing rotated refresh token preserves
the previous token, matching the official Codex refresh response contract; an
explicitly malformed token is rejected.

Before dispatching a refresh, `.danso-refresh-pending` is written and fsynced.
Transport errors, HTTP failures, invalid replies, account mismatch, cancellation
or local save errors leave it in place. Later runs then fail closed without
reusing the potentially consumed refresh token. The adapter does not infer that
a failed HTTP request is safe to repeat. Successful renewal writes/fsyncs the new
generation, parks the previous generation as `danso-auth-archive-<uuid>.json`,
installs the new file, then parks the pending marker as a completion receipt.
Files stay0600 under both permissive/restrictive umasks; path traversal uses
pinned directory descriptors and rejects symlinks/hardlinked files.

Replacement uses two no-overwrite renames, with a possible missing-current-file
gap on crash; it never exposes a partly written current file. A crash or fsync
failure can therefore stop the store. Original, staged and archived artifacts
are retained; there is no automatic repair, rollback, deletion or archive prune.
If adoption or renewal is interrupted, preserve the entire private directory
and inspect paths/permissions only. Reauthenticate into a **new isolated login
home**, then adopt that new home and explicitly select it. Do not restore an old
refresh token or delete a pending marker to force another exchange. Recovery
from an ambiguous remote rotation requires fresh authentication, not a guessed
local rollback. Operational adoption and rollout are separate from synthetic
fixture tests and require the operator-authorized scope.


HTTP status and ChatGPT SSE failures may additionally emit:

```text
DANSO_PROVIDER={"version":1,"reason":"http_status","http_status":429}
```

The exact keys are `version`, `reason`, and `http_status`. Reasons are the
closed native enum `http_status`, `invalid_json`, `response_too_large`,
`stream_ended`, `invalid_stream`, `unsupported_stream_event`, `response_failed`,
`response_incomplete`, and `response_error`. `http_status` is a non-2xx,
three-digit status accepted by reqwest (100..999), and is null for every other
reason. No response body, remote error code/message, URL, or credential is
copied into this record. `invalid_stream` includes malformed or inconsistent
SSE frames and terminal responses; `stream_ended` means no completed response
was present at a valid stream boundary or at `[DONE]`.

The adapter accepts exactly one valid record only with category `provider` and
exit code 3. Missing, malformed, duplicate, or inconsistent optional records
are ignored. Any reserved failure record on exit 0 is an adapter error.
Older binaries remain supported without the extra detail. Usage counts and
terminal failure behavior are unchanged; there is no automatic replay or new
retry. These fields describe the observed failure, not its underlying cause;
previous failures without this metadata cannot be diagnosed retroactively.
Auth-store errors and other response-processing failures may still carry only
the existing category.
