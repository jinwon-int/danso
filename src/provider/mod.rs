//! Adapters translate Pi-compatible history to one provider's wire protocol.
pub mod anthropic;
mod chatgpt;
mod chatgpt_auth;
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

pub struct ModelRequest<'a> {
    pub system: &'a str,
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

/// Responses are terminal Pi-compatible assistant messages. An adapter validates
/// its wire response before returning, bounds network I/O, and marks dispatch
/// in Usage only after local request validation. No session/tool side effects.
#[allow(async_fn_in_trait)]
pub trait Provider {
    fn validate_history(&self, messages: &[Value]) -> Result<()>;
    /// Exact serialized request size for production adapters. The default is
    /// suitable only for non-wire scripted providers.
    fn request_bytes(&self, request: &ModelRequest<'_>) -> Result<usize> {
        Ok(serde_json::to_vec(&serde_json::json!({"system":request.system,"messages":request.messages,"tools":request.tools}))?.len())
    }
    /// Configured output token cap (issue #69 A/B). Scripted providers
    /// report 0 (unknown); the runtime uses it only for the `max_tokens`
    /// length diagnosis.
    fn max_output_tokens(&self) -> u32 {
        0
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value>;
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
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        match self {
            Self::Anthropic(p) => p.complete(request, usage).await,
            Self::OpenAi(p) => p.complete(request, usage).await,
            Self::Glm(p) => p.complete(request, usage).await,
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
}
