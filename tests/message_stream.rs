//! Issue #98 (item a): interim assistant text streams as TextDelta +
//! MessageCompleted at the durable message boundary, before any tool effect;
//! the final answer stays the last event and the journal keeps whole
//! messages only. No HTTP, CLI, or credentials.
use anyhow::Result;
use danso::{
    contracts::{Event, EventSink, ToolCall, ToolDefinition, ToolExecutor, ToolOutcome},
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    session::Session,
    tools::{Registry, Tool},
    usage::{TokenUsage, Usage},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path};

struct ScriptedProvider {
    replies: VecDeque<Value>,
}
impl Provider for ScriptedProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, _: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        usage.attempted = true;
        usage.add(
            "test-provider",
            "scripted",
            TokenUsage {
                input: 1,
                output: 2,
                ..Default::default()
            },
        )?;
        Ok(self.replies.pop_front().expect("unexpected request"))
    }
}

struct MarkerTool;
impl Tool for MarkerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "probe".into(),
            description: "A streaming test tool".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute(&self, _: &Value) -> Result<()> {
        Ok(())
    }
}

struct Executor {
    registry: Registry,
}
impl Executor {
    fn new(registry: Registry) -> Self {
        Self { registry }
    }
}
impl ToolExecutor for Executor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry.definitions()
    }
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }
    async fn execute(&self, _: &ToolCall) -> Result<ToolOutcome> {
        Ok(ToolOutcome {
            output: "probe result".into(),
            is_error: false,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Frame {
    Message(String),
    TextDelta(String),
    MessageCompleted,
    ToolStarted(String),
    ToolSettled(bool),
    FinalAnswer(String),
}

#[derive(Default)]
struct StreamSink {
    frames: Vec<Frame>,
}
impl EventSink for StreamSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        match event {
            Event::Message(message) => {
                let role = message["role"].as_str().expect("role").to_owned();
                self.frames.push(Frame::Message(role))
            }
            Event::TextDelta(text) => self.frames.push(Frame::TextDelta(text.to_owned())),
            Event::MessageCompleted => self.frames.push(Frame::MessageCompleted),
            Event::ToolStarted(tool) => self.frames.push(Frame::ToolStarted(tool.to_owned())),
            Event::ToolSettled { is_error } => self.frames.push(Frame::ToolSettled(is_error)),
            Event::FinalAnswer(message) => {
                let text = message["content"][0]["text"].as_str().expect("text").to_owned();
                self.frames.push(Frame::FinalAnswer(text))
            }
            _ => {}
        }
        Ok(())
    }
}

fn interim_with_text() -> Value {
    json!({
        "role":"assistant",
        "content":[
            {"type":"text","text":"Checking the fixture now."},
            {"type":"toolCall","id":"c1","name":"probe","arguments":{"marker":1}}
        ],
        "stopReason":"toolUse"
    })
}

fn interim_tool_only() -> Value {
    json!({
        "role":"assistant",
        "content":[{"type":"toolCall","id":"c2","name":"probe","arguments":{}}],
        "stopReason":"toolUse"
    })
}

fn final_message(text: &str) -> Value {
    json!({"role":"assistant","content":[{"type":"text","text":text}],"stopReason":"stop"})
}

fn input() -> RunInput<'static> {
    RunInput {
        no_tools: false,
        prompt: "use the probe",
        context: "",
        execution_context: "",
        max_turns: 3,
        compact_at_bytes: None,
        refresh_context: None,
        long_task: None,
        repeat_limit: 0,
        continuation_limit: 0,
        stream_requests: false,
        report_progress: false,
        pause_requested: None,
        cancellation_reason: None,
    }
}

async fn run(session_dir: &Path, replies: Vec<Value>) -> (StreamSink, Vec<Value>) {
    let path = session_dir.join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).expect("session");
    let mut registry = Registry::default();
    registry.register(MarkerTool).expect("register");
    let mut provider = ScriptedProvider {
        replies: replies.into(),
    };
    let mut sink = StreamSink::default();
    let mut usage = Usage::default();
    runtime::run(
        input(),
        &mut provider,
        &Executor::new(registry),
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .expect("run succeeds");
    // The journal keeps only completed messages; streaming must add no
    // records and the session must stay recoverable.
    let messages = session.messages().expect("messages");
    session.check_recovery().expect("recoverable");
    (sink, messages)
}

#[tokio::test]
async fn interim_text_streams_between_tool_calls_and_final_answer_is_last() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sink, messages) = run(dir.path(), vec![interim_with_text(), final_message("done")]).await;
    assert_eq!(
        sink.frames,
        vec![
            Frame::Message("user".into()),
            Frame::Message("assistant".into()),
            Frame::TextDelta("Checking the fixture now.".into()),
            Frame::MessageCompleted,
            Frame::ToolStarted("probe".into()),
            Frame::Message("toolResult".into()),
            Frame::ToolSettled(false),
            Frame::Message("assistant".into()),
            Frame::FinalAnswer("done".into()),
        ]
    );
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"]);
}

#[tokio::test]
async fn final_only_run_never_streams_boundary_deltas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sink, _) = run(dir.path(), vec![final_message("done")]).await;
    // No TextDelta/MessageCompleted for the terminal message: the final
    // answer rendering stays on FinalAnswer alone (no double render).
    assert_eq!(
        sink.frames,
        vec![
            Frame::Message("user".into()),
            Frame::Message("assistant".into()),
            Frame::FinalAnswer("done".into()),
        ]
    );
}

#[tokio::test]
async fn tool_only_interim_message_emits_no_unbalanced_frames() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (sink, _) = run(dir.path(), vec![interim_tool_only(), final_message("done")]).await;
    assert_eq!(
        sink.frames,
        vec![
            Frame::Message("user".into()),
            Frame::Message("assistant".into()),
            Frame::ToolStarted("probe".into()),
            Frame::Message("toolResult".into()),
            Frame::ToolSettled(false),
            Frame::Message("assistant".into()),
            Frame::FinalAnswer("done".into()),
        ]
    );
}
