//! Telegram service wiring for B1.
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
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::{Notify, oneshot};

const JOURNALS_DIR: &str = "journals";
const DEFAULT_PROVIDER: &str = "anthropic";
const DEFAULT_MAX_TURNS: u32 = 48;
const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
const DEFAULT_PROVIDER_TIMEOUT_SECONDS: u64 = 180;
const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 900;
const DEFAULT_PROVIDER_RETRIES: u32 = 3;

/// The command has deliberately no CLI flags. Runtime and provider
/// configuration comes from the documented environment so the service cannot
/// accidentally diverge from a normal Danso run.
#[derive(Debug, Parser)]
#[command(
    name = "danso telegram",
    about = "Run the Telegram single-turn service using environment configuration"
)]
pub struct TelegramArgs {}

#[derive(Clone, Debug)]
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
        })
    }

    fn config(
        &self,
        prompt: String,
        session: PathBuf,
        model: String,
        effort: Option<String>,
        cancellation_reason: Arc<AtomicU8>,
    ) -> crate::app::RunConfig {
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
                scope: "global".to_string(),
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
            timeout_seconds: self.timeout_seconds,
            provider_timeout_seconds: self.provider_timeout_seconds,
            tool_timeout_seconds: self.tool_timeout_seconds,
            tool_home: None,
            long_task: None,
            task_progress: false,
            pause_requested: None,
            cancellation_reason: Some(cancellation_reason),
        }
    }
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
}

impl TelegramSink {
    fn new() -> Self {
        Self { final_text: None }
    }

    fn final_text(self) -> Result<String> {
        self.final_text
            .context("completed Telegram turn did not produce a final answer")
    }
}

impl crate::contracts::EventSink for TelegramSink {
    fn emit(&mut self, event: crate::contracts::Event<'_>) -> Result<()> {
        if let crate::contracts::Event::FinalAnswer(message) = event {
            let mut text = crate::contracts::text_blocks(message).join("");
            if text.is_empty()
                && let Some(content) = message["content"].as_str()
            {
                text = content.to_string();
            }
            ensure!(!text.is_empty(), "final Telegram answer is empty");
            self.final_text = Some(text);
        }
        Ok(())
    }
}

struct TurnOutcome {
    text: String,
    usage: UsageRecord,
}

struct ActiveTurn {
    cancel: Arc<Notify>,
    cancellation_reason: Arc<AtomicU8>,
    cancelled: AtomicBool,
    session_id: String,
}

impl ActiveTurn {
    fn interrupt(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancellation_reason.store(1, Ordering::Release);
        // notify_one retains a permit if the turn thread has not reached its
        // select yet; notify_waiters would lose an early /stop notification.
        self.cancel.notify_one();
    }
}

struct TurnHandle {
    active: Arc<ActiveTurn>,
    receiver: oneshot::Receiver<Result<TurnOutcome>>,
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

    fn start_turn(
        &self,
        session_id: String,
        model: String,
        effort: Option<String>,
        prompt: String,
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
        let cancellation_reason = Arc::new(AtomicU8::new(0));
        let cancel = Arc::new(Notify::new());
        let active = Arc::new(ActiveTurn {
            cancel: Arc::clone(&cancel),
            cancellation_reason: Arc::clone(&cancellation_reason),
            cancelled: AtomicBool::new(false),
            session_id: session_id.clone(),
        });
        let config = self
            .settings
            .config(prompt, journal, model, effort, cancellation_reason);
        let (sender, receiver) = oneshot::channel();
        let thread_name = format!("danso-telegram-turn-{}", &session_id[..8]);
        let _thread = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_turn(config, cancel)
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "Telegram turn panicked; journal retained. No automatic replay."
                    ))
                });
                let _ = sender.send(result);
            })
            .context("could not start Telegram turn")?;
        Ok(TurnHandle { active, receiver })
    }
}

