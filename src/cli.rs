use clap::{Parser, ValueEnum};
use std::path::PathBuf;
#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    Json,
    Text,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SandboxArg {
    Host,
    Bubblewrap,
}

#[derive(Parser)]
#[command(version, about = "Headless worker harness with Pi session interchange")]
pub struct Args {
    /// Prompt for this run. Use -- to separate a prompt beginning with '-'.
    pub prompt: Option<String>,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
    /// JSONL v3 path outside the workspace. Existing linear sessions resume.
    #[arg(long)]
    pub session: PathBuf,
    #[arg(long)]
    pub model: Option<String>,
    /// Wire protocol / service to use. Defaults to the original Anthropic path.
    #[arg(long, default_value = "anthropic", value_parser = ["anthropic", "openai", "openai-codex", "glm"])]
    pub provider: String,
    /// Optional model-specific reasoning effort for OpenAI / GLM.
    #[arg(long, value_parser = ["none", "minimal", "low", "medium", "high", "xhigh", "max"])]
    pub reasoning_effort: Option<String>,
    #[arg(long, value_enum, default_value = "json")]
    pub mode: Mode,
    /// Final answer only; equivalent to --mode text.
    #[arg(short = 'p', long)]
    pub print: bool,
    /// Select JSONL output with body-free durable tool progress notifications.
    #[arg(long, conflicts_with = "print")]
    pub progress_jsonl: bool,
    /// Allow reading project AGENTS.md and skill metadata for this invocation.
    #[arg(long)]
    pub trust_project: bool,
    /// Disable all tool advertising and execution for this invocation.
    #[arg(long)]
    pub no_tools: bool,
    /// Explicit private UTF-8 system context (at most 32768 bytes), refreshed each run.
    #[arg(long)]
    pub system_context_file: Option<PathBuf>,
    /// Local long-term memory injection: off | read (read-write arrives in M4, #52).
    #[arg(long, value_parser = ["off", "read", "read-write"], default_value = "off")]
    pub memory: String,
    /// Memory root directory (scope directories live below it).
    #[arg(long)]
    pub memory_dir: Option<PathBuf>,
    /// Memory scope: global | shared | private-<32 hex>.
    #[arg(long)]
    pub memory_scope: Option<String>,
    /// Task-conditioned query for the local-hot block; default derives from the prompt.
    #[arg(long)]
    pub memory_query: Option<String>,
    /// Total snapshot budget in bytes (1..=24576, default 12000).
    #[arg(long)]
    pub memory_max_bytes: Option<usize>,
    /// Pin recall to an instant (tests/debugging).
    #[arg(long)]
    pub memory_as_of: Option<String>,
    /// Distill scheduling for read-write runs: queue | inline | off.
    #[arg(long, value_parser = ["queue", "inline", "off"], default_value = "queue")]
    pub memory_distill: String,
    /// Context refresh cadence: per-run | per-request (re-assembles after compaction).
    #[arg(long, value_parser = ["per-run", "per-request"], default_value = "per-run")]
    pub memory_refresh: String,
    /// Execution backend: host uses current-user permissions; bubblewrap isolates tools.
    #[arg(long, default_value = "host", value_enum)]
    pub sandbox: SandboxArg,
    /// Deprecated alias for --sandbox host. Cannot be combined with --sandbox.
    #[arg(long, conflicts_with = "sandbox")]
    pub unsafe_no_sandbox: bool,
    #[arg(long, default_value_t = 48)]
    pub max_turns: u32,
    /// Output token cap per provider response (256..=131072, default 16384;
    /// range enforced fail-closed in provider_from_parts). Maps to Anthropic
    /// max_tokens, OpenAI max_output_tokens, GLM max_tokens.
    #[arg(long, value_parser = clap::value_parser!(u32))]
    pub max_output_tokens: Option<u32>,
    /// GLM thinking toggle: enabled | disabled (default enabled; issue #70 A).
    #[arg(long, value_parser = ["enabled", "disabled"])]
    pub glm_thinking: Option<String>,
    /// GLM endpoint preset: general | coding (default general; issue #70 B).
    /// An explicit DANSO_GLM_BASE_URL wins but must not contradict the preset.
    #[arg(long, value_parser = ["general", "coding"])]
    pub glm_endpoint: Option<String>,
    /// Short-mode identical tool-batch guard: 0 disables (default), 2..8
    /// enables detection with a one-time system notice (issue #70 D).
    /// Long-task runs use --task-repeat-limit instead.
    #[arg(long, conflicts_with_all = ["long_task", "resume_task"])]
    pub repeat_limit: Option<u32>,
    /// Bounded provider wire retries for 429/5xx and pre-header transport
    /// failures (0..=5, default 3; issue #67 B).
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=5), default_value_t = 3)]
    pub provider_retries: u32,
    /// Continue a text-only output-cap stop up to N times (0..=2, default
    /// 0; issue #69 B). Unavailable in long-task mode.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=2), default_value_t = 0, conflicts_with_all = ["long_task", "resume_task"])]
    pub continue_on_length: u32,
    /// Emit per-request danso_request frames with --progress-jsonl
    /// (issue #69 F).
    #[arg(long, requires = "progress_jsonl")]
    pub stream_requests: bool,
    /// Opt in to checkpoint compaction above this serialized request size (8192..393216).
    #[arg(long)]
    pub compact_at_bytes: Option<usize>,
    /// Whole-run wall time. Short mode is 1..3600; long mode is 1..21600.
    #[arg(long)]
    pub timeout_seconds: Option<u64>,
    /// Total time per provider request, including response body (1..300 seconds).
    #[arg(long, default_value_t = 180)]
    pub provider_timeout_seconds: u64,
    /// Per-tool wall time. Host defaults to 900 seconds (maximum 3600);
    /// bubblewrap defaults to 30 seconds (maximum 300).
    #[arg(long)]
    pub tool_timeout_seconds: Option<u64>,
    /// Host-only HOME for development tools. Provider/context HOME stays native HOME.
    #[arg(long)]
    pub tool_home: Option<PathBuf>,
    /// Opt in to the bounded long-task journal and cumulative budgets (up to six active hours).
    #[arg(long, conflicts_with = "task_status")]
    pub long_task: bool,
    /// Resume a previously paused long task without appending a new prompt.
    #[arg(long, conflicts_with = "task_status")]
    pub resume_task: bool,
    /// Inspect a session without creating or mutating it, or contacting a provider.
    #[arg(long, conflicts_with_all = ["long_task", "resume_task", "print", "progress_jsonl"])]
    pub task_status: bool,
    /// Soft stage rollover target in model requests (1..1024, default 16).
    #[arg(long)]
    pub task_stage_requests: Option<u64>,
    /// Cumulative model request cap (1..2048, default 1024).
    #[arg(long)]
    pub task_max_requests: Option<u64>,
    /// Cumulative provider-reported token threshold (1..25000000, default 10000000).
    #[arg(long)]
    pub task_max_tokens: Option<u64>,
    /// Consecutive identical settled tool-batch limit (2..8, default 3).
    #[arg(long)]
    pub task_repeat_limit: Option<u64>,
    /// Pause after the numbered settled stage (1..2048).
    #[arg(long)]
    pub task_pause_after_stage: Option<u64>,
    /// Emit body-free DANSO_TASK checkpoint snapshots to stderr.
    #[arg(long)]
    pub task_progress: bool,
}

