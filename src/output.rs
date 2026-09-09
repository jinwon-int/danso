use crate::{
    contracts::{Event, EventSink},
    usage::Usage,
};
use anyhow::{Result, ensure};
use serde_json::json;
use std::io::Write;

/// Opt-in notifications. They carry no model IDs, arguments or tool output.
pub struct ProgressSink<S> {
    inner: S,
    enabled: bool,
    task_enabled: bool,
    requests_enabled: bool,
    sequence: u64,
    active: Option<&'static str>,
}

impl<S> ProgressSink<S> {
    pub fn new(inner: S, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            task_enabled: false,
            requests_enabled: false,
            sequence: 0,
            active: None,
        }
    }

    pub fn with_request_progress(mut self, enabled: bool) -> Self {
        self.requests_enabled = enabled;
        self
    }

    pub fn with_task_progress(mut self, enabled: bool) -> Self {
        self.task_enabled = enabled;
        self
    }
}

impl<S: EventSink> EventSink for ProgressSink<S> {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        if self.enabled {
            let record = match &event {
                Event::ToolStarted(tool) => {
                    ensure!(self.active.is_none(), "overlapping tool progress");
                    let tool = match *tool {
                        "read" => "read",
                        "write" => "write",
                        "edit" => "edit",
                        "bash" => "bash",
                        _ => "other",
                    };
                    self.sequence += 1;
                    self.active = Some(tool);
                    Some(json!({"type":"danso_progress", "version":1,
                        "sequence":self.sequence,"phase":"started","tool":tool}))
                }
                Event::ToolSettled { is_error } => {
                    let tool = self
                        .active
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("tool progress without start"))?;
                    Some(json!({"type":"danso_progress", "version":1,
                        "sequence":self.sequence,"phase":"settled","tool":tool,"success":!is_error}))
                }
                Event::Request {
                    sequence,
                    remaining,
                } => {
                    if !self.requests_enabled {
                        return Ok(());
                    }
                    Some(json!({"type":"danso_request", "version":1,
                        "sequence":sequence, "remaining":remaining}))
                }
                _ => None,
            };
            if let Some(record) = record {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{record}")?;
                stdout.flush()?;
            }
        }
        if self.task_enabled
            && let Event::Task(progress) = &event
        {
            let mut stderr = std::io::stderr().lock();
            writeln!(stderr, "DANSO_TASK={progress}")?;
            stderr.flush()?;
        }
        self.inner.emit(event)
    }
}

#[derive(Clone, Copy)]
pub enum Mode {
    Json,
    Text,
}

pub struct PrintSink(pub Mode);
impl EventSink for PrintSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        match (self.0, event) {
            (
                Mode::Json,
                Event::Session(entry) | Event::Message(entry) | Event::Compaction(entry),
            ) => writeln!(stdout, "{entry}")?,
            (Mode::Text, Event::FinalAnswer(message)) => {
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        if let Some(text) = block["text"].as_str() {
                            writeln!(stdout, "{text}")?;
                        }
                    }
                }
            }
            _ => {}
        }
        stdout.flush()?;
        Ok(())
    }
}

pub fn report_usage(usage: &Usage) {
    let summary = usage.summary();
    eprintln!("DANSO_USAGE={summary}");
    eprintln!("PIRI_USAGE={summary}");
}

/// Body-free run-budget receipt (issue #69 F): ignored or validated by the
/// ccc adapter; never includes prompt or response content.
pub fn report_budget(config: &crate::app::RunConfig, usage: &Usage) {
    let requests_total = match &config.long_task {
        Some(task) => u32::try_from(task.limits.max_requests).unwrap_or(u32::MAX),
        None => config.max_turns,
    };
    let (summary_requests, length_stops, continuations) = usage.budget_counts();
    let cap = crate::provider::resolve_max_output_tokens(config.max_output_tokens).unwrap_or(0);
    eprintln!(
        "DANSO_BUDGET={{\"version\":1,\"requests_used\":{},\"requests_total\":{},\"summary_requests\":{},\"output_tokens_max\":{},\"length_stops\":{},\"continuations\":{}}}",
        usage.snapshot().requests,
        requests_total,
        summary_requests,
        cap,
        length_stops,
        continuations
    );
}
