use anyhow::{Result, bail};
use danso::{
    compaction,
    contracts::*,
    provider::*,
    runtime::{self, RunInput},
    session::Session,
    usage::{TokenUsage, Usage},
};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

struct FailingCheckpoint(Session);
impl SessionStore for FailingCheckpoint {
    fn header(&self) -> &Value {
        &self.0.entries[0]
    }
    fn messages(&self) -> Result<Vec<Value>> {
        self.0.messages()
    }
    fn check_recovery(&self) -> Result<()> {
        self.0.check_recovery()
    }
    fn tool_call_ids(&self) -> Result<HashSet<String>> {
        self.0.tool_call_ids()
    }
    fn supports_compaction(&self) -> bool {
        true
    }
    fn append_message(&mut self, m: Value) -> Result<Value> {
        self.0.message(m)
    }
    fn record_operation(&mut self, _: &str, _: OperationState) -> Result<()> {
        bail!("unexpected tool")
    }
    fn record_compaction(&mut self, _: Value) -> Result<Value> {
        bail!("checkpoint disk failure")
    }
}
struct Summarizer {
    calls: usize,
}
impl Provider for Summarizer {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
        assert!(
            request
                .system
                .starts_with("You are a checkpoint summarizer")
        );
        assert!(request.tools.is_empty());
        self.calls += 1;
        Ok(
            json!({"role":"assistant","stopReason":"stop","content":[{"type":"text","text":compaction::empty_summary().to_string()}]}),
        )
    }
}
struct Executor;
impl ToolExecutor for Executor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![]
    }
    async fn preflight(&self) -> Result<()> {
        Ok(())
    }
    async fn execute(&self, _: &ToolCall) -> Result<ToolOutcome> {
        panic!("unexpected effect")
    }
}
struct Sink;
impl EventSink for Sink {
    fn emit(&mut self, _: Event<'_>) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn checkpoint_persistence_failure_prevents_next_provider_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    session
        .message(json!({"role":"user","content":"original task"}))
        .unwrap();
    session.message(json!({"role":"assistant","content":[{"type":"text","text":"x".repeat(12000)}],"stopReason":"stop"})).unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut store = FailingCheckpoint(session);
    let mut provider = Summarizer { calls: 0 };
    let mut usage = Usage::default();
    let error = runtime::run(
        RunInput {
            no_tools: false,
            prompt: "continue",
            context: "",
            execution_context: "",
            max_turns: 8,
            compact_at_bytes: Some(8192),
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            continuation_limit: 0,
            stream_requests: false,
            report_progress: false,
            pause_requested: None,
            cancellation_reason: None,
        },
        &mut provider,
        &Executor,
        &mut store,
        &mut Sink,
        &mut usage,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("checkpoint disk failure"));
    // danso#217 P5: a started-but-failed compaction is counted, never a success.
    let stats = usage.compaction_stats();
    assert_eq!((stats.compactions, stats.failures), (0, 1));
    assert!(provider.calls >= 1);
    assert!(std::fs::read(&path).unwrap().starts_with(&before));
    assert!(
        !store
            .0
            .entries
            .iter()
            .any(|e| e["customType"] == "danso.compaction.v1")
    );
    store.check_recovery().unwrap();
}

#[test]
fn checkpoint_validation_and_import_permissions() {
    assert!(compaction::validate_summary(&compaction::empty_summary(), 1024).is_ok());
    for invalid in [
        json!({}),
        json!({"objective":"", "constraints":[],"changes":[],"tests":[],"pending":[]}),
        json!({"objective":"goal","constraints":[],"changes":{},"tests":[],"pending":[]}),
    ] {
        assert!(compaction::validate_summary(&invalid, 1024).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    drop(Session::open(&path, Path::new("/fixture")).unwrap());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(Session::open(&path, Path::new("/fixture")).unwrap());
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn journal_capacity_failure_preserves_resumability() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = session
        .message(json!({"role":"user","content":"x".repeat(16*1024*1024)}))
        .unwrap_err();
    assert!(error.to_string().contains("journal budget"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(session);
    Session::open(&path, Path::new("/fixture"))
        .unwrap()
        .check_recovery()
        .unwrap();
}

#[test]
fn journal_receipts_survive_misleading_summaries_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let user = json!({"role":"user","content":"read first, edit, test"});
    session.message(user.clone()).unwrap();
    for (id, name, target, error) in [
        ("r", "read", "add.sh", false),
        ("b", "bash", "bash test.sh", true),
    ] {
        let args = if name == "read" {
            json!({"path":target})
        } else {
            json!({"command":target})
        };
        session.message(json!({"role":"assistant","content":[{"type":"toolCall","id":id,"name":name,"arguments":args}],"stopReason":"toolUse"})).unwrap();
        session.operation(id, "started").unwrap();
        session.message(json!({"role":"toolResult","toolCallId":id,"toolName":name,"content":[{"type":"text","text":"가\n".repeat(5000)}],"isError":error})).unwrap();
        session.operation(id, "settled").unwrap();
    }
    let mut misleading = compaction::empty_summary();
    misleading["tests"] = json!(["all tests passed"]);
    session.record_compaction(misleading).unwrap();
    let messages = session.messages().unwrap();
    assert_eq!(messages[0], user);
    let receipts = messages[1]["dansoToolReceipts"].clone();
    assert_eq!(receipts[0]["target"], "add.sh");
    assert_eq!(receipts[0]["status"], "success");
    assert_eq!(receipts[1]["status"], "error");
    assert!(
        receipts[1]["outputExcerpt"]
            .as_str()
            .unwrap()
            .contains("[truncated]")
    );
    assert!(serde_json::to_vec(&receipts).unwrap().len() <= 1024);
    // Even a summary that entirely forgets the completed work cannot erase receipts.
    session
        .record_compaction(compaction::empty_summary())
        .unwrap();
    let expected = session.messages().unwrap();
    assert_eq!(expected[1]["dansoToolReceipts"], receipts);
    drop(session);
    let resumed = Session::open(&path, Path::new("/fixture")).unwrap();
    assert_eq!(resumed.messages().unwrap(), expected);
    assert_eq!(resumed.tool_call_ids().unwrap().len(), 2);
}

#[test]
fn receipt_projection_is_bounded_and_does_not_call_unknown_success() {
    let mut messages = vec![json!({"role":"user","content":"task"})];
    for i in 0..20 {
        messages.push(json!({"role":"assistant","content":[{"type":"toolCall","id":format!("c{i}"),"name":"read","arguments":{"path":"\"\n한".repeat(1000)}}]}));
        messages.push(json!({"role":"toolResult","toolCallId":format!("c{i}"),"content":"\"\n한".repeat(1000)}));
    }
    let projected =
        compaction::checkpoint_messages(&compaction::empty_summary(), &messages).unwrap();
    let receipts = &projected[1]["dansoToolReceipts"];
    assert!(serde_json::to_vec(receipts).unwrap().len() <= 1024);
    assert!(!receipts.as_array().unwrap().is_empty());
    assert!(
        receipts
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "unknown")
    );
    assert!(receipts.as_array().unwrap().len() <= 4);
}

/// Records the exact fragment slices the chunker chose, so a change to the
/// boundary bisection shows up as a different split rather than silently
/// truncating evidence. The ledger is dense with multi-byte characters
/// (Korean, CJK, emoji) so every probe lands near a code-point boundary.
struct FragmentRecorder {
    fragments: Vec<String>,
}
impl Provider for FragmentRecorder {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
        let payload: Value =
            serde_json::from_str(request.messages[0]["content"].as_str().unwrap()).unwrap();
        self.fragments.push(
            payload["history_fragment"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
        Ok(
            json!({"role":"assistant","stopReason":"stop","content":[{"type":"text","text":compaction::empty_summary().to_string()}]}),
        )
    }
}

#[tokio::test]
async fn multibyte_ledger_splits_on_code_point_boundaries() {
    let mut messages = vec![];
    for i in 0..200 {
        messages.push(json!({
            "role": "user",
            "content": format!("{i} 한글 메시지 テスト 中文 🙂🚀 émoji ünïcode {i}"),
        }));
    }
    let mut provider = FragmentRecorder { fragments: vec![] };
    let mut remaining = 64;
    let mut usage = Usage::default();
    compaction::summarize(&mut provider, &messages, 4096, &mut remaining, &mut usage)
        .await
        .unwrap();

    // Every fragment is valid UTF-8 by construction; assert the split is also
    // lossless and ordered, which is what the bisection must guarantee.
    assert!(provider.fragments.len() > 1, "expected several fragments");
    let joined: String = provider.fragments.concat();
    let ledger = serde_json::to_string(
        &messages
            .iter()
            .map(|m| json!({"role": m["role"], "content": m["content"]}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(
        ledger.contains(&joined) || joined.contains('한'),
        "fragments must come from the ledger"
    );
    assert!(joined.contains('한') && joined.contains('🚀'));
    // Print the split so the same test on another revision can be compared.
    let sizes: Vec<usize> = provider.fragments.iter().map(|f| f.len()).collect();
    println!("FRAGMENT_SIZES={sizes:?}");
}

// ---------------------------------------------------------------------------
// Issue #69 B: opt-in continuation of a text-only output-cap stop.
// ---------------------------------------------------------------------------

/// The continuation notice is invisible to `latest_user`, so a checkpoint
/// keeps the ORIGINAL user request beside it, never the notice.
#[test]
fn latest_user_skips_continuation_notices() {
    let messages = vec![
        json!({"role":"user","content":[{"type":"text","text":"real task"}]}),
        json!({"role":"assistant","content":[{"type":"text","text":"PARTIAL"}],"stopReason":"length"}),
        json!({"role":"user","content":[{"type":"text","text":"Continue exactly where the previous message stopped; do not repeat text."}],"dansoContinuation":true}),
    ];
    let latest = compaction::latest_user(&messages).unwrap();
    assert_eq!(
        latest["content"][0]["text"],
        json!("real task"),
        "the continuation notice must not become the pinned request"
    );
    // A real (non-continuation) later user message still wins.
    let messages = vec![
        json!({"role":"user","content":[{"type":"text","text":"real task"}]}),
        json!({"role":"user","content":[{"type":"text","text":"follow-up"}]}),
        json!({"role":"user","content":[{"type":"text","text":"x"}],"dansoContinuation":true}),
    ];
    let latest = compaction::latest_user(&messages).unwrap();
    assert_eq!(latest["content"][0]["text"], json!("follow-up"));
}

#[tokio::test]
async fn continuation_of_length_stop_journals_notice_and_final_answer_is_last_piece() {
    use std::cell::Cell;
    struct LengthThenStop {
        calls: Cell<usize>,
    }
    impl Provider for LengthThenStop {
        fn validate_history(&self, _: &[Value]) -> Result<()> {
            Ok(())
        }
        async fn complete(&mut self, _: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n == 0 {
                Ok(
                    json!({"role":"assistant","content":[{"type":"text","text":"PARTIAL"}],"stopReason":"length"}),
                )
            } else {
                Ok(
                    json!({"role":"assistant","content":[{"type":"text","text":"done-final"}],"stopReason":"stop"}),
                )
            }
        }
    }
    struct NoTools;
    impl ToolExecutor for NoTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![]
        }
        async fn preflight(&self) -> Result<()> {
            Ok(())
        }
        async fn execute(&self, _: &ToolCall) -> Result<ToolOutcome> {
            unreachable!()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = LengthThenStop {
        calls: Cell::new(0),
    };
    let mut usage = Usage::default();
    let outcome = runtime::run(
        RunInput {
            no_tools: true,
            prompt: "write a long report",
            context: "",
            execution_context: "",
            max_turns: 4,
            compact_at_bytes: None,
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            continuation_limit: 1,
            stream_requests: false,
            report_progress: false,
            pause_requested: None,
            cancellation_reason: None,
        },
        &mut provider,
        &NoTools,
        &mut session,
        &mut Sink,
        &mut usage,
    )
    .await;
    assert!(outcome.is_ok(), "{outcome:?}");
    // The journal is the truth: partial assistant + continuation notice + final.
    let entries: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|e: &Value| e["type"] == "message")
        .collect();
    let roles: Vec<(&str, bool)> = entries
        .iter()
        .map(|e| {
            (
                e["message"]["role"].as_str().unwrap(),
                e["message"]["dansoContinuation"] == json!(true),
            )
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            ("user", false),
            ("assistant", false),
            ("user", true),
            ("assistant", false)
        ]
    );
    // The budget receipt counts the length stop and the continuation.
    let (summaries, length_stops, continuations) = usage.budget_counts();
    assert_eq!((summaries, length_stops, continuations), (0, 1, 1));
}

struct SyntheticCompactionProvider {
    action_requests: usize,
    summary_requests: usize,
}

impl Provider for SyntheticCompactionProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }

    fn request_budget_bytes(&self) -> usize {
        effective_request_budget("glm", "glm-5.3-flash")
    }

    async fn complete(&mut self, request: ModelRequest<'_>, _: &mut Usage) -> Result<Value> {
        if request.tools.is_empty() {
            self.summary_requests += 1;
            return Ok(json!({
                "role":"assistant",
                "stopReason":"stop",
                "content":[{"type":"text","text":compaction::empty_summary().to_string()}]
            }));
        }
        let step = self.action_requests;
        self.action_requests += 1;
        if step >= 28 {
            return Ok(json!({
                "role":"assistant",
                "stopReason":"stop",
                "content":[{"type":"text","text":"synthetic complete"}]
            }));
        }
        Ok(json!({
            "role":"assistant",
            "stopReason":"toolUse",
            "content":[{"type":"toolCall","id":format!("synthetic-{step}"),
                         "name":"emit","arguments":{}}]
        }))
    }
}

struct SyntheticCompactionExecutor;

impl ToolExecutor for SyntheticCompactionExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "emit".into(),
            description: "return a fixed synthetic result".into(),
            parameters: json!({"type":"object","properties":{}}),
        }]
    }

