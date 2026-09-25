//! The state-commit write path and the run-lock machinery (#120 PR2, §6.5).
//!
//! The semantics mirror ccc `agent_cron.py` (`commit_run_state`,
//! `append_run_history`, `apply_run_limit`, the lock commands) plus the two
//! documented danso divergences from §7: the `.bak` store snapshot and the
//! `cron/history/<id>.jsonl` overflow archive.

use chrono::{DateTime, Utc};
use danso::cron::commit::{
    append_run_history, apply_retry_transition, apply_run_limit, archive_history, commit_run_state,
    history_attempt, run_limit_metadata, write_store_document,
};
use danso::cron::locks::{
    acquire_for_run, lock_command, lock_path, quarantine_persist_failure, read_lock,
    release_for_run, store_flock, store_lock_path,
};
use danso::cron::store::{self, RunHistoryItem, store_path};
use danso::cron::time::{boot_id, parse_utc};
use serde_json::{Value, json};
use std::path::PathBuf;

const AT: &str = "2026-09-25T09:30:00Z";
const OTHER_BOOT: &str = "00000000-0000-0000-0000-000000000000";

fn utc(raw: &str) -> DateTime<Utc> {
    parse_utc(Some(raw), "test")
        .expect("parse")
        .expect("non-empty")
}

fn at() -> DateTime<Utc> {
    utc(AT)
}

struct Fixture {
    /// Not read directly; dropping the fixture removes the store's directory.
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn fixture(tasks: Value) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, serde_json::to_string(&tasks).expect("serializable")).expect("write");
    Fixture { _dir: dir, path }
}

fn base_task(id: &str) -> Value {
    json!({
        "id": id,
        "schedule": "*/15 9-17 * * 1-5",
        "prompt": "report",
        "enabled": true,
    })
}

fn read_store(fixture: &Fixture) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&fixture.path).expect("read store"))
        .expect("JSON")
}

fn history_entry(attempt: i64, scheduled_at: &str) -> RunHistoryItem {
    serde_json::from_value(json!({
        "runId": format!("r{attempt}"),
        "scheduledAt": scheduled_at,
        "startedAt": AT,
        "status": "failed",
        "attempt": attempt,
        "notifyState": "none",
    }))
    .expect("valid history entry")
}

// --- store write path: atomic replace + §7 .bak snapshot ---

#[test]
fn a_store_write_snapshots_the_previous_content_to_bak() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let original = std::fs::read_to_string(&fixture.path).expect("read");

    let mut document = read_store(&fixture);
    document["tasks"][0]["prompt"] = json!("changed");
    write_store_document(&fixture.path, &document).expect("write");

    let backup_path = fixture.path.with_file_name("tasks.json.bak");
    assert_eq!(
        std::fs::read_to_string(&backup_path).expect("bak exists"),
        original,
        "the .bak holds the pre-write content"
    );
    let reloaded = read_store(&fixture);
    assert_eq!(reloaded["tasks"][0]["prompt"], json!("changed"));

    use std::os::unix::fs::PermissionsExt as _;
    for path in [&fixture.path, &backup_path] {
        let mode = std::fs::metadata(path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{path:?} stays owner-only");
    }
    let leftovers: Vec<String> = std::fs::read_dir(fixture.path.parent().expect("parent"))
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files are cleaned: {leftovers:?}"
    );
}

#[test]
fn an_invalid_store_is_refused_without_touching_anything() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let original = std::fs::read_to_string(&fixture.path).expect("read");

    let mut document = read_store(&fixture);
    document["version"] = json!(2);
    let error = write_store_document(&fixture.path, &document).expect_err("refused");
    assert!(
        error.contains("refusing invalid agent-cron store"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.path).expect("read"),
        original,
        "the store is untouched"
    );
    assert!(!fixture.path.with_file_name("tasks.json.bak").exists());
}

// --- runHistory: append, cap, overflow archive, attempt counting ---

