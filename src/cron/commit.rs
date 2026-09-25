//! The state-commit write path (§6.5 "상태 커밋", §7 layout): atomic store
//! replacement with the `.bak` snapshot, `runHistory` append with the
//! `cron/history/<id>.jsonl` overflow archive, retry transitions, run-limit
//! application, and the projected run-state commit under the store flock.
//!
//! This is a port of ccc `agent_cron.py` (`write_doc`, `append_run_history`,
//! `history_attempt`, `apply_run_limit`) plus `agent_cron_lib.py`
//! (`apply_retry_transition`). Two danso additions come straight from the §7
//! state layout and are documented as deliberate divergences: the `.bak`
//! snapshot before every store replace, and archiving evicted history entries
//! instead of dropping them. Execution wiring (who calls these) lands with the
//! payload executors in #120 PR3.

use crate::cron::retry::{RetryPolicy, retry_delay};
use crate::cron::store::{RunHistoryItem, Store, Task, atomic_write_bytes, history_dir};
use crate::cron::time::{fmt_dt, truncate_minute};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::Path;

/// Fields a run owns (ccc `RUN_STATE_FIELDS`). Everything else on the task
/// (schedule, prompt, enabled, payload) belongs to whoever edits it and must
/// survive a concurrent run.
const RUN_STATE_FIELDS: [&str; 6] = [
    "runHistory",
    "retryState",
    "lastRunAt",
    "lastStatus",
    "lastRunId",
    "runCount",
];

/// Serialize a store document exactly like ccc `write_doc`:
/// `json.dumps(..., indent=2)` plus a trailing newline.
fn store_bytes(document: &Value) -> Result<Vec<u8>, String> {
    let mut text = serde_json::to_string_pretty(document)
        .map_err(|error| format!("cannot serialize store: {error}"))?;
    text.push('\n');
    Ok(text.into_bytes())
}

/// Validate a raw store document with the same fail-closed rules as
/// `store::load` (structural + semantic), used before any write.
fn validate_document(document: &Value) -> Result<(), String> {
    let store: Store = serde_json::from_value(document.clone())
        .map_err(|error| format!("refusing invalid agent-cron store: {error}"))?;
    let errors = crate::cron::store::validate(&store);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "refusing invalid agent-cron store: {}",
            errors.join("; ")
        ))
    }
}

/// Atomically replace the task store after validation, snapshotting the
/// previous content to `<store>.bak` first (§7 `tasks.json(+.bak)`; ccc does
/// not snapshot — documented divergence). Both writes are atomic 0600.
pub fn write_store_document(store_path: &Path, document: &Value) -> Result<(), String> {
    validate_document(document)?;
    let bytes = store_bytes(document)?;
    if let Ok(previous) = std::fs::read(store_path) {
        let file_name = store_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let backup = store_path.with_file_name(format!("{file_name}.bak"));
        atomic_write_bytes(&backup, &previous, 0o600)?;
    }
    atomic_write_bytes(store_path, &bytes, 0o600)
}

/// ccc `history_attempt`: the attempt number for a new history entry — one
/// past the highest attempt already recorded for this exact occurrence
/// (retryState and matching history rows).
pub fn history_attempt(task: &Task, scheduled_at: &str) -> i64 {
    let mut attempts = 0;
    if let Some(state) = &task.retry_state
        && state.scheduled_at == scheduled_at
    {
        attempts = attempts.max(state.attempt);
    }
    for item in &task.run_history {
        if item.scheduled_at == scheduled_at {
            attempts = attempts.max(item.attempt);
        }
    }
    attempts + 1
}

/// Append one history entry and enforce the `maxRunHistory` cap. Returns the
/// evicted (oldest) entries in chronological order so the caller can archive
/// them to `cron/history/<id>.jsonl` (§7 overflow; ccc drops them).
pub fn append_run_history(task: &mut Task, entry: RunHistoryItem) -> Vec<RunHistoryItem> {
    task.run_history.push(entry);
    // ccc: a non-positive cap means 20, anything above 500 clamps to 500.
    let max = if task.max_run_history < 1 {
        20
    } else {
        task.max_run_history.min(500)
    };
    if task.run_history.len() <= max as usize {
        return Vec::new();
    }
    let keep_from = task.run_history.len() - max as usize;
    let evicted: Vec<RunHistoryItem> = task.run_history.drain(..keep_from).collect();
    evicted
}

