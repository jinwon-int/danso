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
}

/// The executor owns isolation and limits; the loop never invokes tools inline.
#[allow(async_fn_in_trait)]
pub trait ToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn preflight(&self) -> Result<()>;
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutcome>;
}

pub enum Event<'a> {
    Session(&'a Value),
    Message(&'a Value),
    Compaction(&'a Value),
    FinalAnswer(&'a Value),
}

/// An output adapter can render text, JSONL, or collect events in a test.
/// It cannot mutate the session or authorize tool execution.
pub trait EventSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()>;
}
