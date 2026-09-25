//! The cron store-mutation surface (#120 PR4): `add`/`edit`/`remove`/
//! `enable`/`disable` under the store flock with the ccc exit-code contract
//! (rc 0 ok, 1 not-found/duplicate/validation, 2 usage), prompt-free result
//! tasks, and the ccc `tasks.json` import path. A port-test of ccc
//! `agent_cron.py::_crud_add` / `_crud_edit` / `crud_command`.
//!
//! These tests call `crud` directly (clap-free mutation core); the CLI layer
//! only translates flags into `crud::FieldFlags`.

use danso::cron::crud::{self, FieldFlags};
use danso::cron::store::{self, store_path};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Fixture {
    /// Not read directly; dropping the fixture removes the store directory.
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn make_store(seed: Option<Value>) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    if let Some(tasks) = seed {
        std::fs::write(&path, serde_json::to_string(&tasks).expect("serializable"))
            .expect("write store");
    }
    Fixture { _dir: dir, path }
}

impl Fixture {
    fn display(&self) -> String {
        self.path.display().to_string()
    }

    fn raw(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.path).expect("read store"))
            .expect("store json")
    }

    fn loaded(&self) -> store::Store {
        store::load(&self.path).expect("valid store")
    }
}

fn seeded() -> Fixture {
    make_store(Some(json!({
        "version": 1,
        "tasks": [{
            "id": "t1",
            "schedule": "30 9 25 9 *",
            "prompt": "original",
            "enabled": true,
            "payload": { "kind": "command", "argv": ["/bin/sh", "-c", "echo hi"] },
        }],
    })))
}

fn add_flags(schedule: &str, prompt: &str) -> FieldFlags {
    FieldFlags {
        schedule: Some(schedule.to_string()),
        prompt: Some(prompt.to_string()),
        ..FieldFlags::default()
    }
}

fn add_ok(fixture: &Fixture, id: &str, flags: &FieldFlags) -> Value {
    let (document, code) =
        crud::add(&fixture.path, &fixture.display(), id, flags).expect("add runs");
    assert_eq!(code, 0, "add should succeed: {document}");
    document
}

fn task_field(document: &Value, pointer: &str) -> Value {
    document["task"]
        .pointer(pointer)
        .cloned()
        .expect("task field")
}

#[test]
fn add_minimal_creates_task_with_defaults_and_prompt_free_result() {
    let fixture = make_store(None);
    let document = add_ok(&fixture, "new", &add_flags("every 10m", "report"));
    // Prompt-free success payload with the mutation marker (ccc contract).
    assert!(document["task"].get("prompt").is_none());
    assert_eq!(document["taskId"], "new");
    assert_eq!(document["mutations"]["taskStoreWrite"], true);
    // Defaults materialized in the stored task.
    let task = &fixture.raw()["tasks"][0];
    assert_eq!(task["id"], "new");
    assert_eq!(task["enabled"], true);
    assert_eq!(task["notify"], "none");
    assert_eq!(task["timezone"], "UTC");
    assert_eq!(task["maxRunHistory"], 20);
    assert_eq!(task["maxCatchup"], 1);
    assert_eq!(task["redactProfile"], "default");
    assert_eq!(task["runCount"], 0);
    assert!(task.get("payload").is_none());
    // The write is private (0600) like every other store write.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&fixture.path)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn add_duplicate_id_is_rc1_before_required_flag_checks() {
    let fixture = seeded();
    let (document, code) = crud::add(
        &fixture.path,
        &fixture.display(),
        "t1",
        &FieldFlags::default(),
    )
    .expect("runs");
    assert_eq!(code, 1);
    assert_eq!(document["error"], "task id already exists");
    // The duplicate check precedes the rc-2 usage checks (ccc ordering).
    assert_eq!(document["mode"], "add");
}

#[test]
fn add_requires_schedule_then_prompt_with_ccc_messages() {
    let fixture = make_store(None);
    let (document, code) = crud::add(
        &fixture.path,
        &fixture.display(),
        "x",
        &FieldFlags::default(),
    )
    .expect("runs");
    assert_eq!(code, 2);
    assert_eq!(document["error"], "--schedule is required");
    let flags = FieldFlags {
        schedule: Some("every 10m".to_string()),
        ..FieldFlags::default()
    };
    let (document, code) = crud::add(&fixture.path, &fixture.display(), "x", &flags).expect("runs");
    assert_eq!(code, 2);
    assert_eq!(
        document["error"],
        "--prompt is required (for command payloads it is the human description)"
    );
    assert!(fixture.loaded().tasks.is_empty());
}

