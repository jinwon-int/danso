//! `danso update` — the self-update surface (`docs/unified-design.md` §6.3).
//!
//! This slice is state and reporting only: `status` reads what is on disk and
//! what is running, and says whether an activation is outstanding. Downloading,
//! verifying and replacing the binary land in later slices (#119 PR2/PR3).
//!
//! `status` is a read. It takes no lock, performs no network access and writes
//! nothing — the same discipline `doctor` and `service status` follow, for the
//! same reason: inspecting an installation must not change it.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use danso_ops::update::{self, UpdateStatus};

#[derive(Parser)]
#[command(
    name = "danso update",
    about = "Inspect and manage the installed Danso generation. `status` is read-only and offline."
)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: UpdateCommand,
}

#[derive(Subcommand)]
pub enum UpdateCommand {
    /// Report the installed generation and any outstanding activation.
    Status {
        /// Emit the machine-readable report instead of the text rendering.
        #[arg(long)]
        json: bool,
    },
}

/// Collect the update status for this installation.
///
/// The running generation comes from the same digest the health document
/// publishes, so `update status` and the fleet watch cannot disagree about
/// which image is serving.
pub fn status() -> Result<UpdateStatus> {
    let home = crate::config::home().context("DANSO_HOME")?;
    let running = danso_ops::generation::current().binary_sha256.clone();
    update::status(&home, running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use danso_ops::update::{GenerationRef, InstalledGeneration, PendingActivation, Source};

    /// `DANSO_HOME` is process-wide, so these cases share one guard rather than
    /// racing each other through the environment.
    fn with_home<T>(home: &std::path::Path, body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("DANSO_HOME");
        unsafe { std::env::set_var("DANSO_HOME", home) };
        let outcome = body();
        match previous {
            Some(value) => unsafe { std::env::set_var("DANSO_HOME", value) },
            None => unsafe { std::env::remove_var("DANSO_HOME") },
        }
        outcome
    }

    #[test]
    fn a_fresh_installation_has_nothing_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let report = with_home(dir.path(), status).unwrap();
        assert_eq!(report.exit_code(), 0);
        assert!(report.installed.is_none());
        assert!(report.pending.is_none());
        assert!(!report.rollback_available);
    }

    #[test]
    fn the_running_generation_is_this_binarys_own_digest() {
        let dir = tempfile::tempdir().unwrap();
        let report = with_home(dir.path(), status).unwrap();
        assert_eq!(
            report.running_binary_sha256,
            danso_ops::generation::current().binary_sha256,
            "status and health.json must name the same running image, or the \
             fleet watch and the updater can disagree about what is serving"
        );
    }

    #[test]
    fn an_unresolved_activation_is_reported_as_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let state = update::state_dir(dir.path());
        update::write_installed(
            &state,
            &InstalledGeneration::new("0.1.0".into(), "a".repeat(64), Source::Release),
        )
        .unwrap();
        update::write_pending(
            &state,
            &PendingActivation::new(
                GenerationRef {
                    version: "0.2.0".into(),
                    binary_sha256: "b".repeat(64),
                },
                Some(GenerationRef {
                    version: "0.1.0".into(),
                    binary_sha256: "a".repeat(64),
                }),
                vec!["danso".into()],
                None,
            ),
        )
        .unwrap();

        let report = with_home(dir.path(), status).unwrap();
        assert_eq!(report.exit_code(), 1);
        assert!(
            !report.pending_target_is_running,
            "the test binary is not the recorded target, so the activation is \
             unfinished"
        );
    }

    #[test]
    fn status_creates_no_state_in_the_home_it_inspects() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), status).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            entries.is_empty(),
            "inspection must not create state; found {entries:?}"
        );
    }
}