#[test]
fn history_overflow_archives_evicted_entries() {
    let mut task = base_task("a");
    task["maxRunHistory"] = json!(3);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    let mut task = store.tasks[0].clone();

    let mut all_evicted = Vec::new();
    for attempt in 1..=5 {
        let evicted = append_run_history(
            &mut task,
            history_entry(attempt, &format!("2026-09-25T09:{:02}:00Z", attempt)),
        );
        all_evicted.extend(evicted);
    }
    assert_eq!(task.run_history.len(), 3, "the cap keeps the last entries");
    assert_eq!(task.run_history[0].run_id, "r3");
    assert_eq!(task.run_history[2].run_id, "r5");
    assert_eq!(all_evicted.len(), 2, "two entries were evicted");

    archive_history(&fixture.path, "a", &all_evicted).expect("archive");
    let archived = std::fs::read_to_string(fixture.path.parent().unwrap().join("history/a.jsonl"))
        .expect("archive file");
    let lines: Vec<Value> = archived
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["runId"], json!("r1"), "oldest evicted first");
    assert_eq!(lines[1]["runId"], json!("r2"));
}

#[test]
fn history_attempt_counts_matching_occurrences() {
    let mut task = base_task("a");
    task["retryState"] = json!({
        "scheduledAt": AT,
        "attempt": 2,
        "lastStatus": "failed",
    });
    task["runHistory"] = json!([{
        "runId": "r1",
        "scheduledAt": AT,
        "startedAt": AT,
        "status": "failed",
        "attempt": 3,
        "notifyState": "none",
    }, {
        "runId": "r2",
        "scheduledAt": "2026-09-25T09:15:00Z",
        "startedAt": AT,
        "status": "success",
        "attempt": 1,
        "notifyState": "none",
    }]);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    assert_eq!(history_attempt(&store.tasks[0], AT), 4);
    assert_eq!(history_attempt(&store.tasks[0], "2026-09-25T09:15:00Z"), 2);
}

// --- run limit + retry transitions ---

#[test]
fn run_limit_application_disables_at_the_cap() {
    let mut task = base_task("a");
    task["maxRuns"] = json!(2);
    task["retryState"] = json!({
        "scheduledAt": AT,
        "attempt": 1,
        "lastStatus": "failed",
    });
    let fx = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fx.path).expect("valid store");
    let mut task = store.tasks[0].clone();

    let metadata = apply_run_limit(&mut task);
    assert_eq!(metadata["runCount"], json!(1));
    assert_eq!(metadata["reached"], json!(false));
    assert!(task.enabled, "still enabled before the cap");

    let metadata = apply_run_limit(&mut task);
    assert_eq!(metadata["reached"], json!(true));
    assert!(!task.enabled, "the cap disables the task");
    assert!(task.retry_state.is_none(), "pending retries are dropped");

    // A task without maxRuns never counts toward anything.
    let mut unlimited = base_task("b");
    unlimited["maxRuns"] = serde_json::Value::Null;
    let fx_b = fixture(json!({ "version": 1, "tasks": [unlimited] }));
    let store = store::load(&fx_b.path).expect("valid store");
    let mut unlimited = store.tasks[0].clone();
    let metadata = apply_run_limit(&mut unlimited);
    assert_eq!(metadata["reached"], json!(false));
    assert_eq!(metadata["maxRuns"], Value::Null);
    assert_eq!(unlimited.run_count, 0);
}