#[test]
fn add_argv_builds_command_payload_and_bak_snapshot_appears_on_second_write() {
    let fixture = make_store(None);
    let flags = FieldFlags {
        argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
        ..add_flags("every 10m", "run job")
    };
    let document = add_ok(&fixture, "cmd1", &flags);
    assert_eq!(task_field(&document, "/payload/kind"), "command");
    assert_eq!(
        task_field(&document, "/payload/argv"),
        json!(["/bin/sh", "-c", "echo hi"])
    );
    assert!(document["task"]["payload"].get("model").is_none());
    // First write: no previous content, so no .bak yet.
    let backup = fixture.path.with_file_name("tasks.json.bak");
    assert!(!backup.exists());
    add_ok(&fixture, "cmd2", &add_flags("every 10m", "second"));
    let backup_text = std::fs::read_to_string(&backup).expect("backup exists");
    assert!(backup_text.contains("cmd1"));
    assert!(!backup_text.contains("cmd2"));
}

#[test]
fn add_payload_flags_only_build_prompt_payload() {
    let fixture = make_store(None);
    // `--model`/`--timeout-sec` alone form a prompt payload (model is
    // prompt-legal); `--cwd` is command-only, so a cwd-only add is rejected
    // exactly like ccc's add (prompt payload + cwd fails candidate
    // validation).
    let flags = FieldFlags {
        model: Some("claude-x".to_string()),
        timeout_sec: Some(90),
        ..add_flags("every 10m", "p")
    };
    let document = add_ok(&fixture, "p1", &flags);
    assert_eq!(task_field(&document, "/payload/kind"), "prompt");
    assert_eq!(task_field(&document, "/payload/model"), "claude-x");
    assert!(document["task"]["payload"].get("argv").is_none());
    assert!(document["task"]["payload"].get("cwd").is_none());
    let flags = FieldFlags {
        cwd: Some("/tmp".to_string()),
        ..add_flags("every 10m", "p")
    };
    let (document, code) =
        crud::add(&fixture.path, &fixture.display(), "p2", &flags).expect("runs");
    assert_eq!(code, 1);
    assert!(
        document["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cwd is not allowed for kind 'prompt'")
    );
}

#[test]
fn add_command_payload_rejects_model_rc1() {
    let fixture = make_store(None);
    let flags = FieldFlags {
        argv: vec!["/bin/true".to_string()],
        model: Some("claude-x".to_string()),
        ..add_flags("every 10m", "p")
    };
    let (document, code) =
        crud::add(&fixture.path, &fixture.display(), "bad", &flags).expect("runs");
    assert_eq!(code, 1);
    assert!(
        document["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|error| {
                error
                    .as_str()
                    .unwrap_or_default()
                    .contains("model is not allowed")
            })
    );
    assert_eq!(document["error"], document["errors"][0]);
}

