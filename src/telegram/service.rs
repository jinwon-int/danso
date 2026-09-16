//! Telegram service wiring for B3.
//!
//! The service owns polling and command policy, while one small in-process
//! runner owns the boundary to app::run. A turn is never re-entered through
//! the CLI and no danso subprocess is launched for a message. The core tool
//! executor may still launch its normal bounded tool workers.

use super::{
    EFFORT_ENV, MODEL_ENV, PROVIDER_ENV, TelegramConfig, TelegramFoundation, Update, WORKSPACE_ENV,
    ensure_private_dir,
    store::{ConversationRecord, ConversationStore, UsageRecord},
};
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use danso_ops::{
    health::{
        HEALTH_SCHEMA_VERSION, HealthDocument, ProcessHealth, ServiceHealth, TelegramHealth,
        TurnOccupancy, WorkloadHealth,
    },
    status::ServiceState,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc, oneshot};

const JOURNALS_DIR: &str = "journals";
const DEFAULT_PROVIDER: &str = "anthropic";
const DEFAULT_MAX_TURNS: u32 = 48;
const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
const DEFAULT_PROVIDER_TIMEOUT_SECONDS: u64 = 180;
const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 900;
const DEFAULT_PROVIDER_RETRIES: u32 = 3;
const DEFAULT_HEARTBEAT_SECONDS: u64 = 60;
const DEFAULT_FOLLOWUP_CAP: usize = 5;
const MAX_HEARTBEAT_SECONDS: u64 = 3600;
const MAX_FOLLOWUP_CAP: u64 = 100;
const HEALTH_FILE_NAME: &str = "health.json";
const TASK_WALL_ENV: [&str; 6] = [
    "DANSO_TASK_WALL_SECONDS",
    "DANSO_TASK_TIMEOUT_SECONDS",
    "DANSO_TELEGRAM_TASK_WALL_SECONDS",
    "DANSO_TELEGRAM_TASK_TIMEOUT_SECONDS",
    "DANSO_TELEGRAM_TIMEOUT_SECONDS",
    "DANSO_TIMEOUT_SECONDS",
];
const TASK_STAGE_ENV: [&str; 2] = [
    "DANSO_TASK_STAGE_REQUESTS",
    "DANSO_TELEGRAM_TASK_STAGE_REQUESTS",
];
const TASK_REQUESTS_ENV: [&str; 2] = [
    "DANSO_TASK_MAX_REQUESTS",
    "DANSO_TELEGRAM_TASK_MAX_REQUESTS",
];
const TASK_TOKENS_ENV: [&str; 2] = ["DANSO_TASK_MAX_TOKENS", "DANSO_TELEGRAM_TASK_MAX_TOKENS"];
const TASK_REPEAT_ENV: [&str; 2] = [
    "DANSO_TASK_REPEAT_LIMIT",
    "DANSO_TELEGRAM_TASK_REPEAT_LIMIT",
];

const TASK_USAGE: &str = "Usage: /task <prompt>";
const TASK_PAUSE_USAGE: &str = "Usage: /task_pause";
const TASK_RESUME_USAGE: &str = "Usage: /task_resume";
const DISTILL_USAGE: &str = "Usage: /distill";
const MEMORY_PROMOTE_USAGE: &str = "Usage: /memory_promote <fact-id>";
const RESUME_USAGE: &str = "Usage: /resume";
const HISTORY_USAGE: &str = "Usage: /history";
const TASK_PAUSE_REQUESTED: &str = "pause requested; takes effect at the next safe checkpoint";

/// The command has deliberately no CLI flags. Runtime and provider
/// configuration comes from the documented environment so the service cannot
/// accidentally diverge from a normal Danso run.
#[derive(Debug, Parser)]
#[command(
    name = "danso telegram",
    about = "Run the Telegram single-turn service using environment configuration"
)]
pub struct TelegramArgs {}

#[derive(Clone)]
struct RunSettings {
    provider: String,
    default_model: String,
    default_effort: Option<String>,
    workspace: PathBuf,
    trust_project: bool,
    no_tools: bool,
    max_turns: u32,
    timeout_seconds: u64,
    provider_timeout_seconds: u64,
    tool_timeout_seconds: u64,
    provider_retries: u32,
    max_output_tokens: Option<u32>,
    compact_at_bytes: Option<usize>,
    heartbeat_seconds: u64,
    followup_cap: usize,
    memory_root: PathBuf,
    memory_scope: String,
    task_limits: crate::long_task::Limits,
}

impl RunSettings {
    fn from_env() -> Result<Self> {
        let provider = first_env(&[PROVIDER_ENV, "DANSO_PROVIDER"])?
            .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());
        ensure!(
            ["anthropic", "openai", "openai-codex", "glm"].contains(&provider.as_str()),
            "unsupported Telegram provider"
        );

        let mut model_names = vec![MODEL_ENV, "DANSO_MODEL"];
        match provider.as_str() {
            "anthropic" => model_names.push("DANSO_ANTHROPIC_MODEL"),
            "openai" => model_names.push("DANSO_OPENAI_MODEL"),
            "openai-codex" => {
                model_names.push("DANSO_OPENAI_CODEX_MODEL");
                model_names.push("DANSO_OPENAI_MODEL");
            }
            "glm" => model_names.push("DANSO_GLM_MODEL"),
            _ => unreachable!("provider was validated above"),
        }
        let default_model = first_env(&model_names)?
            .context("DANSO_TELEGRAM_MODEL or the normal Danso model environment is required")?;
        validate_model(&default_model)?;

        let default_effort = first_env(&[EFFORT_ENV, "DANSO_REASONING_EFFORT"])?;
        validate_effort(default_effort.as_deref(), &provider)?;

        let workspace = first_env(&[WORKSPACE_ENV, "DANSO_WORKSPACE"])?
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir().context("resolve Telegram workspace")?);
        ensure!(
            workspace.is_absolute(),
            "Telegram workspace must be an absolute path"
        );
        let workspace = workspace
            .canonicalize()
            .context("Telegram workspace does not exist")?;
        ensure!(
            workspace.is_dir() && workspace.parent().is_some(),
            "Telegram workspace must be a non-root directory"
        );

        let max_turns = configured_u32(
            &["DANSO_TELEGRAM_MAX_TURNS", "DANSO_MAX_TURNS"],
            DEFAULT_MAX_TURNS,
            1,
            128,
        )?;
        let timeout_seconds = configured_u64(
            &["DANSO_TELEGRAM_TIMEOUT_SECONDS", "DANSO_TIMEOUT_SECONDS"],
            DEFAULT_TIMEOUT_SECONDS,
            1,
            3600,
        )?;
        let provider_timeout_seconds = configured_u64(
            &[
                "DANSO_TELEGRAM_PROVIDER_TIMEOUT_SECONDS",
                "DANSO_PROVIDER_TIMEOUT_SECONDS",
            ],
            DEFAULT_PROVIDER_TIMEOUT_SECONDS,
            1,
            300,
        )?;
        let tool_timeout_seconds = configured_u64(
            &[
                "DANSO_TELEGRAM_TOOL_TIMEOUT_SECONDS",
                "DANSO_TOOL_TIMEOUT_SECONDS",
            ],
            DEFAULT_TOOL_TIMEOUT_SECONDS,
            1,
            crate::tools::HOST_TOOL_TIMEOUT_MAX_SECONDS,
        )?;
        let provider_retries = configured_u32(
            &["DANSO_TELEGRAM_PROVIDER_RETRIES", "DANSO_PROVIDER_RETRIES"],
            DEFAULT_PROVIDER_RETRIES,
            0,
            5,
        )?;
        let max_output_tokens = configured_optional_u32(&[
            "DANSO_TELEGRAM_MAX_OUTPUT_TOKENS",
            "DANSO_MAX_OUTPUT_TOKENS",
        ])?;
        let compact_at_bytes = configured_optional_usize(&[
            "DANSO_TELEGRAM_COMPACT_AT_BYTES",
            "DANSO_COMPACT_AT_BYTES",
        ])?;
        let heartbeat_seconds = configured_u64(
            &["DANSO_TELEGRAM_HEARTBEAT_SECONDS"],
            DEFAULT_HEARTBEAT_SECONDS,
            0,
            MAX_HEARTBEAT_SECONDS,
        )?;
        let followup_cap = configured_u64(
            &["DANSO_TELEGRAM_FOLLOWUP_CAP"],
            DEFAULT_FOLLOWUP_CAP as u64,
            0,
            MAX_FOLLOWUP_CAP,
        )? as usize;
        let memory_scope =
            first_env(&["DANSO_TELEGRAM_MEMORY_SCOPE"])?.unwrap_or_else(|| "global".to_string());
        ensure!(
            crate::memory::valid_scope(&memory_scope),
            "DANSO_TELEGRAM_MEMORY_SCOPE must be global, shared, or private-<32 hex>"
        );
        let memory_root = first_env(&["DANSO_MEMORY_DIR"])?
            .map(PathBuf::from)
            .unwrap_or_else(crate::memory::MemoryConfig::default_root);
        ensure!(
            memory_root.is_absolute(),
            "DANSO_MEMORY_DIR must be an absolute path"
        );
        let task_limits = task_limits_from_env()?;

        Ok(Self {
            provider,
            default_model,
            default_effort,
            workspace,
            trust_project: configured_bool(
                &["DANSO_TELEGRAM_TRUST_PROJECT", "DANSO_TRUST_PROJECT"],
                false,
            )?,
            no_tools: configured_bool(&["DANSO_TELEGRAM_NO_TOOLS", "DANSO_NO_TOOLS"], false)?,
            max_turns,
            timeout_seconds,
            provider_timeout_seconds,
            tool_timeout_seconds,
            provider_retries,
            max_output_tokens,
            compact_at_bytes,
            heartbeat_seconds,
            followup_cap,
            memory_root,
            memory_scope,
            task_limits,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn config(
        &self,
        prompt: String,
        session: PathBuf,
        model: String,
        effort: Option<String>,
        long_task: Option<crate::runtime::LongTaskRun>,
        pause_requested: Option<Arc<AtomicBool>>,
        cancellation_reason: Arc<AtomicU8>,
    ) -> crate::app::RunConfig {
        let timeout_seconds = long_task
            .map(|task| task.limits.wall_seconds)
            .unwrap_or(self.timeout_seconds);
        crate::app::RunConfig {
            prompt,
            cwd: self.workspace.clone(),
            session,
            model,
            provider: self.provider.clone(),
            reasoning_effort: effort.or_else(|| self.default_effort.clone()),
            trust_project: self.trust_project,
            no_tools: self.no_tools,
            system_context_file: None,
            memory: crate::memory::MemoryConfig {
                root: Some(self.memory_root.clone()),
                scope: self.memory_scope.clone(),
                max_bytes: crate::memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
                ..Default::default()
            },
            memory_refresh: crate::memory::RefreshMode::PerRun,
            memory_distill: crate::memory::DistillMode::Queue,
            backend: crate::app::Backend::Host,
            max_turns: self.max_turns,
            max_output_tokens: self.max_output_tokens,
            glm_thinking: None,
            glm_endpoint: None,
            provider_retries: self.provider_retries,
            continuation_limit: 0,
            stream_requests: false,
            report_progress: false,
            repeat_limit: 0,
            compact_at_bytes: self.compact_at_bytes,
            timeout_seconds,
            provider_timeout_seconds: self.provider_timeout_seconds,
            tool_timeout_seconds: self.tool_timeout_seconds,
            tool_home: None,
            long_task,
            task_progress: false,
            pause_requested,
            cancellation_reason: Some(cancellation_reason),
        }
    }
}

fn task_limits_from_env() -> Result<crate::long_task::Limits> {
    let limits = crate::long_task::Limits {
        wall_seconds: configured_u64(
            &TASK_WALL_ENV,
            crate::long_task::MAX_WALL_SECONDS,
            1,
            crate::long_task::MAX_WALL_SECONDS,
        )?,
        stage_requests: configured_u64(
            &TASK_STAGE_ENV,
            crate::long_task::DEFAULT_STAGE_REQUESTS,
            crate::long_task::MIN_STAGE_REQUESTS,
            crate::long_task::MAX_STAGE_REQUESTS,
        )?,
        max_requests: configured_u64(
            &TASK_REQUESTS_ENV,
            crate::long_task::DEFAULT_MAX_REQUESTS,
            crate::long_task::MIN_MAX_REQUESTS,
            crate::long_task::MAX_MAX_REQUESTS,
        )?,
        max_tokens: configured_u64(
            &TASK_TOKENS_ENV,
            crate::long_task::DEFAULT_MAX_TOKENS,
            crate::long_task::MIN_MAX_TOKENS,
            crate::long_task::MAX_MAX_TOKENS,
        )?,
        repeat_limit: configured_u64(
            &TASK_REPEAT_ENV,
            crate::long_task::DEFAULT_REPEAT_LIMIT,
            crate::long_task::MIN_REPEAT_LIMIT,
            crate::long_task::MAX_REPEAT_LIMIT,
        )?,
    };
    limits.validate()?;
    Ok(limits)
}

fn first_env(names: &[&str]) -> Result<Option<String>> {
    for name in names {
        match std::env::var(name) {
            Ok(value) => return Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{name} must be valid UTF-8")
            }
        }
    }
    Ok(None)
}