#[test]
fn retry_transitions_follow_the_reference() {
    let mut task = base_task("a");
    task["retryPolicy"] = json!({ "maxAttempts": 3, "backoffSec": 60, "backoffMultiplier": 2 });
    task["retryState"] = json!({ "scheduledAt": AT, "attempt": 1, "lastStatus": "failed" });
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");

    // Success clears any state.
    let mut task = store.tasks[0].clone();
    let transition = apply_retry_transition(&mut task, AT, 2, "r1", "success", at());
    assert_eq!(transition["cleared"], json!(true));
    assert_eq!(transition["exhausted"], json!(false));
    assert!(task.retry_state.is_none());

    // A declared policy failing below the cap schedules the next attempt,
    // truncated to the minute (backoffSec * multiplier^(attempt-1)).
    let mut task = store.tasks[0].clone();
    let transition = apply_retry_transition(&mut task, AT, 1, "r1", "failed", at());
    assert_eq!(transition["retryEligibleAt"], json!("2026-09-25T09:31:00Z"));
    let state = task.retry_state.as_ref().expect("state");
    assert_eq!(state.attempt, 1);
    assert_eq!(state.last_status, "failed");
    assert_eq!(state.last_run_id.as_deref(), Some("r1"));

    let transition = apply_retry_transition(&mut task, AT, 2, "r2", "failed", at());
    assert_eq!(transition["retryEligibleAt"], json!("2026-09-25T09:32:00Z"));

    // At the cap the task is marked exhausted with no eligible time.
    let transition = apply_retry_transition(&mut task, AT, 3, "r3", "failed", at());
    assert_eq!(transition["exhausted"], json!(true));
    assert_eq!(transition["retryEligibleAt"], Value::Null);
    let state = task.retry_state.as_ref().expect("state");
    assert_eq!(state.last_status, "exhausted");
    assert!(state.retry_eligible_at.is_none());
}

#[test]
fn a_missing_or_empty_retry_policy_never_marks_exhausted() {
    // No policy at all.
    let fx = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = store::load(&fx.path).expect("valid store");
    let mut task = store.tasks[0].clone();
    let transition = apply_retry_transition(&mut task, AT, 5, "r1", "failed", at());
    assert_eq!(transition["noPolicy"], json!(true));
    assert_eq!(transition["exhausted"], json!(false));
    assert!(task.retry_state.is_none());

    // An empty policy object is falsy in the reference: undeclared.
    let mut task = base_task("b");
    task["retryPolicy"] = json!({});
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    let mut task = store.tasks[0].clone();
    let transition = apply_retry_transition(&mut task, AT, 5, "r1", "failed", at());
    assert_eq!(transition["noPolicy"], json!(true));
}

// --- commit_run_state: projected run-state commit ---

#[test]
fn commit_projects_only_run_state_fields() {
    let fixture = fixture(json!({
        "version": 1,
        "tasks": [base_task("a"), base_task("b")],
    }));

    // The run's view of task "a", mid-flight.
    let mut run_task = read_store(&fixture)["tasks"][0].clone();
    run_task["lastRunAt"] = json!(AT);
    run_task["lastStatus"] = json!("success");
    run_task["lastRunId"] = json!("a-1-1");
    run_task["runCount"] = json!(7);
    run_task["runHistory"] = json!([{
        "runId": "a-1-1",
        "scheduledAt": AT,
        "startedAt": AT,
        "finishedAt": AT,
        "status": "success",
        "exitCode": 0,
        "attempt": 1,
        "notifyState": "none",
    }]);

    // Meanwhile an operator edits the prompt and adds a task.
    let mut edited = read_store(&fixture);
    edited["tasks"][0]["prompt"] = json!("edited mid-flight");
    edited["tasks"]
        .as_array_mut()
        .expect("tasks array")
        .push(json!({
            "id": "c",
            "schedule": "@daily",
            "prompt": "new",
            "enabled": true,
        }));
    std::fs::write(
        &fixture.path,
        serde_json::to_string(&edited).expect("serializable"),
    )
    .expect("write");

    let persisted =
        commit_run_state(&fixture.path, "a", &run_task, false).expect("commit succeeds");
    assert!(persisted);

    let fresh = read_store(&fixture);
    assert_eq!(
        fresh["tasks"][0]["prompt"],
        json!("edited mid-flight"),
        "the concurrent edit survives"
    );
    assert_eq!(fresh["tasks"][0]["lastRunAt"], json!(AT));
    assert_eq!(fresh["tasks"][0]["lastStatus"], json!("success"));
    assert_eq!(fresh["tasks"][0]["runCount"], json!(7));
    assert_eq!(fresh["tasks"][0]["runHistory"][0]["runId"], json!("a-1-1"));
    assert_eq!(
        fresh["tasks"].as_array().expect("tasks").len(),
        3,
        "task c survives"
    );
    assert_eq!(fresh["tasks"][1]["id"], json!("b"));
}

