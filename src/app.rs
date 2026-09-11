//! Composition root: resolves local configuration and chooses production adapters.
use crate::{
    context,
    contracts::{EventSink, SessionStore},
    failure::{Kind, at},
    memory,
    runtime::{self, RunInput},
    session::Session,
    tools,
    tools::Runner,
    usage::Usage,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

/// Tool execution backend. This is the single source of truth for isolation:
/// no caller infers it from a flag default, and adding a variant forces every
/// match site to be revisited.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// Tools run as the current user, with the current user's permissions.
    Host,
    /// Tools run inside a bubblewrap namespace; isolation failure never falls
    /// back to host execution.
    Bubblewrap,
}
impl Backend {
    pub fn is_host(self) -> bool {
        self == Backend::Host
    }
}

pub struct RunConfig {
    pub prompt: String,
    pub cwd: PathBuf,
    pub session: PathBuf,
    pub model: String,
    pub provider: String,
    pub reasoning_effort: Option<String>,
    pub trust_project: bool,
    pub no_tools: bool,
    pub system_context_file: Option<PathBuf>,
    pub memory: memory::MemoryConfig,
    pub memory_refresh: memory::RefreshMode,
    pub memory_distill: memory::DistillMode,
    pub backend: Backend,
    pub max_turns: u32,
    /// Explicit `--max-output-tokens`; env and default resolve in
    /// `provider_from_parts` (issue #69 A).
    pub max_output_tokens: Option<u32>,
    /// GLM-only options resolved in `provider_from_parts` (issue #70 A/B).
    pub glm_thinking: Option<String>,
    pub glm_endpoint: Option<String>,
    /// Bounded wire-level retry budget (0..=5; issue #67 B).
    pub provider_retries: u32,
    /// Opt-in output-cap continuation budget (0..=2; issue #69 B).
    pub continuation_limit: u32,
    /// Opt-in per-request progress frames (issue #69 F).
    pub stream_requests: bool,
    /// Ask for concise user-facing updates alongside tool calls.
    pub report_progress: bool,
    /// Short-mode identical-batch guard (issue #70 D); 0 disables.
    pub repeat_limit: u32,
    pub compact_at_bytes: Option<usize>,
    pub timeout_seconds: u64,
    pub provider_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
    /// Optional host-only HOME for child development tools. The native HOME
    /// remains the source for provider auth and context discovery.
    pub tool_home: Option<PathBuf>,
    pub long_task: Option<runtime::LongTaskRun>,
    pub task_progress: bool,
    /// Process-local graceful-pause request, installed by the CLI signal
    /// handler. Library callers may leave this unset.
    pub pause_requested: Option<Arc<AtomicBool>>,
    pub cancellation_reason: Option<Arc<AtomicU8>>,
}

