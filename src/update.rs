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
    /// Ask the configured release source what it offers for this target.
    ///
    /// Fetches the signed manifest and nothing else — no archive, no writes,
    /// no lock. Exits 10 when a different release is available.
    Check {
        #[arg(long)]
        json: bool,
    },
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

/// Exit code for "a different release is available".
///
/// Its own code so a cron wrapper can branch without parsing output, and not
/// `1`, which `update status` already uses for an unresolved activation.
pub const EXIT_UPDATE_AVAILABLE: i32 = 10;

/// What the release source offers, against what is installed.
///
/// **This does not compare versions, and deliberately says nothing about
/// newer or older.** The manifest carries digests and names; an ordering would
/// have to be parsed out of a file name, and a release source that has been
/// rolled back would then read as "up to date" while serving something else.
/// Different is different — deciding whether to take it is `apply`'s caller's
/// job, which is why this command installs nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Available {
    /// The offered archive is the one this generation was installed from.
    UpToDate { artifact: String },
    /// A different archive. Not necessarily a newer one.
    Different {
        artifact: String,
        artifact_sha256: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_sha256: Option<String>,
    },
    /// The release verifies but names nothing for this target.
    NothingForThisTarget {
        target: String,
        offered: Vec<String>,
    },
    /// Nothing is installed. The release is simply what this node would get.
    NotInstalled {
        artifact: String,
        artifact_sha256: String,
    },
    /// Something is installed, but nothing recorded which archive it came
    /// from, so there is nothing to compare against.
    ///
    /// **Exit 0, not 10.** Acting on "cannot tell" is the fail-open this
    /// command exists to avoid: a wrapper that re-applies on 10 would, after
    /// `update rollback` — which writes a record without these fields —
    /// immediately reinstall the release the operator just rolled away from.
    /// The next `apply` records the archive and the comparison starts working.
    Unknown {
        artifact: String,
        artifact_sha256: String,
    },
    /// The release names more than one artifact for this target.
    ///
    /// Not a choice to make silently. `Manifest` iterates a sorted map, so
    /// "pick the first" means lexicographic order — under which a release
    /// carrying both `…-0.1.0-…` and `…-0.2.0-…` reads as "up to date" on the
    /// former while the latter sits in the same signed manifest. That is the
    /// failure this command is written to avoid, reintroduced by map order
    /// rather than by parsing a version. `release` refuses a duplicate *name*
    /// for the same reason: both are signed, so neither is "the" one.
    Ambiguous {
        target: String,
        candidates: Vec<String>,
    },
}

