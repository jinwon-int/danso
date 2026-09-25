//! Read-only due planning — a port of ccc `agent_cron.py` `lock_status` and
//! `due_plan` (§6.5). Nothing here acquires locks, writes, or executes.

use crate::cron::commit::run_limit_metadata;
use crate::cron::locks::read_lock;
use crate::cron::retry::{RetryPolicy, retry_view};
use crate::cron::schedule::{
    OCCURRENCE_SCAN_LIMIT, Schedule, next_after, parse_schedule, schedule_occurrences,
};
use crate::cron::store::{Store, Task, locks_dir};
use crate::cron::time::{boot_id, fmt_dt, parse_utc};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::path::Path;

/// Python truthiness for the lock fields consulted here (`error`,
/// `bootId`): missing/null/zero/empty are all false.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// True only when the recorded holder pid provably no longer exists
/// (observability only — see the lock contract above).
fn holder_process_gone(pid: i64) -> bool {
    // The lock contract makes bootId change and the opt-in lockTimeoutSec the
    // only staleness sources: a wrong liveness guess must never double-run a
    // task. This surfaces a provably dead same-boot holder only, by the one
    // liveness rule this binary has (`danso service status` uses the same):
    // ESRCH and a zombie are gone, EPERM is alive.
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    !danso_ops::probe::pid_alive(pid as u32)
}

/// Lock state for one task at `at` (ccc `lock_status`).
pub fn lock_status(task_id: &str, task: &Task, at: DateTime<Utc>, locks: &Path) -> Value {
    let lock_path = locks.join(format!("{task_id}.lock"));
    let mut base = json!({
        "lockPath": lock_path.display().to_string(),
        "lockState": "free",
    });
    let lock = read_lock(&lock_path);
    if lock.is_null() {
        return base;
    }
    let string_field = |name: &str| lock.get(name).and_then(Value::as_str);
    let timeout = task.lock_timeout_sec;
    let age = match string_field("acquiredAt") {
        None => None,
        Some(raw) => parse_utc(Some(raw), "acquiredAt")
            .ok()
            .flatten()
            .map(|acquired| (at - acquired).num_seconds().max(0)),
    };
    let current_boot = boot_id();
    let lock_boot = string_field("bootId").unwrap_or_default();
    let quarantined = lock.get("state").and_then(Value::as_str) == Some("persist-failed");
    let mut stale = is_truthy(lock.get("error"));
    if !stale
        && !quarantined
        && !lock_boot.is_empty()
        && !current_boot.is_empty()
        && lock_boot != current_boot
    {
        stale = true;
    }
    if !stale && !quarantined && timeout > 0 && age.is_some_and(|age| age > timeout) {
        stale = true;
    }
    let holder_alive =
        if !lock_boot.is_empty() && !current_boot.is_empty() && lock_boot == current_boot {
            let pid = lock.get("pid").and_then(Value::as_i64);
            // A non-integer (or boolean, or missing) pid is not evidence the
            // holder is gone, so the holder counts as alive — same as Python.
            let gone = pid.is_some_and(holder_process_gone);
            Some(!gone)
        } else {
            None
        };
    let state = if stale {
        "stale"
    } else if quarantined {
        "persist-failed"
    } else {
        "held"
    };
    base["lockState"] = json!(state);
    base["holder"] = lock;
    base["lockAgeSec"] = json!(age);
    base["lockTimeoutSec"] = json!(timeout);
    if let Some(holder_alive) = holder_alive {
        base["holderAlive"] = json!(holder_alive);
    }
    base
}

fn status_from_lock(lock: &Value, current: &str) -> String {
    match lock["lockState"].as_str() {
        Some("persist-failed") => "persist-failed".to_string(),
        Some("held") => "locked".to_string(),
        Some("stale") => "stale-lock".to_string(),
        _ => current.to_string(),
    }
}

