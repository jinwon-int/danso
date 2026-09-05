//! Agent-loop policy. No CLI, environment lookup, HTTP client or shell spawning.
use crate::{
    contracts::{Event, EventSink, OperationState, SessionStore, ToolCall, ToolExecutor},
    provider::{ModelRequest, Provider},
    session::millis,
    usage::Usage,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

pub struct RunInput<'a> {
    pub prompt: &'a str,
    pub context: &'a str,
    pub max_turns: u32,
}

pub async fn run(
    input: RunInput<'_>,
    provider: &mut impl Provider,
    executor: &impl ToolExecutor,
    session: &mut impl SessionStore,
    sink: &mut impl EventSink,
    usage: &mut Usage,
) -> Result<()> {
    ensure!(
        (1..=128).contains(&input.max_turns),
        "max-turns must be 1..128"
    );
    ensure!(
        !input.prompt.trim().is_empty() && input.prompt.len() <= crate::context::CONTEXT_LIMIT,
        "prompt must be 1..65536 bytes"
    );
    ensure!(
        input.context.len() <= crate::context::CONTEXT_LIMIT,
        "bootstrap/skills context exceeds 65536 bytes"
    );
    session.check_recovery()?;
    let mut messages = session.messages()?;
    provider.validate_history(&messages)?;
    executor.preflight().await?;
    let definitions = executor.definitions();
    let names = definitions
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let system = format!(
        "You are a headless coding worker. Use only {names}. Skills are loaded using read.{}",
        input.context
    );
    sink.emit(Event::Session(session.header()))?;
    let user = json!({"role":"user","content":input.prompt,"timestamp":millis()});
    sink.emit(Event::Message(&session.append_message(user.clone())?))?;
    messages.push(user);
    for _ in 0..input.max_turns {
        let message = provider
            .complete(
                ModelRequest {
                    system: &system,
                    messages: &messages,
                    tools: &definitions,
                },
                usage,
            )
            .await?;
        ensure!(
            message["role"] == "assistant",
            "provider must return an assistant message"
        );
        let calls = tool_calls(&message)?;
        ensure!(
            !calls.is_empty()
                || message["stopReason"] == "stop"
                || message["stopReason"] == "length",
            "invalid terminal response"
        );
        ensure!(
            calls.is_empty() || message["stopReason"] == "toolUse",
            "invalid tool response"
        );
        // All adapters must pass this gate before any new side effect.
        let mut ids = std::collections::HashSet::new();
        for m in messages.iter().chain(std::iter::once(&message)) {
            if m["role"] == "assistant" {
                for call in tool_calls(m)? {
                    ensure!(ids.insert(call.id), "duplicate tool call id");
                }
            }
        }
        sink.emit(Event::Message(&session.append_message(message.clone())?))?;
        messages.push(message.clone());
        if calls.is_empty() {
            ensure!(
                message["stopReason"] == "stop",
                "provider response truncated"
            );
            sink.emit(Event::FinalAnswer(&message))?;
            return Ok(());
        }
        for call in calls {
            session.record_operation(&call.id, OperationState::Started)?;
            let outcome = match executor.execute(&call).await {
                Ok(result) => result,
                Err(e) => crate::contracts::ToolOutcome {
                    output: e.to_string(),
                    is_error: true,
                },
            };
            let result = json!({"role":"toolResult","toolCallId":call.id,"toolName":call.name,"content":[{"type":"text","text":outcome.output}],"isError":outcome.is_error,"timestamp":millis()});
            sink.emit(Event::Message(&session.append_message(result.clone())?))?;
            session.record_operation(&call.id, OperationState::Settled)?;
            messages.push(result);
        }
    }
    bail!("turn budget exhausted")
}

fn tool_calls(message: &Value) -> Result<Vec<ToolCall>> {
    message["content"]
        .as_array()
        .context("invalid assistant content")?
        .iter()
        .filter(|b| b["type"] == "toolCall")
        .map(|b| {
            let call: ToolCall = serde_json::from_value(b.clone())?;
            ensure!(
                !call.id.is_empty() && !call.name.is_empty() && call.arguments.is_object(),
                "invalid tool call"
            );
            Ok(call)
        })
        .collect()
}
