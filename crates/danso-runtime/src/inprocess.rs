//! In-process turn runner (docs/unified-design.md §4.2).
//!
//! Each turn runs `danso::app::run` on its own thread with a private
//! current-thread Tokio runtime. Cancellation drops the run future, which is
//! the same path the CLI takes on SIGTERM: the host supervisor guard signals
//! its descendants and the journal keeps any `started` marker without a
//! `settled` one, so recovery refuses to replay uncertain work. A panic in a
//! turn is reported as a runtime error and never unwinds into the embedder.
//!
//! Provider credentials and `HOME` are read from the process environment by
//! the core, exactly as the CLI does. Injecting credentials as values is a
//! later change to the core provider constructors.
use crate::{
    event::{AgentEvent, ErrorCode, TaskState},
    session::{
        AgentSession, ApprovalHandler, EFFORTS, EventStream, ModelInfo, SessionRequest, TurnInput,
        TurnRunner, valid_session_id,
    },
    sink::ChannelSink,
};
use anyhow::{Context, Result, ensure};
use danso::{
    app::{self, Backend, RunConfig},
    long_task::Limits,
    memory,
    runtime::LongTaskRun,
    usage::Usage,
};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

pub const PROVIDERS: [&str; 4] = ["anthropic", "openai", "openai-codex", "glm"];

/// Everything a turn needs except the prompt, workspace and journal. Cloned
/// into a `RunConfig` per turn; defaults match the CLI defaults.
#[derive(Clone, Debug)]
pub struct RunTemplate {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub trust_project: bool,
    pub no_tools: bool,
    pub system_context_file: Option<PathBuf>,
    pub memory: memory::MemoryConfig,
    pub memory_distill: memory::DistillMode,
    pub backend: Backend,
    pub max_turns: u32,
    pub max_output_tokens: Option<u32>,
    pub glm_thinking: Option<String>,
    pub glm_endpoint: Option<String>,
    pub provider_retries: u32,
    pub continuation_limit: u32,
    pub repeat_limit: u32,
    pub compact_at_bytes: Option<usize>,
    /// Whole-turn wall budget; the long-task wall limit when `long_task` is set.
    pub timeout_seconds: u64,
    pub provider_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
    pub tool_home: Option<PathBuf>,
    /// `Some` selects the opt-in long-task mode with these limits. The wall
    /// limit is taken from `timeout_seconds`.
    pub long_task: Option<Limits>,
}

impl RunTemplate {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort: None,
            trust_project: false,
            no_tools: false,
            system_context_file: None,
            memory: memory::MemoryConfig {
                scope: "global".into(),
                max_bytes: memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT,
                ..Default::default()
            },
            memory_distill: memory::DistillMode::Queue,
            backend: Backend::Host,
            max_turns: 48,
            max_output_tokens: None,
            glm_thinking: None,
            glm_endpoint: None,
            provider_retries: 3,
            continuation_limit: 0,
            repeat_limit: 0,
            compact_at_bytes: None,
            timeout_seconds: 1800,
            provider_timeout_seconds: 180,
            tool_timeout_seconds: danso::tools::HOST_TOOL_TIMEOUT_DEFAULT_SECONDS,
            tool_home: None,
            long_task: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            PROVIDERS.contains(&self.provider.as_str()),
            "unsupported provider"
        );
        ensure!(!self.model.trim().is_empty(), "model must not be empty");
        if let Some(effort) = &self.reasoning_effort {
            ensure!(
                EFFORTS.contains(&effort.as_str()),
                "invalid reasoning effort"
            );
            ensure!(
                self.provider != "anthropic",
                "reasoning-effort is unsupported by the Anthropic adapter"
            );
        }
        ensure!(
            (1..=128).contains(&self.max_turns),
            "max-turns must be 1..128"
        );
        Ok(())
    }

    fn config(
        &self,
        prompt: String,
        cwd: PathBuf,
        session: PathBuf,
        effort: Option<String>,
        resume: bool,
        pause_requested: Arc<AtomicBool>,
    ) -> RunConfig {
        RunConfig {
            prompt,
            cwd,
            session,
            model: self.model.clone(),
            provider: self.provider.clone(),
            reasoning_effort: effort.or_else(|| self.reasoning_effort.clone()),
            trust_project: self.trust_project,
            no_tools: self.no_tools,
            system_context_file: self.system_context_file.clone(),
            memory: self.memory.clone(),
            memory_refresh: self.memory.refresh,
            memory_distill: self.memory_distill,
            backend: self.backend,
            max_turns: self.max_turns,
            max_output_tokens: self.max_output_tokens,
            glm_thinking: self.glm_thinking.clone(),
            glm_endpoint: self.glm_endpoint.clone(),
            provider_retries: self.provider_retries,
            continuation_limit: self.continuation_limit,
            stream_requests: false,
            repeat_limit: self.repeat_limit,
            compact_at_bytes: self.compact_at_bytes,
            timeout_seconds: self.timeout_seconds,
            provider_timeout_seconds: self.provider_timeout_seconds,
            tool_timeout_seconds: self.tool_timeout_seconds,
            tool_home: self.tool_home.clone(),
            long_task: self.long_task.map(|limits| LongTaskRun {
                limits: Limits {
                    wall_seconds: self.timeout_seconds,
                    ..limits
                },
                // A resumed task reloads its immutable limits from the
                // journal; the template never asserts them.
                explicit_limits: 0,
                resume,
                pause_after_stage: None,
            }),
            task_progress: false,
            pause_requested: Some(pause_requested),
        }
    }
}