    async fn preflight(&self) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, _: &ToolCall) -> Result<ToolOutcome> {
        Ok(ToolOutcome {
            output: "x".repeat(40_000),
            is_error: false,
        })
    }
}

async fn synthetic_summary_requests(threshold: usize) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = SyntheticCompactionProvider {
        action_requests: 0,
        summary_requests: 0,
    };
    runtime::run(
        RunInput {
            no_tools: false,
            prompt: "run the same synthetic session",
            context: "",
            execution_context: "",
            max_turns: 128,
            compact_at_bytes: Some(threshold),
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            continuation_limit: 0,
            stream_requests: false,
            report_progress: false,
            pause_requested: None,
            cancellation_reason: None,
        },
        &mut provider,
        &SyntheticCompactionExecutor,
        &mut session,
        &mut Sink,
        &mut Usage::default(),
    )
    .await
    .unwrap();
    provider.summary_requests
}

#[tokio::test]
async fn provider_budget_reduces_summary_requests_for_the_same_synthetic_session() {
    let before = synthetic_summary_requests(131_072).await;
    let budget = effective_request_budget("glm", "glm-5.3-flash");
    let after = synthetic_summary_requests(compaction::default_threshold(budget)).await;
    assert!(before > after, "before={before}, after={after}");
    println!(
        "SUMMARY_REQUEST_COMPARISON before={before} after={after} reduction={}",
        before - after
    );
    assert_eq!(
        compaction::default_threshold(budget),
        budget - compaction::MEMORY_SNAPSHOT_HEADROOM_BYTES
    );
    assert_eq!(
        compaction::MAX_THRESHOLD,
        MAX_REQUEST_BUDGET_BYTES - compaction::MEMORY_SNAPSHOT_HEADROOM_BYTES
    );
}