impl Available {
    pub fn exit_code(&self) -> i32 {
        match self {
            Available::UpToDate { .. } => 0,
            Available::Different { .. } => EXIT_UPDATE_AVAILABLE,
            // Not a failure and not an update: a release that carries no
            // artifact for this machine is a fact about the release.
            Available::NothingForThisTarget { .. } => 0,
            Available::NotInstalled { .. } => EXIT_UPDATE_AVAILABLE,
            Available::Unknown { .. } => 0,
            // Neither 0 nor 10: this is "cannot decide", and a cron wrapper
            // that branches on those two must not take either branch.
            Available::Ambiguous { .. } => 2,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Available::UpToDate { artifact } => format!("up to date ({artifact})"),
            Available::Different { artifact, .. } => {
                format!("a different release is available: {artifact}")
            }
            Available::NothingForThisTarget { target, offered } => format!(
                "the release carries nothing for {target}; it offers {}",
                match offered.is_empty() {
                    true => "nothing".to_string(),
                    false => offered.join(", "),
                }
            ),
            Available::NotInstalled { artifact, .. } => {
                format!("nothing is installed; {artifact} is what this node would get")
            }
            Available::Unknown { artifact, .. } => format!(
                "{artifact} is offered, but this installation does not record \
                 which archive it came from, so the two cannot be compared; \
                 the next `update apply` records it"
            ),
            Available::Ambiguous { target, candidates } => format!(
                "the release names {} artifacts for {target} ({}); it is not \
                 this command's job to choose between signed alternatives",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

/// The target triple this binary was built for.
///
/// Used to pick an artifact out of a manifest that may name several. It is a
/// build-time fact, not a runtime guess: asking the running system would let a
/// misconfigured node install a binary it cannot execute.
pub const TARGET: &str = env!("DANSO_TARGET");

/// Ask the configured release source what it has, and compare.
///
/// Read-only and offline-safe in the sense that matters: it fetches the
/// manifest and its signature and **nothing else** — no archive is downloaded,
/// nothing on disk is written, no lock is taken. A `check` that installed
/// something as a side effect would be the worst possible surprise in a cron
/// job.
pub fn check() -> Result<Available> {
    let home = crate::config::home().context("DANSO_HOME")?;
    let key = configured_public_key(&home)?;
    let source = configured_source(&home)?;

    let (manifest_bytes, signature) = crate::fetch::manifest(&source)?;
    // Verified before a single name inside it is read.
    let manifest = danso_ops::release::verify_manifest(&manifest_bytes, &signature, &key)?;

    let suffix = format!("-{TARGET}.tar.gz");
    // Collected, not `find`: the first match of a sorted map is an ordering
    // this command refuses to have an opinion about.
    let candidates: Vec<String> = manifest
        .names()
        .filter(|name| name.ends_with(&suffix))
        .map(str::to_string)
        .collect();
    let artifact = match candidates.len() {
        0 => {
            return Ok(Available::NothingForThisTarget {
                target: TARGET.to_string(),
                offered: manifest.names().map(str::to_string).collect(),
            });
        }
        1 => candidates.into_iter().next().expect("one candidate"),
        _ => {
            return Ok(Available::Ambiguous {
                target: TARGET.to_string(),
                candidates,
            });
        }
    };
    let artifact_sha256 = manifest
        .digest(&artifact)
        .context("the manifest names an artifact it does not list")?
        .to_string();

    // Not `.ok()`: `read_installed` produces an error for a *corrupt* record on
    // purpose, and swallowing it here would report "an update is available" on
    // every tick while the only evidence of what this node is running sits
    // damaged on disk.
    let installed = update::read_installed(&update::state_dir(&home))
        .context("read the installed generation")?;
    let Some(installed) = installed else {
        return Ok(Available::NotInstalled {
            artifact,
            artifact_sha256,
        });
    };
    match installed.artifact_sha256 {
        Some(previous) if previous == artifact_sha256 => Ok(Available::UpToDate { artifact }),
        Some(previous) => Ok(Available::Different {
            artifact,
            artifact_sha256,
            installed_sha256: Some(previous),
        }),
        None => Ok(Available::Unknown {
            artifact,
            artifact_sha256,
        }),
    }
}

/// Where releases come from, from `config.toml`.
fn configured_source(home: &Path) -> Result<String> {
    let path = home.join(crate::config::FILE_NAME);
    let source = match path.exists() {
        true => crate::config::Config::load(&path)?.update.source,
        false => None,
    };
    source.context(
        "no release source is configured; set [update] source in config.toml, \
         or point `apply` at a directory with --artifact-dir",
    )
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
    // An attempt that never started still gets a log line. A node several
    // generations behind is diagnosed from this file, and a silent deferral
    // makes "busy every time" and "the updater never ran" identical.
    match &gate {
        danso_ops::idle::Gate::Deferred {
            reason,
            waited_seconds,
        } => {
            install::log_gate(
                &home,
                gate.log_event(),
                Some(*reason),
                Some(*waited_seconds),
            );
            return Ok(Applied::Deferred(gate));
        }
        danso_ops::idle::Gate::Proceed(danso_ops::idle::Proceeding::Idle) => {}
        danso_ops::idle::Gate::Proceed(proceeding) => {
            let (busy, waited) = match proceeding {
                danso_ops::idle::Proceeding::BudgetExhausted {
                    busy,
                    waited_seconds,
                } => (Some(*busy), Some(*waited_seconds)),
                danso_ops::idle::Proceeding::Untrackable { busy } => (Some(*busy), None),
                danso_ops::idle::Proceeding::Forced { busy } => (*busy, None),
                danso_ops::idle::Proceeding::Idle => (None, None),
            };
            install::log_gate(&home, gate.log_event(), busy, waited);
            // The one case where the gate knowingly does what it exists to
            // prevent has to be said out loud. stderr, so a `--json` consumer's
            // document shape is unchanged.
            if gate.overrides_a_live_turn() {
                eprintln!("warning: {}", gate.summary());
            }
        }
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
