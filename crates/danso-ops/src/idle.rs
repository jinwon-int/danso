//! The idle gate: do not replace a binary out from under a running turn.
//!
//! Ported from ccc-node `scripts/ccc-self-update.sh` (the `bridge_is_busy`
//! function and the deferral block that follows it). The reason that script
//! gives is the reason here: a restart during an in-flight turn SIGTERMs the
//! child doing the work, and the user sees the turn die rather than finish.
//!
//! ## What "busy" reads
//!
//! Two scalars out of `health.json`'s `workload` object — `active_requests`
//! and `oldest_request_age_seconds` — plus the document's top-level
//! `updated_at` for freshness. **Not `turn_occupancy`.** `docs/unified-design.md`
//! §6.3 says the gate reads `turn_occupancy`; it does not, and neither does
//! the ccc original it is a port of, whose own test fixtures write only those
//! two scalars (`ccc-self-update.test.sh`, `mk_health`). `turn_occupancy` is a
//! rendering of the same state for `status` and the doctor, and making it
//! load-bearing here would add a second source of truth for one fact.
//!
//! ## Why the document is parsed loosely
//!
//! Deliberately as JSON values rather than through [`crate::health::HealthDocument`].
//! The process that wrote the health document is the *old* generation — that is
//! the whole point of the gate — so its schema is whatever the previous release
//! published. Parsing strictly would turn a schema addition into a parse
//! failure, and a parse failure fails open, which is precisely the case where
//! failing open kills a turn.
//!
//! ## Fail-open, on purpose
//!
//! Every way of not knowing — no file, unreadable, invalid JSON, missing or
//! non-numeric fields, unparsable or stale `updated_at` — means *proceed*.
//! The alternative is an update lane that a single corrupt file can stop
//! forever, silently. The gate is an optimisation for the common case, not a
//! safety property, and it is written to fail in the direction that keeps
//! updates flowing. Note the contrast with the restart handoff, which is
//! fail-*closed*: not being able to schedule a restart means no restart.
//!
//! ## The deferral is bounded
//!
//! A busy bridge defers, and the wait is remembered in a marker file so the
//! budget spans runs rather than restarting each time. Past
//! [`MAX_DEFER_SECONDS`] the update proceeds **anyway**, busy or not: a hung
//! turn, or continuous load, must not starve updates forever. The same
//! reasoning gives [`BUSY_MAX_SECONDS`] — a turn that has been running longer
//! than that is not evidence of a healthy bridge doing work, so it does not
//! count as busy at all.

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// How fresh `health.json` must be for its workload numbers to mean anything.
///
/// Older than this and the numbers describe a process that may not exist. The
/// ccc original uses the same 90 seconds against a reporter that ticks every
/// 10s with a 30s idle-write throttle. Danso republishes health on every poll
/// iteration and the Telegram long poll defaults to 25 seconds, so an idle
/// service refreshes roughly three times inside this window. Raising
/// `DANSO_TELEGRAM_POLL_TIMEOUT_SECONDS` past this value would make an idle
/// service read as stale — which fails open, so the gate stops deferring
/// rather than stops updating.
pub const HEALTH_FRESH_SECONDS: i64 = 90;

/// A turn older than this stops counting as "busy".
///
/// Without it one wedged turn defers every update until the deferral cap, and
/// the operator learns about it from a stale generation rather than from the
/// wedged turn. `docs/unified-design.md` §6.3 omits this rule; the ccc original
/// has it, and dropping it would make the gate strictly worse.
pub const BUSY_MAX_SECONDS: u64 = 1800;

/// Total time a busy bridge may hold an update back, across runs.
pub const MAX_DEFER_SECONDS: i64 = 3600;

/// Where the accumulated deferral is remembered, under the state directory.
pub const DEFER_MARKER_FILE: &str = "self-update.deferred-since";

/// Exit code for "deferred, nothing was changed" — ccc's exit 8.
///
/// A distinct code because a cron wrapper must be able to treat a deferral as
/// an ordinary outcome rather than a failure; ccc registers its task with
/// `--success-exit-codes 0,8,11` for exactly this.
pub const EXIT_DEFERRED: i32 = 8;