/// Archive evicted history entries as JSONL, one line each (§7
/// `cron/history/`). Appends so successive runs extend the same file.
pub fn archive_history(
    store_path: &Path,
    task_id: &str,
    evicted: &[RunHistoryItem],
) -> Result<(), String> {
    if evicted.is_empty() {
        return Ok(());
    }
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = history_dir(store_path);
    std::fs::create_dir_all(&dir).map_err(|error| format!("cannot create history dir: {error}"))?;
    let mut lines = String::new();
    for item in evicted {
        let line = serde_json::to_string(item)
            .map_err(|error| format!("cannot serialize history entry: {error}"))?;
        lines.push_str(&line);
        lines.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(format!("{task_id}.jsonl")))
        .map_err(|error| format!("cannot open history archive: {error}"))?;
    file.write_all(lines.as_bytes())
        .map_err(|error| format!("cannot write history archive: {error}"))
}

/// ccc `run_limit_metadata` (same shape as the due-plan `runLimit` object).
pub fn run_limit_metadata(task: &Task) -> Value {
    let maximum = task.max_runs;
    let count = task.run_count;
    json!({
        "maxRuns": maximum,
        "runCount": count,
        "remainingRuns": maximum.map(|maximum| (maximum - count).max(0)),
        "reached": maximum.is_some_and(|maximum| count >= maximum),
    })
}

/// ccc `apply_run_limit`: count the run, and when the limit is now reached
/// disable the task and drop any pending retry state. A run may only ever
/// disable a task, never re-enable one.
pub fn apply_run_limit(task: &mut Task) -> Value {
    if task.max_runs.is_none() {
        return run_limit_metadata(task);
    }
    task.run_count += 1;
    let metadata = run_limit_metadata(task);
    if metadata["reached"].as_bool().unwrap_or(false) {
        task.enabled = false;
        task.retry_state = None;
    }
    metadata
}

fn transition(cleared: bool, attempt: i64, extra: Value) -> Value {
    let mut object = json!({
        "cleared": cleared,
        "attempt": attempt,
        "retryEligibleAt": Value::Null,
        "exhausted": false,
    });
    if let (Value::Object(base), Value::Object(fields)) = (&mut object, extra) {
        base.extend(fields);
    }
    object
}

fn policy_value(policy: &RetryPolicy) -> Value {
    json!({
        "maxAttempts": policy.max_attempts,
        "backoffSec": policy.backoff_sec,
        "backoffMultiplier": policy.multiplier,
        "maxBackoffSec": policy.max_backoff_sec,
    })
}

