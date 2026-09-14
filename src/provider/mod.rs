//! Adapters translate Pi-compatible history to one provider's wire protocol.
pub mod anthropic;
mod chatgpt;
mod chatgpt_auth;
pub mod http_diagnostic;
pub use chatgpt_auth::adopt as adopt_chatgpt_auth;
pub mod glm;
mod http;
pub mod openai;
// Experimental wire foundation only. Do not enable in production until bounded
// decoding, capability admission, durable replay and compaction are implemented.
#[cfg(test)]
mod image_pixels;
#[cfg(test)]
mod openai_image;
mod wire;

use crate::{contracts::ToolDefinition, usage::Usage};
use anyhow::Result;
use serde_json::Value;

/// Operational token-to-byte estimates used to size serialized requests.
/// Provider context windows are service claims; this table is deliberately
/// conservative and is updated independently of wire-format code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelTokenEstimate {
    pub provider: &'static str,
    /// A model prefix, or `*` for that provider's fallback.
    pub model_prefix: &'static str,
    pub context_window_tokens: usize,
    pub bytes_per_token: usize,
}

/// Portion of the advertised context window reserved for the serialized
/// request. The remainder covers output, provider framing and estimation
/// error; the exact byte count remains the final admission check.
pub const REQUEST_BUDGET_PERCENT: usize = 70;
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;
pub const DEFAULT_BYTES_PER_TOKEN: usize = 4;
pub const MAX_CONTEXT_WINDOW_TOKENS: usize = 1_000_000;

/// Convert an estimated context window into a conservative serialized-request
/// budget. The token estimate is floored before converting to bytes.
pub const fn request_budget_for_context(
    context_window_tokens: usize,
    bytes_per_token: usize,
) -> usize {
    let token_budget = context_window_tokens.saturating_mul(REQUEST_BUDGET_PERCENT) / 100;
    token_budget.saturating_mul(bytes_per_token)
}

pub const DEFAULT_REQUEST_BUDGET_BYTES: usize =
    request_budget_for_context(DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_BYTES_PER_TOKEN);
/// The largest budget represented by the table; compaction reserves its
/// managed-memory headroom below this value.
pub const MAX_REQUEST_BUDGET_BYTES: usize =
    request_budget_for_context(MAX_CONTEXT_WINDOW_TOKENS, DEFAULT_BYTES_PER_TOKEN);

