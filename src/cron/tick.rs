//! `danso cron tick` — the timer-facing scheduler entry (§6.5). `--dry-run`
//! is the read-only plan; the execute pipeline runs the would-run actions up
//! to `--max-runs` through the `run::execute` pipeline (lock → run → spool →
//! state commit → release), like ccc `scheduler_execute`.

use crate::cron::due::{due_plan, read_only_mutations};
use crate::cron::store::Store;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::Path;

/// ccc `scheduler_actions`: the per-task decision derived from a due plan.
/// `enabled` is the plan row's effective enabled flag (configured enabled and
/// not run-limit reached).
pub fn actions(plan: &Value) -> Vec<Value> {
    let mut actions = Vec::new();
    for row in plan["tasks"].as_array().cloned().unwrap_or_default() {
        let status = row["status"].as_str().unwrap_or_default().to_string();
        let lock_state = row["lockState"].as_str().unwrap_or_default().to_string();
        let enabled = row["enabled"].as_bool().unwrap_or(false);
        let due = row["due"].as_bool().unwrap_or(false);
        let mut item = json!({
            "taskId": row.get("id").cloned().unwrap_or(Value::Null),
            "action": "skip",
            "reason": Value::Null,
            "status": row.get("status").cloned().unwrap_or(Value::Null),
            "scheduledAt": row.get("scheduledAt").cloned().unwrap_or(Value::Null),
            "dueCount": row.get("dueCount").cloned().unwrap_or(json!(0)),
            "missedRuns": row.get("missedRuns").cloned().unwrap_or(json!(0)),
            "retryEligibleAt": row.get("retryEligibleAt").cloned().unwrap_or(Value::Null),
            "retryAttempt": row.get("retryAttempt").cloned().unwrap_or(Value::Null),
            "lockState": row.get("lockState").cloned().unwrap_or(Value::Null),
            "lockPath": row.get("lockPath").cloned().unwrap_or(Value::Null),
        });
        let reason = if !enabled {
            if status == "run-limit-reached" {
                "run-limit-reached"
            } else {
                "disabled"
            }
        } else if due && (lock_state == "held" || lock_state == "persist-failed") {
            if lock_state == "persist-failed" {
                "persist-failed"
            } else {
                "locked"
            }
        } else if due {
            item["action"] = json!("would-run");
            if status.is_empty() {
                "due"
            } else {
                status.as_str()
            }
        } else if status == "retry-wait" {
            "retry-wait"
        } else if status == "retry-exhausted" {
            "retry-exhausted"
        } else {
            "not-due"
        };
        item["reason"] = json!(reason);
        actions.push(item);
    }
    actions
}

/// The `--dry-run` plan: what a tick would run, touching nothing.
pub fn dry_run(
    store: &Store,
    store_display: &str,
    at_raw: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    let plan = due_plan(store, store_display, at_raw, now);
    json!({
        "ok": plan["ok"].as_bool().unwrap_or(false),
        "mode": "tick-dry-run-read-only",
        "store": store_display,
        "at": plan["at"],
        "actions": actions(&plan),
        "errors": plan["errors"].clone(),
        "mutations": read_only_mutations(),
    })
}

/// The execute pipeline (ccc `scheduler_execute`): run the would-run actions
/// up to `max_runs`, handing each run its already-computed plan row and the
/// plan instant so `run::execute` skips the per-task replan. Per-run results
/// are aggregated into the top-level mutation flags; like the reference, the
/// tick itself exits 0 once it executed (per-run failures are visible in each
/// result and its own exit semantics).
pub fn execute(
    store_path: &Path,
    store: &Store,
    store_display: &str,
    at_raw: Option<&str>,
    now: DateTime<Utc>,
    max_runs: usize,
) -> (Value, i32) {
    let plan = due_plan(store, store_display, at_raw, now);
    let actions = actions(&plan);
    let runnable: Vec<&Value> = actions
        .iter()
        .filter(|action| {
            action["action"].as_str() == Some("would-run")
                && action.get("taskId").and_then(Value::as_str).is_some()
        })
        .collect();
    let selected: Vec<&&Value> = runnable.iter().take(max_runs).collect();
    let at = crate::cron::time::parse_utc(Some(plan["at"].as_str().unwrap_or_default()), "plan at")
        .ok()
        .flatten()
        .unwrap_or_else(|| crate::cron::time::truncate_minute(now));
    let rows = plan["tasks"].as_array().cloned().unwrap_or_default();
    let mut rows_by_id: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
    for row in &rows {
        if let Some(id) = row["id"].as_str() {
            rows_by_id.entry(id).or_insert(row);
        }
    }
    let mut results = Vec::new();
    let mut any_lock = false;
    let mut any_store_write = false;
    let mut any_history = false;
    let mut any_spool = false;
    let mut any_execute = false;
    for action in selected {
        let Some(task_id) = action["taskId"].as_str() else {
            continue;
        };
        let empty_row = Value::Null;
        let row = rows_by_id.get(task_id).copied().unwrap_or(&empty_row);
        let (result, _code) = crate::cron::run::execute(
            store_path,
            store,
            store_display,
            task_id,
            None,
            now,
            Some((row, at)),
        );
        if let Value::Object(fields) = &result["mutations"] {
            any_lock = any_lock
                || fields
                    .get("lockAcquire")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            any_store_write = any_store_write
                || fields
                    .get("taskStoreWrite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            any_history = any_history
                || fields
                    .get("historyAppend")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            any_spool = any_spool
                || fields
                    .get("spoolWrite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            any_execute = any_execute
                || fields
                    .get("execute")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
        }
        results.push(result);
    }
    let result = json!({
        "ok": true,
        "mode": "tick-execute",
        "store": store_display,
        "at": plan["at"],
        "plannedActions": actions.len(),
        "runnableActions": runnable.len(),
        "executedActions": results.len(),
        "maxRuns": max_runs,
        "truncated": runnable.len() > results.len(),
        "results": results,
        "errors": plan["errors"].clone(),
        "mutations": {
            "lockAcquire": any_lock,
            "taskStoreWrite": any_store_write,
            "historyAppend": any_history,
            "spoolWrite": any_spool,
            "execute": any_execute,
        },
    });
    (result, 0)
}