/// Why the gate thinks the service is busy.
///
/// Counts only. The health document holds chat and conversation identifiers
/// and none of them belong in an update log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BusyReason {
    pub active: u64,
    pub oldest_seconds: u64,
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "active={} oldest={}s", self.active, self.oldest_seconds)
    }
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum Gate {
    /// Nothing is in flight, or the gate could not tell, or the budget ran out.
    Proceed { forced: bool },
    /// The service is serving; try again later.
    Deferred {
        reason: BusyReason,
        waited_seconds: i64,
    },
}

impl Gate {
    pub fn exit_code(&self) -> i32 {
        match self {
            Gate::Proceed { .. } => 0,
            Gate::Deferred { .. } => EXIT_DEFERRED,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Gate::Proceed { forced: true } => "proceeding: idle gate bypassed".to_string(),
            Gate::Proceed { forced: false } => "proceeding: nothing in flight".to_string(),
            Gate::Deferred {
                reason,
                waited_seconds,
            } => format!("deferred: service busy ({reason}), waited {waited_seconds}s"),
        }
    }
}

/// Read the two workload scalars, or `None` if the service is not busy.
///
/// `None` covers both "idle" and "cannot tell"; they are the same instruction
/// to the caller and distinguishing them would invite a future edit that
/// treats one of them as a reason to stop.
pub fn busy(health_path: Option<&Path>, now: chrono::DateTime<chrono::Utc>) -> Option<BusyReason> {
    let raw = std::fs::read(health_path?).ok()?;
    let document: serde_json::Value = serde_json::from_slice(&raw).ok()?;

    // A missing `updated_at`, or one that will not parse, is not freshness.
    let updated_at = document.get("updated_at")?.as_str()?;
    let updated_at = chrono::DateTime::parse_from_rfc3339(updated_at).ok()?;
    let age = now
        .signed_duration_since(updated_at.with_timezone(&chrono::Utc))
        .num_seconds();
    // Both directions: a document from the future is not evidence either, and
    // a clock step must not make a stale document look current.
    if !(0..=HEALTH_FRESH_SECONDS).contains(&age) {
        return None;
    }

    let workload = document.get("workload")?;
    let active = workload.get("active_requests")?.as_u64()?;
    if active == 0 {
        return None;
    }
    // Absent is zero here, unlike the fields above: a workload object that
    // reports active turns but no age still describes a busy service, and
    // reading that as idle would defeat the gate.
    let oldest_seconds = workload
        .get("oldest_request_age_seconds")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if oldest_seconds >= BUSY_MAX_SECONDS {
        return None;
    }
    Some(BusyReason {
        active,
        oldest_seconds,
    })
}

/// Where the deferral marker lives for a given state directory.
pub fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEFER_MARKER_FILE)
}

/// Decide whether an update may proceed, recording the accumulated wait.
///
/// Writing the marker is best-effort. A state directory that cannot be written
/// is a real problem, but it is `install`'s problem — refusing the update here
/// would mean an unwritable directory silently stops updates, which is the
/// failure mode this gate is written to avoid.
pub fn check(
    state_dir: &Path,
    health_path: Option<&Path>,
    force: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Gate {
    if force {
        clear_marker(state_dir);
        return Gate::Proceed { forced: true };
    }
    let Some(reason) = busy(health_path, now) else {
        clear_marker(state_dir);
        return Gate::Proceed { forced: false };
    };

    let marker = marker_path(state_dir);
    let now_epoch = now.timestamp();
    // A marker that is empty, non-numeric, or dated in the future restarts the
    // clock rather than aborting: corruption must not become a way to make the
    // cap unreachable, in either direction.
    let since = read_marker(&marker).filter(|since| *since <= now_epoch);
    let since = match since {
        Some(since) => since,
        None => {
            write_marker(&marker, now_epoch);
            now_epoch
        }
    };

    let waited_seconds = now_epoch - since;
    if waited_seconds < MAX_DEFER_SECONDS {
        return Gate::Deferred {
            reason,
            waited_seconds,
        };
    }
    // The budget is spent. Proceed even though the service is busy, and clear
    // the marker so the next update starts with a full budget of its own.
    clear_marker(state_dir);
    Gate::Proceed { forced: false }
}

fn read_marker(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_marker(path: &Path, epoch: i64) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, epoch.to_string());
}

fn clear_marker(state_dir: &Path) {
    let _ = std::fs::remove_file(marker_path(state_dir));
}