#[test]
fn commit_can_only_disable_and_reports_removal() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let mut run_task = read_store(&fixture)["tasks"][0].clone();
    run_task["lastStatus"] = json!("success");

    // disable=true clears enabled but can never set it back on.
    let persisted = commit_run_state(&fixture.path, "a", &run_task, true).expect("commit succeeds");
    assert!(persisted);
    assert_eq!(read_store(&fixture)["tasks"][0]["enabled"], json!(false));

    // A removed task is reported, not resurrected.
    let persisted =
        commit_run_state(&fixture.path, "nope", &run_task, false).expect("commit succeeds");
    assert!(!persisted);
    assert_eq!(read_store(&fixture)["tasks"].as_array().unwrap().len(), 1);

    // Absent run-state fields are removed, never left as nulls.
    let mut partial = read_store(&fixture)["tasks"][0].clone();
    {
        let fields = partial.as_object_mut().expect("task object");
        fields.remove("lastStatus");
        fields.remove("lastRunAt");
    }
    partial["runCount"] = json!(3);
    let _ = commit_run_state(&fixture.path, "a", &partial, false).expect("commit");
    let fresh = read_store(&fixture)["tasks"][0].clone();
    assert_eq!(fresh["runCount"], json!(3));
    assert!(
        fresh.get("lastStatus").is_none(),
        "absent fields are removed"
    );
    assert!(fresh.get("lastRunAt").is_none());
}

#[test]
fn commit_fails_closed_on_a_broken_store() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let run_task = read_store(&fixture)["tasks"][0].clone();

    // Corrupted store: the run happened, so the failure must be loud.
    std::fs::write(&fixture.path, "{not json").expect("write");
    let error = commit_run_state(&fixture.path, "a", &run_task, false).expect_err("loud");
    assert!(
        error.contains("task store load failed during run-state commit"),
        "{error}"
    );

    // A missing store file counts as "the task was removed" (ccc load_doc).
    std::fs::remove_file(&fixture.path).expect("remove");
    let persisted = commit_run_state(&fixture.path, "a", &run_task, false).expect("ok");
    assert!(!persisted);
}

// --- locks: acquire, stale reclaim, release, quarantine ---

#[test]
fn acquire_writes_an_exclusive_owner_only_lock() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = store::load(&fixture.path).expect("valid store");

    let (acquired, detail) =
        acquire_for_run(&fixture.path, "a", &store.tasks[0], "r1", AT, at()).expect("flock");
    assert!(acquired);
    assert_eq!(detail["state"], json!("acquired"));

    let path = lock_path(&fixture.path, "a");
    let lock = read_lock(&path);
    assert_eq!(lock["taskId"], json!("a"));
    assert_eq!(lock["runId"], json!("r1"));
    assert_eq!(lock["bootId"], json!(boot_id()));
    assert_eq!(lock["scheduledAt"], json!(AT));
    assert_eq!(lock["acquiredAt"], json!(AT));
    assert_eq!(lock["pid"], json!(std::process::id()));
    assert!(lock["host"].is_string());
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "run locks are owner-only");

    // A second acquirer is refused while the lock is held.
    let (acquired, detail) =
        acquire_for_run(&fixture.path, "a", &store.tasks[0], "r2", AT, at()).expect("flock");
    assert!(!acquired);
    assert_eq!(detail["state"], json!("held"));
}

