//! The normalized event vocabulary (docs/unified-design.md §4.1).
//!
//! Every variant validates on construction so a consumer never has to
//! defend against an empty delta, a nameless tool, or an unbounded counter.
//! Events are body-free by default: tool arguments and results are `None`
//! unless a policy explicitly allows their display.
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;

/// Closed tool vocabulary for progress display. Custom executor tools map to
/// `Other`, matching the `--progress-jsonl` contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    Read,
    Write,
    Edit,
    Bash,
    Other,
}

impl ToolName {
    pub fn classify(name: &str) -> Self {
        match name {
            "read" => Self::Read,
            "write" => Self::Write,
            "edit" => Self::Edit,
            "bash" => Self::Bash,
            _ => Self::Other,
        }
    }
}

/// Long-task checkpoint states, mirroring `DANSO_TASK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Checkpoint,
    Paused,
    Completed,
    Blocked,
}

impl TaskState {
    pub fn parse(state: &str) -> Option<Self> {
        match state {
            "checkpoint" => Some(Self::Checkpoint),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Closed error vocabulary: the core failure categories plus the embedder
/// outcomes that have no CLI exit code of their own. Provider prose, URLs
/// and credentials never enter an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Configuration,
    Session,
    Sandbox,
    Provider,
    ProviderTimeout,
    Compaction,
    RequestBudget,
    Output,
    Runtime,
    RunTimeout,
    Interrupted,
    Memory,
    /// The embedder cancelled the turn; the journal is retained and never
    /// replayed automatically.
    Cancelled,
    /// The turn input was rejected before any provider request.
    Input,
    /// A long task cannot be resumed safely from its saved state.
    TaskResumeUnavailable,
    /// A long task paused at a settled checkpoint and can be resumed.
    TaskPaused,
}

impl From<danso::failure::Kind> for ErrorCode {
    fn from(kind: danso::failure::Kind) -> Self {
        use danso::failure::Kind;
        match kind {
            Kind::Configuration => Self::Configuration,
            Kind::Session => Self::Session,
            Kind::Sandbox => Self::Sandbox,
            Kind::Provider => Self::Provider,
            Kind::ProviderTimeout => Self::ProviderTimeout,
            Kind::Compaction => Self::Compaction,
            Kind::RequestBudget => Self::RequestBudget,
            Kind::Output => Self::Output,
            Kind::Runtime => Self::Runtime,
            Kind::RunTimeout => Self::RunTimeout,
            Kind::Interrupted => Self::Interrupted,
            Kind::Memory => Self::Memory,
        }
    }
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Session => "session",
            Self::Sandbox => "sandbox",
            Self::Provider => "provider",
            Self::ProviderTimeout => "provider_timeout",
            Self::Compaction => "compaction",
            Self::RequestBudget => "request_budget",
            Self::Output => "output",
            Self::Runtime => "runtime",
            Self::RunTimeout => "run_timeout",
            Self::Interrupted => "interrupted",
            Self::Memory => "memory",
            Self::Cancelled => "cancelled",
            Self::Input => "input",
            Self::TaskResumeUnavailable => "task_resume_unavailable",
            Self::TaskPaused => "task_paused",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A non-empty increment of user-visible assistant text.
    TextDelta { text: String },
    /// One user-visible assistant message finished within the turn.
    MessageCompleted,
    /// Provider reasoning; never delivered to a user surface.
    ReasoningDelta { text: String },
    ToolStarted {
        call_id: String,
        tool: ToolName,
        arguments: Option<Value>,
    },
    ToolCompleted {
        call_id: String,
        tool: ToolName,
        success: bool,
        result: Option<Value>,
    },
    ApprovalRequest {
        request_id: String,
        action: String,
        arguments: Value,
        description: String,
    },
    ApprovalResolved {
        request_id: String,
        action: String,
        decision: ApprovalDecision,
    },
    /// Body-free long-task checkpoint counters.
    TaskProgress {
        state: TaskState,
        stage: u64,
        requests: u64,
        reported_tokens: u64,
        elapsed_seconds: u64,
    },
    /// The provider finished the turn.
    Completion { stop_reason: String },
    /// The normalized result of a successful turn.
    Result { result: Value },
    /// Terminal normalized failure.
    Error {
        code: ErrorCode,
        message: String,
        retryable: bool,
    },
}

impl AgentEvent {
    pub fn text_delta(text: impl Into<String>) -> Result<Self> {
        let text = text.into();
        ensure!(!text.is_empty(), "text delta must not be empty");
        Ok(Self::TextDelta { text })
    }

    pub fn reasoning_delta(text: impl Into<String>) -> Result<Self> {
        let text = text.into();
        ensure!(!text.is_empty(), "reasoning delta must not be empty");
        Ok(Self::ReasoningDelta { text })
    }

    pub fn tool_started(
        call_id: impl Into<String>,
        tool: ToolName,
        arguments: Option<Value>,
    ) -> Result<Self> {
        let call_id = call_id.into();
        ensure!(!call_id.is_empty(), "tool call id must not be empty");
        Ok(Self::ToolStarted {
            call_id,
            tool,
            arguments,
        })
    }

    pub fn tool_completed(
        call_id: impl Into<String>,
        tool: ToolName,
        success: bool,
        result: Option<Value>,
    ) -> Result<Self> {
        let call_id = call_id.into();
        ensure!(!call_id.is_empty(), "tool call id must not be empty");
        Ok(Self::ToolCompleted {
            call_id,
            tool,
            success,
            result,
        })
    }

    pub fn approval_request(
        request_id: impl Into<String>,
        action: impl Into<String>,
        arguments: Value,
        description: impl Into<String>,
    ) -> Result<Self> {
        let request_id = request_id.into();
        let action = action.into();
        let description = description.into();
        ensure!(
            !request_id.is_empty(),
            "approval request id must not be empty"
        );
        ensure!(!action.is_empty(), "approval action must not be empty");
        ensure!(
            !description.is_empty(),
            "approval description must not be empty"
        );
        ensure!(
            arguments.is_object(),
            "approval arguments must be an object"
        );
        Ok(Self::ApprovalRequest {
            request_id,
            action,
            arguments,
            description,
        })
    }

    pub fn approval_resolved(
        request_id: impl Into<String>,
        action: impl Into<String>,
        decision: ApprovalDecision,
    ) -> Result<Self> {
        let request_id = request_id.into();
        let action = action.into();
        ensure!(
            !request_id.is_empty(),
            "approval request id must not be empty"
        );
        ensure!(!action.is_empty(), "approval action must not be empty");
        Ok(Self::ApprovalResolved {
            request_id,
            action,
            decision,
        })
    }

    /// Parse the body-free `DANSO_TASK` record. Unknown keys, non-integer
    /// counters and unknown states are rejected.
    pub fn task_progress(record: &Value) -> Result<Self> {
        let object = record
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("task progress must be an object"))?;
        const KEYS: [&str; 6] = [
            "version",
            "state",
            "stage",
            "requests",
            "reported_tokens",
            "elapsed_seconds",
        ];
        ensure!(
            object.len() == KEYS.len() && KEYS.iter().all(|key| object.contains_key(*key)),
            "task progress has unexpected keys"
        );
        ensure!(object["version"] == 1, "unsupported task progress version");
        let state = object["state"]
            .as_str()
            .and_then(TaskState::parse)
            .ok_or_else(|| anyhow::anyhow!("unknown task progress state"))?;
        let counter = |key: &str| {
            object[key]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("task progress {key} must be a u64"))
        };
        Ok(Self::TaskProgress {
            state,
            stage: counter("stage")?,
            requests: counter("requests")?,
            reported_tokens: counter("reported_tokens")?,
            elapsed_seconds: counter("elapsed_seconds")?,
        })
    }