fn configured_u64(names: &[&str], default: u64, min: u64, max: u64) -> Result<u64> {
    let Some(raw) = first_env(names)? else {
        return Ok(default);
    };
    ensure!(!raw.trim().is_empty(), "{} must not be empty", names[0]);
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{} must be an integer", names[0]))?;
    ensure!(
        (min..=max).contains(&value),
        "{} must be {min}..={max}",
        names[0]
    );
    Ok(value)
}

fn configured_u32(names: &[&str], default: u32, min: u32, max: u32) -> Result<u32> {
    let Some(raw) = first_env(names)? else {
        return Ok(default);
    };
    ensure!(!raw.trim().is_empty(), "{} must not be empty", names[0]);
    let value = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{} must be an integer", names[0]))?;
    ensure!(
        (min..=max).contains(&value),
        "{} must be {min}..={max}",
        names[0]
    );
    Ok(value)
}

fn configured_optional_u32(names: &[&str]) -> Result<Option<u32>> {
    let Some(raw) = first_env(names)? else {
        return Ok(None);
    };
    ensure!(!raw.trim().is_empty(), "{} must not be empty", names[0]);
    Ok(Some(raw.trim().parse::<u32>().map_err(|_| {
        anyhow::anyhow!("{} must be an integer", names[0])
    })?))
}

fn configured_optional_usize(names: &[&str]) -> Result<Option<usize>> {
    let Some(raw) = first_env(names)? else {
        return Ok(None);
    };
    ensure!(!raw.trim().is_empty(), "{} must not be empty", names[0]);
    Ok(Some(raw.trim().parse::<usize>().map_err(|_| {
        anyhow::anyhow!("{} must be an integer", names[0])
    })?))
}

fn configured_bool(names: &[&str], default: bool) -> Result<bool> {
    let Some(raw) = first_env(names)? else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("{} must be a boolean", names[0]),
    }
}

fn atomic_write_health(path: &std::path::Path, payload: &[u8]) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file(),
            "Telegram health file must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o777 == 0o600,
                "Telegram health file has unsafe ownership or permissions"
            );
        }
    }
    let parent = path
        .parent()
        .context("Telegram health file has no parent")?;
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .context("Telegram health file has no name")?
        .to_string_lossy();
    let temp = parent.join(format!(".{name}.tmp-{}-{unique}", std::process::id()));
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
        fs::File::open(parent)?.sync_all()?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn validate_model(model: &str) -> Result<()> {
    ensure!(!model.trim().is_empty(), "Telegram model must not be empty");
    ensure!(model.len() <= 4096, "Telegram model is too long");
    ensure!(
        model
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace()),
        "Telegram model must be one non-whitespace token"
    );
    Ok(())
}

fn validate_effort(effort: Option<&str>, provider: &str) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };
    ensure!(
        ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort),
        "Telegram effort is invalid"
    );
    ensure!(
        provider != "anthropic",
        "reasoning effort is unsupported by the Anthropic adapter"
    );
    Ok(())
}

struct TelegramSink {
    final_text: Option<String>,
    paused: bool,
    progress: mpsc::UnboundedSender<ProgressEvent>,
    tool_started: Option<(String, Instant)>,
}

impl TelegramSink {
    fn new(progress: mpsc::UnboundedSender<ProgressEvent>) -> Self {
        Self {
            final_text: None,
            paused: false,
            progress,
            tool_started: None,
        }
    }

    fn final_text(self) -> Result<String> {
        self.final_text
            .context("completed Telegram turn did not produce a final answer")
    }

    fn paused(&self) -> bool {
        self.paused
    }
}

impl crate::contracts::EventSink for TelegramSink {
    fn emit(&mut self, event: crate::contracts::Event<'_>) -> Result<()> {
        match event {
            crate::contracts::Event::FinalAnswer(message) => {
                let mut text = crate::contracts::text_blocks(message).join("");
                if text.is_empty()
                    && let Some(content) = message["content"].as_str()
                {
                    text = content.to_string();
                }
                ensure!(!text.is_empty(), "final Telegram answer is empty");
                self.final_text = Some(text);
            }
            crate::contracts::Event::ToolStarted(name) => {
                self.tool_started = Some((safe_tool_name(name), Instant::now()));
            }
            crate::contracts::Event::ToolSettled { .. } => {
                if let Some((name, started)) = self.tool_started.take() {
                    let elapsed_seconds = started.elapsed().as_secs();
                    let _ = self.progress.send(ProgressEvent::ToolFinished {
                        name,
                        elapsed_seconds,
                    });
                }
            }
            crate::contracts::Event::Task(progress) if progress["state"] == "paused" => {
                self.paused = true;
            }
            _ => {}
        }
        Ok(())
    }
}

struct TurnOutcome {
    text: Option<String>,
    usage: UsageRecord,
    paused: bool,
}

enum ProgressEvent {
    ToolFinished { name: String, elapsed_seconds: u64 },
}

struct ActiveTurn {
    cancel: Arc<Notify>,
    pause_requested: Arc<AtomicBool>,
    cancellation_reason: Arc<AtomicU8>,
    cancelled: AtomicBool,
    completed: AtomicBool,
    progress_message_id: AtomicI64,
    session_id: String,
    long_task: bool,
    started_at: Instant,
}

impl ActiveTurn {
    fn new(session_id: String, long_task: bool) -> Arc<Self> {
        Arc::new(Self {
            cancel: Arc::new(Notify::new()),
            pause_requested: Arc::new(AtomicBool::new(false)),
            cancellation_reason: Arc::new(AtomicU8::new(0)),
            cancelled: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            progress_message_id: AtomicI64::new(0),
            session_id,
            long_task,
            started_at: Instant::now(),
        })
    }

    fn interrupt(&self) {
        if !self.completed.load(Ordering::Acquire) {
            self.cancelled.store(true, Ordering::Release);
            self.cancellation_reason.store(1, Ordering::Release);
        }
        // notify_one retains a permit if the turn thread has not reached its
        // select yet; notify_waiters would lose an early /stop notification.
        self.cancel.notify_one();
    }

    fn request_pause(&self) {
        if !self.completed.load(Ordering::Acquire) {
            self.pause_requested.store(true, Ordering::Release);
        }
    }

    fn clear_pause_request(&self) {
        self.pause_requested.store(false, Ordering::Release);
    }

    fn is_running(&self) -> bool {
        !self.completed.load(Ordering::Acquire)
    }

    fn set_progress_message(&self, message_id: i64) {
        self.progress_message_id
            .store(message_id, Ordering::Release);
    }