/// Build a production provider from explicit parts (shared by `danso run`
/// and the memory drain CLI; credentials come from the environment). The
/// output token cap resolves from the explicit flag, then
/// `DANSO_MAX_OUTPUT_TOKENS`, then the default (issue #69 A). GLM-only
/// options (`glm_thinking` enabled|disabled, `glm_endpoint` general|coding)
/// resolve flags → env → defaults and fail closed on conflicts (issue #70 A/B).
#[allow(clippy::too_many_arguments)]
pub fn provider_from_parts(
    provider: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    provider_timeout_seconds: u64,
    max_output_tokens: Option<u32>,
    glm_thinking: Option<&str>,
    glm_endpoint: Option<&str>,
) -> Result<crate::provider::Selected> {
    let max_output_tokens = crate::provider::resolve_max_output_tokens(max_output_tokens)?;
    let effort = reasoning_effort.map(str::to_string);
    ensure!(!model.trim().is_empty(), "model must not be empty");
    if let Some(effort) = &effort {
        ensure!(
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()),
            "invalid reasoning effort"
        );
    }
    if provider == "openai-codex" {
        let auth = std::env::var_os("DANSO_CHATGPT_AUTH_FILE")
            .context("DANSO_CHATGPT_AUTH_FILE is required")?;
        let base = std::env::var("DANSO_CHATGPT_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".into());
        return Ok(crate::provider::Selected::OpenAi(
            crate::provider::openai::OpenAi::new_chatgpt(
                model.to_string(),
                std::path::Path::new(&auth),
                &base,
                effort,
                provider_timeout_seconds,
            )?,
        ));
    }
    let (key_name, base_name, default_base) = match provider {
        "anthropic" => (
            "ANTHROPIC_API_KEY",
            "DANSO_ANTHROPIC_BASE_URL",
            "https://api.anthropic.com",
        ),
        "openai" => (
            "OPENAI_API_KEY",
            "DANSO_OPENAI_BASE_URL",
            "https://api.openai.com/v1",
        ),
        "glm" => (
            "ZAI_API_KEY",
            "DANSO_GLM_BASE_URL",
            crate::provider::glm::GLM_GENERAL_BASE_URL,
        ),
        _ => anyhow::bail!("unsupported provider"),
    };
    let key = std::env::var(key_name).with_context(|| format!("{key_name} is required"))?;
    let base = if provider == "glm" {
        // GLM resolves the endpoint preset first so an explicit base URL can
        // be cross-checked against it (#70 B); the default below is unused.
        let explicit = std::env::var(base_name).ok();
        crate::provider::glm::resolve_base_url(glm_endpoint, explicit.as_deref())?
    } else {
        std::env::var(base_name).unwrap_or_else(|_| default_base.into())
    };
    match provider {
        "anthropic" => {
            ensure!(
                effort.is_none(),
                "reasoning-effort is unsupported by the Anthropic adapter"
            );
            Ok(crate::provider::Selected::Anthropic(
                crate::provider::anthropic::Anthropic::new_with_timeout(
                    model.to_string(),
                    key,
                    &base,
                    max_output_tokens,
                    provider_timeout_seconds,
                )?,
            ))
        }
        "openai" => Ok(crate::provider::Selected::OpenAi(
            crate::provider::openai::OpenAi::new_with_timeout(
                model.to_string(),
                key,
                &base,
                effort,
                max_output_tokens,
                provider_timeout_seconds,
            )?,
        )),
        "glm" => {
            let thinking = crate::provider::glm::resolve_thinking(glm_thinking)?;
            Ok(crate::provider::Selected::Glm(
                crate::provider::glm::Glm::new_with_timeout(
                    model.to_string(),
                    key,
                    &base,
                    effort,
                    max_output_tokens,
                    thinking,
                    provider_timeout_seconds,
                )?,
            ))
        }
        _ => anyhow::bail!("unsupported provider"),
    }
}