/// One due-plan row. Returns the row plus the error message on any
/// schedule/field parse failure (row carries `status: invalid-schedule`).
fn due_row(task: &Task, index: usize, at: DateTime<Utc>, locks: &Path) -> (Value, Option<String>) {
    let task_id = task.id.as_str();
    let lock = lock_status(task_id, task, at, locks);
    let run_limit = run_limit_metadata(task);
    let configured_enabled = task.enabled;
    let effective_enabled = configured_enabled && !run_limit["reached"].as_bool().unwrap_or(false);
    let mut row = json!({
        "id": task.id,
        "enabled": effective_enabled,
        "configuredEnabled": configured_enabled,
        "schedule": task.schedule,
        "timezone": task.timezone,
        "catchUpPolicy": task.catch_up_policy.label(),
        "maxCatchup": task.max_catchup,
        "lastRunAt": task.last_run_at,
        "notBefore": task.not_before,
        "maxRuns": task.max_runs,
        "runCount": task.run_count,
        "retryEligibleAt": Value::Null,
        "retryAttempt": Value::Null,
        "due": false,
        "dueCount": 0,
        "missedRuns": 0,
        "missedRunsTruncated": false,
        "occurrenceScanLimit": OCCURRENCE_SCAN_LIMIT,
        "scheduledAt": Value::Null,
        "nextDueAt": Value::Null,
        "lockPath": lock["lockPath"],
        "lockState": lock["lockState"],
        "runLimit": run_limit,
        "status": if run_limit["reached"].as_bool().unwrap_or(false) {
            "run-limit-reached"
        } else if !configured_enabled {
            "disabled"
        } else {
            "idle"
        },
    });
    for key in ["holder", "lockAgeSec", "lockTimeoutSec"] {
        if let Some(value) = lock.get(key) {
            row[key] = value.clone();
        }
    }
    if let Some(value) = lock.get("holderAlive") {
        row["holderAlive"] = value.clone();
    }
    // Everything below fails closed per task: a parse failure marks exactly
    // this row invalid-schedule and reports the reason in plan errors.
    let planned = (|| -> Result<(), String> {
        let spec = parse_schedule(&task.schedule, &task.timezone)?;
        row["scheduleKind"] = json!(spec.kind());
        let last = parse_utc(
            task.last_run_at.as_deref(),
            &format!("tasks[{index}].lastRunAt"),
        )?;
        let not_before = parse_utc(
            task.not_before.as_deref(),
            &format!("tasks[{index}].notBefore"),
        )?;
        let anchor = parse_utc(
            task.anchor_at.as_deref(),
            &format!("tasks[{index}].anchorAt"),
        )?;
        if let Some(not_before) = not_before.filter(|not_before| at < *not_before) {
            if effective_enabled {
                row["status"] = json!("not-before");
            }
            row["nextDueAt"] = if matches!(spec, Schedule::Interval { .. }) && anchor.is_none() {
                json!(fmt_dt(Some(not_before)))
            } else {
                json!(fmt_dt(next_after(
                    &spec,
                    not_before - Duration::seconds(1),
                    anchor
                )))
            };
            return Ok(());
        }
        let mut occurrence_floor = last;
        let free_interval =
            matches!(spec, Schedule::Interval { .. }) && anchor.is_none() && last.is_none();
        if let (Some(not_before), false) = (not_before, free_interval) {
            let activation_floor = not_before - Duration::seconds(1);
            if occurrence_floor.is_none_or(|floor| floor < activation_floor) {
                occurrence_floor = Some(activation_floor);
            }
        }
        let (occurrences, truncated) =
            schedule_occurrences(&spec, occurrence_floor, at, anchor, OCCURRENCE_SCAN_LIMIT);
        let raw_missed = occurrences.len();
        row["missedRunsTruncated"] = json!(truncated);
        if effective_enabled && raw_missed > 0 {
            row["due"] = json!(true);
            row["scheduledAt"] = json!(fmt_dt(occurrences.last().copied()));
            let due_count = if task.catch_up_policy == crate::cron::store::CatchUpPolicy::All {
                raw_missed.min(task.max_catchup.max(0) as usize)
            } else {
                1
            };
            row["dueCount"] = json!(due_count);
            row["missedRuns"] = json!(raw_missed.saturating_sub(due_count));
            row["status"] = json!(status_from_lock(&lock, "due"));
        }
        let policy = RetryPolicy::from_spec(task.retry_policy.as_ref());
        if let Some(view) = retry_view(task.retry_state.as_ref(), &policy, at) {
            row["retryEligibleAt"] = json!(view.retry_eligible_at);
            row["retryAttempt"] = json!(view.retry_attempt);
            if let Some(error) = view.error {
                row["retryError"] = json!(error);
            } else if effective_enabled && !row["due"].as_bool().unwrap_or(false) {
                if view.ready {
                    row["due"] = json!(true);
                    row["dueCount"] = json!(1);
                    row["scheduledAt"] = json!(view.scheduled_at);
                    row["status"] = json!(status_from_lock(&lock, "retry-due"));
                } else if view.waiting {
                    row["status"] = json!("retry-wait");
                } else if view.exhausted {
                    row["status"] = json!("retry-exhausted");
                }
            }
        }
        row["nextDueAt"] = json!(fmt_dt(next_after(&spec, at, anchor)));
        Ok(())
    })();
    match planned {
        Ok(()) => (row, None),
        Err(error) => {
            row["status"] = json!("invalid-schedule");
            row["error"] = json!(error);
            (row, Some(format!("{}: {error}", task.id)))
        }
    }
}