    pub fn completion(stop_reason: impl Into<String>) -> Result<Self> {
        let stop_reason = stop_reason.into();
        ensure!(!stop_reason.is_empty(), "stop reason must not be empty");
        Ok(Self::Completion { stop_reason })
    }

    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    /// True for the variants that end a turn's event stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completion { .. } | Self::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn constructors_fail_closed_on_empty_identity() {
        assert!(AgentEvent::text_delta("").is_err());
        assert!(AgentEvent::reasoning_delta("").is_err());
        assert!(AgentEvent::tool_started("", ToolName::Bash, None).is_err());
        assert!(AgentEvent::tool_completed("", ToolName::Bash, true, None).is_err());
        assert!(AgentEvent::approval_request("", "bash", json!({}), "run").is_err());
        assert!(AgentEvent::approval_request("r", "bash", json!([]), "run").is_err());
        assert!(AgentEvent::approval_resolved("r", "", ApprovalDecision::Deny).is_err());
        assert!(AgentEvent::completion("").is_err());
        assert!(AgentEvent::text_delta("x").is_ok());
    }

    #[test]
    fn task_progress_parses_exact_record_only() {
        let ok = json!({"version":1,"state":"paused","stage":2,"requests":3,"reported_tokens":4,"elapsed_seconds":5});
        assert_eq!(
            AgentEvent::task_progress(&ok).unwrap(),
            AgentEvent::TaskProgress {
                state: TaskState::Paused,
                stage: 2,
                requests: 3,
                reported_tokens: 4,
                elapsed_seconds: 5
            }
        );
        for bad in [
            json!({"version":2,"state":"paused","stage":2,"requests":3,"reported_tokens":4,"elapsed_seconds":5}),
            json!({"version":1,"state":"weird","stage":2,"requests":3,"reported_tokens":4,"elapsed_seconds":5}),
            json!({"version":1,"state":"paused","stage":-1,"requests":3,"reported_tokens":4,"elapsed_seconds":5}),
            json!({"version":1,"state":"paused","stage":2,"requests":3,"reported_tokens":4,"elapsed_seconds":5,"prompt":"x"}),
            json!([]),
        ] {
            assert!(AgentEvent::task_progress(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn error_codes_cover_every_core_failure_kind() {
        use danso::failure::Kind;
        for kind in [
            Kind::Configuration,
            Kind::Session,
            Kind::Sandbox,
            Kind::Provider,
            Kind::ProviderTimeout,
            Kind::Compaction,
            Kind::RequestBudget,
            Kind::Output,
            Kind::Runtime,
            Kind::RunTimeout,
            Kind::Interrupted,
            Kind::Memory,
        ] {
            let code: ErrorCode = kind.into();
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::to_value(kind).unwrap(),
                "{kind:?} must serialize identically to DANSO_ERROR"
            );
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
        }
    }

    #[test]
    fn serialized_shape_is_tagged_and_body_free_by_default() {
        let event = AgentEvent::tool_started("c1", ToolName::classify("bash"), None).unwrap();
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({"kind":"tool_started","call_id":"c1","tool":"bash","arguments":null})
        );
        assert_eq!(ToolName::classify("probe"), ToolName::Other);
        assert!(AgentEvent::completion("stop").unwrap().is_terminal());
        assert!(!AgentEvent::MessageCompleted.is_terminal());
    }
}
