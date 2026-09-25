//! Store validation, due-planning statuses, and the read-only CLI contract
//! for `danso cron` (#120 PR1, §6.5).
//!
//! The due/lock semantics mirror ccc `agent_cron.py`; schedule matching
//! itself is golden-tested against the Python reference in `cron_schedule.rs`.

// The cron surface is gated on `ops` (src/lib.rs); a CLI-only build has
// neither the module nor the binary subcommand these tests drive.
#![cfg(feature = "ops")]

use chrono::{DateTime, Utc};
use danso::cron::due::due_plan;
use danso::cron::store::{self, store_path};
use danso::cron::time::{boot_id, parse_utc};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const AT: &str = "2026-09-25T09:30:00Z";

fn utc(raw: &str) -> DateTime<Utc> {
    danso::cron::time::parse_utc(Some(raw), "test")
        .expect("parse")
        .expect("non-empty")
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

fn locks_dir(fixture: &Fixture) -> PathBuf {
    fixture.path.parent().expect("parent").join("locks")
}

fn write_lock(fixture: &Fixture, task_id: &str, lock: Value) {
    let locks = locks_dir(fixture);
    std::fs::create_dir_all(&locks).expect("mkdir locks");
    std::fs::write(
        locks.join(format!("{task_id}.lock")),
        serde_json::to_string(&lock).expect("serializable"),
    )
    .expect("write lock");
}

fn plan(fixture: &Fixture) -> Value {
    let store = store::load(&fixture.path).expect("valid store");
    due_plan(
        &store,
        &fixture.path.display().to_string(),
        Some(AT),
        utc(AT),
    )
}

fn row(plan: &Value, index: usize) -> Value {
    plan["tasks"].as_array().expect("rows")[index].clone()
}

fn base_task(id: &str) -> Value {
    json!({
        "id": id,
        "schedule": "*/15 9-17 * * 1-5",
        "prompt": "report",
        "enabled": true,
    })
}

#[test]
fn a_missing_store_is_an_empty_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = store_path(dir.path());
    let store = store::load(&path).expect("missing store loads empty");
    assert!(store.tasks.is_empty());
    let plan = due_plan(&store, &path.display().to_string(), Some(AT), utc(AT));
    assert_eq!(plan["ok"], json!(true));
    assert!(plan["tasks"].as_array().expect("rows").is_empty());
}

#[test]
fn the_store_path_sits_under_the_state_root() {
    let home = Path::new("/srv/danso-home");
    assert_eq!(
        store_path(home),
        PathBuf::from("/srv/danso-home/cron/tasks.json")
    );
}

#[test]
fn unknown_fields_fail_closed() {
    let mut task = base_task("unknown-field");
    task["bogusField"] = json!(1);
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let error = store::load(&fixture.path).expect_err("rejected");
    assert!(
        error.contains("bogusField"),
        "error mentions the field: {error}"
    );
}

#[test]
fn duplicate_ids_fail_closed() {
    let fixture = fixture(json!({"version": 1, "tasks": [base_task("dup"), base_task("dup")]}));
    let error = store::load(&fixture.path).expect_err("rejected");
    assert!(error.contains("duplicate task id: dup"), "{error}");
}

#[test]
fn wrong_version_fails_closed() {
    let fixture = fixture(json!({"version": 2, "tasks": []}));
    assert!(store::load(&fixture.path).is_err());
}

#[test]
fn cross_field_payload_rules_fail_closed() {
    for (payload, message) in [
        (
            json!({"kind": "command"}),
            "argv is required for kind 'command'",
        ),
        (
            json!({"kind": "command", "argv": ["/bin/true"], "model": "x"}),
            "model is not allowed for kind 'command'",
        ),
        (
            json!({"kind": "prompt", "argv": ["/bin/true"]}),
            "argv is not allowed for kind 'prompt'",
        ),
        (
            json!({"kind": "prompt", "cwd": "/tmp"}),
            "cwd is not allowed for kind 'prompt'",
        ),
    ] {
        let mut task = base_task("payload-rules");
        task["payload"] = payload;
        let fixture = fixture(json!({"version": 1, "tasks": [task]}));
        let error = store::load(&fixture.path).expect_err("rejected");
        assert!(error.contains(message), "{message} not in {error}");
    }
}

