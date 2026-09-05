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
