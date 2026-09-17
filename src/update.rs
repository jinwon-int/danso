//! `danso update` — the self-update surface (`docs/unified-design.md` §6.3).
//!
//! `status` reads what is on disk and what is running and says whether an
//! activation is outstanding. `apply` installs a signed release that is already
//! on disk. Fetching one over the network is a later slice, which is why
//! `apply` names a directory instead of a URL.
//!
//! `status` is a read. It takes no lock, performs no network access and writes
//! nothing — the same discipline `doctor` and `service status` follow, for the
//! same reason: inspecting an installation must not change it. `apply` is the
//! opposite and is ordered accordingly; see `danso_ops::install`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use danso_ops::install;
use danso_ops::update::{self, UpdateStatus};
use std::path::{Path, PathBuf};

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
    /// Resolve an outstanding activation by checking which image is serving.
    /// Exits 1 if the replacement is not serving and 3 if that cannot be told.
    Activate {
        /// Where `health.json` lives; defaults to the service data directory.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// Judge again even if the activation is already recorded as failed.
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        json: bool,
    },
    /// Put `bin/danso.prev` back, recording the swap before performing it.
    Rollback {
        /// Units this activation expects to restart. Repeatable.
        #[arg(long = "service", value_name = "UNIT")]
        services: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install a signed release from a local directory. Exits 13 if it does
    /// not verify, and there is no flag that skips the check.
    Apply {
        /// Directory holding `SHA256SUMS`, `SHA256SUMS.minisig` and the artifact.
        #[arg(long, value_name = "DIR")]
        artifact_dir: PathBuf,
        /// Artifact file name, as listed in the signed manifest.
        #[arg(long, value_name = "NAME")]
        artifact: String,
        /// Units this activation expects to restart. Repeatable.
        #[arg(long = "service", value_name = "UNIT")]
        services: Vec<String>,
        /// Where `health.json` lives; defaults to the service data directory.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// Install even while the service is serving a turn. This kills the
        /// turn in flight; the gate exists so that is a decision, not an
        /// accident.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

/// What `apply` did, or declined to do.
///
/// A deferral is not a failure — nothing was read, locked or written — so it
/// is an `Ok` outcome with its own exit code rather than an `ApplyError`.
pub enum Applied {
    Installed(install::ApplyReport),
    Deferred(danso_ops::idle::Gate),
}

/// Install a verified release over `$DANSO_HOME/bin/danso`.
///
/// The trusted key is the one embedded in this binary unless `config.toml`
/// overrides it. Reading the override through the normal config path is what
/// makes rotation possible without shipping a new binary first; a broken
/// override is an error rather than a silent fall back to the embedded key,
/// because falling back would let a bad rotation keep trusting the old key.
pub fn apply(
    artifact_dir: &Path,
    artifact: &str,
    services: Vec<String>,
    data_dir: Option<PathBuf>,
    force: bool,
) -> Result<Applied, install::ApplyError> {
    let home = crate::config::home()
        .context("DANSO_HOME")
        .map_err(install::ApplyError::Failed)?;

    // The gate comes first, before the key, before `tar`, before the lock:
    // a deferred run must leave no trace that it considered running, and
    // holding the update lock while deferring would block a concurrent
    // `activate` for no reason. Same resolver as `activate`, so the two
    // cannot disagree about which health document describes this service.
    let health = crate::service::resolve_data_dir(data_dir)
        .ok()
        .map(|dir| dir.join(danso_ops::health::HEALTH_FILE_NAME));
    let gate = danso_ops::idle::check(
        &danso_ops::update::state_dir(&home),
        health.as_deref(),
        force,
        chrono::Utc::now(),
    );
    if matches!(gate, danso_ops::idle::Gate::Deferred { .. }) {
        return Ok(Applied::Deferred(gate));
    }

    let key = configured_public_key(&home).map_err(install::ApplyError::Failed)?;
    if !install::tar_available() {
        return Err(install::ApplyError::Failed(anyhow::anyhow!(
            "tar is required to unpack a release"
        )));
    }
    install::apply(&install::ApplyPlan {
        artifact_dir,
        artifact_name: artifact,
        public_key: &key,
        danso_home: &home,
        services,
    })
    .map(Applied::Installed)
}

/// Resolve an outstanding activation.
///
/// The health document is the one `service status` reads, resolved through the
/// same helper: two resolvers would eventually look at two different files and
/// disagree about what is serving.
pub fn activate(data_dir: Option<PathBuf>, retry: bool) -> Result<danso_ops::Activation> {
    let home = crate::config::home().context("DANSO_HOME")?;
    // A CLI-only installation has no service data directory, and requiring one
    // would make `activate` unusable exactly where it is simplest. An
    // unresolvable directory becomes "no health evidence", which a record that
    // names services reports as `unverified` rather than as a failure.
    let health = crate::service::resolve_data_dir(data_dir)
        .ok()
        .map(|dir| dir.join("health.json"));
    let now = chrono::Utc::now();
    match retry {
        true => danso_ops::activate::retry(&home, health.as_deref(), now),
        false => danso_ops::activate::activate(&home, health.as_deref(), now),
    }
}

/// Put the previous binary back.
pub fn rollback(services: Vec<String>) -> Result<install::RollbackReport, install::ApplyError> {
    let home = crate::config::home()
        .context("DANSO_HOME")
        .map_err(install::ApplyError::Failed)?;
    install::rollback(&home, services)
}

/// The release key this installation trusts.
fn configured_public_key(home: &Path) -> Result<String> {
    let path = home.join(crate::config::FILE_NAME);
    let override_key = match path.exists() {
        true => crate::config::Config::load(&path)?.update.public_key,
        false => None,
    };
    install::trusted_public_key(override_key.as_deref())
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
