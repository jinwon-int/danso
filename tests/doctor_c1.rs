//! CLI-level C1 doctor contract checks: one JSON report, fail exit, and the
//! explicit exit-2 boundary when the installation home cannot be resolved.

use serde_json::Value;
use std::fs;

#[test]
fn doctor_prints_one_report_on_a_failed_check() {
    let root = tempfile::tempdir().expect("temporary doctor root");
    let home = root.path().join("danso-home");
    fs::create_dir(&home).expect("home");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .arg("doctor")
        .env("DANSO_HOME", &home)
        .env("HOME", root.path())
        .env_remove("DANSO_TELEGRAM_DATA_DIR")
        .env_remove("DANSO_MEMORY_DIR")
        .output()
        .expect("run doctor");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 JSON");
    assert_eq!(stdout.lines().count(), 1, "doctor emits one JSON object");
    let report: Value = serde_json::from_str(stdout.trim()).expect("doctor report");
    assert_eq!(report["version"], 1);
    assert_eq!(report["summary"]["fail"], 1);
    assert_eq!(
        report["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|check| check["id"] == "config.parse")
            .expect("config check")["detail"],
        "config file missing"
    );
}

#[test]
fn doctor_exit_two_has_no_partial_json_when_home_cannot_resolve() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .arg("doctor")
        .env_remove("DANSO_HOME")
        .env_remove("HOME")
        .env_remove("DANSO_TELEGRAM_DATA_DIR")
        .output()
        .expect("run doctor");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

/// A node that set `DANSO_HOME` and left its Telegram state and memory under
/// `$HOME/.danso` (#136): the report names both paths and the move, moves
/// nothing, and stays a warning rather than a failure.
#[test]
fn doctor_names_legacy_state_left_under_the_old_home() {
    let root = tempfile::tempdir().expect("temporary doctor root");
    let home = root.path().join("danso-home");
    fs::create_dir(&home).expect("home");
    fs::write(home.join("config.toml"), "").expect("config");
    let old = root.path().join(".danso");
    fs::create_dir_all(old.join("telegram")).expect("old telegram");
    fs::write(old.join("telegram/health.json"), "{}").expect("old health");
    fs::create_dir_all(old.join("memory/global")).expect("old memory");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .arg("doctor")
        .env("DANSO_HOME", &home)
        .env("HOME", root.path())
        .env_remove("DANSO_TELEGRAM_DATA_DIR")
        .env_remove("DANSO_MEMORY_DIR")
        .output()
        .expect("run doctor");

    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor report");
    let check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["id"] == "home.legacy_state")
        .expect("legacy state check")
        .clone();
    assert_eq!(check["status"], "warn");
    assert_eq!(
        check["detail"],
        format!(
            "legacy state present; move it: mv {} {}; mv {} {} while the service is stopped",
            old.join("telegram").display(),
            home.join("telegram").display(),
            old.join("memory").display(),
            home.join("memory").display()
        )
    );
    assert!(old.join("telegram/health.json").exists(), "nothing moved");
    assert!(!home.join("telegram").exists(), "nothing created");

    // Pointing the dedicated variable at the old tree is a choice, not a split.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .arg("doctor")
        .env("DANSO_HOME", &home)
        .env("HOME", root.path())
        .env("DANSO_TELEGRAM_DATA_DIR", old.join("telegram"))
        .env("DANSO_MEMORY_DIR", old.join("memory"))
        .output()
        .expect("run doctor");
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor report");
    let check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["id"] == "home.legacy_state")
        .expect("legacy state check")
        .clone();
    assert_eq!(check["detail"], "legacy state absent");
}
