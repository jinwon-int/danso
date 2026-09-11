use anyhow::Result;
use danso::{
    contracts::{
        Event, EventSink, SessionStore, ToolCall, ToolDefinition, ToolExecutor, ToolOutcome,
    },
    failure::{self, Kind},
    long_task::Limits,
    provider::{ModelRequest, Provider},
    runtime::{self, LongTaskRun, RunInput},
    session::Session,
    usage::{TokenUsage, Usage},
};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::{cell::RefCell, collections::VecDeque, path::Path, rc::Rc, time::Duration};

struct FakeProvider {
    replies: VecDeque<Value>,
    calls: usize,
    delay: Option<Duration>,
}

impl Provider for FakeProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }

    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        self.calls += 1;
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        usage.attempted = true;
        usage.add(
            "fake",
            "long-task",
            TokenUsage {
                input: 1,
                output: 2,
                ..Default::default()
            },
        )?;
        if request
            .system
            .starts_with("You are a checkpoint summarizer")
        {
            Ok(summary_reply())
        } else {
            self.replies
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("unexpected fake provider request"))
        }
    }
}

struct FakeExecutor {
    calls: Rc<RefCell<Vec<String>>>,
    outputs: RefCell<VecDeque<String>>,
}

impl ToolExecutor for FakeExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "probe".into(),
            description: "fixture".into(),
            parameters: json!({"type":"object"}),
        }]
    }

    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolOutcome> {
        self.calls.borrow_mut().push(call.id.clone());
        Ok(ToolOutcome {
            output: self
                .outputs
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| "same".into()),
            is_error: false,
        })
    }
}

#[derive(Default)]
struct Sink {
    task_states: Vec<String>,
}

impl EventSink for Sink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        if let Event::Task(progress) = event {
            self.task_states
                .push(progress["state"].as_str().unwrap().into());
        }
        Ok(())
    }
}

fn limits() -> Limits {
    Limits {
        wall_seconds: 600,
        stage_requests: 16,
        max_requests: 16,
        max_tokens: 1000,
        repeat_limit: 3,
    }
}

#[test]
fn configured_six_hour_profile_stays_bounded_above_conservative_defaults() {
    let profile = Limits {
        wall_seconds: 6 * 60 * 60,
        stage_requests: 1024,
        max_requests: 2048,
        max_tokens: 25_000_000,
        repeat_limit: 8,
    };
    assert!(profile.validate().is_ok());
    assert!(
        Limits {
            max_requests: 2049,
            ..profile
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            max_tokens: 25_000_001,
            ..profile
        }
        .validate()
        .is_err()
    );
}

fn input<'a>(prompt: &'a str, task: LongTaskRun) -> RunInput<'a> {
    RunInput {
        no_tools: false,
        prompt,
        context: "",
        execution_context: "",
        max_turns: 128,
        compact_at_bytes: None,
        refresh_context: None,
        long_task: Some(task),
        repeat_limit: 0,
        continuation_limit: 0,
        stream_requests: false,
        report_progress: false,
        pause_requested: None,
        cancellation_reason: None,
    }
}

fn tool_reply(id: &str) -> Value {
    json!({
        "role":"assistant",
        "content":[{"type":"toolCall","id":id,"name":"probe","arguments":{"same":true}}],
        "stopReason":"toolUse"
    })
}

fn final_reply(text: &str) -> Value {
    json!({
        "role":"assistant",
        "content":[{"type":"text","text":text}],
        "stopReason":"stop"
    })
}

fn summary_reply() -> Value {
    json!({
        "role":"assistant",
        "content":[{"type":"text","text":danso::compaction::empty_summary().to_string()}],
        "stopReason":"stop"
    })
}

