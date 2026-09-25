//! `danso cron run` — one manual run of a task (§6.5). This PR ships the
//! read-only `--dry-run` preview and the execute-mode refusal; the execute
//! pipeline (lock → payload run → history → retry → state commit →
//! release/quarantine → notify) lands with the executors in #120 PR3.

use crate::cron::commit::run_limit_metadata;
use crate::cron::due::{due_plan, read_only_mutations};
use crate::cron::store::{NotifyMode, PayloadKind, PermissionMode, Store, Task};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// ccc `DEFAULT_PROMPT_TIMEOUT_SEC` / `DEFAULT_COMMAND_TIMEOUT_SEC` /
/// `DEFAULT_OUTPUT_MAX_BYTES`.
const DEFAULT_PROMPT_TIMEOUT_SEC: i64 = 3600;
const DEFAULT_COMMAND_TIMEOUT_SEC: i64 = 600;
const DEFAULT_OUTPUT_MAX_BYTES: i64 = 65536;

/// ccc `headless_metadata(task, execute=false)`: the payload preview a dry run
/// reports. The prompt runner is in-process in danso, so its `command` is a
/// placeholder rather than the reference's headless.sh path.
pub fn headless_preview(task: &Task) -> Value {
    let payload = task.payload.as_ref();
    let kind = match payload {
        Some(payload) => match payload.kind {
            PayloadKind::Command => "command",
            PayloadKind::Prompt => "prompt",
        },
        None => "prompt",
    };
    let default_timeout = if kind == "command" {
        DEFAULT_COMMAND_TIMEOUT_SEC
    } else {
        DEFAULT_PROMPT_TIMEOUT_SEC
    };
    let timeout = payload
        .and_then(|payload| payload.timeout_sec)
        .filter(|timeout| *timeout >= 1)
        .unwrap_or(default_timeout);
    let output_max_bytes = payload
        .and_then(|payload| payload.output_max_bytes)
        .filter(|cap| *cap >= 1024)
        .unwrap_or(DEFAULT_OUTPUT_MAX_BYTES);
    let mut meta = json!({
        "payloadKind": kind,
        "timeoutSec": timeout,
        "permissionMode": match task.permission_mode {
            PermissionMode::Default => "default",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::AcceptEdits => "acceptEdits",
        },
        "allowedTools": task.allowed_tools,
        "attachMemory": task.attach_memory,
        "attachSkills": task.attach_skills,
        "execute": false,
    });
    if kind == "command" {
        let argv = payload
            .and_then(|payload| payload.argv.clone())
            .unwrap_or_default();
        meta["command"] = json!(argv.join(" "));
        meta["argvLen"] = json!(argv.len());
        meta["cwd"] = json!(payload.and_then(|payload| payload.cwd.clone()));
        meta["outputMaxBytes"] = json!(output_max_bytes);
    } else {
        meta["command"] = json!("(danso in-process prompt runner)");
        meta["promptBytes"] = json!(task.prompt.len());
        if let Some(model) = payload.and_then(|payload| payload.model.clone()) {
            meta["model"] = json!(model);
        }
    }
    meta
}

/// ccc `run_dry_plan`: the read-only preview of what one run would do. The
/// notification `delivery` parity quirk is intentional: only
/// `telegram-owner` previews as `preview-only`, exactly like the reference.
pub fn dry_plan(
    store: &Store,
    store_display: &str,
    task_id: &str,
    at_raw: Option<&str>,
    now: DateTime<Utc>,
) -> (Value, i32) {
    let Some(task) = store.tasks.iter().find(|task| task.id == task_id) else {
        return (
            json!({
                "ok": false,
                "mode": "run-dry-run-read-only",
                "taskId": task_id,
                "error": "task id not found",
            }),
            1,
        );
    };
    let plan = due_plan(store, store_display, at_raw, now);
    let row = plan["tasks"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(task_id)))
        .cloned()
        .unwrap_or(Value::Null);
    if row.is_null() {
        return (
            json!({
                "ok": false,
                "mode": "run-dry-run-read-only",
                "taskId": task_id,
                "error": "task id not found in due plan",
            }),
            1,
        );
    }
    let result = json!({
        "ok": plan["ok"].as_bool().unwrap_or(false),
        "mode": "run-dry-run-read-only",
        "store": store_display,
        "at": plan["at"],
        "taskId": task_id,
        "due": row["due"],
        "status": row["status"],
        "scheduledAt": row["scheduledAt"],
        "dueCount": row["dueCount"],
        "missedRuns": row["missedRuns"],
        "lock": {
            "state": row["lockState"],
            "path": row["lockPath"],
            "holder": row.get("holder").cloned().unwrap_or(Value::Null),
            "probeOnly": true,
        },
        "headless": headless_preview(task),
        "runLimit": run_limit_metadata(task),
        "notification": {
            "policy": task.notify.label(),
            "delivery": if task.notify == NotifyMode::TelegramOwner {
                "preview-only"
            } else {
                "none"
            },
            "redactProfile": task.redact_profile,
            "send": false,
        },
        "mutations": read_only_mutations(),
        "errors": plan["errors"].clone(),
    });
    let code = if result["ok"].as_bool().unwrap_or(false) {
        0
    } else {
        1
    };
    (result, code)
}

/// The PR2 execute-mode refusal (see `tick::execute_unavailable`).
pub fn execute_unavailable(store_display: &str, task_id: &str) -> Value {
    json!({
        "ok": false,
        "mode": "run-execute-unavailable",
        "store": store_display,
        "taskId": task_id,
        "error": "cron task execution lands in #120 PR3; use --dry-run for the read-only preview",
        "mutations": read_only_mutations(),
    })
}
