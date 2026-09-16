//! Three-state status (`danso service status`), preserving the meaning of
//! ccc-node's `bridge/start.sh --status`.
//!
//! The distinction that matters is between *serving but unbookkept* and *down*.
//! Collapsing them is what makes a supervisor start a second consumer on top of
//! a live one (ccc-node, gongyung 2026-08-11). The token lock is the ownership
//! signal; the pid file is bookkeeping; health freshness is the serving signal.

use crate::{
    generation::RuntimeGeneration,
    health::HealthDocument,
    pidfile::{self, ServicePid},
    probe,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Available,
    Degraded,
    Unavailable,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
        }
    }
}

/// What a status read concluded.
///
/// `Indeterminate` is not a fourth service state: it says the evidence could not
/// be read. Reporting it as `Unavailable` would be a false DOWN, which is the
/// one error a supervisor must never act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusOutcome {
    State(ServiceState),
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusReport {
    /// `available` | `degraded` | `unavailable` | `unverified`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_age_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_generation: Option<RuntimeGeneration>,
    /// Always present and always empty here: `status` is a read. The key exists
    /// so a consumer can use one shape for reading and for commands that do
    /// mutate, without treating its absence as "unknown".
    pub mutations: serde_json::Map<String, serde_json::Value>,
}

/// The value printed for a read that could not reach a verdict. It is
/// deliberately *not* one of the three states, so a fleet watch grepping for
/// `unavailable` cannot match it.
pub const UNVERIFIED: &str = "unverified";

impl StatusReport {
    pub fn outcome(&self) -> StatusOutcome {
        match self.state.as_str() {
            "available" => StatusOutcome::State(ServiceState::Available),
            "degraded" => StatusOutcome::State(ServiceState::Degraded),
            "unavailable" => StatusOutcome::State(ServiceState::Unavailable),
            _ => StatusOutcome::Indeterminate,
        }
    }

    /// `0` available, `1` degraded, `2` unavailable, `3` indeterminate.
    pub fn exit_code(&self) -> i32 {
        match self.outcome() {
            StatusOutcome::State(ServiceState::Available) => 0,
            StatusOutcome::State(ServiceState::Degraded) => 1,
            StatusOutcome::State(ServiceState::Unavailable) => 2,
            StatusOutcome::Indeterminate => 3,
        }
    }

    /// The text rendering. The first line is contractual: ccc-node's fleet watch
    /// greps for it, so it must stay a single fixed-prefix line.
    pub fn to_text(&self) -> String {
        let mut out = format!("Bot status: {}\n", self.state);
        if let Some(reason) = &self.reason {
            out.push_str(&format!("Reason: {reason}\n"));
        }
        if let Some(pid) = self.pid {
            out.push_str(&format!("PID: {pid}\n"));
        }
        if let Some(age) = self.health_age_secs {
            out.push_str(&format!("Health age: {age}s\n"));
        }
        if let Some(generation) = &self.runtime_generation {
            out.push_str(&format!(
                "Runtime: {} generation {}\n",
                generation.exe_path.as_deref().unwrap_or("unknown"),
                generation.binary_sha256.as_deref().unwrap_or("unknown"),
            ));
        }
        out
    }

    fn new(state: &str, reason: Option<String>) -> Self {
        Self {
            state: state.to_string(),
            reason,
            pid: None,
            health_age_secs: None,
            runtime_generation: None,
            mutations: serde_json::Map::new(),
        }
    }
}

/// Where the evidence lives.
pub struct StatusInputs<'a> {
    pub pid_path: &'a Path,
    pub health_path: &'a Path,
    /// The Telegram token lock. Its *kernel* lock, not its existence, is the
    /// ownership signal, so a leftover file is not evidence of a live service.
    pub lock_path: &'a Path,
}

/// Determine the service state from on-disk evidence.
///
/// `is_service_argv` decides whether a process that holds the lock is this
/// service; the caller owns that knowledge so this crate stays free of any
/// Telegram or CLI specifics.
pub fn determine(
    inputs: &StatusInputs<'_>,
    now: chrono::DateTime<chrono::Utc>,
    is_service_argv: impl Fn(&[String]) -> bool,
) -> StatusReport {
    let health = match read_health(inputs.health_path) {
        Ok(health) => health,
        Err(reason) => return StatusReport::new(UNVERIFIED, Some(reason)),
    };
    let pid_record = match pidfile::read_raw(inputs.pid_path) {
        Ok(Some(Ok(record))) => Some(record),
        // Debris is not a permission failure: fall through to the lock, which
        // is the authoritative ownership signal.
        Ok(Some(Err(_)) | None) => None,
        Err(error) => {
            return StatusReport::new(UNVERIFIED, Some(format!("pid file unreadable: {error}")));
        }
    };

    if let Some(record) = pid_record.filter(ServicePid::is_live) {
        return bookkept(record, health, now);
    }

    // No usable pid record. A live holder of the token lock is still serving;
    // calling that DOWN is the error this branch exists to prevent.
    match lock_state(inputs.lock_path, is_service_argv) {
        Ok(LockState::Held(holder)) => {
            let mut report = StatusReport::new(
                ServiceState::Degraded.as_str(),
                Some(
                    "serving without usable pid bookkeeping; stop and restart cannot be tracked"
                        .to_string(),
                ),
            );
            report.pid = holder;
            report.health_age_secs = health.as_ref().and_then(|doc| doc.age_seconds(now));
            report.runtime_generation = health.map(|doc| doc.runtime_generation);
            report
        }
        Ok(LockState::Free) => StatusReport::new(ServiceState::Unavailable.as_str(), None),
        Err(reason) => StatusReport::new(UNVERIFIED, Some(reason)),
    }
}

