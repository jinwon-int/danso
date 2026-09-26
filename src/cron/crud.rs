//! The store-mutation surface (#120 PR4): `add`, `edit`, `remove`,
//! `enable`, `disable`, and the ccc `tasks.json` import path — a port of ccc
//! `agent_cron.py::parse_crud_args` / `_crud_add` / `_crud_edit` /
//! `crud_command` / `_dispatch` (the CRUD half), using danso's write path:
//! every mutation takes the store flock, re-reads the store *inside* the
//! guard (the store loaded at CLI startup is discarded as possibly stale),
//! validates the candidate, and replaces the file atomically via
//! `commit::write_store_document`.
//!
//! Exit codes and check ordering match ccc exactly:
//! - `add`: duplicate id (rc 1) → `--schedule` required (rc 2) → `--prompt`
//!   required (rc 2) → schedule/notBefore bounds (rc 2) → candidate
//!   validation (rc 1);
//! - `edit`: unknown id (rc 1) → at least one field flag (rc 2) → bounds
//!   (rc 2) → candidate validation (rc 1); set-only partial update, payload
//!   flags merge (`--argv` replaces the whole argv and flips the kind to
//!   `command`; a payload without argv defaults to kind `prompt`);
//! - `remove`/`enable`/`disable`: unknown id (rc 1); enable/disable write
//!   only on an actual flip and report `changed` (`taskStoreWrite: changed`);
//! - success documents carry the task **without `prompt`** plus
//!   `mutations.taskStoreWrite`; rc 0 ok, 1 not-found/duplicate/validation/
//!   store errors, 2 usage errors.
//!
//! Documented divergences from ccc: the result `task` materializes schema
//! defaults explicitly (ccc omits keys it never set — both serialize to the
//! identical effective store); `remove`/`enable`/`disable` run candidate
//! validation before the write (ccc validates only in add/edit); the import
//! surface is a danso addition (ccc has none).

use crate::cron::commit::write_store_document;
use crate::cron::locks::store_flock;
use crate::cron::schedule::parse_schedule;
use crate::cron::store::{
    self, CatchUpPolicy, NotifyMode, Payload, PayloadKind, PermissionMode, Store, Task,
};
use crate::cron::time::parse_utc;
use serde_json::{Value, json};
use std::path::Path;

/// One CRUD invocation's flags, in ccc `CRUD_VALUE_FLAGS`/`CRUD_BOOL_FLAGS`
/// order. Built from the clap surface in `mod.rs`; this type is clap-free so
/// the mutation logic is directly testable.
#[derive(Debug, Clone, Default)]
pub struct FieldFlags {
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub notify: Option<String>,
    pub notify_chat_id: Option<String>,
    pub permission_mode: Option<String>,
    pub catch_up_policy: Option<String>,
    pub anchor_at: Option<String>,
    pub not_before: Option<String>,
    pub redact_profile: Option<String>,
    /// `--allowed-tools` CSV. `None` = flag absent; `Some` (even empty)
    /// replaces the whole list, matching ccc where the key is simply set.
    pub allowed_tools: Option<Vec<String>>,
    pub success_exit_codes: Option<Vec<i64>>,
    pub max_catchup: Option<i64>,
    pub lock_timeout_sec: Option<i64>,
    pub max_run_history: Option<i64>,
    pub max_runs: Option<i64>,
    // Payload buckets (ccc `payload` bucket + the `argv` bucket).
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub timeout_sec: Option<i64>,
    pub output_max_bytes: Option<i64>,
    /// Each `--argv` occurrence appends one command word (ccc argv bucket).
    pub argv: Vec<String>,
    pub keep_after_run: bool,
    pub disabled: bool,
}