#[tokio::test]
async fn pause_and_explicit_resume_keep_prompt_and_effects_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task.jsonl");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls: Rc::clone(&calls),
        outputs: RefCell::new(VecDeque::from(["tool-result".into()])),
    };
    let task_limits = Limits {
        stage_requests: 1,
        ..limits()
    };
    let mut first = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([tool_reply("c1")]),
        calls: 0,
        delay: None,
    };
    let mut sink = Sink::default();
    let error = runtime::run(
        input(
            "do the task once",
            LongTaskRun {
                limits: task_limits,
                explicit_limits: 31,
                resume: false,
                pause_after_stage: Some(1),
            },
        ),
        &mut provider,
        &executor,
        &mut first,
        &mut sink,
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert_eq!(provider.calls, 1);
    assert_eq!(calls.borrow().as_slice(), ["c1"]);
    assert_eq!(sink.task_states, ["checkpoint", "checkpoint", "paused"]);
    drop(first);

    let mut resumed = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([final_reply("done")]),
        calls: 0,
        delay: None,
    };
    let mut sink = Sink::default();
    runtime::run(
        input(
            "",
            LongTaskRun {
                limits: task_limits,
                explicit_limits: 31,
                resume: true,
                pause_after_stage: None,
            },
        ),
        &mut provider,
        &executor,
        &mut resumed,
        &mut sink,
        &mut Usage::default(),
    )
    .await
    .unwrap();
    assert_eq!(provider.calls, 1);
    assert_eq!(calls.borrow().as_slice(), ["c1"]);
    let messages = resumed.messages().unwrap();
    assert_eq!(messages.iter().filter(|m| m["role"] == "user").count(), 1);
    drop(resumed);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "completed");
    assert_eq!(status["resume_allowed"], false);
}

#[tokio::test]
async fn graceful_pause_flag_stops_before_first_provider_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("signal.jsonl");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls,
        outputs: RefCell::new(VecDeque::new()),
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([final_reply("must not dispatch")]),
        calls: 0,
        delay: None,
    };
    let requested = AtomicBool::new(true);
    let mut run_input = input(
        "pause before dispatch",
        LongTaskRun {
            limits: limits(),
            explicit_limits: 31,
            resume: false,
            pause_after_stage: None,
        },
    );
    run_input.pause_requested = Some(&requested);
    let error = runtime::run(
        run_input,
        &mut provider,
        &executor,
        &mut session,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert_eq!(provider.calls, 0);
    drop(session);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "paused");
    assert_eq!(status["resume_allowed"], true);
}

#[tokio::test]
async fn compaction_summary_can_overshoot_stage_target_before_tool_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stage-edge.jsonl");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls: Rc::clone(&calls),
        outputs: RefCell::new(VecDeque::from(["result".into()])),
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    session
        .message(json!({"role":"user","content":"old"}))
        .unwrap();
    session
        .message(json!({
            "role":"assistant",
            "content":[{"type":"text","text":"history ".repeat(4000)}],
            "stopReason":"stop"
        }))
        .unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([tool_reply("stage-edge")]),
        calls: 0,
        delay: None,
    };
    let mut run_input = input(
        "continue after compaction",
        LongTaskRun {
            limits: Limits {
                stage_requests: 1,
                max_requests: 16,
                ..limits()
            },
            explicit_limits: 31,
            resume: false,
            pause_after_stage: Some(1),
        },
    );
    run_input.compact_at_bytes = Some(8192);
    let error = runtime::run(
        run_input,
        &mut provider,
        &executor,
        &mut session,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert!(provider.calls > 1);
    assert_eq!(calls.borrow().as_slice(), ["stage-edge"]);
    drop(session);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "paused");
    assert_eq!(status["stage"], 1);
    assert_eq!(
        status["usage"]["requests"].as_u64(),
        Some(provider.calls as u64)
    );
}

#[tokio::test]
async fn reported_token_budget_applies_to_compaction_without_action_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("summary-quota.jsonl");
    let executor = FakeExecutor {
        calls: Rc::new(RefCell::new(Vec::new())),
        outputs: RefCell::new(VecDeque::new()),
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    session
        .message(json!({"role":"user","content":"old"}))
        .unwrap();
    session
        .message(json!({
            "role":"assistant",
            "content":[{"type":"text","text":"history ".repeat(4000)}],
            "stopReason":"stop"
        }))
        .unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([final_reply("must not dispatch")]),
        calls: 0,
        delay: None,
    };
    let mut run_input = input(
        "compact with no remaining tokens",
        LongTaskRun {
            limits: Limits {
                max_tokens: 3,
                ..limits()
            },
            explicit_limits: 31,
            resume: false,
            pause_after_stage: None,
        },
    );
    run_input.compact_at_bytes = Some(8192);
    let error = runtime::run(
        run_input,
        &mut provider,
        &executor,
        &mut session,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert_eq!(provider.calls, 1);
    drop(session);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "paused");
    assert_eq!(status["usage"]["requests"], 1);
    assert_eq!(status["resume_allowed"], false);
}

