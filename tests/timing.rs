//! Body-free timing instrumentation (issue #98 e): a fake-provider run
//! asserting the event/frame order, the per-request `elapsed_ms` frames and
//! the exact `DANSO_TIMING` key set. No HTTP, credentials or production
//! tools; the journal contract is exercised through the real session store.
use anyhow::Result;
use danso::{
    contracts::{Event, EventSink, ToolCall, ToolDefinition, ToolExecutor, ToolOutcome},
    output::{ProgressSink, timing_record},
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    session::Session,
    usage::{TokenUsage, Usage},
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::VecDeque, path::Path, rc::Rc};

/// Compaction summaries (system "You are a checkpoint summarizer, ...")
/// return a valid five-field checkpoint; action requests pop the script.
const CHECKPOINT: &str =
    r#"{"objective":"pending","constraints":[],"changes":[],"tests":[],"pending":[]}"#;

struct ScriptedProvider {
    replies: VecDeque<Value>,
    systems: Vec<String>,
}
impl Provider for ScriptedProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        self.systems.push(request.system.joined());
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
        if self
            .systems
            .last()
            .unwrap()
            .starts_with("You are a checkpoint summarizer")
        {
            Ok(
                json!({"role":"assistant","content":[{"type":"text","text":CHECKPOINT}],"stopReason":"stop"}),
            )
        } else {
            Ok(self.replies.pop_front().expect("unexpected action request"))
        }
    }
}

struct ProbeExecutor;
impl ToolExecutor for ProbeExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "probe".into(),
            description: "A timing test tool".into(),
            parameters: json!({"type":"object"}),
        }]
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

/// Ordered, body-free record of every runtime event, plus the request frame
/// tuples as delivered on the `EventSink` boundary.
#[derive(Default)]
struct Events {
    labels: Vec<&'static str>,
    requests: Vec<(u32, u32, u64)>,
}

#[derive(Default, Clone)]
struct Shared(Rc<RefCell<Events>>);

impl EventSink for Shared {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        let mut events = self.0.borrow_mut();
        match event {
            Event::Session(_) => events.labels.push("session"),
            Event::Message(entry) => {
                let role = entry["message"]["role"].as_str().unwrap_or_default();
                events.labels.push(match role {
                    "user" => "message:user",
                    "assistant" => "message:assistant",
                    "toolResult" => "message:toolResult",
                    _ => "message",
                });
            }
            // Stream deltas (issue #98 b): render-only events that carry no
            // timing signal for this suite.
            Event::TextDelta(_) => {}
            Event::Compaction(_) => events.labels.push("compaction"),
            Event::Request {
                sequence,
                remaining,
                elapsed_ms,
            } => {
                events.labels.push("request");
                events.requests.push((sequence, remaining, elapsed_ms));
            }
            Event::Task(_) => events.labels.push("task"),
            Event::FinalAnswer(_) => events.labels.push("final"),
            Event::ToolStarted(_) => events.labels.push("tool_started"),
            Event::ToolSettled { .. } => events.labels.push("tool_settled"),
        }
        Ok(())
    }
}

fn tool_reply(id: &str) -> Value {
    json!({"role":"assistant","content":[{"type":"toolCall","id":id,"name":"probe","arguments":{}}],"stopReason":"toolUse"})
}

fn final_reply() -> Value {
    json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"})
}