/// ccc `apply_retry_transition`: fold one finished run into the task's retry
/// state. Success clears; a task with no declared retry policy has no retry
/// concept and must never be labelled retry-exhausted (a task that explicitly
/// declares a policy, even `{maxAttempts: 1}`, opted into the framework).
pub fn apply_retry_transition(
    task: &mut Task,
    scheduled_at: &str,
    attempt: i64,
    run_id: &str,
    status: &str,
    at: DateTime<Utc>,
) -> Value {
    let clear = |task: &mut Task| {
        let existed = task.retry_state.is_some();
        task.retry_state = None;
        existed
    };
    if status == "success" {
        let existed = clear(task);
        return transition(existed, attempt, json!({}));
    }
    let declared = match &task.retry_policy {
        // An empty policy object counts as undeclared (ccc truthiness).
        Some(policy) => {
            policy.max_attempts.is_some()
                || policy.backoff_sec.is_some()
                || policy.multiplier.is_some()
                || policy.max_backoff_sec.is_some()
        }
        None => false,
    };
    if !declared {
        let existed = clear(task);
        return transition(existed, attempt, json!({ "noPolicy": true }));
    }
    let policy = RetryPolicy::from_spec(task.retry_policy.as_ref());
    if attempt >= policy.max_attempts {
        task.retry_state = Some(crate::cron::store::RetryState {
            scheduled_at: scheduled_at.to_string(),
            attempt,
            retry_eligible_at: None,
            last_status: "exhausted".to_string(),
            last_run_id: Some(run_id.to_string()),
        });
        return json!({
            "cleared": false,
            "attempt": attempt,
            "retryEligibleAt": Value::Null,
            "exhausted": true,
            "policy": policy_value(&policy),
        });
    }
    let delay = retry_delay(&policy, attempt);
    let eligible = truncate_minute(at + chrono::Duration::seconds(delay));
    task.retry_state = Some(crate::cron::store::RetryState {
        scheduled_at: scheduled_at.to_string(),
        attempt,
        retry_eligible_at: fmt_dt(Some(eligible)),
        last_status: status.to_string(),
        last_run_id: Some(run_id.to_string()),
    });
    json!({
        "cleared": false,
        "attempt": attempt,
        "retryEligibleAt": fmt_dt(Some(eligible)),
        "exhausted": false,
        "policy": policy_value(&policy),
    })
}

/// ccc `commit_run_state`: persist one run's outcome without clobbering
/// concurrent edits.
///
/// The scheduler executes headless work between reading the store and writing
/// it back — often for minutes — so it cannot hold the store flock across that
/// window. Instead it re-reads under the flock at write time and projects only
/// [`RUN_STATE_FIELDS`] onto the fresh task, leaving any edit made meanwhile
/// intact. Returns `Ok(false)` when the task was removed while the run was in
/// flight (a missing store file counts as "everything was removed", like ccc
/// `load_doc`). A store load/validation failure returns `Err` so the caller
/// quarantines the completed occurrence instead of releasing it for duplicate
/// execution.
///
/// `run_task` is the full task as the run edited it, as a JSON object; absent
/// run-state fields are removed from the persisted task (never left behind as
/// nulls, matching the Python dict projection). `disable` may only clear
/// `enabled` (one-shot completion, run limit) — it can never set it.
pub fn commit_run_state(
    store_path: &Path,
    task_id: &str,
    run_task: &Value,
    disable: bool,
) -> Result<bool, String> {
    let _guard = crate::cron::locks::store_flock(store_path)?;
    let fresh_text = match std::fs::read_to_string(store_path) {
        Ok(text) => text,
        // A missing store means the task is gone; report it like ccc's
        // load_doc (empty store → task not found → Ok(false)).
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "task store load failed during run-state commit: {}",
                short_text(&error.to_string(), 2000)
            ));
        }
    };
    let mut document: Value = serde_json::from_str(&fresh_text).map_err(|error| {
        format!(
            "task store load failed during run-state commit: {}",
            short_text(&error.to_string(), 2000)
        )
    })?;
    validate_document(&document).map_err(|error| {
        format!(
            "task store load failed during run-state commit: {}",
            short_text(&error, 2000)
        )
    })?;
    if document["tasks"].is_null() {
        document["tasks"] = json!([]);
    }
    let Some(tasks) = document["tasks"].as_array_mut() else {
        return Err(
            "task store load failed during run-state commit: root is not an object".to_string(),
        );
    };
    let Some(target) = tasks
        .iter_mut()
        .find(|task| task["id"].as_str() == Some(task_id))
    else {
        return Ok(false);
    };
    for field in RUN_STATE_FIELDS {
        match run_task.get(field) {
            Some(value) => target[field] = value.clone(),
            None => {
                if let Value::Object(fields) = target {
                    fields.remove(field);
                }
            }
        }
    }
    if disable {
        target["enabled"] = json!(false);
    }
    write_store_document(store_path, &document)?;
    Ok(true)
}

/// ccc `short_text`: clip an error string for reporting.
pub fn short_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut clipped = text.chars().take(limit).collect::<String>();
    clipped.push_str(&format!("\n[truncated {} chars]", text.len() - limit));
    clipped
}
