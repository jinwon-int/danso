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
    #[arg(long, value_enum, default_value = "json")]
    pub mode: Mode,
    /// Final answer only; equivalent to --mode text.
    #[arg(short = 'p', long)]
    pub print: bool,
    /// Allow reading project AGENTS.md and skill metadata for this invocation.
    #[arg(long)]
    pub trust_project: bool,
    /// Explicit opt-out from Linux bubblewrap isolation. For controlled tests only.
    #[arg(long)]
    pub unsafe_no_sandbox: bool,
    #[arg(long, default_value_t = 16)]
    pub max_turns: u32,
    #[arg(long, default_value_t = 300)]
    pub timeout_seconds: u64,
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
            trust_project: self.trust_project,
            unsafe_no_sandbox: self.unsafe_no_sandbox,
            max_turns: self.max_turns,
            timeout_seconds: self.timeout_seconds,
            tool_timeout_seconds: self.tool_timeout_seconds,
        }
    }
    pub fn output_mode(&self) -> danso::output::Mode {
        if self.print || matches!(self.mode, Mode::Text) {
            danso::output::Mode::Text
        } else {
            danso::output::Mode::Json
        }
    }
}