/// Runs turns in this process. One journal root, one configured model.
pub struct InProcessRunner {
    journal_root: PathBuf,
    template: RunTemplate,
}

fn require_private_dir(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "journal root must be an absolute path");
    let meta = std::fs::symlink_metadata(path).context("journal root must exist")?;
    ensure!(meta.is_dir(), "journal root must be a directory");
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "journal root must be owned by the current user"
    );
    ensure!(
        meta.mode() & 0o777 == 0o700,
        "journal root must be mode 0700"
    );
    Ok(())
}

fn require_private_journal(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).context("stored session is missing")?;
    ensure!(meta.is_file(), "stored session must be a regular file");
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "stored session must be owned by the current user"
    );
    ensure!(
        meta.mode() & 0o077 == 0,
        "stored session permissions are too broad"
    );
    Ok(())
}

impl InProcessRunner {
    pub fn new(journal_root: impl Into<PathBuf>, template: RunTemplate) -> Result<Self> {
        let journal_root = journal_root.into();
        require_private_dir(&journal_root)?;
        template.validate()?;
        Ok(Self {
            journal_root,
            template,
        })
    }

    pub fn journal_path(&self, session_id: &str) -> PathBuf {
        self.journal_root.join(format!("{session_id}.jsonl"))
    }
}

impl TurnRunner for InProcessRunner {
    fn start_or_resume(&self, request: SessionRequest) -> Result<Box<dyn AgentSession>> {
        request.validate()?;
        let cwd = request
            .working_directory
            .canonicalize()
            .context("workspace does not exist")?;
        ensure!(cwd.is_dir(), "workspace must be a directory");
        ensure!(
            !self.journal_root.starts_with(&cwd),
            "journal root must be outside the workspace"
        );
        if let Some(model) = &request.model {
            ensure!(
                model == &self.template.model,
                "model changes are not supported by this runner"
            );
        }
        if request.effort.is_some() {
            ensure!(
                self.template.provider != "anthropic",
                "reasoning-effort is unsupported by the Anthropic adapter"
            );
        }
        let id = match request.session_id {
            Some(id) => {
                require_private_journal(&self.journal_path(&id))?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().hyphenated().to_string();
                debug_assert!(valid_session_id(&id));
                ensure!(
                    std::fs::symlink_metadata(self.journal_path(&id)).is_err(),
                    "fresh journal path already exists"
                );
                id
            }
        };
        let journal = self.journal_path(&id);
        Ok(Box::new(InProcessSession {
            id,
            journal,
            cwd,
            template: self.template.clone(),
            effort: request.effort,
            active: Mutex::new(None),
        }))
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        let efforts = if self.template.provider == "anthropic" {
            Vec::new()
        } else {
            EFFORTS.iter().map(|e| e.to_string()).collect()
        };
        vec![ModelInfo {
            id: self.template.model.clone(),
            display_name: self.template.model.clone(),
            default_reasoning_effort: self.template.reasoning_effort.clone(),
            supported_reasoning_efforts: efforts,
            is_default: true,
        }]
    }
}

struct ActiveTurn {
    cancel: Arc<Notify>,
    pause: Arc<AtomicBool>,
    long_task: bool,
    handle: std::thread::JoinHandle<()>,
}

pub struct InProcessSession {
    id: String,
    journal: PathBuf,
    cwd: PathBuf,
    template: RunTemplate,
    effort: Option<String>,
    active: Mutex<Option<ActiveTurn>>,
}

impl InProcessSession {
    pub fn journal_path(&self) -> &Path {
        &self.journal
    }
}