impl FieldFlags {
    /// ccc `fields or payload or argv or disabled` — `--keep-after-run`
    /// counts (it sets a field), `--json` does not.
    fn has_any(&self) -> bool {
        self.schedule.is_some()
            || self.prompt.is_some()
            || self.name.is_some()
            || self.timezone.is_some()
            || self.notify.is_some()
            || self.notify_chat_id.is_some()
            || self.permission_mode.is_some()
            || self.catch_up_policy.is_some()
            || self.anchor_at.is_some()
            || self.not_before.is_some()
            || self.redact_profile.is_some()
            || self.allowed_tools.is_some()
            || self.success_exit_codes.is_some()
            || self.max_catchup.is_some()
            || self.lock_timeout_sec.is_some()
            || self.max_run_history.is_some()
            || self.max_runs.is_some()
            || self.payload_flag_present()
            || !self.argv.is_empty()
            || self.keep_after_run
            || self.disabled
    }

    fn payload_flag_present(&self) -> bool {
        self.cwd.is_some()
            || self.model.is_some()
            || self.timeout_sec.is_some()
            || self.output_max_bytes.is_some()
    }
}

/// ccc `agent-cron add <id> [flags]`.
pub fn add(
    path: &Path,
    store_display: &str,
    id: &str,
    flags: &FieldFlags,
) -> Result<(Value, i32), String> {
    let base = json!({ "mode": "add", "store": store_display, "taskId": id });
    let _guard = store_flock(path)?;
    let mut data = store::load(path)?;
    if data.tasks.iter().any(|task| task.id == id) {
        return Ok((failure(&base, "task id already exists"), 1));
    }
    let schedule = flags.schedule.clone().unwrap_or_default();
    if !non_whitespace(&schedule) {
        return Ok((failure(&base, "--schedule is required"), 2));
    }
    let prompt = flags.prompt.clone().unwrap_or_default();
    if !non_whitespace(&prompt) {
        return Ok((
            failure(
                &base,
                "--prompt is required (for command payloads it is the human description)",
            ),
            2,
        ));
    }
    let timezone = flags.timezone.clone().unwrap_or_else(|| "UTC".to_string());
    // ccc validates the schedule bounds before the full candidate.
    if let Err(error) = parse_schedule(&schedule, &timezone) {
        return Ok((
            failure(&base, &format!("invalid schedule bounds: {error}")),
            2,
        ));
    }
    if let Some(not_before) = &flags.not_before
        && let Err(error) = parse_utc(Some(not_before), "notBefore")
    {
        return Ok((
            failure(&base, &format!("invalid schedule bounds: {error}")),
            2,
        ));
    }
    let mut task = Task {
        id: id.to_string(),
        schedule,
        prompt,
        enabled: !flags.disabled,
        name: flags.name.clone(),
        allowed_tools: csv(flags.allowed_tools.as_deref().unwrap_or_default()),
        success_exit_codes: flags.success_exit_codes.clone(),
        permission_mode: PermissionMode::Default,
        notify: NotifyMode::None,
        notify_chat_id: flags.notify_chat_id.clone(),
        attach_memory: Vec::new(),
        attach_skills: Vec::new(),
        redact_profile: flags
            .redact_profile
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        timezone,
        anchor_at: flags.anchor_at.clone(),
        keep_after_run: flags.keep_after_run,
        payload: add_payload(flags),
        catch_up_policy: CatchUpPolicy::Skip,
        max_catchup: flags.max_catchup.unwrap_or(1),
        lock_timeout_sec: flags.lock_timeout_sec.unwrap_or(0),
        max_run_history: flags.max_run_history.unwrap_or(20),
        not_before: flags.not_before.clone(),
        max_runs: flags.max_runs,
        run_count: 0,
        run_history: Vec::new(),
        retry_policy: None,
        retry_state: None,
        last_run_at: None,
        last_status: None,
        last_run_id: None,
    };
    // Enum-valued flags are plain strings in ccc and only fail inside
    // validate_store (rc 1), after the rc-2 bounds checks above.
    if let Some(error) = apply_enum_flags(&mut task, flags, data.tasks.len()) {
        return Ok((failure(&base, &error), 1));
    }
    data.tasks.push(task.clone());
    if let Some(document) = candidate_rejection(&data, &base) {
        return Ok((document, 1));
    }
    write_store_document(path, &store_value(&data)?)?;
    Ok((success_task(&base, &task), 0))
}

