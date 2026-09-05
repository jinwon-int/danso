//! Composition root: resolves local configuration and chooses production adapters.
use crate::{
    context,
    contracts::EventSink,
    provider::anthropic::Anthropic,
    runtime::{self, RunInput},
    session::Session,
    tools::Runner,
    usage::Usage,
};
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

pub struct RunConfig {
    pub prompt: String,
    pub cwd: PathBuf,
    pub session: PathBuf,
    pub model: String,
    pub trust_project: bool,
    pub unsafe_no_sandbox: bool,
    pub max_turns: u32,
    pub timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
}

pub async fn run(args: &RunConfig, sink: &mut impl EventSink, usage: &mut Usage) -> Result<()> {
    ensure!(
        (1..=128).contains(&args.max_turns),
        "max-turns must be 1..128"
    );
    ensure!(
        (1..=3600).contains(&args.timeout_seconds)
            && (1..=300).contains(&args.tool_timeout_seconds),
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
    let ctx = context::discover(&cwd, &home, args.trust_project)?;
    let session = Session::open(&session_path, &cwd)?;
    let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is required")?;
    let base = std::env::var("DANSO_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".into());
    let mut provider = Anthropic::new(args.model.clone(), key, &base)?;
    let runner = Runner {
        cwd,
        readable: ctx.readable,
        unsafe_no_sandbox: args.unsafe_no_sandbox,
        timeout: Duration::from_secs(args.tool_timeout_seconds),
    };
    let mut session = session;
    runtime::run(
        RunInput {
            prompt: &args.prompt,
            context: &ctx.prompt,
            max_turns: args.max_turns,
        },
        &mut provider,
        &runner,
        &mut session,
        sink,
        usage,
    )
    .await
}
