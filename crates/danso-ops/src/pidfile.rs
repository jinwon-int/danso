//! `service.pid` bookkeeping (`docs/unified-design.md` §7).
//!
//! The pid file is *bookkeeping*, not ownership: the Telegram token lock is the
//! ownership signal. That split is what lets `status` tell "serving but
//! unbookkept" (degraded) apart from "down" (unavailable), which is the ccc-node
//! behaviour observed on gongyung 2026-08-11 and which this must preserve.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const SERVICE_PID_FILE_NAME: &str = "service.pid";
pub const SUPERVISOR_PID_FILE_NAME: &str = "supervisor.pid";

/// What a pid file records about the process that created it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServicePid {
    pub pid: u32,
    pub started_at: String,
    /// The boot that allocated `pid`. A pid recorded before a reboot can name
    /// an unrelated process after one, so a mismatch makes the record stale
    /// rather than a conflict.
    pub boot_id: String,
    /// Digest of the argv that started the service, so a pid reused by a
    /// different command is not mistaken for this service.
    pub argv_hash: String,
}

impl ServicePid {
    /// Describe the calling process.
    pub fn for_current_process(argv: &[String]) -> Self {
        Self {
            pid: std::process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
            boot_id: super::probe::boot_id().unwrap_or_default(),
            argv_hash: argv_hash(argv),
        }
    }

    /// Whether this record still describes a live instance of this service.
    ///
    /// An empty recorded `boot_id` means the kernel did not expose one when the
    /// record was written; there is then no evidence the pid survived a reboot,
    /// so the record is not treated as live.
    pub fn is_live(&self) -> bool {
        if self.boot_id.is_empty() {
            return false;
        }
        if super::probe::boot_id().as_deref() != Some(self.boot_id.as_str()) {
            return false;
        }
        if !super::probe::pid_alive(self.pid) {
            return false;
        }
        match super::probe::cmdline(self.pid) {
            Some(argv) => argv_hash(&argv) == self.argv_hash,
            // The process is alive but its argv is unreadable. Claiming it is
            // not ours would let a second consumer start on top of it.
            None => true,
        }
    }
}

/// Digest of an argv vector. NUL is used as the separator because it cannot
/// appear inside an argument, so `["a b"]` and `["a", "b"]` cannot collide.
pub fn argv_hash(argv: &[String]) -> String {
    let mut hasher = Sha256::new();
    for arg in argv {
        hasher.update(arg.as_bytes());
        hasher.update([0_u8]);
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A pid file owned for the lifetime of the value. Dropping it removes the
/// file, so an orderly exit leaves no record behind for `status` to misread.
#[derive(Debug)]
pub struct PidFile {
    path: PathBuf,
    record: ServicePid,
}

impl PidFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self) -> &ServicePid {
        &self.record
    }

    /// Create `service.pid` for this process.
    ///
    /// Creation is `O_EXCL`, so two processes racing here cannot both win. A
    /// record left by a crashed process is reclaimed once it is shown not to be
    /// live; a record that *is* live is a real conflict and fails.
    pub fn acquire(dir: &Path, name: &str, record: ServicePid) -> Result<Self> {
        let path = dir.join(name);
        match create_exclusive(&path, &record) {
            Ok(()) => return Ok(Self { path, record }),
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error).with_context(|| format!("create pid file: {}", path.display()));
            }
            Err(_) => {}
        }
        // Occupied. Only a record that is demonstrably not live may be removed;
        // an unparseable one cannot prove liveness and is treated as debris.
        if let Some(existing) = read(&path)?
            && existing.is_live()
        {
            bail!(
                "danso service is already running (pid {}); refusing to start a second instance",
                existing.pid
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("clear stale pid file: {}", path.display()))?;
        create_exclusive(&path, &record)
            .with_context(|| format!("create pid file: {}", path.display()))?;
        Ok(Self { path, record })
    }

    /// Release without removing the file. Used when the process is being
    /// replaced rather than stopped, where the record must survive.
    pub fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Only remove a record that is still ours: a reclaimed file may already
        // belong to a successor process.
        if matches!(read(&self.path), Ok(Some(current)) if current == self.record) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_exclusive(path: &Path, record: &ServicePid) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    let payload = serde_json::to_vec(record).expect("serializable pid record");
    file.write_all(&payload)?;
    file.sync_all()
}