#[test]
fn add_invalid_schedule_and_not_before_are_rc2() {
    let fixture = make_store(None);
    let (document, code) = crud::add(
        &fixture.path,
        &fixture.display(),
        "x",
        &add_flags("bogus", "p"),
    )
    .expect("runs");
    assert_eq!(code, 2);
    assert!(
        document["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("invalid schedule bounds")
    );
    let flags = FieldFlags {
        not_before: Some("yesterday".to_string()),
        ..add_flags("every 10m", "p")
    };
    let (document, code) = crud::add(&fixture.path, &fixture.display(), "x", &flags).expect("runs");
    assert_eq!(code, 2);
    assert!(
        document["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("invalid schedule bounds")
    );
}

#[test]
fn add_candidate_validation_and_enum_flags_are_rc1() {
    let fixture = make_store(None);
    // Chat id pattern fails at candidate validation.
    let flags = FieldFlags {
        notify_chat_id: Some("bad id!".to_string()),
        ..add_flags("every 10m", "p")
    };
    let (document, code) = crud::add(&fixture.path, &fixture.display(), "x", &flags).expect("runs");
    assert_eq!(code, 1);
    assert!(
        document["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty())
    );
    // Enum-valued flags are rc 1 (they fail inside validate_store in ccc).
    let flags = FieldFlags {
        notify: Some("bogus".to_string()),
        ..add_flags("every 10m", "p")
    };
    let (document, code) = crud::add(&fixture.path, &fixture.display(), "x", &flags).expect("runs");
    assert_eq!(code, 1);
    assert!(
        document["error"]
            .as_str()
            .unwrap_or_default()
            .contains("notify")
    );
    // But rc-2 usage checks precede the rc-1 enum checks.
    let flags = FieldFlags {
        notify: Some("bogus".to_string()),
        ..FieldFlags::default()
    };
    let (_, code) = crud::add(&fixture.path, &fixture.display(), "x", &flags).expect("runs");
    assert_eq!(code, 2);
}

#[test]
fn add_parses_success_exit_codes() {
    let fixture = make_store(None);
    let flags = FieldFlags {
        success_exit_codes: Some(vec![0, 7]),
        ..add_flags("every 10m", "watch")
    };
    let document = add_ok(&fixture, "watch", &flags);
    assert_eq!(task_field(&document, "/successExitCodes"), json!([0, 7]));
}

#[test]
fn edit_is_a_set_only_partial_update() {
    let fixture = seeded();
    let loaded = fixture.loaded();
    let original = &loaded.tasks[0];
    let flags = FieldFlags {
        schedule: Some("every 5m".to_string()),
        max_runs: Some(5),
        ..FieldFlags::default()
    };
    let (document, code) =
        crud::edit(&fixture.path, &fixture.display(), "t1", &flags).expect("runs");
    assert_eq!(code, 0);
    assert!(document["task"].get("prompt").is_none());
    let updated = &fixture.loaded().tasks[0];
    assert_eq!(updated.schedule, "every 5m");
    assert_eq!(updated.max_runs, Some(5));
    // Untouched fields survive, including the payload.
    assert_eq!(updated.prompt, original.prompt);
    assert_eq!(updated.enabled, original.enabled);
    assert_eq!(
        serde_json::to_value(&updated.payload).expect("serializable"),
        serde_json::to_value(&original.payload).expect("serializable")
    );
}

#[test]
fn edit_merges_payload_and_argv_flips_kind_to_command() {
    // ccc: argv absent + payload flag on an existing command payload keeps
    // kind=command. So exercise the merge from a payload-free task.
    let store_without_payload = make_store(Some(json!({
        "version": 1,
        "tasks": [
            { "id": "t1", "schedule": "every 10m", "prompt": "p", "enabled": true },
            { "id": "t2", "schedule": "every 10m", "prompt": "p2", "enabled": true },
        ],
    })));
    let flags_prompt = FieldFlags {
        timeout_sec: Some(120),
        ..FieldFlags::default()
    };
    let (document, code) = crud::edit(
        &store_without_payload.path,
        &store_without_payload.display(),
        "t1",
        &flags_prompt,
    )
    .expect("runs");
    assert_eq!(code, 0);
    assert_eq!(task_field(&document, "/payload/kind"), "prompt");
    assert_eq!(task_field(&document, "/payload/timeoutSec"), 120);
    // `--argv` replaces the whole argv and flips the kind to command,
    // keeping the merged timeout.
    let flags_argv = FieldFlags {
        argv: vec!["/bin/true".to_string()],
        ..FieldFlags::default()
    };
    let (document, code) = crud::edit(
        &store_without_payload.path,
        &store_without_payload.display(),
        "t1",
        &flags_argv,
    )
    .expect("runs");
    assert_eq!(code, 0, "DEBUG: {document}");
    assert_eq!(task_field(&document, "/payload/kind"), "command");
    let tasks = store_without_payload.loaded().tasks;
    let payload = tasks[0].payload.as_ref().expect("payload exists");
    assert_eq!(payload.kind, store::PayloadKind::Command);
    assert_eq!(
        payload.argv.as_deref(),
        Some(&["/bin/true".to_string()][..])
    );
    assert_eq!(payload.timeout_sec, Some(120));
    // A second task: the model flag merges into a fresh prompt payload.
    let flags_model = FieldFlags {
        model: Some("claude-x".to_string()),
        ..FieldFlags::default()
    };
    let (document, code) = crud::edit(
        &store_without_payload.path,
        &store_without_payload.display(),
        "t2",
        &flags_model,
    )
    .expect("runs");
    assert_eq!(code, 0, "DEBUG: {document}");
    assert_eq!(task_field(&document, "/payload/kind"), "prompt");
    assert_eq!(task_field(&document, "/payload/model"), "claude-x");
}

#[test]
fn edit_argv_flip_with_model_still_set_is_rc1_like_ccc() {
    // ccc merges payload flags, so switching a model-carrying prompt payload
    // to a command payload leaves `model` behind and candidate validation
    // rejects it (model is not allowed for kind 'command').
    let fixture = make_store(Some(json!({
        "version": 1,
        "tasks": [{ "id": "t1", "schedule": "every 10m", "prompt": "p", "enabled": true }],
    })));
    let model = FieldFlags {
        model: Some("claude-x".to_string()),
        ..FieldFlags::default()
    };
    let (_, code) = crud::edit(&fixture.path, &fixture.display(), "t1", &model).expect("runs");
    assert_eq!(code, 0);
    let argv = FieldFlags {
        argv: vec!["/bin/true".to_string()],
        ..FieldFlags::default()
    };
    let (document, code) =
        crud::edit(&fixture.path, &fixture.display(), "t1", &argv).expect("runs");
    assert_eq!(code, 1);
    assert!(
        document["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|error| {
                error
                    .as_str()
                    .unwrap_or_default()
                    .contains("model is not allowed")
            })
    );
    // The task keeps its prompt payload (the rejected edit wrote nothing).
    let tasks = fixture.loaded().tasks;
    let payload = tasks[0].payload.as_ref().expect("payload");
    assert_eq!(payload.kind, store::PayloadKind::Prompt);
}

#[test]
fn edit_unknown_id_rc1_precedes_missing_flags_rc2() {
    let fixture = seeded();
    let (document, code) = crud::edit(
        &fixture.path,
        &fixture.display(),
        "ghost",
        &FieldFlags::default(),
    )
    .expect("runs");
    assert_eq!(code, 1);
    assert_eq!(document["error"], "task id not found");
    let flags = FieldFlags {
        max_runs: Some(9),
        ..FieldFlags::default()
    };
    let (_, code) = crud::edit(&fixture.path, &fixture.display(), "ghost", &flags).expect("runs");
    assert_eq!(code, 1);
    let (_, code) = crud::edit(
        &fixture.path,
        &fixture.display(),
        "t1",
        &FieldFlags::default(),
    )
    .expect("runs");
    assert_eq!(code, 2, "no field flags is a usage error");
}

#[test]
fn edit_keep_after_run_and_disabled_are_one_way() {
    let fixture = seeded();
    let flags = FieldFlags {
        keep_after_run: true,
        disabled: true,
        ..FieldFlags::default()
    };
    let (_, code) = crud::edit(&fixture.path, &fixture.display(), "t1", &flags).expect("runs");
    assert_eq!(code, 0);
    let task = &fixture.loaded().tasks[0];
    assert!(task.keep_after_run);
    assert!(!task.enabled);
    // `enable` (not edit) restores the task.
    let (document, code) =
        crud::simple(&fixture.path, &fixture.display(), "enable", "t1").expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["changed"], true);
    assert!(fixture.loaded().tasks[0].enabled);
}

#[test]
fn remove_stores_only_and_reports_missing() {
    let fixture = seeded();
    let (document, code) =
        crud::simple(&fixture.path, &fixture.display(), "remove", "t1").expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["ok"], true);
    assert_eq!(document["mutations"]["taskStoreWrite"], true);
    assert!(document.get("task").is_none());
    assert!(fixture.loaded().tasks.is_empty());
    let (document, code) =
        crud::simple(&fixture.path, &fixture.display(), "remove", "t1").expect("runs");
    assert_eq!(code, 1);
    assert_eq!(document["error"], "task id not found");
}

