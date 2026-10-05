//! Channel-neutral run settings (#213 ①).
//!
//! What a chat turn runs with — provider, model, limits, memory route, long
//! task limits — resolved from the environment and `config.toml` the same way
//! for every channel. Moved verbatim from the Telegram service; `label` names
//! the channel in error text so the Telegram messages are unchanged.

use crate::settings::{self, Layered};
use anyhow::{Context, Result, bail, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8},
    },
};

const DEFAULT_MAX_TURNS: u32 = 48;
const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
const DEFAULT_PROVIDER_TIMEOUT_SECONDS: u64 = 180;
const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 900;
const DEFAULT_PROVIDER_RETRIES: u32 = 3;
const DEFAULT_HEARTBEAT_SECONDS: u64 = 60;
const DEFAULT_FOLLOWUP_CAP: usize = 5;
const MAX_HEARTBEAT_SECONDS: u64 = 3600;
const MAX_FOLLOWUP_CAP: u64 = 100;
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

#[derive(Clone)]
pub(crate) struct RunSettings {
    pub(crate) provider: String,
    pub(crate) default_model: String,
    pub(crate) default_effort: Option<String>,
    pub(crate) workspace: PathBuf,
    pub(crate) trust_project: bool,
    pub(crate) no_tools: bool,
    pub(crate) max_turns: u32,
    pub(crate) timeout_seconds: u64,
    pub(crate) provider_timeout_seconds: u64,
    pub(crate) tool_timeout_seconds: u64,
    pub(crate) provider_retries: u32,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) compact_at_bytes: Option<usize>,
    pub(crate) heartbeat_seconds: u64,
    pub(crate) followup_cap: usize,
    pub(crate) memory_mode: crate::memory::MemoryMode,
    pub(crate) memory_root: PathBuf,
    pub(crate) memory_scope: String,
    pub(crate) task_limits: crate::long_task::Limits,
}

impl RunSettings {
    /// Environment first, then `config.toml`, then the default (#136). The
    /// file keys mirror the environment names; an error names the source
    /// that actually supplied the value.
    pub(crate) fn resolve(layered: &Layered, label: &'static str) -> Result<Self> {
        let provider = settings::resolve_string(
            layered,
            settings::aliases("provider.name"),
            "provider.name",
            |c| c.provider.name.clone(),
        )?
        .map(|(value, _)| value)
        .unwrap_or_else(|| settings::DEFAULT_PROVIDER.to_string());
        ensure!(
            ["anthropic", "openai", "openai-codex", "glm"].contains(&provider.as_str()),
            "unsupported {label} provider"
        );

        let model_names = settings::model_env_names(&provider);
        let default_model = settings::resolve_string(
            layered,
            &model_names,
            "provider.model",
            |c| c.provider.model.clone(),
        )?
        .map(|(value, _)| value)
        .context(
            "DANSO_TELEGRAM_MODEL, the normal Danso model environment, or provider.model in config.toml is required",
        )?;
        validate_model(&default_model, label)?;

        let default_effort = settings::resolve_string(
            layered,
            settings::aliases("provider.reasoning_effort"),
            "provider.reasoning_effort",
            |c| c.provider.reasoning_effort.clone(),
        )?
        .map(|(value, _)| value);
        validate_effort(default_effort.as_deref(), &provider, label)?;

        let workspace = settings::resolve_string(
            layered,
            settings::aliases("core.workspace"),
            "core.workspace",
            |c| c.core.workspace.as_ref().map(|p| p.display().to_string()),
        )?
        .map(|(value, _)| PathBuf::from(value))
        .unwrap_or(std::env::current_dir().with_context(|| format!("resolve {label} workspace"))?);
        ensure!(
            workspace.is_absolute(),
            "{label} workspace must be an absolute path"
        );
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("{label} workspace does not exist"))?;
        ensure!(
            workspace.is_dir() && workspace.parent().is_some(),
            "{label} workspace must be a non-root directory"
        );

