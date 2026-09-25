//! `danso cron run` — one manual run of a task (§6.5). The `--dry-run`
//! preview is read-only; the execute pipeline is a port of ccc
//! `agent_cron.py::run_execute` (prechecks → lock → payload run → spool →
//! history → retry → run limit → state commit → release/quarantine).
//!
//! Documented PR divergences (§6.5 mapping): the result mutations use the
//! danso names (`spoolWrite`, `execute`) instead of the ccc ones
//! (`pushSpoolWrite`, `headlessExecute`); a prompt payload whose task
//! declares `allowedTools` or a non-default `permissionMode` is refused
//! fail-closed (`unsupported-tool-policy`) because danso cannot enforce
//! per-tool policies yet — a silent permission downgrade is worse than a
//! refusal.

use crate::cron::commit::{
    append_run_history, apply_retry_transition, apply_run_limit, archive_history, commit_run_state,
    history_attempt, run_limit_metadata,
};
use crate::cron::due::{due_plan, read_only_mutations};
use crate::cron::exec::HeadlessOutcome;
use crate::cron::locks::acquire_for_run;
use crate::cron::store::{NotifyMode, PayloadKind, PermissionMode, RunHistoryItem, Store, Task};
use crate::cron::time::{fmt_dt, parse_utc, truncate_minute};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::path::Path;

/// ccc `DEFAULT_PROMPT_TIMEOUT_SEC` / `DEFAULT_COMMAND_TIMEOUT_SEC`.
pub const DEFAULT_PROMPT_TIMEOUT_SEC: i64 = 3600;
pub const DEFAULT_COMMAND_TIMEOUT_SEC: i64 = 600;

/// ccc `headless_metadata`: the payload view a dry run previews and an
/// executed run reports alongside its outcome.
pub fn headless_metadata(task: &Task, execute: bool) -> Value {
    let payload = task.payload.as_ref();
    let kind = match payload {
        Some(payload) => match payload.kind {
            PayloadKind::Command => "command",
            PayloadKind::Prompt => "prompt",
        },
        None => "prompt",
    };
    let (timeout, output_max_bytes) = crate::cron::exec::payload_limits(task);
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
        "execute": execute,
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
        meta["command"] = json!(format!(
            "danso --print (in-process binary, session under cron/runs/{})",
            task.id
        ));
        meta["promptBytes"] = json!(task.prompt.len());
        if let Some(model) = payload.and_then(|payload| payload.model.clone()) {
            meta["model"] = json!(model);
        }
    }
    meta
}

/// The read-only payload preview (`headless_metadata(task, execute=false)`).
pub fn headless_preview(task: &Task) -> Value {
    headless_metadata(task, false)
}

/// Merge one finished payload run into the metadata view.
fn outcome_document(task: &Task, outcome: &HeadlessOutcome) -> Value {
    let mut meta = headless_metadata(task, true);
    meta["exitCode"] = json!(outcome.exit_code);
    meta["stdout"] = json!(outcome.stdout);
    meta["stderr"] = json!(outcome.stderr);
    if outcome.timed_out {
        meta["timedOut"] = json!(true);
    }
    meta
}

/// The §6.5 mutation proof for one executed run: `taskStoreWrite` and
/// `historyAppend` are true only when the durable record landed; `spoolWrite`
/// when a spool record was written; `execute` when the payload actually ran.
fn executed_mutations(persisted: bool, ran: bool, spooled: bool) -> Value {
    json!({
        "lockAcquire": true,
        "taskStoreWrite": persisted,
        "historyAppend": persisted,
        "spoolWrite": spooled,
        "execute": ran,
    })
}

/// ccc `success_exit_codes`: `[0]` unless the task declares its own set.
fn success_exit_codes(task: &Task) -> Vec<i64> {
    task.success_exit_codes.clone().unwrap_or_else(|| vec![0])
}

/// The `--dry-run` preview of what one run would do. The notification
/// `delivery` parity quirk is intentional: only `telegram-owner` previews as
/// `preview-only`, exactly like the reference.
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

/// The typed zero-mutation refusals that precede any lock or payload work.
/// All carry `ok: true, rc 0` except the tool-policy refusal (a due task that
/// cannot be executed as configured is a configuration error, not a skip).
fn precheck_result(base: &Value, status: &str, extra: Value, code: i32) -> (Value, i32) {
    let mut result = base.clone();
    result["ok"] = json!(true);
    result["status"] = json!(status);
    if let Value::Object(fields) = extra {
        for (key, value) in fields {
            result[key.as_str()] = value;
        }
    }
    result["mutations"] = read_only_mutations();
    (result, code)
}

