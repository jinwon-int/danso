//! Crash policy for `danso service run --supervise` (`docs/unified-design.md` §6.4).
//!
//! Termux has no systemd, so `Restart=always` has nothing to implement it. This
//! is the replacement, and it is deliberately not "restart forever": a service
//! that dies immediately on every start would otherwise spin, burning battery
//! and filling logs while looking supervised. Five rapid crashes inside a
//! minute stop the loop and say why.

use std::time::{Duration, Instant};

/// How many crashes inside [`RAPID_WINDOW`] end supervision.
pub const RAPID_CRASH_LIMIT: usize = 5;

/// The window rapid crashes are counted in.
pub const RAPID_WINDOW: Duration = Duration::from_secs(60);

/// The exit status used when supervision gives up.
///
/// 75 is `EX_TEMPFAIL`: the supervisor stopped, but the condition may be
/// transient and a later start is legitimate. It is distinct from the service's
/// own failure codes so an operator can tell "the service failed" from
/// "supervision refused to keep restarting it".
pub const CRASH_LOOP_EXIT: i32 = 75;

/// The `health.json` reason published when supervision gives up.
pub const CRASH_LOOP_REASON: &str = "crash-loop";

/// Signals that mean an operator took the service down.
///
/// SIGTERM and SIGINT are how a stop reaches a process group; SIGHUP is how a
/// closing session does. Everything else — SIGKILL, SIGSEGV, SIGABRT, and the
/// OOM killer's SIGKILL above all — is the service dying, which is the whole
/// reason supervision exists.
pub const OPERATOR_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Whether an exit was the service stopping rather than dying.
///
/// The original rule was "no exit code means a signal, and a signal means the
/// operator", which is true of `SIGTERM` to the process group and false of
/// every other way a process is signalled. Under it a `SIGKILL` — including the
/// OOM killer's, the most likely death on the one platform where `--supervise`
/// *is* the restart policy — read as a clean stop and supervision quietly
/// ended. Measured on yukson 2026-09-17 (#118): `kill -9` on the child left no
/// process, no restart and an empty log.
///
/// `stopping` is the other half. `service stop` escalates to `SIGKILL` when the
/// grace budget runs out, and ccc-node's contract is explicit that a restart
/// must not launch on top of a process whose teardown never ran. So an
/// operator-initiated stop says so out of band, and that marker — not the
/// signal number — is what makes a kill clean.
pub fn is_operator_stop(success: bool, signal: Option<i32>, stopping: bool) -> bool {
    if success || stopping {
        return true;
    }
    signal.is_some_and(|signal| OPERATOR_SIGNALS.contains(&signal))
}

/// What supervision decided after an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Start the service again.
    Restart,
    /// The service exited on purpose; supervision is finished.
    Stop,
    /// Too many crashes too quickly.
    CrashLoop,
}

/// Rapid-crash accounting.
///
/// Only crashes inside the window count. A service that runs for hours and then
/// dies is not in a crash loop, however many times it has happened, so old
/// entries are discarded rather than accumulated.
#[derive(Debug, Default)]
pub struct CrashPolicy {
    recent: Vec<Instant>,
}

impl CrashPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an exit and decide what to do next.
    ///
    /// `clean` means the service chose to exit — an orderly stop, not a crash.
    /// Restarting there would fight the operator who just stopped it.
    pub fn record(&mut self, clean: bool, now: Instant) -> Decision {
        if clean {
            return Decision::Stop;
        }
        self.recent
            .retain(|at| now.duration_since(*at) < RAPID_WINDOW);
        self.recent.push(now);
        if self.recent.len() >= RAPID_CRASH_LIMIT {
            Decision::CrashLoop
        } else {
            Decision::Restart
        }
    }

    /// Crashes currently inside the window.
    pub fn rapid_crashes(&self) -> usize {
        self.recent.len()
    }
}

#[cfg(test)]
mod operator_stop_tests {
    use super::*;

    #[test]
    fn a_clean_exit_is_a_stop() {
        assert!(is_operator_stop(true, None, false));
    }