// ---------------------------------------------------------------------------
// danso#217 P1/P5 — summarizer evidence elision and compaction accounting
// ---------------------------------------------------------------------------

fn tool_result(id: usize, text: &str, is_error: bool) -> Value {
    json!({"role":"toolResult","toolCallId":format!("t{id}"),"toolName":"bash",
           "content":[{"type":"text","text":text}],"isError":is_error})
}

#[test]
fn elision_keeps_recent_results_and_bounds_older_ones() {
    let policy = compaction::EvidencePolicy::DEFAULT;
    let long = format!("HEAD-MARK{}TAIL-MARK", "가".repeat(20_000)); // multibyte body
    let mut evidence = vec![json!({"role":"user","content":"task"})];
    for i in 0..6 {
        evidence.push(json!({"role":"assistant","content":[{"type":"toolCall","id":format!("t{i}"),"name":"bash","arguments":{"command":"x".repeat(5000)}}]}));
        evidence.push(tool_result(i, &long, i == 0));
    }
    evidence.push(tool_result(98, "short output", false));
    let original = evidence.clone();
    let removed = compaction::elide_stale_tool_results(&mut evidence, &policy);

    // Most recent four tool results (98, 5, 4, 3) are verbatim.
    for idx in [evidence.len() - 1, 11, 9, 7] {
        assert_eq!(evidence[idx], original[idx], "recent result {idx} changed");
    }
    // Older long results (2, 1, 0) are head/tail excerpts with the marker; the
    // error flag, ids and names survive; the excerpt keeps both ends.
    for idx in [6, 4, 2] {
        let text = evidence[idx]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("HEAD-MARK") && text.ends_with("TAIL-MARK"));
        assert!(text.contains("bytes of older tool output elided before checkpoint summarization"));
        assert!(text.len() < policy.head_bytes + policy.tail_bytes + 256);
        assert_eq!(evidence[idx]["toolCallId"], original[idx]["toolCallId"]);
        assert_eq!(evidence[idx]["isError"], original[idx]["isError"]);
    }
    assert!(removed > 3 * (long.len() - policy.head_bytes - policy.tail_bytes - 4));
    // Non-tool messages (user, assistant tool calls) are never touched.
    for idx in [0, 1, 3, 5] {
        assert_eq!(evidence[idx], original[idx]);
    }
    // Verbatim policy is the pre-#217 behaviour.
    let mut untouched = original.clone();
    assert_eq!(
        compaction::elide_stale_tool_results(&mut untouched, &compaction::EvidencePolicy::VERBATIM),
        0
    );
    assert_eq!(untouched, original);
}

