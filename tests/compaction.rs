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
            prompt: "continue",
            context: "",
            execution_context: "",
            max_turns: 8,
            compact_at_bytes: Some(8192),
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