#[test]
fn a_chat_notify_without_a_chat_id_fails_closed() {
    let mut task = base_task("chat-notify");
    task["notify"] = json!("telegram-chat");
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let error = store::load(&fixture.path).expect_err("rejected");
    assert!(error.contains("notifyChatId is required"), "{error}");
}

#[test]
fn an_id_outside_the_documented_pattern_fails_closed() {
    let mut task = base_task("../escape");
    let bad = fixture(json!({"version": 1, "tasks": [task.clone()]}));
    assert!(store::load(&bad.path).is_err());
    task["id"] = json!("ok.id-1_2");
    let good = fixture(json!({"version": 1, "tasks": [task]}));
    assert!(store::load(&good.path).is_ok());
}

#[test]
fn a_full_ccc_style_task_loads() {
    let task = json!({
        "id": "adapter-fleet-watch",
        "name": "Fleet watch",
        "schedule": "*/15 * * * *",
        "prompt": "watch",
        "enabled": true,
        "allowedTools": ["Bash"],
        "permissionMode": "dontAsk",
        "notify": "telegram-owner-on-failure",
        "attachSkills": ["fleet"],
        "redactProfile": "default",
        "timezone": "Asia/Seoul",
        "keepAfterRun": false,
        "payload": {"kind": "command", "argv": ["/bin/echo", "hi"], "cwd": "/tmp",
                     "timeoutSec": 600, "outputMaxBytes": 65536},
        "catchUpPolicy": "once",
        "maxCatchup": 3,
        "lockTimeoutSec": 1800,
        "maxRunHistory": 40,
        "notBefore": "2026-09-25T00:00:00Z",
        "maxRuns": 10,
        "runCount": 2,
        "runHistory": [{"runId": "r1", "scheduledAt": "2026-09-25T08:00:00Z",
                         "startedAt": "2026-09-25T08:00:01Z", "finishedAt": null,
                         "status": "success", "exitCode": 0, "attempt": 1,
                         "notifyState": "none"}],
        "retryPolicy": {"maxAttempts": 3, "backoffSec": 120, "backoffMultiplier": 2,
                         "maxBackoffSec": 1800},
        "lastRunAt": "2026-09-25T08:00:01Z",
        "lastStatus": "success",
        "lastRunId": "r1"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let store = store::load(&fixture.path).expect("loads");
    assert_eq!(store.tasks[0].max_catchup, 3);
    assert_eq!(store.tasks[0].run_history.len(), 1);
}

#[test]
fn a_due_cron_row_carries_the_documented_shape() {
    let mut task = base_task("shape");
    // Reference semantics: `at` itself is a due instant, so with lastRunAt at
    // the previous */15 slot the occurrence at `at` is the one that is due.
    task["lastRunAt"] = json!("2026-09-25T09:15:00Z");
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let plan = plan(&fixture);
    assert_eq!(plan["ok"], json!(true));
    assert_eq!(plan["mode"], json!("dry-run-read-only"));
    assert_eq!(plan["at"], json!(AT));
    // §6.5: read-only commands prove it with the mutations object.
    assert_eq!(
        plan["mutations"],
        json!({"lockAcquire": false, "taskStoreWrite": false, "historyAppend": false,
                "spoolWrite": false, "execute": false})
    );
    let row = row(&plan, 0);
    assert_eq!(row["status"], json!("due"));
    assert_eq!(row["due"], json!(true));
    assert_eq!(row["dueCount"], json!(1));
    assert_eq!(row["missedRuns"], json!(0));
    assert_eq!(row["scheduledAt"], json!("2026-09-25T09:30:00Z"));
    assert_eq!(row["nextDueAt"], json!("2026-09-25T09:45:00Z"));
    assert_eq!(row["scheduleKind"], json!("cron"));
    assert_eq!(row["lockState"], json!("free"));
    assert_eq!(row["occurrenceScanLimit"], json!(1000));
    assert_eq!(row["runLimit"]["reached"], json!(false));
}

#[test]
fn catch_up_policies_shape_due_counts() {
    let mut skip = base_task("skip");
    skip["lastRunAt"] = json!("2026-09-22T00:00:00Z");
    let mut all = base_task("all");
    all["lastRunAt"] = json!("2026-09-22T00:00:00Z");
    all["catchUpPolicy"] = json!("all");
    let mut bounded = base_task("bounded");
    bounded["lastRunAt"] = json!("2026-09-22T00:00:00Z");
    bounded["catchUpPolicy"] = json!("all");
    bounded["maxCatchup"] = json!(3);
    let fixture = fixture(json!({"version": 1, "tasks": [skip, all, bounded]}));
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["dueCount"], json!(1));
    assert_eq!(
        row(&plan, 1)["dueCount"],
        json!(1),
        "maxCatchup default 1 caps 'all'"
    );
    let bounded_row = row(&plan, 2);
    assert_eq!(bounded_row["dueCount"], json!(3));
    assert!(bounded_row["missedRuns"].as_i64().expect("missed") > 0);
}

#[test]
fn disabled_and_run_limited_tasks_are_not_due() {
    let mut disabled = base_task("disabled");
    disabled["enabled"] = json!(false);
    disabled["lastRunAt"] = json!("2026-09-22T00:00:00Z");
    let mut limited = base_task("limited");
    limited["lastRunAt"] = json!("2026-09-22T00:00:00Z");
    limited["maxRuns"] = json!(5);
    limited["runCount"] = json!(5);
    let fixture = fixture(json!({"version": 1, "tasks": [disabled, limited]}));
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["status"], json!("disabled"));
    assert_eq!(row(&plan, 0)["due"], json!(false));
    assert_eq!(row(&plan, 1)["status"], json!("run-limit-reached"));
    assert_eq!(row(&plan, 1)["configuredEnabled"], json!(true));
    assert_eq!(row(&plan, 1)["enabled"], json!(false), "effective");
    assert_eq!(row(&plan, 1)["runLimit"]["remainingRuns"], json!(0));
}

