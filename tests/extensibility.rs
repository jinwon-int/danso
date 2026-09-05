//! Executable extension example: no HTTP, CLI, credentials or production tools.
//! New adapters must still pass the same runtime persistence/recovery gates.
use anyhow::{Result, bail};
use danso::{
    contracts::{
        Event, EventSink, OperationState, SessionStore, ToolCall, ToolDefinition, ToolExecutor,
        ToolOutcome,
    },
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    session::Session,
    tools::{Registry, Tool},
    usage::{TokenUsage, Usage},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

struct ScriptedProvider {
    replies: VecDeque<Value>,
    requests: Vec<Value>,
}
impl Provider for ScriptedProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        self.requests.push(
            json!({"tools":request.tools, "messages":request.messages, "system":request.system}),
        );
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
        Ok(self.replies.pop_front().expect("unexpected request"))
    }
}

struct ProbeTool(Rc<RefCell<Vec<Value>>>);
impl Tool for ProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "probe".into(),
            description: "An extension test tool".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn execute(&self, args: &Value) -> Result<()> {
        self.0.borrow_mut().push(args.clone());
        Ok(())
    }
}

struct TestExecutor {
    registry: Registry,
    session_path: PathBuf,
    preflight_fails: bool,
}
impl ToolExecutor for TestExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.registry.definitions()
    }
    async fn preflight(&self) -> Result<()> {
        if self.preflight_fails {
            bail!("test isolation unavailable");
        }
        Ok(())
    }
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutcome> {
        let data = fs::read_to_string(&self.session_path)?;
        let last: Value = serde_json::from_str(data.lines().last().unwrap())?;
        assert_eq!(last["data"]["state"], "started");
        assert_eq!(last["data"]["toolCallId"], call.id);
        self.registry.execute(&call.name, &call.arguments)?;
        Ok(ToolOutcome {
            output: "probe result".into(),
            is_error: false,
        })
    }
}

#[derive(Default)]
struct RecordingSink {
    messages: Vec<Value>,
    final_answers: Vec<Value>,
}
impl EventSink for RecordingSink {
    fn emit(&mut self, event: Event<'_>) -> Result<()> {
        match event {
            Event::Message(m) => self.messages.push(m.clone()),
            Event::FinalAnswer(m) => self.final_answers.push(m.clone()),
            _ => {}
        }
        Ok(())
    }
}

fn tool_message(id: &str) -> Value {
    json!({"role":"assistant", "content":[{"type":"toolCall","id":id,"name":"probe","arguments":{"marker":42}}], "stopReason":"toolUse"})
}
fn provider(replies: Vec<Value>) -> ScriptedProvider {
    ScriptedProvider {
        replies: replies.into(),
        requests: vec![],
    }
}
fn input() -> RunInput<'static> {
    RunInput {
        prompt: "use the probe",
        context: "",
        max_turns: 3,
    }
}

#[tokio::test]
async fn add_provider_tool_and_sink_without_changing_loop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let calls = Rc::new(RefCell::new(vec![]));
    let mut registry = Registry::default();
    registry.register(ProbeTool(calls.clone())).unwrap();
    assert!(registry.register(ProbeTool(calls.clone())).is_err());
    let executor = TestExecutor {
        registry,
        session_path: path,
        preflight_fails: false,
    };
    let mut provider = provider(vec![
        tool_message("p1"),
        json!({"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop"}),
    ]);
    let mut sink = RecordingSink::default();
    let mut usage = Usage::default();
    runtime::run(
        input(),
        &mut provider,
        &executor,
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .unwrap();
    assert_eq!(*calls.borrow(), vec![json!({"marker":42})]);
    assert_eq!(provider.requests[0]["tools"][0]["name"], "probe");
    assert!(
        provider.requests[0]["system"]
            .as_str()
            .unwrap()
            .contains("Use only probe.")
    );
    assert_eq!(provider.requests[1]["messages"][2]["role"], "toolResult");
    assert_eq!(sink.messages.len(), 4);
    assert_eq!(sink.final_answers.len(), 1);
    assert_eq!(
        session.entries.last().unwrap()["message"]["role"],
        "assistant"
    );
    session.check_recovery().unwrap();
    assert_eq!(usage.summary()["models"], json!(["test-provider/scripted"]));
    assert_eq!(usage.summary()["totalTokens"], 6);
}

#[tokio::test]
async fn new_adapter_cannot_bypass_recovery_or_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let mut provider = provider(vec![]);
    let mut executor = TestExecutor {
        registry: Registry::default(),
        session_path: path,
        preflight_fails: true,
    };
    let mut sink = RecordingSink::default();
    let mut usage = Usage::default();
    let e = runtime::run(
        input(),
        &mut provider,
        &executor,
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("isolation unavailable"));
    assert!(session.messages().unwrap().is_empty());
    executor.preflight_fails = false;
    session.message(tool_message("uncertain")).unwrap();
    session.operation("uncertain", "started").unwrap();
    let e = runtime::run(
        input(),
        &mut provider,
        &executor,
        &mut session,
        &mut sink,
        &mut usage,
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("manual recovery"));
    assert!(provider.requests.is_empty());
    assert!(sink.messages.is_empty());
}

struct FailingJournal(Session);
impl SessionStore for FailingJournal {
    fn header(&self) -> &Value {
        &self.0.entries[0]
    }
    fn messages(&self) -> Result<Vec<Value>> {
        self.0.messages()
    }
    fn check_recovery(&self) -> Result<()> {
        self.0.check_recovery()
    }
    fn append_message(&mut self, message: Value) -> Result<Value> {
        self.0.message(message)
    }
    fn record_operation(&mut self, _: &str, _: OperationState) -> Result<()> {
        bail!("journal unavailable")
    }
}

#[tokio::test]
async fn persistence_failure_prevents_extension_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = FailingJournal(Session::open(&path, Path::new("/fixture")).unwrap());
    let calls = Rc::new(RefCell::new(vec![]));
    let mut registry = Registry::default();
    registry.register(ProbeTool(calls.clone())).unwrap();
    let executor = TestExecutor {
        registry,
        session_path: path,
        preflight_fails: false,
    };
    let mut provider = provider(vec![tool_message("p1")]);
    let e = runtime::run(
        input(),
        &mut provider,
        &executor,
        &mut session,
        &mut RecordingSink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("journal unavailable"));
    assert!(calls.borrow().is_empty());
    assert!(session.check_recovery().is_err());
}

#[tokio::test]
async fn repeated_call_ids_from_new_provider_do_not_execute_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut session = Session::open(&path, Path::new("/fixture")).unwrap();
    let calls = Rc::new(RefCell::new(vec![]));
    let mut registry = Registry::default();
    registry.register(ProbeTool(calls.clone())).unwrap();
    let executor = TestExecutor {
        registry,
        session_path: path,
        preflight_fails: false,
    };
    let mut provider = provider(vec![tool_message("p1"), tool_message("p1")]);
    let e = runtime::run(
        input(),
        &mut provider,
        &executor,
        &mut session,
        &mut RecordingSink::default(),
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("duplicate tool call id"));
    assert_eq!(calls.borrow().len(), 1);
    session.check_recovery().unwrap();
}
