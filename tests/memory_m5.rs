//! M5 integration tests (issue #52 §7, §10): private→shared promotion with
//! idempotence and audit, and the per-request context refresh hook.

use anyhow::Result;
use danso::memory::{Route, promote};
use danso::{
    contracts::{Event, EventSink, ToolDefinition, ToolExecutor, ToolOutcome},
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    session::Session,
    usage::Usage,
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::VecDeque, path::Path, rc::Rc};

fn setup_route(dir: &Path, scope: &str) -> Route {
    let route = Route::new(dir, scope).unwrap();
    danso::memory::paths::require_private_dir(&route.memories_dir()).unwrap();
    danso::memory::paths::require_private_dir(&route.state_dir()).unwrap();
    route
}

fn write_private(path: &Path, contents: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
}

fn now() -> chrono::DateTime<chrono::Utc> {
    danso::memory::facts::parse_timestamp("2026-09-08T12:00:00Z").unwrap()
}

fn fact_line(id: &str, text: &str) -> String {
    json!({
        "schema_version": 1,
        "id": id,
        "kind": "preference",
        "text": text,
        "review": "auto-local",
        "privacy": "private",
        "audience": "private",
        "durability": "durable",
        "confidence": 0.7,
        "source_rank": 1,
        "observed_at": "2026-09-01T00:00:00Z",
        "entities": ["user"],
        "tags": ["distilled", "final_answer"],
        "source": {"type": "distill", "provider": "danso", "job_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "thread_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "trigger": "final_answer", "schema_version": 1}
    })
    .to_string()
}

/// Promotion: the destination record lands in the shared store with
/// explicit-promotion review, promotion tags, and the audit trail; a second
/// request is an idempotent no-op (§7).
#[test]
fn promotion_is_audited_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let private = setup_route(dir.path(), "private-11111111111111111111111111111111");
    let _shared = setup_route(dir.path(), "shared");
    let fact_id = "distill-0123456789ab";
    write_private(
        &private.facts_file(),
        &format!("{}\n", fact_line(fact_id, "보고서는 한국어로 쓴다")),
    );

    let result = promote::promote(
        dir.path(),
        "private-11111111111111111111111111111111",
        fact_id,
        now(),
        1000,
    )
    .unwrap();
    assert!(result.promoted);
    assert!(result.destination_fact_id.starts_with("promoted-"));

    let shared = setup_route(dir.path(), "shared");
    let shared_facts = std::fs::read_to_string(shared.facts_file()).unwrap();
    assert!(shared_facts.contains("\"id\":\"promoted-"));
    assert!(shared_facts.contains("\"review\":\"explicit-promotion\""));
    assert!(shared_facts.contains("private-to-shared"));
    // The private fact is copied, not moved.
    let private_facts = std::fs::read_to_string(private.facts_file()).unwrap();
    assert!(private_facts.contains(fact_id));

    // Idempotence: the repeated request reports without duplicating.
    let again = promote::promote(
        dir.path(),
        "private-11111111111111111111111111111111",
        fact_id,
        now(),
        1000,
    )
    .unwrap();
    assert!(!again.promoted, "the repeat is an idempotent no-op");
    let shared_after = std::fs::read_to_string(shared.facts_file()).unwrap();
    assert_eq!(shared_after.matches("\"id\":\"promoted-").count(), 1);

    // Audit trail exists in the shared state.
    let audit = shared.state_dir().join("memory-promotion-audit.jsonl");
    assert!(audit.exists());
    assert!(
        std::fs::read_to_string(&audit)
            .unwrap()
            .contains(&result.promotion_id)
    );
}

/// Promotion source rules: unknown facts and non-private scopes are refused.
#[test]
fn promotion_refuses_unknown_facts_and_bad_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let _private = setup_route(dir.path(), "private-22222222222222222222222222222222");
    let _shared = setup_route(dir.path(), "shared");
    assert!(
        promote::promote(
            dir.path(),
            "private-22222222222222222222222222222222",
            "distill-000000000000",
            now(),
            1000
        )
        .is_err()
    );
    assert!(promote::promote(dir.path(), "global", "distill-000000000000", now(), 1000).is_err());
}

