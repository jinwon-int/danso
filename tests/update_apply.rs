//! CLI-level `danso update apply` contract checks (#119, 4-b PR3).
//!
//! The unit tests in `danso_ops::install` cover the ordering invariant. What
//! can only be checked here is the part an operator and a cron wrapper
//! actually see: which key the process trusts, and what it exits with.
//!
//! The fixture release is signed by a throwaway key (`tests/fixtures/release`),
//! never by the real release key. That is deliberate — it means the default,
//! un-overridden path **must refuse it**, and a test that accidentally stops
//! checking the signature fails loudly instead of quietly installing.

use std::path::{Path, PathBuf};

const FIXTURE_KEY: &str = "RWRDmslv0TCmdcfE0s2lpt3hpMuvKCA0wKzLgDP7W81iToI0T/bZXQC7";
const OK: &str = "danso-9.9.9-ok.tar.gz";
/// `docs/unified-design.md` §6.3.
const EXIT_VERIFICATION_FAILED: i32 = 13;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release")
}

/// Copy the fixture so a test that tampers with it cannot affect another.
fn release_copy(into: &Path) -> PathBuf {
    let dir = into.join("release");
    std::fs::create_dir_all(&dir).expect("release dir");
    for entry in std::fs::read_dir(fixture_dir()).expect("read fixture") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("file type").is_file() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).expect("copy fixture");
        }
    }
    dir
}

fn trust_fixture_key(home: &Path) {
    std::fs::create_dir_all(home).expect("home");
    std::fs::write(
        home.join("config.toml"),
        format!("[update]\npublic_key = \"{FIXTURE_KEY}\"\n"),
    )
    .expect("config");
}

fn apply(home: &Path, release: &Path, artifact: &str) -> std::process::Output {
    apply_with(home, release, artifact, &[])
}

fn apply_with(home: &Path, release: &Path, artifact: &str, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["update", "apply", "--artifact-dir"])
        .arg(release)
        .args(["--artifact", artifact, "--json"])
        .args(extra)
        .env("DANSO_HOME", home)
        .env_remove("HOME")
        .output()
        .expect("run danso update apply")
}

/// Write only the fields the idle gate reads.
fn serving(data_dir: &Path, active: u64, oldest_seconds: u64) -> PathBuf {
    std::fs::create_dir_all(data_dir).expect("data dir");
    let document = serde_json::json!({
        "updated_at": chrono::Utc::now().to_rfc3339(),
        "workload": {
            "active_requests": active,
            "oldest_request_age_seconds": oldest_seconds,
        },
    });
    let path = data_dir.join("health.json");
    std::fs::write(&path, serde_json::to_vec(&document).expect("health")).expect("write health");
    path
}

fn code(output: &std::process::Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn a_release_the_embedded_key_did_not_sign_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    std::fs::create_dir_all(&home).expect("home");
    // No config.toml, so the trusted key is the one compiled into the binary.

    let output = apply(&home, &release, OK);
    assert_eq!(
        code(&output),
        EXIT_VERIFICATION_FAILED,
        "the fixture key is not the release key; installing anyway would mean \
         the embedded key is not being used"
    );
    assert!(
        !home.join("bin/danso").exists(),
        "nothing may be installed by a refused apply"
    );
    assert!(!home.join("state/pending-activation.json").exists());
}

#[test]
fn a_configured_key_is_what_makes_the_fixture_installable() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    trust_fixture_key(&home);

    let output = apply(&home, &release, OK);
    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json report on stdout");
    assert_eq!(report["version"], "9.9.9");
    assert_eq!(report["replaced"], true);
    assert!(home.join("bin/danso").exists());
    assert!(
        home.join("state/pending-activation.json").exists(),
        "an install that records no activation is one nobody can verify"
    );

    // The archive's own digest, not the binary's. `update check` compares
    // against this, and the two are never equal — a record without it makes
    // every later check report "cannot tell" forever.
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.join("state/installed-generation.json")).expect("record"),
    )
    .expect("json");
    let manifest = std::fs::read_to_string(release.join("SHA256SUMS")).expect("manifest");
    let signed = manifest
        .lines()
        .find(|line| line.ends_with(OK))
        .and_then(|line| line.split_whitespace().next())
        .expect("the manifest lists the artifact");
    assert_eq!(record["artifact_name"], OK);
    assert_eq!(
        record["artifact_sha256"], signed,
        "the recorded archive digest must be the one the signed manifest lists"
    );
}