    fn progress_message(&self) -> Option<i64> {
        match self.progress_message_id.load(Ordering::Acquire) {
            0 => None,
            message_id => Some(message_id),
        }
    }
}

struct TurnHandle {
    receiver: oneshot::Receiver<Result<TurnOutcome>>,
    progress: mpsc::UnboundedReceiver<ProgressEvent>,
}

struct PreparedTurn {
    active: Arc<ActiveTurn>,
    session_id: String,
    model: String,
    effort: Option<String>,
    prompt: String,
    long_task: Option<crate::runtime::LongTaskRun>,
}

/// A root-owned in-process adapter. danso-runtime exposes the same
/// provider-neutral runner for external embedders; the root binary cannot
/// depend on that crate because that crate intentionally depends on this
/// core package. This adapter keeps the Telegram binary on the same
/// app::run path without introducing a cyclic Cargo dependency.
struct InProcessRunner {
    journals: PathBuf,
    settings: RunSettings,
}

impl InProcessRunner {
    fn new(journals: PathBuf, settings: RunSettings) -> Result<Self> {
        ensure_private_dir(&journals)?;
        ensure!(
            !journals.starts_with(&settings.workspace),
            "Telegram journals must be outside the workspace"
        );
        Ok(Self { journals, settings })
    }

    fn journal_path(&self, session_id: &str) -> PathBuf {
        self.journals.join(format!("{session_id}.jsonl"))
    }

    fn require_session_file(&self, session_id: &str) -> Result<PathBuf> {
        let parsed =
            uuid::Uuid::parse_str(session_id).context("invalid Telegram session pointer")?;
        ensure!(
            parsed.hyphenated().to_string() == session_id,
            "invalid Telegram session pointer"
        );
        let path = self.journal_path(session_id);
        let metadata =
            std::fs::symlink_metadata(&path).context("Telegram session pointer has no journal")?;
        ensure!(
            metadata.is_file(),
            "Telegram session journal must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.nlink() == 1
                    && metadata.mode() & 0o077 == 0,
                "Telegram session journal has unsafe ownership or permissions"
            );
        }
        Ok(path)
    }

    fn new_session(&self) -> Result<String> {
        for _ in 0..8 {
            let session_id = uuid::Uuid::new_v4().hyphenated().to_string();
            let path = self.journal_path(&session_id);
            if std::fs::symlink_metadata(&path).is_ok() {
                continue;
            }
            let session = crate::session::Session::open(&path, &self.settings.workspace)?;
            drop(session);
            return Ok(session_id);
        }
        bail!("could not allocate a Telegram session")
    }

    fn session_status(&self, session_id: &str) -> Result<serde_json::Value> {
        let journal = self.require_session_file(session_id)?;
        crate::session::Session::read_status(&journal)
    }

    /// Read only the two journal timestamps needed for `/history`. The
    /// parser never retains or returns message values, and malformed stamps
    /// become the fixed `unknown` label rather than being echoed.
    fn session_timestamps(&self, session_id: &str) -> Result<SessionTimestamps> {
        let journal = self.require_session_file(session_id)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(journal)?;
        ensure!(file.metadata()?.len() <= 16 * 1024 * 1024);
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut total_bytes = 0_u64;
        let mut started_at = None;
        let mut updated_at = None;
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            total_bytes = total_bytes
                .checked_add(read as u64)
                .context("Telegram session journal size overflow")?;
            ensure!(total_bytes <= 16 * 1024 * 1024);
            let entry: JournalTimestamp<'_> = serde_json::from_str(&line)?;
            let Some(raw) = entry.timestamp else {
                continue;
            };
            let timestamp = safe_timestamp(raw);
            if started_at.is_none() {
                started_at = Some(timestamp.clone());
            }
            updated_at = Some(timestamp);
        }
        Ok(SessionTimestamps {
            started_at: started_at.unwrap_or_else(|| "unknown".to_string()),
            updated_at: updated_at.unwrap_or_else(|| "unknown".to_string()),
        })
    }

    fn start_turn(
        &self,
        active: Arc<ActiveTurn>,
        session_id: String,
        model: String,
        effort: Option<String>,
        prompt: String,
        long_task: Option<crate::runtime::LongTaskRun>,
    ) -> Result<TurnHandle> {
        let parsed =
            uuid::Uuid::parse_str(&session_id).context("invalid Telegram session pointer")?;
        ensure!(
            parsed.hyphenated().to_string() == session_id,
            "invalid Telegram session pointer"
        );
        validate_model(&model)?;
        validate_effort(effort.as_deref(), &self.settings.provider)?;
        let journal = self.require_session_file(&session_id)?;
        let config = self.settings.config(
            prompt,
            journal,
            model,
            effort,
            long_task,
            Some(Arc::clone(&active.pause_requested)),
            Arc::clone(&active.cancellation_reason),
        );
        let (sender, receiver) = oneshot::channel();
        let (progress_sender, progress_receiver) = mpsc::unbounded_channel();
        let cancel = Arc::clone(&active.cancel);
        let completed = Arc::clone(&active);
        let thread_name = format!("danso-telegram-turn-{}", &session_id[..8]);
        let _thread = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_turn(config, cancel, progress_sender)
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "Telegram turn panicked; journal retained. No automatic replay."
                    ))
                });
                completed.completed.store(true, Ordering::Release);
                let _ = sender.send(result);
            })
            .context("could not start Telegram turn")?;
        Ok(TurnHandle {
            receiver,
            progress: progress_receiver,
        })
    }
}

struct SessionTimestamps {
    started_at: String,
    updated_at: String,
}

#[derive(serde::Deserialize)]
struct JournalTimestamp<'a> {
    #[serde(borrow)]
    timestamp: Option<&'a str>,
}

fn safe_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

fn run_turn(
    config: crate::app::RunConfig,
    cancel: Arc<Notify>,
    progress: mpsc::UnboundedSender<ProgressEvent>,
) -> Result<TurnOutcome> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start Telegram turn runtime")?;
    let mut usage = crate::usage::Usage::default();
    let mut sink = TelegramSink::new(progress);
    let result = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.notified() => Err(anyhow::anyhow!("Telegram turn cancelled")),
            result = tokio::time::timeout(
                Duration::from_secs(config.timeout_seconds),
                crate::app::run(&config, &mut sink, &mut usage),
            ) => match result {
                Ok(result) => result,
                Err(_) => {
                    if let Some(reason) = &config.cancellation_reason {
                        let _ = reason.compare_exchange(
                            0,
                            3,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                    Err(anyhow::anyhow!("Telegram turn timed out"))
                }
            },
        }
    });
    let paused = sink.paused();
    if !paused {
        result?;
    }
    let text = if paused {
        None
    } else {
        Some(sink.final_text()?)
    };
    let usage = UsageRecord::from_summary(&usage.summary())?;
    Ok(TurnOutcome {
        text,
        usage,
        paused,
    })
}

struct ChatState {
    record: Mutex<()>,
    active: Mutex<Option<Arc<ActiveTurn>>>,
}

impl ChatState {
    fn new() -> Self {
        Self {
            record: Mutex::new(()),
            active: Mutex::new(None),
        }
    }
}

/// The outcome of the most recent poll, so the health document can report
/// *serving* rather than merely *running*.
///
/// A process that is up but has failed every poll for ten minutes is not
/// available, and a consumer that only sees `last_poll_at` advancing cannot
/// tell the difference — the timestamp moves on failures too.
#[derive(Default)]
struct PollOutcome {
    last_ok_at: Option<String>,
    last_error_at: Option<String>,
    consecutive_failures: u64,
}

struct HealthFile {
    path: PathBuf,
    started_at: String,
    last_poll_at: Mutex<String>,
    poll: Mutex<PollOutcome>,
}

impl HealthFile {
    fn new(data_dir: &std::path::Path) -> Self {
        Self {
            path: data_dir.join(HEALTH_FILE_NAME),
            started_at: chrono::Utc::now().to_rfc3339(),
            last_poll_at: Mutex::new(String::new()),
            poll: Mutex::new(PollOutcome::default()),
        }
    }

    /// Record whether a poll returned updates or failed, before the document is
    /// written for that wakeup.
    fn record_poll(&self, at: &str, ok: bool) {
        let mut poll = self.poll.lock().expect("Telegram health poll lock");
        if ok {
            poll.last_ok_at = Some(at.to_string());
            poll.consecutive_failures = 0;
        } else {
            poll.last_error_at = Some(at.to_string());
            poll.consecutive_failures = poll.consecutive_failures.saturating_add(1);
        }
    }

