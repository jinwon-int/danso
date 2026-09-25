//! The `agent-cron` task store, schema v1 — structurally identical to ccc
//! `schemas/agent-cron-task-store.schema.json` (§6.5 file compatibility).
//!
//! Loading is fail-closed like `agent_cron_schema.validate_store`: unknown
//! fields are rejected, every documented range/pattern is enforced, and the
//! store-level semantic rules (duplicate ids, cross-field payload rules)
//! are layered on top. A missing store file is an empty store.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub version: u8,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "telegram-owner")]
    TelegramOwner,
    #[serde(rename = "telegram-owner-on-failure")]
    TelegramOwnerOnFailure,
    #[serde(rename = "telegram-chat")]
    TelegramChat,
    #[serde(rename = "telegram-chat-on-failure")]
    TelegramChatOnFailure,
}

impl NotifyMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::TelegramOwner => "telegram-owner",
            Self::TelegramOwnerOnFailure => "telegram-owner-on-failure",
            Self::TelegramChat => "telegram-chat",
            Self::TelegramChatOnFailure => "telegram-chat-on-failure",
        }
    }

    fn needs_chat_id(&self) -> bool {
        matches!(self, Self::TelegramChat | Self::TelegramChatOnFailure)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchUpPolicy {
    #[default]
    #[serde(rename = "skip")]
    Skip,
    #[serde(rename = "once")]
    Once,
    #[serde(rename = "all")]
    All,
}

impl CatchUpPolicy {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Once => "once",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadKind {
    #[serde(rename = "prompt")]
    Prompt,
    #[serde(rename = "command")]
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Payload {
    pub kind: PayloadKind,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_sec: Option<i64>,
    #[serde(default)]
    pub output_max_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicySpec {
    #[serde(default, rename = "maxAttempts")]
    pub max_attempts: Option<i64>,
    #[serde(default, rename = "backoffSec")]
    pub backoff_sec: Option<i64>,
    #[serde(default, rename = "backoffMultiplier")]
    pub multiplier: Option<i64>,
    #[serde(default, rename = "maxBackoffSec")]
    pub max_backoff_sec: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RetryState {
    pub scheduled_at: String,
    pub attempt: i64,
    #[serde(default)]
    pub retry_eligible_at: Option<String>,
    pub last_status: String,
    #[serde(default)]
    pub last_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunHistoryItem {
    pub run_id: String,
    pub scheduled_at: String,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    pub attempt: i64,
    pub notify_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    pub enabled: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub success_exit_codes: Option<Vec<i64>>,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub notify: NotifyMode,
    #[serde(default)]
    pub notify_chat_id: Option<String>,
    #[serde(default)]
    pub attach_memory: Vec<String>,
    #[serde(default)]
    pub attach_skills: Vec<String>,
    #[serde(default = "default_redact_profile")]
    pub redact_profile: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub anchor_at: Option<String>,
    #[serde(default)]
    pub keep_after_run: bool,
    #[serde(default)]
    pub payload: Option<Payload>,
    #[serde(default)]
    pub catch_up_policy: CatchUpPolicy,
    #[serde(default = "default_max_catchup")]
    pub max_catchup: i64,
    #[serde(default)]
    pub lock_timeout_sec: i64,
    #[serde(default = "default_max_run_history")]
    pub max_run_history: i64,
    #[serde(default)]
    pub not_before: Option<String>,
    #[serde(default)]
    pub max_runs: Option<i64>,
    #[serde(default)]
    pub run_count: i64,
    #[serde(default)]
    pub run_history: Vec<RunHistoryItem>,
    #[serde(default)]
    pub retry_policy: Option<RetryPolicySpec>,
    #[serde(default)]
    pub retry_state: Option<RetryState>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_run_id: Option<String>,
}

fn default_redact_profile() -> String {
    "default".to_string()
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_max_catchup() -> i64 {
    1
}

fn default_max_run_history() -> i64 {
    20
}

/// `cron/tasks.json` under the state root (§6.5).
pub fn store_path(home: &Path) -> std::path::PathBuf {
    home.join("cron").join("tasks.json")
}

/// `cron/locks/` next to the store — lock files are `<id>.lock` (§6.5).
pub fn locks_dir(store_path: &Path) -> std::path::PathBuf {
    store_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("locks")
}

/// Load the store. A missing file is an empty store; anything unreadable,
/// structurally invalid, or semantically invalid fails closed with all
/// validation errors joined.
pub fn load(path: &Path) -> Result<Store, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Store {
                version: 1,
                tasks: Vec::new(),
            });
        }
        Err(_) => return Err("cron store is unreadable".to_string()),
    };
    let store: Store = serde_json::from_str(&text)
        .map_err(|error| format!("cron store is not a valid v1 store: {error}"))?;
    let errors = validate(&store);
    if errors.is_empty() {
        Ok(store)
    } else {
        Err(errors.join("; "))
    }
}

fn is_non_whitespace(text: &str) -> bool {
    text.chars().any(|c| !c.is_whitespace())
}

fn in_range(value: i64, low: i64, high: i64) -> bool {
    (low..=high).contains(&value)
}

/// Structural + store-semantic validation, mirroring
/// `agent_cron_schema.validate_store` (schema v1 plus duplicate ids and the
/// cross-field payload rules). Returns every violation found.
pub fn validate(store: &Store) -> Vec<String> {
    fn pattern(storage: &'static OnceLock<regex::Regex>, source: &str) -> &'static regex::Regex {
        storage.get_or_init(|| regex::Regex::new(source).expect("valid pattern"))
    }
    static ID_RX: OnceLock<regex::Regex> = OnceLock::new();
    static CHAT_ID_RX: OnceLock<regex::Regex> = OnceLock::new();
    let mut errors = Vec::new();
    if store.version != 1 {
        errors.push("version must be 1".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for (index, task) in store.tasks.iter().enumerate() {
        let location = format!("tasks[{index}]");
        if !pattern(&ID_RX, r"^[A-Za-z0-9_.-]{1,96}$").is_match(&task.id) {
            errors.push(format!(
                "{location}.id does not match ^[A-Za-z0-9_.-]{{1,96}}$"
            ));
        }
        if !seen.insert(task.id.clone()) {
            errors.push(format!("duplicate task id: {}", task.id));
        }
        for (field, value) in [("schedule", &task.schedule), ("prompt", &task.prompt)] {
            if value.is_empty() || !is_non_whitespace(value) {
                errors.push(format!(
                    "{location}.{field} must contain non-whitespace text"
                ));
            }
        }
        if let Some(codes) = &task.success_exit_codes {
            if codes.is_empty() || codes.len() > 64 {
                errors.push(format!(
                    "{location}.successExitCodes must have 1-64 entries"
                ));
            }
            if codes.iter().any(|code| !in_range(*code, 0, 255)) {
                errors.push(format!("{location}.successExitCodes entries must be 0-255"));
            }
            let unique: std::collections::HashSet<&i64> = codes.iter().collect();
            if unique.len() != codes.len() {
                errors.push(format!(
                    "{location}.successExitCodes entries must be unique"
                ));
            }
        }
        if task.timezone.trim().is_empty() || task.timezone.len() > 64 {
            errors.push(format!("{location}.timezone must be 1-64 characters"));
        }
        if let Some(chat_id) = &task.notify_chat_id {
            let valid = chat_id.len() <= 64
                && pattern(
                    &CHAT_ID_RX,
                    r"^(-?[0-9]{1,32}|@[A-Za-z][A-Za-z0-9_]{3,31})$",
                )
                .is_match(chat_id);
            if !valid {
                errors.push(format!(
                    "{location}.notifyChatId does not match the chat id pattern"
                ));
            }
        }
        if task.notify.needs_chat_id() && task.notify_chat_id.is_none() {
            errors.push(format!(
                "{location}.notifyChatId is required for notify '{}'",
                task.notify.label()
            ));
        }
        if let Some(not_before) = &task.not_before
            && not_before.is_empty()
        {
            errors.push(format!("{location}.notBefore must not be empty"));
        }
        if let Some(max_runs) = task.max_runs
            && !in_range(max_runs, 1, 100_000)
        {
            errors.push(format!("{location}.maxRuns must be 1-100000"));
        }
        if !in_range(task.run_count, 0, 100_000) {
            errors.push(format!("{location}.runCount must be 0-100000"));
        }
        if !in_range(task.max_catchup, 1, 100) {
            errors.push(format!("{location}.maxCatchup must be 1-100"));
        }
        if !in_range(task.lock_timeout_sec, 0, 86400) {
            errors.push(format!("{location}.lockTimeoutSec must be 0-86400"));
        }
        if !in_range(task.max_run_history, 1, 500) {
            errors.push(format!("{location}.maxRunHistory must be 1-500"));
        }
        if let Some(policy) = &task.retry_policy {
            let rules: [(&str, Option<i64>, i64, i64); 4] = [
                ("maxAttempts", policy.max_attempts, 1, 10),
                ("backoffSec", policy.backoff_sec, 0, 86400),
                ("backoffMultiplier", policy.multiplier, 1, 10),
                ("maxBackoffSec", policy.max_backoff_sec, 0, 86400),
            ];
            for (field, value, low, high) in rules {
                if let Some(value) = value
                    && !in_range(value, low, high)
                {
                    errors.push(format!(
                        "{location}.retryPolicy.{field} must be {low}-{high}"
                    ));
                }
            }
        }
        if let Some(state) = &task.retry_state {
            if state.scheduled_at.is_empty() || state.last_status.is_empty() {
                errors.push(format!("{location}.retryState fields must not be empty"));
            }
            if state.attempt < 1 {
                errors.push(format!("{location}.retryState.attempt must be >= 1"));
            }
        }
        for (index, entry) in task.run_history.iter().enumerate() {
            let entry_location = format!("{location}.runHistory[{index}]");
            for (field, value) in [
                ("runId", &entry.run_id),
                ("scheduledAt", &entry.scheduled_at),
                ("startedAt", &entry.started_at),
                ("status", &entry.status),
                ("notifyState", &entry.notify_state),
            ] {
                if value.is_empty() {
                    errors.push(format!("{entry_location}.{field} must not be empty"));
                }
            }
            if entry.attempt < 1 {
                errors.push(format!("{entry_location}.attempt must be >= 1"));
            }
        }
        if let Some(payload) = &task.payload {
            let location = format!("{location}.payload");
            match payload.kind {
                PayloadKind::Command => {
                    if payload.argv.as_ref().is_none_or(|argv| argv.is_empty()) {
                        errors.push(format!("{location}.argv is required for kind 'command'"));
                    }
                    if payload.model.is_some() {
                        errors.push(format!(
                            "{location}.model is not allowed for kind 'command'"
                        ));
                    }
                }
                PayloadKind::Prompt => {
                    for forbidden in ["argv", "cwd"] {
                        let present = match forbidden {
                            "argv" => payload.argv.is_some(),
                            _ => payload.cwd.is_some(),
                        };
                        if present {
                            errors.push(format!(
                                "{location}.{forbidden} is not allowed for kind 'prompt'"
                            ));
                        }
                    }
                }
            }
            if let Some(argv) = &payload.argv {
                if argv.len() > 64 {
                    errors.push(format!("{location}.argv must have at most 64 entries"));
                }
                if argv.iter().any(|item| item.is_empty()) {
                    errors.push(format!("{location}.argv entries must not be empty"));
                }
            }
            if let Some(cwd) = &payload.cwd
                && cwd.is_empty()
            {
                errors.push(format!("{location}.cwd must not be empty"));
            }
            if let Some(model) = &payload.model
                && (model.is_empty() || model.len() > 128)
            {
                errors.push(format!("{location}.model must be 1-128 characters"));
            }
            if let Some(timeout) = payload.timeout_sec
                && !in_range(timeout, 1, 86400)
            {
                errors.push(format!("{location}.timeoutSec must be 1-86400"));
            }
            if let Some(output_max_bytes) = payload.output_max_bytes
                && !in_range(output_max_bytes, 1024, 1_048_576)
            {
                errors.push(format!("{location}.outputMaxBytes must be 1024-1048576"));
            }
        }
    }
    errors
}
