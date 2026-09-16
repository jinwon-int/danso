//! `danso service` — the resident-service surface (`docs/unified-design.md` §6.4).
//!
//! This is the composition root for the service: it owns the CLI, the process
//! bookkeeping and the environment, and it hands the actual work to the
//! existing Telegram loop. `run` does not implement a second polling loop —
//! duplicating the runtime is the failure this module exists to avoid, and
//! `danso telegram` stays as an alias for the same path.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use danso_ops::{
    health::{self, HEALTH_FILE_NAME, RunMode},
    pidfile::{PidFile, SERVICE_PID_FILE_NAME, ServicePid},
    status::{StatusInputs, StatusReport},
    stop::{StopOutcome, TURN_DRAIN_SECS},
};
use std::{path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "danso service",
    about = "Run and inspect the resident Danso service. No provider or network access from `status`."
)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Run the service in the foreground with pid and health bookkeeping.
    Run {
        /// State root. Defaults to the Telegram data directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Supervise the service instead of running it directly.
        ///
        /// For hosts without systemd (Termux). Restarts the service after a
        /// crash and gives up on a crash loop rather than spinning.
        #[arg(long)]
        supervise: bool,
    },
    /// Report whether the service is available, degraded or unavailable.
    Status {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
    },
    /// Stop the running service within a bounded wall-clock budget.
    Stop {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Outer wall-clock budget for the whole stop, in seconds.
        ///
        /// This is not the turn drain: the service drains active turns for
        /// `TURN_DRAIN_SECS` and then tears down, and this budget has to cover
        /// both. Values below the default can cut teardown short.
        #[arg(long, default_value_t = danso_ops::stop::DEFAULT_GRACE_SECS,
              value_parser = grace_secs)]
        grace_secs: u64,
    },
}

/// Reject an out-of-range budget during argument parsing, which runs before
/// any signal is sent. ccc-node makes the same ordering explicit; a rejected
/// budget must leave the service exactly as it was, not half-stopped.
fn grace_secs(raw: &str) -> Result<u64, String> {
    danso_ops::stop::parse_grace_secs(raw)
}

/// Whether an argv belongs to a Danso service process.
///
/// Used to attribute a held token lock. It must not match an unrelated process
/// that merely has the lock file open, and it must not match this very
/// inspection — `status` never takes the lock, so it never appears as a holder.
pub fn is_service_argv(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let is_danso = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "danso" || name.starts_with("danso-"));
    if !is_danso {
        return false;
    }
    argv.iter()
        .skip(1)
        .any(|arg| arg == "service" || arg == "telegram")
        && !argv.iter().any(|arg| arg == "status")
}

fn resolve_data_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None => crate::telegram::data_dir_from_env().context(crate::telegram::DATA_DIR_ENV),
    }
}

/// Run the service in the foreground. Returns only when the loop ends.
///
/// SIGTERM leaves the polling loop, which stops admitting new turns, then
/// drains the turns already running for [`TURN_DRAIN_SECS`]. Teardown — the
/// token lock and the pid record — happens after that, on the way out of this
/// function, which is what the outer budget's remaining seconds pay for.
pub fn run(data_dir: Option<PathBuf>) -> Result<()> {
    let data_dir = resolve_data_dir(data_dir)?;
    // The loop resolves its own state root from the environment, so an explicit
    // `--data-dir` has to be published there rather than threaded separately;
    // two sources for one path is how they drift apart.
    unsafe { std::env::set_var(crate::telegram::DATA_DIR_ENV, &data_dir) };
    crate::telegram::ensure_private_dir(&data_dir)?;
    health::set_run_mode(RunMode::Run);

    let argv: Vec<String> = std::env::args().collect();
    // Bookkeeping is acquired before the loop so a second instance is refused
    // here rather than after it has started touching state. The token lock is
    // still the ownership signal; this only makes ownership *trackable*.
    let _pid = PidFile::acquire(
        &data_dir,
        SERVICE_PID_FILE_NAME,
        ServicePid::for_current_process(&argv),
    )?;

    let service = crate::telegram::TelegramService::from_env()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the service runtime")?;
    let report = runtime.block_on(async move {
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        let signalled = std::sync::Arc::clone(&shutdown);
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            signalled.notify_waiters();
        });
        service
            .run_until_drained(shutdown, Duration::from_secs(TURN_DRAIN_SECS))
            .await
    })?;
    if !report.is_complete() {
        // Not an error: the drain is bounded by contract. Say so plainly so the
        // operator knows work was cut, and keep it body-free.
        eprintln!(
            "service drained with {} turn(s) still running; journals retained, nothing replayed",
            report.still_running
        );
    }
    Ok(())
}

