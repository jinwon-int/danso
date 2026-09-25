//! `danso cron tick` — the timer-facing scheduler entry (§6.5). This PR ships
//! the read-only dry-run plan (`--dry-run`) and the execute-mode refusal; the
//! execute pipeline itself (lock → run → state commit → release) lands with
//! the payload executors in #120 PR3.

use crate::cron::due::{due_plan, read_only_mutations};
use crate::cron::store::Store;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

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

/// The PR2 execute-mode refusal: a defined, typed "not yet" instead of a
/// half-execution. Nothing is locked, written, or run; #120 PR3 replaces this
/// with the real pipeline.
pub fn execute_unavailable(store_display: &str, at_display: &str, max_runs: i64) -> Value {
    json!({
        "ok": false,
        "mode": "tick-execute-unavailable",
        "store": store_display,
        "at": at_display,
        "maxRuns": max_runs,
        "error": "cron task execution lands in #120 PR3; use --dry-run for the read-only plan",
        "mutations": read_only_mutations(),
    })
}