    fn write(
        &self,
        conversations: &ConversationStore,
        chats: &Mutex<HashMap<i64, Arc<ChatState>>>,
        last_poll_at: Option<&str>,
    ) -> Result<()> {
        let last_poll_at = {
            let mut current = self.last_poll_at.lock().expect("Telegram health lock");
            if let Some(last_poll_at) = last_poll_at {
                *current = last_poll_at.to_string();
            }
            if current.is_empty() {
                *current = self.started_at.clone();
            }
            current.clone()
        };
        let records = conversations.load_all()?;
        let states = chats.lock().expect("Telegram chat map lock");
        let mut active_turn_count = 0_u64;
        let mut waiting_for_turn = 0_u64;
        let mut oldest_active = None::<std::time::Duration>;
        let mut queued_counts = BTreeMap::new();
        for record in records {
            let active = states
                .get(&record.chat_id)
                .and_then(|state| state.active.lock().ok())
                .and_then(|active| active.as_ref().cloned())
                .filter(|active| !active.completed.load(Ordering::Acquire));
            if let Some(active) = &active {
                let age = active.started_at.elapsed();
                oldest_active =
                    Some(oldest_active.map_or(age, |oldest: std::time::Duration| oldest.max(age)));
            }
            if active.is_some() || record.turn_active {
                active_turn_count = active_turn_count.saturating_add(1);
            }
            let queued = record.follow_up_queue.len();
            waiting_for_turn = waiting_for_turn.saturating_add(queued as u64);
            queued_counts.insert(record.chat_id.to_string(), queued);
        }
        drop(states);
        let (last_ok_at, last_error_at, consecutive_failures) = {
            let poll = self.poll.lock().expect("Telegram health poll lock");
            (
                poll.last_ok_at.clone(),
                poll.last_error_at.clone(),
                poll.consecutive_failures,
            )
        };
        // Up is not the same as serving: a process whose every poll fails is
        // degraded even though it is running and refreshing this document.
        let state = if consecutive_failures == 0 {
            ServiceState::Available
        } else {
            ServiceState::Degraded
        };
        let reason = (consecutive_failures > 0)
            .then(|| format!("{consecutive_failures} consecutive polling failures"));
        let now = chrono::Utc::now().to_rfc3339();
        let document = HealthDocument {
            schema_version: HEALTH_SCHEMA_VERSION,
            started_at: self.started_at.clone(),
            last_poll_at,
            active_turn_count,
            queued_counts,
            service_pid: std::process::id(),
            process: ProcessHealth {
                pid: std::process::id(),
                started_at: self.started_at.clone(),
                mode: danso_ops::health::run_mode(),
            },
            service: ServiceHealth { state, reason },
            telegram: TelegramHealth {
                state,
                last_ok_at,
                last_error_at,
                consecutive_failures,
            },
            workload: WorkloadHealth {
                active_requests: active_turn_count,
                waiting_for_turn,
                turn_occupancy: if active_turn_count == 0 {
                    TurnOccupancy::Idle
                } else {
                    TurnOccupancy::Occupied
                },
                oldest_request_age_seconds: oldest_active.map(|age| age.as_secs()),
            },
            runtime_generation: danso_ops::generation::current().clone(),
            updated_at: now,
        };
        atomic_write_health(&self.path, &serde_json::to_vec(&document)?)
    }
}

/// What a drain observed. `still_running > 0` means the inner budget expired
/// with work outstanding; the journals are retained and nothing is replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub started_with: usize,
    pub still_running: usize,
}

impl DrainReport {
    pub fn is_complete(self) -> bool {
        self.still_running == 0
    }
}

struct ServiceInner {
    api: super::BotApi,
    access: super::AccessControl,
    conversations: ConversationStore,
    /// Held for the entire lifetime of the service, including all turn
    /// threads. It must never be replaced by a stale-file cleanup.
    _token_lock: super::TokenLock,
    poll_timeout_seconds: u64,
    settings: RunSettings,
    runner: Arc<InProcessRunner>,
    health: HealthFile,
    chats: Mutex<HashMap<i64, Arc<ChatState>>>,
    startup_recovered: tokio::sync::OnceCell<()>,
}

/// One process-wide Telegram consumer and its per-chat turn state.
#[derive(Clone)]
pub struct TelegramService {
    inner: Arc<ServiceInner>,
}

impl TelegramService {
    pub fn from_env() -> Result<Self> {
        let telegram = TelegramConfig::from_env()?;
        let settings = RunSettings::from_env()?;
        Self::from_config(telegram, settings)
    }

    fn from_config(telegram: TelegramConfig, settings: RunSettings) -> Result<Self> {
        let foundation = TelegramFoundation::from_config(telegram)?;
        let journals = foundation.conversations.data_dir().join(JOURNALS_DIR);
        let data_dir = foundation.conversations.data_dir().to_path_buf();
        ensure_private_dir(&journals)?;
        let runner = Arc::new(InProcessRunner::new(journals, settings.clone())?);
        let inner = ServiceInner {
            api: foundation.api,
            access: foundation.access,
            conversations: foundation.conversations,
            _token_lock: foundation.token_lock,
            poll_timeout_seconds: foundation.poll_timeout_seconds,
            settings,
            runner,
            health: HealthFile::new(&data_dir),
            chats: Mutex::new(HashMap::new()),
            startup_recovered: tokio::sync::OnceCell::const_new(),
        };
        let service = Self {
            inner: Arc::new(inner),
        };
        service.refresh_health(None)?;
        Ok(service)
    }

    fn chat(&self, chat_id: i64) -> Arc<ChatState> {
        let mut chats = self.inner.chats.lock().expect("Telegram chat map lock");
        chats
            .entry(chat_id)
            .or_insert_with(|| Arc::new(ChatState::new()))
            .clone()
    }

    fn refresh_health(&self, last_poll_at: Option<&str>) -> Result<()> {
        self.inner
            .health
            .write(&self.inner.conversations, &self.inner.chats, last_poll_at)
    }

    fn save_record(&self, record: &ConversationRecord) -> Result<()> {
        self.inner.conversations.save(record)?;
        self.refresh_health(None)
    }

    async fn ensure_recovered(&self) -> Result<()> {
        self.inner
            .startup_recovered
            .get_or_try_init(|| async {
                self.recover_orphans().await?;
                Ok::<(), anyhow::Error>(())
            })
            .await
            .map(|_| ())
    }

    /// Run the long-poll service. Polling remains live while a turn runs so
    /// /stop can reach the cancellation handle.
    pub async fn run(self) -> Result<()> {
        self.run_loop(None).await
    }

    /// Run until an application-owned notification arrives. The production
    /// command uses `run`; this bounded form keeps loopback service tests and
    /// orderly embedding shutdowns from needing to kill a polling task.
    pub async fn run_until(self, shutdown: Arc<Notify>) -> Result<()> {
        self.run_loop(Some(shutdown)).await
    }

    /// Run until `shutdown` fires, then drain active turns within `budget`.
    ///
    /// This is the SIGTERM path. Leaving the loop stops admitting new turns;
    /// draining gives the ones already running a bounded chance to finish
    /// before teardown. The budget is the *inner* 45s tier — the caller still
    /// owns the outer wall clock, and the difference between them is what pays
    /// for teardown.
    pub async fn run_until_drained(
        self,
        shutdown: Arc<Notify>,
        budget: Duration,
    ) -> Result<DrainReport> {
        // `TelegramService` is an `Arc` handle, so the clone shares one service.
        // The drain must run even when the loop ended with an error: turns are
        // already in flight and a failed poll is no reason to abandon them.
        let draining = self.clone();
        let result = self.run_loop(Some(Arc::clone(&shutdown))).await;
        let report = draining.drain(budget).await;
        result.map(|()| report)
    }

