//! CLI-level `danso service install|reconcile|uninstall` contract checks
//! (#118, 4-a PR3).
//!
//! The behaviours that matter here are the ones about *not* acting: a dry run
//! that writes nothing, a reconcile that repairs nothing, an uninstall that
//! refuses while something is still serving. A renderer that quietly installs
//! is worse than one that fails.

use std::path::Path;

fn service(home: &Path, data_dir: &Path, args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_danso"));
    command
        .arg("service")
        .args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .env("HOME", home)
        // The system scope would write to /etc; every test uses --user, whose
        // directory is derived from HOME and therefore from the sandbox.
        .env_remove("DANSO_TELEGRAM_DATA_DIR");
    command.output().expect("run danso service")
}

fn user_unit(home: &Path) -> std::path::PathBuf {
    home.join(".config/systemd/user/danso.service")
}

#[test]
fn a_dry_run_prints_the_unit_and_writes_nothing() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    let output = service(home, &data_dir, &["install", "--user", "--dry-run"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");

    assert!(
        stdout.starts_with("[Unit]\n"),
        "the unit must be the whole of stdout so `--dry-run > danso.service` \
         produces a usable file; got:\n{stdout}"
    );
    assert!(stdout.ends_with("WantedBy=default.target\n"));
    assert!(
        stdout.contains("TimeoutStopSec=70"),
        "the unit must carry the outer stop budget; got:\n{stdout}"
    );
    assert!(stdout.contains("WantedBy=default.target"), "--user scope");
    assert!(
        !user_unit(home).exists(),
        "a dry run must not create the unit file"
    );
    assert!(
        !home.join(".config/systemd").exists(),
        "a dry run must not create the unit directory either"
    );
}

#[test]
fn the_dry_run_unit_names_the_running_binary_not_a_path_lookup() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stdout =
        String::from_utf8(service(home, &data_dir, &["install", "--user", "--dry-run"]).stdout)
            .expect("UTF-8");
    assert!(
        stdout.contains(&format!("ExecStart={}", env!("CARGO_BIN_EXE_danso"))),
        "the unit must point at the image that installed it; got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("--data-dir {}", data_dir.display())),
        "the unit must pin the state root it was installed for"
    );
}

#[test]
fn reconcile_reports_absent_then_drift_and_repairs_neither() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    let output = service(home, &data_dir, &["reconcile", "--user"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "no unit installed is exit 2, worse than drift"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("not installed"),
        "absence must be named, not implied"
    );

    // Write a unit that differs from what this binary renders.
    let rendered =
        String::from_utf8(service(home, &data_dir, &["install", "--user", "--dry-run"]).stdout)
            .expect("UTF-8");
    let edited = rendered.replace("TimeoutStopSec=70", "TimeoutStopSec=10");
    std::fs::create_dir_all(user_unit(home).parent().unwrap()).unwrap();
    std::fs::write(user_unit(home), &edited).unwrap();

    let output = service(home, &data_dir, &["reconcile", "--user"]);
    assert_eq!(output.status.code(), Some(1), "drift is exit 1");
    assert_eq!(
        std::fs::read_to_string(user_unit(home)).unwrap(),
        edited,
        "reconcile reports drift; rewriting a unit an operator edited is not reconciliation"
    );
}

#[test]
fn reconcile_reports_in_sync_for_an_unmodified_unit() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    let rendered =
        String::from_utf8(service(home, &data_dir, &["install", "--user", "--dry-run"]).stdout)
            .expect("UTF-8");
    std::fs::create_dir_all(user_unit(home).parent().unwrap()).unwrap();
    // Byte-for-byte: stdout is exactly the unit, with no header and no extra
    // newline, so an operator can redirect it straight into place.
    std::fs::write(user_unit(home), &rendered).unwrap();

    let output = service(home, &data_dir, &["reconcile", "--user"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "an untouched unit must not report drift; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn uninstall_refuses_while_the_service_is_still_serving() {
    use std::os::unix::io::AsRawFd;
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Hold the token lock: `status` reports degraded, i.e. something is serving
    // even though it is not bookkept.
    let lock = std::fs::File::create(data_dir.join(".telegram-token.lock")).unwrap();
    // SAFETY: the descriptor is owned by `lock` for the whole test.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    std::fs::create_dir_all(user_unit(home).parent().unwrap()).unwrap();
    std::fs::write(user_unit(home), "placeholder").unwrap();

    let output = service(home, &data_dir, &["uninstall", "--user"]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "removing the unit under a live process leaves it unsupervised and undescribed"
    );
    assert!(
        user_unit(home).exists(),
        "a refused uninstall must leave the unit in place"
    );
}

#[test]
fn uninstall_removes_the_unit_when_nothing_is_serving() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(user_unit(home).parent().unwrap()).unwrap();
    std::fs::write(user_unit(home), "placeholder").unwrap();

    let output = service(home, &data_dir, &["uninstall", "--user"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!user_unit(home).exists());
}

#[test]
fn uninstalling_nothing_is_not_a_failure() {
    let root = tempfile::tempdir().expect("temporary home");
    let home = root.path();
    let data_dir = home.join("state");
    std::fs::create_dir_all(&data_dir).unwrap();

    let output = service(home, &data_dir, &["uninstall", "--user"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not installed"));
}
