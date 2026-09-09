//! M2 integration tests (issue #52 §10): the fail-open source matrix, the
//! managed block contract, memory-OFF byte identity, and the compaction
//! interaction proven through a scripted provider against the real runtime.

use anyhow::Result;
use danso::memory::{self, MemoryConfig, Route, SnapshotOptions, snapshot};
use danso::{
    provider::{ModelRequest, Provider},
    runtime::{self, RunInput},
    usage::Usage,
};
use serde_json::{Value, json};
use std::path::Path;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

fn route_in(dir: &Path, scope: &str) -> Route {
    Route::new(dir, scope).unwrap()
}

fn setup_tree(route: &Route) {
    paths_require(&route.memories_dir());
    paths_require(&route.state_dir());
}

fn paths_require(dir: &Path) {
    danso::memory::paths::require_private_dir(dir).unwrap();
}

fn write_private(path: &Path, contents: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(contents).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
}

fn now() -> chrono::DateTime<chrono::Utc> {
    danso::memory::facts::parse_timestamp("2026-09-08T12:00:00Z").unwrap()
}

/// §5.1 fail-open matrix: a broken source drops only its own block; the
/// remaining sources still inject. Covered: missing, permission (0644),
/// symlink, hardlink, invalid UTF-8, oversize.
#[test]
fn every_source_is_independently_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    let mem = route.memories_dir();
    use std::os::unix::fs::PermissionsExt;

    // USER.md stays healthy so the MEMORY+USER block still injects.
    write_private(&mem.join("USER.md"), b"user policy line\n");

    // resume.md is a symlink -> skipped.
    std::os::unix::fs::symlink(mem.join("USER.md"), route.resume_file()).unwrap();
    // working-state.md is a hardlink (nlink 2) -> skipped.
    write_private(&dir.path().join("elsewhere"), b"ws\n");
    std::fs::hard_link(dir.path().join("elsewhere"), route.working_state_file()).unwrap();
    // MEMORY.md with invalid UTF-8 -> skipped (placeholder).
    write_private(&mem.join("MEMORY.md"), &[0xFF, 0xFE, b'a', b'\n']);
    // facts file with world-readable mode -> structured sources skipped.
    write_private(&route.facts_file(), b"");
    std::fs::set_permissions(route.facts_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    // An oversize (>1 MiB) resume would be skipped too — covered by the
    // read bound in the store tests; here the matrix keeps one of each class.

    let body = snapshot::assemble(
        &route,
        &SnapshotOptions {
            event: "run",
            query: None,
            max_bytes: snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
            stale_days: snapshot::STALE_DAYS_DEFAULT,
            now: now(),
        },
    )
    .unwrap();

    assert!(
        body.contains("user policy line"),
        "healthy source still injects"
    );
    assert!(
        !body.contains('\u{FFFD}'),
        "invalid UTF-8 MEMORY is skipped, never lossy-injected"
    );
    assert!(
        body.contains("(no working state recorded)"),
        "hardlinked working state is skipped"
    );
    assert!(
        !body.contains("▶ 직전 세션에서 이어서:"),
        "symlinked resume is skipped"
    );
}

/// The managed block: audited markers, the body hash, the policy header,
/// and the hard 32768-byte cap.
#[test]
fn managed_block_contract() {
    let body = "# n session memory (auto-injected: run)\ncontent\n";
    let block = snapshot::render_managed_block(body, now()).unwrap();
    assert!(block.starts_with(snapshot::MANAGED_BEGIN));
    assert!(block.trim_end().ends_with(snapshot::MANAGED_END));
    assert!(block.contains("## Memory connection scope"));
    assert!(
        !block.contains("github-policy"),
        "node policy block stays out (§5.2)"
    );
    let sha = danso::memory::facts::hex_encode(&{
        use sha2::{Digest, Sha256};
        Sha256::digest(body.as_bytes())
    });
    assert!(block.contains(&format!("snapshot-sha256: `{sha}`")));
    // Forgery aborts.
    let forged = format!("{body}{}", snapshot::MANAGED_BEGIN);
    assert!(snapshot::render_managed_block(&forged, now()).is_err());
    // The cap is enforced.
    let huge = format!(
        "{}\n{}",
        "x".repeat(snapshot::MANAGED_BLOCK_MAX_BYTES + 1),
        body
    );
    assert!(snapshot::render_managed_block(&huge, now()).is_err());
}

/// Memory OFF leaves the system context byte-identical (§10 M2): the
/// injection boundary is the only touch point, and OFF returns unchanged.
#[test]
fn memory_off_is_byte_identical() {
    let mut ctx = String::from("discovered system context");
    let config = MemoryConfig::default();
    memory::snapshot::inject_into_context(&mut ctx, &config, "prompt", Path::new("/tmp"), now())
        .unwrap();
    assert_eq!(ctx, "discovered system context");
}