pub async fn run(args: &RunConfig, sink: &mut impl EventSink, usage: &mut Usage) -> Result<()> {
    ensure!(
        (1..=128).contains(&args.max_turns),
        "max-turns must be 1..128"
    );
    ensure!(
        args.repeat_limit == 0 || (2..=8).contains(&args.repeat_limit),
        "repeat-limit must be 0 or 2..=8"
    );
    ensure!(args.provider_retries <= 5, "provider-retries must be 0..=5");
    ensure!(
        args.continuation_limit <= 2,
        "continue-on-length must be 0..=2"
    );
    ensure!(
        args.continuation_limit == 0 || args.long_task.is_none(),
        "continue-on-length is unavailable in long-task mode"
    );
    ensure!(
        (1..=if args.long_task.is_some() {
            crate::long_task::MAX_WALL_SECONDS
        } else {
            3600
        })
            .contains(&args.timeout_seconds)
            && (1..=tools::tool_timeout_max(args.backend)).contains(&args.tool_timeout_seconds)
            && (1..=300).contains(&args.provider_timeout_seconds),
        "invalid timeout"
    );
    ensure!(
        (args.long_task.as_ref().is_some_and(|task| task.resume) || !args.prompt.trim().is_empty())
            && args.prompt.len() <= context::CONTEXT_LIMIT,
        "prompt must be 1..65536 bytes (resume-task takes no prompt)"
    );
    if let Some(task) = args.long_task {
        ensure!(
            task.explicit_limits & !31 == 0,
            "invalid long-task limit mask"
        );
        task.limits.validate()?;
        if let Some(stage) = task.pause_after_stage {
            ensure!((1..=crate::long_task::MAX_MAX_REQUESTS).contains(&stage));
        }
    }
    ensure!(
        !(args.long_task.is_some()
            && args.memory.mode == memory::MemoryMode::ReadWrite
            && args.memory_distill == memory::DistillMode::Inline),
        "inline memory distillation is unavailable in long-task mode; use queue or off"
    );
    ensure!(
        std::env::var_os("PIRI_BOOTSTRAP_CONTEXT_FILE").is_none()
            && std::env::var_os("DANSO_BOOTSTRAP_CONTEXT_FILE").is_none(),
        "wrapper bootstrap injection is unsupported; use discovered context exactly once"
    );
    let cwd = args
        .cwd
        .canonicalize()
        .context("workspace does not exist")?;
    ensure!(
        cwd.is_dir() && cwd.parent().is_some(),
        "workspace must be a non-root directory"
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is required")?);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    let tool_home = tools::resolve_tool_home(args.backend, args.tool_home.as_deref(), &home)?;
    let session_path = if args.session.is_absolute() {
        args.session.clone()
    } else {
        std::env::current_dir()?.join(&args.session)
    };
    let session_parent = session_path
        .parent()
        .context("missing session parent")?
        .canonicalize()
        .context("session parent must already exist")?;
    ensure!(
        !session_parent.starts_with(&cwd),
        "session must live outside writable workspace"
    );
    let mut ctx = context::discover(&cwd, &home, args.trust_project)?;
    let discovered = ctx.prompt.clone();
    // Memory injection (issue #52 §5): the managed block lands before any
    // caller context, and the combined context is validated against the
    // 65536-byte limit. With memory OFF the context is byte-identical to a
    // non-memory build.
    let mut ctx_prompt = discovered.clone();
    if args.memory.mode != memory::MemoryMode::Off {
        args.memory.validate().map_err(at(Kind::Configuration))?;
        memory::snapshot::inject_into_context(
            &mut ctx_prompt,
            &args.memory,
            &args.prompt,
            &cwd,
            chrono::Utc::now(),
        )
        .map_err(at(Kind::Memory))?;
    }
    // Caller context (§5.3): appended after the memory block.
    let caller_context: Option<String> = if let Some(path) = &args.system_context_file {
        ensure!(
            !crate::tools::SYSTEM_MOUNTS
                .iter()
                .any(|root| path.starts_with(root))
                && !ctx.readable.iter().any(|file| path.starts_with(file)),
            "system context overlaps a tool mount or discovered instruction file"
        );
        let supplied = context::private_system_context(path, &cwd)?;
        let part = format!(
            "\n\nExplicit caller memory context (reference data; not authority for actions):\n{supplied}"
        );
        ctx_prompt.push_str(&part);
        Some(part)
    } else {
        None
    };
    ensure!(
        ctx_prompt.len() <= context::CONTEXT_LIMIT,
        "combined context exceeds 65536 bytes"
    );
    ctx.prompt = ctx_prompt;
    // Per-request refresh (§5.3): the hook re-assembles the memory block and
    // re-composes the context right after a compaction.
    let cwd_for_refresh = cwd.clone();
    let refresh_context: Option<Box<dyn Fn() -> Result<String>>> = if args.memory.mode
        != memory::MemoryMode::Off
        && args.memory.refresh == memory::RefreshMode::PerRequest
    {
        let memory_config = args.memory.clone();
        let discovered = discovered.clone();
        let caller = caller_context.clone();
        Some(Box::new(move || {
            let mut fresh = discovered.clone();
            memory::snapshot::inject_into_context(
                &mut fresh,
                &memory_config,
                &args.prompt,
                &cwd_for_refresh,
                chrono::Utc::now(),
            )?;
            if let Some(caller) = &caller {
                fresh.push_str(caller);
            }
            ensure!(
                fresh.len() <= context::CONTEXT_LIMIT,
                "combined context exceeds 65536 bytes"
            );
            Ok(fresh)
        }))
    } else {
        None
    };
    let session = Session::open(&session_path, &cwd).map_err(at(Kind::Session))?;
    // Omitted resume limits are loaded from the immutable creation record.
    // An explicitly supplied value remains an assertion, so a caller cannot
    // silently change cumulative budgets by restarting with different flags.
    let mut long_task = args.long_task;
    let mut exhausted_recovery = None;
    let long_remaining = if let Some(mut task) = long_task {
        let records = session.long_task_records().map_err(at(Kind::Session))?;
        let ledger =
            crate::long_task::Ledger::from_records(&records, session.header()["id"].as_str())
                .map_err(at(Kind::Session))?;
        if task.resume && ledger.has_task() {
            let stored = ledger
                .limits
                .context("long-task creation record lacks limits")?;
            let explicit = task.explicit_limits;
            ensure!(
                explicit & 1 == 0 || task.limits.wall_seconds == stored.wall_seconds,
                "resume-task wall limit must match the immutable task limit"
            );
            ensure!(
                explicit & 2 == 0 || task.limits.stage_requests == stored.stage_requests,
                "resume-task stage limit must match the immutable task limit"
            );
            ensure!(
                explicit & 4 == 0 || task.limits.max_requests == stored.max_requests,
                "resume-task request limit must match the immutable task limit"
            );
            ensure!(
                explicit & 8 == 0 || task.limits.max_tokens == stored.max_tokens,
                "resume-task token limit must match the immutable task limit"
            );
            ensure!(
                explicit & 16 == 0 || task.limits.repeat_limit == stored.repeat_limit,
                "resume-task repeat limit must match the immutable task limit"
            );
            task.limits = stored;
            long_task = Some(task);
        }
        if ledger.has_task()
            && !matches!(
                ledger.state,
                crate::long_task::State::Completed | crate::long_task::State::Failed
            )
        {
            let cap = task.limits.wall_seconds.saturating_mul(1000);
            let remaining = cap.saturating_sub(ledger.elapsed_ms);
            if remaining == 0 {
                // The deadline can reject before runtime::run validates the
                // full journal. Never attach advice from unvalidated bindings.
                session.check_recovery().map_err(at(Kind::Session))?;
                exhausted_recovery = Some(ledger.recovery_error());
            }
            Some(remaining)
        } else {
            Some(task.limits.wall_seconds.saturating_mul(1000))
        }
    } else {
        None
    };
    ensure!(!args.model.trim().is_empty(), "model must not be empty");
    if let Some(effort) = &args.reasoning_effort {
        ensure!(
            ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort.as_str()),
            "invalid reasoning effort"
        );
    }
    let mut provider = provider_from_parts(
        &args.provider,
        &args.model,
        args.reasoning_effort.as_deref(),
        args.provider_timeout_seconds,
        args.max_output_tokens,
        args.glm_thinking.as_deref(),
        args.glm_endpoint.as_deref(),
    )?;
    provider.set_retries(args.provider_retries);
    let limits =
        tools::resource_limits(args.backend, Duration::from_secs(args.tool_timeout_seconds));
    let backend = if args.backend.is_host() {
        "host"
    } else {
        "bubblewrap"
    };
    let network = if args.backend.is_host() {
        "current-user network access"
    } else {
        "unshared network namespace"
    };
    let execution_context = context::execution_context_with_capabilities(
        &cwd,
        &format!(
            concat!(
                "backend={}; {}; tool_wall_seconds={}; ",
                "configured_rlimit_as_bytes={}; configured_rlimit_fsize_bytes={}; ",
                "configured_rlimit_nofile={}; configured_rlimit_cpu_seconds={}; ",
                "tool_environment=cleared; inherited_OS_hard_limits_may_tighten",
            ),
            backend,
            network,
            args.tool_timeout_seconds,
            limits.address_space_bytes,
            limits.file_size_bytes,
            limits.open_files,
            limits.cpu_seconds,
        ),
    );
    let runner = Runner {
        cwd,
        readable: ctx.readable,
        backend: args.backend,
        home: tool_home,
        timeout: Duration::from_secs(args.tool_timeout_seconds),
    };
    let mut session = session;
    // The harness records working state on compaction and at a finished run
    // (§5.3); the runtime itself stays memory-agnostic.
    let memory_root = if args.memory.mode != memory::MemoryMode::Off {
        Some(
            args.memory
                .root
                .clone()
                .unwrap_or_else(memory::MemoryConfig::default_root),
        )
    } else {
        None
    };
    let memory_route = memory_root
        .as_deref()
        .map(|root| {
            let route = memory::Route::new(root, &args.memory.scope)?;
            match &args.memory.legacy_read {
                Some(dir) => route.with_legacy(dir),
                None => Ok(route),
            }
        })
        .transpose()?;
    // §8: read mode injects only — the harness records nothing (no
    // working-state, checkpoints, or session-archive writes).
    let mut recorder = match (args.memory.mode, memory_route.as_ref()) {
        (memory::MemoryMode::ReadWrite, Some(route)) => Some(memory::working_state::Recorder::new(
            route.clone(),
            &args.prompt,
        )),
        _ => None,
    };
    let distill_enqueue = args.memory.mode == memory::MemoryMode::ReadWrite
        && args.memory_distill != memory::DistillMode::Off;
    let mut recording = memory::working_state::RecordingSink::new(sink, recorder.as_mut());
    let run = {
        let run_future = runtime::run(
            RunInput {
                no_tools: args.no_tools,
                prompt: &args.prompt,
                context: &ctx.prompt,
                execution_context: &execution_context,
                max_turns: args.max_turns,
                compact_at_bytes: args.compact_at_bytes,
                refresh_context: refresh_context.as_deref(),
                long_task,
                repeat_limit: args.repeat_limit,
                continuation_limit: args.continuation_limit,
                stream_requests: args.stream_requests,
                report_progress: args.report_progress,
                pause_requested: args.pause_requested.as_deref(),
                cancellation_reason: args.cancellation_reason.as_deref(),
            },
            &mut provider,
            &runner,
            &mut session,
            &mut recording,
            usage,
        );
        if let Some(remaining) = long_remaining {
            if remaining == 0 {
                Err(at(Kind::RunTimeout)(exhausted_recovery.unwrap_or_else(
                    || anyhow::anyhow!("long-task wall budget exhausted"),
                )))
            } else {
                tokio::select! {
                    result = run_future => result,
                    _ = async {
                        tokio::time::sleep(Duration::from_millis(remaining)).await;
                        if let Some(reason) = &args.cancellation_reason {
                            reason.store(3, Ordering::Release);
                        }
                    } => Err(at(Kind::RunTimeout)(anyhow::anyhow!("long-task wall budget exhausted"))),
                }
            }
        } else {
            run_future.await
        }
    };
    if run.is_ok()
        && let Some(recorder) = recorder.as_mut()
    {
        recorder.on_run_end().map_err(at(Kind::Memory))?;
    }
    // Distill enqueue (§4.7): final answers and turn-budget exhaustion both
    // leave a durable pending job; drain runs inline only when requested.
    if distill_enqueue && let Some(route) = &memory_route {
        let trigger = match &run {
            Ok(()) => "final_answer",
            Err(error) if crate::failure::category(error) == Some(Kind::RequestBudget) => {
                "budget_exhausted"
            }
            _ => "",
        };
        if !trigger.is_empty() {
            memory::distill::journal::enqueue(route, &session_path, trigger, chrono::Utc::now())
                .map_err(at(Kind::Memory))?;
        }
        if run.is_ok() && args.memory_distill == memory::DistillMode::Inline {
            memory::distill::extract::drain(
                route,
                &mut provider,
                usage,
                1,
                args.provider_timeout_seconds * 1000,
                chrono::Utc::now(),
            )
            .await
            .map_err(at(Kind::Memory))?;
        }
    }
    run.map_err(at(Kind::Runtime))
}