fn bookkept(
    record: ServicePid,
    health: Option<HealthDocument>,
    now: chrono::DateTime<chrono::Utc>,
) -> StatusReport {
    let (state, reason) = match &health {
        Some(doc) if !doc.is_stale(now) => (ServiceState::Available, None),
        Some(doc) => (
            ServiceState::Degraded,
            Some(match doc.age_seconds(now) {
                Some(age) => format!("health has not been refreshed for {age}s"),
                None => "health timestamp is unreadable".to_string(),
            }),
        ),
        None => (
            ServiceState::Degraded,
            Some("process is running but has published no health".to_string()),
        ),
    };
    let mut report = StatusReport::new(state.as_str(), reason);
    report.pid = Some(record.pid);
    report.health_age_secs = health.as_ref().and_then(|doc| doc.age_seconds(now));
    report.runtime_generation = health.map(|doc| doc.runtime_generation);
    report
}

fn read_health(path: &Path) -> Result<Option<HealthDocument>, String> {
    match std::fs::read(path) {
        Ok(raw) => match serde_json::from_slice::<HealthDocument>(&raw) {
            Ok(document) => Ok(Some(document)),
            // A half-written or older document is not proof of anything; treat
            // it as absent so the pid and lock decide.
            Err(_) => Ok(None),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("health file unreadable: {error}")),
    }
}

/// Whether the token lock is held, and by whom.
///
/// `Held(None)` is a real and distinct answer: the lock is held, so something
/// is serving, but the holder could not be identified. That is still degraded,
/// not down — the pid is simply unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockState {
    Free,
    Held(Option<u32>),
}