#[test]
fn a_stale_lock_is_reclaimed_by_rename() {
    let mut task = base_task("a");
    task["lockTimeoutSec"] = json!(0);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    let locks = fixture.path.parent().unwrap().join("locks");
    std::fs::create_dir_all(&locks).expect("mkdir");
    std::fs::write(
        lock_path(&fixture.path, "a"),
        serde_json::to_string(&json!({
            "taskId": "a",
            "runId": "ghost",
            "pid": 1,
            "bootId": OTHER_BOOT,
            "acquiredAt": "2020-01-01T00:00:00Z",
            "scheduledAt": "2020-01-01T00:00:00Z",
        }))
        .expect("serializable"),
    )
    .expect("write lock");

    let (acquired, detail) =
        acquire_for_run(&fixture.path, "a", &store.tasks[0], "r1", AT, at()).expect("flock");
    assert!(acquired, "a foreign-boot lock is stale and reclaimable");
    assert_eq!(detail["state"], json!("acquired"));

    let asides: Vec<String> = std::fs::read_dir(&locks)
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".stale."))
        .collect();
    assert_eq!(asides.len(), 1, "the stale lock is moved aside: {asides:?}");
    let aside = read_lock(&locks.join(&asides[0]));
    assert_eq!(
        aside["runId"],
        json!("ghost"),
        "the old holder is preserved"
    );
}

#[test]
fn release_requires_the_exact_run_id() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = store::load(&fixture.path).expect("valid store");
    let _ = acquire_for_run(&fixture.path, "a", &store.tasks[0], "r1", AT, at()).expect("flock");

    let mismatch = release_for_run(&fixture.path, "a", "r2").expect("flock");
    assert_eq!(mismatch["ok"], json!(false));
    assert_eq!(mismatch["state"], json!("release-mismatch"));

    let released = release_for_run(&fixture.path, "a", "r1").expect("flock");
    assert_eq!(released["ok"], json!(true));
    assert_eq!(released["state"], json!("released"));
    assert!(!lock_path(&fixture.path, "a").exists(), "the lock is gone");

    let free = release_for_run(&fixture.path, "a", "r1").expect("flock");
    assert_eq!(free["state"], json!("free"));
}

#[test]
fn quarantine_blocks_until_the_exact_release() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));
    let store = store::load(&fixture.path).expect("valid store");
    let _ = acquire_for_run(&fixture.path, "a", &store.tasks[0], "r1", AT, at()).expect("flock");

    let quarantined = quarantine_persist_failure(&fixture.path, "a", "r1", at()).expect("flock");
    assert_eq!(quarantined["ok"], json!(true));
    assert_eq!(quarantined["state"], json!("persist-failed"));
    let lock = read_lock(&lock_path(&fixture.path, "a"));
    assert_eq!(lock["state"], json!("persist-failed"));
    assert_eq!(lock["persistFailedAt"], json!(AT));

    // A quarantined lock is not stale, not free, and refuses new acquirers.
    let plan =
        danso::cron::due::due_plan(&store, &fixture.path.display().to_string(), Some(AT), at());
    assert_eq!(plan["tasks"][0]["lockState"], json!("persist-failed"));
    let (acquired, detail) =
        acquire_for_run(&fixture.path, "a", &store.tasks[0], "r2", AT, at()).expect("flock");
    assert!(!acquired);
    assert_eq!(detail["state"], json!("persist-failed"));

    // Only the exact run id can release it.
    let mismatch = release_for_run(&fixture.path, "a", "r2").expect("flock");
    assert_eq!(mismatch["ok"], json!(false));
    let released = release_for_run(&fixture.path, "a", "r1").expect("flock");
    assert_eq!(released["state"], json!("released"));

    // Quarantine edge cases: missing lock and mismatched holder.
    let missing = quarantine_persist_failure(&fixture.path, "a", "r1", at()).expect("flock");
    assert_eq!(missing["state"], json!("persist-quarantine-missing"));
    let _ = acquire_for_run(&fixture.path, "a", &store.tasks[0], "r9", AT, at()).expect("flock");
    let mismatch = quarantine_persist_failure(&fixture.path, "a", "r1", at()).expect("flock");
    assert_eq!(mismatch["state"], json!("persist-quarantine-mismatch"));
}