#[test]
fn not_before_gates_the_status_and_next_due() {
    let mut task = base_task("future");
    task["notBefore"] = json!("2026-10-01T09:00:00Z");
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["status"], json!("not-before"));
    assert_eq!(row["nextDueAt"], json!("2026-10-01T09:00:00Z"));
}

#[test]
fn an_interval_task_without_anchor_or_run_is_due_once_immediately() {
    let task = json!({
        "id": "fresh-interval",
        "schedule": "every 1h",
        "prompt": "work",
        "enabled": true
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["status"], json!("due"));
    assert_eq!(row["dueCount"], json!(1));
    assert_eq!(row["scheduleKind"], json!("interval"));
    assert_eq!(row["scheduledAt"], json!(AT));
    assert_eq!(row["nextDueAt"], json!("2026-09-25T10:30:00Z"));
}

#[test]
fn once_schedules_are_due_exactly_when_unrun_and_past() {
    let past = json!({
        "id": "past", "schedule": "at 2026-09-25T08:00:00Z",
        "prompt": "p", "enabled": true, "keepAfterRun": false
    });
    let done = json!({
        "id": "done", "schedule": "at 2026-09-25T08:00:00Z",
        "prompt": "p", "enabled": true, "lastRunAt": "2026-09-25T08:00:00Z"
    });
    let future = json!({
        "id": "future", "schedule": "2026-09-26T09:00",
        "prompt": "p", "enabled": true, "timezone": "Asia/Seoul"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [past, done, future]}));
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["status"], json!("due"));
    assert_eq!(row(&plan, 0)["scheduledAt"], json!("2026-09-25T08:00:00Z"));
    assert_eq!(row(&plan, 1)["status"], json!("idle"));
    assert_eq!(row(&plan, 2)["status"], json!("idle"));
    assert_eq!(row(&plan, 2)["scheduleKind"], json!("once"));
    assert_eq!(row(&plan, 2)["nextDueAt"], json!("2026-09-26T00:00:00Z"));
}