    /// Interrupt every running turn and wait for them, bounded by `budget`.
    ///
    /// Turns run on their own threads and publish completion through
    /// `ActiveTurn::completed`, so this polls that flag rather than joining:
    /// a turn wedged in a tool call must not make the drain itself unbounded.
    pub async fn drain(&self, budget: Duration) -> DrainReport {
        let active: Vec<Arc<ActiveTurn>> = {
            let chats = self.inner.chats.lock().expect("Telegram chat map lock");
            chats
                .values()
                .filter_map(|state| state.active.lock().ok()?.as_ref().cloned())
                .filter(|turn| turn.is_running())
                .collect()
        };
        let started_with = active.len();
        for turn in &active {
            // One interrupt per turn. `interrupt` records the cancellation
            // reason and notifies; repeating it would not make a wedged tool
            // return any sooner.
            turn.interrupt();
        }
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if active.iter().all(|turn| !turn.is_running()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let still_running = active.iter().filter(|turn| turn.is_running()).count();
        // Publish the final state so a `status` taken during teardown reflects
        // what actually happened rather than the last poll's picture.
        let _ = self.refresh_health(None);
        DrainReport {
            started_with,
            still_running,
        }
    }

    async fn run_loop(self, shutdown: Option<Arc<Notify>>) -> Result<()> {
        self.ensure_recovered().await?;
        let saved_offset = self.inner.conversations.load_poll_offset()?;
        let mut poller = self
            .inner
            .api
            .poller(saved_offset, self.inner.poll_timeout_seconds)?;
        let mut committed_offset = saved_offset;
        let mut reconnect_attempt = 0_u32;
        loop {
            let polled = match &shutdown {
                Some(shutdown) => {
                    tokio::select! {
                        _ = shutdown.notified() => return Ok(()),
                        result = poller.next() => result,
                    }
                }
                None => poller.next().await,
            };
            let poll_time = chrono::Utc::now().to_rfc3339();
            // Record the outcome before publishing: `last_poll_at` advances on
            // failures too, so on its own it cannot distinguish a serving
            // service from one that has been failing every poll.
            self.inner.health.record_poll(&poll_time, polled.is_ok());
            self.refresh_health(Some(&poll_time))?;
            let updates = match polled {
                Ok(updates) => {
                    reconnect_attempt = 0;
                    updates
                }
                Err(_) => {
                    eprintln!("telegram polling failed; reconnecting");
                    let delay = poll_retry_delay(reconnect_attempt);
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    match &shutdown {
                        Some(shutdown) => {
                            tokio::select! {
                                _ = shutdown.notified() => return Ok(()),
                                _ = tokio::time::sleep(delay) => {}
                            }
                        }
                        None => tokio::time::sleep(delay).await,
                    }
                    continue;
                }
            };
            for update in updates {
                if self.handle_update(update.clone()).await.is_err() {
                    eprintln!("telegram update handling failed");
                    poller.set_offset(committed_offset);
                    break;
                }
                let next = update
                    .update_id
                    .checked_add(1)
                    .context("Telegram update id overflow")?;
                self.inner.conversations.save_poll_offset(next)?;
                committed_offset = Some(next);
            }
        }
    }

    async fn recover_orphans(&self) -> Result<()> {
        const RESTART_NOTICE: &str = "Service restarted. The prior turn did not complete; its journal was preserved and will not be replayed.";
        let records = self.inner.conversations.load_all()?;
        let mut queued_chats = Vec::new();
        for record in records {
            let state = self.chat(record.chat_id);
            let task_status = record
                .active_task
                .as_ref()
                .and_then(|task| self.inner.runner.session_status(&task.session_pointer).ok());
            let task_terminal = task_status.as_ref().is_some_and(|status| {
                matches!(
                    status["state"].as_str(),
                    Some("not_long_task" | "completed" | "failed")
                )
            });
            if record.turn_active || task_terminal {
                if record.turn_active && !task_terminal {
                    if let Some(message_id) = record.progress_message_id {
                        if self
                            .inner
                            .api
                            .edit_message_text(record.chat_id, message_id, RESTART_NOTICE)
                            .await
                            .is_err()
                            && self.reply(record.chat_id, RESTART_NOTICE).await.is_err()
                        {
                            eprintln!("telegram restart notice delivery failed");
                        }
                    } else if self.reply(record.chat_id, RESTART_NOTICE).await.is_err() {
                        eprintln!("telegram restart notice delivery failed");
                    }
                }
                let _record = state.record.lock().expect("Telegram record lock");
                let mut current = self.load_record(record.chat_id)?;
                current.clear_turn_active();
                if task_terminal {
                    current.clear_active_task();
                }
                self.save_record(&current)?;
            }
            // A recorded task must remain an explicit operator decision. Do
            // not consume a queued prompt behind a paused or unresolved task
            // during restart recovery; `/task_resume` deliberately refuses
            // until the queue is empty.
            if !record.follow_up_queue.is_empty() && (record.active_task.is_none() || task_terminal)
            {
                queued_chats.push(record.chat_id);
            }
        }
        for chat_id in queued_chats {
            let state = self.chat(chat_id);
            let idle = state
                .active
                .lock()
                .expect("Telegram active-turn lock")
                .is_none();
            if idle && self.start_queued_turn(chat_id, state).await.is_err() {
                eprintln!("telegram queued turn recovery failed");
            }
        }
        self.refresh_health(None)
    }

    async fn start_queued_turn(&self, chat_id: i64, state: Arc<ChatState>) -> Result<()> {
        let prepared = {
            let _record = state.record.lock().expect("Telegram record lock");
            if state
                .active
                .lock()
                .expect("Telegram active-turn lock")
                .is_some()
            {
                return Ok(());
            }
            let mut record = self.load_record(chat_id)?;
            let Some(prompt) = record.follow_up_queue.first().cloned() else {
                return Ok(());
            };
            record.follow_up_queue.remove(0);
            let prepared = match self.prepare_turn(&state, &mut record, prompt.clone()) {
                Ok(prepared) => prepared,
                Err(error) => {
                    record.follow_up_queue.insert(0, prompt);
                    let _ = self.save_record(&record);
                    return Err(error);
                }
            };
            if let Err(error) = self.save_record(&record) {
                *state.active.lock().expect("Telegram active-turn lock") = None;
                record.clear_turn_active();
                record.follow_up_queue.insert(0, prepared.prompt.clone());
                let _ = self.save_record(&record);
                return Err(error);
            }
            prepared
        };
        let retry_prompt = prepared.prompt.clone();
        self.launch_turn(chat_id, state, prepared, Some(retry_prompt))
            .await
    }

    /// Process one update. This is public for loopback/fake-Bot-API tests and
    /// keeps access admission before every command, store read, and reply.
    pub async fn handle_update(&self, update: Update) -> Result<()> {
        if !self.inner.access.authorize(&update) {
            return Ok(());
        }
        self.ensure_recovered().await?;
        let Some(chat_id) = update.chat_id() else {
            return Ok(());
        };
        let Some(text) = update.text() else {
            return self.mark_update(chat_id, update.update_id);
        };
        let state = self.chat(chat_id);
        if let Some(command) = parse_command(text) {
            return self
                .handle_command(chat_id, update.update_id, state, command)
                .await;
        }
        self.handle_prompt(chat_id, update.update_id, state, text.to_string())
            .await
    }

    fn load_record(&self, chat_id: i64) -> Result<ConversationRecord> {
        Ok(self
            .inner
            .conversations
            .load(chat_id)?
            .unwrap_or_else(|| ConversationRecord::new(chat_id, -1, None)))
    }

    fn memory_route(&self) -> Result<crate::memory::Route> {
        crate::memory::Route::new(
            &self.inner.settings.memory_root,
            &self.inner.settings.memory_scope,
        )
    }

    fn distill_reply(&self, record: &ConversationRecord) -> String {
        let Some(session_id) = &record.session_pointer else {
            return "No session journal is available for distillation.".to_string();
        };
        let Ok(session_path) = self.inner.runner.require_session_file(session_id) else {
            return "No session journal is available for distillation.".to_string();
        };
        let Ok(route) = self.memory_route() else {
            return "Distillation is unavailable.".to_string();
        };
        match crate::memory::distill::journal::enqueue(
            &route,
            &session_path,
            "explicit",
            chrono::Utc::now(),
        ) {
            Ok(crate::memory::distill::journal::EnqueueOutcome::Enqueued { job_id }) => {
                format!("Distill enqueued: job_id={job_id}.")
            }
            Ok(crate::memory::distill::journal::EnqueueOutcome::AlreadyPending { job_id }) => {
                format!("Distill already pending: job_id={job_id}.")
            }
            Ok(crate::memory::distill::journal::EnqueueOutcome::Skipped { reason }) => {
                format!("Distill skipped: reason={reason}.")
            }
            Err(_) => "Distillation is unavailable.".to_string(),
        }
    }

    fn memory_promote_reply(&self, fact_id: &str) -> String {
        let Ok(route) = self.memory_route() else {
            return "Memory promotion is unavailable.".to_string();
        };
        let result = crate::memory::promote::promote(
            route.root(),
            &self.inner.settings.memory_scope,
            fact_id,
            chrono::Utc::now(),
            crate::memory::paths::LOCK_TIMEOUT_DEFAULT_MS,
        );
        match result {
            Ok(result) if result.promoted => format!(
                "Memory fact promoted: destination_fact_id={} promotion_id={}",
                result.destination_fact_id, result.promotion_id
            ),
            Ok(result) => format!(
                "Memory fact already-promoted: destination_fact_id={} promotion_id={}",
                result.destination_fact_id, result.promotion_id
            ),
            Err(_) => "Memory promotion is unavailable.".to_string(),
        }
    }

    fn history_reply(&self, state: &Arc<ChatState>, record: &ConversationRecord) -> String {
        let Some(current) = &record.session_pointer else {
            return "No session history is available.".to_string();
        };
        let active = state
            .active
            .lock()
            .expect("Telegram active-turn lock")
            .as_ref()
            .is_some_and(|active| active.is_running());
        let mut sessions = Vec::with_capacity(1 + record.previous_sessions.len());
        sessions.push(("current", current.clone(), active));
        sessions.extend(
            record
                .previous_sessions
                .iter()
                .cloned()
                .map(|session| ("previous", session, false)),
        );
        let mut lines = vec!["Session history:".to_string()];
        for (position, (which, session_id, active)) in sessions.into_iter().enumerate() {
            let Ok(timestamps) = self.inner.runner.session_timestamps(&session_id) else {
                return "Session history is unavailable.".to_string();
            };
            let category = if active || (position == 0 && record.turn_active) {
                "active turn"
            } else if self
                .inner
                .runner
                .session_status(&session_id)
                .ok()
                .is_some_and(|status| status["state"] == "paused")
            {
                "paused task"
            } else {
                "idle"
            };
            lines.push(format!(
                "{which} {} — {category} — created={} updated={}",
                short_session_id(&session_id),
                timestamps.started_at,
                timestamps.updated_at
            ));
        }
        lines.join("\n")
    }

    fn mark_update(&self, chat_id: i64, update_id: i64) -> Result<()> {
        let state = self.chat(chat_id);
        let _record = state.record.lock().expect("Telegram record lock");
        let mut record = self.load_record(chat_id)?;
        if update_id <= record.last_update_id {
            return Ok(());
        }
        record.last_update_id = update_id;
        self.save_record(&record)
    }

    async fn handle_command(
        &self,
        chat_id: i64,
        update_id: i64,
        state: Arc<ChatState>,
        command: CommandInput,
    ) -> Result<()> {
        let (reply, prepared) = {
            let _record = state.record.lock().expect("Telegram record lock");
            let mut record = self.load_record(chat_id)?;
            if update_id <= record.last_update_id {
                return Ok(());
            }
            record.last_update_id = update_id;
            let mut prepared = None;
            let reply = match command.kind {
                CommandKind::Start => Some(if command.invalid_shape {
                    "Usage: /start".to_string()
                } else {
                    "Hello! You are authorized to use this Danso bot. \
                     Access is restricted to the configured Telegram allowlist. \
                     Send text for one turn, or use /new, /stop, /model, /effort, /usage, \
                     /task, /task_pause, /task_resume, /distill, /memory_promote, /resume, and /history."
                        .to_string()
                }),
                CommandKind::New => {
                    if command.invalid_shape {
                        Some("Usage: /new".to_string())
                    } else if state
                        .active
                        .lock()
                        .expect("Telegram active-turn lock")
                        .is_some()
                        || !record.follow_up_queue.is_empty()
                    {
                        Some(
                            "Cannot start a new session while a turn or follow-up queue is active; use /stop first."
                                .to_string(),
                        )
                    } else {
                        record.remember_current_session();
                        let session_id = self.inner.runner.new_session()?;
                        record.session_pointer = Some(session_id);
                        record.clear_active_task();
                        Some("Started a new Danso session.".to_string())
                    }
                }
                CommandKind::Stop => {
                    if command.invalid_shape {
                        Some("Usage: /stop".to_string())
                    } else {
                        let active = state
                            .active
                            .lock()
                            .expect("Telegram active-turn lock")
                            .clone();
                        let queued = !record.follow_up_queue.is_empty();
                        if let Some(active) = &active {
                            active.clear_pause_request();
                            active.interrupt();
                        }
                        record.follow_up_queue.clear();
                        Some(
                            if active
                                .as_ref()
                                .is_some_and(|active| active.is_running())
                            {
                                "Stopping the active turn. Its journal is preserved and will not be replayed."
                            } else if queued {
                                "Cleared the queued follow-up turns."
                            } else {
                                "No turn is currently running."
                            }
                            .to_string(),
                        )
                    }
                }
                CommandKind::Model => {
                    if command.invalid_shape {
                        Some("Usage: /model [model-name]".to_string())
                    } else if let Some(model) = command.argument {
                        match validate_model(&model) {
                            Ok(()) => {
                                record.provider = Some(self.inner.settings.provider.clone());
                                record.model = Some(model.clone());
                                Some(format!("Model set to {model}."))
                            }
                            Err(_) => Some(
                                "Model must be one non-whitespace token of at most 4096 bytes."
                                    .to_string(),
                            ),
                        }
                    } else {
                        Some(format!(
                            "Current model: {}",
                            record.effective_model(&self.inner.settings.default_model)
                        ))
                    }
                }
                CommandKind::Effort => {
                    if command.invalid_shape {
                        Some(
                            "Usage: /effort [none|minimal|low|medium|high|xhigh|max|default]"
                                .to_string(),
                        )
                    } else if let Some(argument) = command.argument {
                        let effort = argument.to_ascii_lowercase();
                        if effort == "default" {
                            record.provider = Some(self.inner.settings.provider.clone());
                            record.effort = None;
                            Some("Reasoning effort reset to the service default.".to_string())
                        } else {
                            match validate_effort(Some(&effort), &self.inner.settings.provider) {
                                Ok(()) => {
                                    record.provider = Some(self.inner.settings.provider.clone());
                                    record.effort = Some(effort.clone());
                                    Some(format!("Reasoning effort set to {effort}."))
                                }
                                Err(_) => Some(
                                    "That reasoning effort is not supported by the configured provider."
                                        .to_string(),
                                ),
                            }
                        }
                    } else {
                        let effort = record
                            .effective_effort(self.inner.settings.default_effort.as_deref())
                            .unwrap_or("default");
                        Some(format!("Current reasoning effort: {effort}."))
                    }
                }
                CommandKind::Usage => Some(if command.invalid_shape {
                    "Usage: /usage".to_string()
                } else {
                    format_usage(&record)
                }),
                CommandKind::Task => {
                    let prompt = command.argument;
                    if command.invalid_shape
                        || prompt
                            .as_deref()
                            .is_none_or(|prompt| prompt.trim().is_empty())
                    {
                        Some(TASK_USAGE.to_string())
                    } else if state
                        .active
                        .lock()
                        .expect("Telegram active-turn lock")
                        .is_some()
                        || !record.follow_up_queue.is_empty()
                    {
                        Some(
                            "Cannot start a long task while a turn or follow-up queue is active."
                                .to_string(),
                        )
                    } else if record.active_task.is_some() {
                        Some(
                            "A paused long task already exists; use /task_resume or /new first."
                                .to_string(),
                        )
                    } else {
                        prepared = Some(self.prepare_long_task_turn(
                            &state,
                            &mut record,
                            prompt.expect("validated task prompt"),
                        )?);
                        None
                    }
                }
                CommandKind::TaskPause => {
                    if command.invalid_shape {
                        Some(TASK_PAUSE_USAGE.to_string())
                    } else {
                        let active = state
                            .active
                            .lock()
                            .expect("Telegram active-turn lock")
                            .clone();
                        if active
                            .as_ref()
                            .is_some_and(|active| active.is_running() && active.long_task)
                        {
                            active.expect("checked above").request_pause();
                            Some(TASK_PAUSE_REQUESTED.to_string())
                        } else {
                            Some("No active long-task turn to pause.".to_string())
                        }
                    }
                }
                CommandKind::TaskResume => {
                    if command.invalid_shape {
                        Some(TASK_RESUME_USAGE.to_string())
                    } else if state
                        .active
                        .lock()
                        .expect("Telegram active-turn lock")
                        .is_some()
                        || !record.follow_up_queue.is_empty()
                    {
                        Some(
                            "Cannot resume a long task while a turn or follow-up queue is active."
                                .to_string(),
                        )
                    } else {
                        let task = record.active_task.clone();
                        let resumable = task.as_ref().is_some_and(|task| {
                            self.inner
                                .runner
                                .session_status(&task.session_pointer)
                                .ok()
                                .is_some_and(|status| {
                                    status["state"] == "paused" && status["resume_allowed"] == true
                                })
                        });
                        if !resumable {
                            Some("No paused long task is available to resume.".to_string())
                        } else {
                            let task = task.expect("resumable task has metadata");
                            record.session_pointer = Some(task.session_pointer.clone());
                            prepared = Some(self.prepare_resume_turn(&state, &mut record, task)?);
                            None
                        }
                    }
                }
                CommandKind::Distill => {
                    if command.invalid_shape {
                        Some(DISTILL_USAGE.to_string())
                    } else {
                        Some(self.distill_reply(&record))
                    }
                }
                CommandKind::MemoryPromote => {
                    if command.invalid_shape
                        || command
                            .argument
                            .as_deref()
                            .is_none_or(|fact| !valid_distill_id(fact))
                    {
                        Some(MEMORY_PROMOTE_USAGE.to_string())
                    } else if !self.inner.settings.memory_scope.starts_with("private-") {
                        Some("Memory promotion requires a private memory scope.".to_string())
                    } else {
                        Some(self.memory_promote_reply(
                            command.argument.as_deref().expect("validated fact id"),
                        ))
                    }
                }
                CommandKind::Resume => {
                    if command.invalid_shape {
                        Some(RESUME_USAGE.to_string())
                    } else if state
                        .active
                        .lock()
                        .expect("Telegram active-turn lock")
                        .is_some()
                        || !record.follow_up_queue.is_empty()
                    {
                        Some(
                            "Cannot resume a session while a turn or follow-up queue is active."
                                .to_string(),
                        )
                    } else if record.previous_sessions.is_empty() {
                        Some("No previous session is available.".to_string())
                    } else {
                        let previous = record.previous_sessions[0].clone();
                        if self.inner.runner.require_session_file(&previous).is_err() {
                            Some("The previous session is unavailable.".to_string())
                        } else {
                            let _ = record.swap_previous_session();
                            Some("Resumed the previous Danso session.".to_string())
                        }
                    }
                }
                CommandKind::History => {
                    if command.invalid_shape {
                        Some(HISTORY_USAGE.to_string())
                    } else {
                        Some(self.history_reply(&state, &record))
                    }
                }
                CommandKind::Unknown => Some(
                    "Unknown command. Use /start, /new, /stop, /model, /effort, /usage, /task, \
                     /task_pause, /task_resume, /distill, /memory_promote, /resume, or /history."
                        .to_string(),
                ),
            };
            if let Err(error) = self.save_record(&record) {
                if prepared.is_some() {
                    *state.active.lock().expect("Telegram active-turn lock") = None;
                }
                return Err(error);
            }
            (reply, prepared)
        };
        if let Some(reply) = reply {
            self.reply(chat_id, &reply).await?;
        }
        if let Some(prepared) = prepared {
            self.launch_turn(chat_id, state, prepared, None).await?;
        }
        Ok(())
    }

    async fn handle_prompt(
        &self,
        chat_id: i64,
        update_id: i64,
        state: Arc<ChatState>,
        prompt: String,
    ) -> Result<()> {
        let prepared;
        let queue_reply;
        {
            let _record = state.record.lock().expect("Telegram record lock");
            let mut record = self.load_record(chat_id)?;
            if update_id <= record.last_update_id {
                return Ok(());
            }
            record.last_update_id = update_id;
            let active = state
                .active
                .lock()
                .expect("Telegram active-turn lock")
                .clone();
            if active.is_some() || !record.follow_up_queue.is_empty() {
                if record.follow_up_queue.len() >= self.inner.settings.followup_cap {
                    queue_reply = Some(format!(
                        "Follow-up queue is full (maximum {}).",
                        self.inner.settings.followup_cap
                    ));
                } else {
                    record.follow_up_queue.push(prompt);
                    queue_reply = Some(format!(
                        "Follow-up queued ({}/{}).",
                        record.follow_up_queue.len(),
                        self.inner.settings.followup_cap
                    ));
                }
                self.save_record(&record)?;
                prepared = None;
            } else if record.active_task.is_some() {
                queue_reply = Some(
                    "A paused long task must be resumed or replaced with /new first.".to_string(),
                );
                self.save_record(&record)?;
                prepared = None;
            } else {
                let next = self.prepare_turn(&state, &mut record, prompt)?;
                if let Err(error) = self.save_record(&record) {
                    *state.active.lock().expect("Telegram active-turn lock") = None;
                    return Err(error);
                }
                prepared = Some(next);
                queue_reply = None;
            }
        }
        if let Some(reply) = queue_reply {
            self.reply(chat_id, &reply).await?;
            return Ok(());
        }
        self.launch_turn(
            chat_id,
            state,
            prepared.context("Telegram turn preparation failed")?,
            None,
        )
        .await
    }

    fn prepare_turn(
        &self,
        state: &Arc<ChatState>,
        record: &mut ConversationRecord,
        prompt: String,
    ) -> Result<PreparedTurn> {
        self.prepare_turn_with_task(state, record, prompt, None)
    }

    fn prepare_long_task_turn(
        &self,
        state: &Arc<ChatState>,
        record: &mut ConversationRecord,
        prompt: String,
    ) -> Result<PreparedTurn> {
        let task = crate::runtime::LongTaskRun {
            limits: self.inner.settings.task_limits,
            explicit_limits: 0,
            resume: false,
            follow_up: false,
            pause_after_stage: None,
        };
        let prepared = self.prepare_turn_with_task(state, record, prompt, Some(task))?;
        let started_at = crate::session::now();
        record.mark_long_task_active(prepared.session_id.clone(), started_at);
        Ok(prepared)
    }

    fn prepare_resume_turn(
        &self,
        state: &Arc<ChatState>,
        record: &mut ConversationRecord,
        task: super::store::ActiveTaskRecord,
    ) -> Result<PreparedTurn> {
        ensure!(
            task.kind == "long_task",
            "Telegram active task kind is invalid"
        );
        let long_task = crate::runtime::LongTaskRun {
            limits: self.inner.settings.task_limits,
            explicit_limits: 0,
            resume: true,
            follow_up: false,
            pause_after_stage: None,
        };
        let model = record
            .model
            .clone()
            .unwrap_or_else(|| self.inner.settings.default_model.clone());
        let effort = record.effort.clone();
        record.provider = Some(self.inner.settings.provider.clone());
        record.model = Some(model.clone());
        record.session_pointer = Some(task.session_pointer.clone());
        record.mark_turn_active();
        let active = ActiveTurn::new(task.session_pointer.clone(), true);
        *state.active.lock().expect("Telegram active-turn lock") = Some(Arc::clone(&active));
        Ok(PreparedTurn {
            active,
            session_id: task.session_pointer,
            model,
            effort,
            prompt: String::new(),
            long_task: Some(long_task),
        })
    }

    fn prepare_turn_with_task(
        &self,
        state: &Arc<ChatState>,
        record: &mut ConversationRecord,
        prompt: String,
        long_task: Option<crate::runtime::LongTaskRun>,
    ) -> Result<PreparedTurn> {
        let session_id = match record.session_pointer.clone() {
            Some(session_id) => session_id,
            None => self.inner.runner.new_session()?,
        };
        let model = record
            .model
            .clone()
            .unwrap_or_else(|| self.inner.settings.default_model.clone());
        let effort = record.effort.clone();
        record.provider = Some(self.inner.settings.provider.clone());
        record.model = Some(model.clone());
        record.session_pointer = Some(session_id.clone());
        record.mark_turn_active();
        let active = ActiveTurn::new(session_id.clone(), long_task.is_some());
        *state.active.lock().expect("Telegram active-turn lock") = Some(Arc::clone(&active));
        Ok(PreparedTurn {
            active,
            session_id,
            model,
            effort,
            prompt,
            long_task,
        })
    }

    async fn launch_turn(
        &self,
        chat_id: i64,
        state: Arc<ChatState>,
        prepared: PreparedTurn,
        requeue_on_failure: Option<String>,
    ) -> Result<()> {
        let progress_text = format_progress(&prepared.active, None);
        let progress_message = match self.inner.api.send_message(chat_id, &progress_text).await {
            Ok(message) => message,
            Err(error) => {
                self.abort_prepared_turn(
                    chat_id,
                    &state,
                    &prepared.active,
                    requeue_on_failure.clone(),
                )?;
                return Err(error);
            }
        };
        prepared
            .active
            .set_progress_message(progress_message.message_id);
        {
            let _record = state.record.lock().expect("Telegram record lock");
            let mut record = self.load_record(chat_id)?;
            if let Err(error) = record.set_progress_message(progress_message.message_id) {
                drop(_record);
                self.abort_prepared_turn(
                    chat_id,
                    &state,
                    &prepared.active,
                    requeue_on_failure.clone(),
                )?;
                return Err(error);
            }
            if let Err(error) = self.save_record(&record) {
                drop(_record);
                self.abort_prepared_turn(
                    chat_id,
                    &state,
                    &prepared.active,
                    requeue_on_failure.clone(),
                )?;
                return Err(error);
            }
        }
        if prepared.active.cancelled.load(Ordering::Acquire) {
            self.abort_prepared_turn(
                chat_id,
                &state,
                &prepared.active,
                requeue_on_failure.clone(),
            )?;
            self.edit_progress(
                chat_id,
                &prepared.active,
                "⏹ Turn stopped — journal preserved; no replay.",
            )
            .await;
            return Ok(());
        }
        let handle = match self.inner.runner.start_turn(
            Arc::clone(&prepared.active),
            prepared.session_id,
            prepared.model,
            prepared.effort,
            prepared.prompt,
            prepared.long_task,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.abort_prepared_turn(chat_id, &state, &prepared.active, requeue_on_failure)?;
                self.edit_progress(
                    chat_id,
                    &prepared.active,
                    "⚠️ Turn could not start — journal preserved; no replay.",
                )
                .await;
                return Err(error);
            }
        };
        let service = self.clone();
        tokio::spawn(async move {
            service
                .finish_turn(
                    chat_id,
                    state,
                    prepared.active,
                    handle.receiver,
                    handle.progress,
                )
                .await;
        });
        Ok(())
    }

