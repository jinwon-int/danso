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
    /// Execution backend: host uses current-user permissions; bubblewrap isolates tools.
    #[arg(long, default_value = "host", value_enum)]
    pub sandbox: SandboxArg,
    /// Deprecated alias for --sandbox host. Cannot be combined with --sandbox.
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
    #[arg(long, default_value_t = 180)]
    pub provider_timeout_seconds: u64,
    #[arg(long, default_value_t = 30)]
    pub tool_timeout_seconds: u64,
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
            },
            memory_distill: match self.memory_distill.as_str() {
                "inline" => danso::memory::DistillMode::Inline,
                "off" => danso::memory::DistillMode::Off,
                _ => danso::memory::DistillMode::Queue,
            },
            backend: self.backend(),
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
}