/// ccc `agent-cron edit <id> [flags]`: set-only partial update with payload
/// merge. No clear semantics — unset by remove+add, like ccc.
pub fn edit(
    path: &Path,
    store_display: &str,
    id: &str,
    flags: &FieldFlags,
) -> Result<(Value, i32), String> {
    let base = json!({ "mode": "edit", "store": store_display, "taskId": id });
    let _guard = store_flock(path)?;
    let mut data = store::load(path)?;
    let Some(index) = data.tasks.iter().position(|task| task.id == id) else {
        return Ok((failure(&base, "task id not found"), 1));
    };
    if !flags.has_any() {
        return Ok((failure(&base, "edit requires at least one field flag"), 2));
    }
    let mut updated = data.tasks[index].clone();
    if let Some(value) = &flags.schedule {
        updated.schedule = value.clone();
    }
    if let Some(value) = &flags.prompt {
        updated.prompt = value.clone();
    }
    if let Some(value) = &flags.name {
        updated.name = Some(value.clone());
    }
    if let Some(value) = &flags.timezone {
        updated.timezone = value.clone();
    }
    if let Some(value) = &flags.notify_chat_id {
        updated.notify_chat_id = Some(value.clone());
    }
    if let Some(value) = &flags.anchor_at {
        updated.anchor_at = Some(value.clone());
    }
    if let Some(value) = &flags.not_before {
        updated.not_before = Some(value.clone());
    }
    if let Some(value) = &flags.redact_profile {
        updated.redact_profile = value.clone();
    }
    if let Some(values) = &flags.allowed_tools {
        updated.allowed_tools = csv(values);
    }
    if let Some(codes) = &flags.success_exit_codes {
        updated.success_exit_codes = Some(codes.clone());
    }
    if let Some(value) = flags.max_catchup {
        updated.max_catchup = value;
    }
    if let Some(value) = flags.lock_timeout_sec {
        updated.lock_timeout_sec = value;
    }
    if let Some(value) = flags.max_run_history {
        updated.max_run_history = value;
    }
    if let Some(value) = flags.max_runs {
        updated.max_runs = Some(value);
    }
    // ccc: `--keep-after-run` only ever sets true; `--disabled` only ever
    // clears `enabled` (re-enabling is the separate `enable` command).
    if flags.keep_after_run {
        updated.keep_after_run = true;
    }
    if flags.disabled {
        updated.enabled = false;
    }
    if !flags.argv.is_empty() || flags.payload_flag_present() {
        let mut merged = updated.payload.clone().unwrap_or(Payload {
            kind: PayloadKind::Prompt,
            argv: None,
            cwd: None,
            model: None,
            timeout_sec: None,
            output_max_bytes: None,
        });
        if !flags.argv.is_empty() {
            merged.kind = PayloadKind::Command;
            merged.argv = Some(flags.argv.clone());
        }
        if flags.cwd.is_some() {
            merged.cwd = flags.cwd.clone();
        }
        if flags.model.is_some() {
            merged.model = flags.model.clone();
        }
        if flags.timeout_sec.is_some() {
            merged.timeout_sec = flags.timeout_sec;
        }
        if flags.output_max_bytes.is_some() {
            merged.output_max_bytes = flags.output_max_bytes;
        }
        updated.payload = Some(merged);
    }
    // Bounds and enum flags are re-checked on the merged task, like ccc.
    if let Err(error) = parse_schedule(&updated.schedule, &updated.timezone) {
        return Ok((
            failure(&base, &format!("invalid schedule bounds: {error}")),
            2,
        ));
    }
    if let Some(not_before) = &updated.not_before
        && let Err(error) = parse_utc(Some(not_before), "notBefore")
    {
        return Ok((
            failure(&base, &format!("invalid schedule bounds: {error}")),
            2,
        ));
    }
    if let Some(error) = apply_enum_flags(&mut updated, flags, index) {
        return Ok((failure(&base, &error), 1));
    }
    data.tasks[index] = updated.clone();
    if let Some(document) = candidate_rejection(&data, &base) {
        return Ok((document, 1));
    }
    write_store_document(path, &store_value(&data)?)?;
    Ok((success_task(&base, &updated), 0))
}

