//! M5 integration tests (issue #52 §7, §10): private→shared promotion with
//! idempotence and audit, and the per-request context refresh hook.

use anyhow::Result;
use danso::memory::{Route, promote, transaction};
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

    // #65 §1.5: the promotion went through the rollback transaction — it
    // left exactly one undoable head on the shared scope and a body-free
    // MemoryCommit ledger event.
    let transaction = transaction::Transaction::new(&shared.state_dir());
    let (head, actions) = transaction.status().unwrap();
    assert_eq!(actions, 1, "one rollback action");
    assert!(head.is_some(), "the promotion left an undoable head");
    let audit_events = std::fs::read_to_string(shared.state_dir().join("audit.jsonl")).unwrap();
    assert!(
        audit_events.contains("\"event\":\"MemoryCommit\"")
            && audit_events.contains("\"facts_added\":1"),
        "promotion records a MemoryCommit event: {audit_events}"
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
            report_progress: false,
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

// ---------------------------------------------------------------------------
// Legacy read lane (#52 §7/§8/§9, issue #86)
// ---------------------------------------------------------------------------

/// Build a ccc-shaped legacy tree: `state/` and `memories/` directly below
/// the given root, which is how a real ccc node lays out `~/.claude`.
fn setup_legacy_tree(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("ccc");
    danso::memory::paths::require_private_dir(&root.join("state")).unwrap();
    danso::memory::paths::require_private_dir(&root.join("memories")).unwrap();
    root
}

fn write_with_mode(path: &Path, contents: &str, mode: u32) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .unwrap();
}

fn search_paths(route: &Route, query: &str) -> Vec<String> {
    let options = danso::memory::recall::SearchOptions::new(query, now());
    let result = danso::memory::recall::search(route, &options).unwrap();
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["path"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// §9: the legacy lane contributes MEMORY.md and the facts file, and — the
/// case that matters in practice — a `working-state.md` at 0644, which is the
/// mode a real ccc node leaves it at. Under the 0600 rule that governs
/// Danso's own state this file would be dropped without a word.
#[test]
fn legacy_lane_reads_a_real_ccc_tree_including_its_0644_working_state() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    let legacy = setup_legacy_tree(dir.path());
    write_private(
        &legacy.join("memories/MEMORY.md"),
        "legacy 운영 노트: 브로커 경계는 T1/T2로 나뉜다\n",
    );
    write_with_mode(
        &legacy.join("state/working-state.md"),
        "목표: 레거시 이관 검증\n",
        0o644,
    );
    write_private(
        &legacy.join("state/memory-facts.jsonl"),
        &format!("{}\n", fact_line("legacy-1", "레거시 사실 하나")),
    );

    let with_legacy = route.clone().with_legacy(&legacy).unwrap();
    let paths = search_paths(&with_legacy, "레거시");
    assert!(
        paths.iter().any(|p| p.contains("ccc/state/memory-facts")),
        "legacy facts must be searchable: {paths:?}"
    );

    let paths = search_paths(&with_legacy, "이관 검증");
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("ccc/state/working-state.md")),
        "a 0644 legacy working-state must still be read: {paths:?}"
    );

    // Without the flag nothing from the legacy tree is reachable.
    assert!(
        search_paths(&route, "레거시").is_empty(),
        "the lane is opt-in"
    );
}

/// §6.1 still applies to the legacy lane: a group-writable file is not
/// trustworthy input, and the failure is per-source (fail-open), not fatal.
#[test]
fn legacy_lane_refuses_group_writable_files_but_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    let legacy = setup_legacy_tree(dir.path());
    write_with_mode(
        &legacy.join("memories/MEMORY.md"),
        "세계쓰기 가능한 오염 문서\n",
        0o666,
    );
    write_private(
        &legacy.join("memories/USER.md"),
        "정상 문서: 오염되지 않음\n",
    );

    let with_legacy = route.with_legacy(&legacy).unwrap();
    let paths = search_paths(&with_legacy, "오염");
    assert!(
        !paths.iter().any(|p| p.ends_with("MEMORY.md")),
        "group-writable source is excluded: {paths:?}"
    );
    let paths = search_paths(&with_legacy, "정상 문서");
    assert!(
        paths.iter().any(|p| p.ends_with("USER.md")),
        "the rest of the lane still reads (fail-open): {paths:?}"
    );
}