impl Args {
    /// Resolve the execution backend from the flags that select it. The
    /// deprecated alias is read here rather than inferred from --sandbox's
    /// default, so changing that default cannot silently invert its meaning.
    pub fn backend(&self) -> danso::app::Backend {
        if self.unsafe_no_sandbox {
            return danso::app::Backend::Host;
        }
        match self.sandbox {
            SandboxArg::Host => danso::app::Backend::Host,
            SandboxArg::Bubblewrap => danso::app::Backend::Bubblewrap,
        }
    }
    pub fn config(&self) -> danso::app::RunConfig {
        let long = self.long_task || self.resume_task;
        let backend = self.backend();
        let tool_timeout_seconds = self
            .tool_timeout_seconds
            .unwrap_or_else(|| danso::tools::tool_timeout_default(backend));
        let timeout_seconds = self.timeout_seconds.unwrap_or(if long {
            danso::long_task::MAX_WALL_SECONDS
        } else {
            1800
        });
        danso::app::RunConfig {
            prompt: self.prompt.clone().unwrap_or_default(),
            cwd: self.cwd.clone(),
            session: self.session.clone(),
            model: self.model.clone().unwrap_or_default(),
            provider: self.provider.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            trust_project: self.trust_project,
            no_tools: self.no_tools,
            system_context_file: self.system_context_file.clone(),
            memory: danso::memory::MemoryConfig {
                mode: match self.memory.as_str() {
                    "read" => danso::memory::MemoryMode::Read,
                    "read-write" => danso::memory::MemoryMode::ReadWrite,
                    _ => danso::memory::MemoryMode::Off,
                },
                root: self.memory_dir.clone(),
                scope: self
                    .memory_scope
                    .clone()
                    .unwrap_or_else(|| "global".to_string()),
                query: self.memory_query.clone(),
                max_bytes: self
                    .memory_max_bytes
                    .unwrap_or(danso::memory::snapshot::SNAPSHOT_MAX_BYTES_DEFAULT),
                as_of: self.memory_as_of.clone(),
                refresh: match self.memory_refresh.as_str() {
                    "per-request" => danso::memory::RefreshMode::PerRequest,
                    _ => danso::memory::RefreshMode::PerRun,
                },
            },
            memory_refresh: match self.memory_refresh.as_str() {
                "per-request" => danso::memory::RefreshMode::PerRequest,
                _ => danso::memory::RefreshMode::PerRun,
            },
            memory_distill: match self.memory_distill.as_str() {
                "inline" => danso::memory::DistillMode::Inline,
                "off" => danso::memory::DistillMode::Off,
                _ => danso::memory::DistillMode::Queue,
            },
            backend,
            max_turns: self.max_turns,
            max_output_tokens: self.max_output_tokens,
            glm_thinking: self.glm_thinking.clone(),
            glm_endpoint: self.glm_endpoint.clone(),
            repeat_limit: self.repeat_limit.unwrap_or(0),
            provider_retries: self.provider_retries,
            continuation_limit: self.continue_on_length,
            stream_requests: self.stream_requests,
            report_progress: self.progress_jsonl,
            compact_at_bytes: self.compact_at_bytes,
            timeout_seconds,
            provider_timeout_seconds: self.provider_timeout_seconds,
            tool_timeout_seconds,
            tool_home: self.tool_home.clone(),
            long_task: long.then(|| danso::runtime::LongTaskRun {
                limits: danso::long_task::Limits {
                    wall_seconds: timeout_seconds,
                    stage_requests: self
                        .task_stage_requests
                        .unwrap_or(danso::long_task::DEFAULT_STAGE_REQUESTS),
                    max_requests: self
                        .task_max_requests
                        .unwrap_or(danso::long_task::DEFAULT_MAX_REQUESTS),
                    max_tokens: self
                        .task_max_tokens
                        .unwrap_or(danso::long_task::DEFAULT_MAX_TOKENS),
                    repeat_limit: self
                        .task_repeat_limit
                        .unwrap_or(danso::long_task::DEFAULT_REPEAT_LIMIT),
                },
                explicit_limits: (u8::from(self.timeout_seconds.is_some()))
                    | (u8::from(self.task_stage_requests.is_some()) << 1)
                    | (u8::from(self.task_max_requests.is_some()) << 2)
                    | (u8::from(self.task_max_tokens.is_some()) << 3)
                    | (u8::from(self.task_repeat_limit.is_some()) << 4),
                resume: self.resume_task,
                pause_after_stage: self.task_pause_after_stage,
            }),
            task_progress: self.task_progress,
            pause_requested: None,
        }
    }
    pub fn output_mode(&self) -> danso::output::Mode {
        if self.progress_jsonl {
            danso::output::Mode::Json
        } else if self.print || matches!(self.mode, Mode::Text) {
            danso::output::Mode::Text
        } else {
            danso::output::Mode::Json
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use danso::app::Backend;

    const BASE: [&str; 6] = [
        "danso",
        "a prompt",
        "--session",
        "/tmp/s.jsonl",
        "--model",
        "m",
    ];

    fn parse(extra: &[&str]) -> Args {
        let mut argv = BASE.to_vec();
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    fn try_parse(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut argv = BASE.to_vec();
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    #[test]
    fn backend_selection_is_explicit() {
        assert_eq!(parse(&[]).backend(), Backend::Host);
        assert_eq!(parse(&["--sandbox", "host"]).backend(), Backend::Host);
        assert_eq!(
            parse(&["--sandbox", "bubblewrap"]).backend(),
            Backend::Bubblewrap
        );
        assert_eq!(parse(&["--unsafe-no-sandbox"]).backend(), Backend::Host);
    }

    #[test]
    fn deprecated_alias_conflicts_with_explicit_sandbox() {
        assert!(try_parse(&["--unsafe-no-sandbox", "--sandbox", "host"]).is_err());
        assert!(try_parse(&["--unsafe-no-sandbox", "--sandbox", "bubblewrap"]).is_err());
    }

    // The regression this guards: --unsafe-no-sandbox used to be inferred from
    // --sandbox's default rather than read. Under that implementation, flipping
    // the default to bubblewrap would have turned the flag into a silent no-op
    // that ran *sandboxed* — the opposite of its name. Simulate the flipped
    // default directly so the invariant does not depend on a default value.
    #[test]
    fn deprecated_alias_survives_a_changed_sandbox_default() {
        let mut args = parse(&["--unsafe-no-sandbox"]);
        assert!(args.unsafe_no_sandbox);
        args.sandbox = SandboxArg::Bubblewrap;
        assert_eq!(args.backend(), Backend::Host);
    }

    #[test]
    fn provider_timeout_default_and_explicit_override_are_stable() {
        assert_eq!(parse(&[]).provider_timeout_seconds, 180);
        assert_eq!(
            parse(&["--provider-timeout-seconds", "42"]).provider_timeout_seconds,
            42
        );
    }

    /// Issue #69 C: longer default budgets for real work. Explicit flags
    /// still win; the long-task wall default is unchanged.
    #[test]
    fn budget_defaults_are_extended_and_flags_still_win() {
        let config = parse(&[]).config();
        assert_eq!(config.max_turns, 48);
        assert_eq!(config.timeout_seconds, 1800);
        let config = parse(&["--max-turns", "16", "--timeout-seconds", "60"]).config();
        assert_eq!(config.max_turns, 16);
        assert_eq!(config.timeout_seconds, 60);
        assert_eq!(
            parse(&["--long-task"]).config().timeout_seconds,
            danso::long_task::MAX_WALL_SECONDS
        );
    }

    /// Issue #69 A: the flag is optional and passed through; range and env
    /// resolution happen fail-closed in provider_from_parts.
    #[test]
    fn max_output_tokens_flag_is_optional_and_preserved() {
        assert_eq!(parse(&[]).config().max_output_tokens, None);
        assert_eq!(
            parse(&["--max-output-tokens", "32768"])
                .config()
                .max_output_tokens,
            Some(32768)
        );
        // In-range parsing succeeds; the 256..=131072 range is enforced
        // fail-closed at provider construction (see provider::mod tests).
        assert_eq!(
            try_parse(&["--max-output-tokens", "0"])
                .unwrap()
                .config()
                .max_output_tokens,
            Some(0)
        );
        assert_eq!(
            try_parse(&["--max-output-tokens", "999999999"])
                .unwrap()
                .config()
                .max_output_tokens,
            Some(999999999)
        );
    }

    /// Issue #70 D: the short-mode repeat guard is off by default and is a
    /// short-mode-only flag — long-task keeps its own --task-repeat-limit.
    #[test]
    fn repeat_limit_is_short_mode_only() {
        assert_eq!(parse(&[]).config().repeat_limit, 0);
        assert_eq!(parse(&["--repeat-limit", "3"]).config().repeat_limit, 3);
        assert!(try_parse(&["--repeat-limit", "3", "--long-task"]).is_err());
        assert!(try_parse(&["--repeat-limit", "3", "--resume-task"]).is_err());
    }

    #[test]
    fn tool_timeout_defaults_follow_backend_and_explicit_values_are_preserved() {
        assert_eq!(
            parse(&[]).config().tool_timeout_seconds,
            danso::tools::HOST_TOOL_TIMEOUT_DEFAULT_SECONDS
        );
        assert_eq!(
            parse(&["--sandbox", "bubblewrap"])
                .config()
                .tool_timeout_seconds,
            danso::tools::BUBBLEWRAP_TOOL_TIMEOUT_DEFAULT_SECONDS
        );
        assert_eq!(
            parse(&["--tool-timeout-seconds", "3600"])
                .config()
                .tool_timeout_seconds,
            3600
        );
        assert_eq!(
            parse(&["--sandbox", "bubblewrap", "--tool-timeout-seconds", "300",])
                .config()
                .tool_timeout_seconds,
            300
        );
    }

    #[test]
    fn tool_home_is_optional_and_preserved_in_config() {
        assert!(parse(&[]).config().tool_home.is_none());
        let args = parse(&["--tool-home", "/opt/native-tool-home"]);
        assert_eq!(
            args.config().tool_home,
            Some(PathBuf::from("/opt/native-tool-home"))
        );
    }

    #[test]
    fn long_task_defaults_are_bounded_and_resume_omits_immutable_assertions() {
        let args = parse(&["--long-task"]);
        let config = args.config();
        let task = config.long_task.unwrap();
        assert_eq!(task.limits.wall_seconds, danso::long_task::MAX_WALL_SECONDS);
        assert_eq!(task.limits.stage_requests, 16);
        assert_eq!(task.limits.max_requests, 1024);
        assert_eq!(task.limits.max_tokens, 10_000_000);
        assert_eq!(task.limits.repeat_limit, 3);
        assert_eq!(task.explicit_limits, 0);

        let args = parse(&[
            "--resume-task",
            "--timeout-seconds",
            "60",
            "--task-stage-requests",
            "4",
            "--task-max-requests",
            "5",
            "--task-max-tokens",
            "500",
            "--task-repeat-limit",
            "2",
        ]);
        let task = args.config().long_task.unwrap();
        assert_eq!(task.explicit_limits, 31);
        assert_eq!(task.limits.wall_seconds, 60);
        assert_eq!(task.limits.stage_requests, 4);
        assert_eq!(task.limits.max_requests, 5);
        assert_eq!(task.limits.max_tokens, 500);
        assert_eq!(task.limits.repeat_limit, 2);
    }
}
