use danso::{context, session::Session, tools};
use serde_json::json;
use std::{fs, path::Path};

#[test]
fn piri_fixture_roundtrip_and_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let fixture = include_str!("fixtures/piri-v3.jsonl");
    fs::write(&path, fixture).unwrap();
    let mut s = Session::open(&path, Path::new("/fixture/repo")).unwrap();
    assert_eq!(s.messages().unwrap().len(), 4);
    s.check_recovery().unwrap();
    s.message(json!({"role":"user","content":"Continue","timestamp":1}))
        .unwrap();
    drop(s);
    assert!(fs::read_to_string(&path).unwrap().starts_with(fixture));
    let s = Session::open(&path, Path::new("/fixture/repo")).unwrap();
    assert_eq!(s.messages().unwrap().len(), 5);
}

#[test]
fn torn_duplicate_and_branched_sessions_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    fs::write(&path, "{\"type\":").unwrap();
    assert!(Session::open(&path, Path::new("/repo")).is_err());
    fs::remove_file(&path).unwrap();
    let mut s = Session::open(&path, Path::new("/repo")).unwrap();
    s.message(json!({"role":"user","content":"one"})).unwrap();
    s.message(json!({"role":"user","content":"two"})).unwrap();
    s.entries[2]["parentId"] = serde_json::Value::Null;
    assert!(s.messages().is_err());
    drop(s);
    let data = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{}{}\n", data, data.lines().last().unwrap())).unwrap();
    assert!(Session::open(&path, Path::new("/repo")).is_err());
}

#[test]
fn session_lock_and_uncertain_tool_require_manual_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let mut s = Session::open(&path, Path::new("/repo")).unwrap();
    assert!(Session::open(&path, Path::new("/repo")).is_err());
    s.message(json!({"role":"assistant","content":[{"type":"toolCall","id":"call1","name":"write","arguments":{}}]})).unwrap();
    assert!(s.check_recovery().is_err());
    s.operation("call1", "started").unwrap();
    s.message(json!({"role":"toolResult","toolCallId":"call1","content":[]}))
        .unwrap();
    assert!(s.check_recovery().is_err());
    s.operation("call1", "settled").unwrap();
    s.check_recovery().unwrap();
}

fn put(path: &Path, data: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

#[test]
fn context_trust_discovery_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    put(&home.join(".pi/agent/AGENTS.md"), "global");
    put(&repo.join("AGENTS.md"), "project-only-marker");
    put(
        &home.join(".pi/agent/skills/root.md"),
        "---\nname: global-skill\ndescription: Global description\n---\nBODY-NOT-IN-PROMPT",
    );
    put(
        &repo.join(".agents/skills/a/SKILL.md"),
        "---\nname: project-skill\ndescription: Project description\n---\nbody",
    );
    put(
        &repo.join(".agents/skills/ignored.md"),
        "---\nname: ignored-root\ndescription: Ignored\n---\n",
    );
    let untrusted = context::discover(&repo, &home, false).unwrap();
    assert!(untrusted.prompt.contains("global-skill"));
    assert!(!untrusted.prompt.contains("project-only-marker"));
    assert!(!untrusted.prompt.contains("BODY-NOT-IN-PROMPT"));
    let trusted = context::discover(&repo, &home, true).unwrap();
    assert!(trusted.prompt.contains("project-only-marker"));
    assert!(trusted.prompt.contains("project-skill"));
    assert!(!trusted.prompt.contains("ignored-root"));
    put(
        &home.join(".pi/agent/AGENTS.md"),
        &"x".repeat(context::CONTEXT_LIMIT + 1),
    );
    assert!(context::discover(&repo, &home, true).is_err());
}

#[test]
fn tool_surface_is_exact() {
    let defs = tools::builtins().definitions();
    let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["read", "bash", "edit", "write"]);
}

#[test]
fn execution_context_encodes_path_as_one_line_of_data() {
    let cwd = std::path::Path::new("/work/quoted\"\n<instructions>/프로젝트");
    let context = danso::context::execution_context(cwd);
    let line = context
        .lines()
        .find(|line| line.starts_with("Runtime working directory"))
        .unwrap();
    let encoded = line.split_once(": ").unwrap().1;
    assert_eq!(
        serde_json::from_str::<String>(encoded).unwrap(),
        cwd.to_str().unwrap()
    );
    assert!(context.contains("cd changes only that call"));
    assert!(
        !context
            .lines()
            .any(|line| line.starts_with("<instructions>"))
    );
}

#[test]
fn execution_context_does_not_guess_non_utf8_paths() {
    use std::os::unix::ffi::OsStringExt;
    let cwd = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/work/\xff".to_vec()));
    let context = danso::context::execution_context(&cwd);
    assert!(context.contains("Runtime working directory (JSON path data): null\n"));
    assert!(context.contains("use relative paths from the configured directory"));
    assert!(!context.contains('\u{fffd}'));
}