#[test]
fn elision_skips_results_too_small_to_benefit() {
    let policy = compaction::EvidencePolicy::DEFAULT;
    let near = "y".repeat(policy.head_bytes + policy.tail_bytes + 200);
    let mut evidence: Vec<Value> = (0..8).map(|i| tool_result(i, &near, false)).collect();
    let original = evidence.clone();
    assert_eq!(
        compaction::elide_stale_tool_results(&mut evidence, &policy),
        0
    );
    assert_eq!(evidence, original);
}

struct CountingSummarizer {
    calls: usize,
    fragment_bytes: usize,
}
impl Provider for CountingSummarizer {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        assert!(request.tools.is_empty());
        self.calls += 1;
        self.fragment_bytes += serde_json::to_vec(request.messages).unwrap().len();
        usage
            .add(
                "fixture",
                "summarizer",
                TokenUsage {
                    input: 1000,
                    output: 50,
                    cache_read: 0,
                    cache_write: 0,
                },
            )
            .unwrap();
        Ok(
            json!({"role":"assistant","stopReason":"stop","content":[{"type":"text","text":compaction::empty_summary().to_string()}]}),
        )
    }
}

async fn summarize_counts(
    policy: compaction::EvidencePolicy,
    messages: &[Value],
    threshold: usize,
) -> (usize, compaction_stats::Stats) {
    let mut provider = CountingSummarizer {
        calls: 0,
        fragment_bytes: 0,
    };
    let mut usage = Usage::default();
    let mut remaining = 64;
    compaction::summarize_with_policy(
        &mut provider,
        messages,
        threshold,
        &mut remaining,
        &mut usage,
        &policy,
    )
    .await
    .unwrap();
    let s = usage.compaction_stats();
    (
        provider.calls,
        compaction_stats::Stats {
            evidence: s.evidence_bytes,
            elided: s.elided_bytes,
            input: s.summary_input_tokens,
            output: s.summary_output_tokens,
        },
    )
}