        let max_turns = settings::resolve_u32(
            layered,
            settings::aliases("core.max_turns"),
            "core.max_turns",
            |c| c.core.max_turns,
            DEFAULT_MAX_TURNS,
            1,
            128,
        )?;
        let timeout_seconds = settings::resolve_u64(
            layered,
            settings::aliases("core.timeout_seconds"),
            "core.timeout_seconds",
            |c| c.core.timeout_seconds,
            DEFAULT_TIMEOUT_SECONDS,
            1,
            3600,
        )?;
        let provider_timeout_seconds = settings::resolve_u64(
            layered,
            settings::aliases("provider.timeout_seconds"),
            "provider.timeout_seconds",
            |c| c.provider.timeout_seconds,
            DEFAULT_PROVIDER_TIMEOUT_SECONDS,
            1,
            300,
        )?;
        let tool_timeout_seconds = settings::resolve_u64(
            layered,
            settings::aliases("core.tool_timeout_seconds"),
            "core.tool_timeout_seconds",
            |c| c.core.tool_timeout_seconds,
            DEFAULT_TOOL_TIMEOUT_SECONDS,
            1,
            crate::tools::HOST_TOOL_TIMEOUT_MAX_SECONDS,
        )?;
        let provider_retries = settings::resolve_u32(
            layered,
            settings::aliases("provider.retries"),
            "provider.retries",
            |c| c.provider.retries,
            DEFAULT_PROVIDER_RETRIES,
            0,
            5,
        )?;
        let max_output_tokens = settings::resolve_optional_u32(
            layered,
            settings::aliases("provider.max_output_tokens"),
            "provider.max_output_tokens",
            |c| c.provider.max_output_tokens,
        )?;
        let compact_at_bytes = settings::env_optional_usize(&[
            "DANSO_TELEGRAM_COMPACT_AT_BYTES",
            "DANSO_COMPACT_AT_BYTES",
        ])?;
        let heartbeat_seconds = settings::resolve_u64(
            layered,
            settings::aliases("telegram.heartbeat_seconds"),
            "telegram.heartbeat_seconds",
            |c| c.telegram.heartbeat_seconds,
            DEFAULT_HEARTBEAT_SECONDS,
            0,
            MAX_HEARTBEAT_SECONDS,
        )?;
        let followup_cap = settings::resolve_u64(
            layered,
            &["DANSO_TELEGRAM_FOLLOWUP_CAP"],
            "(none)",
            |_| None,
            DEFAULT_FOLLOWUP_CAP as u64,
            0,
            MAX_FOLLOWUP_CAP,
        )? as usize;
        // `memory.mode` decides whether a turn carries the managed memory
        // block (#209). `off` stays the default, so a node that never set it
        // keeps sending exactly what it sent before; `memory.scope` and
        // `memory.dir` below pick the route only once it is on.
        let memory_mode = match settings::resolve_string(
            layered,
            settings::aliases("memory.mode"),
            "memory.mode",
            |c| c.memory.mode.clone(),
        )?
        .map(|(value, _)| value)
        .as_deref()
        {
            None | Some("off") => crate::memory::MemoryMode::Off,
            Some("read") => crate::memory::MemoryMode::Read,
            Some("read-write") => crate::memory::MemoryMode::ReadWrite,
            Some(_) => {
                bail!("DANSO_TELEGRAM_MEMORY_MODE or memory.mode must be off, read, or read-write")
            }
        };
        let memory_scope = settings::resolve_string(
            layered,
            settings::aliases("memory.scope"),
            "memory.scope",
            |c| c.memory.scope.clone(),
        )?
        .map(|(value, _)| value)
        .unwrap_or_else(|| "global".to_string());
        ensure!(
            crate::memory::valid_scope(&memory_scope),
            "DANSO_TELEGRAM_MEMORY_SCOPE or memory.scope must be global, shared, or private-<32 hex>"
        );
        let memory_root = crate::memory::MemoryConfig::resolve_root(
            layered.config().and_then(|c| c.memory.dir.clone()),
        )?;
        ensure!(
            memory_root.is_absolute(),
            "DANSO_MEMORY_DIR or memory.dir must be an absolute path"
        );
        let task_limits = task_limits_from_env()?;

        Ok(Self {
            provider,
            default_model,
            default_effort,
            workspace,
            trust_project: settings::env_bool(
                &["DANSO_TELEGRAM_TRUST_PROJECT", "DANSO_TRUST_PROJECT"],
                false,
            )?,
            no_tools: settings::env_bool(&["DANSO_TELEGRAM_NO_TOOLS", "DANSO_NO_TOOLS"], false)?,
            max_turns,
            timeout_seconds,
            provider_timeout_seconds,
            tool_timeout_seconds,
            provider_retries,
            max_output_tokens,
            compact_at_bytes,
            heartbeat_seconds,
            followup_cap,
            memory_mode,
            memory_root,
            memory_scope,
            task_limits,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn config(
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
                mode: self.memory_mode,
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

pub(crate) fn task_limits_from_env() -> Result<crate::long_task::Limits> {
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

fn configured_u64(names: &[&str], default: u64, min: u64, max: u64) -> Result<u64> {
    settings::resolve_u64(
        &Layered::without_file(),
        names,
        "(none)",
        |_| None,
        default,
        min,
        max,
    )
}

pub(crate) fn validate_model(model: &str, label: &'static str) -> Result<()> {
    ensure!(!model.trim().is_empty(), "{label} model must not be empty");
    ensure!(model.len() <= 4096, "{label} model is too long");
    ensure!(
        model
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace()),
        "{label} model must be one non-whitespace token"
    );
    Ok(())
}

pub(crate) fn validate_effort(
    effort: Option<&str>,
    provider: &str,
    label: &'static str,
) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };
    ensure!(
        ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort),
        "{label} effort is invalid"
    );
    ensure!(
        provider != "anthropic",
        "reasoning effort is unsupported by the Anthropic adapter"
    );
    Ok(())
}
