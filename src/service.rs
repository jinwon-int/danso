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
};
use std::path::PathBuf;

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
    },
    /// Report whether the service is available, degraded or unavailable.
    Status {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
    },
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
    crate::telegram::run()
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