    fn abort_prepared_turn(
        &self,
        chat_id: i64,
        state: &Arc<ChatState>,
        active: &Arc<ActiveTurn>,
        requeue: Option<String>,
    ) -> Result<()> {
        let _record = state.record.lock().expect("Telegram record lock");
        let mut record = self.load_record(chat_id)?;
        let is_current = state
            .active
            .lock()
            .expect("Telegram active-turn lock")
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, active));
        if is_current {
            *state.active.lock().expect("Telegram active-turn lock") = None;
            record.clear_turn_active();
            if active.long_task
                && record
                    .active_task
                    .as_ref()
                    .is_some_and(|task| task.session_pointer == active.session_id.as_str())
            {
                record.clear_active_task();
            }
            if let Some(prompt) = requeue
                && !active.cancelled.load(Ordering::Acquire)
            {
                record.follow_up_queue.insert(0, prompt);
            }
            self.save_record(&record)?;
        }
        Ok(())
    }

    async fn finish_turn(
        &self,
        chat_id: i64,
        state: Arc<ChatState>,
        active: Arc<ActiveTurn>,
        receiver: oneshot::Receiver<Result<TurnOutcome>>,
        mut progress: mpsc::UnboundedReceiver<ProgressEvent>,
    ) {
        let mut receiver = receiver;
        let mut heartbeat = (self.inner.settings.heartbeat_seconds > 0).then(|| {
            tokio::time::interval_at(
                tokio::time::Instant::now()
                    + Duration::from_secs(self.inner.settings.heartbeat_seconds),
                Duration::from_secs(self.inner.settings.heartbeat_seconds),
            )
        });
        let mut last_tool = None;
        let mut progress_closed = false;
        let result = loop {
            tokio::select! {
                result = &mut receiver => {
                    break result.unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("Telegram turn result unavailable"))
                    });
                }
                event = progress.recv(), if !progress_closed => {
                    match event {
                        Some(ProgressEvent::ToolFinished { name, elapsed_seconds }) => {
                            last_tool = Some((name, elapsed_seconds));
                            self.update_progress(chat_id, &active, last_tool.as_ref()).await;
                        }
                        None => progress_closed = true,
                    }
                }
                _ = wait_for_heartbeat(&mut heartbeat) => {
                    self.update_progress(chat_id, &active, last_tool.as_ref()).await;
                }
            }
        };
        let cancelled = active.cancelled.load(Ordering::Acquire);
        let mut paused = false;
        let (reply, failed, usage_saved) =
            {
                let _record_guard = state.record.lock().expect("Telegram record lock");
                let mut usage_saved = true;
                let mut reply = None;
                let mut failed = false;
                match result {
                    Ok(outcome) if !cancelled => {
                        let TurnOutcome {
                            text,
                            usage,
                            paused: task_paused,
                        } = outcome;
                        paused = task_paused;
                        match self.load_record(chat_id) {
                            Ok(mut record) => {
                                let same_session = record.session_pointer.as_deref()
                                    == Some(active.session_id.as_str());
                                let same_task = record.active_task.as_ref().is_some_and(|task| {
                                    task.session_pointer == active.session_id.as_str()
                                });
                                if task_paused {
                                    // The long-task ledger already contains the
                                    // durable `paused` marker. Keep the task
                                    // metadata so `/task_resume` can find its
                                    // own journal, but release this process-local
                                    // active-turn slot.
                                    record.clear_turn_active();
                                    if self.save_record(&record).is_err() {
                                        usage_saved = false;
                                    }
                                } else if same_session {
                                    if record.record_usage(usage).is_err() {
                                        usage_saved = false;
                                    }
                                    record.clear_turn_active();
                                    if same_task {
                                        record.clear_active_task();
                                    }
                                    if self.save_record(&record).is_err() {
                                        usage_saved = false;
                                    }
                                } else {
                                    usage_saved = false;
                                    record.clear_turn_active();
                                    if self.save_record(&record).is_err() {
                                        usage_saved = false;
                                    }
                                }
                            }
                            Err(_) => usage_saved = false,
                        }
                        reply = text;
                    }
                    Ok(_) => {
                        if let Ok(mut record) = self.load_record(chat_id) {
                            record.clear_turn_active();
                            if record.active_task.as_ref().is_some_and(|task| {
                                task.session_pointer == active.session_id.as_str()
                            }) {
                                record.clear_active_task();
                            }
                            if self.save_record(&record).is_err() {
                                usage_saved = false;
                            }
                        } else {
                            usage_saved = false;
                        }
                    }
                    Err(_) => {
                        failed = !cancelled;
                        if let Ok(mut record) = self.load_record(chat_id) {
                            record.clear_turn_active();
                            if record.active_task.as_ref().is_some_and(|task| {
                                task.session_pointer == active.session_id.as_str()
                            }) {
                                record.clear_active_task();
                            }
                            if self.save_record(&record).is_err() {
                                usage_saved = false;
                            }
                        } else {
                            usage_saved = false;
                        }
                    }
                }
                (reply, failed, usage_saved)
            };
        let progress_status = if cancelled {
            "⏹ Turn stopped — journal preserved; no replay."
        } else if paused {
            "⏸ Long task paused."
        } else if reply.is_some() {
            "✅ Turn complete."
        } else {
            "⚠️ Turn failed — journal preserved; no replay."
        };
        self.edit_progress(chat_id, &active, progress_status).await;
        if let Some(text) = reply {
            if !usage_saved {
                eprintln!("telegram usage persistence failed");
            }
            if self.reply(chat_id, &text).await.is_err() {
                eprintln!("telegram final answer delivery failed");
            }
        } else if failed
            && self
                .reply(
                    chat_id,
                    "Turn failed. The journal was retained and no automatic replay was performed.",
                )
                .await
                .is_err()
        {
            eprintln!("telegram failure notice delivery failed");
        }
        if paused {
            let current = state
                .active
                .lock()
                .expect("Telegram active-turn lock")
                .clone();
            if current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &active))
            {
                *state.active.lock().expect("Telegram active-turn lock") = None;
            }
        } else {
            self.advance_after_turn(chat_id, state, active).await;
        }
    }

    fn advance_after_turn(
        &self,
        chat_id: i64,
        state: Arc<ChatState>,
        active: Arc<ActiveTurn>,
    ) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        // Boxed so the finish_turn future (which awaits this via the
        // follow-up advance path) does not embed this future's type, which
        // itself spawns finish_turn — a mutually recursive async chain that
        // rustc cannot prove Send without the type erasure.
        Box::pin(async move {
            let prepared = {
                let _record = state.record.lock().expect("Telegram record lock");
                let mut record = match self.load_record(chat_id) {
                    Ok(record) => record,
                    Err(_) => return,
                };
                let current = state
                    .active
                    .lock()
                    .expect("Telegram active-turn lock")
                    .clone();
                if !current
                    .as_ref()
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &active))
                {
                    return;
                }
                let Some(prompt) = record.follow_up_queue.first().cloned() else {
                    *state.active.lock().expect("Telegram active-turn lock") = None;
                    let _ = self.save_record(&record);
                    return;
                };
                record.follow_up_queue.remove(0);
                match self.prepare_turn(&state, &mut record, prompt.clone()) {
                    Ok(prepared) => {
                        if self.save_record(&record).is_ok() {
                            Some(prepared)
                        } else {
                            record.clear_turn_active();
                            record.follow_up_queue.insert(0, prepared.prompt);
                            *state.active.lock().expect("Telegram active-turn lock") =
                                Some(Arc::clone(&active));
                            let _ = self.save_record(&record);
                            None
                        }
                    }
                    Err(_) => {
                        record.follow_up_queue.insert(0, prompt);
                        let _ = self.save_record(&record);
                        None
                    }
                }
            };
            if let Some(prepared) = prepared
                && self
                    .launch_turn(
                        chat_id,
                        state,
                        PreparedTurn {
                            active: Arc::clone(&prepared.active),
                            session_id: prepared.session_id.clone(),
                            model: prepared.model.clone(),
                            effort: prepared.effort.clone(),
                            prompt: prepared.prompt.clone(),
                            long_task: prepared.long_task,
                        },
                        Some(prepared.prompt),
                    )
                    .await
                    .is_err()
            {
                eprintln!("telegram queued turn start failed");
            }
        })
    }

    async fn update_progress(
        &self,
        chat_id: i64,
        active: &Arc<ActiveTurn>,
        last_tool: Option<&(String, u64)>,
    ) {
        let text = format_progress(active, last_tool);
        self.edit_progress(chat_id, active, &text).await;
    }

    async fn edit_progress(&self, chat_id: i64, active: &ActiveTurn, text: &str) {
        let Some(message_id) = active.progress_message() else {
            return;
        };
        if self
            .inner
            .api
            .edit_message_text(chat_id, message_id, text)
            .await
            .is_err()
        {
            eprintln!("telegram progress update failed");
        }
    }

    async fn reply(&self, chat_id: i64, text: &str) -> Result<()> {
        for part in split_message(text, 4096) {
            self.inner.api.send_message(chat_id, &part).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandKind {
    Start,
    New,
    Stop,
    Model,
    Effort,
    Usage,
    Task,
    TaskPause,
    TaskResume,
    Distill,
    MemoryPromote,
    Resume,
    History,
    Unknown,
}

#[derive(Debug)]
struct CommandInput {
    kind: CommandKind,
    argument: Option<String>,
    invalid_shape: bool,
}

fn parse_command(text: &str) -> Option<CommandInput> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let command_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let command = trimmed.get(..command_end)?.strip_prefix('/')?;
    let command = command.split('@').next().unwrap_or(command);
    let kind = match command.to_ascii_lowercase().as_str() {
        "start" => CommandKind::Start,
        "new" => CommandKind::New,
        "stop" => CommandKind::Stop,
        "model" => CommandKind::Model,
        "effort" => CommandKind::Effort,
        "usage" => CommandKind::Usage,
        "task" => CommandKind::Task,
        "task_pause" => CommandKind::TaskPause,
        "task_resume" => CommandKind::TaskResume,
        "distill" => CommandKind::Distill,
        "memory_promote" => CommandKind::MemoryPromote,
        "resume" => CommandKind::Resume,
        "history" => CommandKind::History,
        _ => CommandKind::Unknown,
    };
    let rest = trimmed.get(command_end..).unwrap_or_default();
    let argument = if kind == CommandKind::Task {
        let prompt = rest.trim();
        (!prompt.is_empty()).then(|| prompt.to_string())
    } else {
        rest.split_whitespace().next().map(str::to_string)
    };
    let invalid_shape = if kind == CommandKind::Task {
        false
    } else {
        let mut parts = rest.split_whitespace();
        let _ = parts.next();
        parts.next().is_some()
            || (matches!(
                kind,
                CommandKind::Start
                    | CommandKind::New
                    | CommandKind::Stop
                    | CommandKind::Usage
                    | CommandKind::TaskPause
                    | CommandKind::TaskResume
                    | CommandKind::Distill
                    | CommandKind::Resume
                    | CommandKind::History
            ) && argument.is_some())
    };
    Some(CommandInput {
        kind,
        argument,
        invalid_shape,
    })
}

fn valid_distill_id(fact_id: &str) -> bool {
    fact_id.len() == "distill-".len() + 12
        && fact_id.starts_with("distill-")
        && fact_id["distill-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn short_session_id(session_id: &str) -> String {
    let bytes = session_id.as_bytes();
    if bytes.len() < 8
        || !bytes[..8]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return "unknown".to_string();
    }
    String::from_utf8(bytes[..8].to_vec()).unwrap_or_else(|_| "unknown".to_string())
}

fn format_usage(record: &ConversationRecord) -> String {
    let last = match &record.last_turn_usage {
        Some(usage) => format_usage_record(usage),
        None => "none".to_string(),
    };
    format!(
        "Last-turn usage: {last}\nPer-chat aggregate usage: {}",
        format_usage_record(&record.usage)
    )
}

fn format_usage_record(usage: &UsageRecord) -> String {
    format!(
        "requests={}, input_tokens={}, output_tokens={}, cache_read_tokens={}, \
         cache_write_tokens={}, total_tokens={}",
        usage.requests,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.total_tokens
    )
}

fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.chars().count() == limit {
            parts.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn safe_tool_name(name: &str) -> String {
    let mut safe: String = name
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        })
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        safe.push_str("unknown");
    }
    safe
}