#[test]
fn enable_disable_write_only_on_change() {
    let fixture = seeded();
    let before = std::fs::read(&fixture.path).expect("read");
    // Already enabled: no write, `changed: false`.
    let (document, code) =
        crud::simple(&fixture.path, &fixture.display(), "enable", "t1").expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["changed"], false);
    assert_eq!(document["enabled"], true);
    assert_eq!(document["mutations"]["taskStoreWrite"], false);
    assert_eq!(std::fs::read(&fixture.path).expect("read"), before);
    // Disable flips and writes.
    let (document, code) =
        crud::simple(&fixture.path, &fixture.display(), "disable", "t1").expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["changed"], true);
    assert!(!fixture.loaded().tasks[0].enabled);
    // Second disable is a no-op write again.
    let (document, _) =
        crud::simple(&fixture.path, &fixture.display(), "disable", "t1").expect("runs");
    assert_eq!(document["changed"], false);
}

#[test]
fn crud_fails_closed_on_corrupt_store() {
    let fixture = make_store(None);
    std::fs::write(&fixture.path, "not json").expect("write corrupt");
    let outcome = crud::add(
        &fixture.path,
        &fixture.display(),
        "x",
        &add_flags("every 10m", "p"),
    );
    let error = outcome.expect_err("corrupt store is refused");
    assert!(error.contains("not a valid v1 store"), "{error}");
}

