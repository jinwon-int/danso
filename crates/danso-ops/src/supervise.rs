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