/// §7/§8 read matrix: a `shared` run never opens a legacy tree. The flag is a
/// configuration error there rather than a silently ignored option.
#[test]
fn shared_scope_never_opens_the_legacy_lane() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = setup_legacy_tree(dir.path());
    let shared = Route::new(dir.path(), "shared").unwrap();
    let error = shared
        .clone()
        .with_legacy(&legacy)
        .expect_err("shared must refuse the legacy lane");
    assert!(
        error.to_string().contains("shared"),
        "error names the scope: {error}"
    );
    // And the accessor refuses too, so no read path can reach it.
    assert!(shared.legacy_route().is_none());

    // private-* and global are both allowed (§8).
    let private = Route::new(dir.path(), &format!("private-{}", "a".repeat(32))).unwrap();
    assert!(private.with_legacy(&legacy).is_ok());
    assert!(
        Route::new(dir.path(), "global")
            .unwrap()
            .with_legacy(&legacy)
            .is_ok()
    );
}

/// §9 single-writer rule: the lane is read-only. Prove it by content and
/// mtime rather than by inspecting the code path.
#[test]
fn legacy_lane_never_writes_the_foreign_tree() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    let legacy = setup_legacy_tree(dir.path());
    let facts = legacy.join("state/memory-facts.jsonl");
    write_private(
        &facts,
        &format!("{}\n", fact_line("legacy-1", "불변이어야 하는 사실")),
    );
    let memory = legacy.join("memories/MEMORY.md");
    write_private(&memory, "불변 문서\n");

    let snapshot = |path: &Path| {
        let meta = std::fs::symlink_metadata(path).unwrap();
        (std::fs::read(path).unwrap(), meta.modified().unwrap())
    };
    let before = [snapshot(&facts), snapshot(&memory)];

    let with_legacy = route.with_legacy(&legacy).unwrap();
    let _ = search_paths(&with_legacy, "불변");
    let _ = danso::memory::snapshot::assemble(
        &with_legacy,
        &danso::memory::snapshot::SnapshotOptions {
            event: "run",
            query: Some("불변"),
            max_bytes: danso::memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
            stale_days: danso::memory::snapshot::STALE_DAYS_DEFAULT,
            now: now(),
        },
    )
    .unwrap();

    assert_eq!(
        before,
        [snapshot(&facts), snapshot(&memory)],
        "reads must leave the legacy tree byte- and mtime-identical"
    );

    // The lane itself refuses any write path outright.
    let lane = with_legacy.legacy_route().unwrap();
    assert!(lane.is_legacy());
    assert!(lane.require_writable().is_err());
}

/// §7 dedup: the legacy tree is what the live tree was migrated from, so the
/// same fact is usually present in both under different ids. It must appear
/// once, and the live copy is the one that survives.
#[test]
fn duplicate_facts_across_lanes_collapse_to_the_live_copy() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    let legacy = setup_legacy_tree(dir.path());
    let shared_text = "브로커 경계는 T1 과 T2 로 나뉜다";
    write_private(
        &route.facts_file(),
        &format!("{}\n", fact_line("live-1", shared_text)),
    );
    write_private(
        &legacy.join("state/memory-facts.jsonl"),
        &format!(
            "{}\n{}\n",
            fact_line("legacy-1", shared_text),
            fact_line("legacy-2", "레거시에만 있는 사실")
        ),
    );

    let with_legacy = route.with_legacy(&legacy).unwrap();
    let block = danso::memory::snapshot::assemble(
        &with_legacy,
        &danso::memory::snapshot::SnapshotOptions {
            event: "run",
            query: Some("브로커"),
            max_bytes: danso::memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
            stale_days: 0,
            now: now(),
        },
    )
    .unwrap();
    assert_eq!(
        block.matches(shared_text).count(),
        1,
        "the duplicated fact appears once:\n{block}"
    );
    assert!(
        block.contains("레거시에만 있는 사실"),
        "legacy-only facts still merge in:\n{block}"
    );
}

/// §5.1 fail-open: an absent or unreadable legacy tree costs the run nothing.
#[test]
fn a_missing_legacy_tree_is_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    write_private(
        &route.facts_file(),
        &format!("{}\n", fact_line("live-1", "라이브 사실")),
    );
    let with_legacy = route
        .clone()
        .with_legacy(&dir.path().join("does-not-exist"))
        .unwrap();

    let options = danso::memory::snapshot::SnapshotOptions {
        event: "run",
        query: Some("라이브"),
        max_bytes: danso::memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
        stale_days: 0,
        now: now(),
    };
    let with = danso::memory::snapshot::assemble(&with_legacy, &options).unwrap();
    let without = danso::memory::snapshot::assemble(&route, &options).unwrap();
    assert_eq!(
        with, without,
        "an absent legacy tree changes nothing at all"
    );
}

/// The flag must be an absolute path (§8), like `--memory-dir`.
#[test]
fn legacy_lane_requires_an_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let route = setup_route(dir.path(), "global");
    assert!(route.with_legacy(Path::new("relative/ccc")).is_err());
}