/// Read a pid record.
///
/// `Ok(None)` means the file is absent. A file that exists but cannot be parsed
/// is reported as `Ok(None)` for reclaim purposes and separately surfaced by
/// [`read_raw`] for status, which must not turn unreadable evidence into DOWN.
pub fn read(path: &Path) -> Result<Option<ServicePid>> {
    match read_raw(path)? {
        Some(Ok(record)) => Ok(Some(record)),
        Some(Err(_)) | None => Ok(None),
    }
}

/// Distinguish absent (`None`) from present-but-unparseable (`Some(Err(_))`).
#[allow(clippy::type_complexity)]
pub fn read_raw(path: &Path) -> Result<Option<Result<ServicePid, String>>> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read pid file: {}", path.display()));
        }
    };
    Ok(Some(
        serde_json::from_slice::<ServicePid>(&raw).map_err(|error| error.to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32) -> ServicePid {
        ServicePid {
            pid,
            started_at: "2026-09-16T00:00:00Z".to_string(),
            boot_id: super::super::probe::boot_id().unwrap_or_default(),
            argv_hash: argv_hash(&["danso".to_string(), "service".to_string()]),
        }
    }

    #[test]
    fn argv_hash_separates_arguments() {
        let joined = argv_hash(&["a b".to_string()]);
        let split = argv_hash(&["a".to_string(), "b".to_string()]);
        assert_ne!(joined, split);
    }

    #[test]
    fn acquire_creates_a_private_record() {
        let dir = tempfile::tempdir().unwrap();
        let pid = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&["danso".to_string()]),
        )
        .unwrap();
        let path = pid.path().to_path_buf();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(pid);
        assert!(!path.exists(), "an orderly exit must leave no pid record");
    }

    #[test]
    fn acquire_refuses_a_second_live_instance() {
        let dir = tempfile::tempdir().unwrap();
        let argv: Vec<String> = super::super::probe::cmdline(std::process::id()).unwrap();
        let _held = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&argv),
        )
        .unwrap();
        let error = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&argv),
        )
        .unwrap_err();
        assert!(error.to_string().contains("already running"), "{error}");
    }

    #[test]
    fn acquire_reclaims_a_dead_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SERVICE_PID_FILE_NAME);
        std::fs::write(&path, serde_json::to_vec(&record(u32::MAX - 1)).unwrap()).unwrap();
        let pid = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&["danso".to_string()]),
        )
        .unwrap();
        assert_eq!(pid.record().pid, std::process::id());
    }

    #[test]
    fn acquire_reclaims_a_record_from_another_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SERVICE_PID_FILE_NAME);
        let mut stale = record(std::process::id());
        stale.boot_id = "00000000-0000-0000-0000-000000000000".to_string();
        assert!(!stale.is_live(), "a pid from another boot is not evidence");
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let pid = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&["danso".to_string()]),
        )
        .unwrap();
        assert_eq!(pid.record().pid, std::process::id());
    }

    #[test]
    fn live_pid_with_a_different_argv_is_not_this_service() {
        let mut impostor = record(std::process::id());
        impostor.argv_hash = argv_hash(&["something-else".to_string()]);
        assert!(!impostor.is_live());
    }

    #[test]
    fn unparseable_record_is_distinguishable_from_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SERVICE_PID_FILE_NAME);
        assert!(read_raw(&path).unwrap().is_none());
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(matches!(read_raw(&path).unwrap(), Some(Err(_))));
        assert!(read(&path).unwrap().is_none());
    }

    #[test]
    fn drop_does_not_remove_a_successors_record() {
        let dir = tempfile::tempdir().unwrap();
        let pid = PidFile::acquire(
            dir.path(),
            SERVICE_PID_FILE_NAME,
            ServicePid::for_current_process(&["danso".to_string()]),
        )
        .unwrap();
        let path = pid.path().to_path_buf();
        // A successor reclaimed the file while this guard was still alive.
        std::fs::write(&path, serde_json::to_vec(&record(4242)).unwrap()).unwrap();
        drop(pid);
        assert!(path.exists(), "the successor's record must survive");
        assert_eq!(read(&path).unwrap().unwrap().pid, 4242);
    }
}