fn input(max_turns: u32, context: &'static str) -> RunInput<'static> {
    RunInput {
        no_tools: false,
        prompt: "use the probe",
        context,
        execution_context: "",
        max_turns,
        compact_at_bytes: None,
        refresh_context: None,
        long_task: None,
        repeat_limit: 0,
        continuation_limit: 0,
        stream_requests: true,
        report_progress: false,
        pause_requested: None,
        cancellation_reason: None,
    }
}

#[tokio::test]
async fn run_orders_events_and_frames_and_pins_the_timing_key_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = ScriptedProvider {
        replies: VecDeque::from(vec![tool_reply("p1"), final_reply()]),
        systems: vec![],
    };
    let shared = Shared::default();
    let mut sink = ProgressSink::new(shared.clone(), true).with_request_progress(true);
    let mut usage = Usage::default();
    runtime::run(
        input(2, ""),
        &mut provider,
        &ProbeExecutor,
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .unwrap();
    session.check_recovery().unwrap();

    // The per-request frame precedes its model dispatch; tool notifications
    // stay between the durable message frames.
    assert_eq!(
        shared.0.borrow().labels,
        [
            "session",
            "message:user",
            "request",
            "message:assistant",
            "tool_started",
            "message:toolResult",
            "tool_settled",
            "request",
            "message:assistant",
            "final"
        ]
    );
    let requests = shared.0.borrow().requests.clone();
    assert_eq!(
        requests.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
        vec![(1, 1), (2, 0)]
    );
    assert!(requests[0].2 <= requests[1].2, "run clock never moves back");

    // Exactly the documented key set, and in the documented order in the raw
    // hand-written record; durations are plain non-negative integers and
    // never carry bodies or paths.
    let record = timing_record(&usage);
    let parsed: Value = serde_json::from_str(&record).unwrap();
    let expected = [
        "version",
        "provider_ms",
        "provider_requests",
        "retry_wait_ms",
        "tool_ms",
        "tool_calls",
        "journal_ms",
        "summary_requests",
        "startup_ms",
    ];
    let mut actual_keys: Vec<_> = parsed.as_object().unwrap().keys().cloned().collect();
    actual_keys.sort();
    let mut expected_keys: Vec<_> = expected.iter().map(|key| key.to_string()).collect();
    expected_keys.sort();
    assert_eq!(actual_keys, expected_keys, "exact DANSO_TIMING key set");
    let positions: Vec<usize> = expected
        .iter()
        .map(|key| record.find(&format!("\"{key}\":")).expect("key present"))
        .collect();
    let mut sorted_positions = positions.clone();
    sorted_positions.sort();
    assert_eq!(positions, sorted_positions, "documented key order");
    assert!(record.starts_with("{\"version\":1,\"provider_ms\":"));
    assert_eq!(parsed["version"], 1);
    // Two action requests, one tool call, no retries, no summaries here.
    assert_eq!(parsed["provider_requests"], 2);
    assert_eq!(parsed["tool_calls"], 1);
    assert_eq!(parsed["retry_wait_ms"], 0);
    assert_eq!(parsed["summary_requests"], 0);
    assert_eq!(parsed["startup_ms"], 0);
    for key in ["provider_ms", "tool_ms", "journal_ms"] {
        assert!(parsed[key].is_u64(), "{key} must be a plain integer");
    }
}

#[tokio::test]
async fn compaction_summary_requests_stay_out_of_provider_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    // Bulk history via a large tool-call argument: the stable system block and
    // the verbatim user request survive compaction, so they stay small.
    let big_call = json!({"role":"assistant","content":[{"type":"toolCall","id":"p1","name":"probe","arguments":{"pad":"z".repeat(9000)}}],"stopReason":"toolUse"});
    let mut provider = ScriptedProvider {
        replies: VecDeque::from(vec![big_call, final_reply()]),
        systems: vec![],
    };
    let shared = Shared::default();
    let mut sink = ProgressSink::new(shared.clone(), true).with_request_progress(true);
    let mut usage = Usage::default();
    let request = RunInput {
        compact_at_bytes: Some(8192),
        ..input(4, "")
    };
    runtime::run(
        request,
        &mut provider,
        &ProbeExecutor,
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .unwrap();
    session.check_recovery().unwrap();

    // The checkpoint frame precedes the next action dispatch.
    assert_eq!(
        shared.0.borrow().labels,
        [
            "session",
            "message:user",
            "request",
            "message:assistant",
            "tool_started",
            "message:toolResult",
            "tool_settled",
            "compaction",
            "request",
            "message:assistant",
            "final"
        ]
    );
    // provider_* counts only action requests; compaction summaries are
    // broken out separately. Their sum is every completion the provider saw.
    let record: Value = serde_json::from_str(&timing_record(&usage)).unwrap();
    assert_eq!(record["provider_requests"], 2);
    let summaries = record["summary_requests"].as_u64().unwrap();
    assert!(
        summaries >= 1,
        "compaction spent at least one summary request"
    );
    assert_eq!(
        provider.systems.len() as u64,
        2 + summaries,
        "every completion is one action or one summary"
    );
    assert!(!provider.systems[0].starts_with("You are a checkpoint summarizer"));
    assert!(
        provider.systems[provider.systems.len() - 1]
            .starts_with("You are a headless coding worker")
    );
    assert_eq!(record["tool_calls"], 1);
}