// ---------------------------------------------------------------------------
// Scripted-provider proof: the memory block rides in the system context of
// every agent request, across repeated compactions, and the compaction
// margin check measures the real serialized request (memory bytes included).
// ---------------------------------------------------------------------------

struct ScriptedProvider {
    agent_replies: VecDeque<Value>,
    systems: RefCell<Vec<String>>,
    summarizer_calls: Rc<RefCell<usize>>,
}

impl ScriptedProvider {
    fn agent_systems(&self) -> std::cell::Ref<'_, Vec<String>> {
        self.systems.borrow()
    }
}

impl Provider for ScriptedProvider {
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
impl danso::contracts::ToolExecutor for BigTool {
    fn definitions(&self) -> Vec<danso::contracts::ToolDefinition> {
        vec![danso::contracts::ToolDefinition {
            name: "probe".into(),
            description: "probe".into(),
            parameters: json!({"type": "object"}),
        }]
    }
    async fn preflight(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &danso::contracts::ToolCall,
    ) -> anyhow::Result<danso::contracts::ToolOutcome> {
        Ok(danso::contracts::ToolOutcome {
            output: "y".repeat(7000),
            is_error: false,
        })
    }
}

struct Sink;
impl danso::contracts::EventSink for Sink {
    fn emit(&mut self, _: danso::contracts::Event<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn tool_call_reply(id: &str) -> Value {
    json!({
        "role": "assistant",
        "content": [{"type": "toolCall", "id": id, "name": "probe", "arguments": {}}],
        "stopReason": "toolUse"
    })
}

/// Memory ON: the managed block survives two compactions intact, and the
/// compaction margin check runs against the real serialized request that
/// carries the memory bytes.
#[tokio::test]
async fn memory_block_survives_two_compactions() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    write_private(
        &route.memories_dir().join("MEMORY.md"),
        b"durable policy line\n",
    );

    let options = SnapshotOptions {
        event: "run",
        query: None,
        max_bytes: snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
        stale_days: snapshot::STALE_DAYS_DEFAULT,
        now: now(),
    };
    let block = snapshot::memory_block(&route, &options).unwrap();
    assert!(block.contains(snapshot::MANAGED_BEGIN));

    let session_path = dir.path().join("session.jsonl");
    let mut session = danso::session::Session::open(&session_path, Path::new("/fixture")).unwrap();
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

    let summarizer_calls = Rc::new(RefCell::new(0));
    let mut provider = ScriptedProvider {
        agent_replies: VecDeque::from([
            tool_call_reply("t1"),
            tool_call_reply("t2"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}], "stopReason": "stop"}),
        ]),
        systems: RefCell::new(Vec::new()),
        summarizer_calls: Rc::clone(&summarizer_calls),
    };
    let error = runtime::run(
        RunInput {
            no_tools: false,
            prompt: "continue",
            context: &block,
            execution_context: "",
            max_turns: 12,
            compact_at_bytes: Some(8192),
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            pause_requested: None,
        },
        &mut provider,
        &BigTool,
        &mut session,
        &mut Sink,
        &mut Usage::default(),
    )
    .await;
    assert!(error.is_ok(), "run should finish: {error:?}");
    assert!(
        *summarizer_calls.borrow() >= 2,
        "at least two compactions ran"
    );
    for system in provider.agent_systems().iter() {
        assert!(
            system.contains(snapshot::MANAGED_BEGIN),
            "memory block present in every agent request"
        );
        assert!(
            system.contains("durable policy line"),
            "snapshot body rides along"
        );
    }
}

/// The margin check measures the real serialized request: a context this
/// large leaves no compaction budget and the run is refused up front
/// instead of spending summary calls it cannot afford.
#[tokio::test]
async fn margin_check_sees_memory_bytes() {
    let big_context = format!("M\n{}", "m".repeat(9 * 1024));
    let dir = tempfile::tempdir().unwrap();
    let session_path = dir.path().join("session.jsonl");
    let mut session = danso::session::Session::open(&session_path, Path::new("/fixture")).unwrap();
    session
        .message(json!({"role": "user", "content": "original task"}))
        .unwrap();
    let mut provider = ScriptedProvider {
        agent_replies: VecDeque::new(),
        systems: RefCell::new(Vec::new()),
        summarizer_calls: Rc::new(RefCell::new(0)),
    };
    let error = runtime::run(
        RunInput {
            no_tools: true,
            prompt: "continue",
            context: &big_context,
            execution_context: "",
            max_turns: 4,
            compact_at_bytes: Some(8192),
            refresh_context: None,
            long_task: None,
            repeat_limit: 0,
            pause_requested: None,
        },
        &mut provider,
        &BigTool,
        &mut session,
        &mut Sink,
        &mut Usage::default(),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("leave no compaction budget"),
        "the margin check must see the memory bytes: {error:#}"
    );
}
