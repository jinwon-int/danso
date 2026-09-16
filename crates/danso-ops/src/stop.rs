//! Bounded stop (`danso service stop`), preserving ccc-node's `do_stop` contract.
//!
//! The budget is **two tiers** and they are not the same quantity
//! (`ccc-node/bridge/start.sh` L603-604):
//!
//! > One wall-clock grace budget for all stop targets: the Python bridge drains
//! > for 45s, then tears down. Match systemd's 70s allowance, not the old 10s kill.
//!
//! The inner 45s is the application's turn drain; the outer 70s is the whole
//! stop's wall clock and matches the unit's `TimeoutStopSec`. The difference is
//! the teardown allowance. Raising the inner budget to the outer one leaves
//! nothing for tidying status messages, releasing the token lock and removing
//! the pid file — the process gets SIGKILLed mid-teardown and its files survive
//! to block the next start. (Operator decision 2026-09-16, #118.)

use crate::probe;
use std::time::{Duration, Instant};

/// The inner budget: how long an active turn may keep running after SIGTERM.
pub const TURN_DRAIN_SECS: u64 = 45;

/// The outer budget: the whole stop's wall clock, matching ccc-node's
/// `CCC_BRIDGE_STOP_GRACE_SECONDS` default and the rendered `TimeoutStopSec`.
pub const DEFAULT_GRACE_SECS: u64 = 70;

/// ccc-node accepts 1..=3600 and rejects anything else *before signalling*.
pub const MIN_GRACE_SECS: u64 = 1;
pub const MAX_GRACE_SECS: u64 = 3600;

/// After the deadline the remaining target is killed; ccc then observes exit
/// for at most two more seconds rather than reporting an outcome it has not
/// seen.
pub const KILL_OBSERVE: Duration = Duration::from_secs(2);

// The two tiers must stay distinct, and the inner one must leave wall clock for
// teardown. Enforced at compile time rather than in a test: a build where these
// have been collapsed should not exist at all, not merely fail its tests.
const _: () = assert!(
    TURN_DRAIN_SECS < DEFAULT_GRACE_SECS,
    "the inner turn drain must leave wall clock for teardown inside the outer budget"
);
const _: () = assert!(
    DEFAULT_GRACE_SECS >= MIN_GRACE_SECS && DEFAULT_GRACE_SECS <= MAX_GRACE_SECS,
    "the default budget must itself be accepted by the validator"
);

/// Validate an outer stop budget.
///
/// Callers must run this before sending any signal: a rejected budget must
/// leave the service exactly as it was, not half-stopped. ccc-node makes the
/// same ordering explicit ("Validate it before signalling any process").
pub fn parse_grace_secs(raw: &str) -> Result<u64, String> {
    let value: u64 = raw.parse().map_err(|_| {
        format!("grace must be an integer from {MIN_GRACE_SECS} to {MAX_GRACE_SECS}")
    })?;
    if !(MIN_GRACE_SECS..=MAX_GRACE_SECS).contains(&value) {
        return Err(format!(
            "grace must be an integer from {MIN_GRACE_SECS} to {MAX_GRACE_SECS}"
        ));
    }
    Ok(value)
}

/// What a stop attempt concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Nothing was running; there was nothing to stop.
    NotRunning,
    /// The target exited within the budget.
    Drained,
    /// The budget elapsed and the target was killed.
    ///
    /// ccc-node keeps pid and lock bookkeeping in this case and exits non-zero:
    /// a kill is a failed stop, and a restart must not launch on top of a
    /// process whose teardown never ran.
    Killed,
    /// The target survived even SIGKILL, or its exit could not be observed.
    Survived,
}

impl StopOutcome {
    /// `0` only when the service actually drained or was already down.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::NotRunning | Self::Drained => 0,
            Self::Killed => 1,
            Self::Survived => 2,
        }
    }

    /// Whether pid and lock bookkeeping must be retained rather than cleared.
    pub fn retains_bookkeeping(self) -> bool {
        matches!(self, Self::Killed | Self::Survived)
    }
}

