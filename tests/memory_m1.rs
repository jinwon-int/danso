//! M1 integration tests (issue #52 §11): fixture interpretation, file
//! rules, gate behavior through the real store paths, recall semantics,
//! scope isolation, and the real-binary CLI surface.

use danso::memory::{
    Route,
    facts::{self, Candidate, Line},
    paths, recall, transaction,
};
use serde_json::{Value, json};
use std::path::Path;

fn route_in(dir: &Path, scope: &str) -> Route {
    Route::new(dir, scope).unwrap()
}

fn setup_tree(route: &Route) {
    paths::require_private_dir(&route.memories_dir()).unwrap();
    paths::require_private_dir(&route.state_dir()).unwrap();
}

fn write_private(path: &std::path::Path, contents: &str) {
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

fn manual_candidate(text: &str) -> Candidate {
    Candidate {
        kind: "preference".into(),
        text: text.into(),
        because: None,
        quote: None,
        source_rank: 3,
        observed_at: facts::format_timestamp(
            facts::parse_timestamp("2026-09-08T00:00:00Z").unwrap(),
        ),
        entities: vec!["user".into()],
        tags: vec!["manual".into()],
        valid_from: None,
        valid_until: None,
        transcript: None,
        manual: true,
        job_id: None,
        explicit_id: None,
    }
}

fn pinned_now() -> chrono::DateTime<chrono::Utc> {
    facts::parse_timestamp("2026-09-08T12:00:00Z").unwrap()
}

/// All three writer variants are readable; the ccc hook variant gets its
/// defaults filled and opaque lines survive (§4.1 file rules).
#[test]
fn the_three_writer_variants_are_interpreted() {
    for (file, expects_schema) in [
        ("ccc-sink.jsonl", true),
        ("ccc-hook.jsonl", false),
        ("danso.jsonl", true),
    ] {
        let payload = match file {
            "ccc-sink.jsonl" => include_str!("fixtures/memory/ccc-sink.jsonl"),
            "ccc-hook.jsonl" => include_str!("fixtures/memory/ccc-hook.jsonl"),
            _ => include_str!("fixtures/memory/danso.jsonl"),
        };
        let parsed = facts::read(Some(payload.as_bytes().to_vec())).unwrap();
        let records: Vec<&facts::FactRecord> = parsed.records().collect();
        assert!(!records.is_empty(), "{file}");
        assert_eq!(records[0].schema_version, 1, "{file}");
        assert_eq!(
            records[0].durability,
            facts::FactRecord::default_durability(&records[0].kind),
            "{file}"
        );
        assert_eq!(
            expects_schema,
            records[0].raw.get("schema_version").is_some(),
            "{file}"
        );
    }
}

/// A corrupt line between good records is preserved byte-for-byte, and the
/// 8 MiB read bound refuses oversize files.
#[test]
fn opaque_lines_survive_and_the_read_bound_is_enforced() {
    let good = include_str!("fixtures/memory/ccc-hook.jsonl");
    let payload = format!("{good}{{corrupt line\n");
    let parsed = facts::read(Some(payload.as_bytes().to_vec())).unwrap();
    assert_eq!(parsed.lines.len(), 3);
    assert!(matches!(&parsed.lines[2], Line::Opaque(raw) if raw == "{corrupt line"));

    // The 8 MiB read bound is enforced through the store loader.
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    write_private(&route.facts_file(), &"x".repeat(9_000_000));
    assert!(facts::load(&route).is_err());
}

/// Write gates through the real store: manual add, dedup, supersede, close.
#[test]
fn store_roundtrip_gates_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);

    // First add writes the file.
    let existing = facts::load(&route).unwrap();
    let now = pinned_now();
    let out = facts::gate_and_render(
        &existing,
        vec![manual_candidate("보고서는 한국어로 쓴다")],
        "private",
        now,
        facts::MAX_FACTS_DEFAULT,
    )
    .unwrap();
    assert!(out.changed && out.report.saved == 1);
    let payload: String = out.lines.concat();
    write_private(&route.facts_file(), &payload);

    // A duplicate add changes nothing.
    let existing = facts::load(&route).unwrap();
    let out = facts::gate_and_render(
        &existing,
        vec![manual_candidate("보고서는 한국어로 쓴다!")],
        "private",
        now,
        facts::MAX_FACTS_DEFAULT,
    )
    .unwrap();
    assert!(!out.changed);

    // A mutable-ops observation is refused by the gates.
    let out = facts::gate_and_render(
        &existing,
        vec![Candidate {
            kind: "observation".into(),
            text: "워커 active (running) 상태".into(),
            ..manual_candidate("x")
        }],
        "private",
        now,
        facts::MAX_FACTS_DEFAULT,
    )
    .unwrap();
    assert_eq!(out.report.skipped_mutable, 1);
    assert!(!out.changed);

    // Close sets valid_until=now and is idempotent.
    let id = facts::load(&route)
        .unwrap()
        .records()
        .next()
        .unwrap()
        .id
        .clone();
    assert_eq!(
        facts::close(&route, &id, now, 1000).unwrap(),
        facts::CloseOutcome::Closed
    );
    assert_eq!(
        facts::close(&route, &id, now, 1000).unwrap(),
        facts::CloseOutcome::AlreadyClosed
    );
    let closed = facts::load(&route)
        .unwrap()
        .records()
        .next()
        .unwrap()
        .clone();
    assert_eq!(closed.valid_until.as_deref(), Some("2026-09-08T12:00:00Z"));
    // All other fields survive the close.
    assert_eq!(closed.review, "manual");
    assert_eq!(closed.raw["tags"][0], json!("manual"));
}

