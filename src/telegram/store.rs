use super::{client::Update, ensure_private_dir};
use anyhow::{Context, Result, ensure};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const CONVERSATIONS_DIR: &str = "conversations";
const POLL_OFFSET_FILE: &str = "poll-offset.json";
const MAX_RECORD_BYTES: u64 = 64 * 1024;
pub const MAX_PREVIOUS_SESSIONS: usize = 5;

/// The bounded, provider-neutral counters retained for Telegram's local
/// `/usage` view. The field names on disk follow the core usage summary, while
/// the aliases keep hand-written/early records readable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageRecord {
    #[serde(default)]
    pub requests: u64,
    #[serde(rename = "inputTokens", alias = "input_tokens", default)]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens", alias = "output_tokens", default)]
    pub output_tokens: u64,
    #[serde(rename = "cacheReadTokens", alias = "cache_read_tokens", default)]
    pub cache_read_tokens: u64,
    #[serde(rename = "cacheWriteTokens", alias = "cache_write_tokens", default)]
    pub cache_write_tokens: u64,
    #[serde(rename = "totalTokens", alias = "total_tokens", default)]
    pub total_tokens: u64,
}

impl UsageRecord {
    pub fn from_summary(summary: &Value) -> Result<Self> {
        let usage: Self =
            serde_json::from_value(summary.clone()).context("decode Telegram turn usage")?;
        validate_usage(&usage)?;
        Ok(usage)
    }

    pub fn add(&mut self, other: &Self) -> Result<()> {
        let add = |left: u64, right: u64| {
            left.checked_add(right)
                .context("Telegram usage counter overflow")
        };
        let next = Self {
            requests: add(self.requests, other.requests)?,
            input_tokens: add(self.input_tokens, other.input_tokens)?,
            output_tokens: add(self.output_tokens, other.output_tokens)?,
            cache_read_tokens: add(self.cache_read_tokens, other.cache_read_tokens)?,
            cache_write_tokens: add(self.cache_write_tokens, other.cache_write_tokens)?,
            total_tokens: add(self.total_tokens, other.total_tokens)?,
        };
        validate_usage(&next)?;
        *self = next;
        Ok(())
    }

    pub fn is_zero(&self) -> bool {
        self == &Self::default()
    }
}

/// Durable metadata for the one long task that can be resumed for a chat.
/// Prompts and task output deliberately do not belong in the conversation
/// record; the session pointer is the only link to the journal.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveTaskRecord {
    pub kind: String,
    #[serde(alias = "startedAt")]
    pub started_at: String,
    #[serde(alias = "sessionPointer")]
    pub session_pointer: String,
}

impl fmt::Debug for ActiveTaskRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveTaskRecord")
            .field("kind", &self.kind)
            .field("started_at", &self.started_at)
            .field("session_pointer", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRecord {
    pub chat_id: i64,
    /// -1 means that the record has no consumed update yet.
    pub last_update_id: i64,
    #[serde(default)]
    pub session_pointer: Option<String>,
    /// Newest-first bounded history of displaced session pointers. Entries
    /// are opaque ids here; timeline rendering derives only short ids and
    /// timestamps from the corresponding journals.
    #[serde(
        default,
        alias = "previousSessions",
        alias = "previous_session_pointers",
        alias = "session_history"
    )]
    pub previous_sessions: Vec<String>,
    /// The currently resumable long task, if any. This remains present while
    /// the task is paused until an explicit `/new` replacement clears it.
    #[serde(default, alias = "activeTask")]
    pub active_task: Option<ActiveTaskRecord>,
    /// Provider selected by the service for this chat. Old records omit this
    /// field and inherit the current service provider on their next turn.
    #[serde(default)]
    pub provider: Option<String>,
    /// Per-chat model override. `None` means the service default.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-chat reasoning-effort override. `None` means the service default.
    #[serde(default)]
    pub effort: Option<String>,
    /// A durable marker for a turn that was admitted but has not reached its
    /// completion boundary. A restart clears this marker without replaying
    /// the associated journal.
    #[serde(default, alias = "active_turn", alias = "turnActive", alias = "active")]
    pub turn_active: bool,
    /// The one Telegram message used for all progress edits for the active
    /// turn. It is cleared when the turn reaches a terminal state.
    #[serde(default, alias = "progressMessageId")]
    pub progress_message_id: Option<i64>,
    /// Text-only follow-up prompts waiting for the active turn. These are
    /// durable so a process restart cannot silently discard user work.
    #[serde(default, alias = "followUpQueue", alias = "queued_prompts")]
    pub follow_up_queue: Vec<String>,
    /// Usage from the most recent completed turn, if one exists.
    #[serde(
        rename = "lastTurnUsage",
        alias = "last_turn_usage",
        alias = "last_usage",
        alias = "lastUsage",
        default
    )]
    pub last_turn_usage: Option<UsageRecord>,
    /// Aggregate usage for all completed turns in this chat. `usage` is the
    /// stable short field name; the alias accepts the design-document spelling.
    #[serde(
        rename = "usage",
        alias = "aggregate_usage",
        alias = "aggregateUsage",
        alias = "total_usage",
        alias = "totalUsage",
        default
    )]
    pub usage: UsageRecord,
}

