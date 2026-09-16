//! CLI-level `danso update status` contract checks (#119, 4-b PR1).
//!
//! The property under test is that an outstanding activation is *visible* and
//! *non-zero*. ccc-node #1527 was an activation that failed and left nothing an
//! operator or a later run could act on; the whole point of this record is that
//! the gap between "binary replaced" and "replacement proven to serve" has a
//! name and an exit code.

use serde_json::Value;

fn update(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .arg("update")
        .args(args)
        .env("DANSO_HOME", home)
        .env_remove("HOME")
        .output()
        .expect("run danso update")
}

fn write_pending(home: &std::path::Path, target_sha: &str) {
    let state = home.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let record = serde_json::json!({
        "schema": "danso.self-update.activation.v1",
        "target": {"version": "0.2.0", "binary_sha256": target_sha},
        "previous": {"version": "0.1.0", "binary_sha256": "a".repeat(64)},
        "started_at": "2026-09-16T00:00:00Z",
        "updated_at": "2026-09-16T00:00:00Z",
        "outcome": "pending",
        "services": ["danso"],
    });
    std::fs::write(
        state.join("pending-activation.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .expect("write pending");
}

#[test]
fn a_fresh_home_reports_nothing_outstanding_and_exits_zero() {
    let home = tempfile::tempdir().expect("temporary home");
    let output = update(home.path(), &["status"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no activation outstanding"),
        "absence must be named, not implied"
    );
}

#[test]
fn an_unresolved_activation_exits_nonzero_so_something_must_finish_it() {
    let home = tempfile::tempdir().expect("temporary home");
    write_pending(home.path(), &"b".repeat(64));
    let output = update(home.path(), &["status"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a staged-but-unproven replacement is the #1527 state; exiting zero \
         would let a cron tick call it finished"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("previous generation still serving"),
        "the report must say which generation is actually serving; got:\n{stdout}"
    );
}

#[test]
fn an_activation_whose_target_is_running_is_still_unconfirmed() {
    let home = tempfile::tempdir().expect("temporary home");
    // Name this very binary as the target: it IS running, yet nothing has
    // marked the activation done.
    let running = String::from_utf8(
        std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
            .arg("update")
            .arg("status")
            .arg("--json")
            .env("DANSO_HOME", home.path())
            .output()
            .expect("probe")
            .stdout,
    )
    .expect("UTF-8");
    let probe: Value = serde_json::from_str(running.trim()).expect("json");
    let sha = probe["running_binary_sha256"]
        .as_str()
        .expect("the test binary is readable")
        .to_string();

    write_pending(home.path(), &sha);
    let output = update(home.path(), &["status", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "running is not the same as confirmed — only an explicit activation \
         may resolve the record"
    );
    let report: Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("json");
    assert_eq!(report["pending_target_is_running"], true);
    assert_eq!(report["pending"]["outcome"], "pending");
}

#[test]
fn the_json_report_carries_the_documented_shape() {
    let home = tempfile::tempdir().expect("temporary home");
    write_pending(home.path(), &"b".repeat(64));
    let output = update(home.path(), &["status", "--json"]);
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 JSON");
    assert_eq!(stdout.lines().count(), 1, "status emits one JSON object");
    let report: Value = serde_json::from_str(stdout.trim()).expect("status report");
    assert_eq!(
        report["pending"]["schema"],
        "danso.self-update.activation.v1"
    );
    assert_eq!(report["pending_target_is_running"], false);
    assert_eq!(report["rollback_available"], false);
}

#[test]
fn a_corrupt_pending_record_fails_rather_than_reading_as_absent() {
    let home = tempfile::tempdir().expect("temporary home");
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(state.join("pending-activation.json"), b"{ not json").expect("write");
    let output = update(home.path(), &["status"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "reporting a corrupt record as 'nothing outstanding' would discard the \
         only evidence that a replacement happened"
    );
    assert!(
        output.stdout.is_empty(),
        "a failed read must not print a status"
    );
}

#[test]
fn status_creates_no_state_in_the_home_it_inspects() {
    let home = tempfile::tempdir().expect("temporary home");
    update(home.path(), &["status"]);
    let entries: Vec<_> = std::fs::read_dir(home.path())
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "inspection must not create state; found {entries:?}"
    );
}

#[test]
fn rollback_availability_follows_the_retained_binary() {
    let home = tempfile::tempdir().expect("temporary home");
    let previous = home.path().join("bin/danso.prev");
    std::fs::create_dir_all(previous.parent().unwrap()).expect("bin dir");
    std::fs::write(&previous, b"#!/bin/sh\n").expect("write previous");
    let output = update(home.path(), &["status", "--json"]);
    let report: Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("json");
    assert_eq!(report["rollback_available"], true);
}
