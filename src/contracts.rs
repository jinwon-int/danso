//! Shared harness contracts. Provider wire formats and CLI flags stay outside.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug)]
pub struct ToolOutcome {
    pub output: String,
    pub is_error: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum OperationState {
    Started,
    Settled,
}
impl OperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Settled => "settled",
        }
    }
}

/// Persistence operations required by the loop. Implementations must make
/// successful appends durable before returning and exclude concurrent writers.
pub trait SessionStore {
    fn header(&self) -> &Value;
    fn messages(&self) -> Result<Vec<Value>>;
    fn check_recovery(&self) -> Result<()>;
    /// All call IDs from the original journal, including compacted-away messages.
    fn tool_call_ids(&self) -> Result<std::collections::HashSet<String>>;
    fn supports_compaction(&self) -> bool {
        false
    }
    fn record_compaction(&mut self, _summary: Value) -> Result<Value> {
        anyhow::bail!("session store does not support compaction")
    }
    fn append_message(&mut self, message: Value) -> Result<Value>;
    fn record_operation(&mut self, id: &str, state: OperationState) -> Result<()>;
    /// Long-task metadata is append-only and body-free. Stores that do not
    /// support the opt-in mode keep the default refusal behavior.
    fn long_task_records(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn record_long_task(&mut self, _data: Value) -> Result<Value> {
        anyhow::bail!("session store does not support long tasks")
    }
}

/// Admission verdict for one tool call, decided before the call is journaled
/// or dispatched. A denied call is recorded as a failed tool result so the
/// model sees the refusal; the executor is never invoked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny {
        reason: String,
    },
    /// Requires an explicit approval. Until an approval route exists the
    /// runtime treats this exactly like `Deny` (fail-closed).
    Ask,
}

/// The executor owns isolation and limits; the loop never invokes tools inline.
#[allow(async_fn_in_trait)]
pub trait ToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn preflight(&self) -> Result<()>;
    /// Policy admission for one call. The default admits everything, which
    /// keeps the CLI behavior unchanged; embedders wrap an executor to apply
    /// a stricter policy. Called by the loop before the `started` marker.
    fn admit(&self, _call: &ToolCall) -> Verdict {
        Verdict::Allow
    }
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutcome>;
}

pub enum Event<'a> {
    Session(&'a Value),
    Message(&'a Value),
    Compaction(&'a Value),
    FinalAnswer(&'a Value),
    /// Issue #98 (item a): the complete text of one interim assistant
    /// message, streamed at its durable message boundary — after the journal
    /// append, before any tool effect. A render notification only: never a
    /// journal record and never authorization to act on partial output.
    TextDelta(&'a str),
    /// Closes the assistant message whose text the preceding TextDelta
    /// frames carried. Emitted at most once per streamed message.
    MessageCompleted,
    /// Body-free opt-in long-task checkpoint notification.
    Task(&'a Value),
    /// Opt-in per-model-request notification (issue #69 F): body-free. The
    /// frame carries `elapsed_ms`, the run-clock dispatch stamp (issue #98 e).
    Request {
        sequence: u32,
        remaining: u32,
        elapsed_ms: u64,
    },
    /// Emitted only after the corresponding operation marker is durable.
    ToolStarted(&'a str),
    ToolSettled {
        is_error: bool,
    },
}

/// An output adapter can render text, JSONL, or collect events in a test.
/// It cannot mutate the session or authorize tool execution.
pub trait EventSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()>;
}

/// The text blocks of a message, in order (issue #98 a). One shared
/// extraction so the runtime boundary stream and renderers agree on what
/// counts as assistant text.
pub fn text_blocks(message: &Value) -> Vec<&str> {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect()
        })
        .unwrap_or_default()
}