/// Body-free terminal failure. Only closed enums and bounded counters from
/// the typed diagnostics are rendered; provider prose never appears.
fn failure_event(error: &anyhow::Error, last_task: Option<TaskState>, usage: &Usage) -> AgentEvent {
    let kind = danso::failure::category(error);
    let mut code = kind
        .map(ErrorCode::from)
        .unwrap_or(ErrorCode::Configuration);
    if code == ErrorCode::RequestBudget && last_task == Some(TaskState::Paused) {
        code = ErrorCode::TaskPaused;
    }
    let snapshot = usage.snapshot();
    let mut message = format!(
        "worker failed: category={}, reported_requests={}, reported_tokens={}",
        code.as_str(),
        snapshot.requests,
        snapshot.total
    );
    if let Some(provider) = danso::failure::provider(error) {
        message.push_str(&format!(
            ", reason={}",
            serde_json::to_value(provider.reason())
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ));
        if let Some(status) = provider.http_status() {
            message.push_str(&format!(", http_status={status}"));
        }
    }
    if let Some(transport) = danso::failure::transport(error) {
        message.push_str(&format!(
            ", elapsed_ms={}, attempts={}",
            transport.elapsed_ms(),
            transport.attempts()
        ));
    }
    if let Some(recovery) = danso::failure::task_recovery(error) {
        message.push_str(&format!(", recovery={}", recovery));
    }
    message.push_str(". No automatic replay.");
    AgentEvent::error(code, message)
}

fn run_turn(
    config: RunConfig,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: Arc<Notify>,
    timeout: Duration,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            tx.send(AgentEvent::error(
                ErrorCode::Runtime,
                "worker runtime could not start",
            ))
            .ok();
            return;
        }
    };
    let mut usage = Usage::default();
    let mut sink = ChannelSink::new(tx.clone());
    let outcome = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.notified() => Err(None),
            result = tokio::time::timeout(timeout, app::run(&config, &mut sink, &mut usage)) => {
                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(Some(error)),
                    Err(_) => Err(Some(danso::failure::at(danso::failure::Kind::RunTimeout)(
                        anyhow::anyhow!("run timed out"),
                    ))),
                }
            }
        }
    });
    let terminal = match outcome {
        Ok(()) => {
            let text = sink.final_text().unwrap_or_default().to_string();
            let result = AgentEvent::Result {
                result: serde_json::json!({"text": text, "usage": usage.summary()}),
            };
            tx.send(result).ok();
            AgentEvent::completion("stop").expect("non-empty stop reason")
        }
        Err(None) => AgentEvent::error(
            ErrorCode::Cancelled,
            "worker interrupted; journal retained. No automatic replay.",
        ),
        Err(Some(error)) => failure_event(&error, sink.last_task_state(), &usage),
    };
    tx.send(terminal).ok();
}

impl AgentSession for InProcessSession {
    fn session_id(&self) -> &str {
        &self.id
    }

    fn send_turn(
        &self,
        input: TurnInput,
        _approvals: Arc<dyn ApprovalHandler>,
    ) -> Result<EventStream> {
        input.validate()?;
        let resume = matches!(input, TurnInput::ResumeTask);
        ensure!(
            !resume || self.template.long_task.is_some(),
            "long-task resume is disabled for this runner"
        );
        let mut active = self.active.lock().expect("turn lock");
        if let Some(turn) = active.as_ref() {
            ensure!(turn.handle.is_finished(), "a turn is already active");
        }
        let prompt = match input {
            TurnInput::Prompt(prompt) => prompt,
            TurnInput::ResumeTask => String::new(),
        };
        let pause = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(Notify::new());
        let config = self.template.config(
            prompt,
            self.cwd.clone(),
            self.journal.clone(),
            self.effort.clone(),
            resume,
            Arc::clone(&pause),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let timeout = Duration::from_secs(self.template.timeout_seconds);
        let thread_cancel = Arc::clone(&cancel);
        let thread_tx = tx.clone();
        let handle = std::thread::Builder::new()
            .name(format!("danso-turn-{}", &self.id[..8]))
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_turn(config, thread_tx.clone(), thread_cancel, timeout);
                }));
                if outcome.is_err() {
                    thread_tx
                        .send(AgentEvent::error(
                            ErrorCode::Runtime,
                            "worker turn panicked; journal retained. No automatic replay.",
                        ))
                        .ok();
                }
            })
            .context("could not start turn thread")?;
        *active = Some(ActiveTurn {
            cancel,
            pause,
            long_task: self.template.long_task.is_some(),
            handle,
        });
        drop(tx);
        Ok(rx)
    }

    fn interrupt(&self) {
        let active = self.active.lock().expect("turn lock");
        if let Some(turn) = active.as_ref()
            && !turn.handle.is_finished()
        {
            turn.cancel.notify_one();
        }
    }

    fn request_pause(&self) -> bool {
        let active = self.active.lock().expect("turn lock");
        match active.as_ref() {
            Some(turn) if turn.long_task && !turn.handle.is_finished() => {
                turn.pause.store(true, Ordering::Release);
                true
            }
            _ => false,
        }
    }
}