#[test]
fn a_tampered_manifest_exits_13_even_with_the_signing_key_configured() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    trust_fixture_key(&home);

    let manifest = release.join("SHA256SUMS");
    let mut text = std::fs::read_to_string(&manifest).expect("manifest");
    text.push_str("0000000000000000000000000000000000000000000000000000000000000000  evil\n");
    std::fs::write(&manifest, text).expect("tamper");

    let output = apply(&home, &release, OK);
    assert_eq!(code(&output), EXIT_VERIFICATION_FAILED);
    assert!(!home.join("bin/danso").exists());
    // Body-free: the failure names a reason, not the manifest it rejected.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("verification_failed"), "{stderr}");
    assert!(!stderr.contains("evil"), "{stderr}");
}

#[test]
fn an_unusable_configured_key_is_an_error_not_a_fallback() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    std::fs::create_dir_all(&home).expect("home");
    // A bare ed25519 key: the shape `update.public_key` accepted before the
    // release key existed. Falling back to the embedded key here would make a
    // botched rotation look like a working install.
    std::fs::write(
        home.join("config.toml"),
        "[update]\npublic_key = \"uhnlFLDCRGn9SMAfkZQRDrHU0C7iYZm8P42pccxVwyo=\"\n",
    )
    .expect("config");

    let output = apply(&home, &release, OK);
    // Pin the code, not just "non-zero": a silent fall back to the embedded
    // key also exits non-zero (13, because the fixture key did not sign this
    // release), so `assert_ne!(code, 0)` passes for the very bug this test
    // exists to catch.
    assert_eq!(
        code(&output),
        2,
        "a broken override must fail as a configuration error, not fall back          to the embedded key; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.join("bin/danso").exists());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json on failure");
    assert_eq!(report["exit_code"], 2);
}

#[test]
fn a_failing_apply_prints_no_filesystem_path() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    trust_fixture_key(&home);

    // Correctly signed, but the archive has no `danso` member: the failure
    // comes from `tar`, whose own messages name the staged archive path.
    let output = apply(&home, &release, "danso-9.9.9-nomember.tar.gz");
    assert_eq!(code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.trim(),
        "update apply failed: apply_failed",
        "{stderr}"
    );
}

#[test]
fn applying_twice_reports_the_second_as_already_installed() {
    let temp = tempfile::tempdir().expect("temp");
    let home = temp.path().join("home");
    let release = release_copy(temp.path());
    trust_fixture_key(&home);

    assert_eq!(code(&apply(&home, &release, OK)), 0);
    let output = apply(&home, &release, OK);
    assert_eq!(code(&output), 0);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(report["replaced"], false);
}

/// The idle gate (`docs/unified-design.md` §6.3), at the level a cron wrapper
/// sees it. The decision logic is unit-tested in `danso_ops::idle`; what only
/// a process can show is that a deferral really installs nothing.
mod idle_gate {
    use super::*;

    /// ccc-node's exit 8: deferred, nothing changed.
    const EXIT_DEFERRED: i32 = 8;

    fn installed(home: &Path) -> bool {
        home.join("bin").join("danso").exists()
    }