/// Read the marker, for `update status` and for tests.
pub fn deferred_since(state_dir: &Path) -> Result<Option<i64>> {
    Ok(read_marker(&marker_path(state_dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds_ago: i64, now: chrono::DateTime<chrono::Utc>) -> String {
        (now - chrono::Duration::seconds(seconds_ago)).to_rfc3339()
    }

    /// Only the fields the gate actually reads, matching ccc's `mk_health`.
    fn health(
        dir: &Path,
        updated_ago: i64,
        active: u64,
        oldest: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> PathBuf {
        let path = dir.join("health.json");
        let document = serde_json::json!({
            "updated_at": at(updated_ago, now),
            "workload": {
                "active_requests": active,
                "oldest_request_age_seconds": oldest,
            },
        });
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        path
    }

    #[test]
    fn a_serving_bridge_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 2, 30, now);
        assert_eq!(
            busy(Some(&path), now),
            Some(BusyReason {
                active: 2,
                oldest_seconds: 30
            })
        );
    }

    #[test]
    fn an_idle_bridge_is_not_busy() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 0, 0, now);
        assert_eq!(busy(Some(&path), now), None);
    }

    #[test]
    fn a_stale_document_is_not_evidence_of_work() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        // Busy numbers, but written by a process that may no longer exist.
        let path = health(dir.path(), HEALTH_FRESH_SECONDS + 1, 3, 10, now);
        assert_eq!(
            busy(Some(&path), now),
            None,
            "a document older than the freshness window describes a process \
             that may be gone; blocking on it would stop updates forever"
        );
    }

    #[test]
    fn a_document_from_the_future_is_not_evidence_either() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), -600, 3, 10, now);
        assert_eq!(busy(Some(&path), now), None);
    }

    #[test]
    fn a_wedged_turn_stops_counting_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 1, BUSY_MAX_SECONDS, now);
        assert_eq!(
            busy(Some(&path), now),
            None,
            "a turn past the per-task cap is a stuck turn, not a reason to \
             keep deferring updates"
        );
        let still = health(dir.path(), 5, 1, BUSY_MAX_SECONDS - 1, now);
        assert!(busy(Some(&still), now).is_some(), "the cap is exclusive");
    }

    #[test]
    fn every_way_of_not_knowing_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        assert_eq!(busy(None, now), None, "no service data directory");
        assert_eq!(
            busy(Some(&dir.path().join("absent.json")), now),
            None,
            "no health document"
        );

        let cases: [(&str, serde_json::Value); 5] = [
            (
                "no updated_at",
                serde_json::json!({"workload":{"active_requests":3}}),
            ),
            (
                "unparsable updated_at",
                serde_json::json!({"updated_at":"soon","workload":{"active_requests":3}}),
            ),
            ("no workload", serde_json::json!({"updated_at": at(1, now)})),
            (
                "non-numeric active_requests",
                serde_json::json!({"updated_at": at(1, now),"workload":{"active_requests":"many"}}),
            ),
            (
                "workload is not an object",
                serde_json::json!({"updated_at": at(1, now),"workload": 7}),
            ),
        ];
        for (label, document) in cases {
            let path = dir.path().join("case.json");
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            assert_eq!(busy(Some(&path), now), None, "{label}");
        }

        let path = dir.path().join("broken.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(busy(Some(&path), now), None, "invalid JSON");
    }

    #[test]
    fn a_schema_this_binary_does_not_know_still_defers() {
        // The health document was written by the generation being replaced, so
        // it is the *old* schema by construction. Strict parsing would fail
        // open here and kill the turn this gate exists to protect.
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = dir.path().join("health.json");
        let document = serde_json::json!({
            "schema_version": 99,
            "updated_at": at(2, now),
            "workload": {
                "active_requests": 1,
                "oldest_request_age_seconds": 12,
                "a_field_from_the_future": {"nested": true},
            },
            "something_else_entirely": [1, 2, 3],
        });
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(
            busy(Some(&path), now),
            Some(BusyReason {
                active: 1,
                oldest_seconds: 12
            })
        );
    }

    #[test]
    fn active_turns_without_an_age_still_count_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let path = dir.path().join("health.json");
        let document = serde_json::json!({
            "updated_at": at(2, now),
            "workload": {"active_requests": 4},
        });
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(
            busy(Some(&path), now),
            Some(BusyReason {
                active: 4,
                oldest_seconds: 0
            })
        );
    }

    #[test]
    fn a_busy_bridge_defers_and_remembers_how_long() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 1, 3, now);

        let first = check(&state, Some(&path), false, now);
        assert!(matches!(first, Gate::Deferred { .. }));
        assert_eq!(first.exit_code(), EXIT_DEFERRED);
        let since = deferred_since(&state).unwrap().expect("marker written");

        // A later run in the same busy period accumulates rather than resetting.
        let later = now + chrono::Duration::seconds(600);
        let path = health(dir.path(), 5, 1, 3, later);
        match check(&state, Some(&path), false, later) {
            Gate::Deferred { waited_seconds, .. } => assert_eq!(waited_seconds, 600),
            other => panic!("expected a deferral, got {other:?}"),
        }
        assert_eq!(
            deferred_since(&state).unwrap(),
            Some(since),
            "the marker records when waiting began, not when it was last checked"
        );
    }

    #[test]
    fn the_deferral_budget_runs_out_and_the_update_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 1, 3, now);
        check(&state, Some(&path), false, now);

        let past_cap = now + chrono::Duration::seconds(MAX_DEFER_SECONDS);
        let path = health(dir.path(), 5, 1, 3, past_cap);
        assert_eq!(
            check(&state, Some(&path), false, past_cap),
            Gate::Proceed { forced: false },
            "continuous load must not starve updates forever"
        );
        assert_eq!(
            deferred_since(&state).unwrap(),
            None,
            "the next update starts with a budget of its own"
        );
    }

    #[test]
    fn going_idle_clears_the_accumulated_wait() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        check(&state, Some(&health(dir.path(), 5, 1, 3, now)), false, now);
        assert!(deferred_since(&state).unwrap().is_some());

        let idle = now + chrono::Duration::seconds(10);
        let path = health(dir.path(), 5, 0, 0, idle);
        assert_eq!(
            check(&state, Some(&path), false, idle),
            Gate::Proceed { forced: false }
        );
        assert_eq!(deferred_since(&state).unwrap(), None);
    }

    #[test]
    fn force_bypasses_the_gate_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 9, 3, now);
        check(&state, Some(&path), false, now);
        assert!(deferred_since(&state).unwrap().is_some());

        assert_eq!(
            check(&state, Some(&path), true, now),
            Gate::Proceed { forced: true }
        );
        assert_eq!(deferred_since(&state).unwrap(), None);
    }

    #[test]
    fn a_corrupt_marker_restarts_the_clock_rather_than_aborting() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 1, 3, now);

        for corrupt in ["", "   ", "not-a-number", "9999999999999"] {
            std::fs::write(marker_path(&state), corrupt).unwrap();
            match check(&state, Some(&path), false, now) {
                Gate::Deferred { waited_seconds, .. } => assert_eq!(
                    waited_seconds, 0,
                    "a corrupt marker ({corrupt:?}) must restart the wait, not \
                     make the cap unreachable in either direction"
                ),
                other => panic!("expected a deferral for {corrupt:?}, got {other:?}"),
            }
            assert_eq!(deferred_since(&state).unwrap(), Some(now.timestamp()));
        }
    }

    #[test]
    fn an_unwritable_state_directory_does_not_stop_updates() {
        // Best-effort marker: the gate is an optimisation, and a directory it
        // cannot write must not become a silent brake on every future update.
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("absent").join("deeper");
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 0, 0, now);
        assert_eq!(
            check(&state, Some(&path), false, now),
            Gate::Proceed { forced: false }
        );
    }

    #[test]
    fn the_reason_carries_counts_and_nothing_else() {
        let reason = BusyReason {
            active: 2,
            oldest_seconds: 45,
        };
        assert_eq!(reason.to_string(), "active=2 oldest=45s");
        let rendered = serde_json::to_string(&Gate::Deferred {
            reason,
            waited_seconds: 7,
        })
        .unwrap();
        for forbidden in ["chat", "user", "conversation", "token", "path"] {
            assert!(
                !rendered.contains(forbidden),
                "the deferral reason reaches a log and a chat; it carries \
                 counts only, found {forbidden:?} in {rendered}"
            );
        }
    }
}