impl fmt::Debug for ConversationRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationRecord")
            .field("chat_id", &self.chat_id)
            .field("last_update_id", &self.last_update_id)
            .field("session_pointer", &self.session_pointer)
            .field("previous_sessions_len", &self.previous_sessions.len())
            .field("active_task", &self.active_task)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("turn_active", &self.turn_active)
            .field("progress_message_id", &self.progress_message_id)
            .field("follow_up_queue_len", &self.follow_up_queue.len())
            .field("last_turn_usage", &self.last_turn_usage)
            .field("usage", &self.usage)
            .finish()
    }
}

impl ConversationRecord {
    pub fn new(chat_id: i64, last_update_id: i64, session_pointer: Option<String>) -> Self {
        Self {
            chat_id,
            last_update_id,
            session_pointer,
            previous_sessions: Vec::new(),
            active_task: None,
            provider: None,
            model: None,
            effort: None,
            turn_active: false,
            progress_message_id: None,
            follow_up_queue: Vec::new(),
            last_turn_usage: None,
            usage: UsageRecord::default(),
        }
    }

    pub fn effective_model<'a>(&'a self, default: &'a str) -> &'a str {
        self.model.as_deref().unwrap_or(default)
    }

    pub fn effective_effort<'a>(&'a self, default: Option<&'a str>) -> Option<&'a str> {
        self.effort.as_deref().or(default)
    }

    pub fn record_turn(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: Option<String>,
        usage: UsageRecord,
    ) -> Result<()> {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self.effort = effort;
        validate_record(self)?;
        self.usage.add(&usage)?;
        self.last_turn_usage = Some(usage);
        validate_record(self)
    }

    /// Record only a completed turn's counters. Settings commands may update
    /// the next-turn model or effort while a provider request is in flight;
    /// keeping this operation separate prevents completion from overwriting
    /// that newer per-chat choice.
    pub fn record_usage(&mut self, usage: UsageRecord) -> Result<()> {
        self.usage.add(&usage)?;
        self.last_turn_usage = Some(usage);
        validate_record(self)
    }

    pub fn mark_turn_active(&mut self) {
        self.turn_active = true;
        self.progress_message_id = None;
    }

    pub fn mark_long_task_active(&mut self, session_pointer: String, started_at: String) {
        self.active_task = Some(ActiveTaskRecord {
            kind: "long_task".to_string(),
            started_at,
            session_pointer,
        });
        self.mark_turn_active();
    }

    pub fn clear_active_task(&mut self) {
        self.active_task = None;
    }

    pub fn set_progress_message(&mut self, message_id: i64) -> Result<()> {
        ensure!(message_id > 0, "Telegram progress message id is invalid");
        self.turn_active = true;
        self.progress_message_id = Some(message_id);
        Ok(())
    }

    pub fn clear_turn_active(&mut self) {
        self.turn_active = false;
        self.progress_message_id = None;
    }

    /// Remember the displaced head before `/new`. The newest displaced
    /// pointer is first, and duplicate pointers are moved to the front so a
    /// resume toggle remains deterministic.
    pub fn remember_current_session(&mut self) {
        let Some(current) = self.session_pointer.clone() else {
            return;
        };
        self.previous_sessions.retain(|pointer| pointer != &current);
        self.previous_sessions.insert(0, current);
        self.previous_sessions.truncate(MAX_PREVIOUS_SESSIONS);
    }

    /// Swap the current head with the most recent previous session. Returning
    /// the newly selected pointer keeps command code from ever displaying a
    /// full session id.
    pub fn swap_previous_session(&mut self) -> Option<String> {
        let previous = self.previous_sessions.first()?.clone();
        self.previous_sessions.remove(0);
        if let Some(current) = self.session_pointer.replace(previous.clone()) {
            self.previous_sessions.insert(0, current);
            self.previous_sessions.truncate(MAX_PREVIOUS_SESSIONS);
        }
        Some(previous)
    }
}

