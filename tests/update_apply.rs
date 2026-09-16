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
    std::process::Command::new(env!("CARGO_BIN_EXE_danso"))
        .args(["update", "apply", "--artifact-dir"])
        .arg(release)
        .args(["--artifact", artifact, "--json"])
        .env("DANSO_HOME", home)
        .env_remove("HOME")
        .output()
        .expect("run danso update apply")
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
