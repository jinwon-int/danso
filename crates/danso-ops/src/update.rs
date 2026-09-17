//! self-update state (`docs/unified-design.md` §6.3, §7; ccc-node #1527).
//!
//! Two records, both under `$DANSO_HOME/state/`, both written only by `update`:
//!
//! * `installed-generation.json` — what this installation believes it is
//!   running. ccc-node's `self-update.installed-sha` equivalent.
//! * `pending-activation.json` — a replacement that has been staged but not yet
//!   proven to be serving. Its whole reason for existing is that "the unit
//!   restarted" is not evidence the binary changed: a restart that re-execs the
//!   same image reports success while the generation is unchanged, and #1527
//!   was the incident where that gap swallowed a failed activation.
//!
//! The ordering invariant this module exists to protect: the pending record is
//! written **before** the binary is replaced. If it cannot be written, nothing
//! is replaced at all. A replaced binary with no pending record is an update
//! nobody can verify or roll back.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INSTALLED_GENERATION_SCHEMA: &str = "danso.installed-generation.v1";
pub const ACTIVATION_SCHEMA: &str = "danso.self-update.activation.v1";

pub const INSTALLED_GENERATION_FILE: &str = "installed-generation.json";
pub const PENDING_ACTIVATION_FILE: &str = "pending-activation.json";
pub const UPDATE_LOG_FILE: &str = "self-update.log";
pub const UPDATE_LOCK_FILE: &str = "update.lock";
pub const INSTALLED_BINARY_FILE: &str = "danso";
pub const PREVIOUS_BINARY_FILE: &str = "danso.prev";

/// `$DANSO_HOME/state`.
pub fn state_dir(danso_home: &Path) -> PathBuf {
    danso_home.join("state")
}

/// `$DANSO_HOME/bin/danso` — the binary an install replaces.
pub fn installed_binary(danso_home: &Path) -> PathBuf {
    danso_home.join("bin").join(INSTALLED_BINARY_FILE)
}

/// `$DANSO_HOME/bin/danso.prev` — the rollback target.
pub fn previous_binary(danso_home: &Path) -> PathBuf {
    danso_home.join("bin").join(PREVIOUS_BINARY_FILE)
}

/// Where a release came from. Recorded because the two paths have different
/// trust chains: a signed artifact and a locally built tree are not
/// interchangeable evidence of what is running.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Release,
    Git,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledGeneration {
    pub schema: String,
    pub version: String,
    pub binary_sha256: String,
    pub installed_at: String,
    pub source: Source,
    /// The signed archive this generation came out of.
    ///
    /// Recorded so `update check` can compare like with like: the manifest
    /// lists *archive* digests, `binary_sha256` is the digest of the binary
    /// inside one, and the two are never equal. Absent on a record written
    /// before this field existed, which `check` reports as "cannot tell"
    /// rather than guessing — and which the next apply fills in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
}

impl InstalledGeneration {
    pub fn new(version: String, binary_sha256: String, source: Source) -> Self {
        Self {
            schema: INSTALLED_GENERATION_SCHEMA.to_string(),
            version,
            binary_sha256,
            installed_at: chrono::Utc::now().to_rfc3339(),
            source,
            artifact_name: None,
            artifact_sha256: None,
        }
    }

    /// Name the archive this generation was unpacked from.
    pub fn from_artifact(mut self, name: &str, sha256: String) -> Self {
        self.artifact_name = Some(name.to_string());
        self.artifact_sha256 = Some(sha256);
        self
    }
}

/// One side of an activation — the generation being left or entered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationRef {
    pub version: String,
    pub binary_sha256: String,
}

/// How an activation ended.
///
/// `Pending` is the state that must survive a crash: it is what tells the next
/// run that a replacement happened and has not been verified.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    Activated,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingActivation {
    pub schema: String,
    pub target: GenerationRef,
    /// What was running before. Absent only on a first install, where there is
    /// nothing to roll back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<GenerationRef>,
    pub started_at: String,
    pub updated_at: String,
    pub outcome: Outcome,
    /// Units the activation expects to restart. Empty for a CLI-only install.
    #[serde(default)]
    pub services: Vec<String>,
    /// Path of the retained previous binary, when one was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

impl PendingActivation {
    pub fn new(
        target: GenerationRef,
        previous: Option<GenerationRef>,
        services: Vec<String>,
        snapshot: Option<String>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            schema: ACTIVATION_SCHEMA.to_string(),
            target,
            previous,
            started_at: now.clone(),
            updated_at: now,
            outcome: Outcome::Pending,
            services,
            snapshot,
        }
    }

    /// Whether this record still describes unverified work.
    pub fn is_unresolved(&self) -> bool {
        self.outcome == Outcome::Pending
    }

    /// Whether the running generation satisfies this activation's target.
    ///
    /// Identity of the image, not "did the unit restart". A restart that
    /// re-execs the same binary looks like success to systemd and changes
    /// nothing here.
    pub fn is_satisfied_by(&self, running_binary_sha256: Option<&str>) -> bool {
        running_binary_sha256.is_some_and(|sha| sha == self.target.binary_sha256)
    }
}

