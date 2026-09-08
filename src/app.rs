//! Composition root: resolves local configuration and chooses production adapters.
use crate::{
    context,
    contracts::EventSink,
    failure::{Kind, at},
    memory,
    runtime::{self, RunInput},
    session::Session,
    tools::Runner,
    usage::Usage,
};
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

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
    pub compact_at_bytes: Option<usize>,
    pub timeout_seconds: u64,
    pub provider_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
}

/// Build a production provider from explicit parts (shared by `danso run`
/// and the memory drain CLI; credentials come from the environment).
pub fn provider_from_parts(
    provider: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    provider_timeout_seconds: u64,
) -> Result<crate::provider::Selected> {
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
            "https://api.z.ai/api/paas/v4",
        ),
        _ => anyhow::bail!("unsupported provider"),
    };
    let key = std::env::var(key_name).with_context(|| format!("{key_name} is required"))?;
    let base = std::env::var(base_name).unwrap_or_else(|_| default_base.into());
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
                provider_timeout_seconds,
            )?,
        )),
        "glm" => Ok(crate::provider::Selected::Glm(
            crate::provider::glm::Glm::new_with_timeout(
                model.to_string(),
                key,
                &base,
                effort,
                provider_timeout_seconds,
            )?,
        )),
        _ => anyhow::bail!("unsupported provider"),
    }
}

pub async fn run(args: &RunConfig, sink: &mut impl EventSink, usage: &mut Usage) -> Result<()> {
    ensure!(
        (1..=128).contains(&args.max_turns),
        "max-turns must be 1..128"
    );
    ensure!(
        (1..=3600).contains(&args.timeout_seconds)
            && (1..=300).contains(&args.tool_timeout_seconds)
            && (1..=300).contains(&args.provider_timeout_seconds),
        "invalid timeout"
    );
    ensure!(
        !args.prompt.trim().is_empty() && args.prompt.len() <= context::CONTEXT_LIMIT,
        "prompt must be 1..65536 bytes"
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
        == memory::MemoryMode::Read
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
    )?;
    let execution_context = context::execution_context(&cwd);
    let runner = Runner {
        cwd,
        readable: ctx.readable,
        backend: args.backend,
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
        .map(|root| memory::Route::new(root, &args.memory.scope))
        .transpose()?;
    let mut recorder = memory_route
        .as_ref()
        .map(|route| memory::working_state::Recorder::new(route.clone(), &args.prompt));
    let distill_enqueue = args.memory.mode == memory::MemoryMode::ReadWrite
        && args.memory_distill != memory::DistillMode::Off;
    let mut recording = memory::working_state::RecordingSink::new(sink, recorder.as_mut());
    let run = runtime::run(
        RunInput {
            no_tools: args.no_tools,
            prompt: &args.prompt,
            context: &ctx.prompt,
            execution_context: &execution_context,
            max_turns: args.max_turns,
            compact_at_bytes: args.compact_at_bytes,
            refresh_context: refresh_context.as_deref(),
        },
        &mut provider,
        &runner,
        &mut session,
        &mut recording,
        usage,
    )
    .await;
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
