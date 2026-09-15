//! Session and runner traits (docs/unified-design.md §4.2).
//!
//! The traits are synchronous: an implementation spawns its own thread or
//! task and hands back an event stream. This keeps the contract usable from
//! any executor and keeps `Send` requirements out of the core loop.
use crate::event::{AgentEvent, ApprovalDecision};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

/// Reasoning efforts accepted by the core CLI.
pub const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Prompt byte limit shared with the core context limit.
pub const MAX_PROMPT_BYTES: usize = danso::context::CONTEXT_LIMIT;

/// Provider-neutral inputs for starting or resuming a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRequest {
    pub working_directory: PathBuf,
    /// An exact journal identity to resume. `None` starts a new journal.
    pub session_id: Option<String>,
    /// Must equal the configured model when set; arbitrary models are refused.
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl SessionRequest {
    pub fn new(working_directory: impl Into<PathBuf>) -> Self {
        Self {
            working_directory: working_directory.into(),
            session_id: None,
            model: None,
            effort: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.working_directory.is_absolute(),
            "working directory must be an absolute path"
        );
        if let Some(id) = &self.session_id {
            ensure!(valid_session_id(id), "session id must be a lowercase UUID");
        }
        if let Some(model) = &self.model {
            ensure!(!model.trim().is_empty(), "model must not be empty");
        }
        if let Some(effort) = &self.effort {
            ensure!(
                EFFORTS.contains(&effort.as_str()),
                "invalid reasoning effort"
            );
        }
        Ok(())
    }
}

/// Journal identities are hyphenated lowercase UUIDs, exactly as `uuid::Uuid`
/// renders them; no other spelling names a journal.
pub fn valid_session_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub is_default: bool,
}

/// What a turn carries. A long-task resume takes no prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnInput {
    Prompt(String),
    ResumeTask,
}

impl TurnInput {
    pub fn validate(&self) -> Result<()> {
        if let Self::Prompt(prompt) = self {
            ensure!(!prompt.trim().is_empty(), "prompt must not be empty");
            ensure!(
                prompt.len() <= MAX_PROMPT_BYTES,
                "prompt exceeds {MAX_PROMPT_BYTES} bytes"
            );
        }
        Ok(())
    }
}

/// The request an approval handler decides on. Mirrors
/// [`AgentEvent::ApprovalRequest`].
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub action: String,
    pub arguments: Value,
    pub description: String,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Decides approval requests. The absence of a handler, a timeout and an
/// error are all `Deny`; implementations must not weaken that default.
pub trait ApprovalHandler: Send + Sync {
    fn decide(&self, request: &ApprovalRequest) -> BoxFuture<'_, ApprovalDecision>;
}

/// The fail-closed default.
pub struct DenyAll;

impl ApprovalHandler for DenyAll {
    fn decide(&self, _: &ApprovalRequest) -> BoxFuture<'_, ApprovalDecision> {
        Box::pin(async { ApprovalDecision::Deny })
    }
}

/// Events of one turn, ending with exactly one terminal event unless the
/// producer died, in which case the channel closes without one.
pub type EventStream = tokio::sync::mpsc::UnboundedReceiver<AgentEvent>;

/// A live session bound to one journal.
pub trait AgentSession: Send + Sync {
    /// Stable journal identity (a lowercase UUID).
    fn session_id(&self) -> &str;
    /// Start one turn. Refused while another turn on this session is active;
    /// the embedder's conversation queue serializes turns.
    fn send_turn(
        &self,
        input: TurnInput,
        approvals: Arc<dyn ApprovalHandler>,
    ) -> Result<EventStream>;
    /// Cancel in-flight work. Idle sessions ignore it. The journal is kept
    /// and never replayed automatically.
    fn interrupt(&self);
    /// Ask a long task to pause at its next settled boundary. Returns false
    /// when there is no active long-task turn.
    fn request_pause(&self) -> bool;
}

/// Factory and model discovery.
pub trait TurnRunner: Send + Sync {
    fn start_or_resume(&self, request: SessionRequest) -> Result<Box<dyn AgentSession>>;
    fn list_models(&self) -> Vec<ModelInfo>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_exact_lowercase_uuids() {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        assert!(valid_session_id(&id));
        assert!(!valid_session_id(&id.to_uppercase()));
        assert!(!valid_session_id(&id.replace('-', "")));
        assert!(!valid_session_id("../etc/passwd"));
        assert!(!valid_session_id(""));
    }

    #[test]
    fn request_and_input_validation_is_fail_closed() {
        let mut request = SessionRequest::new("/tmp/work");
        request.validate().unwrap();
        request.effort = Some("turbo".into());
        assert!(request.validate().is_err());
        request.effort = Some("high".into());
        request.session_id = Some("not-a-uuid".into());
        assert!(request.validate().is_err());
        assert!(SessionRequest::new("relative").validate().is_err());
        assert!(TurnInput::Prompt("  ".into()).validate().is_err());
        assert!(
            TurnInput::Prompt("x".repeat(MAX_PROMPT_BYTES + 1))
                .validate()
                .is_err()
        );
        TurnInput::ResumeTask.validate().unwrap();
    }

    #[tokio::test]
    async fn default_handler_denies() {
        let request = ApprovalRequest {
            request_id: "r".into(),
            action: "bash".into(),
            arguments: serde_json::json!({}),
            description: "run".into(),
        };
        assert_eq!(DenyAll.decide(&request).await, ApprovalDecision::Deny);
    }
}