#[tokio::test]
async fn cancelled_provider_requires_explicit_resume_and_preserves_unknown_usage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pending.jsonl");
    let limits = limits();
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls,
        outputs: RefCell::new(VecDeque::new()),
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([final_reply("never reaches journal")]),
        calls: 0,
        delay: Some(Duration::from_millis(100)),
    };
    let mut sink = Sink::default();
    let mut usage = Usage::default();
    let run = runtime::run(
        input(
            "pending request",
            LongTaskRun {
                limits,
                explicit_limits: 31,
                resume: false,
                pause_after_stage: None,
            },
        ),
        &mut provider,
        &executor,
        &mut session,
        &mut sink,
        &mut usage,
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), run)
            .await
            .is_err()
    );
    drop(session);

    let mut resumed = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut retry = FakeProvider {
        replies: VecDeque::from([final_reply("must not replay")]),
        calls: 0,
        delay: None,
    };
    runtime::run(
        input(
            "",
            LongTaskRun {
                limits,
                explicit_limits: 31,
                resume: true,
                pause_after_stage: None,
            },
        ),
        &mut retry,
        &executor,
        &mut resumed,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap();
    assert_eq!(retry.calls, 1);
    drop(resumed);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["usage"]["requests"], 2);
    assert_eq!(status["usage"]["unknown_usage_requests"], 1);
    assert_eq!(status["state"], "completed");
}

#[tokio::test]
async fn repeated_batches_stop_before_the_fourth_provider_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repeat.jsonl");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls: Rc::clone(&calls),
        outputs: RefCell::new(VecDeque::from([
            "same".into(),
            "same".into(),
            "same".into(),
        ])),
    };
    let mut provider = FakeProvider {
        replies: VecDeque::from([
            tool_reply("c1"),
            tool_reply("c2"),
            tool_reply("c3"),
            final_reply("bad"),
        ]),
        calls: 0,
        delay: None,
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let error = runtime::run(
        input(
            "repeat guard",
            LongTaskRun {
                limits: Limits {
                    stage_requests: 16,
                    ..limits()
                },
                explicit_limits: 31,
                resume: false,
                pause_after_stage: None,
            },
        ),
        &mut provider,
        &executor,
        &mut session,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::Runtime));
    assert_eq!(provider.calls, 3);
    assert_eq!(calls.borrow().len(), 3);
    drop(session);
    assert_eq!(Session::read_status(&path).unwrap()["state"], "failed");
}

#[tokio::test]
async fn reported_token_limit_stops_before_next_http_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quota.jsonl");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls,
        outputs: RefCell::new(VecDeque::from(["done".into()])),
    };
    let limits = Limits {
        max_tokens: 3,
        ..limits()
    };
    let mut provider = FakeProvider {
        replies: VecDeque::from([tool_reply("c1"), final_reply("must not happen")]),
        calls: 0,
        delay: None,
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let error = runtime::run(
        input(
            "quota",
            LongTaskRun {
                limits,
                explicit_limits: 31,
                resume: false,
                pause_after_stage: None,
            },
        ),
        &mut provider,
        &executor,
        &mut session,
        &mut Sink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert_eq!(provider.calls, 1);
    drop(session);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "paused");
    assert_eq!(status["resume_allowed"], false);
}