/// Read a state record, distinguishing absent from unreadable.
///
/// A corrupt record is an error, not an absence: treating it as "no pending
/// activation" would discard the only evidence that a replacement happened.
pub fn read_pending(state_dir: &Path) -> Result<Option<PendingActivation>> {
    read_record(&state_dir.join(PENDING_ACTIVATION_FILE))
}

pub fn read_installed(state_dir: &Path) -> Result<Option<InstalledGeneration>> {
    read_record(&state_dir.join(INSTALLED_GENERATION_FILE))
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read state: {}", path.display()));
        }
    };
    let record =
        serde_json::from_slice(&raw).with_context(|| format!("parse state: {}", path.display()))?;
    Ok(Some(record))
}

/// Write the pending record, creating the state directory if needed.
///
/// Callers must succeed at this **before** replacing any binary. The `Err`
/// return is the signal to abort the whole update having changed nothing,
/// which is the #1527 invariant.
pub fn write_pending(state_dir: &Path, record: &PendingActivation) -> Result<()> {
    write_record(&state_dir.join(PENDING_ACTIVATION_FILE), record)
}

pub fn write_installed(state_dir: &Path, record: &InstalledGeneration) -> Result<()> {
    write_record(&state_dir.join(INSTALLED_GENERATION_FILE), record)
}

fn write_record<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create state directory: {}", dir.display()))?;
    let payload = serde_json::to_vec(record).context("serialize state")?;
    // Same-directory temp then rename: a reader must never observe a half
    // written record, and a torn pending record is indistinguishable from a
    // missing one to anything that only checks whether the file parses.
    let temp = path.with_extension("tmp");
    write_private(&temp, &payload).with_context(|| format!("write state: {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("commit state: {}", path.display()))?;
    Ok(())
}

fn write_private(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(payload)?;
    file.sync_all()
}

/// What `danso update status` reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstalledGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingActivation>,
    /// The generation actually running right now, when it could be measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_binary_sha256: Option<String>,
    /// True when a pending activation's target is what is running. Reported
    /// separately from `pending` so a consumer does not have to re-derive the
    /// comparison and get it subtly wrong.
    pub pending_target_is_running: bool,
    pub rollback_available: bool,
}

impl UpdateStatus {
    /// `0` nothing outstanding · `1` a pending activation is unresolved ·
    /// `2` state could not be read.
    ///
    /// An unresolved pending activation is not an error — it is the normal
    /// state between replacing a binary and proving the replacement serves.
    /// It is still non-zero because something must finish it.
    pub fn exit_code(&self) -> i32 {
        match &self.pending {
            Some(pending) if pending.is_unresolved() => 1,
            _ => 0,
        }
    }

    pub fn summary(&self) -> String {
        let installed = self
            .installed
            .as_ref()
            .map(|record| record.version.clone())
            .unwrap_or_else(|| "-".to_string());
        match &self.pending {
            Some(pending) if pending.is_unresolved() => {
                if self.pending_target_is_running {
                    format!(
                        "Update: {installed}; activation of {} is running but unconfirmed",
                        pending.target.version
                    )
                } else {
                    format!(
                        "Update: {installed}; activation of {} pending, previous generation still serving",
                        pending.target.version
                    )
                }
            }
            Some(pending) => format!("Update: {installed}; last activation {:?}", pending.outcome),
            None => format!("Update: {installed}; no activation outstanding"),
        }
    }
}