/// Stop the running service within `grace`, then report what happened.
///
/// A kill is a failed stop: bookkeeping is retained and the exit code is
/// non-zero, so a supervisor does not start a replacement on top of a process
/// whose teardown never ran.
pub fn stop(data_dir: Option<PathBuf>, grace: Duration) -> Result<StopOutcome> {
    let data_dir = resolve_data_dir(data_dir)?;
    let pid_path = data_dir.join(SERVICE_PID_FILE_NAME);
    let Some(record) = danso_ops::pidfile::read(&pid_path)? else {
        // No usable record. The lock may still be held by an unbookkept
        // process, but stopping something we cannot attribute is how an
        // unrelated pid gets signalled; `status` reports that case as degraded
        // and it is resolved by hand.
        return Ok(StopOutcome::NotRunning);
    };
    if !record.is_live() {
        return Ok(StopOutcome::NotRunning);
    }
    Ok(danso_ops::stop::terminate_and_wait(record.pid, grace))
}

/// Read the service state. Never acquires a lock, creates state, or calls out.
pub fn status(data_dir: Option<PathBuf>) -> Result<StatusReport> {
    let data_dir = resolve_data_dir(data_dir)?;
    let pid_path = data_dir.join(SERVICE_PID_FILE_NAME);
    let health_path = data_dir.join(HEALTH_FILE_NAME);
    let lock_path = data_dir.join(crate::telegram::TOKEN_LOCK_FILE_NAME);
    Ok(danso_ops::status::determine(
        &StatusInputs {
            pid_path: &pid_path,
            health_path: &health_path,
            lock_path: &lock_path,
        },
        chrono::Utc::now(),
        is_service_argv,
    ))
}

/// Supervise the service on a host without systemd.
///
/// The supervisor runs the service as a child rather than in-process: a crash
/// that takes down the runtime must not take the supervisor with it, which is
/// the whole point of having one. The child does its own pid and health
/// bookkeeping, so this loop owns only the restart decision.
pub fn supervise(data_dir: Option<PathBuf>) -> Result<i32> {
    let data_dir = resolve_data_dir(data_dir)?;
    crate::telegram::ensure_private_dir(&data_dir)?;
    health::set_run_mode(RunMode::Supervise);
    let exe = std::env::current_exe().context("could not resolve the running binary")?;

    // A wake lock keeps Android from suspending the process between polls.
    // Absent elsewhere, and its absence is not an error.
    if let Ok(wake_lock) = which("termux-wake-lock") {
        let _ = std::process::Command::new(wake_lock).status();
    }

    let mut policy = danso_ops::CrashPolicy::new();
    loop {
        let status = std::process::Command::new(&exe)
            .arg("service")
            .arg("run")
            .arg("--data-dir")
            .arg(&data_dir)
            .status()
            .context("could not start the supervised service")?;
        // A signal-terminated child has no exit code. SIGTERM to the whole
        // process group is how an operator stops a supervised service, so it
        // counts as a clean exit rather than a crash to restart from.
        let clean = status.success() || status.code().is_none();
        match policy.record(clean, std::time::Instant::now()) {
            danso_ops::Decision::Stop => return Ok(0),
            danso_ops::Decision::Restart => {}
            danso_ops::Decision::CrashLoop => {
                eprintln!(
                    "service supervision stopped: {} rapid crashes ({})",
                    policy.rapid_crashes(),
                    danso_ops::supervise::CRASH_LOOP_REASON
                );
                return Ok(danso_ops::supervise::CRASH_LOOP_EXIT);
            }
        }
    }
}

fn which(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is unset")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .context("program not found")
}

/// The text rendering of a stop outcome. Kept beside the states it names so a
/// new outcome cannot be added without deciding what it prints.
pub fn stop_text(outcome: StopOutcome) -> &'static str {
    match outcome {
        StopOutcome::NotRunning => "Bot stop: not running",
        StopOutcome::Drained => "Bot stop: drained",
        StopOutcome::Killed => "Bot stop: killed after the grace budget; pid and lock retained",
        StopOutcome::Survived => "Bot stop: target survived SIGKILL; pid and lock retained",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn service_and_telegram_processes_are_attributed() {
        assert!(is_service_argv(&argv(&[
            "/usr/local/bin/danso",
            "service",
            "run"
        ])));
        assert!(is_service_argv(&argv(&["danso", "telegram"])));
    }

    #[test]
    fn an_unrelated_process_is_not_attributed() {
        assert!(!is_service_argv(&argv(&["cat", "service"])));
        assert!(!is_service_argv(&argv(&["/bin/tail", "-f", "danso"])));
        assert!(!is_service_argv(&argv(&["danso"])));
        assert!(!is_service_argv(&argv(&[])));
    }

    #[test]
    fn a_status_inspection_is_not_a_holder() {
        // `status` opens the lock file to probe it. Attributing that read as
        // the holder would make every inspection report a running service.
        assert!(!is_service_argv(&argv(&["danso", "service", "status"])));
        assert!(!is_service_argv(&argv(&[
            "danso", "service", "status", "--json"
        ])));
    }

    #[test]
    fn a_similarly_named_program_is_not_attributed() {
        assert!(!is_service_argv(&argv(&["dansomething", "service"])));
        assert!(is_service_argv(&argv(&["danso-test", "service"])));
    }
}