#[test]
fn status_is_read_only_and_rejects_expired_budget_without_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expired.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let user = session
        .message(json!({"role":"user","content":"expired"}))
        .unwrap();
    let user_id = user["id"].as_str().unwrap();
    let limits = Limits {
        wall_seconds: 1,
        ..limits()
    };
    session
        .record_long_task(limits.json(session.header()["id"].as_str().unwrap(), user_id))
        .unwrap();
    session
        .record_long_task(
            json!({"version":1,"event":"request_started","sequence":1,"stage":0,"elapsed_ms":1000}),
        )
        .unwrap();
    session
        .record_long_task(json!({"version":1,"event":"request_settled","sequence":1,"stage":0,"elapsed_ms":1000,"response_kind":"summary","input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"total_tokens":0}))
        .unwrap();
    session
        .record_long_task(json!({"version":1,"event":"paused","reason":"wall_timeout","stage":0,"elapsed_ms":1000}))
        .unwrap();
    drop(session);
    let before = std::fs::read(&path).unwrap();
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "paused");
    assert_eq!(status["resume_allowed"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// Issue #70 D: the short-mode repeat guard guides once at the threshold and
/// terminates on the next identical batch. Run-local: the journal records no
/// long-task state and no task events are emitted.
struct RecordingProvider {
    replies: VecDeque<Value>,
    systems: RefCell<Vec<String>>,
}
impl Provider for RecordingProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
        self.systems.borrow_mut().push(request.system.to_string());
        self.replies
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected fake provider request"))
    }
}

#[tokio::test]
async fn short_mode_repeat_guard_guides_once_then_terminates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let replies: VecDeque<Value> = ["call1", "call2", "call3", "call4"]
        .iter()
        .map(|id| tool_reply(id))
        .collect();
    let mut provider = RecordingProvider {
        replies,
        systems: RefCell::new(vec![]),
    };
    let executor = FakeExecutor {
        calls: Rc::new(RefCell::new(vec![])),
        outputs: RefCell::new(VecDeque::new()),
    };
    let mut sink = Sink::default();
    let input = RunInput {
        no_tools: false,
        prompt: "probe the fixture",
        context: "",
        execution_context: "",
        max_turns: 8,
        compact_at_bytes: None,
        refresh_context: None,
        long_task: None,
        repeat_limit: 3,
        continuation_limit: 0,
        stream_requests: false,
        report_progress: false,
        pause_requested: None,
        cancellation_reason: None,
    };
    let error = runtime::run(
        input,
        &mut provider,
        &executor,
        &mut session,
        &mut sink,
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure::category(&error), Some(Kind::RequestBudget));
    assert_eq!(
        provider.systems.borrow().len(),
        4,
        "guide request + terminating repeat"
    );
    // Requests 1-3 are guidance-free; request 4 carries the one-time notice.
    let systems = provider.systems.borrow();
    for system in &systems[..3] {
        assert!(!system.contains("Identical tool batch"), "{system}");
    }
    assert!(
        systems[3].contains("Identical tool batch repeated 3 times"),
        "{}",
        systems[3]
    );
    assert!(sink.task_states.is_empty(), "no long-task task events");
    assert!(
        session.long_task_records().unwrap().is_empty(),
        "the guard is run-local and writes no long-task journal records"
    );
    // The final answer path never ran: no terminal stop reply was consumed.
}

#[test]
fn recovery_advice_distinguishes_checkpoints_uncertainty_and_budgets() {
    use danso::long_task::{Ledger, State};
    for (state, reason, allowed) in [
        (State::Ready, "explicit_resume_required", true),
        (State::Paused, "explicit_resume_required", true),
        (State::PendingProvider, "uncertain_work", false),
        (State::PendingTools, "uncertain_work", false),
        (State::FinalPending, "uncertain_work", false),
        (State::Completed, "terminal_task", false),
        (State::Failed, "terminal_task", false),
    ] {
        let mut ledger = Ledger {
            limits: Some(limits()),
            state,
            ..Default::default()
        };
        let error = failure::at(Kind::Session)(ledger.recovery_error()).context("PRIVATE context");
        let diagnostic = failure::task_recovery(&error).unwrap();
        assert_eq!(
            serde_json::to_value(diagnostic).unwrap(),
            json!({
                "version": 1, "state": state.as_str(), "reason": reason,
                "resume_allowed": allowed,
                "action": if allowed { "resume_task" } else { "new_session" },
            })
        );
        assert_eq!(ledger.ensure_safe_resume().is_ok(), allowed);
        if allowed {
            for budget in 0..4 {
                ledger = Ledger {
                    limits: Some(limits()),
                    state,
                    ..Default::default()
                };
                match budget {
                    0 => ledger.elapsed_ms = limits().wall_seconds * 1000,
                    1 => ledger.requests = limits().max_requests,
                    2 => ledger.reported_tokens = limits().max_tokens,
                    _ => ledger.repeat_count = limits().repeat_limit,
                }
                let error = ledger.ensure_safe_resume().unwrap_err();
                let record = serde_json::to_value(failure::task_recovery(&error).unwrap()).unwrap();
                assert_eq!(record["reason"], "budget_exhausted");
                assert_eq!(record["resume_allowed"], false);
                assert_eq!(record["action"], "new_session");
            }
        }
    }
    assert!(failure::task_recovery(&anyhow::anyhow!("DANSO_RECOVERY=PRIVATE")).is_none());
}

#[tokio::test]
async fn repeated_provider_cancellation_consumes_durable_budget_without_tools() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancelled.jsonl");
    let effects = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls: effects.clone(),
        outputs: RefCell::new(VecDeque::new()),
    };
    for attempt in 0..3 {
        let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
        let mut provider = FakeProvider {
            replies: VecDeque::new(),
            calls: 0,
            delay: Some(Duration::from_secs(1)),
        };
        let mut sink = Sink::default();
        let mut usage = Usage::default();
        let run = runtime::run(
            input(
                if attempt == 0 {
                    "original objective"
                } else {
                    ""
                },
                LongTaskRun {
                    limits: limits(),
                    explicit_limits: 31,
                    resume: attempt > 0,
                    pause_after_stage: None,
                },
            ),
            &mut provider,
            &executor,
            &mut session,
            &mut sink,
            &mut usage,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), run)
                .await
                .is_err()
        );
        drop(session);
        let status = Session::read_status(&path).unwrap();
        assert_eq!(status["usage"]["requests"], attempt + 1);
        assert_eq!(status["usage"]["unknown_usage_requests"], attempt + 1);
        assert_eq!(status["resume_allowed"], attempt < 2);
        assert!(status["elapsed_ms"].as_u64().unwrap() >= (attempt + 1) * 10);
    }
    let before = std::fs::read(&path).unwrap();
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::new(),
        calls: 0,
        delay: None,
    };
    assert!(
        runtime::run(
            input(
                "",
                LongTaskRun {
                    limits: limits(),
                    explicit_limits: 31,
                    resume: true,
                    pause_after_stage: None
                }
            ),
            &mut provider,
            &executor,
            &mut session,
            &mut Sink::default(),
            &mut Usage::default()
        )
        .await
        .is_err()
    );
    assert_eq!(provider.calls, 0);
    assert!(effects.borrow().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn output_failure_after_assistant_append_cannot_be_reclassified_as_provider_wait() {
    struct BrokenSink;
    impl EventSink for BrokenSink {
        fn emit(&mut self, event: Event<'_>) -> Result<()> {
            if let Event::Message(entry) = event {
                if entry["message"]["role"] == "assistant" {
                    anyhow::bail!("synthetic sink failure");
                }
            }
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output.jsonl");
    let effects = Rc::new(RefCell::new(Vec::new()));
    let executor = FakeExecutor {
        calls: effects.clone(),
        outputs: RefCell::new(VecDeque::new()),
    };
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = FakeProvider {
        replies: VecDeque::from([final_reply("durable")]),
        calls: 0,
        delay: None,
    };
    assert!(
        runtime::run(
            input(
                "goal",
                LongTaskRun {
                    limits: limits(),
                    explicit_limits: 31,
                    resume: false,
                    pause_after_stage: None
                }
            ),
            &mut provider,
            &executor,
            &mut session,
            &mut BrokenSink,
            &mut Usage::default()
        )
        .await
        .is_err()
    );
    drop(session);
    let status = Session::read_status(&path).unwrap();
    assert_eq!(status["state"], "pending_provider");
    assert_eq!(status["resume_allowed"], false);
    assert_eq!(status["usage"]["unknown_usage_requests"], 0);
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    // Even a syntactically valid interruption record must not cross the
    // assistant append, whether or not that assistant contains tool calls.
    session
        .record_long_task(json!({"version":1,"event":"provider_interrupted",
        "sequence":1,"stage":0,"elapsed_ms":1,"reason":"unknown","request_kind":"action"}))
        .unwrap();
    assert!(session.check_recovery().is_err());
    drop(session);
    assert!(Session::read_status(&path).is_err());
    assert!(effects.borrow().is_empty());
}
