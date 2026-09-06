//! Agent-loop policy. No CLI, environment lookup, HTTP client or shell spawning.
use crate::{
    contracts::{Event, EventSink, OperationState, SessionStore, ToolCall, ToolExecutor},
    failure::{Kind, at},
    provider::{ModelRequest, Provider},
    session::millis,
    usage::Usage,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub struct RunInput<'a> {
    pub prompt: &'a str,
    pub context: &'a str,
    pub max_turns: u32,
    pub compact_at_bytes: Option<usize>,
}

pub async fn run(
    input: RunInput<'_>,
    provider: &mut impl Provider,
    executor: &impl ToolExecutor,
    session: &mut impl SessionStore,
    sink: &mut impl EventSink,
    usage: &mut Usage,
) -> Result<()> {
    (|| {
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
        if let Some(limit) = input.compact_at_bytes {
            ensure!(
                (crate::compaction::MIN_THRESHOLD..=crate::compaction::MAX_THRESHOLD)
                    .contains(&limit),
                "compact-at-bytes must be 8192..393216"
            );
            ensure!(
                session.supports_compaction(),
                "session store does not support compaction"
            );
        }
        Ok::<(), anyhow::Error>(())
    })()
    .map_err(at(Kind::Configuration))?;
    session.check_recovery().map_err(at(Kind::Session))?;
    let mut ids = session.tool_call_ids().map_err(at(Kind::Session))?;
    let mut messages = session.messages().map_err(at(Kind::Session))?;
    provider
        .validate_history(&messages)
        .map_err(at(Kind::Provider))?;
    executor.preflight().await.map_err(at(Kind::Sandbox))?;
    let definitions = executor.definitions();
    let names = definitions
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let system = format!(
        "You are a headless coding worker. Use only {names}. Skills are loaded using read. Prefer targeted line-range reads and searches over whole-file dumps. After compaction, continue from recorded progress; re-read only missing or changed information.{}",
        input.context
    );
    sink.emit(Event::Session(session.header()))
        .map_err(at(Kind::Output))?;
    let user = json!({"role":"user","content":input.prompt,"timestamp":millis()});
    sink.emit(Event::Message(
        &session
            .append_message(user.clone())
            .map_err(at(Kind::Session))?,
    ))
    .map_err(at(Kind::Output))?;
    messages.push(user);
    let mut remaining = input.max_turns;
    while remaining > 0 {
        async {
            if let Some(limit) = input.compact_at_bytes {
                let before = provider.request_bytes(&ModelRequest {
                    system: &system,
                    messages: &messages,
                    tools: &definitions,
                })?;
                if before > limit {
                    // The current request and static instructions cannot be summarized
                    // away. Reject impossible budgets before spending summary calls.
                    let latest = crate::compaction::latest_user(&messages)?;
                    let bare = provider.request_bytes(&ModelRequest {
                        system: &system,
                        messages: std::slice::from_ref(&latest),
                        tools: &definitions,
                    })?;
                    let summary_budget = (limit / 8).min(crate::compaction::MAX_SUMMARY_BYTES);
                    ensure!(
                        bare + 2 * summary_budget + 512 <= limit,
                        "current request and instructions leave no compaction budget"
                    );
                    session.check_recovery().map_err(at(Kind::Session))?;
                    let summary = crate::compaction::summarize(
                        provider,
                        &messages,
                        limit,
                        &mut remaining,
                        usage,
                    )
                    .await?;
                    let compacted = crate::compaction::checkpoint_messages(&summary, &messages)?;
                    let after = provider.request_bytes(&ModelRequest {
                        system: &system,
                        messages: &compacted,
                        tools: &definitions,
                    })?;
                    ensure!(
                        after <= limit && after < before,
                        "compaction did not reduce request below threshold"
                    );
                    let entry = session
                        .record_compaction(summary)
                        .map_err(at(Kind::Session))?;
                    // Durable checkpoint before the next request; renderer failure
                    // also stops continuation, leaving a resumable journal.
                    sink.emit(Event::Compaction(&entry))
                        .map_err(at(Kind::Output))?;
                    messages = session.messages().map_err(at(Kind::Session))?;
                    ensure!(
                        messages == compacted,
                        "session store returned inconsistent compacted context"
                    );
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await
        .map_err(at(Kind::Compaction))?;
        remaining -= 1;
        let message = provider
            .complete(
                ModelRequest {
                    system: &system,
                    messages: &messages,
                    tools: &definitions,
                },
                usage,
            )
            .await
            .map_err(at(Kind::Provider))?;
        let calls = (|| {
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
            Ok::<_, anyhow::Error>(calls)
        })()
        .map_err(at(Kind::Provider))?;
        // All adapters must pass this gate before any new side effect.
        for call in &calls {
            ensure!(ids.insert(call.id.clone()), "duplicate tool call id");
        }
        sink.emit(Event::Message(
            &session
                .append_message(message.clone())
                .map_err(at(Kind::Session))?,
        ))
        .map_err(at(Kind::Output))?;
        messages.push(message.clone());
        if calls.is_empty() {
            if message["stopReason"] != "stop" {
                return Err(at(Kind::Provider)(anyhow::anyhow!(
                    "provider response truncated"
                )));
            }
            sink.emit(Event::FinalAnswer(&message))
                .map_err(at(Kind::Output))?;
            return Ok(());
        }
        for call in calls {
            session
                .record_operation(&call.id, OperationState::Started)
                .map_err(at(Kind::Session))?;
            let outcome = match executor.execute(&call).await {
                Ok(result) => result,
                Err(e) => crate::contracts::ToolOutcome {
                    output: e.to_string(),
                    is_error: true,
                },
            };
            let result = json!({"role":"toolResult","toolCallId":call.id,"toolName":call.name,"content":[{"type":"text","text":outcome.output}],"isError":outcome.is_error,"timestamp":millis()});
            sink.emit(Event::Message(
                &session
                    .append_message(result.clone())
                    .map_err(at(Kind::Session))?,
            ))
            .map_err(at(Kind::Output))?;
            session
                .record_operation(&call.id, OperationState::Settled)
                .map_err(at(Kind::Session))?;
            messages.push(result);
        }
    }
    Err(at(Kind::RequestBudget)(anyhow::anyhow!(
        "turn budget exhausted"
    )))
}

pub(crate) fn tool_calls(message: &Value) -> Result<Vec<ToolCall>> {
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