fn run_turn(config: crate::app::RunConfig, cancel: Arc<Notify>) -> Result<TurnOutcome> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start Telegram turn runtime")?;
    let mut usage = crate::usage::Usage::default();
    let mut sink = TelegramSink::new();
    runtime.block_on(async {
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
    })?;
    let text = sink.final_text()?;
    let usage = UsageRecord::from_summary(&usage.summary())?;
    Ok(TurnOutcome { text, usage })
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
    chats: Mutex<HashMap<i64, Arc<ChatState>>>,
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
            chats: Mutex::new(HashMap::new()),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    fn chat(&self, chat_id: i64) -> Arc<ChatState> {
        let mut chats = self.inner.chats.lock().expect("Telegram chat map lock");
        chats
            .entry(chat_id)
            .or_insert_with(|| Arc::new(ChatState::new()))
            .clone()
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

    async fn run_loop(self, shutdown: Option<Arc<Notify>>) -> Result<()> {
        let mut poller = self
            .inner
            .api
            .poller(None, self.inner.poll_timeout_seconds)?;
        loop {
            let updates = match &shutdown {
                Some(shutdown) => {
                    tokio::select! {
                        _ = shutdown.notified() => return Ok(()),
                        updates = poller.next() => updates?,
                    }
                }
                None => poller.next().await?,
            };
            for update in updates {
                if let Err(_error) = self.handle_update(update).await {
                    eprintln!("telegram update handling failed");
                }
            }
        }
    }

    /// Process one update. This is public for loopback/fake-Bot-API tests and
    /// keeps access admission before every command, store read, and reply.
    pub async fn handle_update(&self, update: Update) -> Result<()> {
        if !self.inner.access.authorize(&update) {
            return Ok(());
        }
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

    fn mark_update(&self, chat_id: i64, update_id: i64) -> Result<()> {
        let state = self.chat(chat_id);
        let _record = state.record.lock().expect("Telegram record lock");
        let mut record = self.load_record(chat_id)?;
        if update_id <= record.last_update_id {
            return Ok(());
        }
        record.last_update_id = update_id;
        self.inner.conversations.save(&record)
    }

    async fn handle_command(
        &self,
        chat_id: i64,
        update_id: i64,
        state: Arc<ChatState>,
        command: CommandInput,
    ) -> Result<()> {
        let reply;
        {
            let _record = state.record.lock().expect("Telegram record lock");
            let mut record = self.load_record(chat_id)?;
            if update_id <= record.last_update_id {
                return Ok(());
            }
            record.last_update_id = update_id;
            match command.kind {
                CommandKind::Start => {
                    reply = Some(if command.invalid_shape {
                        "Usage: /start".to_string()
                    } else {
                        "Hello! You are authorized to use this Danso bot. \
                         Access is restricted to the configured Telegram allowlist. \
                         Send text for one turn, or use /new, /stop, /model, /effort, and /usage."
                            .to_string()
                    });
                }
                CommandKind::New => {
                    if command.invalid_shape {
                        reply = Some("Usage: /new".to_string());
                    } else if state
                        .active
                        .lock()
                        .expect("Telegram active-turn lock")
                        .is_some()
                    {
                        reply = Some("A turn is already running; use /stop first.".to_string());
                    } else {
                        let session_id = self.inner.runner.new_session()?;
                        record.session_pointer = Some(session_id);
                        reply = Some("Started a new Danso session.".to_string());
                    }
                }
                CommandKind::Stop => {
                    if command.invalid_shape {
                        reply = Some("Usage: /stop".to_string());
                    } else {
                        let active = state
                            .active
                            .lock()
                            .expect("Telegram active-turn lock")
                            .clone();
                        if let Some(active) = &active {
                            active.interrupt();
                        }
                        reply = Some(if active.is_some() {
                            "Stopping the active turn. Its journal is preserved and will not be replayed."
                        } else {
                            "No turn is currently running."
                        }
                        .to_string());
                    }
                }
                CommandKind::Model => {
                    if command.invalid_shape {
                        reply = Some("Usage: /model [model-name]".to_string());
                    } else if let Some(model) = command.argument {
                        match validate_model(&model) {
                            Ok(()) => {
                                record.provider = Some(self.inner.settings.provider.clone());
                                record.model = Some(model.clone());
                                reply = Some(format!("Model set to {model}."));
                            }
                            Err(_) => {
                                reply = Some(
                                    "Model must be one non-whitespace token of at most 4096 bytes."
                                        .to_string(),
                                )
                            }
                        }
                    } else {
                        reply = Some(format!(
                            "Current model: {}",
                            record.effective_model(&self.inner.settings.default_model)
                        ));
                    }
                }
                CommandKind::Effort => {
                    if command.invalid_shape {
                        reply = Some(
                            "Usage: /effort [none|minimal|low|medium|high|xhigh|max|default]"
                                .to_string(),
                        );
                    } else if let Some(argument) = command.argument {
                        let effort = argument.to_ascii_lowercase();
                        if effort == "default" {
                            record.provider = Some(self.inner.settings.provider.clone());
                            record.effort = None;
                            reply =
                                Some("Reasoning effort reset to the service default.".to_string());
                        } else {
                            match validate_effort(Some(&effort), &self.inner.settings.provider) {
                                Ok(()) => {
                                    record.provider = Some(self.inner.settings.provider.clone());
                                    record.effort = Some(effort.clone());
                                    reply = Some(format!("Reasoning effort set to {effort}."));
                                }
                                Err(_) => reply = Some(
                                    "That reasoning effort is not supported by the configured provider."
                                        .to_string(),
                                ),
                            }
                        }
                    } else {
                        let effort = record
                            .effective_effort(self.inner.settings.default_effort.as_deref())
                            .unwrap_or("default");
                        reply = Some(format!("Current reasoning effort: {effort}."));
                    }
                }
                CommandKind::Usage => {
                    reply = Some(if command.invalid_shape {
                        "Usage: /usage".to_string()
                    } else {
                        format_usage(&record)
                    });
                }
                CommandKind::Unknown => {
                    reply = Some(
                        "Unknown command. Use /start, /new, /stop, /model, /effort, or /usage."
                            .to_string(),
                    );
                }
            }
            self.inner.conversations.save(&record)?;
        }
        if let Some(reply) = reply {
            self.reply(chat_id, &reply).await?;
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
        let handle;
        let busy;
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
            if active.is_some() {
                busy = true;
                handle = None;
            } else {
                busy = false;
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
                self.inner.conversations.save(&record)?;
                let next = self
                    .inner
                    .runner
                    .start_turn(session_id, model, effort, prompt)?;
                *state.active.lock().expect("Telegram active-turn lock") =
                    Some(Arc::clone(&next.active));
                handle = Some(next);
            }
            if busy {
                self.inner.conversations.save(&record)?;
            }
        }
        if busy {
            self.reply(chat_id, "A turn is already running; use /stop first.")
                .await?;
            return Ok(());
        }
        let handle = handle.context("Telegram turn handle disappeared")?;
        let service = self.clone();
        tokio::spawn(async move {
            service
                .finish_turn(chat_id, state, handle.active, handle.receiver)
                .await;
        });
        Ok(())
    }

    async fn finish_turn(
        &self,
        chat_id: i64,
        state: Arc<ChatState>,
        active: Arc<ActiveTurn>,
        receiver: oneshot::Receiver<Result<TurnOutcome>>,
    ) {
        let result = receiver.await;
        // Every path takes the per-chat record lock before the active-turn
        // lock. Command handling uses the same order; reversing it here can
        // deadlock /stop or /new against completion persistence. Keep the
        // guard in this block so the spawned async task never holds a
        // non-Send std mutex guard across a reply await.
        let (reply, failed) = {
            let _record_guard = state.record.lock().expect("Telegram record lock");
            {
                let mut current = state.active.lock().expect("Telegram active-turn lock");
                if current
                    .as_ref()
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &active))
                {
                    *current = None;
                }
            }
            let cancelled = active.cancelled.load(Ordering::Acquire);

            match result {
                Ok(Ok(outcome)) if !cancelled => {
                    let TurnOutcome { text, usage } = outcome;
                    let mut usage_saved = true;
                    match self.load_record(chat_id) {
                        Ok(mut record) => {
                            if record.session_pointer.as_deref() == Some(active.session_id.as_str())
                            {
                                if record
                                    .record_usage(usage)
                                    .and_then(|_| self.inner.conversations.save(&record))
                                    .is_err()
                                {
                                    usage_saved = false;
                                }
                            } else {
                                usage_saved = false;
                            }
                        }
                        Err(_) => usage_saved = false,
                    }
                    (Some((text, usage_saved)), false)
                }
                Ok(Ok(_)) => {
                    // A /stop won the race with a completed response. Never
                    // deliver a result after the caller requested cancellation.
                    (None, false)
                }
                Ok(Err(_error)) if !cancelled => (None, true),
                Ok(Err(_)) | Err(_) => (None, false),
            }
        };

        if let Some((text, usage_saved)) = reply {
            if !usage_saved {
                eprintln!("telegram usage persistence failed");
            }
            if self.reply(chat_id, &text).await.is_err() {
                eprintln!("telegram final answer delivery failed");
            }
        } else if failed {
            let _ = self
                .reply(
                    chat_id,
                    "Turn failed. The journal was retained and no automatic replay was performed.",
                )
                .await;
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
    let mut parts = trimmed.split_whitespace();
    let command = parts.next()?.strip_prefix('/')?;
    let command = command.split('@').next().unwrap_or(command);
    let kind = match command.to_ascii_lowercase().as_str() {
        "start" => CommandKind::Start,
        "new" => CommandKind::New,
        "stop" => CommandKind::Stop,
        "model" => CommandKind::Model,
        "effort" => CommandKind::Effort,
        "usage" => CommandKind::Usage,
        _ => CommandKind::Unknown,
    };
    let argument = parts.next().map(str::to_string);
    let invalid_shape = parts.next().is_some()
        || (matches!(
            kind,
            CommandKind::Start | CommandKind::New | CommandKind::Stop | CommandKind::Usage
        ) && argument.is_some());
    Some(CommandInput {
        kind,
        argument,
        invalid_shape,
    })
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
}