#[test]
fn retry_states_drive_retry_statuses() {
    // ready: eligible <= at, attempt < maxAttempts.
    let ready = json!({
        "id": "ready", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-25T00:00:00Z",
        "retryPolicy": {"maxAttempts": 3},
        "retryState": {"scheduledAt": "2026-09-24T00:00:00Z", "attempt": 1,
                        "retryEligibleAt": "2026-09-25T09:00:00Z", "lastStatus": "failed"}
    });
    // waiting: eligible > at.
    let waiting = json!({
        "id": "waiting", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-25T00:00:00Z",
        "retryPolicy": {"maxAttempts": 3},
        "retryState": {"scheduledAt": "2026-09-24T00:00:00Z", "attempt": 2,
                        "retryEligibleAt": "2026-09-25T23:00:00Z", "lastStatus": "failed"}
    });
    // exhausted: attempt >= maxAttempts.
    let exhausted = json!({
        "id": "exhausted", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-25T00:00:00Z",
        "retryPolicy": {"maxAttempts": 2},
        "retryState": {"scheduledAt": "2026-09-24T00:00:00Z", "attempt": 2,
                        "retryEligibleAt": "2026-09-25T09:00:00Z", "lastStatus": "failed"}
    });
    let fixture = fixture(json!({"version": 1, "tasks": [ready, waiting, exhausted]}));
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["status"], json!("retry-due"));
    assert_eq!(row(&plan, 0)["dueCount"], json!(1));
    assert_eq!(row(&plan, 0)["scheduledAt"], json!("2026-09-24T00:00:00Z"));
    assert_eq!(row(&plan, 0)["retryAttempt"], json!(2));
    assert_eq!(row(&plan, 1)["status"], json!("retry-wait"));
    assert_eq!(row(&plan, 2)["status"], json!("retry-exhausted"));
}