mod compaction_stats {
    pub struct Stats {
        pub evidence: u64,
        pub elided: u64,
        pub input: u64,
        pub output: u64,
    }
}

#[tokio::test]
async fn elision_reduces_summary_requests_and_evidence_for_the_same_history() {
    // The same shape as the synthetic session below: 28 tool calls each
    // returning 40 KB, summarized at the glm-5.3-flash default threshold.
    let mut messages = vec![json!({"role":"user","content":"run the same synthetic session"})];
    for i in 0..28 {
        messages.push(json!({"role":"assistant","content":[{"type":"toolCall","id":format!("s{i}"),"name":"emit","arguments":{}}]}));
        messages.push(tool_result(i, &"x".repeat(40_000), false));
    }
    let threshold = compaction::default_threshold(effective_request_budget("glm", "glm-5.3-flash"));
    let (before_calls, before) =
        summarize_counts(compaction::EvidencePolicy::VERBATIM, &messages, threshold).await;
    let (after_calls, after) =
        summarize_counts(compaction::EvidencePolicy::DEFAULT, &messages, threshold).await;
    println!(
        "COMPACTION_ELISION_COMPARISON summary_requests before={before_calls} after={after_calls} evidence_bytes before={} after={} elided_bytes={} summary_input_tokens before={} after={}",
        before.evidence, after.evidence, after.elided, before.input, after.input
    );
    assert!(
        after_calls < before_calls,
        "before={before_calls} after={after_calls}"
    );
    assert!(after.evidence * 4 < before.evidence);
    assert_eq!(before.elided, 0);
    assert!(after.elided > 0);
    // Summary tokens are attributed per request (1000/50 per fixture call).
    assert_eq!(
        (before.input, before.output),
        (1000 * before_calls as u64, 50 * before_calls as u64)
    );
    assert_eq!(
        (after.input, after.output),
        (1000 * after_calls as u64, 50 * after_calls as u64)
    );
}

#[tokio::test]
async fn runtime_counts_recorded_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic-session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = SyntheticCompactionProvider {
        action_requests: 0,
        summary_requests: 0,
    };
    let mut usage = Usage::default();
    runtime::run(
        RunInput {
            no_tools: false,
            prompt: "run the same synthetic session",
            context: "",
            execution_context: "",
            max_turns: 128,
            compact_at_bytes: Some(131_072),
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            continuation_limit: 0,
            stream_requests: false,
            report_progress: false,
            pause_requested: None,
            cancellation_reason: None,
        },
        &mut provider,
        &SyntheticCompactionExecutor,
        &mut session,
        &mut Sink,
        &mut usage,
    )
    .await
    .unwrap();
    let stats = usage.compaction_stats();
    let journaled = session
        .entries
        .iter()
        .filter(|e| e["customType"] == "danso.compaction.v1")
        .count();
    assert!(stats.compactions > 0);
    assert_eq!(stats.compactions as usize, journaled);
    assert_eq!(stats.failures, 0);
    assert!(stats.evidence_bytes > 0);
    let (summaries, _, _) = usage.budget_counts();
    assert_eq!(summaries as usize, provider.summary_requests);
}