#[derive(Debug, Clone)]
pub struct ConversationStore {
    data_dir: PathBuf,
    records_dir: PathBuf,
}

impl ConversationStore {
    pub fn new(data_dir: &Path) -> Result<Self> {
        ensure_private_dir(data_dir)?;
        let records_dir = data_dir.join(CONVERSATIONS_DIR);
        ensure_private_dir(&records_dir)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            records_dir,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn record_path(&self, chat_id: i64) -> PathBuf {
        self.records_dir.join(format!("{chat_id}.json"))
    }

    pub fn load(&self, chat_id: i64) -> Result<Option<ConversationRecord>> {
        let path = self.record_path(chat_id);
        let Some(file) = open_record(&path)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD_BYTES,
            "Telegram conversation record exceeds its size limit"
        );
        let record: ConversationRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode Telegram conversation record: {}", path.display()))?;
        validate_record(&record)?;
        ensure!(
            record.chat_id == chat_id,
            "Telegram conversation record chat id does not match its path"
        );
        Ok(Some(record))
    }

    pub fn save(&self, record: &ConversationRecord) -> Result<()> {
        validate_record(record)?;
        if let Some(previous) = self.load(record.chat_id)? {
            ensure!(
                record.last_update_id >= previous.last_update_id,
                "Telegram conversation update id cannot move backwards"
            );
        }
        let payload = serde_json::to_vec(record)?;
        ensure!(
            payload.len() as u64 <= MAX_RECORD_BYTES,
            "Telegram conversation record exceeds its size limit"
        );
        atomic_write(&self.record_path(record.chat_id), &payload)
    }

    pub fn save_record(&self, record: &ConversationRecord) -> Result<()> {
        self.save(record)
    }

    /// Load every persisted chat record for service-start recovery and health
    /// reporting. Only numeric JSON record names are part of this store.
    pub fn load_all(&self) -> Result<Vec<ConversationRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.records_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(chat_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<i64>().ok())
            else {
                continue;
            };
            if let Some(record) = self.load(chat_id)? {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.chat_id);
        Ok(records)
    }

    /// Persist the next Telegram update offset separately from per-chat
    /// records. This lets a reconnect resume the same poll position without
    /// relying on an in-memory poller.
    pub fn load_poll_offset(&self) -> Result<Option<i64>> {
        let path = self.data_dir.join(POLL_OFFSET_FILE);
        let Some(file) = open_record(&path)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(64).read_to_end(&mut bytes)?;
        let offset: i64 =
            serde_json::from_slice(&bytes).context("decode Telegram polling offset")?;
        ensure!(offset >= 0, "Telegram polling offset is invalid");
        Ok(Some(offset))
    }

    pub fn save_poll_offset(&self, offset: i64) -> Result<()> {
        ensure!(offset >= 0, "Telegram polling offset is invalid");
        if let Some(previous) = self.load_poll_offset()? {
            ensure!(
                offset >= previous,
                "Telegram polling offset cannot move backwards"
            );
        }
        let payload = serde_json::to_vec(&offset)?;
        atomic_write(&self.data_dir.join(POLL_OFFSET_FILE), &payload)
    }

    /// Advance a chat record monotonically after the caller has handled an
    /// update. A missing session pointer leaves an existing pointer intact.
    pub fn record_update(
        &self,
        update: &Update,
        session_pointer: Option<String>,
    ) -> Result<ConversationRecord> {
        let chat_id = update
            .chat_id()
            .context("Telegram update has no message chat")?;
        let mut record = self
            .load(chat_id)?
            .unwrap_or_else(|| ConversationRecord::new(chat_id, -1, None));
        if update.update_id > record.last_update_id {
            record.last_update_id = update.update_id;
        }
        if session_pointer.is_some() {
            record.session_pointer = session_pointer;
        }
        self.save(&record)?;
        Ok(record)
    }

    pub fn update(
        &self,
        chat_id: i64,
        last_update_id: i64,
        session_pointer: Option<String>,
    ) -> Result<ConversationRecord> {
        let mut record = self
            .load(chat_id)?
            .unwrap_or_else(|| ConversationRecord::new(chat_id, -1, None));
        ensure!(
            last_update_id >= record.last_update_id,
            "Telegram conversation update id cannot move backwards"
        );
        record.last_update_id = last_update_id;
        if session_pointer.is_some() {
            record.session_pointer = session_pointer;
        }
        self.save(&record)?;
        Ok(record)
    }
}