    /// Every line of `state/self-update.log`, as JSON.
    fn log(home: &Path) -> Vec<serde_json::Value> {
        let path = home.join("state").join("self-update.log");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str(line).expect("log line is json"))
            .collect()
    }

    fn events(home: &Path) -> Vec<String> {
        log(home)
            .iter()
            .map(|line| line["event"].as_str().expect("event").to_string())
            .collect()
    }

    #[test]
    fn a_serving_service_defers_and_installs_nothing() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 1, 12);

        let output = apply_with(
            &home,
            &release,
            OK,
            &["--data-dir", data_dir.to_str().expect("utf-8")],
        );
        assert_eq!(code(&output), EXIT_DEFERRED);
        assert!(
            !installed(&home),
            "a deferral must not replace the binary it declined to replace"
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(report["gate"], "deferred");
        assert_eq!(report["reason"]["active"], 1);

        // An attempt that never started still leaves evidence. Without it a
        // node several generations behind cannot be told apart from one whose
        // updater never ran at all.
        let lines = log(&home);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["event"], "deferred");
        assert_eq!(lines[0]["active_requests"], 1);
        assert_eq!(lines[0]["waited_seconds"], 0);
        assert!(
            lines[0].get("target_sha256").is_none(),
            "nothing was read, so nothing may be named: {:?}",
            lines[0]
        );
    }

    #[test]
    fn proceeding_over_a_live_turn_says_so_and_is_logged() {
        // `--force` is the reachable case; the cap-exceeded and unrecordable
        // cases take the same branch and are unit-tested in `danso_ops::idle`.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 2, 9);

        let output = apply_with(
            &home,
            &release,
            OK,
            &["--data-dir", data_dir.to_str().expect("utf-8"), "--force"],
        );
        assert_eq!(code(&output), 0);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("while serving"),
            "ending a live turn on purpose must be said out loud: {stderr}"
        );
        // stdout stays the install report: a --json consumer's shape must not
        // change depending on what the gate decided.
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(report["replaced"], true);
        assert!(report.get("gate").is_none());

        let recorded = events(&home);
        assert_eq!(recorded, ["gate_forced", "applied"], "{recorded:?}");
    }

    #[test]
    fn an_ordinary_idle_install_does_not_clutter_the_log() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 0, 0);

        assert_eq!(
            code(&apply_with(
                &home,
                &release,
                OK,
                &["--data-dir", data_dir.to_str().expect("utf-8")]
            )),
            0
        );
        assert_eq!(
            events(&home),
            ["applied"],
            "the common case is the install line alone; a gate line on every \
             run would bury the ones that mean something"
        );
    }

    #[test]
    fn the_default_data_dir_is_what_the_service_env_names() {
        // Without `--data-dir` the gate resolves the same directory the
        // service uses. If that resolution fails the gate is inert and silent,
        // so the resolved case needs a test of its own.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 1, 7);

        let output = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
            .args(["update", "apply", "--artifact-dir"])
            .arg(&release)
            .args(["--artifact", OK, "--json"])
            .env("DANSO_HOME", &home)
            .env("DANSO_TELEGRAM_DATA_DIR", &data_dir)
            .env_remove("HOME")
            .output()
            .expect("run danso update apply");
        assert_eq!(
            code(&output),
            EXIT_DEFERRED,
            "the gate must find the service's health document without being \
             told where it is; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!installed(&home));
    }

    #[test]
    fn an_idle_service_installs() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 0, 0);

        let output = apply_with(
            &home,
            &release,
            OK,
            &["--data-dir", data_dir.to_str().expect("utf-8")],
        );
        assert_eq!(
            code(&output),
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(installed(&home));
    }

    #[test]
    fn force_installs_over_a_serving_turn() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 3, 12);

        let output = apply_with(
            &home,
            &release,
            OK,
            &["--data-dir", data_dir.to_str().expect("utf-8"), "--force"],
        );
        assert_eq!(
            code(&output),
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(installed(&home));
    }

    #[test]
    fn a_node_with_no_service_is_not_gated() {
        // A CLI-only installation publishes no health document. Requiring one
        // would make the gate a reason updates never run on exactly the hosts
        // that have no service to protect.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);

        let output = apply_with(
            &home,
            &release,
            OK,
            &[
                "--data-dir",
                temp.path().join("absent").to_str().expect("utf-8"),
            ],
        );
        assert_eq!(
            code(&output),
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(installed(&home));
    }

    #[test]
    fn a_deferral_does_not_wait_for_the_update_lock() {
        // The gate is ordered before `UpdateLock::acquire`, and this is what
        // that ordering is worth: a deferring run must not queue behind — or
        // block — a concurrent `apply`. Held from this process, the lock makes
        // the two orderings give different exit codes, which is the only way
        // to tell them apart from outside.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        serving(&data_dir, 1, 4);

        let state = home.join("state");
        std::fs::create_dir_all(&state).expect("state");
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.join("update.lock"))
            .expect("open the update lock");
        use std::os::unix::io::AsRawFd;
        // SAFETY: `lock` owns the descriptor for the whole call.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "the test must hold the lock for this to prove anything"
        );

        let output = apply_with(
            &home,
            &release,
            OK,
            &["--data-dir", data_dir.to_str().expect("utf-8")],
        );
        assert_eq!(
            code(&output),
            EXIT_DEFERRED,
            "a gate placed after the lock would fail on contention (exit 2) \
             instead of deferring; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(lock);
    }

    #[test]
    fn a_deferral_leaves_the_verification_path_untouched() {
        // The gate runs before the signature check, so a deferral must not be
        // reachable as a way to get a bad release past verification: once the
        // service goes idle the same artifact is still refused.
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        let release = release_copy(temp.path());
        std::fs::create_dir_all(&home).expect("home");
        // No config.toml: the embedded key did not sign the fixture.
        serving(&data_dir, 1, 12);
        let dir = data_dir.to_str().expect("utf-8");

        assert_eq!(
            code(&apply_with(&home, &release, OK, &["--data-dir", dir])),
            EXIT_DEFERRED
        );
        serving(&data_dir, 0, 0);
        assert_eq!(
            code(&apply_with(&home, &release, OK, &["--data-dir", dir])),
            EXIT_VERIFICATION_FAILED,
            "deferring must not become a way around the signature check"
        );
        assert!(!installed(&home));
    }
}