fn signal(pid: u32, signal: i32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with a validated positive pid targets exactly that process.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Send SIGTERM and wait for the target to exit, then SIGKILL at the deadline.
///
/// One TERM only: ccc-node's driver does not repeat the signal, because a
/// second TERM means *force* to the drain handler and would cut the drain it
/// is waiting for.
pub fn terminate_and_wait(pid: u32, grace: Duration) -> StopOutcome {
    terminate_and_wait_with(pid, grace, || std::thread::sleep(POLL_INTERVAL))
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// [`terminate_and_wait`] with an injectable wait, so tests do not sleep in
/// real time.
pub fn terminate_and_wait_with(pid: u32, grace: Duration, tick: impl FnMut()) -> StopOutcome {
    decide(pid, grace, tick, probe::pid_alive, signal)
}

/// The stop decision with its two effects — liveness and signalling — supplied
/// by the caller.
///
/// They are parameters because the outcomes that matter most cannot be produced
/// safely with a real process. `Survived` needs a target that outlives SIGKILL
/// (an uninterruptible sleep, or one we may not signal); the only real
/// candidates on a host are processes a test must never touch, such as pid 1.
/// Driving the decision directly tests it without that hazard.
fn decide(
    pid: u32,
    grace: Duration,
    mut tick: impl FnMut(),
    alive: impl Fn(u32) -> bool,
    send: impl Fn(u32, i32) -> bool,
) -> StopOutcome {
    if !alive(pid) {
        return StopOutcome::NotRunning;
    }
    if !send(pid, libc::SIGTERM) && !alive(pid) {
        return StopOutcome::NotRunning;
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !alive(pid) {
            return StopOutcome::Drained;
        }
        tick();
    }
    if !alive(pid) {
        return StopOutcome::Drained;
    }
    send(pid, libc::SIGKILL);
    // Observe the exit rather than assuming it. A reported kill that did not
    // happen is what lets a restart launch on top of a live consumer, so the
    // absence of evidence is reported as `Survived`, not as success.
    let observe_until = Instant::now() + KILL_OBSERVE;
    while Instant::now() < observe_until {
        if !alive(pid) {
            return StopOutcome::Killed;
        }
        tick();
    }
    if alive(pid) {
        StopOutcome::Survived
    } else {
        StopOutcome::Killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_budgets_match_the_ccc_node_contract() {
        // The ordering invariant is a compile-time assertion above; these pin
        // the actual values to the contract they came from.
        assert_eq!(
            TURN_DRAIN_SECS, 45,
            "the inner drain is ccc's 45s application window"
        );
        assert_eq!(
            DEFAULT_GRACE_SECS, 70,
            "the outer budget is ccc's CCC_BRIDGE_STOP_GRACE_SECONDS default \
             and the rendered TimeoutStopSec"
        );
        assert_eq!(
            DEFAULT_GRACE_SECS - TURN_DRAIN_SECS,
            25,
            "the teardown allowance is what a collapsed budget would destroy"
        );
    }

    #[test]
    fn grace_accepts_the_documented_range_only() {
        assert_eq!(parse_grace_secs("1"), Ok(1));
        assert_eq!(parse_grace_secs("70"), Ok(70));
        assert_eq!(parse_grace_secs("3600"), Ok(3600));
        for rejected in ["0", "3601", "-1", "", "abc", "70.5", "07 "] {
            assert!(
                parse_grace_secs(rejected).is_err(),
                "{rejected} must be rejected"
            );
        }
    }

    /// How a scripted target responds to signals.
    ///
    /// Liveness is driven by which signal has been delivered, not by how many
    /// times it was probed: the probe count depends on how fast the busy-wait
    /// spins inside the budget, which is a property of the machine rather than
    /// of the contract under test.
    #[derive(Clone, Copy)]
    enum Target {
        AlreadyDead,
        HonoursTerm,
        IgnoresTermDiesOnKill,
        Immortal,
    }

    fn scripted(grace: Duration, target: Target) -> (StopOutcome, Vec<i32>) {
        let sent = std::cell::RefCell::new(Vec::<i32>::new());
        let outcome = decide(
            4242,
            grace,
            || {},
            |_| {
                let delivered = sent.borrow();
                match target {
                    Target::AlreadyDead => false,
                    Target::HonoursTerm => !delivered.contains(&libc::SIGTERM),
                    Target::IgnoresTermDiesOnKill => !delivered.contains(&libc::SIGKILL),
                    Target::Immortal => true,
                }
            },
            |_, signal| {
                sent.borrow_mut().push(signal);
                true
            },
        );
        (outcome, sent.into_inner())
    }

    #[test]
    fn a_target_that_honours_sigterm_is_never_killed() {
        let (outcome, sent) = scripted(Duration::from_secs(30), Target::HonoursTerm);
        assert_eq!(outcome, StopOutcome::Drained);
        assert_eq!(
            sent,
            vec![libc::SIGTERM],
            "a target that drained must never be sent SIGKILL"
        );
    }

    #[test]
    fn a_target_that_outlives_sigkill_is_reported_survived_not_killed() {
        let (outcome, sent) = scripted(Duration::from_millis(1), Target::Immortal);
        assert_eq!(
            outcome,
            StopOutcome::Survived,
            "a kill whose effect was never observed must not be reported as success"
        );
        assert_eq!(
            outcome.exit_code(),
            2,
            "an unobserved kill is the worst outcome, not a partial success"
        );
        assert!(outcome.retains_bookkeeping());
        assert_eq!(sent, vec![libc::SIGTERM, libc::SIGKILL]);
    }

    #[test]
    fn a_target_that_dies_to_sigkill_is_reported_killed() {
        // Alive for the initial check and the budget, then gone once SIGKILL
        // has landed: the observation window is what distinguishes this from
        // `Survived`.
        let (outcome, sent) = scripted(Duration::from_millis(1), Target::IgnoresTermDiesOnKill);
        assert_eq!(outcome, StopOutcome::Killed);
        assert_eq!(sent, vec![libc::SIGTERM, libc::SIGKILL]);
    }

    #[test]
    fn only_one_term_is_ever_sent() {
        // A second TERM means *force* to ccc's drain handler and would cut the
        // drain the driver is waiting for.
        let (_, sent) = scripted(Duration::from_millis(1), Target::Immortal);
        assert_eq!(
            sent.iter()
                .filter(|signal| **signal == libc::SIGTERM)
                .count(),
            1
        );
    }

    #[test]
    fn a_dead_target_is_never_signalled_at_all() {
        let (outcome, sent) = scripted(Duration::from_secs(1), Target::AlreadyDead);
        assert_eq!(outcome, StopOutcome::NotRunning);
        assert!(
            sent.is_empty(),
            "nothing may be signalled when there is nothing to stop"
        );
    }

    #[test]
    fn a_dead_target_is_not_running() {
        assert_eq!(
            terminate_and_wait_with(u32::MAX - 1, Duration::from_secs(1), || {}),
            StopOutcome::NotRunning
        );
    }

    #[test]
    fn a_cooperating_target_drains_and_exits_zero() {
        // Default SIGTERM disposition terminates, so this child stands in for a
        // service that honours the signal.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn cooperating child");
        let outcome = terminate_and_wait(child.id(), Duration::from_secs(10));
        assert_eq!(outcome, StopOutcome::Drained);
        assert_eq!(outcome.exit_code(), 0);
        assert!(!outcome.retains_bookkeeping());
        child.wait().expect("reap");
    }

    #[test]
    fn a_target_that_ignores_sigterm_is_killed_and_keeps_bookkeeping() {
        // `trap '' TERM` makes the child ignore SIGTERM, which is exactly the
        // wedged-drain case the budget exists for.
        //
        // The child announces that the trap is installed before it starts
        // waiting. Signalling straight after spawn would race the shell: the
        // default disposition still applies until `trap` runs, so the child
        // would sometimes die on the TERM and the test would report `Drained`
        // while claiming to have proved the kill path.
        let dir = tempfile::tempdir().expect("temporary fixture dir");
        let ready = dir.path().join("trap-installed");
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("trap '' TERM; : > '{}'; sleep 30", ready.display()))
            .spawn()
            .expect("spawn stubborn child");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "child never installed its TERM trap; the fixture cannot test this"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let outcome = terminate_and_wait(child.id(), Duration::from_millis(300));
        assert_eq!(
            outcome,
            StopOutcome::Killed,
            "a target that outlives the budget must be killed, not reported drained"
        );
        assert_eq!(
            outcome.exit_code(),
            1,
            "a kill is a failed stop and must not exit zero"
        );
        assert!(
            outcome.retains_bookkeeping(),
            "pid and lock records must survive a kill so a restart refuses to \
             launch over a process whose teardown never ran"
        );
        child.wait().expect("reap");
    }

    #[test]
    fn a_zombie_target_counts_as_already_stopped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe::process_state(pid) != probe::ProcState::Zombie {
            assert!(Instant::now() < deadline, "child never became a zombie");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Without the zombie exclusion this would wait out the whole budget and
        // then "kill" a process that exited long ago.
        assert_eq!(
            terminate_and_wait(pid, Duration::from_secs(30)),
            StopOutcome::NotRunning
        );
        child.wait().expect("reap");
    }
}