fn validate_record(record: &ConversationRecord) -> Result<()> {
    ensure!(
        record.last_update_id >= -1,
        "Telegram conversation update id is invalid"
    );
    if let Some(pointer) = &record.session_pointer {
        ensure!(
            !pointer.is_empty() && pointer.len() <= 4096,
            "Telegram session pointer must be 1..=4096 bytes"
        );
    }
    ensure!(
        record.previous_sessions.len() <= MAX_PREVIOUS_SESSIONS,
        "Telegram session history exceeds its bound"
    );
    for pointer in &record.previous_sessions {
        ensure!(
            !pointer.is_empty() && pointer.len() <= 4096,
            "Telegram previous session pointer must be 1..=4096 bytes"
        );
    }
    ensure!(
        record
            .previous_sessions
            .windows(2)
            .all(|window| window[0] != window[1]),
        "Telegram session history contains duplicate adjacent pointers"
    );
    if let Some(task) = &record.active_task {
        ensure!(
            task.kind == "long_task",
            "Telegram active task kind is invalid"
        );
        ensure!(
            !task.session_pointer.is_empty() && task.session_pointer.len() <= 4096,
            "Telegram active task session pointer is invalid"
        );
        ensure!(
            !task.started_at.is_empty()
                && task.started_at.len() <= 64
                && !task.started_at.chars().any(char::is_control),
            "Telegram active task start time is invalid"
        );
        ensure!(
            DateTime::parse_from_rfc3339(&task.started_at).is_ok(),
            "Telegram active task start time must be RFC 3339"
        );
    }
    if let Some(message_id) = record.progress_message_id {
        ensure!(message_id > 0, "Telegram progress message id is invalid");
    }
    for prompt in &record.follow_up_queue {
        ensure!(
            !prompt.is_empty(),
            "Telegram follow-up prompt must not be empty"
        );
        ensure!(
            prompt.len() <= 16 * 1024,
            "Telegram follow-up prompt is too long"
        );
    }
    for (name, value) in [
        ("provider", record.provider.as_deref()),
        ("model", record.model.as_deref()),
        ("effort", record.effort.as_deref()),
    ] {
        if let Some(value) = value {
            ensure!(
                !value.trim().is_empty(),
                "Telegram {name} must not be empty"
            );
            ensure!(value.len() <= 4096, "Telegram {name} is too long");
            ensure!(
                value.chars().all(|character| !character.is_control()),
                "Telegram {name} contains a control character"
            );
        }
    }
    if let Some(effort) = &record.effort {
        ensure!(
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()),
            "Telegram effort is invalid"
        );
    }
    if let Some(usage) = &record.last_turn_usage {
        validate_usage(usage)?;
    }
    validate_usage(&record.usage)?;
    Ok(())
}

fn validate_usage(usage: &UsageRecord) -> Result<()> {
    let total = usage
        .input_tokens
        .checked_add(usage.output_tokens)
        .and_then(|value| value.checked_add(usage.cache_read_tokens))
        .and_then(|value| value.checked_add(usage.cache_write_tokens))
        .context("Telegram usage counter overflow")?;
    ensure!(
        total == usage.total_tokens,
        "Telegram usage total does not match its counters"
    );
    Ok(())
}

fn open_record(path: &Path) -> Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "Telegram conversation record must be a regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() },
                    "Telegram conversation record must be owned by the current user"
                );
                ensure!(
                    metadata.nlink() == 1,
                    "Telegram conversation record must not have hard links"
                );
                ensure!(
                    metadata.mode() & 0o777 == 0o600,
                    "Telegram conversation record must have mode 0600"
                );
            }
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(Some(options.open(path)?))
}

fn atomic_write(path: &Path, payload: &[u8]) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file(),
            "Telegram conversation record must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.nlink() == 1,
                "Telegram conversation record has hard links"
            );
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() },
                "Telegram conversation record has the wrong owner"
            );
            ensure!(
                metadata.mode() & 0o777 == 0o600,
                "Telegram conversation record must have mode 0600"
            );
        }
    }
    let dir = path
        .parent()
        .context("Telegram conversation record has no parent")?;
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .context("missing Telegram record name")?
        .to_string_lossy();
    let temp = dir.join(format!(".{name}.tmp-{}-{unique}", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temp)?;
        file.write_all(payload)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