fn format_progress(active: &ActiveTurn, last_tool: Option<&(String, u64)>) -> String {
    let elapsed_seconds = active.started_at.elapsed().as_secs();
    match last_tool {
        Some((name, tool_seconds)) => {
            format!("⏳ Turn in progress — {elapsed_seconds}s\nTool {name} — {tool_seconds}s")
        }
        None => format!("⏳ Turn in progress — {elapsed_seconds}s"),
    }
}

async fn wait_for_heartbeat(interval: &mut Option<tokio::time::Interval>) {
    match interval.as_mut() {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

fn poll_retry_delay(attempt: u32) -> Duration {
    const SCHEDULE_MS: [u64; 5] = [100, 250, 500, 1_000, 2_000];
    Duration::from_millis(SCHEDULE_MS[(attempt as usize).min(SCHEDULE_MS.len() - 1)])
}

/// Construct the service from environment and run it in the one process
/// started by danso telegram.
pub fn run() -> Result<()> {
    let service = TelegramService::from_env()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start Telegram service runtime")?;
    runtime.block_on(service.run())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_accept_bot_suffix_and_reject_extra_arguments() {
        assert_eq!(
            parse_command("/start@danso").map(|command| command.kind),
            Some(CommandKind::Start)
        );
        let model = parse_command("/model gpt-test").unwrap();
        assert_eq!(model.kind, CommandKind::Model);
        assert_eq!(model.argument.as_deref(), Some("gpt-test"));
        assert!(!model.invalid_shape);
        assert!(parse_command("/usage now").unwrap().invalid_shape);
    }

    #[test]
    fn task_parser_keeps_the_whole_free_form_prompt() {
        let task = parse_command("/task  inspect   src/telegram/service.rs  carefully ").unwrap();
        assert_eq!(task.kind, CommandKind::Task);
        assert_eq!(
            task.argument.as_deref(),
            Some("inspect   src/telegram/service.rs  carefully")
        );
        assert!(!task.invalid_shape);
        assert_eq!(parse_command("/task").unwrap().argument, None);
        assert!(valid_distill_id("distill-0123456789ab"));
        assert!(!valid_distill_id("distill-0123456789aB"));
        assert!(!valid_distill_id("distill-0123456789a"));
    }

    #[test]
    fn message_splitting_obeys_character_limit() {
        let parts = split_message(&"x".repeat(8193), 4096);
        assert_eq!(
            parts
                .iter()
                .map(|part| part.chars().count())
                .collect::<Vec<_>>(),
            vec![4096, 4096, 1]
        );
    }

    #[test]
    fn progress_labels_are_body_free() {
        let active = ActiveTurn::new("session-id".to_string(), false);
        let tool = safe_tool_name("bash /private/user/file.txt");
        let text = format_progress(&active, Some(&(tool, 4)));
        assert!(text.contains("4s"));
        assert!(!text.contains('/'));
        assert!(!text.contains("file.txt"));
    }
}