/// Valid-time semantics on the real search path: current excludes future and
/// demotes expired; an explicit as-of keeps only then-valid facts.
#[test]
fn search_applies_valid_time_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    let scenario = include_str!("fixtures/memory/scenario/facts.jsonl");
    write_private(&route.facts_file(), scenario);

    let now = pinned_now();
    let current = recall::search(
        &route,
        &recall::SearchOptions {
            query: "team standup time",
            as_of: None,
            limit: 5,
            now,
        },
    )
    .unwrap();
    let paths: Vec<String> = current["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths[0].ends_with("standup-new") || paths[0].contains("standup-new"));
    assert!(
        paths.iter().any(|p| p.contains("standup-old")),
        "expired facts are demoted, never deleted"
    );
    assert!(
        paths
            .iter()
            .position(|p| p.contains("standup-new"))
            .unwrap()
            < paths
                .iter()
                .position(|p| p.contains("standup-old"))
                .unwrap()
    );

    let as_of = recall::search(
        &route,
        &recall::SearchOptions {
            query: "team standup time",
            as_of: Some("2026-01-15T00:00:00Z"),
            limit: 5,
            now,
        },
    )
    .unwrap();
    assert_eq!(as_of["temporal"]["mode"], json!("as_of"));
    let paths: Vec<String> = as_of["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.iter().any(|p| p.contains("standup-old")));
    assert!(!paths.iter().any(|p| p.contains("standup-new")));

    // An unparseable explicit as_of degrades to current mode with a signal.
    let degraded = recall::search(
        &route,
        &recall::SearchOptions {
            query: "team standup time",
            as_of: Some("nonsense"),
            limit: 5,
            now,
        },
    )
    .unwrap();
    assert_eq!(degraded["temporal"]["mode"], json!("current"));
    assert_eq!(degraded["temporal"]["degraded"], json!(1));
    assert_eq!(
        degraded["temporal"]["reason"],
        json!("as-of-parse-failed-using-current")
    );
}

/// Scope isolation (§7): private reads private + shared; shared reads only
/// shared; global reads only global.
#[test]
fn scope_read_rules_isolate_private_trees() {
    let dir = tempfile::tempdir().unwrap();
    for scope in [
        "private-11111111111111111111111111111111",
        "shared",
        "global",
    ] {
        let route = route_in(dir.path(), scope);
        setup_tree(&route);
        write_private(
            &route.memories_dir().join("MEMORY.md"),
            &format!("scope marker {scope} unique-{scope}.\n"),
        );
    }
    let now = pinned_now();

    let private = route_in(dir.path(), "private-11111111111111111111111111111111");
    let seen = recall::search(
        &private,
        &recall::SearchOptions {
            query: "scope marker",
            as_of: None,
            limit: 10,
            now,
        },
    )
    .unwrap();
    let joined = serde_json::to_string(&seen["results"]).unwrap();
    assert!(
        joined.contains("unique-private-"),
        "private sees its own tree"
    );
    assert!(
        joined.contains("unique-shared"),
        "private additionally reads shared"
    );
    assert!(
        !joined.contains("unique-global"),
        "private never reads global"
    );

    let shared = route_in(dir.path(), "shared");
    let seen = recall::search(
        &shared,
        &recall::SearchOptions {
            query: "scope marker",
            as_of: None,
            limit: 10,
            now,
        },
    )
    .unwrap();
    let joined = serde_json::to_string(&seen["results"]).unwrap();
    assert!(joined.contains("unique-shared"));
    assert!(!joined.contains("unique-private-") && !joined.contains("unique-global"));

    let global = route_in(dir.path(), "global");
    let seen = recall::search(
        &global,
        &recall::SearchOptions {
            query: "scope marker",
            as_of: None,
            limit: 10,
            now,
        },
    )
    .unwrap();
    let joined = serde_json::to_string(&seen["results"]).unwrap();
    assert!(joined.contains("unique-global"));
    assert!(!joined.contains("unique-shared") && !joined.contains("unique-private-"));
}

/// Search results are scanned: a stored credential never reaches a snippet.
#[test]
fn search_snippets_are_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    write_private(
        &route.facts_file(),
        concat!(
            r#"{"id":"leaky","kind":"observation","text":"the leaked token was ghp_0123456789abcdefghij and must be revoked","review":"auto-local","privacy":"private","observed_at":"2026-09-08T00:00:00Z","entities":["security"],"tags":[]}"#,
            "\n"
        ),
    );
    let now = pinned_now();
    let out = recall::search(
        &route,
        &recall::SearchOptions {
            query: "leaked token revoked",
            as_of: None,
            limit: 5,
            now,
        },
    )
    .unwrap();
    let snippet = out["results"][0]["snippet"].as_str().unwrap();
    assert!(snippet.contains("[REDACTED:credential]"));
    assert!(!snippet.contains("ghp_0123456789abcdefghij"));
}