#[test]
fn add_waits_for_the_store_flock() {
    let fixture = make_store(None);
    let path = fixture.path.clone();
    let holder = std::thread::spawn(move || {
        let _guard = danso::cron::locks::store_flock(&path).expect("flock");
        std::thread::sleep(Duration::from_millis(300));
    });
    std::thread::sleep(Duration::from_millis(80));
    let started = Instant::now();
    let (_, code) = crud::add(
        &fixture.path,
        &fixture.display(),
        "x",
        &add_flags("every 10m", "p"),
    )
    .expect("add completes");
    assert_eq!(code, 0);
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "add must block until the flock holder releases"
    );
    holder.join().expect("holder thread");
}

#[test]
fn import_adds_tasks_and_materializes_defaults() {
    let fixture = make_store(None);
    let source = fixture.path.with_file_name("ccc-tasks.json");
    std::fs::write(
        &source,
        serde_json::to_string(&json!({
            "version": 1,
            "tasks": [{ "id": "imported", "schedule": "every 10m", "prompt": "check", "enabled": true }],
        }))
        .expect("serializable"),
    )
    .expect("write source");
    let (document, code) = crud::import(&fixture.path, &fixture.display(), &source).expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["added"], json!(["imported"]));
    assert_eq!(document["skipped"], json!([]));
    assert_eq!(document["mutations"]["taskStoreWrite"], true);
    let task = &fixture.loaded().tasks[0];
    assert_eq!(task.id, "imported");
    assert_eq!(task.max_run_history, 20);
    assert_eq!(task.max_catchup, 1);
    assert_eq!(task.notify, store::NotifyMode::None);
    assert_eq!(task.redact_profile, "default");
}

#[test]
fn import_skips_existing_ids_and_writes_only_on_add() {
    let fixture = seeded();
    let source = fixture.path.with_file_name("ccc-tasks.json");
    let write_source = |tasks: Value| {
        std::fs::write(
            &source,
            serde_json::to_string(&json!({ "version": 1, "tasks": tasks })).expect("serializable"),
        )
        .expect("write source")
    };
    write_source(json!([
        { "id": "t1", "schedule": "every 10m", "prompt": "other", "enabled": false },
        { "id": "t2", "schedule": "at 2030-01-01T00:00:00Z", "prompt": "one-shot", "enabled": true },
    ]));
    let (document, code) = crud::import(&fixture.path, &fixture.display(), &source).expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["added"], json!(["t2"]));
    assert_eq!(
        document["skipped"],
        json!([{ "id": "t1", "reason": "task id already exists" }])
    );
    // The existing task is untouched by the import.
    let tasks = fixture.loaded().tasks;
    assert_eq!(tasks[0].prompt, "original");
    assert!(tasks[0].enabled);
    assert_eq!(tasks.len(), 2);
    // A second import adds nothing and performs no write.
    let before = std::fs::read(&fixture.path).expect("read");
    let (document, code) = crud::import(&fixture.path, &fixture.display(), &source).expect("runs");
    assert_eq!(code, 0);
    assert_eq!(document["added"], json!([]));
    assert_eq!(document["mutations"]["taskStoreWrite"], false);
    assert_eq!(std::fs::read(&fixture.path).expect("read"), before);
}

#[test]
fn import_fails_closed_on_invalid_or_missing_source() {
    let fixture = make_store(None);
    let source = fixture.path.with_file_name("bad.json");
    std::fs::write(&source, json!({ "version": 1, "tasks": [] }).to_string()).expect("write");
    // A valid but non-store document.
    std::fs::write(&source, "[]").expect("write");
    let outcome = crud::import(&fixture.path, &fixture.display(), &source);
    assert!(outcome.is_err());
    // A semantically invalid source (duplicate ids) fails at source load —
    // the same stderr rc-1 path as a corrupt store, before any mutation.
    std::fs::write(
        &source,
        json!({
            "version": 1,
            "tasks": [
                { "id": "d", "schedule": "every 10m", "prompt": "a", "enabled": true },
                { "id": "d", "schedule": "every 10m", "prompt": "b", "enabled": true },
            ],
        })
        .to_string(),
    )
    .expect("write");
    let outcome = crud::import(&fixture.path, &fixture.display(), &source);
    let error = outcome.expect_err("duplicate ids fail closed");
    assert!(error.contains("duplicate"), "{error}");
    // A missing source file is a store-level error, not a panic.
    let outcome = crud::import(
        &fixture.path,
        &fixture.display(),
        Path::new("/nonexistent/tasks.json"),
    );
    let error = outcome.expect_err("missing source");
    assert!(error.contains("cannot read import source"), "{error}");
}
