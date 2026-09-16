//! CLI-level `danso service status` contract checks (#118, 4-a).
//!
//! These exercise the real binary, because the parts most likely to break are
//! the ones a unit test cannot see: the exit code the supervisor branches on,
//! and the first line the fleet watch greps for.

use serde_json::Value;

fn status(data_dir: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_danso"));
    command
        .arg("service")
        .arg("status")
        .arg("--data-dir")
        .arg(data_dir);
    for argument in extra {
        command.arg(argument);
    }
    command.output().expect("run service status")
}

#[test]
fn an_empty_state_root_is_unavailable_with_exit_two() {
    let root = tempfile::tempdir().expect("temporary state root");
    let output = status(root.path(), &[]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 text");
    assert_eq!(
        stdout.lines().next(),
        Some("Bot status: unavailable"),
        "the first line is the fleet-watch contract"
    );
}

#[test]
fn the_json_report_carries_the_documented_shape() {
    let root = tempfile::tempdir().expect("temporary state root");
    let output = status(root.path(), &["--json"]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 JSON");
    assert_eq!(stdout.lines().count(), 1, "status emits one JSON object");
    let report: Value = serde_json::from_str(stdout.trim()).expect("status report");
    assert_eq!(report["state"], "unavailable");
    assert_eq!(
        report["mutations"],
        Value::Object(serde_json::Map::new()),
        "a read reports an empty mutation set, not an absent one"
    );
}

#[test]
fn a_held_token_lock_without_bookkeeping_is_degraded_with_exit_one() {
    use std::os::unix::io::AsRawFd;
    let root = tempfile::tempdir().expect("temporary state root");
    let lock_path = root.path().join(".telegram-token.lock");
    let lock = std::fs::File::create(&lock_path).expect("create the token lock");
    // SAFETY: the descriptor is owned by `lock` for the whole test.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the fixture must start from a free lock"
    );

    let output = status(root.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a serving-but-unbookkept service is degraded, never down"
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 text");
    assert_eq!(stdout.lines().next(), Some("Bot status: degraded"));
}

#[test]
fn a_released_lock_file_reads_as_unavailable() {
    let root = tempfile::tempdir().expect("temporary state root");
    let lock_path = root.path().join(".telegram-token.lock");
    // The lock file is retained on purpose after a stop; only the kernel lock
    // signals ownership, so its mere presence must not read as running.
    std::fs::write(&lock_path, b"").expect("leave a released lock file");
    let output = status(root.path(), &[]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn unreadable_evidence_exits_three_and_never_renders_as_down() {
    let root = tempfile::tempdir().expect("temporary state root");
    // A directory where the pid file belongs: present, but not readable as a
    // record. ccc-node's `AVAIL=unverified` case.
    std::fs::create_dir(root.path().join("service.pid")).expect("occupy the pid path");
    let output = status(root.path(), &[]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "evidence that cannot be read is exit 3, not a false DOWN"
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 text");
    assert_eq!(stdout.lines().next(), Some("Bot status: unverified"));
    assert!(
        !stdout.contains("unavailable"),
        "an unverified read must not match a watch grepping for unavailable"
    );
}

#[test]
fn status_creates_no_state_in_the_directory_it_inspects() {
    let root = tempfile::tempdir().expect("temporary state root");
    let before: Vec<_> = std::fs::read_dir(root.path())
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(before.is_empty());
    status(root.path(), &[]);
    let after: Vec<_> = std::fs::read_dir(root.path())
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        after.is_empty(),
        "inspection must not create state; found {after:?}"
    );
}