/// The derived index is deterministic: rebuilding and searching again
/// returns byte-identical output for the same pinned clock (§10 M1).
#[test]
fn search_is_deterministic_across_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);
    write_private(
        &route.facts_file(),
        include_str!("fixtures/memory/scenario/facts.jsonl"),
    );
    write_private(
        &route.memories_dir().join("MEMORY.md"),
        include_str!("fixtures/memory/scenario/MEMORY.md"),
    );
    write_private(
        &route.memories_dir().join("USER.md"),
        include_str!("fixtures/memory/scenario/USER.md"),
    );
    let now = pinned_now();
    let first = recall::search(
        &route,
        &recall::SearchOptions {
            query: "editor preference helix",
            as_of: None,
            limit: 5,
            now,
        },
    )
    .unwrap();
    let second = recall::search(
        &route,
        &recall::SearchOptions {
            query: "editor preference helix",
            as_of: None,
            limit: 5,
            now,
        },
    )
    .unwrap();
    assert_eq!(first, second);
}

/// The real binary: init → add → search → close end-to-end through the CLI.
#[test]
fn cli_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_danso");
    let run = |args: &[&str]| -> (i32, String) {
        let output = std::process::Command::new(bin)
            .args(args)
            .env("DANSO_MEMORY_DIR", dir.path())
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };

    let (code, out) = run(&["memory", "init", "--scope", "global"]);
    assert_eq!(code, 0, "{out}");
    let value: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["scope"], json!("global"));

    let (code, out) = run(&[
        "memory",
        "add",
        "--kind",
        "decision",
        "--text",
        "스탠드업은 09:30 KST에 한다",
        "--because",
        "팀 절반이 08시 이전에 접속하지 못한다",
        "--subject",
        "session",
    ]);
    assert_eq!(code, 0, "{out}");
    let value: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["added"], json!(true));

    // A decision without a reason is a configuration error (exit 2).
    let (code, _) = run(&[
        "memory",
        "add",
        "--kind",
        "decision",
        "--text",
        "이유 없는 결정",
    ]);
    assert_eq!(code, 2);

    let (code, out) = run(&["memory", "search", "스탠드업 시간", "--json"]);
    assert_eq!(code, 0, "{out}");
    let value: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["results"].as_array().unwrap().len(), 1);

    let (code, out) = run(&["memory", "close", "--fact", "does-not-exist"]);
    assert_eq!(code, 1, "{out}");

    let facts_path = dir.path().join("global/state/memory-facts.jsonl");
    let stored = std::fs::read_to_string(&facts_path).unwrap();
    let id: String = stored
        .lines()
        .next()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            value["id"].as_str().unwrap().to_string()
        })
        .unwrap();
    let (code, out) = run(&["memory", "close", "--fact", &id]);
    assert_eq!(code, 0, "{out}");
    let value: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["closed"], json!(true));
}