// ---------------------------------------------------------------------------
// Per-request context refresh (§5.3): the runtime re-resolves the context
// right after a compaction through a memory-agnostic hook.
// ---------------------------------------------------------------------------

struct RefreshProvider {
    agent_replies: VecDeque<Value>,
    systems: RefCell<Vec<String>>,
    summarizer_calls: Rc<RefCell<usize>>,
}
impl Provider for RefreshProvider {
    fn validate_history(&self, _: &[Value]) -> Result<()> {
        Ok(())
    }
    async fn complete(&mut self, request: ModelRequest<'_>, usage: &mut Usage) -> Result<Value> {
        if request
            .system
            .starts_with("You are a checkpoint summarizer")
        {
            *self.summarizer_calls.borrow_mut() += 1;
            return Ok(json!({
                "role": "assistant",
                "stopReason": "stop",
                "content": [{"type": "text", "text": danso::compaction::empty_summary().to_string()}]
            }));
        }
        self.systems.borrow_mut().push(request.system.to_string());
        usage.attempted = true;
        Ok(self
            .agent_replies
            .pop_front()
            .expect("unexpected agent request"))
    }
}

struct BigTool;
impl ToolExecutor for BigTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "probe".into(),
            description: "probe".into(),
            parameters: json!({"type": "object"}),
        }]
    }
    async fn preflight(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn execute(&self, _: &danso::contracts::ToolCall) -> anyhow::Result<ToolOutcome> {
        Ok(ToolOutcome {
            output: "z".repeat(8000),
            is_error: false,
        })
    }
}

struct NullSink;
impl EventSink for NullSink {
    fn emit(&mut self, _: Event<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn per_request_refresh_re_resolves_after_compaction() {
    let session_path = {
        let dir = tempfile::tempdir().unwrap();
        dir.path().join("session.jsonl")
    };
    std::fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    let mut session = Session::open(&session_path, Path::new("/fixture")).unwrap();
    session
        .message(json!({"role": "user", "content": "original task"}))
        .unwrap();
    session
        .message(json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "x".repeat(12000)}],
            "stopReason": "stop"
        }))
        .unwrap();

    let summarizer_calls = Rc::new(RefCell::new(0usize));
    let counter = Rc::new(RefCell::new(0usize));
    let counter_for_hook = Rc::clone(&counter);
    let refresh = move || -> anyhow::Result<String> {
        let n = {
            let mut c = counter_for_hook.borrow_mut();
            *c += 1;
            *c
        };
        Ok(format!("REFRESH-MARKER-{n} static context tail"))
    };

    let mut provider = RefreshProvider {
        agent_replies: VecDeque::from([
            tool_call("t1"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}], "stopReason": "stop"}),
        ]),
        systems: RefCell::new(Vec::new()),
        summarizer_calls: Rc::clone(&summarizer_calls),
    };
    let hook: &dyn Fn() -> anyhow::Result<String> = &refresh;
    let result = runtime::run(
        RunInput {
            no_tools: false,
            prompt: "continue",
            context: "REFRESH-MARKER-1 static context tail",
            execution_context: "",
            max_turns: 8,
            compact_at_bytes: Some(8192),
            refresh_context: Some(&hook),
            long_task: None,
            repeat_limit: 0,
            continuation_limit: 0,
            stream_requests: false,
            pause_requested: None,
        },
        &mut provider,
        &BigTool,
        &mut session,
        &mut NullSink,
        &mut Usage::default(),
    )
    .await;
    assert!(result.is_ok(), "run should finish: {result:?}");
    assert!(
        *summarizer_calls.borrow() >= 1,
        "a compaction must have run"
    );
    let systems = provider.systems.borrow();
    assert!(
        systems[0].contains("REFRESH-MARKER-1"),
        "first request uses the initial context"
    );
    assert!(
        systems.len() >= 2 && systems[1].contains("REFRESH-MARKER-2"),
        "the post-compaction request re-resolves the context"
    );
}

fn tool_call(id: &str) -> Value {
    json!({
        "role": "assistant",
        "content": [{"type": "toolCall", "id": id, "name": "probe", "arguments": {}}],
        "stopReason": "toolUse"
    })
}
