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
//! One deliberate divergence from the ccc original: its freshness test is
//! one-sided (`now - updated_at <= window`), so a future-dated document is
//! "fresh" and defers. This one bounds the age on both sides, so a document
//! from the future is not evidence and the update proceeds. A clock stepping
//! backwards — a resumed VM, a late NTP correction — makes every existing
//! document future-dated at once, and under the one-sided rule that turns into
//! a deferral the operator cannot explain.
//!
//! ## What this does *not* protect
//!
//! `apply` replaces a file; it does not restart anything. The SIGTERM that
//! ends a turn comes from the restart that follows, and an operator who runs
//! `apply` (exit 8, deferred) and then restarts by hand anyway is not
//! protected by any of this. The gate's real job is to make the *wrapper*
//! stop early — which is why deferral is an exit code and not a warning.
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

/// Why the gate let an update through.
///
/// Four different facts, and an operator needs them apart. Two of them mean
/// "a turn is running and we are about to kill it", and collapsing those into
/// the same value as `Idle` is how that becomes invisible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Proceeding {
    /// Nothing in flight, or no evidence that anything is.
    Idle,
    /// `--force`.
    Forced { busy: Option<BusyReason> },
    /// The hour ran out. A turn is running and this update will end it.
    BudgetExhausted {
        busy: BusyReason,
        waited_seconds: i64,
    },
    /// The wait could not be written down, so it could not be bounded.
    ///
    /// Deferring here would defer from zero every run and the cap would never
    /// be reached — a permanently blocked update lane, which is the one
    /// outcome this gate must never produce. A turn is running and this update
    /// will end it.
    Untrackable { busy: BusyReason },
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum Gate {
    Proceed(Proceeding),
    /// The service is serving; try again later.
    Deferred {
        reason: BusyReason,
        waited_seconds: i64,
    },
}

impl Gate {
    pub fn exit_code(&self) -> i32 {
        match self {
            Gate::Proceed(_) => 0,
            Gate::Deferred { .. } => EXIT_DEFERRED,
        }
    }

    /// `true` when a turn is running and this update is going in anyway.
    ///
    /// The caller has to be able to say this out loud; it is the one case
    /// where the gate knowingly does the thing it exists to prevent.
    pub fn overrides_a_live_turn(&self) -> bool {
        matches!(
            self,
            Gate::Proceed(
                Proceeding::BudgetExhausted { .. }
                    | Proceeding::Untrackable { .. }
                    | Proceeding::Forced { busy: Some(_) }
            )
        )
    }

    /// A fixed token for the update log. Never interpolated content.
    pub fn log_event(&self) -> &'static str {
        match self {
            Gate::Deferred { .. } => "deferred",
            Gate::Proceed(Proceeding::Idle) => "gate_idle",
            Gate::Proceed(Proceeding::Forced { .. }) => "gate_forced",
            Gate::Proceed(Proceeding::BudgetExhausted { .. }) => "gate_budget_exhausted",
            Gate::Proceed(Proceeding::Untrackable { .. }) => "gate_untrackable",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Gate::Proceed(Proceeding::Idle) => "proceeding: nothing in flight".to_string(),
            Gate::Proceed(Proceeding::Forced { busy: None }) => {
                "proceeding: idle gate bypassed".to_string()
            }
            Gate::Proceed(Proceeding::Forced { busy: Some(busy) }) => {
                format!("proceeding: idle gate bypassed while serving ({busy})")
            }
            Gate::Proceed(Proceeding::BudgetExhausted {
                busy,
                waited_seconds,
            }) => format!(
                "proceeding: service still busy ({busy}) after {waited_seconds}s, \
                 deferral budget spent — this will end the turn in flight"
            ),
            Gate::Proceed(Proceeding::Untrackable { busy }) => format!(
                "proceeding: service busy ({busy}) but the deferral could not be \
                 recorded, so it could not be bounded — this will end the turn \
                 in flight"
            ),
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
        .and_then(seconds)
        .unwrap_or(0);
    if oldest_seconds >= BUSY_MAX_SECONDS {
        return None;
    }
    Some(BusyReason {
        active,
        oldest_seconds,
    })
}

/// A duration in seconds, however the writing generation chose to encode it.
///
/// The ccc original coerces with `float(...)`, so `3600.0` and `"3600"` are
/// 3600 there. Reading only a JSON integer would turn either into the
/// `unwrap_or(0)` default, and a wedged hour-long turn would read as a
/// zero-second one — defeating [`BUSY_MAX_SECONDS`] on exactly the documents
/// this module parses loosely because they come from another generation.
fn seconds(value: &serde_json::Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_f64() {
        // Negative or non-finite is not a duration; treat it as unstated.
        return (number.is_finite() && number >= 0.0).then_some(number as u64);
    }
    value
        .as_str()?
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|n| (n.is_finite() && n >= 0.0).then_some(n as u64))
}

