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
                    elapsed_ms,
                } => {
                    if !self.requests_enabled {
                        return Ok(());
                    }
                    Some(request_frame(*sequence, *remaining, *elapsed_ms))
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

/// The `danso_request` progress frame (issue #69 F) plus the body-free
/// run-clock `elapsed_ms` dispatch stamp (issue #98 e). Split out so the
/// field set stays pinned by a test.
fn request_frame(sequence: u32, remaining: u32, elapsed_ms: u64) -> serde_json::Value {
    json!({"type":"danso_request", "version":1,
        "sequence":sequence, "remaining":remaining, "elapsed_ms":elapsed_ms})
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
            // Issue #98 (item a): interim assistant text streams as framed
            // records a process consumer can parse incrementally. JSON mode
            // ignores them; the transcript frames are unchanged. The records
            // are hand-written so the key order stays stable for adapters
            // (serde_json maps serialize alphabetically).
            (Mode::Text, Event::TextDelta(text)) => writeln!(
                stdout,
                "{{\"type\":\"danso_text_delta\",\"version\":1,\"text\":{}}}",
                serde_json::to_string(text)?
            )?,
            (Mode::Text, Event::MessageCompleted) => writeln!(
                stdout,
                "{{\"type\":\"danso_message_completed\",\"version\":1}}"
            )?,
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
    let cap = crate::provider::resolve_max_output_tokens(config.max_output_tokens).unwrap_or(0);
    eprintln!("DANSO_BUDGET={}", budget_record(usage, requests_total, cap));
}

/// The DANSO_BUDGET payload as a string, split out from the side-effecting
/// printer and from `RunConfig` so the field set can be pinned by a test.
/// Hand-written rather than `json!` so the key order stays stable for
/// existing adapters.
pub fn budget_record(usage: &Usage, requests_total: u32, cap: u32) -> String {
    let (summary_requests, length_stops, continuations) = usage.budget_counts();
    format!(
        "{{\"version\":1,\"requests_used\":{},\"requests_total\":{},\"summary_requests\":{},\"memory_requests\":{},\"output_tokens_max\":{},\"length_stops\":{},\"continuations\":{}}}",
        usage.snapshot().requests,
        requests_total,
        summary_requests,
        usage.memory_requests(),
        cap,
        length_stops,
        continuations
    )
}

/// Body-free run timing receipt (issue #98 e): printed once on stderr at run
/// end. Durations and counts only — never prompt text, file paths or response
/// bodies. Hand-written rather than `json!` so the key order stays stable for
/// existing adapters.
pub fn timing_record(usage: &Usage) -> String {
    let timing = usage.timing();
    let (summary_requests, _, _) = usage.budget_counts();
    format!(
        "{{\"version\":1,\"provider_ms\":{},\"provider_requests\":{},\"retry_wait_ms\":{},\"tool_ms\":{},\"tool_calls\":{},\"journal_ms\":{},\"summary_requests\":{},\"startup_ms\":{}}}",
        timing.provider_ms,
        timing.provider_requests,
        timing.retry_wait_ms,
        timing.tool_ms,
        timing.tool_calls,
        timing.journal_ms,
        summary_requests,
        timing.startup_ms
    )
}

/// One `DANSO_TIMING=` line at run end (issue #98 e); the record itself is
/// pinned by tests via `timing_record`.
pub fn report_timing(usage: &Usage) {
    eprintln!("DANSO_TIMING={}", timing_record(usage));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_frame_has_the_exact_documented_field_set() {
        assert_eq!(
            request_frame(2, 46, 1234),
            serde_json::json!({"type":"danso_request","version":1,
                "sequence":2,"remaining":46,"elapsed_ms":1234})
        );
    }

    #[test]
    fn timing_record_has_the_exact_key_set_and_order() {
        let mut usage = Usage::default();
        usage.record_startup(5);
        usage.record_provider_request(17);
        usage.record_provider_request(3);
        usage.record_retry_wait(875);
        usage.record_tool_execution(41);
        usage.record_journal_append(2);
        assert_eq!(
            timing_record(&usage),
            "{\"version\":1,\"provider_ms\":20,\"provider_requests\":2,\"retry_wait_ms\":875,\"tool_ms\":41,\"tool_calls\":1,\"journal_ms\":2,\"summary_requests\":0,\"startup_ms\":5}"
        );
    }
}