/// Probe the token lock.
///
/// Holding is established by a non-blocking lock attempt: if the attempt
/// succeeds the lock was free, and releasing it immediately restores the prior
/// state. Identity then comes from `/proc`.
fn lock_state(
    path: &Path,
    is_service_argv: impl Fn(&[String]) -> bool,
) -> Result<LockState, String> {
    use std::os::unix::io::AsRawFd;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(LockState::Free),
        Err(error) => return Err(format!("token lock unreadable: {error}")),
    };
    // SAFETY: `file` owns the descriptor for the duration of the call.
    let attempt = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if attempt == 0 {
        // The lock was free. Release it so this read leaves nothing behind.
        // SAFETY: as above; we hold the lock we are releasing.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Ok(LockState::Free);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EWOULDBLOCK) => {}
        _ => return Err(format!("token lock could not be probed: {error}")),
    }
    match probe::descriptor_holder(path, is_service_argv) {
        Ok(holder) => Ok(LockState::Held(holder)),
        Err(error) => Err(format!("lock holder could not be identified: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{
        ProcessHealth, RunMode, ServiceHealth, TelegramHealth, TurnOccupancy, WorkloadHealth,
    };
    use std::collections::BTreeMap;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn pid_path(&self) -> std::path::PathBuf {
            self.dir.path().join(pidfile::SERVICE_PID_FILE_NAME)
        }

        fn health_path(&self) -> std::path::PathBuf {
            self.dir.path().join(crate::health::HEALTH_FILE_NAME)
        }

        fn lock_path(&self) -> std::path::PathBuf {
            self.dir.path().join(".telegram-token.lock")
        }

        fn write_health(&self, updated_at: chrono::DateTime<chrono::Utc>) {
            let document = HealthDocument {
                schema_version: crate::health::HEALTH_SCHEMA_VERSION,
                started_at: updated_at.to_rfc3339(),
                last_poll_at: updated_at.to_rfc3339(),
                active_turn_count: 0,
                queued_counts: BTreeMap::new(),
                service_pid: std::process::id(),
                process: ProcessHealth {
                    pid: std::process::id(),
                    started_at: updated_at.to_rfc3339(),
                    mode: RunMode::Run,
                },
                service: ServiceHealth {
                    state: ServiceState::Available,
                    reason: None,
                },
                telegram: TelegramHealth {
                    state: ServiceState::Available,
                    last_ok_at: Some(updated_at.to_rfc3339()),
                    last_error_at: None,
                    consecutive_failures: 0,
                },
                workload: WorkloadHealth {
                    active_requests: 0,
                    waiting_for_turn: 0,
                    turn_occupancy: TurnOccupancy::Idle,
                    oldest_request_age_seconds: None,
                },
                runtime_generation: crate::generation::current().clone(),
                updated_at: updated_at.to_rfc3339(),
            };
            std::fs::write(self.health_path(), serde_json::to_vec(&document).unwrap()).unwrap();
        }

        fn write_live_pid(&self) {
            let argv = probe::cmdline(std::process::id()).unwrap();
            let record = ServicePid::for_current_process(&argv);
            assert!(record.is_live());
            std::fs::write(self.pid_path(), serde_json::to_vec(&record).unwrap()).unwrap();
        }

        fn inputs(&self) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
            (self.pid_path(), self.health_path(), self.lock_path())
        }
    }

    fn determine_at(fixture: &Fixture, now: chrono::DateTime<chrono::Utc>) -> StatusReport {
        let (pid_path, health_path, lock_path) = fixture.inputs();
        determine(
            &StatusInputs {
                pid_path: &pid_path,
                health_path: &health_path,
                lock_path: &lock_path,
            },
            now,
            |_| true,
        )
    }

    #[test]
    fn nothing_on_disk_is_unavailable() {
        let fixture = Fixture::new();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, "unavailable");
        assert_eq!(report.exit_code(), 2);
        assert!(report.to_text().starts_with("Bot status: unavailable\n"));
    }

    #[test]
    fn live_pid_with_fresh_health_is_available() {
        let fixture = Fixture::new();
        let now = chrono::Utc::now();
        fixture.write_live_pid();
        fixture.write_health(now);
        let report = determine_at(&fixture, now);
        assert_eq!(report.state, "available");
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.pid, Some(std::process::id()));
        assert!(report.runtime_generation.is_some());
        assert!(report.to_text().starts_with("Bot status: available\n"));
    }

    #[test]
    fn live_pid_with_stale_health_is_degraded_not_available() {
        let fixture = Fixture::new();
        let now = chrono::Utc::now();
        fixture.write_live_pid();
        fixture
            .write_health(now - chrono::Duration::seconds(crate::health::STALE_AFTER_SECONDS + 1));
        let report = determine_at(&fixture, now);
        assert_eq!(report.state, "degraded");
        assert_eq!(report.exit_code(), 1);
        assert!(report.reason.is_some());
    }

    #[test]
    fn live_pid_without_health_is_degraded() {
        let fixture = Fixture::new();
        fixture.write_live_pid();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, "degraded");
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn dead_pid_record_without_a_lock_holder_is_unavailable() {
        let fixture = Fixture::new();
        let record = ServicePid {
            pid: u32::MAX - 1,
            started_at: chrono::Utc::now().to_rfc3339(),
            boot_id: probe::boot_id().unwrap_or_default(),
            argv_hash: pidfile::argv_hash(&["danso".to_string()]),
        };
        std::fs::write(fixture.pid_path(), serde_json::to_vec(&record).unwrap()).unwrap();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, "unavailable");
    }

    #[test]
    fn an_unlocked_lock_file_is_not_evidence_of_a_service() {
        let fixture = Fixture::new();
        // The file is retained on purpose after a stop; only the kernel lock
        // means ownership.
        std::fs::write(fixture.lock_path(), b"").unwrap();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(
            report.state, "unavailable",
            "a leftover lock file must not read as a running service"
        );
    }

    #[test]
    fn a_held_lock_without_pid_bookkeeping_is_degraded() {
        use std::os::unix::io::AsRawFd;
        let fixture = Fixture::new();
        let file = std::fs::File::create(fixture.lock_path()).unwrap();
        // SAFETY: the descriptor is owned by `file` for the whole test.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, "degraded");
        assert_eq!(report.exit_code(), 1);
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pid bookkeeping"))
        );
    }

    #[test]
    fn unparseable_pid_debris_falls_through_to_the_lock() {
        let fixture = Fixture::new();
        std::fs::write(fixture.pid_path(), b"{ not json").unwrap();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, "unavailable");
        assert_eq!(report.exit_code(), 2);
    }

    #[test]
    fn unreadable_evidence_is_unverified_never_down() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.pid_path()).unwrap();
        let report = determine_at(&fixture, chrono::Utc::now());
        assert_eq!(report.state, UNVERIFIED);
        assert_eq!(report.exit_code(), 3);
        assert!(
            !report.to_text().contains("unavailable"),
            "an unverified read must not render as DOWN"
        );
    }

    #[test]
    fn status_json_always_carries_an_empty_mutations_object() {
        let fixture = Fixture::new();
        let report = determine_at(&fixture, chrono::Utc::now());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["mutations"],
            serde_json::Value::Object(serde_json::Map::new())
        );
    }
}