/// Model/service estimates used by the native adapters. Specific prefixes must
/// precede provider fallbacks. `openai-codex` is separate because the
/// subscription transport has its own provider identity in usage records.
pub const MODEL_TOKEN_ESTIMATES: &[ModelTokenEstimate] = &[
    ModelTokenEstimate {
        provider: "anthropic",
        model_prefix: "claude-opus-4",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "anthropic",
        model_prefix: "claude-sonnet-4",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "anthropic",
        model_prefix: "claude-3-7-sonnet",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "anthropic",
        model_prefix: "claude-3-5-sonnet",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "anthropic",
        model_prefix: "*",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "gpt-4.1",
        context_window_tokens: 1_000_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "gpt-5",
        context_window_tokens: 400_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "o3",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "o4-mini",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "gpt-4o",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
    ModelTokenEstimate {
        provider: "openai",
        model_prefix: "*",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
    ModelTokenEstimate {
        provider: "openai-codex",
        model_prefix: "gpt-5",
        context_window_tokens: 400_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai-codex",
        model_prefix: "o3",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai-codex",
        model_prefix: "o4-mini",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "openai-codex",
        model_prefix: "*",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
    ModelTokenEstimate {
        provider: "glm",
        model_prefix: "glm-5.3-flash",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "glm",
        model_prefix: "glm-5.3",
        context_window_tokens: 200_000,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "glm",
        model_prefix: "glm-4.5",
        context_window_tokens: 131_072,
        bytes_per_token: 4,
    },
    ModelTokenEstimate {
        provider: "glm",
        model_prefix: "*",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
    ModelTokenEstimate {
        provider: "*",
        model_prefix: "*",
        context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
        bytes_per_token: DEFAULT_BYTES_PER_TOKEN,
    },
];

fn model_prefix_matches(prefix: &str, model: &str) -> bool {
    prefix == "*" || model.starts_with(prefix)
}

/// Return the first matching operational estimate. Unknown models use their
/// provider fallback; unknown providers use the conservative global fallback.
pub fn model_token_estimate(provider: &str, model: &str) -> ModelTokenEstimate {
    MODEL_TOKEN_ESTIMATES
        .iter()
        .find(|estimate| {
            (estimate.provider == provider || estimate.provider == "*")
                && model_prefix_matches(estimate.model_prefix, model)
        })
        .copied()
        .expect("the global provider/model fallback must be present")
}

/// Effective serialized-request budget for one provider/model pair.
pub fn effective_request_budget(provider: &str, model: &str) -> usize {
    let estimate = model_token_estimate(provider, model);
    request_budget_for_context(estimate.context_window_tokens, estimate.bytes_per_token)
}

/// Cache-friendly system split (issue #69 E). `stable` carries the base
/// instructions, discovery context, memory block and execution context —
/// byte-identical between consecutive requests unless a per-request memory
/// refresh re-resolved it after a compaction. `volatile` carries per-request
/// guidance; only it changes turn to turn.
#[derive(Clone, Copy, Debug)]
pub struct SystemParts<'a> {
    pub stable: &'a str,
    pub volatile: &'a str,
}

impl<'a> SystemParts<'a> {
    /// One stable block, no volatile tail (extraction, summarizer).
    pub fn single(text: &'a str) -> Self {
        Self {
            stable: text,
            volatile: "",
        }
    }
    /// How string-concat adapters (OpenAI/GLM) render the split.
    pub fn joined(&self) -> String {
        let mut joined = String::with_capacity(self.stable.len() + self.volatile.len() + 1);
        joined.push_str(self.stable);
        if !self.volatile.is_empty() {
            joined.push('\n');
            joined.push_str(self.volatile);
        }
        joined
    }
    /// Prefix test on the joined rendering; scripted providers use it to
    /// discriminate request kinds (agent vs summarizer).
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.joined().starts_with(prefix)
    }
}

impl std::fmt::Display for SystemParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.joined())
    }
}

pub struct ModelRequest<'a> {
    pub system: SystemParts<'a>,
    pub messages: &'a [Value],
    pub tools: &'a [ToolDefinition],
}

/// Output token cap (issue #69 A): one flag mapped onto Anthropic
/// `max_tokens`, OpenAI `max_output_tokens`, and GLM `max_tokens`.
/// Out-of-range values are configuration errors, never clamped.
pub const MAX_OUTPUT_TOKENS_DEFAULT: u32 = 16384;
pub const MAX_OUTPUT_TOKENS_MIN: u32 = 256;
pub const MAX_OUTPUT_TOKENS_MAX: u32 = 131072;

/// Resolve the output token cap from an explicit flag value (already
/// parsed), the `DANSO_MAX_OUTPUT_TOKENS` environment variable, or the
/// default. Fail closed on out-of-range or unparseable configuration.
pub fn resolve_max_output_tokens(explicit: Option<u32>) -> Result<u32> {
    let from_env = || -> Result<Option<u32>> {
        let Some(raw) = std::env::var_os("DANSO_MAX_OUTPUT_TOKENS") else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy().trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        let value: u32 = raw
            .parse()
            .map_err(|_| anyhow::anyhow!("DANSO_MAX_OUTPUT_TOKENS must be an integer"))?;
        Ok(Some(value))
    };
    let value = match explicit {
        Some(value) => value,
        None => from_env()?.unwrap_or(MAX_OUTPUT_TOKENS_DEFAULT),
    };
    anyhow::ensure!(
        (MAX_OUTPUT_TOKENS_MIN..=MAX_OUTPUT_TOKENS_MAX).contains(&value),
        "max output tokens must be {}..={}",
        MAX_OUTPUT_TOKENS_MIN,
        MAX_OUTPUT_TOKENS_MAX
    );
    Ok(value)
}