/// The full cycle an operator actually runs: install, resolve the activation,
/// then put the old binary back. Each step's exit code is the contract.
mod lifecycle {
    use super::*;

    fn update(home: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
            .arg("update")
            .args(args)
            .env("DANSO_HOME", home)
            .env_remove("HOME")
            .output()
            .expect("run danso update")
    }

    /// A CLI-only install has no service to ask, so `activate` judges it by the
    /// installed file — which is what every future invocation will run.
    #[test]
    fn install_then_activate_then_roll_back() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);

        // Something to roll back to.
        std::fs::create_dir_all(home.join("bin")).expect("bin");
        std::fs::write(home.join("bin/danso"), "#!/bin/sh\necho 'danso 0.0.1'\n").expect("seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join("bin/danso"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
        }

        assert_eq!(code(&apply(&home, &release, OK)), 0);

        // Before activation the record is outstanding, so `status` is non-zero.
        assert_eq!(
            code(&update(&home, &["status"])),
            1,
            "a replacement that has not been shown to serve is not done"
        );

        let activated = update(&home, &["activate", "--json"]);
        assert_eq!(
            code(&activated),
            0,
            "stderr: {}",
            String::from_utf8_lossy(&activated.stderr)
        );
        let report: serde_json::Value =
            serde_json::from_slice(&activated.stdout).expect("json activation");
        assert_eq!(report["result"], "activated");

        assert_eq!(
            code(&update(&home, &["status"])),
            0,
            "the record is resolved, so nothing is outstanding"
        );

        let rolled = update(&home, &["rollback", "--json"]);
        assert_eq!(
            code(&rolled),
            0,
            "stderr: {}",
            String::from_utf8_lossy(&rolled.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&rolled.stdout).expect("json");
        assert_eq!(report["version"], "0.0.1", "the seeded binary is back");

        // The rollback records an activation of its own, so it is outstanding
        // until it too is shown to be serving.
        assert_eq!(code(&update(&home, &["status"])), 1);
        assert_eq!(code(&update(&home, &["activate"])), 0);
    }

    /// A service install cannot be judged by a file the installer wrote; it
    /// needs a health document that postdates the activation.
    #[test]
    fn a_service_install_is_unverified_until_something_publishes_health() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);
        std::fs::create_dir_all(&data).expect("data");

        let applied = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
            .args(["update", "apply", "--artifact-dir"])
            .arg(&release)
            .args(["--artifact", OK, "--service", "danso.service", "--json"])
            .env("DANSO_HOME", &home)
            .env_remove("HOME")
            .output()
            .expect("apply");
        assert_eq!(
            code(&applied),
            0,
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );

        let out = std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
            .args(["update", "activate", "--data-dir"])
            .arg(&data)
            .arg("--json")
            .env("DANSO_HOME", &home)
            .env_remove("HOME")
            .output()
            .expect("activate");
        assert_eq!(
            code(&out),
            3,
            "no health document exists, so which image is serving is unknown"
        );
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
        assert_eq!(report["result"], "unverified");
        assert_eq!(report["serving"]["evidence"], "missing");
    }

    #[test]
    fn activate_is_a_no_op_when_nothing_is_pending() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let output = update(&home, &["activate", "--json"]);
        assert_eq!(code(&output), 0);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(report["result"], "nothing");
    }

    #[test]
    fn rollback_without_a_snapshot_fails_without_touching_anything() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let output = update(&home, &["rollback", "--json"]);
        assert_eq!(code(&output), 2);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(report["exit_code"], 2);
        assert!(!home.join("state/pending-activation.json").exists());
    }

    #[test]
    fn an_outstanding_activation_blocks_the_next_install() {
        let temp = tempfile::tempdir().expect("temp");
        let home = temp.path().join("home");
        let release = release_copy(temp.path());
        trust_fixture_key(&home);

        assert_eq!(code(&apply(&home, &release, OK)), 0);
        // A *different* artifact while the first activation is unresolved: it
        // would destroy the record and the rollback target that record needs.
        let second = apply(&home, &release, "danso-9.9.9-badexit.tar.gz");
        assert_eq!(code(&second), 2);
        assert_eq!(
            code(&update(&home, &["activate"])),
            0,
            "and resolving the activation is what unblocks it"
        );
        // Still refused, but now for the artifact's own reason rather than the
        // outstanding record.
        let third = apply(&home, &release, "danso-9.9.9-badexit.tar.gz");
        assert_eq!(code(&third), 2);
        let stderr = String::from_utf8_lossy(&third.stderr);
        assert_eq!(
            stderr.trim(),
            "update apply failed: apply_failed",
            "{stderr}"
        );
    }
}