/// Collect the status from on-disk state plus the running generation.
pub fn status(danso_home: &Path, running_binary_sha256: Option<String>) -> Result<UpdateStatus> {
    let state = state_dir(danso_home);
    let installed = read_installed(&state)?;
    let pending = read_pending(&state)?;
    let pending_target_is_running = pending
        .as_ref()
        .is_some_and(|pending| pending.is_satisfied_by(running_binary_sha256.as_deref()));
    Ok(UpdateStatus {
        installed,
        pending,
        running_binary_sha256,
        pending_target_is_running,
        rollback_available: previous_binary(danso_home).is_file(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> GenerationRef {
        GenerationRef {
            version: "0.2.0".to_string(),
            binary_sha256: "b".repeat(64),
        }
    }

    fn previous() -> GenerationRef {
        GenerationRef {
            version: "0.1.0".to_string(),
            binary_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn the_activation_schema_matches_the_ccc_node_record() {
        // ccc-node writes `ccc.self-update.activation.v1`; the danso record is
        // the same shape under its own name, so a reader ported from ccc finds
        // the fields where it expects them.
        let record = PendingActivation::new(target(), Some(previous()), vec!["danso".into()], None);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["schema"], ACTIVATION_SCHEMA);
        for key in [
            "target",
            "previous",
            "started_at",
            "updated_at",
            "outcome",
            "services",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["outcome"], "pending");
        assert_eq!(value["target"]["binary_sha256"], "b".repeat(64));
    }

    #[test]
    fn a_first_install_has_no_previous_and_omits_the_key() {
        let record = PendingActivation::new(target(), None, vec![], None);
        let value = serde_json::to_value(&record).unwrap();
        assert!(
            value.get("previous").is_none(),
            "a first install has nothing to roll back to; a null would read as \
             'there was one and we lost it'"
        );
    }

    #[test]
    fn activation_is_satisfied_by_the_image_not_by_a_restart() {
        let record = PendingActivation::new(target(), Some(previous()), vec![], None);
        assert!(record.is_satisfied_by(Some(&"b".repeat(64))));
        assert!(
            !record.is_satisfied_by(Some(&"a".repeat(64))),
            "the previous generation still serving is an unfinished activation, \
             not a successful one"
        );
        assert!(
            !record.is_satisfied_by(None),
            "an unmeasurable generation is not evidence the target is running"
        );
    }

    #[test]
    fn records_round_trip_through_disk_at_owner_only_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        let record = PendingActivation::new(target(), Some(previous()), vec!["danso".into()], None);
        write_pending(&state, &record).unwrap();
        assert_eq!(read_pending(&state).unwrap().as_ref(), Some(&record));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(state.join(PENDING_ACTIVATION_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let installed = InstalledGeneration::new("0.1.0".into(), "a".repeat(64), Source::Release);
        write_installed(&state, &installed).unwrap();
        assert_eq!(read_installed(&state).unwrap().as_ref(), Some(&installed));
    }

    #[test]
    fn a_write_that_cannot_happen_is_an_error_not_a_silent_skip() {
        // The caller aborts the update on this error and replaces nothing
        // (#1527). Returning Ok here would let a binary be replaced with no
        // record of it.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("state");
        // A file where the state directory belongs.
        std::fs::write(&blocked, b"not a directory").unwrap();
        let record = PendingActivation::new(target(), None, vec![], None);
        assert!(write_pending(&blocked, &record).is_err());
        // The contract is fail-closed, not a particular error message: whether
        // the directory creation or the file write is what refuses, the caller
        // must see Err and find nothing recorded. (This is why swallowing the
        // `create_dir_all` error alone is an equivalent mutant — the write
        // behind it still refuses.)
        assert!(
            !blocked.is_dir(),
            "the blocked path must not have become a state directory"
        );
    }

    #[test]
    fn a_corrupt_record_is_an_error_not_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(PENDING_ACTIVATION_FILE), b"{ not json").unwrap();
        assert!(
            read_pending(&state).is_err(),
            "reporting a corrupt pending record as 'none' discards the only \
             evidence that a replacement happened"
        );
    }

    #[test]
    fn absent_state_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        assert_eq!(read_pending(&state).unwrap(), None);
        assert_eq!(read_installed(&state).unwrap(), None);
    }

    #[test]
    fn status_reports_an_unresolved_activation_as_exit_one() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        write_installed(
            &state,
            &InstalledGeneration::new("0.1.0".into(), "a".repeat(64), Source::Release),
        )
        .unwrap();
        write_pending(
            &state,
            &PendingActivation::new(target(), Some(previous()), vec![], None),
        )
        .unwrap();

        // The previous generation is still serving: activation is unfinished.
        let report = status(dir.path(), Some("a".repeat(64))).unwrap();
        assert_eq!(report.exit_code(), 1);
        assert!(!report.pending_target_is_running);
        assert!(
            report
                .summary()
                .contains("previous generation still serving")
        );

        // The target is serving, but nothing has marked the activation done.
        let report = status(dir.path(), Some("b".repeat(64))).unwrap();
        assert_eq!(
            report.exit_code(),
            1,
            "running is not the same as confirmed"
        );
        assert!(report.pending_target_is_running);
        assert!(report.summary().contains("unconfirmed"));
    }

    #[test]
    fn status_is_clean_once_the_activation_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        let mut record = PendingActivation::new(target(), Some(previous()), vec![], None);
        record.outcome = Outcome::Activated;
        write_pending(&state, &record).unwrap();
        let report = status(dir.path(), Some("b".repeat(64))).unwrap();
        assert_eq!(report.exit_code(), 0);
        assert!(!report.rollback_available, "no danso.prev was written");
    }

    #[test]
    fn rollback_availability_follows_the_retained_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!status(dir.path(), None).unwrap().rollback_available);
        let previous = previous_binary(dir.path());
        std::fs::create_dir_all(previous.parent().unwrap()).unwrap();
        std::fs::write(&previous, b"#!/bin/sh\n").unwrap();
        assert!(status(dir.path(), None).unwrap().rollback_available);
    }
}