/// ccc `agent-cron remove|enable|disable <task-id>` — store-only mutations
/// with no flags (the clap surface rejects flags before this runs).
pub fn simple(
    path: &Path,
    store_display: &str,
    command: &str,
    id: &str,
) -> Result<(Value, i32), String> {
    let base = json!({ "mode": command, "store": store_display, "taskId": id });
    let _guard = store_flock(path)?;
    let mut data = store::load(path)?;
    let Some(index) = data.tasks.iter().position(|task| task.id == id) else {
        return Ok((failure(&base, "task id not found"), 1));
    };
    if command == "remove" {
        data.tasks.remove(index);
        if let Some(document) = candidate_rejection(&data, &base) {
            return Ok((document, 1));
        }
        write_store_document(path, &store_value(&data)?)?;
        let mut document = base;
        document["ok"] = json!(true);
        document["mutations"] = json!({ "taskStoreWrite": true });
        return Ok((document, 0));
    }
    let desired = command == "enable";
    let changed = data.tasks[index].enabled != desired;
    data.tasks[index].enabled = desired;
    if changed {
        write_store_document(path, &store_value(&data)?)?;
    }
    let mut document = base;
    document["ok"] = json!(true);
    document["enabled"] = json!(desired);
    document["changed"] = json!(changed);
    document["mutations"] = json!({ "taskStoreWrite": changed });
    Ok((document, 0))
}

/// danso addition: import tasks from a ccc-compatible `tasks.json` (schema
/// v1, fail-closed validated). Ids already present in the store are skipped;
/// the write happens only when at least one task was added.
pub fn import(path: &Path, store_display: &str, source: &Path) -> Result<(Value, i32), String> {
    let base = json!({
        "mode": "import",
        "store": store_display,
        "source": source.display().to_string(),
    });
    let _guard = store_flock(path)?;
    let mut data = store::load(path)?;
    let text = std::fs::read_to_string(source)
        .map_err(|error| format!("cannot read import source {}: {error}", source.display()))?;
    let incoming = store::load_str(&text)?;
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for task in incoming.tasks {
        if data.tasks.iter().any(|existing| existing.id == task.id) {
            skipped.push(json!({ "id": task.id, "reason": "task id already exists" }));
            continue;
        }
        added.push(task);
    }
    let added_ids: Vec<String> = added.iter().map(|task| task.id.clone()).collect();
    let mut document = base;
    document["ok"] = json!(true);
    document["added"] = json!(added_ids);
    document["skipped"] = json!(skipped);
    if added.is_empty() {
        document["mutations"] = json!({ "taskStoreWrite": false });
        return Ok((document, 0));
    }
    data.tasks.extend(added);
    if let Some(rejection) = candidate_rejection(&data, &document) {
        return Ok((rejection, 1));
    }
    write_store_document(path, &store_value(&data)?)?;
    document["mutations"] = json!({ "taskStoreWrite": true });
    Ok((document, 0))
}

/// ccc `add`: argv present → `command` payload (flags merged in, so a
/// mistaken `--model` still lands and candidate validation rejects it);
/// else payload flags present → `prompt` payload; else no payload.
fn add_payload(flags: &FieldFlags) -> Option<Payload> {
    if flags.argv.is_empty() && !flags.payload_flag_present() {
        return None;
    }
    Some(Payload {
        kind: if flags.argv.is_empty() {
            PayloadKind::Prompt
        } else {
            PayloadKind::Command
        },
        argv: if flags.argv.is_empty() {
            None
        } else {
            Some(flags.argv.clone())
        },
        cwd: flags.cwd.clone(),
        model: flags.model.clone(),
        timeout_sec: flags.timeout_sec,
        output_max_bytes: flags.output_max_bytes,
    })
}