    #[test]
    fn the_signals_an_operator_sends_are_a_stop() {
        for signal in OPERATOR_SIGNALS {
            assert!(
                is_operator_stop(false, Some(signal), false),
                "signal {signal} reaches the process group when an operator \
                 stops a supervised service"
            );
        }
    }

    #[test]
    fn a_kill_is_a_crash_unless_a_stop_is_under_way() {
        // The OOM killer's signal, on the one platform where supervision is
        // the restart policy. Reading it as a stop is how a service dies and
        // nothing brings it back.
        assert!(!is_operator_stop(false, Some(libc::SIGKILL), false));
        // But `service stop` escalates to SIGKILL when the budget runs out,
        // and restarting on top of a teardown that never ran is worse.
        assert!(is_operator_stop(false, Some(libc::SIGKILL), true));
    }

    #[test]
    fn the_other_ways_a_service_dies_are_crashes() {
        for signal in [libc::SIGSEGV, libc::SIGABRT, libc::SIGBUS, libc::SIGFPE] {
            assert!(
                !is_operator_stop(false, Some(signal), false),
                "signal {signal} is the service dying"
            );
        }
    }

    #[test]
    fn a_non_zero_exit_is_still_a_crash() {
        assert!(!is_operator_stop(false, None, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_exit_ends_supervision() {
        let mut policy = CrashPolicy::new();
        assert_eq!(policy.record(true, Instant::now()), Decision::Stop);
        assert_eq!(
            policy.rapid_crashes(),
            0,
            "an orderly stop is not a crash and must not count toward the limit"
        );
    }

    #[test]
    fn four_rapid_crashes_still_restart_and_the_fifth_stops() {
        let mut policy = CrashPolicy::new();
        let now = Instant::now();
        for attempt in 1..RAPID_CRASH_LIMIT {
            assert_eq!(
                policy.record(false, now + Duration::from_secs(attempt as u64)),
                Decision::Restart,
                "crash {attempt} is inside the allowance"
            );
        }
        assert_eq!(
            policy.record(false, now + Duration::from_secs(RAPID_CRASH_LIMIT as u64)),
            Decision::CrashLoop
        );
    }

    #[test]
    fn crashes_spread_beyond_the_window_never_trip_the_limit() {
        let mut policy = CrashPolicy::new();
        let mut now = Instant::now();
        for _ in 0..20 {
            now += RAPID_WINDOW + Duration::from_secs(1);
            assert_eq!(
                policy.record(false, now),
                Decision::Restart,
                "a service that runs a full window between crashes is not looping"
            );
            assert_eq!(policy.rapid_crashes(), 1, "stale crashes must be discarded");
        }
    }

    #[test]
    fn a_crash_exactly_one_window_old_is_dropped_but_a_younger_one_is_kept() {
        // The boundary is exclusive on the old side: `now - at < RAPID_WINDOW`.
        // Stating it with two entries rather than a batch keeps the assertion
        // about the boundary itself instead of about the fixture's spacing.
        let start = Instant::now();
        let mut policy = CrashPolicy::new();
        policy.record(false, start);
        assert_eq!(policy.rapid_crashes(), 1);
        // Exactly one window later, the first entry is no longer inside it.
        policy.record(false, start + RAPID_WINDOW);
        assert_eq!(
            policy.rapid_crashes(),
            1,
            "a crash exactly one window old must age out"
        );

        let mut policy = CrashPolicy::new();
        policy.record(false, start);
        // One millisecond short of a window: still inside, so it accumulates.
        policy.record(false, start + RAPID_WINDOW - Duration::from_millis(1));
        assert_eq!(
            policy.rapid_crashes(),
            2,
            "a crash just inside the window must still count"
        );
    }

    #[test]
    fn the_crash_loop_exit_is_distinct_from_the_services_own_codes() {
        // `run` exits 1 on failure and `stop` uses 0/1/2; 75 must not collide.
        assert_eq!(CRASH_LOOP_EXIT, 75);
        assert!(!(0..=3).contains(&CRASH_LOOP_EXIT));
    }
}