/// #65 §1.5: manual closes go through the rollback transaction — the write
/// leaves an undoable head (serialized with distill commits) and a
/// body-free MemoryCommit ledger event.
#[test]
fn close_goes_through_the_rollback_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let route = route_in(dir.path(), "global");
    setup_tree(&route);

    let existing = facts::load(&route).unwrap();
    let now = pinned_now();
    let out = facts::gate_and_render(
        &existing,
        vec![manual_candidate("보고서는 한국어로 쓴다")],
        "private",
        now,
        facts::MAX_FACTS_DEFAULT,
    )
    .unwrap();
    let payload: String = out.lines.concat();
    write_private(&route.facts_file(), &payload);
    let id = facts::load(&route)
        .unwrap()
        .records()
        .next()
        .unwrap()
        .id
        .clone();

    let transaction = transaction::Transaction::new(&route.state_dir());
    assert_eq!(
        transaction.status().unwrap(),
        (None, 0),
        "fresh scope has no rollback head"
    );

    assert_eq!(
        facts::close(&route, &id, now, 1000).unwrap(),
        facts::CloseOutcome::Closed
    );
    // One undoable head now exists.
    let (head, actions) = transaction.status().unwrap();
    assert_eq!(head.as_deref(), Some(head_action_id(&route).as_str()));
    assert_eq!(actions, 1, "the close left exactly one action");

    // Body-free MemoryCommit ledger event recorded.
    let audit = std::fs::read_to_string(route.state_dir().join("audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"event\":\"MemoryCommit\"") && audit.contains("\"facts_added\":0"),
        "close records a MemoryCommit event: {audit}"
    );

    // The head actually rolls the close back.
    let action_id = head.unwrap();
    assert_eq!(
        transaction.rollback(1000, &action_id).unwrap(),
        transaction::RollbackOutcome::RolledBack
    );
    let record = facts::load(&route)
        .unwrap()
        .records()
        .next()
        .unwrap()
        .clone();
    assert_eq!(
        record.valid_until, None,
        "rollback undoes the close (valid_until cleared)"
    );
}

fn head_action_id(route: &Route) -> String {
    String::from_utf8_lossy(&std::fs::read(route.state_dir().join("memory-rollback/HEAD")).unwrap())
        .trim()
        .to_string()
}