/// Bounded provider wire retries (issue #67 B): default 3, max 5, 0 disables.
pub const PROVIDER_RETRIES_DEFAULT: u32 = 3;
pub const PROVIDER_RETRIES_MAX: u32 = 5;

/// Resolve the retry budget from an explicit flag value, the
/// `DANSO_PROVIDER_RETRIES` environment variable, or the default. Fail
/// closed on out-of-range or unparseable configuration.
pub fn resolve_provider_retries(explicit: Option<u32>) -> Result<u32> {
    let from_env = || -> Result<Option<u32>> {
        let Some(raw) = std::env::var_os("DANSO_PROVIDER_RETRIES") else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy().trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        let value: u32 = raw
            .parse()
            .map_err(|_| anyhow::anyhow!("DANSO_PROVIDER_RETRIES must be an integer"))?;
        Ok(Some(value))
    };
    let value = match explicit {
        Some(value) => value,
        None => from_env()?.unwrap_or(PROVIDER_RETRIES_DEFAULT),
    };
    anyhow::ensure!(
        value <= PROVIDER_RETRIES_MAX,
        "provider retries must be 0..={PROVIDER_RETRIES_MAX}"
    );
    Ok(value)
}

/// Responses are terminal Pi-compatible assistant messages. An adapter validates
/// its wire response before returning, bounds network I/O, and marks dispatch
/// in Usage only after local request validation. No session/tool side effects.
#[allow(async_fn_in_trait)]
pub trait Provider {
    fn validate_history(&self, messages: &[Value]) -> Result<()>;
    /// Exact serialized request size for production adapters. The default is
    /// suitable only for non-wire scripted providers.
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        Ok(serde_json::to_vec(&serde_json::json!({"system":request.system.joined(),"messages":request.messages,"tools":request.tools}))?.len())
    }
    /// Provider/model-derived serialized-request budget. Scripted providers
    /// use the conservative fallback; production adapters override it with
    /// the estimate selected during construction.
    fn request_budget_bytes(&self) -> usize {
        DEFAULT_REQUEST_BUDGET_BYTES
    }
    /// Configured output token cap (issue #69 A/B). Scripted providers
    /// report 0 (unknown); the runtime uses it only for the `max_tokens`
    /// length diagnosis.
    fn max_output_tokens(&self) -> u32 {
        0
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value>;
    /// Complete a request while forwarding validated text deltas to the
    /// caller. Deltas are notifications only: the returned value remains the
    /// sole message eligible for persistence and tool dispatch.
    async fn complete_streaming(
        &mut self,
        request: ModelRequest<'_>,
        usage: &mut Usage,
        _on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        self.complete(request, usage).await
    }
}

