//! Adapters translate Pi-compatible history to one provider's wire protocol.
pub mod anthropic;
pub mod glm;
mod http;
pub mod openai;
mod wire;

use crate::{contracts::ToolDefinition, usage::Usage};
use anyhow::Result;
use serde_json::Value;

pub struct ModelRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [Value],
    pub tools: &'a [ToolDefinition],
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
