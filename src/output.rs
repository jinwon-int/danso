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
    sequence: u64,
    active: Option<&'static str>,
}

impl<S> ProgressSink<S> {
    pub fn new(inner: S, enabled: bool) -> Self {
        Self {
            inner,
            enabled,
            task_enabled: false,
            sequence: 0,
            active: None,
        }
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