#[test]
fn the_lock_command_probes_acquires_and_releases() {
    let mut task = base_task("a");
    task["lockTimeoutSec"] = json!(0);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");

    let (result, code) =
        lock_command(&fixture.path, "a", &store.tasks[0], "probe", "", "", at()).expect("probe");
    assert_eq!(code, 0);
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["lockState"], json!("free"));
    assert_eq!(result["mutations"]["lockAcquire"], json!(false));
    assert_eq!(result["mutations"]["execute"], json!(false));

    let (result, code) = lock_command(
        &fixture.path,
        "a",
        &store.tasks[0],
        "acquire",
        "r1",
        AT,
        at(),
    )
    .expect("acquire");
    assert_eq!(code, 0);
    assert_eq!(result["lockState"], json!("acquired"));
    assert_eq!(result["reclaimedStale"], json!(false));
    assert_eq!(result["holder"]["runId"], json!("r1"));
    assert_eq!(result["mutations"]["lockAcquire"], json!(true));
    assert_eq!(result["mutations"]["taskStoreWrite"], json!(false));

    let (result, code) =
        lock_command(&fixture.path, "a", &store.tasks[0], "probe", "", "", at()).expect("probe");
    assert_eq!(code, 0);
    assert_eq!(result["lockState"], json!("held"));
    assert_eq!(result["holderAlive"], json!(true), "same boot, our own pid");

    let (result, code) = lock_command(
        &fixture.path,
        "a",
        &store.tasks[0],
        "release",
        "wrong",
        "",
        at(),
    )
    .expect("release");
    assert_eq!(code, 1);
    assert_eq!(result["lockState"], json!("release-mismatch"));

    let (result, code) = lock_command(
        &fixture.path,
        "a",
        &store.tasks[0],
        "release",
        "r1",
        "",
        at(),
    )
    .expect("release");
    assert_eq!(code, 0);
    assert_eq!(result["lockState"], json!("released"));

    let (result, code) = lock_command(
        &fixture.path,
        "a",
        &store.tasks[0],
        "release",
        "r1",
        "",
        at(),
    )
    .expect("release");
    assert_eq!(code, 0);
    assert_eq!(result["lockState"], json!("free"));
}

#[test]
fn the_lock_command_reclaims_a_stale_lock() {
    let mut task = base_task("a");
    task["lockTimeoutSec"] = json!(0);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    let locks = fixture.path.parent().unwrap().join("locks");
    std::fs::create_dir_all(&locks).expect("mkdir");
    std::fs::write(
        lock_path(&fixture.path, "a"),
        serde_json::to_string(&json!({
            "taskId": "a",
            "runId": "ghost",
            "pid": 1,
            "bootId": OTHER_BOOT,
            "acquiredAt": "2020-01-01T00:00:00Z",
        }))
        .expect("serializable"),
    )
    .expect("write lock");

    let (result, code) = lock_command(
        &fixture.path,
        "a",
        &store.tasks[0],
        "acquire",
        "r1",
        AT,
        at(),
    )
    .expect("acquire");
    assert_eq!(code, 0);
    assert_eq!(result["lockState"], json!("acquired"));
    assert_eq!(result["reclaimedStale"], json!(true));
    let asides: Vec<String> = std::fs::read_dir(&locks)
        .expect("dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".stale."))
        .collect();
    assert_eq!(asides.len(), 1);
}

#[test]
fn the_store_flock_serializes_writers() {
    let fixture = fixture(json!({ "version": 1, "tasks": [base_task("a")] }));

    let guard = store_flock(&fixture.path).expect("flock");
    // A second writer's non-blocking attempt conflicts while we hold it.
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(store_lock_path(&fixture.path))
        .expect("open");
    assert!(
        fs2::FileExt::try_lock_exclusive(&probe).is_err(),
        "the held flock excludes other writers"
    );
    drop(guard);

    // After release, the next writer acquires cleanly.
    let guard = store_flock(&fixture.path).expect("flock");
    drop(guard);
}

#[test]
fn run_limit_metadata_shapes_the_due_view() {
    let mut task = base_task("a");
    task["maxRuns"] = json!(5);
    task["runCount"] = json!(4);
    let fixture = fixture(json!({ "version": 1, "tasks": [task] }));
    let store = store::load(&fixture.path).expect("valid store");
    let metadata = run_limit_metadata(&store.tasks[0]);
    assert_eq!(metadata["remainingRuns"], json!(1));
    assert_eq!(metadata["reached"], json!(false));
}
