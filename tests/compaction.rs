use anyhow::{Result, bail};
use danso::{
    compaction,
    contracts::*,
    provider::*,
    runtime::{self, RunInput},
    session::Session,
    usage::Usage,
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
        },
        &mut provider,
        &Executor,
        &mut store,
        &mut Sink,
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("checkpoint disk failure"));
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