/// The full due plan (ccc `due_plan`), including the §6.5 `mutations` object
/// proving read-onlyness.
pub fn due_plan(
    store: &Store,
    store_display: &str,
    at_raw: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    let at = match at_raw {
        None => Some(crate::cron::time::truncate_minute(now)),
        Some(raw) => match parse_utc(Some(raw), "--at") {
            Ok(parsed) => parsed,
            Err(error) => {
                return json!({
                    "ok": false,
                    "store": store_display,
                    "at": raw,
                    "mode": "dry-run-read-only",
                    "mutations": read_only_mutations(),
                    "tasks": [],
                    "errors": [error],
                });
            }
        },
    };
    let at = at.unwrap_or(now);
    let locks = locks_dir(Path::new(store_display));
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (index, task) in store.tasks.iter().enumerate() {
        let (row, error) = due_row(task, index, at, &locks);
        rows.push(row);
        if let Some(error) = error {
            errors.push(error);
        }
    }
    json!({
        "ok": errors.is_empty(),
        "store": store_display,
        "at": fmt_dt(Some(at)),
        "mode": "dry-run-read-only",
        "mutations": read_only_mutations(),
        "tasks": rows,
        "errors": errors,
    })
}

/// §6.5: every read-only command carries this object in its result JSON —
/// typed proof that it acquires no lock, writes no store, appends no
/// history, writes no spool, and executes nothing.
pub fn read_only_mutations() -> Value {
    json!({
        "lockAcquire": false,
        "taskStoreWrite": false,
        "historyAppend": false,
        "spoolWrite": false,
        "execute": false,
    })
}

/// ccc `agent_cron_model.normalize`: the stable, prompt-free listing
/// projection for one task.
pub fn normalize(task: &Task) -> Value {
    json!({
        "id": task.id,
        "name": task.name.clone().unwrap_or_else(|| task.id.clone()),
        "schedule": task.schedule,
        "enabled": task.enabled,
        "notify": task.notify.label(),
        "allowedTools": task.allowed_tools,
        "permissionMode": match task.permission_mode {
            crate::cron::store::PermissionMode::Default => "default",
            crate::cron::store::PermissionMode::DontAsk => "dontAsk",
            crate::cron::store::PermissionMode::AcceptEdits => "acceptEdits",
        },
        "attachMemory": task.attach_memory,
        "attachSkills": task.attach_skills,
        "timezone": task.timezone,
        "catchUpPolicy": task.catch_up_policy.label(),
        "maxCatchup": task.max_catchup,
        "lockTimeoutSec": task.lock_timeout_sec,
        "maxRunHistory": task.max_run_history,
        "notBefore": task.not_before,
        "maxRuns": task.max_runs,
        "runCount": task.run_count,
        "redactProfile": task.redact_profile,
        // `Payload` carries no prompt text (kind/argv/cwd/model/limits), so
        // the projection can show what a command task will execute.
        "payload": task.payload,
        "lastRunAt": task.last_run_at,
        "lastStatus": task.last_status,
        "lastRunId": task.last_run_id,
        "runHistoryCount": task.run_history.len(),
        "retryPolicy": task.retry_policy,
        "retryState": task.retry_state,
    })
}