/// A prompt payload cannot have its declared tool policy honored (danso has
/// no per-tool allowlist or permission mode yet), so it refuses before any
/// mutation. Command payloads ignore these fields, exactly like ccc.
fn unsupported_tool_policy(task: &Task) -> bool {
    let prompt_kind = task
        .payload
        .as_ref()
        .map(|payload| payload.kind)
        .unwrap_or(PayloadKind::Prompt)
        == PayloadKind::Prompt;
    prompt_kind
        && (!task.allowed_tools.is_empty() || task.permission_mode != PermissionMode::Default)
}

/// One executed run (ccc `run_execute`). `plan_handoff` carries the tick's
/// already-computed plan row and instant so the per-run path skips the
/// all-tasks replan; the manual `run` command keeps `None` and plans for
/// itself. Returns the result document and the process exit code.
#[allow(clippy::too_many_lines)]
pub fn execute(
    store_path: &Path,
    store: &Store,
    store_display: &str,
    task_id: &str,
    at_raw: Option<&str>,
    now: DateTime<Utc>,
    plan_handoff: Option<(&Value, DateTime<Utc>)>,
) -> (Value, i32) {
    let Some(task) = store.tasks.iter().find(|task| task.id == task_id) else {
        return (
            json!({
                "ok": false,
                "mode": "run-execute",
                "taskId": task_id,
                "error": "task id not found",
            }),
            1,
        );
    };
    let (row, at) = match plan_handoff {
        Some((row, at)) => (row.clone(), at),
        None => {
            let plan = due_plan(store, store_display, at_raw, now);
            let at = parse_utc(Some(plan["at"].as_str().unwrap_or_default()), "plan at")
                .ok()
                .flatten()
                .unwrap_or_else(|| truncate_minute(now));
            let row = plan["tasks"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(task_id)))
                .cloned()
                .unwrap_or(Value::Null);
            (row, at)
        }
    };
    if row.is_null() {
        return (
            json!({
                "ok": false,
                "mode": "run-execute",
                "taskId": task_id,
                "error": "task id not found in due plan",
            }),
            1,
        );
    }
    let base = json!({
        "mode": "run-execute",
        "store": store_display,
        "at": fmt_dt(Some(at)),
        "taskId": task_id,
        "scheduledAt": row.get("scheduledAt").cloned().unwrap_or(Value::Null),
        "due": row.get("due").cloned().unwrap_or(json!(false)),
        "notification": crate::cron::notify::notification_base(task),
    });
    let current_limit = run_limit_metadata(task);
    if current_limit["reached"].as_bool().unwrap_or(false) {
        return precheck_result(
            &base,
            "run-limit-reached",
            json!({ "runLimit": current_limit }),
            0,
        );
    }
    if !task.enabled {
        return precheck_result(&base, "disabled", json!({}), 0);
    }
    if !row["due"].as_bool().unwrap_or(false) {
        return precheck_result(&base, "not-due", json!({}), 0);
    }
    if unsupported_tool_policy(task) {
        let mut result = base.clone();
        result["ok"] = json!(false);
        result["status"] = json!("unsupported-tool-policy");
        result["error"] = json!(
            "prompt tasks with allowedTools or a non-default permissionMode are refused: danso cannot enforce per-tool policies yet"
        );
        result["mutations"] = read_only_mutations();
        return (result, 1);
    }
    let scheduled_at = row
        .get("scheduledAt")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fmt_dt(Some(at)).unwrap_or_default());
    let run_id = format!("{task_id}-{}-{}", at.timestamp(), std::process::id());
    let (acquired, lock) =
        match acquire_for_run(store_path, task_id, task, &run_id, &scheduled_at, at) {
            Ok((acquired, lock)) => (acquired, lock),
            // The task guard flock failed (filesystem-level); nothing was
            // locked or run.
            Err(error) => {
                let mut result = base.clone();
                result["ok"] = json!(false);
                result["status"] = json!("lock-error");
                result["runId"] = json!(run_id);
                result["lock"] = json!({ "state": "error", "error": error });
                result["mutations"] = read_only_mutations();
                return (result, 1);
            }
        };
    if !acquired {
        let state = lock["state"].as_str().unwrap_or("lock-error").to_string();
        let status = if state == "held" || state == "stale" {
            "locked".to_string()
        } else {
            state
        };
        let mut result = base.clone();
        result["ok"] = json!(false);
        result["status"] = json!(status);
        result["runId"] = json!(run_id);
        result["lock"] = lock;
        result["mutations"] = read_only_mutations();
        return (result, 1);
    }

    // --- payload run, spool, history, state commit (no early returns) ---
    let headless = outcome_document(
        task,
        &crate::cron::exec::run_payload(task, store_path, &run_id).unwrap_or_else(|error| {
            HeadlessOutcome {
                exit_code: 127,
                stdout: String::new(),
                stderr: crate::cron::commit::short_text(&error, 4000),
                timed_out: false,
            }
        }),
    );
    let exit_code = headless["exitCode"].as_i64().unwrap_or(127);
    let timed_out = headless["timedOut"].as_bool().unwrap_or(false);
    let mut ok = success_exit_codes(task).contains(&exit_code);
    // A timed-out run is never a success even if 124 were listed.
    if ok && timed_out {
        ok = false;
    }
    let status = if ok {
        "success"
    } else if timed_out {
        "timeout"
    } else {
        "failed"
    };
    let rc_run: i32 = if ok { 0 } else { 1 };
    let notification = crate::cron::notify::write_owner_spool(
        task,
        task_id,
        &run_id,
        &scheduled_at,
        status,
        &headless,
        at,
    );
    let mut notify_state = notification["delivery"]
        .as_str()
        .unwrap_or("none")
        .to_string();
    if task.notify == NotifyMode::None {
        notify_state = "none".to_string();
    }
    let attempt = history_attempt(task, &scheduled_at);
    let entry = RunHistoryItem {
        run_id: run_id.clone(),
        scheduled_at: scheduled_at.clone(),
        started_at: fmt_dt(Some(at)).unwrap_or_default(),
        finished_at: fmt_dt(Some(at)),
        status: status.to_string(),
        exit_code: Some(exit_code),
        attempt,
        notify_state,
    };
    let mut run_task = task.clone();
    let evicted = append_run_history(&mut run_task, entry);
    // Archive first: a failure here is a durable-record failure and must
    // quarantine the occurrence exactly like a failed state commit.
    let mut persist_error: Option<String> = archive_history(store_path, task_id, &evicted).err();
    let mut retry =
        apply_retry_transition(&mut run_task, &scheduled_at, attempt, &run_id, status, at);
    let run_limit = apply_run_limit(&mut run_task);
    if run_limit["reached"].as_bool().unwrap_or(false)
        && retry["retryEligibleAt"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    {
        retry["retryEligibleAt"] = Value::Null;
        retry["cancelledByRunLimit"] = json!(true);
    }
    run_task.last_run_at = Some(scheduled_at.clone());
    run_task.last_status = Some(status.to_string());
    run_task.last_run_id = Some(run_id.clone());
    let mut one_shot_disabled = false;
    if status == "success"
        && !run_task.keep_after_run
        && let Ok(spec) =
            crate::cron::schedule::parse_schedule(&run_task.schedule, &run_task.timezone)
        && spec.kind() == "once"
    {
        run_task.enabled = false;
        one_shot_disabled = true;
    }
    let run_task_value = serde_json::to_value(&run_task).unwrap_or(Value::Null);
    let persisted = if persist_error.is_some() {
        false
    } else {
        match commit_run_state(store_path, task_id, &run_task_value, !run_task.enabled) {
            Ok(persisted) => persisted,
            Err(error) => {
                persist_error = Some(crate::cron::commit::short_text(&error, 4000));
                false
            }
        }
    };
    // Release only a run whose durable record landed; a persist failure
    // quarantines the lock so the next tick cannot re-execute the occurrence.
    let release = if persist_error.is_none() {
        crate::cron::locks::release_for_run(store_path, task_id, &run_id).unwrap_or_else(
            |error| json!({ "ok": false, "state": "release-error", "error": error }),
        )
    } else {
        crate::cron::locks::quarantine_persist_failure(store_path, task_id, &run_id, at)
            .unwrap_or_else(
                |error| json!({ "ok": false, "state": "persist-quarantine-error", "error": error }),
            )
    };
    let ran = true; // ccc `headless is not None`: an outcome (even exit 127) means the payload ran.
    let spooled = notification["delivery"].as_str() == Some("spooled");
    let mut result = base.clone();
    result["ok"] = json!(ok);
    result["status"] = json!(status);
    result["runId"] = json!(run_id);
    result["lock"] = json!({
        "state": lock["state"].clone(),
        "path": lock["path"].clone(),
        "release": release,
    });
    result["headless"] = headless;
    result["notification"] = notification;
    result["oneShotDisabled"] = json!(one_shot_disabled);
    result["retry"] = retry;
    result["runLimit"] = json!(run_limit);
    result["mutations"] = executed_mutations(persisted, ran, spooled);
    let mut code = rc_run;
    if let Some(persist_error) = persist_error {
        // The run really happened; only its durable record failed. Surface it
        // loudly so the operator repairs the store instead of letting the next
        // tick re-execute this occurrence.
        result["ok"] = json!(false);
        result["persistError"] = json!(persist_error);
        result["status"] = json!("persist-failed");
        code = 1;
    }
    if !release["ok"].as_bool().unwrap_or(false) {
        result["ok"] = json!(false);
        result["releaseError"] = json!(release);
        code = 1;
    }
    (result, code)
}
