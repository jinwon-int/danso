use clap::{Parser, ValueEnum};
use std::path::PathBuf;
#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    Json,
    Text,
}

#[derive(Parser)]
#[command(version, about = "Headless worker harness with Pi session interchange")]
pub struct Args {
    /// Prompt for this run. Use -- to separate a prompt beginning with '-'.
    pub prompt: String,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
    /// JSONL v3 path outside the workspace. Existing linear sessions resume.
    #[arg(long)]
    pub session: PathBuf,
    #[arg(long)]
    pub model: String,
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
    /// Execution backend: host uses current-user permissions; bubblewrap isolates tools.
    #[arg(long, default_value = "host", value_parser = ["host", "bubblewrap"])]
    pub sandbox: String,
    /// Deprecated alias for host execution. Cannot be combined with --sandbox.
    #[arg(long, conflicts_with = "sandbox")]
    pub unsafe_no_sandbox: bool,
    #[arg(long, default_value_t = 16)]
    pub max_turns: u32,
    /// Opt in to checkpoint compaction above this serialized request size (8192..393216).
    #[arg(long)]
    pub compact_at_bytes: Option<usize>,
    #[arg(long, default_value_t = 300)]
    pub timeout_seconds: u64,
    /// Total time per provider request, including response body (1..300 seconds).
    #[arg(long, default_value_t = 60)]
    pub provider_timeout_seconds: u64,
    #[arg(long, default_value_t = 30)]
    pub tool_timeout_seconds: u64,
}

impl Args {
    pub fn config(&self) -> danso::app::RunConfig {
        danso::app::RunConfig {
            prompt: self.prompt.clone(),
            cwd: self.cwd.clone(),
            session: self.session.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            trust_project: self.trust_project,
            no_tools: self.no_tools,
            system_context_file: self.system_context_file.clone(),
            unsafe_no_sandbox: self.sandbox == "host",
            max_turns: self.max_turns,
            compact_at_bytes: self.compact_at_bytes,
            timeout_seconds: self.timeout_seconds,
            provider_timeout_seconds: self.provider_timeout_seconds,
            tool_timeout_seconds: self.tool_timeout_seconds,
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