/// Enum-valued flags (`--notify`, `--permission-mode`, `--catch-up-policy`)
/// are plain strings in ccc and fail inside validate_store (rc 1). Returns
/// the error text for the first failure, located like the validator does.
fn apply_enum_flags(task: &mut Task, flags: &FieldFlags, index: usize) -> Option<String> {
    if let Some(raw) = &flags.notify {
        match parse_notify(raw) {
            Ok(mode) => task.notify = mode,
            Err(error) => return Some(format!("tasks[{index}].{error}")),
        }
    }
    if let Some(raw) = &flags.permission_mode {
        match raw.as_str() {
            "default" => task.permission_mode = PermissionMode::Default,
            "dontAsk" => task.permission_mode = PermissionMode::DontAsk,
            "acceptEdits" => task.permission_mode = PermissionMode::AcceptEdits,
            other => {
                return Some(format!(
                    "tasks[{index}].permissionMode: unrecognized value '{other}' \
                     (expected one of default, dontAsk, acceptEdits)"
                ));
            }
        }
    }
    if let Some(raw) = &flags.catch_up_policy {
        match raw.as_str() {
            "skip" => task.catch_up_policy = CatchUpPolicy::Skip,
            "once" => task.catch_up_policy = CatchUpPolicy::Once,
            "all" => task.catch_up_policy = CatchUpPolicy::All,
            other => {
                return Some(format!(
                    "tasks[{index}].catchUpPolicy: unrecognized value '{other}' \
                     (expected one of skip, once, all)"
                ));
            }
        }
    }
    None
}

fn parse_notify(raw: &str) -> Result<NotifyMode, String> {
    match raw {
        "none" => Ok(NotifyMode::None),
        "telegram-owner" => Ok(NotifyMode::TelegramOwner),
        "telegram-owner-on-failure" => Ok(NotifyMode::TelegramOwnerOnFailure),
        "telegram-chat" => Ok(NotifyMode::TelegramChat),
        "telegram-chat-on-failure" => Ok(NotifyMode::TelegramChatOnFailure),
        other => Err(format!(
            "notify: unrecognized value '{other}' (expected one of none, \
             telegram-owner, telegram-owner-on-failure, telegram-chat, \
             telegram-chat-on-failure)"
        )),
    }
}

/// ccc `_crud_csv`: split on commas, drop empty items.
fn csv(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|item| item.split(','))
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn non_whitespace(text: &str) -> bool {
    text.chars().any(|c| !c.is_whitespace())
}

fn failure(base: &Value, error: &str) -> Value {
    let mut document = base.clone();
    document["ok"] = json!(false);
    document["error"] = json!(error);
    document
}

fn candidate_rejection(data: &Store, base: &Value) -> Option<Value> {
    let errors = store::validate(data);
    if errors.is_empty() {
        return None;
    }
    let mut document = base.clone();
    document["ok"] = json!(false);
    document["error"] = json!(errors[0]);
    document["errors"] = json!(errors);
    Some(document)
}

fn store_value(data: &Store) -> Result<Value, String> {
    serde_json::to_value(data).map_err(|error| format!("cannot serialize store: {error}"))
}

/// Success document: the stored task **without `prompt`** plus the mutation
/// marker (ccc `{k: v for k, v in task.items() if k != 'prompt'}`).
fn success_task(base: &Value, task: &Task) -> Value {
    let mut value = serde_json::to_value(task).expect("serializable task");
    if let Value::Object(fields) = &mut value {
        fields.remove("prompt");
    }
    let mut document = base.clone();
    document["ok"] = json!(true);
    document["task"] = value;
    document["mutations"] = json!({ "taskStoreWrite": true });
    document
}
