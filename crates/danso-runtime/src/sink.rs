//! Bridges the core `EventSink` to the normalized event stream.
//!
//! The core loop emits `Event` values at durable boundaries; this sink maps
//! them to body-free [`AgentEvent`]s. Tool call ids come from the assistant
//! message that precedes each `ToolStarted`, so the mapping stays inside the
//! sink and the core loop keeps its existing event surface.
use crate::event::{AgentEvent, TaskState, ToolName};
use anyhow::{Context, Result, ensure};
use danso::contracts::{Event, EventSink};
use serde_json::Value;
use std::collections::VecDeque;
use tokio::sync::mpsc::UnboundedSender;

pub struct ChannelSink {
    tx: UnboundedSender<AgentEvent>,
    pending_calls: VecDeque<(String, ToolName)>,
    active: Option<(String, ToolName)>,
    final_text: Option<String>,
    last_task_state: Option<TaskState>,
}

impl ChannelSink {
    pub fn new(tx: UnboundedSender<AgentEvent>) -> Self {
        Self {
            tx,
            pending_calls: VecDeque::new(),
            active: None,
            final_text: None,
            last_task_state: None,
        }
    }

    /// The final answer text once `FinalAnswer` was observed.
    pub fn final_text(&self) -> Option<&str> {
        self.final_text.as_deref()
    }

    pub fn last_task_state(&self) -> Option<TaskState> {
        self.last_task_state
    }

    fn send(&self, event: AgentEvent) -> Result<()> {
        self.tx
            .send(event)
            .map_err(|_| anyhow::anyhow!("event consumer is gone"))
    }
}

fn text_of(message: &Value) -> String {
    let mut text = String::new();
    if let Some(blocks) = message["content"].as_array() {
        for block in blocks {
            if let Some(part) = block["text"].as_str() {
                text.push_str(part);
            }
        }
    }
    text
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        match event {
            Event::Session(_) | Event::Compaction(_) | Event::Request { .. } => Ok(()),
            Event::Message(entry) => {
                if entry["role"] == "assistant"
                    && let Some(blocks) = entry["content"].as_array()
                {
                    for block in blocks.iter().filter(|b| b["type"] == "toolCall") {
                        let id = block["id"]
                            .as_str()
                            .context("tool call without id")?
                            .to_string();
                        let name = block["name"].as_str().context("tool call without name")?;
                        self.pending_calls.push_back((id, ToolName::classify(name)));
                    }
                }
                Ok(())
            }
            Event::ToolStarted(name) => {
                ensure!(self.active.is_none(), "overlapping tool progress");
                let (id, tool) = self
                    .pending_calls
                    .pop_front()
                    .context("tool started without a journaled call")?;
                ensure!(
                    tool == ToolName::classify(name),
                    "tool start does not match the journaled call"
                );
                self.active = Some((id.clone(), tool));
                self.send(AgentEvent::tool_started(id, tool, None)?)
            }
            Event::ToolSettled { is_error } => {
                let (id, tool) = self.active.take().context("tool settled without start")?;
                self.send(AgentEvent::tool_completed(id, tool, !is_error, None)?)
            }
            Event::Task(record) => {
                let event = AgentEvent::task_progress(record)?;
                if let AgentEvent::TaskProgress { state, .. } = &event {
                    self.last_task_state = Some(*state);
                }
                self.send(event)
            }
            Event::FinalAnswer(message) => {
                let text = text_of(message);
                if !text.is_empty() {
                    self.send(AgentEvent::text_delta(text.clone())?)?;
                }
                self.send(AgentEvent::MessageCompleted)?;
                self.final_text = Some(text);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn tool_events_carry_journaled_ids_and_no_bodies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ChannelSink::new(tx);
        let assistant = json!({"role":"assistant","content":[
            {"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"rm -rf /secret"}}
        ],"stopReason":"toolUse"});
        sink.emit(Event::Message(&assistant)).unwrap();
        sink.emit(Event::ToolStarted("bash")).unwrap();
        let result = json!({"role":"toolResult","toolCallId":"c1","content":[{"type":"text","text":"output body"}],"isError":true});
        sink.emit(Event::Message(&result)).unwrap();
        sink.emit(Event::ToolSettled { is_error: true }).unwrap();
        let events = drain(&mut rx);
        assert_eq!(
            events,
            vec![
                AgentEvent::tool_started("c1", ToolName::Bash, None).unwrap(),
                AgentEvent::tool_completed("c1", ToolName::Bash, false, None).unwrap(),
            ]
        );
        let rendered = serde_json::to_string(&events).unwrap();
        assert!(!rendered.contains("rm -rf"));
        assert!(!rendered.contains("output body"));
    }

    #[test]
    fn unjournaled_or_overlapping_tool_progress_is_rejected() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ChannelSink::new(tx);
        assert!(sink.emit(Event::ToolStarted("bash")).is_err());
        assert!(sink.emit(Event::ToolSettled { is_error: false }).is_err());
        let assistant = json!({"role":"assistant","content":[
            {"type":"toolCall","id":"c1","name":"read","arguments":{}},
            {"type":"toolCall","id":"c2","name":"read","arguments":{}}
        ]});
        sink.emit(Event::Message(&assistant)).unwrap();
        sink.emit(Event::ToolStarted("read")).unwrap();
        assert!(sink.emit(Event::ToolStarted("read")).is_err());
    }

    #[test]
    fn final_answer_yields_delta_and_boundary_and_records_text() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = ChannelSink::new(tx);
        let message = json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"});
        sink.emit(Event::FinalAnswer(&message)).unwrap();
        assert_eq!(
            drain(&mut rx),
            vec![
                AgentEvent::text_delta("done").unwrap(),
                AgentEvent::MessageCompleted
            ]
        );
        assert_eq!(sink.final_text(), Some("done"));
    }

    #[test]
    fn closed_consumer_fails_the_emit() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let mut sink = ChannelSink::new(tx);
        let record = json!({"version":1,"state":"checkpoint","stage":0,"requests":0,"reported_tokens":0,"elapsed_seconds":0});
        assert!(sink.emit(Event::Task(&record)).is_err());
    }
}