/// Where the deferral marker lives for a given state directory.
pub fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEFER_MARKER_FILE)
}

/// Decide whether an update may proceed, recording the accumulated wait.
///
/// **A deferral this function cannot write down is a deferral it does not
/// make.** The marker is what bounds the wait; without it every run would
/// start the clock again, `waited_seconds` would be zero forever, and the cap
/// would be unreachable — a busy service would block updates permanently and
/// silently. That is the single outcome this gate must not produce, so an
/// unwritable marker proceeds ([`Proceeding::Untrackable`]) rather than
/// deferring into a wait that has no end.
pub fn check(
    state_dir: &Path,
    health_path: Option<&Path>,
    force: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Gate {
    if force {
        let busy = busy(health_path, now);
        clear_marker(state_dir);
        return Gate::Proceed(Proceeding::Forced { busy });
    }
    let Some(reason) = busy(health_path, now) else {
        clear_marker(state_dir);
        return Gate::Proceed(Proceeding::Idle);
    };

    let marker = marker_path(state_dir);
    let now_epoch = now.timestamp();
    // A marker that is empty, non-numeric, or dated in the future restarts the
    // clock rather than aborting: corruption must not become a way to make the
    // cap unreachable, in either direction.
    let since = match read_marker(&marker).filter(|since| *since <= now_epoch) {
        Some(since) => since,
        // Starting a new deferral period, which only means anything if it
        // survives this process.
        None if write_marker(&marker, now_epoch) => now_epoch,
        None => return Gate::Proceed(Proceeding::Untrackable { busy: reason }),
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
    Gate::Proceed(Proceeding::BudgetExhausted {
        busy: reason,
        waited_seconds,
    })
}

fn read_marker(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Persist the start of a deferral. `false` when it did not land.
///
/// Written through a temporary file and renamed, like every other record under
/// `state/`: a concurrent reader must never see a truncated marker and decide
/// the accumulated wait was corrupt. This one is written without the update
/// lock — deliberately, because the gate runs before the lock is taken — so it
/// is the one state file where that race is reachable at all.
fn write_marker(path: &Path, epoch: i64) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let temporary = parent.join(format!(".{DEFER_MARKER_FILE}.{}", std::process::id()));
    if std::fs::write(&temporary, epoch.to_string()).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    if std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    true
}

fn clear_marker(state_dir: &Path) {
    let _ = std::fs::remove_file(marker_path(state_dir));
}

/// When the current deferral period began, if one is in progress.
///
/// `None` also covers a marker that exists but cannot be read or parsed —
/// [`check`] treats that the same way, by starting a new period.
pub fn deferred_since(state_dir: &Path) -> Option<i64> {
    read_marker(&marker_path(state_dir))
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
        let since = deferred_since(&state).expect("marker written");

        // A later run in the same busy period accumulates rather than resetting.
        let later = now + chrono::Duration::seconds(600);
        let path = health(dir.path(), 5, 1, 3, later);
        match check(&state, Some(&path), false, later) {
            Gate::Deferred { waited_seconds, .. } => assert_eq!(waited_seconds, 600),
            other => panic!("expected a deferral, got {other:?}"),
        }
        assert_eq!(
            deferred_since(&state),
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
        let gate = check(&state, Some(&path), false, past_cap);
        assert_eq!(
            gate,
            Gate::Proceed(Proceeding::BudgetExhausted {
                busy: BusyReason {
                    active: 1,
                    oldest_seconds: 3
                },
                waited_seconds: MAX_DEFER_SECONDS,
            }),
            "continuous load must not starve updates forever — and the outcome \
             must say a turn is being ended, not read as an idle proceed"
        );
        assert!(gate.overrides_a_live_turn());
        assert_eq!(
            deferred_since(&state),
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
        assert!(deferred_since(&state).is_some());

        let idle = now + chrono::Duration::seconds(10);
        let path = health(dir.path(), 5, 0, 0, idle);
        assert_eq!(
            check(&state, Some(&path), false, idle),
            Gate::Proceed(Proceeding::Idle)
        );
        assert_eq!(deferred_since(&state), None);
    }

    #[test]
    fn force_bypasses_the_gate_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 9, 3, now);
        check(&state, Some(&path), false, now);
        assert!(deferred_since(&state).is_some());

        let gate = check(&state, Some(&path), true, now);
        assert_eq!(
            gate,
            Gate::Proceed(Proceeding::Forced {
                busy: Some(BusyReason {
                    active: 9,
                    oldest_seconds: 3
                })
            }),
            "`--force` still records what it overrode: an operator who ends \
             nine turns on purpose should see that in the log"
        );
        assert!(gate.overrides_a_live_turn());
        assert_eq!(deferred_since(&state), None);
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
            assert_eq!(deferred_since(&state), Some(now.timestamp()));
        }
    }

    /// A marker path that can never be written: a directory of that name.
    ///
    /// `fs::write` fails with EISDIR and `remove_file` cannot clear it, which
    /// is the unrecoverable shape of this failure. The realistic trigger is a
    /// root-created 0600 marker that a lower-privileged run can neither read
    /// nor replace; this reproduces the same pair of failures without needing
    /// two users.
    fn unwritable_marker(state: &Path) {
        std::fs::create_dir_all(marker_path(state)).unwrap();
    }

    #[test]
    fn a_deferral_that_cannot_be_recorded_is_not_made() {
        // The wait is bounded by the marker. If the marker never lands, every
        // run starts the clock again, `waited_seconds` is zero forever and the
        // cap is unreachable — a busy service would block updates permanently
        // and silently, which is the one outcome this gate must not produce.
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        unwritable_marker(&state);
        let now = chrono::Utc::now();
        let busy_now = health(dir.path(), 5, 1, 3, now);

        let first = check(&state, Some(&busy_now), false, now);
        assert_eq!(
            first,
            Gate::Proceed(Proceeding::Untrackable {
                busy: BusyReason {
                    active: 1,
                    oldest_seconds: 3
                }
            }),
            "an unrecordable deferral must proceed, not defer into a wait that \
             can never end"
        );
        assert!(first.overrides_a_live_turn());

        // And it stays that way: the failure is not transient, so a later run
        // must not quietly start deferring from zero either.
        let later = now + chrono::Duration::seconds(MAX_DEFER_SECONDS * 2);
        let busy_later = health(dir.path(), 5, 1, 3, later);
        assert!(matches!(
            check(&state, Some(&busy_later), false, later),
            Gate::Proceed(Proceeding::Untrackable { .. })
        ));
    }

    #[test]
    fn an_unwritable_state_directory_does_not_stop_updates() {
        // Same rule one level up: a state directory that cannot be created.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let state = blocker.join("state");
        let now = chrono::Utc::now();
        let path = health(dir.path(), 5, 2, 3, now);
        assert!(
            matches!(
                check(&state, Some(&path), false, now),
                Gate::Proceed(Proceeding::Untrackable { .. })
            ),
            "a directory the gate cannot write must not become a silent brake \
             on every future update"
        );
    }

    #[test]
    fn the_freshness_window_is_inclusive_at_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        // Exactly at the window: still evidence. One second past: not.
        assert!(
            busy(
                Some(&health(dir.path(), HEALTH_FRESH_SECONDS, 1, 3, now)),
                now
            )
            .is_some(),
            "the freshness window is inclusive; an off-by-one here silently \
             narrows the gate"
        );
        assert!(
            busy(
                Some(&health(dir.path(), HEALTH_FRESH_SECONDS + 1, 1, 3, now)),
                now
            )
            .is_none()
        );
        // Age zero is the ordinary case for a document just written.
        assert!(busy(Some(&health(dir.path(), 0, 1, 3, now)), now).is_some());
        assert!(
            busy(Some(&health(dir.path(), -1, 1, 3, now)), now).is_none(),
            "a document from the future is not evidence — the deliberate \
             divergence from ccc's one-sided test"
        );
    }

    #[test]
    fn a_duration_counts_however_the_writing_generation_encoded_it() {
        // ccc coerces with `float(...)`. Reading only a JSON integer would turn
        // a wedged hour-long turn into a zero-second one and defeat the cap on
        // exactly the foreign documents this module parses loosely.
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        for encoded in [
            serde_json::json!(3600.0),
            serde_json::json!("3600"),
            serde_json::json!(3600),
        ] {
            let path = dir.path().join("health.json");
            let document = serde_json::json!({
                "updated_at": at(2, now),
                "workload": {"active_requests": 1, "oldest_request_age_seconds": encoded},
            });
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            assert_eq!(
                busy(Some(&path), now),
                None,
                "a wedged turn encoded as {encoded} must still be over the cap"
            );
        }
    }

    #[test]
    fn proceeding_over_a_live_turn_is_distinguishable_from_idle() {
        // The two cases where the gate knowingly kills a turn must not render
        // or serialize as the ordinary idle proceed, or nobody can tell from
        // the log that a turn was ended on purpose.
        let busy = BusyReason {
            active: 1,
            oldest_seconds: 4,
        };
        let idle = Gate::Proceed(Proceeding::Idle);
        let exhausted = Gate::Proceed(Proceeding::BudgetExhausted {
            busy,
            waited_seconds: MAX_DEFER_SECONDS,
        });
        let untrackable = Gate::Proceed(Proceeding::Untrackable { busy });

        assert!(!idle.overrides_a_live_turn());
        assert!(exhausted.overrides_a_live_turn());
        assert!(untrackable.overrides_a_live_turn());
        assert!(!Gate::Proceed(Proceeding::Forced { busy: None }).overrides_a_live_turn());
        assert!(Gate::Proceed(Proceeding::Forced { busy: Some(busy) }).overrides_a_live_turn());

        let events: Vec<&str> = [&idle, &exhausted, &untrackable]
            .iter()
            .map(|gate| gate.log_event())
            .collect();
        assert_eq!(
            events,
            ["gate_idle", "gate_budget_exhausted", "gate_untrackable"],
            "each outcome needs its own token or the log cannot separate them"
        );
        for gate in [&exhausted, &untrackable] {
            assert_ne!(gate.summary(), idle.summary());
            assert!(
                gate.summary().contains("end the turn in flight"),
                "the summary must say what is about to happen: {}",
                gate.summary()
            );
        }
    }

    #[test]
    fn the_reason_carries_counts_and_nothing_else() {
        let reason = BusyReason {
            active: 2,
            oldest_seconds: 45,
        };
        assert_eq!(reason.to_string(), "active=2 oldest=45s");
        // Pin the shape, not a denylist: a substring check passes for any new
        // field whose name nobody thought to forbid.
        let rendered = serde_json::to_value(Gate::Deferred {
            reason,
            waited_seconds: 7,
        })
        .unwrap();
        assert_eq!(
            rendered,
            serde_json::json!({
                "gate": "deferred",
                "reason": {"active": 2, "oldest_seconds": 45},
                "waited_seconds": 7,
            }),
            "this document reaches a log and an operator; it carries counts only"
        );
        let rendered = rendered.to_string();
        for forbidden in ["chat", "user", "conversation", "token", "path"] {
            assert!(
                !rendered.contains(forbidden),
                "found {forbidden:?} in {rendered}"
            );
        }
    }
}
