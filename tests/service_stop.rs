//! CLI-level `danso service stop` contract checks (#118, 4-a PR2).
//!
//! The behaviours worth testing at this level are the ones a unit test cannot
//! see: that an out-of-range budget is rejected *before* anything is signalled,
//! and that the exit code a supervisor branches on is the one we intend.

fn stop(args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_danso"));
    command.arg("service").arg("stop");
    for argument in args {
        command.arg(argument);
    }
    command.output().expect("run service stop")
}

#[test]
fn stopping_an_empty_state_root_is_not_running_and_exits_zero() {
    let root = tempfile::tempdir().expect("temporary state root");
    let output = stop(&["--data-dir", root.path().to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stopping something already down is success, not failure"
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 text");
    assert_eq!(stdout.lines().next(), Some("Bot stop: not running"));
}

#[test]
fn an_out_of_range_budget_is_rejected_before_anything_is_signalled() {
    let root = tempfile::tempdir().expect("temporary state root");
    // A pid record naming this very test process. If validation ran after
    // signalling, the test would receive SIGTERM and die instead of asserting.
    let argv: Vec<String> = std::fs::read(format!("/proc/{}/cmdline", std::process::id()))
        .expect("read argv")
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    let record = serde_json::json!({
        "pid": std::process::id(),
        "started_at": "2026-09-16T00:00:00Z",
        "boot_id": std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("boot id").trim(),
        "argv_hash": danso_ops::pidfile::argv_hash(&argv),
    });
    std::fs::write(
        root.path().join("service.pid"),
        serde_json::to_vec(&record).unwrap(),
    )
    .expect("write pid record");

    for rejected in ["0", "3601", "abc", "-1"] {
        let output = stop(&[
            "--data-dir",
            root.path().to_str().unwrap(),
            "--grace-secs",
            rejected,
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "budget {rejected} must be rejected with exit 2"
        );
        assert!(
            output.stdout.is_empty(),
            "a rejected budget must not print a stop outcome"
        );
    }
    // Still alive: nothing was signalled.
    assert!(danso_ops::probe::pid_alive(std::process::id()));
}

#[test]
fn the_documented_budget_bounds_are_accepted() {
    let root = tempfile::tempdir().expect("temporary state root");
    for accepted in ["1", "70", "3600"] {
        let output = stop(&[
            "--data-dir",
            root.path().to_str().unwrap(),
            "--grace-secs",
            accepted,
        ]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "budget {accepted} is inside the documented range"
        );
    }
}

#[test]
fn the_default_budget_is_the_outer_seventy_second_tier() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["service", "stop", "--help"])
        .output()
        .expect("run help");
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(
        help.contains("70"),
        "the default must be the outer 70s budget matching TimeoutStopSec; got:\n{help}"
    );
    assert_eq!(
        danso_ops::stop::DEFAULT_GRACE_SECS,
        70,
        "the outer budget matches ccc CCC_BRIDGE_STOP_GRACE_SECONDS"
    );
    assert_eq!(
        danso_ops::stop::TURN_DRAIN_SECS,
        45,
        "the inner turn drain stays at the application window"
    );
}

#[test]
fn a_stale_pid_record_does_not_cause_an_unrelated_process_to_be_signalled() {
    let root = tempfile::tempdir().expect("temporary state root");
    // A record from another boot naming a live pid: the pid is real, but the
    // record is not evidence it is ours.
    let record = serde_json::json!({
        "pid": std::process::id(),
        "started_at": "2026-09-16T00:00:00Z",
        "boot_id": "00000000-0000-0000-0000-000000000000",
        "argv_hash": danso_ops::pidfile::argv_hash(&["danso".to_string()]),
    });
    std::fs::write(
        root.path().join("service.pid"),
        serde_json::to_vec(&record).unwrap(),
    )
    .expect("write pid record");

    let output = stop(&["--data-dir", root.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 text");
    assert_eq!(stdout.lines().next(), Some("Bot stop: not running"));
    assert!(
        danso_ops::probe::pid_alive(std::process::id()),
        "a record from another boot must never be used to signal a live pid"
    );
}