#[test]
fn an_invalid_retry_eligible_at_is_reported_but_not_runnable() {
    let task = json!({
        "id": "bad-eligible", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-25T00:00:00Z",
        "retryPolicy": {"maxAttempts": 3},
        "retryState": {"scheduledAt": "2026-09-24T00:00:00Z", "attempt": 1,
                        "retryEligibleAt": "not-a-timestamp", "lastStatus": "failed"}
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let plan = plan(&fixture);
    let row = row(&plan, 0);
    assert_eq!(row["status"], json!("idle"));
    assert_eq!(row["due"], json!(false));
    assert_eq!(row["retryError"], json!("invalid retryEligibleAt"));
}

#[test]
fn invalid_schedules_fail_closed_per_row() {
    let task = json!({
        "id": "broken", "schedule": "* * * *", "prompt": "p", "enabled": true
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let plan = plan(&fixture);
    assert_eq!(plan["ok"], json!(false));
    assert_eq!(row(&plan, 0)["status"], json!("invalid-schedule"));
    assert!(
        plan["errors"].as_array().expect("errors")[0]
            .as_str()
            .expect("message")
            .starts_with("broken: ")
    );
}

#[test]
fn unknown_timezones_fail_closed_per_row() {
    let task = json!({
        "id": "mars", "schedule": "@daily", "prompt": "p", "enabled": true,
        "timezone": "Mars/Olympus"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["status"], json!("invalid-schedule"));
    assert!(
        row(&plan, 0)["error"]
            .as_str()
            .expect("error")
            .contains("unknown timezone: Mars/Olympus")
    );
}

#[test]
fn lock_states_flow_into_statuses() {
    let held = json!({
        "id": "held", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let stale = json!({
        "id": "stale", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let quarantined = json!({
        "id": "quarantined", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [held, stale, quarantined]}));
    let live_pid = std::process::id() as i64;
    write_lock(
        &fixture,
        "held",
        json!({"acquiredAt": "2026-09-25T09:00:00Z",
        "pid": live_pid, "bootId": boot_id(), "runId": "run-held"}),
    );
    write_lock(
        &fixture,
        "stale",
        json!({"acquiredAt": "2026-09-25T09:00:00Z",
        "pid": live_pid, "bootId": "a-previous-boot", "runId": "run-stale"}),
    );
    write_lock(
        &fixture,
        "quarantined",
        json!({"acquiredAt": "2026-09-25T09:00:00Z",
        "pid": live_pid, "bootId": boot_id(), "runId": "run-q",
        "state": "persist-failed"}),
    );
    let plan = plan(&fixture);
    assert_eq!(row(&plan, 0)["status"], json!("locked"));
    assert_eq!(row(&plan, 0)["lockState"], json!("held"));
    assert_eq!(row(&plan, 0)["lockAgeSec"], json!(1800));
    assert_eq!(row(&plan, 0)["holderAlive"], json!(true));
    assert_eq!(row(&plan, 1)["status"], json!("stale-lock"));
    assert_eq!(row(&plan, 1)["lockState"], json!("stale"));
    assert_eq!(row(&plan, 2)["status"], json!("persist-failed"));
    assert_eq!(row(&plan, 2)["lockState"], json!("persist-failed"));
}

#[test]
fn a_dead_same_boot_holder_is_surfaced_but_never_stolen() {
    let task = json!({
        "id": "dead-holder", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    write_lock(
        &fixture,
        "dead-holder",
        json!({"acquiredAt": "2026-09-25T09:00:00Z",
        "pid": 2147483000i64, "bootId": boot_id(), "runId": "run-dead"}),
    );
    let row = row(&plan(&fixture), 0);
    // Observability only: still `locked` — a wrong liveness guess must not
    // double-run a task. Release is deliberate (`cron lock --release`).
    assert_eq!(row["status"], json!("locked"));
    assert_eq!(row["holderAlive"], json!(false));
}

#[test]
fn lock_timeout_makes_an_old_lock_stale() {
    let task = json!({
        "id": "timed", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z", "lockTimeoutSec": 60
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    write_lock(
        &fixture,
        "timed",
        json!({"acquiredAt": "2026-09-25T08:00:00Z",
        "pid": std::process::id() as i64, "bootId": boot_id(), "runId": "run-old"}),
    );
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["status"], json!("stale-lock"));
    assert_eq!(row["lockTimeoutSec"], json!(60));
}

#[test]
fn a_broken_lock_file_reads_as_stale_not_fatal() {
    let task = json!({
        "id": "corrupt", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let locks = locks_dir(&fixture);
    std::fs::create_dir_all(&locks).expect("mkdir");
    std::fs::write(locks.join("corrupt.lock"), "{not json").expect("write");
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["status"], json!("stale-lock"));
    assert!(
        row["holder"]["error"]
            .as_str()
            .expect("error")
            .contains("invalid lock JSON")
    );
}

/// Timestamps are sliced by byte offset; a multibyte character at one of
/// those offsets must be an ISO8601 error, never a char-boundary panic —
/// from `--at` as well as from store fields the validator does not parse.
#[test]
fn non_ascii_timestamps_fail_closed_without_panicking() {
    for raw in ["2026-01-0é", "2026-01-01T00:00+1é0", "2026-01-01T0é:00"] {
        let error = parse_utc(Some(raw), "field").expect_err(raw);
        assert!(error.contains("field is not valid ISO8601"), "{error}");
    }
    let mut task = base_task("multibyte");
    task["lastRunAt"] = json!("2026-01-0é");
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    let at_plan = due_plan(
        &store::load(&fixture.path).expect("store"),
        fixture.path.to_str().expect("utf8"),
        Some("2026-01-0é"),
        utc("2026-09-25T09:30:00Z"),
    );
    assert_eq!(at_plan["ok"], json!(false));
    assert!(
        at_plan["errors"][0]
            .as_str()
            .expect("error")
            .contains("--at")
    );
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["status"], json!("invalid-schedule"));
    assert!(
        row["error"]
            .as_str()
            .expect("error")
            .contains("lastRunAt is not valid ISO8601")
    );
}

/// Only a missing lock file means free. A lock that exists but cannot be
/// read is an errored holder and the row is `stale-lock`, like invalid
/// JSON — never `free`/`due` for a task that may well be held.
#[test]
fn an_unreadable_lock_file_reads_as_stale_not_free() {
    let task = json!({
        "id": "unreadable", "schedule": "@daily", "prompt": "p", "enabled": true,
        "lastRunAt": "2026-09-22T00:00:00Z"
    });
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    // A directory where the lock file should be: exists, cannot be read as
    // a file, on every platform and as every user.
    std::fs::create_dir_all(locks_dir(&fixture).join("unreadable.lock")).expect("mkdir");
    let row = row(&plan(&fixture), 0);
    assert_eq!(row["lockState"], json!("stale"));
    assert_eq!(row["status"], json!("stale-lock"));
    assert!(
        row["holder"]["error"]
            .as_str()
            .expect("error")
            .starts_with("cannot read lock:")
    );
}

/// ccc's JSON Schema `maxLength` counts characters; so must the port, or a
/// store ccc accepts fails closed here on multibyte text.
#[test]
fn length_limits_count_characters_not_bytes() {
    let mut task = base_task("hangul");
    task["payload"] = json!({"kind": "prompt", "model": "모".repeat(128)});
    task["notifyChatId"] = json!("@abcd");
    let within = fixture(json!({"version": 1, "tasks": [task]}));
    store::load(&within.path).expect("128 characters of model name load");
    let mut task = base_task("hangul-long");
    task["payload"] = json!({"kind": "prompt", "model": "모".repeat(129)});
    let beyond = fixture(json!({"version": 1, "tasks": [task]}));
    let error = store::load(&beyond.path).expect_err("129 characters refused");
    assert!(error.contains("model must be 1-128 characters"), "{error}");
}

// --- CLI contract (real binary): exit codes, JSON shapes, read-onlyness ---

fn cron(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(std::iter::once("cron").chain(args.iter().copied()))
        .output()
        .expect("run danso cron")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_cli_reports_due_plans_and_exit_codes() {
    let mut task = base_task("cli");
    task["lastRunAt"] = json!("2026-09-25T09:00:00Z");
    let good = fixture(json!({"version": 1, "tasks": [task]}));
    let store_text = std::fs::read_to_string(&good.path).expect("read store");

    let output = cron(&[
        "--store",
        good.path.to_str().expect("utf8"),
        "due",
        "--at",
        AT,
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(document["tasks"][0]["status"], json!("due"));
    assert_eq!(document["mutations"]["execute"], json!(false));

    // A broken schedule makes `due` exit 1 after printing the plan.
    let mut broken = base_task("broken");
    broken["schedule"] = json!("* * * *");
    let bad = fixture(json!({"version": 1, "tasks": [broken]}));
    let output = cron(&[
        "--store",
        bad.path.to_str().expect("utf8"),
        "due",
        "--at",
        AT,
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON plan still printed");
    assert_eq!(document["ok"], json!(false));

    // Text mode carries the header the docs show.
    let output = cron(&[
        "--store",
        good.path.to_str().expect("utf8"),
        "due",
        "--at",
        AT,
    ]);
    assert!(stdout(&output).starts_with("# danso cron due plan"));

    // Nothing was written anywhere.
    assert_eq!(
        std::fs::read_to_string(&good.path).expect("reread"),
        store_text
    );
    assert!(!locks_dir(&good).exists(), "no locks dir is created");
}

#[test]
fn the_cli_lists_and_describes_prompt_free() {
    let fixture = fixture(json!({"version": 1, "tasks": [base_task("visible")]}));
    let output = cron(&[
        "--store",
        fixture.path.to_str().expect("utf8"),
        "list",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(document["version"], json!(1));
    assert_eq!(document["mutations"]["taskStoreWrite"], json!(false));
    assert_eq!(document["tasks"][0]["id"], json!("visible"));

    let output = cron(&[
        "--store",
        fixture.path.to_str().expect("utf8"),
        "describe",
        "visible",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(document["id"], json!("visible"));
    assert_eq!(document["scheduleValid"], json!(true));
    assert_eq!(document["mutations"]["execute"], json!(false));
    let rendered = serde_json::to_string(&document).expect("string");
    assert!(
        !rendered.contains("\"prompt\""),
        "describe stays prompt-free: {rendered}"
    );

    let output = cron(&[
        "--store",
        fixture.path.to_str().expect("utf8"),
        "describe",
        "missing",
    ]);
    assert_eq!(output.status.code(), Some(1));
}

/// `describe` shows what a command task executes (payload is prompt-free)
/// and, when the row cannot be planned for a non-schedule reason, why —
/// the same `error` that `due` reports, instead of a bare
/// `invalid-schedule` next to `scheduleValid: true`.
#[test]
fn describe_reports_payload_and_plan_errors() {
    let mut command = base_task("cmd");
    command["payload"] = json!({"kind": "command", "argv": ["/bin/false", "-x"], "cwd": "/tmp"});
    let mut broken = base_task("broken");
    broken["lastRunAt"] = json!("yesterday");
    let fixture = fixture(json!({"version": 1, "tasks": [command, broken]}));
    let store_arg = fixture.path.to_str().expect("utf8");

    let output = cron(&["--store", store_arg, "describe", "cmd", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(document["payload"]["kind"], json!("command"));
    assert_eq!(document["payload"]["argv"], json!(["/bin/false", "-x"]));
    assert_eq!(document["payload"]["cwd"], json!("/tmp"));
    assert!(document.get("error").is_none());
    let output = cron(&["--store", store_arg, "describe", "cmd"]);
    assert!(
        stdout(&output).contains("- payload: "),
        "{}",
        stdout(&output)
    );

    let output = cron(&["--store", store_arg, "describe", "broken", "--json"]);
    let document: Value = serde_json::from_str(&stdout(&output)).expect("JSON");
    assert_eq!(document["scheduleValid"], json!(true));
    assert_eq!(document["status"], json!("invalid-schedule"));
    assert!(
        document["error"]
            .as_str()
            .expect("error")
            .contains("lastRunAt is not valid ISO8601: yesterday")
    );
    let output = cron(&["--store", store_arg, "describe", "broken"]);
    assert!(stdout(&output).contains("- error: "), "{}", stdout(&output));
}

#[test]
fn an_invalid_store_fails_closed_at_the_cli() {
    let mut task = base_task("bad-store");
    task["sneaky"] = json!(true);
    let fixture = fixture(json!({"version": 1, "tasks": [task]}));
    for command in ["list", "due"] {
        let output = cron(&["--store", fixture.path.to_str().expect("utf8"), command]);
        assert_eq!(output.status.code(), Some(1), "{command}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8");
        assert!(stderr.contains("cron:"), "body-free error: {stderr}");
    }
}