/// Runtime selection without introducing provider-specific branches in the loop.
pub enum Selected {
    Anthropic(anthropic::Anthropic),
    OpenAi(openai::OpenAi),
    Glm(glm::Glm),
}
impl Provider for Selected {
    fn validate_history(&self, messages: &[Value]) -> Result<()> {
        match self {
            Self::Anthropic(p) => p.validate_history(messages),
            Self::OpenAi(p) => p.validate_history(messages),
            Self::Glm(p) => p.validate_history(messages),
        }
    }
    fn max_output_tokens(&self) -> u32 {
        match self {
            Self::Anthropic(p) => p.max_output_tokens(),
            Self::OpenAi(p) => p.max_output_tokens(),
            Self::Glm(p) => p.max_output_tokens(),
        }
    }
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        match self {
            Self::Anthropic(p) => p.request_bytes(request),
            Self::OpenAi(p) => p.request_bytes(request),
            Self::Glm(p) => p.request_bytes(request),
        }
    }
    fn request_budget_bytes(&self) -> usize {
        match self {
            Self::Anthropic(p) => p.request_budget_bytes(),
            Self::OpenAi(p) => p.request_budget_bytes(),
            Self::Glm(p) => p.request_budget_bytes(),
        }
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        match self {
            Self::Anthropic(p) => p.complete(request, usage).await,
            Self::OpenAi(p) => p.complete(request, usage).await,
            Self::Glm(p) => p.complete(request, usage).await,
        }
    }
    async fn complete_streaming(
        &mut self,
        request: ModelRequest<'_>,
        usage: &mut Usage,
        on_delta: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Value> {
        match self {
            Self::Anthropic(p) => p.complete_streaming(request, usage, on_delta).await,
            Self::OpenAi(p) => p.complete_streaming(request, usage, on_delta).await,
            Self::Glm(p) => p.complete_streaming(request, usage, on_delta).await,
        }
    }
}

impl Selected {
    /// Bounded wire-retry budget (issue #67 B); 0 disables.
    pub fn set_retries(&mut self, retries: u32) {
        match self {
            Self::Anthropic(p) => p.set_retries(retries),
            Self::OpenAi(p) => p.set_retries(retries),
            Self::Glm(p) => p.set_retries(retries),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flag > env > default, fail-closed on garbage or out-of-range. One
    /// test owns the process-global environment variable; the unsafe blocks
    /// are the only sanctioned way tests mutate it.
    #[test]
    fn output_cap_resolution_is_explicit_and_fails_closed() {
        let read = |explicit| resolve_max_output_tokens(explicit);
        unsafe {
            std::env::remove_var("DANSO_MAX_OUTPUT_TOKENS");
        }
        assert_eq!(read(None).unwrap(), MAX_OUTPUT_TOKENS_DEFAULT);
        assert_eq!(read(Some(4096)).unwrap(), 4096);
        unsafe {
            std::env::set_var("DANSO_MAX_OUTPUT_TOKENS", "8192");
        }
        assert_eq!(read(None).unwrap(), 8192);
        // Explicit flag wins over the environment.
        assert_eq!(read(Some(4096)).unwrap(), 4096);
        // Fail closed: out of range, unparseable, then restore.
        unsafe {
            std::env::set_var("DANSO_MAX_OUTPUT_TOKENS", "999999999");
        }
        assert!(read(None).is_err());
        unsafe {
            std::env::set_var("DANSO_MAX_OUTPUT_TOKENS", "not-a-number");
        }
        assert!(read(None).is_err());
        assert!(read(Some(255)).is_err());
        assert!(read(Some(MAX_OUTPUT_TOKENS_MAX + 1)).is_err());
        unsafe {
            std::env::remove_var("DANSO_MAX_OUTPUT_TOKENS");
        }
        assert_eq!(read(None).unwrap(), MAX_OUTPUT_TOKENS_DEFAULT);
    }

    #[test]
    fn model_budget_table_uses_provider_fallbacks_and_exact_headroom_inputs() {
        let fallback = effective_request_budget("glm", "fixture");
        assert_eq!(fallback, DEFAULT_REQUEST_BUDGET_BYTES);
        assert_eq!(
            effective_request_budget("glm", "glm-5.3-flash"),
            request_budget_for_context(200_000, 4)
        );
        assert_eq!(
            effective_request_budget("openai", "gpt-4.1-mini"),
            request_budget_for_context(1_000_000, 4)
        );
        assert_eq!(
            effective_request_budget("unlisted-provider", "unlisted-model"),
            DEFAULT_REQUEST_BUDGET_BYTES
        );
        assert!(effective_request_budget("glm", "glm-5.3-flash") > fallback);
    }
}
