//! `health.json` — the single document the fleet watch, `doctor` and
//! self-update read (`docs/unified-design.md` §6.4, §7).
//!
//! The schema is a deliberate *subset* of ccc-node's `schema_version 1`: only
//! the keys those consumers actually read. The pre-existing keys are preserved
//! byte-for-byte alongside the additions, so a reader written against the old
//! document keeps working.

use crate::{generation::RuntimeGeneration, status::ServiceState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HEALTH_FILE_NAME: &str = "health.json";
pub const HEALTH_SCHEMA_VERSION: u64 = 1;

/// How long a health document may go unrefreshed before it stops being
/// evidence that the service is serving.
///
/// The poll long-polls for at most `DEFAULT_POLL_TIMEOUT_SECONDS` and writes on
/// every wakeup, so this is several poll periods: long enough that an idle
/// service is never called stale, short enough that a wedged one is caught.
pub const STALE_AFTER_SECONDS: i64 = 150;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Run,
    Supervise,
}

/// How this process was started.
///
/// The polling loop publishes the mode but does not choose it — `danso service`
/// does, before the loop exists. A process-wide cell is the honest shape for a
/// property of the process itself; it is set once at startup and only read
/// afterwards. `Run` is the default so an embedder that never sets it still
/// publishes a truthful document.
static RUN_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_run_mode(mode: RunMode) {
    let encoded = match mode {
        RunMode::Run => 0,
        RunMode::Supervise => 1,
    };
    RUN_MODE.store(encoded, std::sync::atomic::Ordering::Release);
}

pub fn run_mode() -> RunMode {
    match RUN_MODE.load(std::sync::atomic::Ordering::Acquire) {
        1 => RunMode::Supervise,
        _ => RunMode::Run,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnOccupancy {
    Idle,
    Occupied,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessHealth {
    pub pid: u32,
    pub started_at: String,
    pub mode: RunMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceHealth {
    pub state: ServiceState,
    /// Present only when the state is not `available`; a reason on a healthy
    /// service reads as an unacknowledged fault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramHealth {
    pub state: ServiceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<String>,
    pub consecutive_failures: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadHealth {
    pub active_requests: u64,
    pub waiting_for_turn: u64,
    pub turn_occupancy: TurnOccupancy,
    /// Age of the longest-running turn. `None` when nothing is running — zero
    /// would be indistinguishable from "a turn that just started".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_request_age_seconds: Option<u64>,
}

/// The whole document. Legacy keys come first and keep their exact names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthDocument {
    pub schema_version: u64,
    pub started_at: String,
    pub last_poll_at: String,
    pub active_turn_count: u64,
    pub queued_counts: BTreeMap<String, usize>,
    pub service_pid: u32,
    pub process: ProcessHealth,
    pub service: ServiceHealth,
    pub telegram: TelegramHealth,
    pub workload: WorkloadHealth,
    pub runtime_generation: RuntimeGeneration,
    pub updated_at: String,
}

impl HealthDocument {
    /// The timestamp staleness is measured from.
    pub fn observed_at(&self) -> &str {
        &self.updated_at
    }

    /// Seconds since this document was written, or `None` when `updated_at` is
    /// not a timestamp we can read. An unreadable timestamp must not silently
    /// become "fresh".
    pub fn age_seconds(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let written = chrono::DateTime::parse_from_rfc3339(self.observed_at()).ok()?;
        Some((now - written.with_timezone(&chrono::Utc)).num_seconds())
    }

    /// Whether the document is too old to be evidence of a serving process.
    ///
    /// A document from the future is not treated as stale: clock adjustment is
    /// not a reason to report a running service as down.
    pub fn is_stale(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        match self.age_seconds(now) {
            Some(age) => age > STALE_AFTER_SECONDS,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(updated_at: &str) -> HealthDocument {
        HealthDocument {
            schema_version: HEALTH_SCHEMA_VERSION,
            started_at: "2026-09-16T00:00:00+00:00".to_string(),
            last_poll_at: updated_at.to_string(),
            active_turn_count: 0,
            queued_counts: BTreeMap::new(),
            service_pid: 1,
            process: ProcessHealth {
                pid: 1,
                started_at: "2026-09-16T00:00:00+00:00".to_string(),
                mode: RunMode::Run,
            },
            service: ServiceHealth {
                state: ServiceState::Available,
                reason: None,
            },
            telegram: TelegramHealth {
                state: ServiceState::Available,
                last_ok_at: Some(updated_at.to_string()),
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
            updated_at: updated_at.to_string(),
        }
    }

    #[test]
    fn legacy_keys_keep_their_names() {
        let value = serde_json::to_value(document("2026-09-16T00:00:00+00:00")).unwrap();
        for key in [
            "schema_version",
            "started_at",
            "last_poll_at",
            "active_turn_count",
            "queued_counts",
            "service_pid",
        ] {
            assert!(value.get(key).is_some(), "legacy key {key} disappeared");
        }
        assert_eq!(value["schema_version"], 1);
    }

    #[test]
    fn extension_keys_are_present_with_the_documented_names() {
        let value = serde_json::to_value(document("2026-09-16T00:00:00+00:00")).unwrap();
        for key in [
            "process",
            "service",
            "telegram",
            "workload",
            "runtime_generation",
            "updated_at",
        ] {
            assert!(value.get(key).is_some(), "missing extension key {key}");
        }
        assert_eq!(value["service"]["state"], "available");
        assert_eq!(value["workload"]["turn_occupancy"], "idle");
        assert_eq!(value["process"]["mode"], "run");
        assert_eq!(
            value["runtime_generation"]["schema"],
            crate::generation::RUNTIME_GENERATION_SCHEMA
        );
    }

    #[test]
    fn absent_measurements_are_omitted_not_zeroed() {
        let value = serde_json::to_value(document("2026-09-16T00:00:00+00:00")).unwrap();
        assert!(
            value["workload"]
                .get("oldest_request_age_seconds")
                .is_none(),
            "an idle service must not report an age of zero"
        );
        assert!(value["service"].get("reason").is_none());
    }

    #[test]
    fn staleness_uses_the_documented_threshold() {
        let now = chrono::Utc::now();
        let fresh = document(&(now - chrono::Duration::seconds(STALE_AFTER_SECONDS)).to_rfc3339());
        assert!(!fresh.is_stale(now));
        let stale =
            document(&(now - chrono::Duration::seconds(STALE_AFTER_SECONDS + 1)).to_rfc3339());
        assert!(stale.is_stale(now));
    }

    #[test]
    fn a_future_timestamp_is_not_stale() {
        let now = chrono::Utc::now();
        let skewed = document(&(now + chrono::Duration::seconds(600)).to_rfc3339());
        assert!(
            !skewed.is_stale(now),
            "clock skew must not report a running service as down"
        );
    }

    #[test]
    fn an_unreadable_timestamp_is_stale() {
        let broken = document("not-a-timestamp");
        assert!(broken.age_seconds(chrono::Utc::now()).is_none());
        assert!(broken.is_stale(chrono::Utc::now()));
    }

    #[test]
    fn document_round_trips() {
        let original = document("2026-09-16T00:00:00+00:00");
        let encoded = serde_json::to_vec(&original).unwrap();
        let decoded: HealthDocument = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(original, decoded);
    }
}
